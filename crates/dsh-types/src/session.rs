//! The session event vocabulary — the Rust analogue of `SessionEventMap`.
//!
//! A [`Session`](crate::Session) is an append-only log of [`SessionEvent`]s,
//! the single source of truth for an agent interaction. LLM message history
//! is *derived* from the log (`derive_messages`), never stored separately.

use crate::{LlmCallConfig, Message, StreamChunk, TokenUsage, ToolSchema};
use serde::{Deserialize, Serialize};

/// Shared agent/session identity.
pub type SessionId = String;

/// Why a turn ended — `TurnEndReasonMap`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum TurnEndReason {
    Completed,
    /// A cancellation request interrupted the live turn.
    Aborted { cause: String },
    Blocked,
    /// The turn failed.
    Error { message: String, code: String },
    /// At least one step reached its output-token ceiling.
    MaxTokens,
    /// A persistence backend closed a crash-orphaned turn on reload.
    Interrupted,
}

/// One entry in an agent's todo list — the unit of the `todo/write` event's
/// whole-list snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// The logged request envelope: call config, rendered system prompt, and
/// assembled tool schemas — the latest snapshot reconstructs the next request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EpochHeader {
    pub config: LlmCallConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSchema>>,
}

/// How a message-producing event entered the ordered surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SurfaceOp {
    Append,
    /// Replaces surface nodes from `start` through `end` (inclusive).
    Replace { start: u64, end: u64 },
}

/// The merge-extensible, append-only source of truth for an agent interaction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum SessionEventData {
    /// Opens turn `turn`.
    TurnStart { turn: u64 },
    /// Closes turn `turn` with the reason that ended it.
    TurnEnd { turn: u64, reason: TurnEndReason },
    /// Opens step `step` of turn `turn`.
    StepStart { turn: u64, step: u64 },
    /// Closes step `step` of turn `turn`.
    StepEnd { turn: u64, step: u64 },
    /// A user-role message on the model-visible surface.
    UserMessage { message: Message },
    /// Raw stream chunk — token-level replay fidelity.
    AssistantChunk { turn: u64, step: u64, chunk: StreamChunk },
    /// Assembled assistant message for one step (derived history uses this).
    AssistantMessage {
        turn: u64,
        step: u64,
        message: Message,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
        #[serde(skip_serializing_if = "Option::is_none")]
        interrupted: Option<bool>,
    },
    /// The model requested one tool invocation; `arguments` is the RAW JSON
    /// string exactly as produced.
    ToolCall {
        turn: u64,
        step: u64,
        call_id: String,
        name: String,
        arguments: String,
    },
    /// A completed tool call's model-facing result.
    ToolResult { turn: u64, step: u64, message: Message },
    /// Whole-list snapshot; latest write wins on replay.
    TodoWrite { todos: Vec<TodoItem> },
    /// Full header for the next request, appended inside its step.
    RequestHeader { header: EpochHeader },
    /// Marks the end of a constructor seed (resume/fork/replay).
    SessionEndSeed,
}

impl SessionEventData {
    /// Whether this event type is message-producing (a surface node).
    pub fn is_surface(&self) -> bool {
        matches!(
            self,
            SessionEventData::UserMessage { .. }
                | SessionEventData::AssistantMessage { .. }
                | SessionEventData::ToolResult { .. }
        )
    }

    pub fn event_type(&self) -> &'static str {
        match self {
            SessionEventData::TurnStart { .. } => "turn/start",
            SessionEventData::TurnEnd { .. } => "turn/end",
            SessionEventData::StepStart { .. } => "step/start",
            SessionEventData::StepEnd { .. } => "step/end",
            SessionEventData::UserMessage { .. } => "user/message",
            SessionEventData::AssistantChunk { .. } => "assistant/chunk",
            SessionEventData::AssistantMessage { .. } => "assistant/message",
            SessionEventData::ToolCall { .. } => "tool/call",
            SessionEventData::ToolResult { .. } => "tool/result",
            SessionEventData::TodoWrite { .. } => "todo/write",
            SessionEventData::RequestHeader { .. } => "request/header",
            SessionEventData::SessionEndSeed => "session/end-seed",
        }
    }
}

/// One immutable entry in the session log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEvent {
    /// Monotonic position in the log (`seq = log length`).
    pub seq: u64,
    /// Unix epoch milliseconds.
    pub time: u64,
    #[serde(flatten)]
    pub data: SessionEventData,
}

impl SessionEvent {
    pub fn new(seq: u64, time: u64, data: SessionEventData) -> Self {
        SessionEvent { seq, time, data }
    }

    pub fn event_type(&self) -> &'static str {
        self.data.event_type()
    }
}

/// Storage metadata attached to a session, kept out of the event log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHeader {
    /// Validated absolute working directory.
    pub cwd: Option<String>,
    pub created_at: u64,
    /// Durable fork lineage: the parent session id, when forked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<SessionId>,
    /// Length of the constructor seed (replay/fork boundary).
    pub seed_length: u64,
}

impl Default for SessionHeader {
    fn default() -> Self {
        SessionHeader {
            cwd: None,
            created_at: now_ms(),
            parent_session: None,
            seed_length: 0,
        }
    }
}

/// Unix epoch milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}


/// Options for creating a session.
#[derive(Debug, Clone, Default)]
pub struct CreateSessionOptions {
    pub id: Option<SessionId>,
    pub cwd: Option<String>,
    /// Seed events (replay/fork).
    pub seed: Vec<SessionEvent>,
    /// Parent session lineage for forks.
    pub parent_session: Option<SessionId>,
}

impl CreateSessionOptions {
    pub fn with_id(id: impl Into<SessionId>) -> Self {
        CreateSessionOptions {
            id: Some(id.into()),
            ..Default::default()
        }
    }
}
