//! Message and streaming vocabulary shared by every dsh-rs crate.
//!
//! Mirrors the `packages/llm` vocabulary of the reference harness: a
//! conversation is a list of [`Message`]s, each an array of typed
//! [`ContentBlock`]s; adapters answer a [`GenerateOptions`] request with a raw
//! [`StreamChunk`] stream that the [`BlockAssembler`] folds back into blocks.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Stable identity of one message.
pub type MessageId = String;
/// Stable identity pairing a `tool-call` block with its `tool-result`.
pub type CallId = String;
/// Provider route key selecting an adapter (e.g. `"mock"`, `"openai"`).
pub type ProviderId = String;

/// One typed content block; the Rust analogue of `ContentBlockMap`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ContentBlock {
    /// Visible model output.
    Text {
        text: String,
    },
    /// Chain-of-thought style reasoning, distinct from visible text.
    Reasoning {
        text: String,
    },
    /// A model-requested tool invocation. `arguments` is the RAW JSON string
    /// exactly as the model produced it — never parsed here.
    ToolCall {
        id: CallId,
        name: String,
        arguments: String,
    },
    /// A completed tool call's model-facing result.
    ToolResult {
        tool_call_id: CallId,
        content: Vec<ContentBlock>,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text { text: text.into() }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text { text } => Some(text),
            _ => None,
        }
    }
}

/// Provider-neutral conversation role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    System,
    User,
    Assistant,
}

/// Where a message (or injected content) came from — `MessageSourceMap`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum MessageSource {
    User,
    /// Producer-supplied context (file-change notices, skill content, ...).
    Plugin {
        plugin: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        form: Option<ContextForm>,
    },
    /// An assistant message produced by a provider/model.
    Model { provider: String, model: String },
    /// A tool-result-derived message.
    Tool { tool: String },
}

/// The kind of information in producer-supplied context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "form", rename_all = "kebab-case")]
pub enum ContextForm {
    Instructions,
    Catalog,
    Snapshot { sections: Vec<ContextSnapshotSection> },
    Notice { summary: String },
    Relay,
    Recall,
}

/// One named contribution to a `snapshot`-form context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextSnapshotSection {
    pub name: String,
    pub text: String,
}

/// One immutable message shared by delivery, durable history, and requests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub role: Role,
    pub content: Vec<ContentBlock>,
    pub source: MessageSource,
}

impl Message {
    pub fn user(id: impl Into<MessageId>, content: Vec<ContentBlock>) -> Self {
        Message {
            id: id.into(),
            role: Role::User,
            content,
            source: MessageSource::User,
        }
    }

    /// The text of this message's visible text blocks, concatenated.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join("")
    }

    /// Tool calls requested by this (assistant) message, in order.
    pub fn tool_calls(&self) -> Vec<&ContentBlock> {
        self.content
            .iter()
            .filter(|b| matches!(b, ContentBlock::ToolCall { .. }))
            .collect()
    }
}

/// A user-role specialization of [`Message`].
pub type UserMessage = Message;

/// Per-call token accounting; counts are disjoint (cache fields optional).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

/// Serializable provider or transport failure facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmFailure {
    pub message: String,
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_retry_after_ms: Option<u64>,
}

impl LlmFailure {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        LlmFailure {
            message: message.into(),
            code: code.into(),
            status: None,
            provider_retry_after_ms: None,
        }
    }
}

impl std::fmt::Display for LlmFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// Why a model response stopped — `FinishReasonMap`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    MaxTokens,
    Aborted { failure: LlmFailure },
    Error { failure: LlmFailure },
}

impl FinishReason {
    pub fn is_terminal_error(&self) -> bool {
        matches!(self, FinishReason::Error { .. } | FinishReason::Aborted { .. })
    }
}

/// The raw streaming protocol emitted by adapters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum StreamChunk {
    BlockStart { index: usize, block_type: String },
    TextDelta { index: usize, text: String },
    ReasoningDelta { index: usize, text: String },
    ToolCallDelta {
        index: usize,
        id: CallId,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        arguments_delta: String,
    },
    /// Carries the fully-assembled block; consumers do not re-assemble deltas.
    BlockEnd { index: usize, block: ContentBlock },
    Usage { usage: TokenUsage },
    Finish { reason: FinishReason },
}

/// JSON-schema description of a tool as sent to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// JSON Schema object for the arguments.
    pub parameters: Value,
}

/// One fully assembled model request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GenerateOptions {
    pub provider: ProviderId,
    pub model: String,
    /// Ordered conversation messages, exactly as the provider sees them.
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSchema>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Provider/model/reasoning/sampling scalars of one conversation's requests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmCallConfig {
    pub provider: ProviderId,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
}

impl Default for LlmCallConfig {
    fn default() -> Self {
        LlmCallConfig {
            provider: "mock".to_string(),
            model: "mock-1".to_string(),
            temperature: None,
            max_tokens: None,
            stop: None,
        }
    }
}

/// Display metadata for one registered provider route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmProviderInfo {
    pub id: ProviderId,
    pub name: String,
}

/// Error type for the LLM seam; carries provider-neutral [`LlmFailure`] facts.
#[derive(Debug, Clone, thiserror::Error)]
#[error("llm failure ({failure})")]
pub struct LlmError {
    pub failure: LlmFailure,
}

impl LlmError {
    pub fn new(failure: LlmFailure) -> Self {
        LlmError { failure }
    }

    pub fn code(code: impl Into<String>, message: impl Into<String>) -> Self {
        LlmError::new(LlmFailure::new(code, message))
    }
}

impl From<serde_json::Error> for LlmError {
    fn from(err: serde_json::Error) -> Self {
        LlmError::code("SERIALIZE", err.to_string())
    }
}


impl From<LlmError> for cordis::Error {
    fn from(err: LlmError) -> Self {
        cordis::Error::msg(err.to_string())
    }
}
