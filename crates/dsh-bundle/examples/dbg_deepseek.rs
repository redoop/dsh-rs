//! dbg_deepseek — 直接驱动 LLM 适配器，输出每个 chunk / 错误，便于排查
//! 真实提供者的对接问题。运行：cargo run -p dsh-bundle --example dbg_deepseek
//!
//! 消费者侧只使用 `dsh-api` 的服务 wrapper（`LlmService`）与 `dsh-types`
//! 词汇（chunk 组装仍复用实现 crate 的 `BlockAssembler`，bundle 本就依赖它）。

use cordis::Context;
use dsh_api::services::LlmService;
use dsh_llm::BlockAssembler;
use dsh_types::{ContentBlock, GenerateOptions, Message, StreamChunk, ToolSchema};

#[tokio::main]
async fn main() -> Result<(), String> {
    // 读取仓库根的 dsh.json（与 CLI 相同的自动发现路径）。
    let text = std::fs::read_to_string("dsh.json").map_err(|e| e.to_string())?;
    let profile: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let config = dsh_bundle::BaseConfig::from_value(&profile);

    let ctx = Context::new();
    dsh_bundle::install_base(&ctx, config).await?;

    let runtime = ctx.require::<LlmService>("llm").map_err(|e| e.to_string())?;
    println!("providers: {:?}", runtime.list_providers());

    let tools = std::env::var("DSH_TOOLS").map(|_| true).unwrap_or(false);
    let with_system = std::env::var("DSH_SYSTEM").map(|_| true).unwrap_or(false);
    let options = GenerateOptions {
        provider: "deepseek".to_string(),
        model: "deepseek-v4-flash".to_string(),
        messages: vec![Message::user("u-1", vec![ContentBlock::text("hi")])],
        system: if with_system { Some("You are a helpful assistant.".to_string()) } else { None },
        tools: if tools {
            Some(vec![
                ToolSchema {
                    name: "bash".to_string(),
                    description: "Run a shell command.".to_string(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": { "command": { "type": "string" } },
                        "required": ["command"]
                    }),
                },
            ])
        } else {
            None
        },
        temperature: None,
        max_tokens: Some(64),
        stop: None,
        session_id: None,
    };

    println!("--- calling deepseek ... ---");
    let stream = runtime
        .stream(options)
        .await
        .map_err(|e| format!("stream() error: {e}"))?;
    let mut stream = std::pin::pin!(stream);
    let mut assembler = BlockAssembler::new();
    let mut count = 0;
    while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
        count += 1;
        println!("chunk[{count}]: {chunk:?}");
        assembler.push(&chunk);
        if matches!(chunk, StreamChunk::Finish { .. }) {
            break;
        }
    }
    println!("--- total chunks: {count} ---");
    println!("finish: {:?}", assembler.finish());
    println!("blocks: {:?}", assembler.blocks());
    Ok(())
}