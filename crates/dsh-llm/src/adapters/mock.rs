//! The `mock` adapter: a deterministic, scriptable provider for tests and
//! offline runs. With no script it echoes the last user message as assistant
//! text; a script queues whole chunk sequences, popped one per request.

use std::collections::VecDeque;
use std::sync::Mutex;

use cordis::plugin::BoxFuture;
use serde_json::Value;

use crate::runtime::{stream_from_chunks, BoxStream, LlmAdapter};
use crate::types::{
    ContentBlock, FinishReason, GenerateOptions, LlmError, Message, StreamChunk, TokenUsage,
};

#[derive(Default)]
pub struct MockAdapter {
    /// One chunk sequence per request, consumed FIFO.
    script: Mutex<VecDeque<Vec<StreamChunk>>>,
    /// When true, simulate a fixed small token usage on every response.
    report_usage: bool,
}

impl MockAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a scripted response for the next N requests.
    pub fn scripted(turns: Vec<Vec<StreamChunk>>) -> Self {
        MockAdapter {
            script: Mutex::new(turns.into()),
            report_usage: false,
        }
    }

    pub fn with_usage(mut self) -> Self {
        self.report_usage = true;
        self
    }

    /// Echo the given text as an assistant text block.
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
                block: ContentBlock::Text { text: text.to_string() },
            },
            StreamChunk::Finish { reason: FinishReason::Stop },
        ]
    }

    /// One tool-call chunk sequence for the given call.
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
}

impl LlmAdapter for MockAdapter {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn stream(
        &self,
        options: GenerateOptions,
    ) -> BoxFuture<Result<BoxStream<StreamChunk>, LlmError>> {
        let scripted = self.script.lock().unwrap().pop_front();
        let report_usage = self.report_usage;
        Box::pin(async move {
            let mut chunks = match scripted {
                Some(script) => script,
                None => {
                    let last_user: Option<&Message> =
                        options.messages.iter().rev().find(|m| m.role == crate::types::Role::User);
                    let echoed = match last_user {
                        Some(message) => message.text(),
                        None => String::new(),
                    };
                    MockAdapter::text_response(&echoed)
                }
            };
            if report_usage {
                chunks.push(StreamChunk::Usage {
                    usage: TokenUsage {
                        input_tokens: 12,
                        output_tokens: 7,
                        ..Default::default()
                    },
                });
            }
            Ok(stream_from_chunks(chunks))
        })
    }
}
