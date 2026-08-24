//! # dsh-api
//!
//! The interface layer of dsh-rs: service traits and wrappers ([`services`]),
//! typed events ([`events`]), and the plugin manifest ([`manifest`]).
//! Consumers depend on this crate — never on the implementation crates.

pub mod events;
pub mod manifest;
pub mod services;

pub use events::*;
pub use manifest::*;
pub use services::*;
