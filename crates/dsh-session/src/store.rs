//! The [`SessionStore`] — in-memory session store service (`ctx.sessions`).
//!
//! Persistence is deliberately not implemented here: persistence plugins
//! subscribe through the store's append notifier and flush on
//! [`SessionStore::flush`] / the `session/flush` event.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cordis::{plugin, Context, Plugin};
use serde_json::{json, Value};

use crate::event::{now_ms, SessionEvent, SessionEventData, SessionHeader, SessionId};
use crate::persistence::SessionPersistence;
use crate::session::Session;

/// The `sessions` service key.
pub const SESSIONS_SERVICE: &str = "sessions";

/// Options for creating a session.
#[derive(Debug, Clone, Default)]
pub struct CreateSessionOptions {
    pub id: Option<SessionId>,
    pub cwd: Option<String>,
    /// Seed events (replay/fork).
    pub seed: Vec<SessionEvent>,
    /// Parent session lineage for forks.
    pub parent_session: Option<SessionId>,
}

impl CreateSessionOptions {
    pub fn with_id(id: impl Into<SessionId>) -> Self {
        CreateSessionOptions {
            id: Some(id.into()),
            ..Default::default()
        }
    }
}

struct SessionStoreInner {
    sessions: Mutex<HashMap<SessionId, Arc<Session>>>,
    ctx: Context,
    backends: Mutex<Vec<Arc<dyn SessionPersistence>>>,
    next: AtomicU64,
}

/// In-memory session store (`ctx.sessions`). Cheap-clone service handle.
#[derive(Clone)]
pub struct SessionStore {
    inner: Arc<SessionStoreInner>,
}

impl SessionStore {
    pub fn new(ctx: Context) -> Self {
        SessionStore {
            inner: Arc::new(SessionStoreInner {
                sessions: Mutex::new(HashMap::new()),
                ctx,
                backends: Mutex::new(Vec::new()),
                next: AtomicU64::new(0),
            }),
        }
    }

    fn mint_id(&self) -> SessionId {
        format!("session-{}", self.inner.next.fetch_add(1, Ordering::SeqCst) + 1)
    }

    /// Create a session owned by the store; emits `session/created`.
    pub fn create(&self, options: CreateSessionOptions) -> Arc<Session> {
        let id = options.id.unwrap_or_else(|| self.mint_id());
        let header = SessionHeader {
            cwd: options.cwd,
            created_at: now_ms(),
            parent_session: options.parent_session,
            seed_length: options.seed.len() as u64,
        };
        let session = Arc::new(Session::new(id.clone(), header, options.seed));
        let notifier = self.make_notifier(id.clone());
        session.add_notifier(notifier);
        self.inner
            .sessions
            .lock()
            .unwrap()
            .insert(id.clone(), session.clone());
        self.inner.ctx.emit(
            "session/created",
            json!({ "session": id, "time": now_ms() }),
        );
        session
    }

    /// Build a session WITHOUT entering it into the store (pairs with
    /// [`SessionStore::enter`]); the caller owns its lifecycle.
    pub fn prepare(&self, options: CreateSessionOptions) -> Arc<Session> {
        let id = options.id.unwrap_or_else(|| self.mint_id());
        let header = SessionHeader {
            cwd: options.cwd,
            created_at: now_ms(),
            parent_session: options.parent_session,
            seed_length: options.seed.len() as u64,
        };
        Arc::new(Session::new(id, header, options.seed))
    }

    /// Enter a prepared session into the store; returns the detach disposer.
    pub fn enter(&self, session: Arc<Session>) -> Result<(), crate::SessionError> {
        let mut map = self.inner.sessions.lock().unwrap();
        if map.contains_key(&session.id) {
            return Err(crate::SessionError::Duplicate(session.id.clone()));
        }
        let notifier = self.make_notifier(session.id.clone());
        session.add_notifier(notifier);
        map.insert(session.id.clone(), session);
        Ok(())
    }

    /// Announce a just-entered session with `session/created`.
    pub fn announce(&self, session: &Arc<Session>) {
        self.inner.ctx.emit(
            "session/created",
            json!({ "session": session.id, "time": now_ms() }),
        );
    }

    /// Per-session notifier: fires the `session/event` firehose and forwards
    /// the append to every attached persistence backend.
    fn make_notifier(&self, session_id: SessionId) -> Arc<dyn Fn(&SessionEvent) + Send + Sync> {
        let inner = self.inner.clone();
        Arc::new(move |event: &SessionEvent| {
            let payload = json!({ "session": session_id, "event": event });
            inner.ctx.emit("session/event", payload);
            let backends = inner.backends.lock().unwrap().clone();
            for backend in &backends {
                backend.on_event(&session_id, event);
            }
        })
    }

    /// Look up a live session.
    pub fn get(&self, id: &str) -> Option<Arc<Session>> {
        self.inner.sessions.lock().unwrap().get(id).cloned()
    }

    /// All live sessions, in creation order.
    pub fn list(&self) -> Vec<Arc<Session>> {
        self.inner.sessions.lock().unwrap().values().cloned().collect()
    }

    /// Remove a session from the store; emits `session/disposed`.
    pub fn remove(&self, id: &str) -> Option<Arc<Session>> {
        let session = self.inner.sessions.lock().unwrap().remove(id);
        if session.is_some() {
            self.inner.ctx.emit("session/disposed", json!({ "session": id }));
        }
        session
    }

    /// Attach a persistence backend; it receives every future append and is
    /// flushed by [`SessionStore::flush`].
    pub fn attach_persistence(&self, backend: Arc<dyn SessionPersistence>) {
        self.inner.backends.lock().unwrap().push(backend);
    }

    /// The durability checkpoint: flush every backend for one session, then
    /// dispatch the awaited `session/flush` parallel event.
    pub async fn flush(&self, session: &Arc<Session>) -> Result<(), crate::SessionError> {
        let backends = self.inner.backends.lock().unwrap().clone();
        for backend in &backends {
            backend.flush(&session.id).map_err(crate::SessionError::Io)?;
        }
        let payload = json!({ "session": session.id });
        let result = self
            .inner
            .ctx
            .parallel("session/flush", payload)
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(err) => Err(crate::SessionError::Other(err.to_string())),
        }
    }

    /// Fork a live session from a stable prefix of its log.
    ///
    /// `boundary` is an inclusive source event seq; the prefix must end
    /// outside an open turn.
    pub fn fork(
        &self,
        source: &Arc<Session>,
        boundary: Option<u64>,
        child_id: Option<SessionId>,
    ) -> Result<Arc<Session>, crate::SessionError> {
        let events = source.events();
        let boundary = boundary.unwrap_or_else(|| events.last().map(|e| e.seq).unwrap_or(0));
        let prefix: Vec<SessionEvent> = events
            .into_iter()
            .filter(|e| e.seq <= boundary)
            .collect();
        // The prefix must end outside an open turn.
        let mut open: Option<u64> = None;
        for event in &prefix {
            match &event.data {
                SessionEventData::TurnStart { turn } => open = Some(*turn),
                SessionEventData::TurnEnd { .. } => open = None,
                _ => {}
            }
        }
        if let Some(turn) = open {
            return Err(crate::SessionError::OpenTurn(turn));
        }
        let options = CreateSessionOptions {
            id: child_id,
            cwd: source.header.cwd.clone(),
            seed: prefix,
            parent_session: Some(source.id.clone()),
        };
        let child = self.create(options);
        child.append(SessionEventData::SessionEndSeed);
        Ok(child)
    }
}

/// The `sessions` plugin: provides the `ctx.sessions` service.
pub fn session_plugin() -> Arc<dyn Plugin> {
    plugin("sessions", |ctx, _config: Value| async move {
        let store = SessionStore::new(ctx.clone());
        ctx.provide(SESSIONS_SERVICE, store.clone()).await?;
        Ok(())
    })
}
