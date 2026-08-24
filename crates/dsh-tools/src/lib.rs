//! # dsh-tools
//!
//! The scoped tool registry and guarded execution pipeline of dsh-rs — the
//! Rust analogue of the reference harness's `packages/core/tools`. A
//! [`ToolDefinition`] pairs a model-facing schema with an `execute` body; the
//! registry runs calls through the `tools/pre-execute` → guards →
//! `tools/execute` → body → `tools/post-execute` pipeline.

pub mod builtin;
pub mod matcher;
pub mod registry;

pub use registry::{
    TOOLS_SERVICE, ToolCallArgs, ToolDefinition, ToolExecutionResult, ToolGuard, ToolRegistry,
    ToolRunContext, tools_plugin,
};
