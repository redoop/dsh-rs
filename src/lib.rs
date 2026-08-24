//! # dsh-rs
//!
//! The dsh agent harness as a single crate: everything is a plugin on the
//! cordis kernel, exposed through an interface layer (`api`) over a shared
//! vocabulary (`types`). The former workspace crates are now modules:
//!
//! - [`types`] — shared vocabulary (messages, streams, session events, agent
//!   options); no dsh implementation.
//! - [`api`] — the interface layer: service traits + wrappers, typed events,
//!   the plugin manifest (consumers depend only on this).
//! - [`llm`] — the LLM adapter seam: mock + OpenAI-compatible adapters, the
//!   block assembler, and the provider routing.
//! - [`session`] — the event-sourced session log, store, and JSONL
//!   persistence.
//! - [`tools`] — the scoped tool registry with guarded execution and the
//!   built-in tools (bash/read_file/write_file/edit_file/glob/grep/todo_write).
//! - [`core`] — system-prompt assembly, the agent registry, and the agent
//!   loop.
//! - [`bundle`] — base-bundle composition, the manifest registry, and the
//!   dynamic-plugin (cdylib) host.
//! - [`cli`] — headless runner helpers and the terminal chat UI; the `dsh`
//!   binary lives in `src/main.rs`.

pub mod api;
pub mod bundle;
pub mod cli;
pub mod core;
pub mod llm;
pub mod session;
pub mod tools;
pub mod types;