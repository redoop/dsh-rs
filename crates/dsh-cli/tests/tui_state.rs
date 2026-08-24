//! Tests for the chat TUI state projection and its `session/event` listener.

use std::sync::{Arc, Mutex};

use cordis::Context;
use dsh_cli::tui::{attach_listener, ChatItem, ChatState};
use dsh_api::services::AgentRegistryService;
use dsh_core::AgentOptions;
use dsh_llm::{ContentBlock, MessageSource, Role, StreamChunk};
use dsh_session::{SessionEvent, SessionEventData, TurnEndReason, user_message};
use serde_json::json;

fn user_event(text: &str) -> dsh_session::SessionEvent {
    SessionEvent::new(
        0,
        0,
        SessionEventData::UserMessage {
            message: user_message("u-1", text),
        },
    )
}

fn chunk_event(text: &str) -> dsh_session::SessionEvent {
    SessionEvent::new(
        1,
        0,
        SessionEventData::AssistantChunk {
            turn: 1,
            step: 1,
            chunk: StreamChunk::TextDelta {
                index: 0,
                text: text.to_string(),
            },
        },
    )
}

fn assistant_event(text: &str) -> dsh_session::SessionEvent {
    SessionEvent::new(
        2,
        0,
        SessionEventData::AssistantMessage {
            turn: 1,
            step: 1,
            message: dsh_llm::Message {
                id: "a-1".into(),
                role: Role::Assistant,
                content: vec![ContentBlock::text(text)],
                source: MessageSource::Model {
                    provider: "mock".into(),
                    model: "mock-1".into(),
                },
            },
            usage: None,
            interrupted: None,
        },
    )
}

fn tool_call_event() -> dsh_session::SessionEvent {
    SessionEvent::new(
        3,
        0,
        SessionEventData::ToolCall {
            turn: 1,
            step: 2,
            call_id: "call-1".into(),
            name: "bash".into(),
            arguments: json!({ "command": "ls" }).to_string(),
        },
    )
}

fn tool_result_event(text: &str, is_error: bool) -> dsh_session::SessionEvent {
    SessionEvent::new(
        4,
        0,
        SessionEventData::ToolResult {
            turn: 1,
            step: 2,
            message: dsh_llm::Message {
                id: "tr-call-1".into(),
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_call_id: "call-1".into(),
                    content: vec![ContentBlock::text(text)],
                    is_error: Some(is_error),
                }],
                source: MessageSource::Tool {
                    tool: "bash".into(),
                },
            },
        },
    )
}

#[test]
fn state_projects_a_full_step() {
    let mut state = ChatState::default();

    // User message renders as a user item first.
    state.apply_event(&user_event("run ls"));
    assert_eq!(
        state.items.first(),
        Some(&ChatItem::User { text: "run ls".to_string() })
    );

    // Streaming deltas accumulate in `pending`.
    state.apply_event(&chunk_event("Hel"));
    state.apply_event(&chunk_event("lo"));
    assert_eq!(state.pending, "Hello");

    // The assembled assistant message finalizes the pending text.
    state.apply_event(&assistant_event("Hello"));
    assert!(state.pending.is_empty());
    assert_eq!(
        state.items.last(),
        Some(&ChatItem::Assistant { text: "Hello".to_string() })
    );

    // Tool call and tool result render as distinct items.
    state.apply_event(&tool_call_event());
    assert!(matches!(state.items.last(), Some(ChatItem::ToolCall { name, .. }) if name == "bash"));
    state.apply_event(&tool_result_event("bin\nlib", false));
    assert!(matches!(
        state.items.last(),
        Some(ChatItem::ToolResult { text, is_error: false }) if text == "bin\nlib"
    ));

    // Non-completed turn ends surface as a system note; completed ones do not.
    state.apply_event(&SessionEvent::new(
        5,
        0,
        SessionEventData::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Aborted { cause: "user".into() },
        },
    ));
    assert!(matches!(state.items.last(), Some(ChatItem::System { .. })));

    let mut state2 = ChatState::default();
    state2.apply_event(&assistant_event("ok"));
    state2.apply_event(&SessionEvent::new(
        0,
        0,
        SessionEventData::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        },
    ));
    assert_eq!(state2.items.len(), 1, "completed turns add no system item");
}

#[tokio::test]
async fn listener_folds_live_session_events() {
    let ctx = Context::new();
    let llm = ctx.plugin(dsh_llm::llm_plugin(), Some(json!({})));
    let sessions = ctx.plugin(dsh_session::session_plugin(), None);
    let tools = ctx.plugin(dsh_tools::tools_plugin(), Some(serde_json::Value::Null));
    let prompt = ctx.plugin(dsh_core::prompt::system_prompt_plugin(), None);
    let agent_loop = ctx.plugin(dsh_core::agent_loop_plugin(), None);
    for handle in [&llm, &sessions, &tools, &prompt, &agent_loop] {
        handle.join().await.unwrap();
    }

    // Script the mock adapter: one tool call, then the final answer.
    let runtime = ctx.require::<dsh_api::services::LlmService>("llm").unwrap();
    runtime.unregister_adapter(&["mock"]);
    runtime
        .register_adapter(
            &["mock"],
            Arc::new(dsh_llm::adapters::mock::MockAdapter::scripted(vec![
                dsh_llm::adapters::mock::MockAdapter::tool_call_response(
                    "call-1",
                    "bash",
                    json!({ "command": "echo tui-ok" }),
                ),
                dsh_llm::adapters::mock::MockAdapter::text_response("tui done"),
            ])),
        )
        .unwrap();

    let agents = ctx.require::<AgentRegistryService>("agents").unwrap();
    let agent = agents
        .create(None, AgentOptions::mock("mock-1"), Some("/tmp".to_string()), None)
        .unwrap();

    // Attach the TUI projection before any work happens.
    let state = Arc::new(Mutex::new(ChatState {
        session_id: Some(agent.id().to_string()),
        ..Default::default()
    }));
    attach_listener(&ctx, agent.id().to_string(), state.clone()).await.unwrap();

    agent.followup(dsh_core::agent::user_message_with_text("u-1", "run the tool"));
    agent.when_idle().await;

    let state = state.lock().unwrap();
    assert_eq!(state.session_id.as_deref(), Some(agent.id()));
    assert!(state.items.iter().any(|item| matches!(item, ChatItem::User { text } if text == "run the tool")));
    assert!(state
        .items
        .iter()
        .any(|item| matches!(item, ChatItem::ToolCall { name, .. } if name == "bash")));
    assert!(state
        .items
        .iter()
        .any(|item| matches!(item, ChatItem::ToolResult { text, .. } if text.contains("tui-ok"))));
    assert!(state
        .items
        .iter()
        .any(|item| matches!(item, ChatItem::Assistant { text } if text == "tui done")));
    assert!(state.pending.is_empty());
}
