use std::sync::Arc;

use dsh_api::services::LlmService;
use dsh_llm::{
    llm_plugin, stream_via_waterfall, BlockAssembler, CancelToken, ContentBlock, FinishReason,
    GenerateOptions, LlmAdapter, LlmRuntime, Message, MessageSource, Role, StreamChunk,
    StreamTable, TokenUsage,
};

fn text_delta(index: usize, text: &str) -> StreamChunk {
    StreamChunk::TextDelta {
        index,
        text: text.to_string(),
    }
}

#[test]
fn assembler_folds_delta_only_text() {
    let mut a = BlockAssembler::new();
    a.push(&text_delta(0, "Hello"));
    a.push(&text_delta(0, ", world"));
    a.push(&StreamChunk::Finish { reason: FinishReason::Stop });
    let blocks = a.blocks();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0], ContentBlock::Text { text: "Hello, world".into() });
    assert_eq!(a.finish(), FinishReason::Stop);
}

#[test]
fn assembler_folds_tool_call_deltas() {
    let mut a = BlockAssembler::new();
    a.push(&StreamChunk::BlockStart {
        index: 0,
        block_type: "tool-call".into(),
    });
    a.push(&StreamChunk::ToolCallDelta {
        index: 0,
        id: "call-1".into(),
        name: Some("bash".into()),
        arguments_delta: "{\"comma".into(),
    });
    a.push(&StreamChunk::ToolCallDelta {
        index: 0,
        id: "call-1".into(),
        name: None,
        arguments_delta: "nd\":\"ls\"}".into(),
    });
    let blocks = a.blocks();
    assert_eq!(
        blocks[0],
        ContentBlock::ToolCall {
            id: "call-1".into(),
            name: "bash".into(),
            arguments: "{\"command\":\"ls\"}".into(),
        }
    );
}

#[test]
fn assembler_max_tokens_drops_tool_calls() {
    let mut a = BlockAssembler::new();
    let chunks = dsh_llm::adapters::mock::MockAdapter::tool_call_response(
        "call-1",
        "bash",
        serde_json::json!({ "command": "ls" }),
    );
    for chunk in chunks.into_iter().filter(|c| !matches!(c, StreamChunk::Finish { .. })) {
        a.push(&chunk);
    }
    a.push(&StreamChunk::Finish {
        reason: FinishReason::MaxTokens,
    });
    assert!(a.blocks().is_empty(), "truncated tool calls must be dropped");
}

#[test]
fn assembler_interrupted_blocks_keep_text_only() {
    let mut a = BlockAssembler::new();
    a.push(&text_delta(0, "partial output"));
    a.push(&dsh_llm::adapters::mock::MockAdapter::tool_call_response(
        "call-9",
        "bash",
        serde_json::json!({}),
    )[0].clone());
    let kept = a.interrupted_blocks();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0], ContentBlock::Text { text: "partial output".into() });
}

#[test]
fn assembler_message_carries_source() {
    let mut a = BlockAssembler::new();
    a.push(&text_delta(0, "hi"));
    let msg = a.message(
        "m-1",
        MessageSource::Model {
            provider: "mock".into(),
            model: "mock-1".into(),
        },
    );
    assert_eq!(msg.role, Role::Assistant);
    assert_eq!(msg.id, "m-1");
    assert_eq!(msg.text(), "hi");
}

#[tokio::test]
async fn mock_adapter_echoes_last_user_message() {
    let adapter = dsh_llm::adapters::mock::MockAdapter::new();
    let options = GenerateOptions {
        provider: "mock".into(),
        model: "mock-1".into(),
        messages: vec![Message::user("u-1", vec![ContentBlock::text("ping")])],
        system: None,
        tools: None,
        temperature: None,
        max_tokens: None,
        stop: None,
        session_id: None,
    };
    let stream = adapter.stream(options).await.unwrap();
    let mut assembler = BlockAssembler::new();
    let mut stream = std::pin::pin!(stream);
    while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
        assembler.push(&chunk);
    }
    assert_eq!(assembler.blocks()[0], ContentBlock::Text { text: "ping".into() });
}

#[tokio::test]
async fn runtime_routes_and_rejects_duplicates() {
    let runtime = LlmRuntime::new();
    let adapter: Arc<dyn LlmAdapter> = Arc::new(dsh_llm::adapters::mock::MockAdapter::new());
    runtime.register_adapter(&["mock"], adapter).unwrap();
    assert!(runtime.has_provider("mock"));
    assert!(runtime
        .register_adapter(&["mock"], Arc::new(dsh_llm::adapters::mock::MockAdapter::new()))
        .is_err());
    runtime.unregister_adapter(&["mock"]);
    assert!(!runtime.has_provider("mock"));
    let options = GenerateOptions {
        provider: "nope".into(),
        model: "x".into(),
        messages: vec![],
        system: None,
        tools: None,
        temperature: None,
        max_tokens: None,
        stop: None,
        session_id: None,
    };
    assert!(runtime.stream(options).await.is_err());
}

#[tokio::test]
async fn waterfall_dispatches_stream() {
    let ctx = cordis::Context::new();
    let handle = ctx.plugin(llm_plugin(), Some(serde_json::json!({})));
    handle.join().await.unwrap();
    let runtime = ctx.require::<LlmService>("llm").unwrap();
    let streams = ctx.require::<StreamTable>("llmStreams").unwrap();

    let options = GenerateOptions {
        provider: "mock".into(),
        model: "mock-1".into(),
        messages: vec![Message::user("u-1", vec![ContentBlock::text("hello")])],
        system: None,
        tools: None,
        temperature: None,
        max_tokens: None,
        stop: None,
        session_id: None,
    };
    let mut stream = stream_via_waterfall(&ctx, (*runtime).clone(), (*streams).clone(), options).await.unwrap();
    let mut assembler = BlockAssembler::new();
    while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
        assembler.push(&chunk);
    }
    assert_eq!(assembler.blocks()[0], ContentBlock::Text { text: "hello".into() });
}

#[test]
fn cancel_token_fires_once() {
    let token = CancelToken::new();
    assert!(!token.is_cancelled());
    token.cancel();
    token.cancel();
    assert!(token.is_cancelled());
}

#[tokio::test]
async fn cancel_token_race() {
    let token = CancelToken::new();
    let token2 = token.clone();
    let handle = tokio::spawn(async move {
        let result = token2
            .race(std::future::pending::<u32>())
            .await;
        assert!(result.is_none());
    });
    token.cancel();
    handle.await.unwrap();
}

#[test]
fn usage_roundtrip_through_json() {
    let usage = TokenUsage {
        input_tokens: 10,
        output_tokens: 5,
        cache_read_tokens: Some(2),
        ..Default::default()
    };
    let value = serde_json::to_value(usage).unwrap();
    let back: TokenUsage = serde_json::from_value(value).unwrap();
    assert_eq!(back, usage);
}
