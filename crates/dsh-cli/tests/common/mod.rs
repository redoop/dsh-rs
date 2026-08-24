//! Shared test fixtures for the dsh-cli integration tests.
//!
//! Everything here is **interface-only**: adapters implement `dsh-api` traits
//! over `dsh-types` vocabulary, and the harness boots through the base bundle
//! (`dsh-bundle`), so the consumer test files never import implementation
//! crate types.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cordis::Context;
use dsh_api::services::{BoxFuture, LlmAdapterApi, LlmService};
use dsh_types::{
    BoxStream, ContentBlock, FinishReason, GenerateOptions, LlmError, Role, StreamChunk,
    stream_from_chunks,
};
use serde_json::Value;

/// A scripted provider: pops one chunk sequence per request; without a script
/// it echoes the last user message (mirror of the built-in mock adapter).
pub struct ScriptedAdapter {
    script: Mutex<VecDeque<Vec<StreamChunk>>>,
}

impl ScriptedAdapter {
    pub fn new(script: Vec<Vec<StreamChunk>>) -> Self {
        ScriptedAdapter {
            script: Mutex::new(script.into()),
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
    ) -> BoxFuture<Result<BoxStream<StreamChunk>, LlmError>> {
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
                    text_response(&echoed)
                }
            };
            Ok(stream_from_chunks(chunks))
        })
    }
}

/// One assistant text-block response.
pub fn text_response(text: &str) -> Vec<StreamChunk> {
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

/// One tool-call response for the given call.
pub fn tool_call_response(id: &str, name: &str, arguments: Value) -> Vec<StreamChunk> {
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

/// Boot the base bundle (if `install` is true) and mount `script` as the
/// `mock` provider route. When the harness already installed the bundle,
/// pass `false` to only swap the adapter (installing twice would re-register
/// every service in the same scope).
pub async fn boot_scripted(ctx: &Context, install: bool, script: Vec<Vec<StreamChunk>>) {
    if install {
        dsh_bundle::install_base_default(ctx).await.expect("base bundle boots");
    }
    let runtime = ctx
        .require::<LlmService>(dsh_api::LLM_SERVICE)
        .expect("llm service live");
    runtime.unregister_adapter(&["mock"]);
    runtime
        .register_adapter(
            &["mock"],
            Arc::new(ScriptedAdapter::new(script)),
        )
        .expect("scripted adapter registers under mock");
}

/// An agent created through the registry service wrapper.
pub async fn create_agent(ctx: &Context) -> Arc<dyn dsh_api::services::AgentView> {
    let agents = ctx
        .require::<dsh_api::services::AgentRegistryService>(dsh_api::AGENTS_SERVICE)
        .expect("agents service live");
    agents
        .create(
            None,
            dsh_types::AgentOptions::mock("mock-1"),
            Some("/tmp".to_string()),
            None,
        )
        .expect("agent creation succeeds")
}