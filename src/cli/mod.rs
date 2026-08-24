//! Shared helpers for the dsh-rs headless runner.
//!
//! This crate is a pure **consumer** of the interface layer: it depends only
//! on `dsh-api` (service wrappers), `dsh-types` (shared vocabulary), and
//! `dsh-bundle` (base-bundle composition). It never imports implementation
//! crate types.

pub mod tui;

use crate::types::{
    ContentBlock, Message, MessageSource, Role, SessionEvent, SessionEventData, TurnEndReason,
    now_ms,
};
use serde_json::Value;

/// Build a user-role message for the model (consumer-side helper).
pub fn user_message_with_text(id: impl Into<String>, text: impl Into<String>) -> Message {
    Message {
        id: id.into(),
        role: Role::User,
        content: vec![ContentBlock::text(text)],
        source: MessageSource::User,
    }
}

/// Render the final assistant text of a session (the last assistant message).
pub fn last_assistant_text(session: &dyn crate::api::services::SessionView) -> String {
    let messages = session.derive_messages();
    messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| m.text())
        .unwrap_or_default()
}

/// One-line textual rendering of a session event (transcript display).
pub fn render_event(event: &SessionEvent) -> String {
    match &event.data {
        SessionEventData::UserMessage { message } => format!("[user] {}", message.text()),
        SessionEventData::AssistantMessage { message, .. } => {
            format!("[assistant] {}", message.text())
        }
        SessionEventData::ToolCall { name, arguments, .. } => {
            let parsed: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
            format!("[tool-call] {name} {parsed}")
        }
        SessionEventData::ToolResult { message, .. } => {
            format!("[tool-result] {}", message.text())
        }
        SessionEventData::TurnStart { .. } => "[turn/start]".to_string(),
        SessionEventData::TurnEnd { reason, .. } => format!("[turn/end] {reason:?}"),
        SessionEventData::StepStart { .. } => "[step/start]".to_string(),
        SessionEventData::StepEnd { .. } => "[step/end]".to_string(),
        SessionEventData::AssistantChunk { chunk, .. } => format!("[assistant/chunk] {chunk:?}"),
        SessionEventData::TodoWrite { todos } => format!("[todo/write] {todos:?}"),
        SessionEventData::RequestHeader { .. } => "[request/header]".to_string(),
        SessionEventData::SessionEndSeed => "[session/end-seed]".to_string(),
    }
}

/// Close a crash-orphaned trailing turn with `turn/end { interrupted }`.
/// Returns whether a repair was made (idempotent). Mirrors the persistence
/// crate's reload repair, expressed over the shared vocabulary only.
pub fn repair_crash_turns(events: &mut Vec<SessionEvent>) -> bool {
    let mut open: Option<u64> = None;
    for event in events.iter() {
        match &event.data {
            SessionEventData::TurnStart { turn } => open = Some(*turn),
            SessionEventData::TurnEnd { .. } => open = None,
            _ => {}
        }
    }
    let Some(turn) = open else { return false };
    events.push(SessionEvent::new(
        events.len() as u64,
        now_ms(),
        SessionEventData::TurnEnd {
            turn,
            reason: TurnEndReason::Interrupted,
        },
    ));
    true
}

/// Parse `--key value` style arguments. Returns (flags, positionals).
///
/// A flag whose next token is itself a flag (e.g. `--line --provider mock`)
/// is treated as a bare flag, and the next token is parsed normally — it is
/// never swallowed as a value.
pub fn parse_args(args: &[String]) -> (std::collections::HashMap<String, String>, Vec<String>) {
    let mut flags = std::collections::HashMap::new();
    let mut positionals = Vec::new();
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        if let Some(key) = arg.strip_prefix("--") {
            let key = key.to_string();
            // --flag=value or --flag value
            if let Some((k, v)) = key.split_once('=') {
                flags.insert(k.to_string(), v.to_string());
            } else if iter.peek().is_some_and(|next| !next.starts_with("--")) {
                flags.insert(key, iter.next().unwrap().clone());
            } else {
                flags.insert(key, String::new());
            }
        } else {
            positionals.push(arg.clone());
        }
    }
    (flags, positionals)
}

/// Load a JSON profile from a file path or inline JSON.
pub fn load_profile(source: &str) -> Result<Value, String> {
    let trimmed = source.trim();
    if trimmed.starts_with('{') {
        return serde_json::from_str(trimmed).map_err(|e| format!("bad profile JSON: {e}"));
    }
    let text = std::fs::read_to_string(source).map_err(|e| format!("cannot read {source}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("bad profile JSON in {source}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::parse_args;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flag_followed_by_another_flag_is_not_swallowed() {
        // Regression: `--line --provider mock` must keep both flags; the
        // second flag used to be eaten as `--line`'s value.
        let (flags, positionals) = parse_args(&args(&["chat", "--line", "--provider", "mock", "--model", "mock-1"]));
        assert_eq!(flags.get("line").map(|s| s.as_str()), Some(""));
        assert_eq!(flags.get("provider").map(|s| s.as_str()), Some("mock"));
        assert_eq!(flags.get("model").map(|s| s.as_str()), Some("mock-1"));
        assert_eq!(positionals, vec!["chat"]);
    }

    #[test]
    fn flag_value_and_equals_forms() {
        let (flags, positionals) = parse_args(&args(&["run", "--prompt", "hi", "--max-tokens=64", "--print-json"]));
        assert_eq!(flags.get("prompt").map(|s| s.as_str()), Some("hi"));
        assert_eq!(flags.get("max-tokens").map(|s| s.as_str()), Some("64"));
        assert!(flags.contains_key("print-json"));
        assert_eq!(positionals, vec!["run"]);
    }

    #[test]
    fn trailing_bare_flag_is_ok() {
        let (flags, positionals) = parse_args(&args(&["chat", "--line"]));
        assert!(flags.contains_key("line"));
        assert_eq!(positionals, vec!["chat"]);
    }
}