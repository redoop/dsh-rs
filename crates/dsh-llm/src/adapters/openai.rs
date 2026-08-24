//! OpenAI-compatible chat-completions adapter over HTTP with SSE streaming.
//!
//! Config section (the `openai` object of the `llm` plugin config):
//! ```json
//! {
//!   "base_url": "https://api.openai.com/v1",
//!   "api_key": "sk-...",
//!   "model": "gpt-4o-mini"
//! }
//! ```

use std::pin::Pin;
use std::task::Poll;

use cordis::plugin::BoxFuture;
use serde_json::{json, Value};

use crate::runtime::{BoxStream, LlmAdapter};
use crate::types::{
    CallId, ContentBlock, FinishReason, GenerateOptions, LlmError, LlmFailure, Message,
    StreamChunk, TokenUsage,
};

pub struct OpenAiAdapter {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    default_model: Option<String>,
}

fn into_failure(code: &str, status: u16, body: &str) -> LlmFailure {
    let message = if body.is_empty() {
        format!("openai error (http {status})")
    } else {
        body.chars().take(500).collect()
    };
    LlmFailure {
        message,
        code: code.to_string(),
        status: Some(status),
        provider_retry_after_ms: None,
    }
}

impl OpenAiAdapter {
    pub fn new(config: Value) -> Result<Self, LlmError> {
        let base_url = config
            .get("base_url")
            .and_then(|v| v.as_str())
            .unwrap_or("https://api.openai.com/v1")
            .trim_end_matches('/')
            .to_string();
        let api_key = config.get("api_key").and_then(|v| v.as_str()).map(str::to_string);
        let default_model = config.get("model").and_then(|v| v.as_str()).map(str::to_string);
        let client = reqwest::Client::builder()
            .user_agent(format!("dsh-rs/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| LlmError::code("CLIENT", err.to_string()))?;
        Ok(OpenAiAdapter {
            client,
            base_url,
            api_key,
            default_model,
        })
    }

    fn build_body(&self, options: &GenerateOptions) -> Value {
        // DeepSeek-style providers require `reasoning_content` echoed back;
        // other OpenAI-compatible endpoints reject the field.
        let deepseek_style =
            options.provider.contains("deepseek") || self.base_url.contains("deepseek");
        let mut messages: Vec<Value> = Vec::new();
        if let Some(system) = &options.system {
            messages.push(json!({ "role": "system", "content": system }));
        }
        for message in &options.messages {
            messages.extend(openai_message(message, deepseek_style));
        }

        let mut body = json!({
            "model": options.model,
            "messages": messages,
            "stream": true,
        });
        if let Some(tools) = &options.tools {
            let mapped: Vec<Value> = tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool.name,
                            "description": tool.description,
                            "parameters": tool.parameters,
                        }
                    })
                })
                .collect();
            body["tools"] = Value::Array(mapped);
            body["parallel_tool_calls"] = json!(true);
        }
        if let Some(temperature) = options.temperature {
            body["temperature"] = json!(temperature);
        }
        if let Some(max_tokens) = options.max_tokens {
            body["max_tokens"] = json!(max_tokens);
        }
        if let Some(stop) = &options.stop {
            body["stop"] = json!(stop);
        }
        body
    }
}

/// Map one harness message to one or more OpenAI chat messages.
///
/// DeepSeek's thinking mode additionally requires the assistant's
/// `reasoning_content` to be echoed back on later requests; it is included
/// only for DeepSeek-style providers, since other OpenAI-compatible endpoints
/// reject the field.
fn openai_message(message: &Message, include_reasoning: bool) -> Vec<Value> {
    let mut out = Vec::new();
    let mut text: Vec<String> = Vec::new();
    let mut reasoning: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text: t } => text.push(t.clone()),
            ContentBlock::Reasoning { text: t } => reasoning.push(t.clone()),
            ContentBlock::ToolCall { id, name, arguments } => tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": arguments },
            })),
            ContentBlock::ToolResult { tool_call_id, content, .. } => {
                if !text.is_empty() || !tool_calls.is_empty() || !reasoning.is_empty() {
                    out.push(role_message(
                        message.role,
                        text.drain(..).collect(),
                        std::mem::take(&mut tool_calls),
                        if include_reasoning {
                            std::mem::take(&mut reasoning).join("")
                        } else {
                            String::new()
                        },
                    ));
                }
                let rendered: String = content
                    .iter()
                    .filter_map(|b| b.as_text())
                    .collect::<Vec<_>>()
                    .join("");
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": rendered,
                }));
            }
        }
    }
    if !text.is_empty() || !tool_calls.is_empty() || !reasoning.is_empty() {
        out.push(role_message(
            message.role,
            text.join(""),
            tool_calls,
            if include_reasoning { reasoning.join("") } else { String::new() },
        ));
    }
    if out.is_empty() {
        out.push(json!({ "role": role_str(message.role), "content": "" }));
    }
    out
}

fn role_str(role: crate::types::Role) -> &'static str {
    match role {
        crate::types::Role::System => "system",
        crate::types::Role::User => "user",
        crate::types::Role::Assistant => "assistant",
    }
}

/// One wire message carrying the harness message's ROLE (the previous
/// implementation hardcoded `assistant`, which mislabels user messages).
fn role_message(
    role: crate::types::Role,
    content: String,
    tool_calls: Vec<Value>,
    reasoning_content: String,
) -> Value {
    let mut message = json!({
        "role": role_str(role),
        "content": content,
    });
    // Some OpenAI-compatible providers (DeepSeek) reject an EMPTY tool_calls
    // array; omit the field entirely when there are no calls.
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    // DeepSeek thinking mode: echo reasoning back or the API rejects the call.
    if !reasoning_content.is_empty() {
        message["reasoning_content"] = Value::String(reasoning_content);
    }
    message
}

impl LlmAdapter for OpenAiAdapter {
    fn name(&self) -> &'static str {
        "openai-compatible"
    }

    fn stream(
        &self,
        options: GenerateOptions,
    ) -> BoxFuture<Result<BoxStream<StreamChunk>, LlmError>> {
        let body = self.build_body(&options);
        let client = self.client.clone();
        let url = format!("{}/chat/completions", self.base_url);
        let api_key = self.api_key.clone();
        let _ = self.default_model.clone();

        Box::pin(async move {
            let mut request = client.post(&url).json(&body);
            if let Some(key) = &api_key {
                request = request.bearer_auth(key);
            }
            let response = request
                .send()
                .await
                .map_err(|err| LlmError::code("TRANSPORT", err.to_string()))?;
            let status = response.status();
            if !status.is_success() {
                let text = response.text().await.unwrap_or_default();
                return Err(LlmError::new(into_failure(
                    &format!("HTTP_{}", status.as_u16()),
                    status.as_u16(),
                    &text,
                )));
            }

            let byte_stream = response.bytes_stream();
            Ok(Box::pin(SseParser {
                stream: Box::pin(byte_stream),
                buffer: Vec::new(),
                done: false,
            }) as BoxStream<StreamChunk>)
        })
    }
}

/// Incrementally parses an OpenAI SSE byte stream into [`StreamChunk`]s.
struct SseParser {
    stream: Pin<Box<dyn futures_util::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send>>,
    buffer: Vec<u8>,
    done: bool,
}

impl futures_util::Stream for SseParser {
    type Item = StreamChunk;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        loop {
            if self.done {
                return Poll::Ready(None);
            }
            // Try to emit one chunk from a complete buffered SSE event.
            if let Some(chunk) = self.parse_buffered() {
                return Poll::Ready(Some(chunk));
            }
            match futures_util::Stream::poll_next(self.stream.as_mut(), cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    self.buffer.extend_from_slice(&bytes);
                    continue;
                }
                Poll::Ready(Some(Err(err))) => {
                    self.done = true;
                    return Poll::Ready(Some(StreamChunk::Finish {
                        reason: FinishReason::Error {
                            failure: LlmFailure::new("STREAM", err.to_string()),
                        },
                    }));
                }
                Poll::Ready(None) => {
                    self.done = true;
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl SseParser {
    /// Extract one `data:` event from the buffer and convert it to a chunk.
    fn parse_buffered(&mut self) -> Option<StreamChunk> {
        let pos = self.buffer.iter().position(|&b| b == b'\n')?;
        let line: Vec<u8> = self.buffer.drain(..=pos).collect();
        let line = String::from_utf8_lossy(&line);
        let line = line.trim();
        if line.is_empty() || !line.starts_with("data:") {
            return self.parse_buffered();
        }
        let data = line["data:".len()..].trim();
        if data == "[DONE]" {
            self.done = true;
            return Some(StreamChunk::Finish { reason: FinishReason::Stop });
        }
        match serde_json::from_str::<Value>(data) {
            Ok(event) => Some(parse_event(&event)),
            Err(_) => self.parse_buffered(),
        }
    }
}

/// Convert one OpenAI SSE event object into a stream chunk.
fn parse_event(event: &Value) -> StreamChunk {
    if let Some(usage) = event.get("usage") {
        return StreamChunk::Usage {
            usage: TokenUsage {
                input_tokens: usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                output_tokens: usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
            },
        };
    }
    let choice = &event["choices"][0];
    let delta = &choice["delta"];
    // Each block KIND gets its own assembler index: text = 0, reasoning = 1,
    // tool calls = 2 + array index. (DeepSeek thinking mode interleaves
    // reasoning deltas and tool-call deltas in one stream; sharing an index
    // would silently drop the second kind in the block assembler.)

    if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
        if let Some(first) = tool_calls.first() {
            let call_index = first.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let id: CallId = first
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("call-unknown")
                .to_string();
            let name = first
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let arguments_delta = first
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            return StreamChunk::ToolCallDelta {
                index: 2 + call_index,
                id,
                name,
                arguments_delta,
            };
        }
    }
    if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
        if !text.is_empty() {
            return StreamChunk::TextDelta {
                index: 0,
                text: text.to_string(),
            };
        }
    }
    if let Some(text) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
        if !text.is_empty() {
            return StreamChunk::ReasoningDelta {
                index: 1,
                text: text.to_string(),
            };
        }
    }
    if let Some(reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
        let reason = match reason {
            "tool_calls" => FinishReason::ToolCalls,
            "length" => FinishReason::MaxTokens,
            _ => FinishReason::Stop,
        };
        return StreamChunk::Finish { reason };
    }
    // Nothing actionable in this event; synthesize an empty text delta so the
    // stream stays live (consumers tolerate empty deltas).
    StreamChunk::TextDelta {
        index: 0,
        text: String::new(),
    }
}
