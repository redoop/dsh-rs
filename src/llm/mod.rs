//! The LLM seam: adapters (mock + OpenAI-compatible), provider routing, and
//! the block assembler. The vocabulary itself lives in [`crate::types`].

pub mod adapters;
pub mod assembler;
pub mod cancel;
pub mod runtime;
pub mod types;

pub use assembler::BlockAssembler;
pub use cancel::CancelToken;
pub use runtime::{BoxStream, LlmAdapter, LlmRuntime, StreamTable, llm_plugin, stream_via_waterfall};
pub use types::*;

/// The `llm` service key registered by [`llm_plugin`].
pub const LLM_SERVICE: &str = "llm";
/// The `llmStreams` service key: a process-local [`StreamTable`].
pub const LLM_STREAMS_SERVICE: &str = "llmStreams";
