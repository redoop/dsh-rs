//! # dsh-core
//!
//! The packages every dsh-rs composition boots: system-prompt assembly
//! ([`prompt`]), the agent registry and [`Agent`] handle ([`agent`]), the
//! concrete driver ([`loop_driver`]), and the `agent-loop` cordis plugin that
//! wires them into a runnable agent.

pub mod agent;
pub mod loop_driver;
pub mod prompt;

use std::sync::Arc;

use cordis::plugin::{plugin_with, Injection, Plugin};
use serde_json::Value;

use crate::api::services::{
    AgentRegistryService, LlmService, SessionService, SystemPromptService as SystemPromptApiService,
    ToolsService,
};

pub use agent::{AGENTS_SERVICE, Agent, AgentRegistry};
pub use crate::types::{AgentCancelCause, AgentOptions, AgentStatus};
pub use loop_driver::injected_message;
pub use crate::types::PromptAssembly;
pub use prompt::{
    SYSTEM_PROMPT_SERVICE, PromptContext, PromptSection, PromptText, SystemPromptService,
};

/// Errors from agent operations.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("an agent with id \"{0}\" already exists")]
    Duplicate(String),
    #[error("agent loop error: {0}")]
    Loop(String),
}

/// The `agent-loop` plugin: provides `ctx.agents` and the todo tool.
///
/// Declares the service dependencies so its fiber activates only after the
/// sessions, system-prompt, tools, and llm services are live.
pub fn agent_loop_plugin() -> Arc<dyn Plugin> {
    plugin_with(
        "agent-loop",
        vec![
            Injection::new(crate::api::SESSIONS_SERVICE.to_string()),
            Injection::new(crate::api::SYSTEM_PROMPT_SERVICE.to_string()),
            Injection::new(crate::api::TOOLS_SERVICE.to_string()),
            Injection::new(crate::api::LLM_SERVICE.to_string()),
            Injection::new(crate::api::LLM_STREAMS_SERVICE.to_string()),
        ],
        |ctx, _config: Value| async move {
            let sessions = ctx
                .require::<SessionService>(crate::api::SESSIONS_SERVICE)?
                .as_ref()
                .clone();
            let prompt = ctx
                .require::<SystemPromptApiService>(crate::api::SYSTEM_PROMPT_SERVICE)?
                .as_ref()
                .clone();
            let tools = ctx
                .require::<ToolsService>(crate::api::TOOLS_SERVICE)?
                .as_ref()
                .clone();
            let llm = ctx
                .require::<LlmService>(crate::api::LLM_SERVICE)?
                .as_ref()
                .clone();
            let _ = llm;

            let _ = prompt;
            let registry = AgentRegistry::new(ctx.clone(), sessions);
            let api: std::sync::Arc<dyn crate::api::services::AgentRegistryApi> =
                std::sync::Arc::new(registry.clone());
            ctx.provide(AGENTS_SERVICE, AgentRegistryService::new(api)).await?;
            let _ = tools;
            Ok(())
        },
    )
}
