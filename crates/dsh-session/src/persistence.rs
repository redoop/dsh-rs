//! Persistence seam for session logs: the [`SessionPersistence`] trait, a
//! JSONL file backend, crash-repair of orphaned turns, and the plugin that
//! wires a backend to the session store.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cordis::plugin::{plugin_with, Injection, Plugin};
use serde_json::Value;

use crate::event::{now_ms, SessionEvent, SessionEventData, SessionId, TurnEndReason};
use crate::store::SESSIONS_SERVICE;

/// A durability backend for one or more session logs.
pub trait SessionPersistence: Send + Sync + 'static {
    /// Record one committed append (sync, non-blocking; buffered).
    fn on_event(&self, session: &SessionId, event: &SessionEvent);
    /// Persist all buffered events for `session` (the checkpoint).
    fn flush(&self, session: &SessionId) -> std::io::Result<()>;
    /// Load the full stored log for `session`.
    fn load(&self, session: &SessionId) -> std::io::Result<Vec<SessionEvent>>;
    /// Stored session ids.
    fn list(&self) -> Vec<SessionId>;
}

/// JSONL backend: one line per event, `<session-id>.jsonl` under a directory.
pub struct JsonlPersistence {
    dir: PathBuf,
    buffers: Mutex<HashMap<SessionId, Vec<String>>>,
}

impl JsonlPersistence {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        JsonlPersistence {
            dir: dir.into(),
            buffers: Mutex::new(HashMap::new()),
        }
    }

    fn path(&self, session: &SessionId) -> PathBuf {
        self.dir.join(format!("{session}.jsonl"))
    }
}

impl SessionPersistence for JsonlPersistence {
    fn on_event(&self, session: &SessionId, event: &SessionEvent) {
        let line = serde_json::to_string(event).expect("session events are JSON-serializable");
        self.buffers
            .lock()
            .unwrap()
            .entry(session.clone())
            .or_default()
            .push(line);
    }

    fn flush(&self, session: &SessionId) -> std::io::Result<()> {
        let lines = self.buffers.lock().unwrap().remove(session);
        let Some(lines) = lines else { return Ok(()) };
        if lines.is_empty() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir)?;
        let path = self.path(session);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        for line in lines {
            writeln!(file, "{line}")?;
        }
        file.flush()?;
        Ok(())
    }

    fn load(&self, session: &SessionId) -> std::io::Result<Vec<SessionEvent>> {
        let path = self.path(session);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = std::fs::File::open(&path)?;
        let reader = BufReader::new(file);
        let mut events = Vec::new();
        for line in reader.lines() {
            let line = line?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let event: SessionEvent = serde_json::from_str(line).map_err(|err| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("corrupt session line in {}: {err}", path.display()),
                )
            })?;
            events.push(event);
        }
        Ok(events)
    }

    fn list(&self) -> Vec<SessionId> {
        let mut ids = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Some(id) = name.strip_suffix(".jsonl") {
                    ids.push(id.to_string());
                }
            }
        }
        ids.sort();
        ids
    }
}

/// Crash recovery: close a genuinely open trailing turn with
/// `turn/end { kind: interrupted }`. Returns true when a repair was made.
///
/// The loop never emits this reason; persistence synthesizes it on reload.
pub fn repair_crash_turns(events: &mut Vec<SessionEvent>) -> bool {
    let mut open: Option<u64> = None;
    for event in events.iter() {
        match &event.data {
            SessionEventData::TurnStart { turn } => open = Some(*turn),
            SessionEventData::TurnEnd { .. } => open = None,
            _ => {}
        }
    }
    let Some(turn) = open else { return false };
    events.push(SessionEvent::new(
        events.len() as u64,
        now_ms(),
        SessionEventData::TurnEnd {
            turn,
            reason: TurnEndReason::Interrupted,
        },
    ));
    true
}

/// Load a session log from a JSONL backend, repairing crash-orphaned turns.
pub fn load_with_repair(
    backend: &dyn SessionPersistence,
    session: &SessionId,
) -> std::io::Result<Vec<SessionEvent>> {
    let mut events = backend.load(session)?;
    repair_crash_turns(&mut events);
    Ok(events)
}

/// Service handle for the attached persistence backend (`ctx.sessionPersistence`).
#[derive(Clone)]
pub struct PersistenceService {
    pub backend: Arc<dyn SessionPersistence>,
}

/// The JSONL persistence plugin: attaches a backend to the session store and
/// provides it as the `sessionPersistence` service. Requires `sessions`.
pub fn jsonl_persistence_plugin(dir: PathBuf) -> Arc<dyn Plugin> {
    plugin_with(
        "session-persistence",
        vec![Injection::new(SESSIONS_SERVICE.to_string())],
        move |ctx, _config: Value| {
            let dir = dir.clone();
            async move {
                let store = ctx
                    .get::<dsh_api::services::SessionService>(SESSIONS_SERVICE)
                    .ok_or_else(|| cordis::Error::msg("sessions service missing"))?;
                let backend = Arc::new(JsonlPersistence::new(dir));
                store.attach_persistence(backend.clone());
                ctx.provide("sessionPersistence", PersistenceService { backend }).await?;
                Ok(())
            }
        },
    )
}

/// Re-export helper for tests.
pub fn temp_dir(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("dsh-session-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    base
}

/// Ensure a directory exists.
pub fn ensure_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

impl dsh_api::services::SessionPersistenceApi for JsonlPersistence {
    fn on_event(&self, session: &str, event: &dsh_types::SessionEvent) {
        SessionPersistence::on_event(self, &session.to_string(), event)
    }

    fn flush(&self, session: &str) -> std::io::Result<()> {
        SessionPersistence::flush(self, &session.to_string())
    }

    fn load(&self, session: &str) -> std::io::Result<Vec<dsh_types::SessionEvent>> {
        SessionPersistence::load(self, &session.to_string())
    }

    fn list(&self) -> Vec<String> {
        SessionPersistence::list(self)
    }
}
