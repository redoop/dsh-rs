//! # dsh-core
//!
//! The packages every dsh-rs composition boots: system-prompt assembly
//! ([`prompt`]), the agent registry and [`Agent`] handle ([`agent`]), the
//! concrete driver ([`loop_driver`]), and the `agent-loop` cordis plugin that
//! wires them into a runnable agent.

pub mod agent;
pub mod loop_driver;
pub mod prompt;
pub mod todo;

use std::sync::Arc;

use cordis::plugin::{plugin_with, Injection, Plugin};
use serde_json::Value;

use dsh_session::SessionStore;
use dsh_tools::ToolRegistry;

pub use agent::{AGENTS_SERVICE, Agent, AgentCancelCause, AgentOptions, AgentRegistry, AgentStatus};
pub use loop_driver::injected_message;
pub use prompt::{
    SYSTEM_PROMPT_SERVICE, PromptAssembly, PromptContext, PromptSection, PromptText,
    SystemPromptService,
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
            Injection::new(dsh_session::SESSIONS_SERVICE.to_string()),
            Injection::new(SYSTEM_PROMPT_SERVICE.to_string()),
            Injection::new(dsh_tools::TOOLS_SERVICE.to_string()),
            Injection::new(dsh_llm::LLM_SERVICE.to_string()),
            Injection::new(dsh_llm::LLM_STREAMS_SERVICE.to_string()),
        ],
        |ctx, _config: Value| async move {
            let sessions = ctx
                .require::<SessionStore>(dsh_session::SESSIONS_SERVICE)?
                .as_ref()
                .clone();
            let prompt = ctx
                .require::<SystemPromptService>(SYSTEM_PROMPT_SERVICE)?
                .as_ref()
                .clone();
            let tools = ctx
                .require::<ToolRegistry>(dsh_tools::TOOLS_SERVICE)?
                .as_ref()
                .clone();

            let _ = prompt;
            let registry = AgentRegistry::new(ctx.clone(), sessions);
            ctx.provide(AGENTS_SERVICE, registry.clone()).await?;
            crate::todo::register_todo_tool(&tools);
            Ok(())
        },
    )
}
