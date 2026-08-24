//! Terminal chat UI for `dsh chat`, built on ratatui + crossterm.
//!
//! The UI is driven by two inputs:
//! - the append-only session log (rendered through a [`ChatState`] projection
//!   fed by a `session/event` listener), and
//! - keyboard input (send, history, scroll, quit).
//!
//! Streaming assistant text is shown live: `assistant/chunk` text deltas
//! accumulate in `ChatState::pending` until the step's `assistant/message`
//! finalizes it.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cordis::Context;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};
use unicode_width::UnicodeWidthStr;

use dsh_core::Agent;
use dsh_llm::ContentBlock;
use dsh_session::{SessionEvent, SessionEventData};
use unicode_segmentation::UnicodeSegmentation;

/// Hard cap on pasted text, so a huge paste cannot freeze the UI.
const PASTE_LIMIT: usize = 4096;

/// One renderable line of the conversation.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatItem {
    User { text: String },
    Assistant { text: String },
    ToolCall { name: String, arguments: String },
    ToolResult { text: String, is_error: bool },
    System { text: String },
}

/// The TUI's projection of the session log.
#[derive(Debug, Default)]
pub struct ChatState {
    pub items: Vec<ChatItem>,
    /// Streaming assistant text for the current step, not yet finalized.
    pub pending: String,
    pub session_id: Option<String>,
}

/// Extract the text nested inside a tool-result content block.
fn tool_result_text(message: &dsh_llm::Message) -> String {
    let mut out = String::new();
    for block in &message.content {
        if let ContentBlock::ToolResult { content, .. } = block {
            for inner in content {
                if let ContentBlock::Text { text } = inner {
                    out.push_str(text);
                }
            }
        }
    }
    out
}

fn message_is_error(message: &dsh_llm::Message) -> bool {
    message.content.iter().any(|block| {
        matches!(
            block,
            ContentBlock::ToolResult {
                is_error: Some(true),
                ..
            }
        )
    })
}

impl ChatState {
    /// Fold one committed session event into the projection. Returns whether
    /// the visible state changed.
    pub fn apply_event(&mut self, event: &SessionEvent) -> bool {
        match &event.data {
            SessionEventData::UserMessage { message } => {
                let text = message.text();
                if !text.is_empty() {
                    self.flush_pending();
                    self.items.push(ChatItem::User { text });
                }
                true
            }
            SessionEventData::AssistantChunk { chunk, .. } => {
                if let dsh_llm::StreamChunk::TextDelta { text, .. } = chunk {
                    self.pending.push_str(text);
                }
                true
            }
            SessionEventData::AssistantMessage { message, .. } => {
                let text = message.text();
                // The assembled message is authoritative; drop the raw deltas.
                self.pending.clear();
                if !text.is_empty() {
                    self.items.push(ChatItem::Assistant { text });
                }
                true
            }
            SessionEventData::ToolCall { name, arguments, .. } => {
                let arguments = serde_json::from_str::<serde_json::Value>(arguments)
                    .map(|v| v.to_string())
                    .unwrap_or_else(|_| arguments.clone());
                self.items.push(ChatItem::ToolCall {
                    name: name.clone(),
                    arguments,
                });
                true
            }
            SessionEventData::ToolResult { message, .. } => {
                self.items.push(ChatItem::ToolResult {
                    text: tool_result_text(message),
                    is_error: message_is_error(message),
                });
                true
            }
            SessionEventData::TurnEnd { reason, .. } => {
                use dsh_session::TurnEndReason;
                if !matches!(reason, TurnEndReason::Completed) {
                    self.flush_pending();
                    self.items.push(ChatItem::System {
                        text: format!("— turn ended: {reason:?}"),
                    });
                }
                true
            }
            _ => false,
        }
    }

    /// Promote any unfinalized streaming text into a finalized assistant item.
    pub fn flush_pending(&mut self) {
        if !self.pending.is_empty() {
            self.items.push(ChatItem::Assistant {
                text: std::mem::take(&mut self.pending),
            });
        }
    }

    pub fn item_count(&self) -> usize {
        self.items.len()
    }
}

/// Register a `session/event` listener that folds events for `session_id`
/// into `state`. The listener runs on the cordis event bus (tokio task).
pub async fn attach_listener(
    ctx: &Context,
    session_id: String,
    state: Arc<Mutex<ChatState>>,
) -> Result<cordis::fiber::EffectGuard, cordis::Error> {
    ctx.on("session/event", move |_ctx, payload, _next| {
        let state = state.clone();
        let session_id = session_id.clone();
        Box::pin(async move {
            let seen = payload.get("session").and_then(|s| s.as_str()).unwrap_or("");
            if seen != session_id {
                return Ok(serde_json::Value::Null);
            }
            let event = payload.get("event").cloned().unwrap_or(serde_json::Value::Null);
            if let Ok(event) = serde_json::from_value::<SessionEvent>(event) {
                state.lock().unwrap().apply_event(&event);
            }
            Ok(serde_json::Value::Null)
        })
    })
    .await
}

// ---------------------------------------------------------------------------
// TUI
// ---------------------------------------------------------------------------

/// The interactive chat TUI.
pub struct ChatTui {
    input: String,
    /// Byte offset of the caret inside `input`.
    cursor: usize,
    /// Horizontal scroll offset (columns) of the input field.
    input_scroll: u16,
    history: Vec<String>,
    history_pos: Option<usize>,
    scroll: usize,
    auto_follow: bool,
    seq: u64,
    list_state: ListState,
    /// Set when the visible frame must be redrawn.
    dirty: bool,
    /// Last seen projection size, to detect changes without locking often.
    last_items: usize,
    last_pending: usize,
}

impl Default for ChatTui {
    fn default() -> Self {
        ChatTui {
            input: String::new(),
            cursor: 0,
            input_scroll: 0,
            history: Vec::new(),
            history_pos: None,
            scroll: 0,
            auto_follow: true,
            seq: 0,
            list_state: ListState::default(),
            dirty: true,
            last_items: 0,
            last_pending: 0,
        }
    }
}

/// Run the chat TUI until the user quits. Enters raw mode + alternate screen;
/// restores the terminal before returning.
pub async fn run_chat(ctx: &Context, agent: &Arc<Agent>) -> Result<(), String> {
    let state = Arc::new(Mutex::new(ChatState {
        session_id: Some(agent.id.clone()),
        ..Default::default()
    }));
    attach_listener(ctx, agent.id.clone(), state.clone()).await.map_err(|e| e.to_string())?;

    let mut terminal = ratatui::init();
    let mut tui = ChatTui::default();
    let result = tui.event_loop(&mut terminal, agent, state);
    ratatui::restore();
    result.map_err(|e| format!("terminal error: {e}"))
}

impl ChatTui {
    /// The current input text.
    pub fn input(&self) -> &str {
        &self.input
    }

    /// Byte offset of the caret inside `input`.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Submitted prompts, oldest first.
    pub fn history(&self) -> &[String] {
        &self.history
    }

    // --- caret / editing helpers -----------------------------------------

    fn cursor_col(&self) -> u16 {
        self.input[..self.cursor].width() as u16
    }

    fn caret_to_end(&mut self) {
        self.cursor = self.input.len();
    }

    fn caret_left(&mut self) {
        let before = &self.input[..self.cursor];
        if let Some(last) = before.graphemes(true).next_back() {
            self.cursor -= last.len();
        }
    }

    fn caret_right(&mut self) {
        let after = &self.input[self.cursor..];
        if let Some(first) = after.graphemes(true).next() {
            self.cursor += first.len();
        }
    }

    /// Insert one grapheme cluster at the caret.
    fn insert_at_caret(&mut self, cluster: &str) {
        self.input.insert_str(self.cursor, cluster);
        self.cursor += cluster.len();
    }

    /// Delete the grapheme cluster before the caret (Backspace).
    fn delete_before_caret(&mut self) {
        let before = &self.input[..self.cursor];
        if let Some(last) = before.graphemes(true).next_back() {
            let start = self.cursor - last.len();
            self.input.replace_range(start..self.cursor, "");
            self.cursor = start;
        }
    }

    /// Delete the grapheme cluster after the caret (Delete).
    fn delete_after_caret(&mut self) {
        let after = &self.input[self.cursor..];
        if let Some(first) = after.graphemes(true).next() {
            self.input.replace_range(self.cursor..self.cursor + first.len(), "");
        }
    }

    /// The slice of `input` visible inside the field, scrolled so the caret
    /// stays in view.
    fn visible_window(&mut self, visible_cols: u16) -> String {
        let cursor_col = self.cursor_col();
        let visible = visible_cols.max(1);
        if cursor_col < self.input_scroll {
            self.input_scroll = cursor_col;
        } else if cursor_col >= self.input_scroll + visible {
            self.input_scroll = cursor_col - visible + 1;
        }
        let mut out = String::new();
        let mut width = 0u16;
        for grapheme in self.input.graphemes(true) {
            let gwidth = grapheme.width() as u16;
            if width + gwidth > self.input_scroll + visible {
                break;
            }
            if width + gwidth > self.input_scroll {
                out.push_str(grapheme);
            }
            width += gwidth;
        }
        out
    }

    fn event_loop(
        &mut self,
        terminal: &mut Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>,
        agent: &Arc<Agent>,
        state: Arc<Mutex<ChatState>>,
    ) -> io::Result<()> {
        loop {
            // Redraw only when something changed: input edits, pasted text,
            // new session events, resizes, or an active stream. While idle
            // with no changes we stay dark so the terminal's own IME
            // composition preview (preedit) is not flickered over.
            let changed = {
                let snapshot = state.lock().unwrap();
                snapshot.items.len() != self.last_items || snapshot.pending.len() != self.last_pending
            };
            if changed {
                let snapshot = state.lock().unwrap();
                self.last_items = snapshot.items.len();
                self.last_pending = snapshot.pending.len();
            }
            if self.dirty || changed || agent.driver_busy() {
                terminal.draw(|frame| self.draw(frame, agent, &state))?;
                self.dirty = false;
            }

            if event::poll(Duration::from_millis(60))? {
                match event::read()? {
                    Event::Key(key)
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
                        if self.handle_key(key, agent)? {
                            break;
                        }
                    }
                    Event::Paste(text) => {
                        self.handle_paste(text);
                    }
                    Event::Resize(..) => {
                        self.dirty = true;
                    }
                    Event::FocusGained | Event::FocusLost => {}
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// Fold pasted text (and IME whole-phrase commits that arrive through the
    /// paste channel) into the input at the caret: single-line, capped,
    /// inserted whole so CJK and combining sequences are never split into
    /// garbled pieces.
    pub fn handle_paste(&mut self, text: String) {
        // CRLF collapses to one space; the result stays a single line.
        let normalized = text.replace("\r\n", "\n").replace(['\r', '\n'], " ");
        let cleaned: String = normalized.chars().take(PASTE_LIMIT).collect();
        self.insert_at_caret(&cleaned);
        self.history_pos = None;
        self.dirty = true;
    }

    /// Render one frame. Public so tests (TestBackend) and alternative
    /// frontends can drive the TUI programmatically.
    pub fn draw(
        &mut self,
        frame: &mut Frame,
        agent: &Arc<Agent>,
        state: &Mutex<ChatState>,
    ) {
        let area = frame.area();
        let chunks = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

        let state = state.lock().unwrap();

        // --- message list ---
        let mut items: Vec<ListItem> = state
            .items
            .iter()
            .map(|item| ListItem::new(Self::render_item(item)))
            .collect();
        if !state.pending.is_empty() {
            items.push(ListItem::new(Line::from(Span::styled(
                format!("▍{}", state.pending),
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
            ))));
        }

        let visible = (chunks[0].height.saturating_sub(2)) as usize;
        let max_scroll = items.len().saturating_sub(visible);
        let scroll = if self.auto_follow {
            max_scroll
        } else {
            self.scroll.min(max_scroll)
        };

        let title = state
            .session_id
            .as_deref()
            .map(|id| format!(" dsh chat — {id} "))
            .unwrap_or_else(|| " dsh chat ".to_string());
        let list = List::new(items)
            .block(Block::bordered().title(Line::from(Span::styled(
                title,
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ))));
        *self.list_state.offset_mut() = scroll.min(usize::MAX);
        frame.render_stateful_widget(list, chunks[0], &mut self.list_state);

        // --- input ---
        // Single-line, horizontally scrolling around the CARET: the window is
        // scrolled so the caret stays in view wherever it is moved. CJK wide
        // chars are never split mid-grapheme (only the display is clipped).
        let prompt_width = 2u16; // "❯ "
        let inner_width = chunks[1].width.saturating_sub(2); // inside the border
        let input_visible = inner_width.saturating_sub(prompt_width);
        let visible_input = self.visible_window(input_visible);

        let prompt = Span::styled("❯ ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD));
        let input_text = Text::from(Line::from(vec![prompt, Span::raw(visible_input)]));
        // No wrapping: the field scrolls horizontally, so a long line can
        // never spill onto a second row or bleed outside the border.
        let input_widget = Paragraph::new(input_text).block(Block::bordered().title(" input "));
        frame.render_widget(input_widget, chunks[1]);

        // Caret at its position inside the visible window, clamped inside the
        // field.
        let caret_col = self.cursor_col().saturating_sub(self.input_scroll);
        let x = (chunks[1].x + 1 + prompt_width + caret_col)
            .min(chunks[1].right().saturating_sub(1));
        let y = chunks[1].y + 1;
        frame.set_cursor_position(Position::new(x, y));

        // --- status line ---
        let status = if agent.driver_busy() {
            Span::styled(
                "● running",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled("○ idle", Style::default().fg(Color::Green))
        };
        let hint = Span::styled(
            "  enter send  ←/→ move  ↑/↓ hist  ctrl-a/e line  ctrl-u clear  ctrl-c/q quit",
            Style::default().fg(Color::DarkGray),
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![status, hint])),
            chunks[2],
        );
    }

    fn render_item(item: &ChatItem) -> Line<'static> {
        match item {
            ChatItem::User { text } => Line::from(vec![
                Span::styled(
                    "❯ ",
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    text.clone(),
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
            ]),
            ChatItem::Assistant { text } => Line::from(vec![
                Span::styled(
                    "◈ ",
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                ),
                Span::styled(text.clone(), Style::default().fg(Color::LightBlue)),
            ]),
            ChatItem::ToolCall { name, arguments } => Line::from(vec![
                Span::styled(
                    "⚙ ",
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{name} {arguments}"),
                    Style::default().fg(Color::Yellow),
                ),
            ]),
            ChatItem::ToolResult { text, is_error } => {
                let color = if *is_error { Color::Red } else { Color::DarkGray };
                Line::from(vec![
                    Span::styled("↩ ", Style::default().fg(color).add_modifier(Modifier::BOLD)),
                    Span::styled(text.clone(), Style::default().fg(color)),
                ])
            }
            ChatItem::System { text } => Line::from(Span::styled(
                text.clone(),
                Style::default().fg(Color::Magenta).add_modifier(Modifier::ITALIC),
            )),
        }
    }

    /// Handle one key press. Returns `Ok(true)` when the UI should quit.
    /// Public so tests can simulate input and alternative frontends can reuse
    /// the same key semantics.
    ///
    /// IME-friendly rules: quitting is only possible via Ctrl-C / Ctrl-Q — a
    /// bare `q` must never quit, because a Chinese/Japanese IME commits `q`
    /// (e.g. 拼音 "请/去") as an ordinary character.
    pub fn handle_key(&mut self, key: ratatui::crossterm::event::KeyEvent, agent: &Arc<Agent>) -> io::Result<bool> {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Ok(true),
            KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => Ok(true),
            // Ctrl-U: clear the whole line.
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.clear();
                self.cursor = 0;
                self.input_scroll = 0;
                self.history_pos = None;
                self.dirty = true;
                Ok(false)
            }
            // Ctrl-W: delete the word before the caret.
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let before = &self.input[..self.cursor];
                let trimmed = before.trim_end();
                let cut = trimmed
                    .rfind(char::is_whitespace)
                    .map(|pos| pos + 1)
                    .unwrap_or(0);
                self.input.replace_range(cut..self.cursor, "");
                self.cursor = cut;
                self.history_pos = None;
                self.dirty = true;
                Ok(false)
            }
            // Ctrl-A / Ctrl-E: jump to the start / end of the line.
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor = 0;
                self.dirty = true;
                Ok(false)
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.caret_to_end();
                self.dirty = true;
                Ok(false)
            }
            KeyCode::Esc => {
                // Cancels the composition buffer (IME cancel also surfaces as
                // Esc on some terminals) — clear the line.
                self.input.clear();
                self.cursor = 0;
                self.input_scroll = 0;
                self.history_pos = None;
                self.dirty = true;
                Ok(false)
            }
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.input);
                self.cursor = 0;
                self.input_scroll = 0;
                self.history_pos = None;
                self.dirty = true;
                if text.trim().is_empty() {
                    return Ok(false);
                }
                self.history.push(text.clone());
                self.seq += 1;
                agent.followup(dsh_core::agent::user_message_with_text(
                    format!("u-{}", self.seq),
                    text,
                ));
                Ok(false)
            }
            KeyCode::Backspace => {
                // Delete one full grapheme cluster BEFORE the caret so
                // combining marks, ZWJ emoji, and IME-committed sequences are
                // not torn apart.
                self.delete_before_caret();
                self.dirty = true;
                Ok(false)
            }
            KeyCode::Delete => {
                // Delete one full grapheme cluster AFTER the caret.
                self.delete_after_caret();
                self.dirty = true;
                Ok(false)
            }
            KeyCode::Left => {
                self.caret_left();
                self.dirty = true;
                Ok(false)
            }
            KeyCode::Right => {
                self.caret_right();
                self.dirty = true;
                Ok(false)
            }
            KeyCode::Home => {
                self.cursor = 0;
                self.dirty = true;
                Ok(false)
            }
            KeyCode::End => {
                self.caret_to_end();
                self.dirty = true;
                Ok(false)
            }
            KeyCode::Char(c) => {
                // Insert at the caret (not just append) so editing in the
                // middle of the line works.
                self.insert_at_caret(&c.to_string());
                self.history_pos = None;
                self.dirty = true;
                Ok(false)
            }
            KeyCode::Up => {
                if !self.history.is_empty() {
                    // Walk one step further back through the history.
                    let pos = self
                        .history_pos
                        .map_or(0, |p| p + 1)
                        .min(self.history.len() - 1);
                    self.history_pos = Some(pos);
                    self.input = self.history[self.history.len() - 1 - pos].clone();
                    self.caret_to_end();
                    self.dirty = true;
                }
                Ok(false)
            }
            KeyCode::Down => {
                if let Some(pos) = self.history_pos {
                    if pos == 0 {
                        self.history_pos = None;
                        self.input.clear();
                        self.cursor = 0;
                    } else {
                        let pos = pos - 1;
                        self.history_pos = Some(pos);
                        self.input = self.history[self.history.len() - 1 - pos].clone();
                        self.caret_to_end();
                    }
                    self.dirty = true;
                }
                Ok(false)
            }
            KeyCode::PageUp => {
                self.auto_follow = false;
                self.scroll = self.scroll.saturating_add(10);
                self.dirty = true;
                Ok(false)
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_sub(10);
                if self.scroll == 0 {
                    self.auto_follow = true;
                }
                self.dirty = true;
                Ok(false)
            }
            _ => Ok(false),
        }
    }
}
