//! # dsh-llm
//!
//! The LLM vocabulary and adapter seam of dsh-rs — the Rust analogue of the
//! reference harness's `packages/llm`. Everything an agent loop moves lives
//! here or is derived from it: [`Message`]s and [`ContentBlock`]s, the raw
//! [`StreamChunk`] streaming protocol, the [`LlmAdapter`] contract, the
//! [`BlockAssembler`], and the `llm` cordis plugin that provides `ctx.llm`.

pub mod adapters;
pub mod assembler;
pub mod cancel;
pub mod runtime;
pub mod types;

pub use assembler::BlockAssembler;
pub use cancel::CancelToken;
pub use runtime::{llm_plugin, stream_via_waterfall, BoxStream, LlmAdapter, LlmRuntime, StreamTable};
pub use types::*;

/// The `llm` service key registered by [`llm_plugin`].
pub const LLM_SERVICE: &str = "llm";
/// The `llmStreams` service key: a process-local [`StreamTable`].
pub const LLM_STREAMS_SERVICE: &str = "llmStreams";
