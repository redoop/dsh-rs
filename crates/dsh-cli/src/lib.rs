//! Shared helpers for the dsh-rs headless runner.

pub mod tui;

use serde_json::Value;

/// Render the final assistant text of a session (the last assistant message).
pub fn last_assistant_text(session: &dsh_session::Session) -> String {
    let messages = session.derive_messages();
    messages
        .iter()
        .rev()
        .find(|m| m.role == dsh_llm::Role::Assistant)
        .map(|m| m.text())
        .unwrap_or_default()
}

/// Render a compact transcript of the session log.
pub fn transcript(session: &dsh_session::Session) -> String {
    session.render()
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
