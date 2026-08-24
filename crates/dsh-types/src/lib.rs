//! # dsh-types
//!
//! The shared vocabulary of dsh-rs: messages, streams, session events, tool
//! results, agent options — every data type that crosses a service boundary.
//!
//! This crate deliberately contains NO dsh implementation: it is the contract
//! layer that both interface definitions (`dsh-api`) and implementations
//! (`dsh-llm`, `dsh-session`, ...) depend on. The only framework type it
//! carries is the cordis kernel `Context` handle (inside [`ToolRunContext`]).

pub mod agent;
pub mod cancel;
pub mod llm;
pub mod prompt;
pub mod session;
pub mod stream;
pub mod tools;

pub use agent::*;
pub use cancel::*;
pub use llm::*;
pub use prompt::*;
pub use session::*;
pub use stream::*;
pub use tools::*;
