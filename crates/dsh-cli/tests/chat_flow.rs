//! End-to-end tests for `dsh chat`.
//!
//! Three layers:
//! 1. **binary smoke test** — spawns the real `dsh` binary in line mode
//!    (piped stdin ⇒ non-TTY fallback) and verifies a full conversation runs;
//! 2. **key handling** — drives the TUI's `handle_key` directly and verifies
//!    typing, submit, history, and quit semantics;
//! 3. **rendering** — renders the TUI to a ratatui `TestBackend` buffer and
//!    asserts the visible layout without needing a real terminal.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cordis::Context;
use dsh_cli::tui::{attach_listener, ChatItem, ChatState, ChatTui};
use serde_json::json;

mod common;

// ---------------------------------------------------------------------------
// 1. Binary smoke test (real `dsh` executable, hermetic environment)
// ---------------------------------------------------------------------------

/// Run `dsh chat --line` in a hermetic environment (no default config, mock
/// provider) with `input` piped to stdin; returns (stdout, status).
fn run_chat_binary(input: &str, extra_args: &[&str]) -> (String, std::process::ExitStatus) {
    let bin = env!("CARGO_BIN_EXE_dsh");
    let workdir = std::env::temp_dir().join(format!("dsh-chat-test-{}", std::process::id()));
    std::fs::create_dir_all(&workdir).unwrap();

    let input_file = workdir.join("input.txt");
    std::fs::write(&input_file, input).unwrap();

    let mut cmd = Command::new(bin);
    cmd.arg("chat")
        .args(["--line", "--provider", "mock", "--model", "mock-1"])
        .args(extra_args)
        // Hermetic: no ~/.dsh/config.json, no ./dsh.json, no DSH_CONFIG.
        .current_dir(&workdir)
        .env("HOME", &workdir)
        .env_remove("DSH_CONFIG")
        .stdin(Stdio::from(std::fs::File::open(&input_file).unwrap()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn dsh chat");
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break status,
            None => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    panic!("dsh chat did not exit within 60s");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    let _ = std::fs::remove_dir_all(&workdir);
    (stdout, status)
}

#[test]
fn chat_line_mode_runs_a_conversation() {
    let (stdout, status) = run_chat_binary(
        "first message\nsecond message\n",
        &[],
    );

    assert!(status.success(), "dsh chat exited with {status:?}");

    // Banner, then one echo reply per line (mock adapter echoes the user).
    let mut lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "dsh chat — type a line, Ctrl-D to exit");
    assert!(
        lines.contains(&"first message"),
        "first reply missing from:\n{stdout}"
    );
    assert!(
        lines.contains(&"second message"),
        "second reply missing from:\n{stdout}"
    );
    // Nothing unexpected beyond banner + 2 replies.
    lines.remove(0);
    assert_eq!(lines.len(), 2, "unexpected extra output:\n{stdout}");
}

#[test]
fn chat_line_mode_uses_the_configured_defaults() {
    // With a config file providing a default model, `dsh chat` picks it up.
    let workdir = std::env::temp_dir().join(format!("dsh-chat-cfg-{}", std::process::id()));
    std::fs::create_dir_all(workdir.join(".dsh")).unwrap();
    std::fs::write(
        workdir.join(".dsh").join("config.json"),
        json!({
            "provider": "mock",
            "model": "configured-model",
        })
        .to_string(),
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_dsh");
    let mut child = Command::new(bin)
        .arg("chat")
        .args(["--line"])
        .current_dir(&workdir)
        .env("HOME", &workdir)
        .env_remove("DSH_CONFIG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"ping\n")
        .unwrap();
    // Dropping stdin closes the pipe → EOF → line loop ends → process exits.

    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        match child.try_wait().unwrap() {
            Some(status) => break status,
            None => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    panic!("timeout waiting for dsh chat");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };
    let mut stdout = String::new();
    child.stdout.take().unwrap().read_to_string(&mut stdout).unwrap();
    assert!(status.success());
    assert!(
        stdout.contains("ping"),
        "expected the mock echo of 'ping', got:\n{stdout}"
    );
    let _ = std::fs::remove_dir_all(&workdir);
}

// ---------------------------------------------------------------------------
// 2. TUI key handling (simulated input against a live harness)
// ---------------------------------------------------------------------------

/// Compose the minimal harness through the base bundle and return (ctx, agent).
async fn harness() -> (Context, Arc<dyn dsh_api::services::AgentView>) {
    let ctx = Context::new();
    dsh_bundle::install_base_default(&ctx).await.unwrap();
    let agent = common::create_agent(&ctx).await;
    (ctx, agent)
}

fn key(code: ratatui::crossterm::event::KeyCode) -> ratatui::crossterm::event::KeyEvent {
    ratatui::crossterm::event::KeyEvent::new(code, ratatui::crossterm::event::KeyModifiers::NONE)
}

fn ctrl_c() -> ratatui::crossterm::event::KeyEvent {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
}

#[tokio::test]
async fn tui_keys_type_submit_and_receive() {
    let (ctx, agent) = harness().await;
    let state = Arc::new(Mutex::new(ChatState {
        session_id: Some(agent.id().to_string()),
        ..Default::default()
    }));
    attach_listener(&ctx, agent.id().to_string(), state.clone()).await.unwrap();

    let mut tui = ChatTui::default();

    // Type "hi" character by character, then submit with Enter.
    for c in "hi".chars() {
        let quit = tui.handle_key(key(ratatui::crossterm::event::KeyCode::Char(c)), &agent).unwrap();
        assert!(!quit, "typing must not quit");
    }
    assert_eq!(tui.input(), "hi");

    let quit = tui.handle_key(key(ratatui::crossterm::event::KeyCode::Enter), &agent).unwrap();
    assert!(!quit);
    assert!(tui.input().is_empty(), "input cleared after submit");

    // The conversation runs and the projection reflects user + assistant.
    agent.when_idle().await;
    let state = state.lock().unwrap();
    assert!(state.items.iter().any(|i| matches!(i, ChatItem::User { text } if text == "hi")));
    assert!(state.items.iter().any(|i| matches!(i, ChatItem::Assistant { text } if text == "hi")));
}

#[tokio::test]
async fn tui_keys_history_navigation() {
    let (_ctx, agent) = harness().await;
    let mut tui = ChatTui::default();

    let submit = |tui: &mut ChatTui, text: &str| {
        for c in text.chars() {
            tui.handle_key(key(ratatui::crossterm::event::KeyCode::Char(c)), &agent).unwrap();
        }
        tui.handle_key(key(ratatui::crossterm::event::KeyCode::Enter), &agent).unwrap();
    };

    submit(&mut tui, "one");
    submit(&mut tui, "two");
    assert_eq!(tui.history().len(), 2);

    // Up recalls the most recent submission, then the older one.
    tui.handle_key(key(ratatui::crossterm::event::KeyCode::Up), &agent).unwrap();
    assert_eq!(tui.input(), "two");
    tui.handle_key(key(ratatui::crossterm::event::KeyCode::Up), &agent).unwrap();
    assert_eq!(tui.input(), "one");

    // Down returns to "two", then clears past the newest entry.
    tui.handle_key(key(ratatui::crossterm::event::KeyCode::Down), &agent).unwrap();
    assert_eq!(tui.input(), "two");
    tui.handle_key(key(ratatui::crossterm::event::KeyCode::Down), &agent).unwrap();
    assert!(tui.input().is_empty());

    // Esc clears the current input.
    for c in "abc".chars() {
        tui.handle_key(key(ratatui::crossterm::event::KeyCode::Char(c)), &agent).unwrap();
    }
    tui.handle_key(key(ratatui::crossterm::event::KeyCode::Esc), &agent).unwrap();
    assert!(tui.input().is_empty());
}

#[test]
fn tui_keys_quit_semantics() {
    let (ctx, agent) = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(harness());
    let _ = ctx;
    let mut tui = ChatTui::default();

    // IME safety: a bare 'q' (e.g. 拼音 "请/去") must NEVER quit, even with
    // an empty input buffer.
    assert!(
        !tui.handle_key(key(ratatui::crossterm::event::KeyCode::Char('q')), &agent).unwrap(),
        "bare 'q' must not quit — an IME commits it as a normal character"
    );
    assert_eq!(tui.input(), "q");

    // Ctrl-C always quits.
    assert!(tui.handle_key(ctrl_c(), &agent).unwrap());
}

#[test]
fn tui_keys_ctrl_q_quits() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let (ctx, agent) = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(harness());
    let _ = ctx;
    let mut tui = ChatTui::default();
    let ctrl_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
    assert!(tui.handle_key(ctrl_q, &agent).unwrap());
}

#[tokio::test]
async fn tui_paste_appends_whole_text_single_line() {
    let (_ctx, _agent) = harness().await;
    let mut tui = ChatTui::default();

    // Multi-line paste with CRLF must collapse to a single line, appended
    // whole so CJK text is never torn into garbled pieces.
    tui.handle_paste("第一行\r\n第二行\n第三行".to_string());
    assert_eq!(tui.input(), "第一行 第二行 第三行");
    assert!(!tui.input().contains('\n'));
    assert!(!tui.input().contains('\r'));

    // Pasting into existing input appends, and oversize pastes are capped.
    let mut tui2 = ChatTui::default();
    tui2.handle_paste("a".to_string());
    let huge = "x".repeat(10_000);
    tui2.handle_paste(huge);
    assert!(tui2.input().len() <= 4096 + 1);
}

#[tokio::test]
async fn tui_ctrl_u_clears_and_ctrl_w_deletes_word() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let (_ctx, agent) = harness().await;
    let mut tui = ChatTui::default();
    let ctrl_u = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
    let ctrl_w = KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);

    for c in "hello world".chars() {
        tui.handle_key(key(KeyCode::Char(c)), &agent).unwrap();
    }
    assert_eq!(tui.input(), "hello world");

    // Ctrl-W deletes the last whitespace-delimited word.
    tui.handle_key(ctrl_w, &agent).unwrap();
    assert_eq!(tui.input(), "hello ");

    // Ctrl-U clears the whole line.
    tui.handle_key(ctrl_u, &agent).unwrap();
    assert!(tui.input().is_empty());
}

#[tokio::test]
async fn tui_backspace_deletes_full_graphemes() {
    let (_ctx, agent) = harness().await;
    let mut tui = ChatTui::default();

    // Typing an emoji, then backspace, removes the whole cluster.
    for c in "a👨‍👩‍👧".chars() {
        tui.handle_key(key(ratatui::crossterm::event::KeyCode::Char(c)), &agent).unwrap();
    }
    assert_eq!(tui.input(), "a👨\u{200d}👩\u{200d}👧");
    tui.handle_key(key(ratatui::crossterm::event::KeyCode::Backspace), &agent).unwrap();
    assert_eq!(tui.input(), "a", "backspace must remove the whole ZWJ emoji cluster");

    // Combining mark: 'e' + U+0301 is one grapheme; one backspace removes both.
    let mut tui2 = ChatTui::default();
    for c in ['e', '\u{0301}'].into_iter() {
        tui2.handle_key(key(ratatui::crossterm::event::KeyCode::Char(c)), &agent).unwrap();
    }
    assert_eq!(tui2.input(), "e\u{0301}");
    tui2.handle_key(key(ratatui::crossterm::event::KeyCode::Backspace), &agent).unwrap();
    assert!(tui2.input().is_empty(), "combining sequence deleted as one grapheme");
}

#[tokio::test]
async fn tui_repeat_keys_still_enter_text() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    let (_ctx, agent) = harness().await;
    let mut tui = ChatTui::default();

    // Long-press repeat events must behave like presses (no dropped chars).
    for _ in 0..3 {
        let mut ev = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        ev.kind = KeyEventKind::Repeat;
        tui.handle_key(ev, &agent).unwrap();
    }
    assert_eq!(tui.input(), "aaa");
}

#[tokio::test]
async fn tui_caret_moves_and_edits_in_the_middle() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let (_ctx, agent) = harness().await;
    let mut tui = ChatTui::default();
    let left = || key(KeyCode::Left);

    // Type "abc": caret sits after 'c'.
    for c in "abc".chars() {
        tui.handle_key(key(KeyCode::Char(c)), &agent).unwrap();
    }
    assert_eq!(tui.input(), "abc");
    assert_eq!(tui.cursor(), 3);

    // Move left twice: caret between 'a' and 'b'.
    tui.handle_key(left(), &agent).unwrap();
    tui.handle_key(left(), &agent).unwrap();
    assert_eq!(tui.cursor(), 1);

    // Insert 'X' at the caret → "aXbc", caret now after 'X'.
    tui.handle_key(key(KeyCode::Char('X')), &agent).unwrap();
    assert_eq!(tui.input(), "aXbc");
    assert_eq!(tui.cursor(), 2);

    // Backspace deletes what is BEFORE the caret ('X') → "abc", caret at 1.
    tui.handle_key(key(KeyCode::Backspace), &agent).unwrap();
    assert_eq!(tui.input(), "abc");
    assert_eq!(tui.cursor(), 1);

    // Jump to the start; Delete removes what is AFTER the caret ('a') → "bc".
    tui.handle_key(key(KeyCode::Home), &agent).unwrap();
    tui.handle_key(key(KeyCode::Delete), &agent).unwrap();
    assert_eq!(tui.input(), "bc");

    // Home / End jump the caret around further edits.
    tui.handle_key(key(KeyCode::Char('Z')), &agent).unwrap();
    assert_eq!(tui.input(), "Zbc");
    tui.handle_key(key(KeyCode::Home), &agent).unwrap();
    tui.handle_key(key(KeyCode::Char('0')), &agent).unwrap();
    assert_eq!(tui.input(), "0Zbc");
    tui.handle_key(key(KeyCode::End), &agent).unwrap();
    tui.handle_key(key(KeyCode::Char('1')), &agent).unwrap();
    assert_eq!(tui.input(), "0Zbc1");

    // Ctrl-A / Ctrl-E also jump the caret.
    let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
    let ctrl_e = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL);
    tui.handle_key(ctrl_a, &agent).unwrap();
    assert_eq!(tui.cursor(), 0);
    tui.handle_key(ctrl_e, &agent).unwrap();
    assert_eq!(tui.cursor(), tui.input().len());
}

#[tokio::test]
async fn tui_caret_moves_by_grapheme_clusters() {
    let (_ctx, agent) = harness().await;
    let mut tui = ChatTui::default();
    let left = || key(ratatui::crossterm::event::KeyCode::Left);

    // A ZWJ emoji family is ONE grapheme cluster: one Left jump crosses it.
    for c in "a👨\u{200d}👩\u{200d}👧".chars() {
        tui.handle_key(key(ratatui::crossterm::event::KeyCode::Char(c)), &agent).unwrap();
    }
    assert_eq!(tui.cursor(), tui.input().len());
    tui.handle_key(left(), &agent).unwrap();
    assert_eq!(&tui.input()[..tui.cursor()], "a", "one Left must cross the whole emoji cluster");

    // Backspace at the caret deletes the whole cluster.
    let mut tui2 = ChatTui::default();
    for c in ['e', '\u{0301}', 'x'].into_iter() {
        tui2.handle_key(key(ratatui::crossterm::event::KeyCode::Char(c)), &agent).unwrap();
    }
    // Caret at end; move left past 'x' to the combining sequence.
    tui2.handle_key(key(ratatui::crossterm::event::KeyCode::Left), &agent).unwrap();
    tui2.handle_key(key(ratatui::crossterm::event::KeyCode::Backspace), &agent).unwrap();
    assert_eq!(tui2.input(), "x", "backspace deletes e + combining mark as one grapheme");
}

#[tokio::test]
async fn tui_caret_scrolls_the_input_window() {
    let (_ctx, agent) = harness().await;
    let mut tui = ChatTui::default();
    let left = || key(ratatui::crossterm::event::KeyCode::Left);

    // Long CJK input; jump the caret to the very start and back to the end.
    let long = "这是一个很长的中文输入".repeat(5); // 55 CJK chars, 110 cols
    for c in long.chars() {
        tui.handle_key(key(ratatui::crossterm::event::KeyCode::Char(c)), &agent).unwrap();
    }
    // Walk the caret all the way back to the start.
    while tui.cursor() > 0 {
        tui.handle_key(left(), &agent).unwrap();
    }
    assert_eq!(tui.cursor(), 0);

    // Render a narrow field; the caret (at 0) must be visible.
    let backend = ratatui::backend::TestBackend::new(40, 12);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let state = Arc::new(Mutex::new(ChatState::default()));
    terminal
        .draw(|frame| tui.draw(frame, &agent, &state))
        .unwrap();
    let text = buffer_text(terminal.backend().buffer());
    assert!(text.contains("❯"), "caret window must render:\n{text}");
    assert!(text.contains('这'), "start of input must be visible at caret 0:\n{text}");
}

// ---------------------------------------------------------------------------
// 3. Rendering (ratatui TestBackend — no terminal required)
// ---------------------------------------------------------------------------

fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
    let mut out = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            if let Some(cell) = buffer.cell((x, y)) {
                out.push_str(cell.symbol());
            }
        }
        out.push('\n');
    }
    out
}

#[tokio::test]
async fn tui_renders_conversation_and_input() {
    let (ctx, agent) = harness().await;
    let state = Arc::new(Mutex::new(ChatState {
        session_id: Some(agent.id().to_string()),
        ..Default::default()
    }));
    attach_listener(&ctx, agent.id().to_string(), state.clone()).await.unwrap();

    // Run one short conversation so the projection has content.
    let mut tui = ChatTui::default();
    for c in "hello".chars() {
        tui.handle_key(key(ratatui::crossterm::event::KeyCode::Char(c)), &agent).unwrap();
    }
    tui.handle_key(key(ratatui::crossterm::event::KeyCode::Enter), &agent).unwrap();
    agent.when_idle().await;

    // Render into a fixed 100x30 test buffer.
    let backend = ratatui::backend::TestBackend::new(120, 30);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| tui.draw(frame, &agent, &state))
        .unwrap();
    let text = buffer_text(terminal.backend().buffer());

    // Title carries the session id.
    assert!(text.contains("dsh chat"), "title missing:\n{text}");
    assert!(text.contains(agent.id()), "session id missing:\n{text}");

    // The conversation is rendered: user line, assistant echo line.
    assert!(text.contains("hello"), "conversation missing:\n{text}");

    // Input area with the prompt marker; status line with the idle hint.
    assert!(text.contains("❯"), "input prompt missing:\n{text}");
    assert!(text.contains("idle") || text.contains("running"), "status missing:\n{text}");
    assert!(text.contains("quit"), "key hint missing:\n{text}");
    assert!(text.contains("←/→"), "caret hint missing:\n{text}");
}

#[tokio::test]
async fn tui_renders_long_input_without_overflow() {
    let (_ctx, agent) = harness().await;
    let mut tui = ChatTui::default();

    // A long CJK input (each char is 2 columns wide) must render inside the
    // input field: the field scrolls horizontally, the tail stays visible,
    // and nothing bleeds outside the field's area.
    let long_cjk = "这是一个很长的中文输入".repeat(5); // 80 columns wide
    for c in long_cjk.chars() {
        tui.handle_key(key(ratatui::crossterm::event::KeyCode::Char(c)), &agent).unwrap();
    }
    assert_eq!(tui.input(), long_cjk);

    let backend = ratatui::backend::TestBackend::new(60, 20);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| tui.draw(frame, &agent, &Arc::new(Mutex::new(ChatState::default()))))
        .unwrap();
    let text = buffer_text(terminal.backend().buffer());

    // The tail of the input is visible. CJK wide chars occupy two cells in
    // the buffer (char + blank), so compare against the spaced rendering.
    let tail = &long_cjk[long_cjk.len() - 6..];
    let tail_rendered: String = tail.chars().map(|c| format!("{c} ")).collect::<String>().trim_end().to_string();
    assert!(
        text.contains(&tail_rendered),
        "input tail not visible (expected `{tail_rendered}`):\n{text}"
    );

    // Every line stays within the buffer — no cell overflowed the area.
    for line in text.lines() {
        assert!(line.chars().count() <= 60, "line overflowed the buffer:\n{line}");
    }
}

#[tokio::test]
async fn tui_renders_tool_activity() {
    let (ctx, agent) = harness().await;

    // Script the mock: a bash tool call, then the final answer. The bundle is
    // already installed by `harness()`, so only swap the adapter.
    common::boot_scripted(
        &ctx,
        false,
        vec![
            common::tool_call_response("call-1", "bash", json!({ "command": "echo rendered-ok" })),
            common::text_response("all done"),
        ],
    )
    .await;

    let state = Arc::new(Mutex::new(ChatState {
        session_id: Some(agent.id().to_string()),
        ..Default::default()
    }));
    attach_listener(&ctx, agent.id().to_string(), state.clone()).await.unwrap();

    let mut tui = ChatTui::default();
    for c in "go".chars() {
        tui.handle_key(key(ratatui::crossterm::event::KeyCode::Char(c)), &agent).unwrap();
    }
    tui.handle_key(key(ratatui::crossterm::event::KeyCode::Enter), &agent).unwrap();
    agent.when_idle().await;

    let backend = ratatui::backend::TestBackend::new(100, 30);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| tui.draw(frame, &agent, &state))
        .unwrap();
    let text = buffer_text(terminal.backend().buffer());

    // The tool call, its result, and the final answer are all visible.
    assert!(text.contains("bash"), "tool call missing:\n{text}");
    assert!(text.contains("rendered-ok"), "tool result missing:\n{text}");
    assert!(text.contains("all done"), "final answer missing:\n{text}");
}
