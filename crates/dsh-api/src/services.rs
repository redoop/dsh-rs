//! Service interfaces — the dependency-injected seams of dsh-rs.
//!
//! Consumers depend on THIS crate (the interface) and never on the
//! implementation crates (`dsh-llm`, `dsh-session`, ...). Each service is a
//! cheap-clone wrapper around a trait object, provided on the cordis context
//! by its implementing plugin:
//!
//! ```text
//! consumer  ---depends on-->  dsh-api (trait + wrapper)
//!     ^                              ^
//!     |                       implements / provides
//!     +-- ctx.require::<LlmService>()--+
//!                                      |
//!                              dsh-llm (implementation plugin)
//! ```

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use dsh_types::{
    AgentCancelCause, AgentOptions, AgentStatus, BoxStream, EpochHeader, GenerateOptions, LlmError,
    LlmProviderInfo, Message, PromptAssembly, SessionEvent, SessionEventData, StreamChunk,
    ToolExecutionResult, ToolSchema,
};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Service names
// ---------------------------------------------------------------------------

pub const LLM_SERVICE: &str = "llm";
pub const LLM_STREAMS_SERVICE: &str = "llmStreams";
pub const SESSIONS_SERVICE: &str = "sessions";
pub const TOOLS_SERVICE: &str = "tools";
pub const SYSTEM_PROMPT_SERVICE: &str = "systemPrompt";
pub const AGENTS_SERVICE: &str = "agents";
pub const SESSION_PERSISTENCE_SERVICE: &str = "sessionPersistence";
pub const MANIFEST_SERVICE: &str = "manifest";

/// `BoxFuture` re-export for interface method signatures.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

// ---------------------------------------------------------------------------
// Session view
// ---------------------------------------------------------------------------

/// The read/write surface of a session, as seen by consumers. Implemented by
/// `dsh_session::Session`; consumers never need the concrete type.
pub trait SessionView: Send + Sync + 'static {
    fn id(&self) -> &str;
    fn header_cwd(&self) -> Option<String>;
    fn events(&self) -> Vec<SessionEvent>;
    fn surface(&self) -> Vec<u64>;
    fn derive_messages(&self) -> Vec<Message>;
    fn append(&self, data: SessionEventData) -> SessionEvent;
    fn request_header(&self) -> Option<EpochHeader>;
    /// The open turn number, if any.
    fn open_turn(&self) -> Option<u64>;
}

// ---------------------------------------------------------------------------
// Interfaces
// ---------------------------------------------------------------------------

/// A durability backend for session logs, implemented by `dsh-session`'s
/// JSONL backend.
pub trait SessionPersistenceApi: Send + Sync + 'static {
    fn on_event(&self, session: &str, event: &SessionEvent);
    fn flush(&self, session: &str) -> std::io::Result<()>;
    fn load(&self, session: &str) -> std::io::Result<Vec<SessionEvent>>;
    fn list(&self) -> Vec<String>;
}

/// The session store service (`ctx.sessions`) implemented by `dsh-session`.
pub trait SessionStoreApi: Send + Sync + 'static {
    fn create(&self, options: dsh_types::CreateSessionOptions) -> Arc<dyn SessionView>;
    fn get(&self, id: &str) -> Option<Arc<dyn SessionView>>;
    fn list(&self) -> Vec<Arc<dyn SessionView>>;
    fn remove(&self, id: &str) -> Option<Arc<dyn SessionView>>;
    fn fork(
        &self,
        source_id: &str,
        boundary: Option<u64>,
        child_id: Option<String>,
    ) -> Result<Arc<dyn SessionView>, String>;
    fn flush(&self, session_id: &str) -> BoxFuture<Result<(), String>>;
    fn attach_persistence(&self, backend: Arc<dyn SessionPersistenceApi>);
}

/// A model-wire adapter (`ctx.llm` route provider), implemented by
/// `dsh-llm`'s adapters (mock, openai-compatible).
pub trait LlmAdapterApi: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn stream(
        &self,
        options: GenerateOptions,
    ) -> BoxFuture<Result<BoxStream<StreamChunk>, LlmError>>;
}

/// The LLM runtime service (`ctx.llm`) implemented by `dsh-llm`.
pub trait LlmRuntimeApi: Send + Sync + 'static {
    fn list_providers(&self) -> Vec<LlmProviderInfo>;
    fn has_provider(&self, name: &str) -> bool;
    fn register_adapter(
        &self,
        providers: &[&str],
        adapter: Arc<dyn LlmAdapterApi>,
    ) -> Result<(), LlmError>;
    fn unregister_adapter(&self, providers: &[&str]);
    fn stream(
        &self,
        options: GenerateOptions,
    ) -> BoxFuture<Result<BoxStream<StreamChunk>, LlmError>>;
}

/// An externally supplied tool (e.g. from a dynamically loaded plugin):
/// schema plus the execution callback. Lets the host adapt foreign code
/// through the interface layer without exposing the concrete `ToolDefinition`.
pub struct DynamicToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    /// `arguments` is the parsed JSON args; returns the model-facing result.
    pub exec: Arc<dyn Fn(Value) -> BoxFuture<ToolExecutionResult> + Send + Sync>,
}

/// The tool registry service (`ctx.tools`) implemented by `dsh-tools`.
pub trait ToolRegistryApi: Send + Sync + 'static {
    fn schemas(&self) -> Vec<ToolSchema>;
    fn list(&self) -> Vec<String>;
    fn execute(
        &self,
        call_id: String,
        name: String,
        arguments: Value,
        run_ctx: dsh_types::ToolRunContext,
    ) -> BoxFuture<ToolExecutionResult>;
    fn register_dynamic_tool(&self, spec: DynamicToolSpec);
    fn unregister_dynamic_tool(&self, name: &str);
}

/// The system-prompt service (`ctx.systemPrompt`) implemented by `dsh-core`.
pub trait SystemPromptApi: Send + Sync + 'static {
    fn section(&self, name: &str, order: i32, text: &str, complete: bool);
    fn variable(
        &self,
        name: &str,
        provider: dsh_types::VariableProvider,
    );
    fn assemble(&self) -> PromptAssembly;
}

/// The live agent handles held by the agent registry (`ctx.agents`).
pub trait AgentView: Send + Sync + 'static {
    fn id(&self) -> &str;
    fn session(&self) -> Arc<dyn SessionView>;
    fn followup(&self, message: Message);
    fn steer(&self, message: Message);
    fn inject(&self, message: Message);
    fn cancel(&self, cause: AgentCancelCause, keep_inbox: bool);
    fn status(&self) -> AgentStatus;
    fn driver_busy(&self) -> bool;
    fn when_idle(&self) -> BoxFuture<()>;
}

/// The agent registry service (`ctx.agents`) implemented by `dsh-core`.
pub trait AgentRegistryApi: Send + Sync + 'static {
    fn create(
        &self,
        id: Option<String>,
        options: AgentOptions,
        cwd: Option<String>,
        seed_prompt: Option<String>,
    ) -> Result<Arc<dyn AgentView>, String>;
    fn get(&self, id: &str) -> Option<Arc<dyn AgentView>>;
    fn list(&self) -> Vec<Arc<dyn AgentView>>;
    fn dispose(&self, agent: &Arc<dyn AgentView>);
}

// ---------------------------------------------------------------------------
// Service wrappers (the values provided on the cordis context)
// ---------------------------------------------------------------------------

macro_rules! wrapper {
    ($name:ident, $trait:ident) => {
        /// Cheap-clone handle to the [`$trait`] service.
        #[derive(Clone)]
        pub struct $name {
            pub inner: Arc<dyn $trait>,
        }

        impl $name {
            pub fn new(inner: Arc<dyn $trait>) -> Self {
                $name { inner }
            }
        }

        impl std::ops::Deref for $name {
            type Target = dyn $trait;
            fn deref(&self) -> &Self::Target {
                &*self.inner
            }
        }
    };
}

wrapper!(SessionService, SessionStoreApi);
wrapper!(LlmService, LlmRuntimeApi);
wrapper!(ToolsService, ToolRegistryApi);
wrapper!(SystemPromptService, SystemPromptApi);
wrapper!(AgentRegistryService, AgentRegistryApi);

/// Cheap-clone handle to the manifest registry service.
#[derive(Clone)]
pub struct ManifestService {
    pub inner: Arc<dyn super::manifest::ManifestApi>,
}

impl ManifestService {
    pub fn new(inner: Arc<dyn super::manifest::ManifestApi>) -> Self {
        ManifestService { inner }
    }
}

impl std::ops::Deref for ManifestService {
    type Target = dyn super::manifest::ManifestApi;
    fn deref(&self) -> &Self::Target {
        &*self.inner
    }
}