//! The `todo_write` tool: writes the whole-list `todo/write` snapshot to the
//! calling agent's session log.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::json;

use dsh_session::{SessionEventData, SessionStore, TodoItem, TodoStatus};
use dsh_tools::{
    TOOLS_SERVICE, ToolCallArgs, ToolDefinition, ToolExecutionResult, ToolRegistry, ToolRunContext,
};

#[derive(Deserialize)]
struct TodoWriteArgs {
    todos: Vec<TodoEntry>,
}

#[derive(Deserialize)]
struct TodoEntry {
    content: String,
    #[serde(default)]
    status: Option<String>,
}

/// Register the `todo_write` tool on a registry.
pub fn register_todo_tool(registry: &ToolRegistry) {
    let parameters = json!({
        "type": "object",
        "properties": {
            "todos": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "content": { "type": "string" },
                        "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] }
                    },
                    "required": ["content"]
                }
            }
        },
        "required": ["todos"]
    });
    let tool = ToolDefinition::new(
        "todo_write",
        "Replace the agent's whole todo list. Use to track multi-step work; the list is overwritten on every call.",
        parameters,
        |args: ToolCallArgs, run_ctx: ToolRunContext| {
            Box::pin(async move {
                let parsed: TodoWriteArgs = match serde_json::from_value(args.arguments) {
                    Ok(parsed) => parsed,
                    Err(err) => return ToolExecutionResult::error("INVALID_ARGS", err.to_string()),
                };
                let Some(agent_id) = run_ctx.agent_id.clone() else {
                    return ToolExecutionResult::error("NO_AGENT", "tool called without an agent");
                };
                let Some(sessions) = run_ctx.ctx.get::<SessionStore>("sessions") else {
                    return ToolExecutionResult::error("NO_SESSIONS", "sessions service unavailable");
                };
                let Some(session) = sessions.get(&agent_id) else {
                    return ToolExecutionResult::error(
                        "NO_SESSION",
                        format!("no live session for agent {agent_id}"),
                    );
                };
                let todos: Vec<TodoItem> = parsed
                    .todos
                    .into_iter()
                    .map(|entry| TodoItem {
                        content: entry.content,
                        status: match entry.status.as_deref() {
                            Some("in_progress") => TodoStatus::InProgress,
                            Some("completed") => TodoStatus::Completed,
                            _ => TodoStatus::Pending,
                        },
                    })
                    .collect();
                session.append(SessionEventData::TodoWrite { todos: todos.clone() });
                ToolExecutionResult::success_text(
                    format!("todo list updated ({} items)", todos.len()),
                    json!({ "todos": todos }),
                )
            })
        },
    );
    registry.register(Arc::new(tool));
}

/// Convenience: register the todo tool on the `tools` service of `ctx`.
pub fn register_todo_tool_on_ctx(ctx: &cordis::Context) -> Result<(), cordis::Error> {
    let tools = ctx
        .get::<ToolRegistry>(TOOLS_SERVICE)
        .ok_or_else(|| cordis::Error::msg("tools service missing"))?;
    register_todo_tool(&tools);
    Ok(())
}
