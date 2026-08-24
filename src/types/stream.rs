//! Async stream plumbing shared by the LLM seam: a boxed chunk stream plus a
//! helper to turn a chunk list into one.

use std::pin::Pin;

use tokio::sync::mpsc;

/// A boxed async stream of chunks.
pub type BoxStream<T> = Pin<Box<dyn futures::Stream<Item = T> + Send>>;

/// `tokio::sync::mpsc::Receiver` as a `futures::Stream` (no tokio-stream dep).
pub struct ReceiverStream<T> {
    rx: mpsc::Receiver<T>,
}

impl<T> futures::Stream for ReceiverStream<T> {
    type Item = T;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<T>> {
        self.rx.poll_recv(cx)
    }
}

/// Wrap a vector of chunks as a stream.
pub fn stream_from_chunks<T: Send + 'static>(chunks: Vec<T>) -> BoxStream<T> {
    let (tx, rx) = mpsc::channel(chunks.len().max(1));
    tokio::spawn(async move {
        for chunk in chunks {
            if tx.send(chunk).await.is_err() {
                break;
            }
        }
    });
    Box::pin(ReceiverStream { rx })
}
