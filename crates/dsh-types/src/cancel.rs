//! Cooperative cancellation for agent turns and tool executions.
//!
//! The Rust analogue of an `AbortSignal`: a cheap-clone handle whose
//! `cancelled()` future resolves the first time `cancel()` fires. Build on a
//! `tokio::sync::watch` channel so no notification is ever missed.

use std::sync::Arc;
use tokio::sync::watch;

#[derive(Clone)]
pub struct CancelToken {
    tx: Arc<watch::Sender<bool>>,
    rx: watch::Receiver<bool>,
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(false);
        CancelToken {
            tx: Arc::new(tx),
            rx,
        }
    }

    /// Fire the cancellation; idempotent.
    pub fn cancel(&self) {
        let _ = self.tx.send(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    /// Resolve as soon as the token is cancelled.
    pub async fn cancelled(&self) {
        let mut rx = self.rx.clone();
        if *rx.borrow() {
            return;
        }
        // Register our interest, then re-check: a cancel fired between the
        // check above and `changed()`'s registration would otherwise be lost.
        let _ = rx.changed().await;
    }

    /// Run `fut` until the token fires; return `None` when cancelled first.
    pub async fn race<T>(&self, fut: impl std::future::Future<Output = T>) -> Option<T> {
        tokio::select! {
            _ = self.cancelled() => None,
            value = fut => Some(value),
        }
    }
}
