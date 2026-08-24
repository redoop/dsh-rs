//! # dsh-session
//!
//! The event-sourced session model of dsh-rs — the Rust analogue of the
//! reference harness's `packages/core/session`. A [`Session`] is an
//! append-only log of typed [`SessionEventData`] events; LLM message history
//! is derived from it. [`SessionStore`] is the `ctx.sessions` service, and the
//! JSONL backend makes logs durable.

pub mod event;
pub mod persistence;
pub mod session;
pub mod store;

pub use event::{
    EpochHeader, SessionEvent, SessionEventData, SessionHeader, SessionId, SurfaceOp, TodoItem,
    TodoStatus, TurnEndReason, now_ms,
};
pub use persistence::{
    JsonlPersistence, SessionPersistence, jsonl_persistence_plugin, load_with_repair,
    repair_crash_turns,
};
pub use session::{Session, user_message};
pub use crate::types::CreateSessionOptions;
pub use store::{SESSIONS_SERVICE, SessionStore, session_plugin};

/// Errors from session operations.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("a session with id \"{0}\" already exists")]
    Duplicate(SessionId),
    #[error("the forked prefix ends inside open turn {0}")]
    OpenTurn(u64),
    #[error("persistence error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}
