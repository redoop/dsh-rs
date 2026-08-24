//! Typed events — bind every event name to its payload type so changing a
//! payload breaks the compilation of BOTH sides, not just the producer.
//!
//! The cordis bus still carries `serde_json::Value`; these helpers serialize
//! at emit and deserialize at listen, but the type safety lives at the
//! boundary: an event's name and payload struct are one unit.

use cordis::plugin::BoxFuture;
use cordis::Context;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;

use dsh_types::SessionEvent;

/// A typed event: name + payload are bound together.
pub trait EventPayload: Serialize + DeserializeOwned + Send + Sync + 'static {
    const NAME: &'static str;
}

// ---------------------------------------------------------------------------
// Event payloads (one struct per core event)
// ---------------------------------------------------------------------------

/// `session/event` — one committed append on a session's log.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct SessionEventPayload {
    pub session: String,
    pub event: SessionEvent,
}
impl EventPayload for SessionEventPayload {
    const NAME: &'static str = "session/event";
}

/// `session/created` — a session entered the store.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct SessionCreatedPayload {
    pub session: String,
}
impl EventPayload for SessionCreatedPayload {
    const NAME: &'static str = "session/created";
}

/// `session/disposed` — a session left the store.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct SessionDisposedPayload {
    pub session: String,
}
impl EventPayload for SessionDisposedPayload {
    const NAME: &'static str = "session/disposed";
}

/// `agent/created` — an agent was published.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct AgentCreatedPayload {
    pub agent: String,
}
impl EventPayload for AgentCreatedPayload {
    const NAME: &'static str = "agent/created";
}

/// `agent/disposed` — an agent left the registry.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct AgentDisposedPayload {
    pub agent: String,
}
impl EventPayload for AgentDisposedPayload {
    const NAME: &'static str = "agent/disposed";
}

/// `agent/status` — an agent changed lifecycle state.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct AgentStatusPayload {
    pub agent: String,
    pub status: String,
}
impl EventPayload for AgentStatusPayload {
    const NAME: &'static str = "agent/status";
}

/// `agent/error` — a step or turn errored.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct AgentErrorPayload {
    pub agent: String,
    pub error: String,
}
impl EventPayload for AgentErrorPayload {
    const NAME: &'static str = "agent/error";
}

/// `system-prompt/change` — prompt providers changed (payload unused).
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct SystemPromptChangePayload;
impl EventPayload for SystemPromptChangePayload {
    const NAME: &'static str = "system-prompt/change";
}

// ---------------------------------------------------------------------------
// Typed emit / listen helpers
// ---------------------------------------------------------------------------

/// Emit a typed event through the cordis bus.
pub fn emit<E: EventPayload>(ctx: &Context, payload: &E) {
    if let Ok(value) = serde_json::to_value(payload) {
        ctx.emit(E::NAME, value);
    }
}

/// Register a typed listener. `handler` receives the parsed payload; errors
/// are reported (and contained) by the bus like any listener failure.
pub async fn on<E, F>(ctx: &Context, handler: F) -> Result<cordis::fiber::EffectGuard, cordis::Error>
where
    E: EventPayload,
    F: Fn(Context, E) -> BoxFuture<cordis::Result<Value>> + Send + Sync + 'static,
{
    ctx.on(E::NAME, move |ctx, payload, _next| {
        let parsed = match serde_json::from_value::<E>(payload) {
            Ok(parsed) => parsed,
            Err(err) => {
                return Box::pin(async move {
                    Err(cordis::Error::msg(format!(
                        "typed event {}: {err}",
                        E::NAME
                    )))
                })
            }
        };
        handler(ctx, parsed)
    })
    .await
}

/// Serialize a typed payload to the bus value (for waterfall payloads that
/// the caller must still inspect/rewrite).
pub fn to_value<E: EventPayload>(payload: &E) -> Result<Value, cordis::Error> {
    serde_json::to_value(payload).map_err(|e| cordis::Error::msg(e.to_string()))
}