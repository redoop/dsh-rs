//! Tool-call vocabulary shared by the tools registry and the agent loop.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::{CancelToken, ContentBlock};

/// Parsed arguments for one tool call (tools validate their own schema).
#[derive(Debug, Clone)]
pub struct ToolCallArgs {
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
}

/// The model-facing outcome of one tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ToolExecutionResult {
    Success {
        content: Vec<ContentBlock>,
        value: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
    },
    Error {
        message: String,
        code: String,
        content: Vec<ContentBlock>,
    },
}

impl ToolExecutionResult {
    pub fn success_text(text: impl Into<String>, value: Value) -> Self {
        ToolExecutionResult::Success {
            content: vec![ContentBlock::text(text)],
            value,
            meta: None,
        }
    }

    pub fn success_value(value: Value) -> Self {
        let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
        ToolExecutionResult::success_text(text, value)
    }

    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        let message: String = message.into();
        let code: String = code.into();
        ToolExecutionResult::Error {
            message: message.clone(),
            code,
            content: vec![ContentBlock::text(format!("Error: {message}"))],
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(self, ToolExecutionResult::Error { .. })
    }

    /// The model-facing content of this outcome.
    pub fn content(&self) -> Vec<ContentBlock> {
        match self {
            ToolExecutionResult::Success { content, .. } => content.clone(),
            ToolExecutionResult::Error { content, .. } => content.clone(),
        }
    }
}

/// Runtime context handed to a tool body. Carries the cordis kernel handle
/// (the one framework dependency the contract layer accepts) plus cooperative
/// cancellation and the calling agent's identity.
#[derive(Clone)]
pub struct ToolRunContext {
    pub ctx: cordis::Context,
    pub signal: CancelToken,
    pub agent_id: Option<String>,
    pub cwd: Option<String>,
}
