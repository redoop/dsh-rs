//! The [`Agent`] handle and [`AgentRegistry`] — the Rust analogue of
//! `packages/core/agent`.
//!
//! An agent owns one session (the durable event log), an inbox of pending
//! user messages, a lifecycle status, and a driver task running the
//! [`crate::loop_driver::drive`] loop.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cordis::Context;

use dsh_llm::{CancelToken, Message, MessageSource, Role};
use dsh_api::services::{SessionService, SessionView};
use dsh_session::{CreateSessionOptions, SessionId, user_message};
use dsh_types::{AgentCancelCause, AgentOptions, AgentStatus};


/// The `agents` service key.
pub const AGENTS_SERVICE: &str = "agents";

/// The two ordered pending-message lists owned by an agent.
#[derive(Debug, Default)]
struct Inbox {
    next_turn: VecDeque<Message>,
    next_step: VecDeque<Message>,
}

/// Live agent handle: the surface every plugin programs against.
pub struct Agent {
    pub id: SessionId,
    pub options: AgentOptions,
    pub session: Arc<dyn SessionView>,
    /// Agent-scoped context.
    pub ctx: Context,
    status: Mutex<AgentStatus>,
    inbox: Mutex<Inbox>,
    cancel: Mutex<Option<CancelToken>>,
    /// Bumped whenever the inbox gains work (wakes the driver).
    wake_tx: tokio::sync::watch::Sender<u64>,
    wake_rx: tokio::sync::watch::Receiver<u64>,
    /// True while the driver is processing a batch.
    driver_busy: Arc<AtomicBool>,
    /// Number of pending (unclaimed) inbox messages.
    pending: Arc<AtomicUsize>,
    /// Bumped whenever the driver settles back to waiting.
    settle_tx: tokio::sync::watch::Sender<u64>,
    settle_rx: tokio::sync::watch::Receiver<u64>,
    disposed: Arc<AtomicBool>,
    /// Set by the loop when the assistant completes a turn for the first time.
    pub completed_turns: AtomicU64,
    _driver: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Agent {
    pub fn new(
        id: SessionId,
        options: AgentOptions,
        session: Arc<dyn SessionView>,
        ctx: Context,
    ) -> Arc<Agent> {
        let (wake_tx, wake_rx) = tokio::sync::watch::channel(0);
        let (settle_tx, settle_rx) = tokio::sync::watch::channel(0);
        Arc::new(Agent {
            id,
            options,
            session,
            ctx,
            status: Mutex::new(AgentStatus::Idle),
            inbox: Mutex::new(Inbox::default()),
            cancel: Mutex::new(None),
            wake_tx,
            wake_rx,
            driver_busy: Arc::new(AtomicBool::new(false)),
            pending: Arc::new(AtomicUsize::new(0)),
            settle_tx,
            settle_rx,
            disposed: Arc::new(AtomicBool::new(false)),
            completed_turns: AtomicU64::new(0),
            _driver: Mutex::new(None),
        })
    }

    pub fn status(&self) -> AgentStatus {
        *self.status.lock().unwrap()
    }

    pub fn is_disposed(&self) -> bool {
        self.disposed.load(Ordering::SeqCst)
    }

    /// Queue an ordinary follow-up turn and wake the driver.
    pub fn followup(&self, message: Message) {
        self.inbox.lock().unwrap().next_turn.push_back(message);
        self.pending.fetch_add(1, Ordering::SeqCst);
        self.wake();
    }

    /// Submit steering for the nearest step (does not open a new turn).
    pub fn steer(&self, message: Message) {
        self.inbox.lock().unwrap().next_step.push_back(message);
        self.pending.fetch_add(1, Ordering::SeqCst);
        self.wake();
    }

    /// Queue model-facing context without waking the driver.
    pub fn inject(&self, message: Message) {
        self.inbox.lock().unwrap().next_step.push_back(message);
        self.pending.fetch_add(1, Ordering::SeqCst);
    }

    /// Bump the wake channel so the driver re-checks its inbox.
    pub fn wake(&self) {
        let version = self.wake_tx.borrow().wrapping_add(1);
        let _ = self.wake_tx.send(version);
    }

    /// Cancel the active turn and (unless `keep_inbox`) clear pending work.
    pub fn cancel(&self, cause: AgentCancelCause, keep_inbox: bool) {
        if let Some(token) = self.cancel.lock().unwrap().as_ref() {
            token.cancel();
        }
        if !keep_inbox {
            self.inbox.lock().unwrap().next_turn.clear();
            self.inbox.lock().unwrap().next_step.clear();
        }
        let _ = cause;
    }

    pub fn driver_busy(&self) -> bool {
        self.driver_busy.load(Ordering::SeqCst)
    }

    /// Resolve after the driver reaches quiescence: no active batch and no
    /// pending woken work.
    pub async fn when_idle(&self) {
        let mut rx = self.settle_rx.clone();
        loop {
            if !self.driver_busy.load(Ordering::SeqCst) && !self.has_work() {
                return;
            }
            // Copy the version out so the read guard drops BEFORE awaiting:
            // holding it across changed() deadlocks single-threaded runtimes.
            let _version = *rx.borrow_and_update();
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    pub(crate) fn has_work(&self) -> bool {
        self.pending.load(Ordering::SeqCst) > 0
    }

    /// Claim one turn-opening batch: one `next-turn` message plus all
    /// `next-step` input.
    pub(crate) fn claim_batch(&self) -> Vec<Message> {
        let mut inbox = self.inbox.lock().unwrap();
        let mut batch = Vec::new();
        if let Some(message) = inbox.next_turn.pop_front() {
            batch.push(message);
        }
        batch.extend(inbox.next_step.drain(..));
        self.pending.store(0, Ordering::SeqCst);
        batch
    }

    /// Claim only the `next-step` input (a step continuation).
    pub(crate) fn claim_next_step(&self) -> Vec<Message> {
        let mut inbox = self.inbox.lock().unwrap();
        let drained: Vec<Message> = inbox.next_step.drain(..).collect();
        self.pending.store(0, Ordering::SeqCst);
        drained
    }

    pub(crate) fn begin_turn(&self, token: CancelToken) {
        *self.cancel.lock().unwrap() = Some(token);
        *self.status.lock().unwrap() = AgentStatus::Running;
        self.driver_busy.store(true, Ordering::SeqCst);
        self.emit_status();
    }

    pub(crate) fn end_turn(&self) {
        *self.status.lock().unwrap() = AgentStatus::Idle;
        self.driver_busy.store(false, Ordering::SeqCst);
        // Copy the version BEFORE send: the borrow guard must not outlive the
        // statement (a read lock held across the write lock self-deadlocks).
        let version = self.settle_tx.borrow().wrapping_add(1);
        let _ = self.settle_tx.send(version);
        self.emit_status();
    }

    pub(crate) fn turn_token(&self) -> Option<CancelToken> {
        self.cancel.lock().unwrap().clone()
    }

    pub(crate) fn wake_receiver(&self) -> tokio::sync::watch::Receiver<u64> {
        self.wake_rx.clone()
    }

    fn emit_status(&self) {
        dsh_api::events::emit(
            &self.ctx,
            &dsh_api::events::AgentStatusPayload {
                agent: self.id.clone(),
                status: self.status().as_str_name().to_string(),
            },
        );
    }
}

struct AgentRegistryInner {
    agents: Mutex<std::collections::HashMap<SessionId, Arc<Agent>>>,
    ctx: Context,
    sessions: SessionService,
}

/// Live agent registry (`ctx.agents`). Cheap-clone handle.
#[derive(Clone)]
pub struct AgentRegistry {
    inner: Arc<AgentRegistryInner>,
}

impl AgentRegistry {
    pub fn new(ctx: Context, sessions: SessionService) -> Self {
        AgentRegistry {
            inner: Arc::new(AgentRegistryInner {
                agents: Mutex::new(std::collections::HashMap::new()),
                ctx,
                sessions,
            }),
        }
    }

    /// Create a session and agent under one identity, then start its driver.
    pub fn create(
        &self,
        id: Option<SessionId>,
        options: AgentOptions,
        cwd: Option<String>,
        seed_prompt: Option<String>,
    ) -> Result<Arc<Agent>, crate::AgentError> {
        let session = self.inner.sessions.create(CreateSessionOptions {
            id: id.clone(),
            cwd: cwd.clone(),
            ..Default::default()
        });
        let agent_id = session.id().to_string();
        let agent = Agent::new(agent_id.clone(), options.clone(), session, self.inner.ctx.clone());

        if let Some(seed) = seed_prompt {
            agent.session.append(dsh_session::SessionEventData::UserMessage {
                message: user_message(format!("seed-{}", agent_id), seed),
            });
        }

        {
            let mut map = self.inner.agents.lock().unwrap();
            if map.contains_key(&agent_id) {
                return Err(crate::AgentError::Duplicate(agent_id));
            }
            map.insert(agent_id.clone(), agent.clone());
        }
        dsh_api::events::emit(
            &self.inner.ctx,
            &dsh_api::events::AgentCreatedPayload { agent: agent_id },
        );
        crate::loop_driver::spawn_driver(agent.clone());
        Ok(agent)
    }

    pub fn get(&self, id: &str) -> Option<Arc<Agent>> {
        self.inner.agents.lock().unwrap().get(id).cloned()
    }

    pub fn list(&self) -> Vec<Arc<Agent>> {
        self.inner.agents.lock().unwrap().values().cloned().collect()
    }

    /// Dispose an agent: mark disposed, wake the driver so it exits.
    pub fn dispose(&self, agent: &Arc<Agent>) {
        agent.disposed.store(true, Ordering::SeqCst);
        agent.cancel(AgentCancelCause::Disposed, false);
        agent.wake();
        let removed = self.inner.agents.lock().unwrap().remove(&agent.id).is_some();
        if removed {
            dsh_api::events::emit(
                &self.inner.ctx,
                &dsh_api::events::AgentDisposedPayload {
                    agent: agent.id.clone(),
                },
            );
        }
    }

    /// Dispose every agent (harness shutdown).
    pub async fn dispose_all(&self) {
        let agents = self.list();
        for agent in agents {
            self.dispose(&agent);
            let _ = tokio::time::timeout(Duration::from_secs(2), agent.when_idle()).await;
        }
    }
}

/// Build a user-role message for the model.
pub fn user_message_with_text(id: impl Into<String>, text: impl Into<String>) -> Message {
    Message {
        id: id.into(),
        role: Role::User,
        content: vec![dsh_llm::ContentBlock::text(text)],
        source: MessageSource::User,
    }
}

impl dsh_api::services::AgentView for Agent {
    fn id(&self) -> &str {
        &self.id
    }

    fn session(&self) -> Arc<dyn dsh_api::services::SessionView> {
        self.session.clone()
    }

    fn followup(&self, message: Message) {
        Agent::followup(self, message)
    }

    fn steer(&self, message: Message) {
        Agent::steer(self, message)
    }

    fn inject(&self, message: Message) {
        Agent::inject(self, message)
    }

    fn cancel(&self, cause: dsh_types::AgentCancelCause, keep_inbox: bool) {
        Agent::cancel(self, cause, keep_inbox)
    }

    fn status(&self) -> dsh_types::AgentStatus {
        Agent::status(self)
    }

    fn driver_busy(&self) -> bool {
        Agent::driver_busy(self)
    }

    fn when_idle(&self) -> dsh_api::services::BoxFuture<()> {
        let mut rx = self.settle_rx.clone();
        let busy = self.driver_busy.clone();
        let pending = self.pending.clone();
        Box::pin(async move {
            loop {
                if !busy.load(Ordering::SeqCst) && pending.load(Ordering::SeqCst) == 0 {
                    return;
                }
                let _version = *rx.borrow_and_update();
                if rx.changed().await.is_err() {
                    return;
                }
            }
        })
    }
}

impl dsh_api::services::AgentRegistryApi for AgentRegistry {
    fn create(
        &self,
        id: Option<String>,
        options: AgentOptions,
        cwd: Option<String>,
        seed_prompt: Option<String>,
    ) -> Result<Arc<dyn dsh_api::services::AgentView>, String> {
        self.create(id, options, cwd, seed_prompt)
            .map(|a| a as Arc<dyn dsh_api::services::AgentView>)
            .map_err(|e| e.to_string())
    }

    fn get(&self, id: &str) -> Option<Arc<dyn dsh_api::services::AgentView>> {
        self.get(id)
            .map(|a| a as Arc<dyn dsh_api::services::AgentView>)
    }

    fn list(&self) -> Vec<Arc<dyn dsh_api::services::AgentView>> {
        self.list()
            .into_iter()
            .map(|a| a as Arc<dyn dsh_api::services::AgentView>)
            .collect()
    }

    fn dispose(&self, agent: &Arc<dyn dsh_api::services::AgentView>) {
        if let Some(concrete) = self.get(agent.id()) {
            self.dispose(&concrete);
        }
    }
}
