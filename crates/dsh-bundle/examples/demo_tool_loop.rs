//! demo_tool_loop — 演示 dsh-rs 的 agent loop 完整工具调用流程。
//!
//! 步骤：
//!   1. 组装 base bundle（llm / sessions / tools / system-prompt / agent-loop）
//!   2. 把 mock 适配器脚本化为两轮响应：先请求调用 bash 工具，再给出最终回答
//!   3. 创建 agent、发送提示词、等待收敛
//!   4. 打印会话日志（turn -> tool/call -> tool/result -> assistant -> turn/end）
//!
//! 运行：cargo run -p dsh-bundle --example demo_tool_loop

use std::sync::Arc;

use cordis::Context;
use dsh_core::agent::user_message_with_text;
use dsh_core::{AgentOptions, AgentRegistry};
use dsh_llm::LlmRuntime;
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), String> {
    let ctx = Context::new();

    // 1. 组装 base bundle —— 一切皆插件，内核自动收敛依赖图。
    let handles = dsh_bundle::install_base_default(&ctx).await?;
    println!("[boot] {} plugins converged", handles.len());

    // 2. 脚本化 mock 适配器：第一轮请求 bash 工具，第二轮给出最终回答。
    let runtime = ctx.require::<LlmRuntime>("llm").map_err(|e| e.to_string())?;
    runtime.unregister_adapter(&["mock"]);
    runtime
        .register_adapter(
            &["mock"],
            Arc::new(dsh_llm::adapters::mock::MockAdapter::scripted(vec![
                dsh_llm::adapters::mock::MockAdapter::tool_call_response(
                    "call-1",
                    "bash",
                    json!({ "command": "echo 'Hello from bash!' && uname -s" }),
                ),
                dsh_llm::adapters::mock::MockAdapter::text_response(
                    "工具已执行完成，输出为：Hello from bash! / Darwin",
                ),
            ])),
        )
        .map_err(|e| e.to_string())?;
    println!("[mock] scripted: tool-call -> final answer");

    // 3. 创建 agent 并发送提示词。
    let agents = ctx
        .require::<AgentRegistry>("agents")
        .map_err(|e| e.to_string())?;
    let agent = agents
        .create(None, AgentOptions::mock("mock-1"), Some("/tmp".to_string()), None)
        .map_err(|e| e.to_string())?;
    println!("[agent] created: {}", agent.id);

    agent.followup(user_message_with_text("u-1", "请执行一条 shell 命令并告诉我结果"));
    agent.when_idle().await;

    // 4. 打印会话日志。
    println!("\n--- session transcript ---");
    for event in agent.session.events() {
        println!("{}", dsh_session::Session::render_event(&event));
    }

    println!("\n--- 最终回答 ---");
    let messages = agent.session.derive_messages();
    if let Some(last) = messages.iter().rev().find(|m| m.role == dsh_llm::Role::Assistant) {
        println!("{}", last.text());
    }

    let _ = ctx;
    Ok(())
}
