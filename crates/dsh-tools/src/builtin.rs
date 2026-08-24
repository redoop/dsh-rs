//! Built-in tools registered by the `tools` plugin: `bash`, `read_file`,
//! `write_file`, `edit_file`, `glob`, and `grep`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cordis::Error;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use dsh_llm::ContentBlock;

use crate::matcher::glob_match;
use crate::registry::{ToolDefinition, ToolRegistry};
use crate::{ToolCallArgs, ToolExecutionResult, ToolRunContext};

/// Register every built-in tool on the registry.
pub fn register_builtin_tools(registry: &ToolRegistry) -> Result<(), Error> {
    registry.register(bash_tool());
    registry.register(read_file_tool());
    registry.register(write_file_tool());
    registry.register(edit_file_tool());
    registry.register(glob_tool());
    registry.register(grep_tool());
    registry.register(todo_write_tool());
    Ok(())
}

/// The `todo_write` tool: replaces the calling agent's whole todo list by
/// appending a `todo/write` session event through the (interface) session
/// service.
fn todo_write_tool() -> Arc<ToolDefinition> {
    let parameters = serde_json::json!({
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
    Arc::new(ToolDefinition::new(
        "todo_write",
        "Replace the agent's whole todo list. Use to track multi-step work; the list is overwritten on every call.",
        parameters,
        |args: ToolCallArgs, run_ctx: ToolRunContext| {
            Box::pin(async move {
                let parsed: serde_json::Value = match serde_json::from_value(args.arguments.clone()) {
                    Ok(parsed) => parsed,
                    Err(err) => return ToolExecutionResult::error("INVALID_ARGS", err.to_string()),
                };
                let Some(agent_id) = run_ctx.agent_id.clone() else {
                    return ToolExecutionResult::error("NO_AGENT", "tool called without an agent");
                };
                let Some(sessions) = run_ctx.ctx.get::<dsh_api::services::SessionService>("sessions")
                else {
                    return ToolExecutionResult::error("NO_SESSIONS", "sessions service unavailable");
                };
                let Some(session) = sessions.get(&agent_id) else {
                    return ToolExecutionResult::error(
                        "NO_SESSION",
                        format!("no live session for agent {agent_id}"),
                    );
                };
                let todos: Vec<dsh_types::TodoItem> = parsed
                    .get("todos")
                    .and_then(|t| t.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(|entry| dsh_types::TodoItem {
                                content: entry
                                    .get("content")
                                    .and_then(|c| c.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                status: match entry
                                    .get("status")
                                    .and_then(|s| s.as_str())
                                {
                                    Some("in_progress") => dsh_types::TodoStatus::InProgress,
                                    Some("completed") => dsh_types::TodoStatus::Completed,
                                    _ => dsh_types::TodoStatus::Pending,
                                },
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                session.append(dsh_types::SessionEventData::TodoWrite { todos: todos.clone() });
                ToolExecutionResult::success_text(
                    format!("todo list updated ({} items)", todos.len()),
                    serde_json::json!({ "todos": todos }),
                )
            })
        },
    ))
}

fn resolve_path(run_ctx: &ToolRunContext, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(cwd) = &run_ctx.cwd {
        Path::new(cwd).join(path)
    } else {
        path.to_path_buf()
    }
}

// ---------------------------------------------------------------------------
// bash
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    cwd: Option<String>,
}

fn bash_tool() -> Arc<ToolDefinition> {
    let parameters = json!({
        "type": "object",
        "properties": {
            "command": { "type": "string", "description": "The shell command to execute." },
            "timeout_ms": { "type": "integer", "description": "Optional timeout in milliseconds." },
            "cwd": { "type": "string", "description": "Optional working directory." }
        },
        "required": ["command"]
    });
    Arc::new(ToolDefinition::new(
        "bash",
        "Run a shell command and capture its output. Use for anything requiring a terminal, process execution, or system access.",
        parameters,
        |args: ToolCallArgs, run_ctx: ToolRunContext| {
            Box::pin(async move {
                let parsed: BashArgs = match serde_json::from_value(args.arguments) {
                    Ok(parsed) => parsed,
                    Err(err) => return ToolExecutionResult::error("INVALID_ARGS", err.to_string()),
                };
                let workdir = parsed
                    .cwd
                    .clone()
                    .map(|c| resolve_path(&run_ctx, &c))
                    .or_else(|| run_ctx.cwd.clone().map(PathBuf::from));
                let mut command = tokio::process::Command::new("sh");
                command.arg("-c").arg(&parsed.command);
                if let Some(dir) = &workdir {
                    command.current_dir(dir);
                }
                command.stdout(std::process::Stdio::piped());
                command.stderr(std::process::Stdio::piped());
                command.kill_on_drop(true);

                let child = match command.spawn() {
                    Ok(child) => child,
                    Err(err) => return ToolExecutionResult::error("SPAWN", err.to_string()),
                };

                let timeout = parsed.timeout_ms.map(Duration::from_millis);
                let wait_fut = async {
                    let output = child.wait_with_output().await?;
                    Ok::<_, std::io::Error>(output)
                };

                // Race cancellation against the (optionally bounded) wait.
                // `kill_on_drop(true)` kills the child whenever the wait
                // future is dropped — cancellation and timeout both drop it.
                let outcome = run_ctx
                    .signal
                    .race(async {
                        match timeout {
                            Some(d) => match tokio::time::timeout(d, wait_fut).await {
                                Ok(Ok(output)) => Ok(Some(output)),
                                Ok(Err(err)) => Err(err),
                                Err(_) => Ok(None), // timed out; child killed on drop
                            },
                            None => match wait_fut.await {
                                Ok(output) => Ok(Some(output)),
                                Err(err) => Err(err),
                            },
                        }
                    })
                    .await;

                let output = match outcome {
                    None => {
                        return ToolExecutionResult::error(
                            "CANCELLED",
                            format!("command cancelled: {}", parsed.command),
                        );
                    }
                    Some(Ok(Some(output))) => output,
                    Some(Ok(None)) => {
                        return ToolExecutionResult::error(
                            "TIMEOUT",
                            format!(
                                "command timed out after {}ms: {}",
                                parsed.timeout_ms.unwrap_or(0),
                                parsed.command
                            ),
                        );
                    }
                    Some(Err(err)) => {
                        return ToolExecutionResult::error("EXEC", err.to_string());
                    }
                };

                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let exit_code = output.status.code().unwrap_or(-1);
                let value = json!({
                    "stdout": stdout,
                    "stderr": stderr,
                    "exit_code": exit_code,
                });
                if output.status.success() {
                    let mut text = stdout.clone();
                    if !stderr.is_empty() {
                        text.push_str(&format!("\n[stderr]\n{stderr}"));
                    }
                    ToolExecutionResult::success_text(text.trim().to_string(), value)
                } else {
                    let message = format!(
                        "command exited with code {exit_code}\n{stdout}\n[stderr]\n{stderr}"
                    );
                    ToolExecutionResult::Error {
                        message: message.trim().to_string(),
                        code: "EXIT".to_string(),
                        content: vec![ContentBlock::text(message.trim().to_string())],
                    }
                }
            })
        },
    ))
}

// ---------------------------------------------------------------------------
// read_file
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
}

fn read_file_tool() -> Arc<ToolDefinition> {
    let parameters = json!({
        "type": "object",
        "properties": { "path": { "type": "string", "description": "Path of the file to read." } },
        "required": ["path"]
    });
    Arc::new(ToolDefinition::new(
        "read_file",
        "Read a text file and return its full contents.",
        parameters,
        |args: ToolCallArgs, run_ctx: ToolRunContext| {
            Box::pin(async move {
                let parsed: ReadArgs = match serde_json::from_value(args.arguments) {
                    Ok(parsed) => parsed,
                    Err(err) => return ToolExecutionResult::error("INVALID_ARGS", err.to_string()),
                };
                let path = resolve_path(&run_ctx, &parsed.path);
                match tokio::fs::read_to_string(&path).await {
                    Ok(content) => {
                        let value = json!({ "path": path.display().to_string(), "content": content });
                        ToolExecutionResult::success_text(content, value)
                    }
                    Err(err) => ToolExecutionResult::error(
                        "READ_FAILED",
                        format!("cannot read {}: {err}", path.display()),
                    ),
                }
            })
        },
    ))
}

// ---------------------------------------------------------------------------
// write_file
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

fn write_file_tool() -> Arc<ToolDefinition> {
    let parameters = json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path of the file to write." },
            "content": { "type": "string", "description": "Full new content of the file." }
        },
        "required": ["path", "content"]
    });
    Arc::new(ToolDefinition::new(
        "write_file",
        "Write a file with the given content, creating parent directories.",
        parameters,
        |args: ToolCallArgs, run_ctx: ToolRunContext| {
            Box::pin(async move {
                let parsed: WriteArgs = match serde_json::from_value(args.arguments) {
                    Ok(parsed) => parsed,
                    Err(err) => return ToolExecutionResult::error("INVALID_ARGS", err.to_string()),
                };
                let path = resolve_path(&run_ctx, &parsed.path);
                if let Some(parent) = path.parent() {
                    if let Err(err) = tokio::fs::create_dir_all(parent).await {
                        return ToolExecutionResult::error(
                            "MKDIR_FAILED",
                            format!("cannot create {}: {err}", parent.display()),
                        );
                    }
                }
                match tokio::fs::write(&path, &parsed.content).await {
                    Ok(()) => {
                        let value = json!({ "path": path.display().to_string(), "bytes": parsed.content.len() });
                        ToolExecutionResult::success_text(
                            format!("wrote {} bytes to {}", parsed.content.len(), path.display()),
                            value,
                        )
                    }
                    Err(err) => ToolExecutionResult::error(
                        "WRITE_FAILED",
                        format!("cannot write {}: {err}", path.display()),
                    ),
                }
            })
        },
    ))
}

// ---------------------------------------------------------------------------
// edit_file
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

fn edit_file_tool() -> Arc<ToolDefinition> {
    let parameters = json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path of the file to edit." },
            "old_string": { "type": "string", "description": "Text to find." },
            "new_string": { "type": "string", "description": "Replacement text." },
            "replace_all": { "type": "boolean", "description": "Replace every occurrence (default false)." }
        },
        "required": ["path", "old_string", "new_string"]
    });
    Arc::new(ToolDefinition::new(
        "edit_file",
        "Replace old_string with new_string in a file. Fails when old_string is absent.",
        parameters,
        |args: ToolCallArgs, run_ctx: ToolRunContext| {
            Box::pin(async move {
                let parsed: EditArgs = match serde_json::from_value(args.arguments) {
                    Ok(parsed) => parsed,
                    Err(err) => return ToolExecutionResult::error("INVALID_ARGS", err.to_string()),
                };
                let path = resolve_path(&run_ctx, &parsed.path);
                let content = match tokio::fs::read_to_string(&path).await {
                    Ok(content) => content,
                    Err(err) => {
                        return ToolExecutionResult::error(
                            "READ_FAILED",
                            format!("cannot read {}: {err}", path.display()),
                        );
                    }
                };
                let updated = if parsed.replace_all {
                    if !content.contains(&parsed.old_string) {
                        return ToolExecutionResult::error(
                            "NOT_FOUND",
                            format!("old_string not found in {}", path.display()),
                        );
                    }
                    content.replace(&parsed.old_string, &parsed.new_string)
                } else {
                    match content.find(&parsed.old_string) {
                        Some(_) => content.replacen(&parsed.old_string, &parsed.new_string, 1),
                        None => {
                            return ToolExecutionResult::error(
                                "NOT_FOUND",
                                format!("old_string not found in {}", path.display()),
                            );
                        }
                    }
                };
                match tokio::fs::write(&path, &updated).await {
                    Ok(()) => {
                        let value = json!({ "path": path.display().to_string(), "bytes": updated.len() });
                        ToolExecutionResult::success_text(
                            format!("edited {}", path.display()),
                            value,
                        )
                    }
                    Err(err) => ToolExecutionResult::error("WRITE_FAILED", err.to_string()),
                }
            })
        },
    ))
}

// ---------------------------------------------------------------------------
// glob
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Serialize)]
struct GlobResult {
    matches: Vec<String>,
    count: usize,
}

fn glob_tool() -> Arc<ToolDefinition> {
    let parameters = json!({
        "type": "object",
        "properties": {
            "pattern": { "type": "string", "description": "Glob pattern; ** crosses directories." },
            "path": { "type": "string", "description": "Base directory (default: cwd)." }
        },
        "required": ["pattern"]
    });
    Arc::new(ToolDefinition::new(
        "glob",
        "List files matching a glob pattern under a directory.",
        parameters,
        |args: ToolCallArgs, run_ctx: ToolRunContext| {
            Box::pin(async move {
                let parsed: GlobArgs = match serde_json::from_value(args.arguments) {
                    Ok(parsed) => parsed,
                    Err(err) => return ToolExecutionResult::error("INVALID_ARGS", err.to_string()),
                };
                let base = parsed
                    .path
                    .map(|p| resolve_path(&run_ctx, &p))
                    .or_else(|| run_ctx.cwd.clone().map(PathBuf::from))
                    .unwrap_or_else(|| PathBuf::from("."));
                let pattern = parsed.pattern.clone();
                let matches = walk_and_match(&base, &pattern).await;
                let relative: Vec<String> = matches
                    .iter()
                    .map(|m| {
                        let rel = m.strip_prefix(&base).unwrap_or(m);
                        let rel = rel.to_string_lossy().replace('\\', "/");
                        rel.trim_start_matches('/').to_string()
                    })
                    .collect();
                let value = serde_json::to_value(&GlobResult {
                    count: relative.len(),
                    matches: relative.clone(),
                })
                .unwrap_or(Value::Null);
                let text = if relative.is_empty() {
                    format!("no files match {pattern}")
                } else {
                    relative.join("\n")
                };
                ToolExecutionResult::success_text(text, value)
            })
        },
    ))
}

async fn walk_and_match(base: &Path, pattern: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else { continue };
        let mut collected = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            collected.push(entry);
        }
        // Sort for determinism.
        collected.sort_by_key(|e| e.file_name());
        for entry in collected {
            let path = entry.path();
            let Ok(meta) = entry.metadata().await else { continue };
            if meta.is_dir() {
                stack.push(path.clone());
            }
            if let Ok(relative) = path.strip_prefix(base) {
                let rel = relative.to_string_lossy().replace('\\', "/");
                if glob_match(pattern, &rel) {
                    out.push(path);
                }
            }
        }
    }
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    include: Option<String>,
}

#[derive(Serialize)]
struct GrepMatch {
    path: String,
    line: u64,
    text: String,
}

fn grep_tool() -> Arc<ToolDefinition> {
    let parameters = json!({
        "type": "object",
        "properties": {
            "pattern": { "type": "string", "description": "Regular expression to search for." },
            "path": { "type": "string", "description": "File or directory to search (default: cwd)." },
            "include": { "type": "string", "description": "Optional glob filter over file names." }
        },
        "required": ["pattern"]
    });
    Arc::new(ToolDefinition::new(
        "grep",
        "Search files for lines matching a regular expression, returning matches with line numbers.",
        parameters,
        |args: ToolCallArgs, run_ctx: ToolRunContext| {
            Box::pin(async move {
                let parsed: GrepArgs = match serde_json::from_value(args.arguments) {
                    Ok(parsed) => parsed,
                    Err(err) => return ToolExecutionResult::error("INVALID_ARGS", err.to_string()),
                };
                let re = match regex::Regex::new(&parsed.pattern) {
                    Ok(re) => re,
                    Err(err) => {
                        return ToolExecutionResult::error("BAD_REGEX", err.to_string());
                    }
                };
                let base = parsed
                    .path
                    .map(|p| resolve_path(&run_ctx, &p))
                    .or_else(|| run_ctx.cwd.clone().map(PathBuf::from))
                    .unwrap_or_else(|| PathBuf::from("."));
                let files = collect_files(&base).await;
                let mut matches: Vec<GrepMatch> = Vec::new();
                for file in files {
                    if let Some(include) = &parsed.include {
                        let name = file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        if !glob_match(include, &name) {
                            continue;
                        }
                    }
                    let Ok(content) = tokio::fs::read_to_string(&file).await else { continue };
                    for (idx, line) in content.lines().enumerate() {
                        if re.is_match(line) {
                            matches.push(GrepMatch {
                                path: file.display().to_string(),
                                line: (idx + 1) as u64,
                                text: line.chars().take(300).collect(),
                            });
                            if matches.len() >= 500 {
                                break;
                            }
                        }
                    }
                    if matches.len() >= 500 {
                        break;
                    }
                }
                let value = serde_json::to_value(&matches).unwrap_or(Value::Null);
                let text = if matches.is_empty() {
                    format!("no matches for {}", parsed.pattern)
                } else {
                    matches
                        .iter()
                        .map(|m| format!("{}:{}:{}", m.path, m.line, m.text))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                ToolExecutionResult::success_text(text, value)
            })
        },
    ))
}

async fn collect_files(base: &Path) -> Vec<PathBuf> {
    if base.is_file() {
        return vec![base.to_path_buf()];
    }
    let mut out = Vec::new();
    let mut stack = vec![base.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else { continue };
        let mut collected = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            collected.push(entry);
        }
        collected.sort_by_key(|e| e.file_name());
        for entry in collected {
            let path = entry.path();
            let Ok(meta) = entry.metadata().await else { continue };
            if meta.is_dir() {
                stack.push(path.clone());
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}
