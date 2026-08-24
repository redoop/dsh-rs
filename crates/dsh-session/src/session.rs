//! The [`Session`] — an append-only event log with an ordered *surface*
//! projection from which LLM message history is derived.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use dsh_llm::{Message, Role};
use serde_json::Value;

use crate::event::{
    now_ms, SessionEvent, SessionEventData, SessionHeader, SessionId,
};

/// A synchronous observer of committed appends (persistence, telemetry).
pub type AppendNotifier = Arc<dyn Fn(&SessionEvent) + Send + Sync>;

/// An event-sourced session: an append-only log of [`SessionEvent`]s.
pub struct Session {
    pub id: SessionId,
    pub header: SessionHeader,
    events: Mutex<Vec<SessionEvent>>,
    /// Surface node seqs in model-visible order.
    surface: Mutex<Vec<u64>>,
    replace_generation: Mutex<u64>,
    next_seq: AtomicU64,
    /// The folded request header from the latest `request/header` event.
    request_header_state: Mutex<Option<crate::event::EpochHeader>>,
    notifiers: Mutex<Vec<AppendNotifier>>,
}

impl Session {
    /// Create a session, optionally seeded with existing events (replay/fork).
    pub fn new(id: SessionId, header: SessionHeader, seed: Vec<SessionEvent>) -> Self {
        let next_seq = seed.iter().map(|e| e.seq).max().map(|m| m + 1).unwrap_or(0);
        let surface: Vec<u64> = seed.iter().filter(|e| e.data.is_surface()).map(|e| e.seq).collect();
        let session = Session {
            id,
            header,
            events: Mutex::new(seed),
            surface: Mutex::new(surface),
            replace_generation: Mutex::new(0),
            next_seq: AtomicU64::new(next_seq),
            request_header_state: Mutex::new(None),
            notifiers: Mutex::new(Vec::new()),
        };
        // Replay folds the request header lazily.
        session.rebuild_header();
        session
    }

    /// Attach a synchronous append observer.
    pub fn add_notifier(&self, notifier: AppendNotifier) {
        self.notifiers.lock().unwrap().push(notifier);
    }

    /// The next event's sequence number — always the log length.
    pub fn seq(&self) -> u64 {
        self.next_seq.load(Ordering::SeqCst)
    }

    /// An immutable snapshot of the append-only log.
    pub fn events(&self) -> Vec<SessionEvent> {
        self.events.lock().unwrap().clone()
    }

    /// Append one typed event. The hot path never blocks on I/O — notifiers
    /// run synchronously but must not await.
    pub fn append(&self, data: SessionEventData) -> SessionEvent {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let event = SessionEvent::new(seq, now_ms(), data);
        let mut events = self.events.lock().unwrap();
        events.push(event.clone());
        if event.data.is_surface() {
            self.surface.lock().unwrap().push(seq);
        }
        if let SessionEventData::RequestHeader { header } = &event.data {
            *self.request_header_state.lock().unwrap() = Some(header.clone());
        }
        drop(events);
        let notifiers = self.notifiers.lock().unwrap().clone();
        for notifier in notifiers {
            notifier(&event);
        }
        event
    }

    /// The surface node seqs in model-visible order.
    pub fn surface(&self) -> Vec<u64> {
        self.surface.lock().unwrap().clone()
    }

    pub fn replace_generation(&self) -> u64 {
        *self.replace_generation.lock().unwrap()
    }

    /// The folded request header for the NEXT request, or `None` before the
    /// first `request/header` snapshot.
    pub fn request_header(&self) -> Option<crate::event::EpochHeader> {
        self.request_header_state.lock().unwrap().clone()
    }

    fn rebuild_header(&self) {
        let events = self.events.lock().unwrap().clone();
        let mut latest = None;
        for event in &events {
            if let SessionEventData::RequestHeader { header } = &event.data {
                latest = Some(header.clone());
            }
        }
        *self.request_header_state.lock().unwrap() = latest;
    }

    /// Derive the LLM message history by walking the surface nodes.
    ///
    /// Projection rules (mirroring the reference harness):
    /// - `user/message` → a user message carrying its content verbatim;
    /// - `assistant/message` → the assistant message, skipping empty content;
    /// - `tool/result` → the user-role message carrying the tool-result block.
    pub fn derive_messages(&self) -> Vec<Message> {
        let events = self.events.lock().unwrap().clone();
        let surface = self.surface.lock().unwrap().clone();
        let mut messages = Vec::new();
        for seq in surface {
            let Some(event) = events.iter().find(|e| e.seq == seq) else { continue };
            match &event.data {
                SessionEventData::UserMessage { message } => {
                    messages.push(message.clone());
                }
                SessionEventData::AssistantMessage { message, .. } => {
                    if !message.content.is_empty() {
                        messages.push(message.clone());
                    }
                }
                SessionEventData::ToolResult { message, .. } => {
                    messages.push(message.clone());
                }
                _ => {}
            }
        }
        messages
    }

    /// Project one event to a message, or `None` when it produces none.
    pub fn derive_event_message(&self, event: &SessionEvent) -> Option<Message> {
        match &event.data {
            SessionEventData::UserMessage { message } => Some(message.clone()),
            SessionEventData::AssistantMessage { message, .. } => {
                if message.content.is_empty() {
                    None
                } else {
                    Some(message.clone())
                }
            }
            SessionEventData::ToolResult { message, .. } => Some(message.clone()),
            _ => None,
        }
    }

    /// Turn numbers of the open turn (last `turn/start` without `turn/end`).
    pub fn open_turn(&self) -> Option<u64> {
        let events = self.events.lock().unwrap().clone();
        let mut open = None;
        for event in &events {
            match &event.data {
                SessionEventData::TurnStart { turn } => open = Some(*turn),
                SessionEventData::TurnEnd { .. } => open = None,
                _ => {}
            }
        }
        open
    }

    /// Serialize the log as a JSONL string (one event per line).
    pub fn to_jsonl(&self) -> String {
        let events = self.events.lock().unwrap();
        events
            .iter()
            .map(|e| serde_json::to_string(e).expect("session events are JSON-serializable"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Full events for serialization/display, including a raw text rendering.
    pub fn render_event(event: &SessionEvent) -> String {
        match &event.data {
            SessionEventData::UserMessage { message } => {
                format!("[user] {}", message.text())
            }
            SessionEventData::AssistantMessage { message, .. } => {
                format!("[assistant] {}", message.text())
            }
            SessionEventData::ToolCall { name, arguments, .. } => {
                let parsed: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
                format!("[tool-call] {name} {parsed}")
            }
            SessionEventData::ToolResult { message, .. } => {
                format!("[tool-result] {}", message.text())
            }
            SessionEventData::TurnStart { .. } => "[turn/start]".to_string(),
            SessionEventData::TurnEnd { reason, .. } => format!("[turn/end] {reason:?}"),
            SessionEventData::StepStart { .. } => "[step/start]".to_string(),
            SessionEventData::StepEnd { .. } => "[step/end]".to_string(),
            SessionEventData::AssistantChunk { chunk, .. } => {
                format!("[assistant/chunk] {chunk:?}")
            }
            SessionEventData::TodoWrite { todos } => format!("[todo/write] {todos:?}"),
            SessionEventData::RequestHeader { .. } => "[request/header]".to_string(),
            SessionEventData::SessionEndSeed => "[session/end-seed]".to_string(),
        }
    }

    pub fn render(&self) -> String {
        self.events()
            .iter()
            .map(Session::render_event)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A user-role message helper.
pub fn user_message(id: impl Into<String>, text: impl Into<String>) -> Message {
    Message {
        id: id.into(),
        role: Role::User,
        content: vec![dsh_llm::ContentBlock::text(text)],
        source: dsh_llm::MessageSource::User,
    }
}

