//! demo_tool_loop — 演示 dsh-rs 的 agent loop 完整工具调用流程。
//!
//! 步骤：
//!   1. 组装 base bundle（manifest / llm / sessions / tools / system-prompt /
//!      agent-loop）
//!   2. 通过 **接口**（`dsh-api` 的 `LlmService` / `AgentRegistryService`）
//!      脚本化 mock 适配器：先请求调用 bash 工具，再给出最终回答
//!   3. 创建 agent、发送提示词、等待收敛
//!   4. 打印会话日志（turn -> tool/call -> tool/result -> assistant -> turn/end）
//!
//! 运行：cargo run -p dsh-bundle --example demo_tool_loop

use std::sync::Arc;

use cordis::Context;
use dsh_api::services::{LlmAdapterApi, LlmService};
use dsh_types::{
    AgentOptions, ContentBlock, FinishReason, GenerateOptions, Role, StreamChunk,
    stream_from_chunks,
};
use serde_json::{json, Value};

#[tokio::main]
async fn main() -> Result<(), String> {
    let ctx = Context::new();

    // 1. 组装 base bundle —— 一切皆插件，内核自动收敛依赖图。
    let handles = dsh_bundle::install_base_default(&ctx).await?;
    println!("[boot] {} plugins converged", handles.len());

    // 2. 脚本化 mock 适配器：第一轮请求 bash 工具，第二轮给出最终回答。
    let runtime = ctx.require::<LlmService>("llm").map_err(|e| e.to_string())?;
    runtime.unregister_adapter(&["mock"]);
    runtime
        .register_adapter(
            &["mock"],
            Arc::new(ScriptedAdapter::new(vec![
                tool_call_chunks(
                    "call-1",
                    "bash",
                    json!({ "command": "echo 'Hello from bash!' && uname -s" }),
                ),
                text_chunks("工具已执行完成，输出为：Hello from bash! / Darwin"),
            ])),
        )
        .map_err(|e| e.to_string())?;
    println!("[mock] scripted: tool-call -> final answer");

    // 3. 创建 agent 并发送提示词。
    let agents = ctx
        .require::<dsh_api::services::AgentRegistryService>("agents")
        .map_err(|e| e.to_string())?;
    let agent = agents
        .create(None, AgentOptions::mock("mock-1"), Some("/tmp".to_string()), None)
        .map_err(|e| e.to_string())?;
    println!("[agent] created: {}", agent.id());

    agent.followup(dsh_types::Message::user(
        "u-1",
        vec![ContentBlock::text("请执行一条 shell 命令并告诉我结果")],
    ));
    agent.when_idle().await;

    // 4. 打印会话日志。
    println!("\n--- session transcript ---");
    for event in agent.session().events() {
        println!("{}", render_event(&event));
    }

    println!("\n--- 最终回答 ---");
    let messages = agent.session().derive_messages();
    if let Some(last) = messages.iter().rev().find(|m| m.role == Role::Assistant) {
        println!("{}", last.text());
    }

    let _ = ctx;
    Ok(())
}

/// One-line textual rendering of a session event (mirrors the CLI helper).
fn render_event(event: &dsh_types::SessionEvent) -> String {
    match &event.data {
        dsh_types::SessionEventData::UserMessage { message } => {
            format!("[user] {}", message.text())
        }
        dsh_types::SessionEventData::AssistantMessage { message, .. } => {
            format!("[assistant] {}", message.text())
        }
        dsh_types::SessionEventData::ToolCall { name, arguments, .. } => {
            let parsed: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
            format!("[tool-call] {name} {parsed}")
        }
        dsh_types::SessionEventData::ToolResult { message, .. } => {
            format!("[tool-result] {}", message.text())
        }
        dsh_types::SessionEventData::TurnStart { .. } => "[turn/start]".to_string(),
        dsh_types::SessionEventData::TurnEnd { reason, .. } => format!("[turn/end] {reason:?}"),
        dsh_types::SessionEventData::StepStart { .. } => "[step/start]".to_string(),
        dsh_types::SessionEventData::StepEnd { .. } => "[step/end]".to_string(),
        dsh_types::SessionEventData::AssistantChunk { chunk, .. } => {
            format!("[assistant/chunk] {chunk:?}")
        }
        dsh_types::SessionEventData::TodoWrite { todos } => format!("[todo/write] {todos:?}"),
        dsh_types::SessionEventData::RequestHeader { .. } => "[request/header]".to_string(),
        dsh_types::SessionEventData::SessionEndSeed => "[session/end-seed]".to_string(),
    }
}

/// One assistant text-block response as raw chunks.
fn text_chunks(text: &str) -> Vec<StreamChunk> {
    vec![
        StreamChunk::BlockStart {
            index: 0,
            block_type: "text".to_string(),
        },
        StreamChunk::TextDelta {
            index: 0,
            text: text.to_string(),
        },
        StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::Text {
                text: text.to_string(),
            },
        },
        StreamChunk::Finish {
            reason: FinishReason::Stop,
        },
    ]
}

/// One tool-call response as raw chunks.
fn tool_call_chunks(id: &str, name: &str, arguments: Value) -> Vec<StreamChunk> {
    let arguments = arguments.to_string();
    vec![
        StreamChunk::BlockStart {
            index: 0,
            block_type: "tool-call".to_string(),
        },
        StreamChunk::ToolCallDelta {
            index: 0,
            id: id.to_string(),
            name: Some(name.to_string()),
            arguments_delta: arguments.clone(),
        },
        StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments,
            },
        },
        StreamChunk::Finish {
            reason: FinishReason::ToolCalls,
        },
    ]
}

/// A minimal scripted adapter implementing the LLM interface: pops one chunk
/// sequence per request; without a script it echoes the last user message.
struct ScriptedAdapter {
    script: std::sync::Mutex<std::collections::VecDeque<Vec<StreamChunk>>>,
}

impl ScriptedAdapter {
    fn new(script: Vec<Vec<StreamChunk>>) -> Self {
        ScriptedAdapter {
            script: std::sync::Mutex::new(script.into()),
        }
    }
}

impl LlmAdapterApi for ScriptedAdapter {
    fn name(&self) -> &'static str {
        "scripted"
    }

    fn stream(
        &self,
        options: GenerateOptions,
    ) -> dsh_api::services::BoxFuture<
        Result<dsh_types::BoxStream<StreamChunk>, dsh_types::LlmError>,
    > {
        let scripted = self.script.lock().unwrap().pop_front();
        Box::pin(async move {
            let chunks = match scripted {
                Some(script) => script,
                None => {
                    let echoed = options
                        .messages
                        .iter()
                        .rev()
                        .find(|m| m.role == Role::User)
                        .map(|m| m.text())
                        .unwrap_or_default();
                    text_chunks(&echoed)
                }
            };
            Ok(stream_from_chunks(chunks))
        })
    }
}