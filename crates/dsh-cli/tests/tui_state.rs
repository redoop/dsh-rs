//! Tests for the chat TUI state projection and its `session/event` listener.

use std::sync::{Arc, Mutex};

use cordis::Context;
use dsh_cli::tui::{attach_listener, ChatItem, ChatState};
use dsh_types::{
    ContentBlock, Message, MessageSource, Role, SessionEvent, SessionEventData, StreamChunk,
    TurnEndReason,
};
use serde_json::json;

mod common;

fn user_event(text: &str) -> SessionEvent {
    SessionEvent::new(
        0,
        0,
        SessionEventData::UserMessage {
            message: Message::user("u-1", vec![ContentBlock::text(text)]),
        },
    )
}

fn chunk_event(text: &str) -> SessionEvent {
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

fn assistant_event(text: &str) -> SessionEvent {
    SessionEvent::new(
        2,
        0,
        SessionEventData::AssistantMessage {
            turn: 1,
            step: 1,
            message: Message {
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

fn tool_call_event() -> SessionEvent {
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

fn tool_result_event(text: &str, is_error: bool) -> SessionEvent {
    SessionEvent::new(
        4,
        0,
        SessionEventData::ToolResult {
            turn: 1,
            step: 2,
            message: Message {
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
    common::boot_scripted(
        &ctx,
        true,
        vec![
            common::tool_call_response("call-1", "bash", json!({ "command": "echo tui-ok" })),
            common::text_response("tui done"),
        ],
    )
    .await;

    let agent = common::create_agent(&ctx).await;

    // Attach the TUI projection before any work happens.
    let state = Arc::new(Mutex::new(ChatState {
        session_id: Some(agent.id().to_string()),
        ..Default::default()
    }));
    attach_listener(&ctx, agent.id().to_string(), state.clone()).await.unwrap();

    agent.followup(dsh_cli::user_message_with_text("u-1", "run the tool"));
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
