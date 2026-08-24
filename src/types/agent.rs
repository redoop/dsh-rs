//! Agent-facing vocabulary shared by the agent registry, the loop, and
//! consumers.

use serde::{Deserialize, Serialize};

/// Creation options for one agent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentOptions {
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

impl AgentOptions {
    pub fn mock(model: impl Into<String>) -> Self {
        AgentOptions {
            provider: "mock".to_string(),
            model: model.into(),
            max_tokens: None,
        }
    }
}

/// An agent's lifecycle state: `idle` means no driver activity; `running`
/// spans the driver draining its inbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentStatus {
    Idle,
    Running,
}

impl AgentStatus {
    pub fn as_str_name(self) -> &'static str {
        match self {
            AgentStatus::Idle => "idle",
            AgentStatus::Running => "running",
        }
    }
}

/// Why an active driver was cancelled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum AgentCancelCause {
    User,
    Parent,
    Hook { reason: String },
    Disposed,
}
