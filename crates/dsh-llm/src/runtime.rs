//! The LLM adapter seam: [`LlmAdapter`] trait, the [`LlmRuntime`] registry
//! service, and the cordis plugin that provides `ctx.llm`.
//!
//! One adapter instance is registered under one or more provider routes;
//! `GenerateOptions.provider` selects the route. The `llm/stream` cordis
//! waterfall is the interception extension point: listeners may rewrite the
//! request (by calling `next()` with modified options), short-circuit with an
//! error, or hand back their own stream handle.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use cordis::plugin::{plugin, BoxFuture, Plugin};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::types::{
    GenerateOptions, LlmError, LlmProviderInfo, ProviderId, StreamChunk,
};

/// A boxed async stream of chunks.
pub type BoxStream<T> = Pin<Box<dyn futures::Stream<Item = T> + Send>>;

/// Provider-wire adapter for the harness message and stream vocabulary.
pub trait LlmAdapter: Send + Sync + 'static {
    /// Display name of the adapter implementation.
    fn name(&self) -> &'static str;

    /// Answer one fully assembled request with a raw chunk stream.
    ///
    /// Transport/protocol failures return `Err(LlmError)`; provider in-band
    /// failures end the stream with a `Finish::Error` chunk.
    fn stream(
        &self,
        options: GenerateOptions,
    ) -> BoxFuture<Result<BoxStream<StreamChunk>, LlmError>>;
}

/// Wrap an iterator (or vector) of chunks as a stream.
pub fn stream_from_chunks(chunks: Vec<StreamChunk>) -> BoxStream<StreamChunk> {
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

/// Registry of provider routes to adapter instances.
///
/// Cheap-clone service handle: the registry itself lives behind an `Arc`, so
/// the type can be provided through `ctx.provide` and read back with
/// `ctx.require::<LlmRuntime>` (cordis stores the value behind one more `Arc`).
#[derive(Clone, Default)]
pub struct LlmRuntime {
    inner: Arc<LlmRuntimeInner>,
}

#[derive(Default)]
struct LlmRuntimeInner {
    adapters: Mutex<HashMap<ProviderId, Arc<dyn LlmAdapter>>>,
}

impl LlmRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one adapter under `providers`. Duplicate routes fail atomically.
    pub fn register_adapter(
        &self,
        providers: &[&str],
        adapter: Arc<dyn LlmAdapter>,
    ) -> Result<(), LlmError> {
        let mut map = self.inner.adapters.lock().unwrap();
        for provider in providers {
            if map.contains_key(*provider) {
                return Err(LlmError::code(
                    "ROUTE_CONFLICT",
                    format!("provider route already registered: {provider}"),
                ));
            }
        }
        for provider in providers {
            map.insert(provider.to_string(), adapter.clone());
        }
        Ok(())
    }

    /// Release every route in `providers`.
    pub fn unregister_adapter(&self, providers: &[&str]) {
        let mut map = self.inner.adapters.lock().unwrap();
        for provider in providers {
            map.remove(*provider);
        }
    }

    /// Live provider routes, in registration order.
    pub fn list_providers(&self) -> Vec<LlmProviderInfo> {
        let map = self.inner.adapters.lock().unwrap();
        map.iter()
            .map(|(id, adapter)| LlmProviderInfo {
                id: id.clone(),
                name: adapter.name().to_string(),
            })
            .collect()
    }

    pub fn has_provider(&self, provider: &str) -> bool {
        self.inner.adapters.lock().unwrap().contains_key(provider)
    }

    /// Resolve the route's adapter and open a chunk stream.
    pub fn stream(
        &self,
        options: GenerateOptions,
    ) -> BoxFuture<Result<BoxStream<StreamChunk>, LlmError>> {
        let adapter = {
            let map = self.inner.adapters.lock().unwrap();
            match map.get(&options.provider) {
                Some(adapter) => adapter.clone(),
                None => {
                    return Box::pin(async move {
                        Err(LlmError::code(
                            "UNKNOWN_PROVIDER",
                            format!("no adapter registered for provider \"{}\"", options.provider),
                        ))
                    })
                }
            }
        };
        adapter.stream(options)
    }
}

/// A live chunk stream parked under a process-local id, so the JSON-only
/// cordis event bus can hand streams around the `llm/stream` waterfall.
#[derive(Clone, Default)]
pub struct StreamTable {
    inner: Arc<StreamTableInner>,
}

#[derive(Default)]
struct StreamTableInner {
    streams: Mutex<HashMap<String, BoxStream<StreamChunk>>>,
    next: std::sync::atomic::AtomicU64,
}

impl StreamTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, stream: BoxStream<StreamChunk>) -> String {
        let id = format!(
            "stream-{}",
            self.inner.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        );
        self.inner.streams.lock().unwrap().insert(id.clone(), stream);
        id
    }

    pub fn take(&self, id: &str) -> Option<BoxStream<StreamChunk>> {
        self.inner.streams.lock().unwrap().remove(id)
    }
}

/// The `llm` plugin: provides the `llm` service and the `llm/stream` waterfall.
pub fn llm_plugin() -> Arc<dyn Plugin> {
    plugin("llm", |ctx, config: Value| async move {
        let runtime = LlmRuntime::new();
        let streams = StreamTable::new();
        ctx.provide("llm", runtime.clone()).await?;
        ctx.provide("llmStreams", streams.clone()).await?;

        // Adapters from config: { "mock": { "providers": [...] }, "openai": { ... } }
        let adapters = config.get("adapters").cloned().unwrap_or(Value::Null);

        // Mock adapter is always available under "mock".
        let mock = Arc::new(crate::adapters::mock::MockAdapter::new());
        runtime.register_adapter(&["mock"], mock as Arc<dyn LlmAdapter>)?;

        // OpenAI-compatible adapter, configured via the "openai" section.
        #[cfg(feature = "openai")]
        {
            use crate::adapters::openai::OpenAiAdapter;
            if let Some(section) = adapters.get("openai") {
                let providers: Vec<String> = section
                    .get("providers")
                    .and_then(|p| serde_json::from_value(p.clone()).ok())
                    .unwrap_or_else(|| vec!["openai".to_string()]);
                let adapter = Arc::new(
                    OpenAiAdapter::new(section.clone())
                        .map_err(|err| cordis::Error::msg(err.to_string()))?,
                );
                let refs: Vec<&str> = providers.iter().map(|s| s.as_str()).collect();
                runtime.register_adapter(&refs, adapter as Arc<dyn LlmAdapter>)?;
            }
        }
        #[cfg(not(feature = "openai"))]
        let _ = adapters;

        Ok(())
    })
}

/// Dispatch one request through the `llm/stream` waterfall and return its
/// chunk stream. The terminal continuation performs route resolution and
/// adapter dispatch; plugins may intercept by registering on `llm/stream`.
///
/// Returns `Err` when no listener/fallback produced a `stream_id`.
pub async fn stream_via_waterfall(
    ctx: &cordis::Context,
    runtime: LlmRuntime,
    streams: StreamTable,
    options: GenerateOptions,
) -> Result<BoxStream<StreamChunk>, LlmError> {
    let payload = serde_json::to_value(&options).map_err(|err| LlmError::code("SERIALIZE", err.to_string()))?;
    let runtime = runtime.clone();
    let streams = streams.clone();
    let streams_fallback = streams.clone();
    let result = ctx
        .waterfall("llm/stream", payload, move |payload| {
            let runtime = runtime.clone();
            let streams = streams_fallback.clone();
            Box::pin(async move {
                let options: GenerateOptions =
                    serde_json::from_value(payload).map_err(|err| cordis::Error::msg(err.to_string()))?;
                let stream = runtime
                    .stream(options)
                    .await
                    .map_err(|err| cordis::Error::msg(format!("llm stream failed: {err}")))?;
                let stream_id = streams.insert(stream);
                Ok(json!({ "stream_id": stream_id }))
            })
        })
        .await
        .map_err(|err| LlmError::code("WATERFALL", err.to_string()))?;

    match result.get("stream_id").and_then(|id| id.as_str()) {
        Some(id) => streams
            .take(id)
            .ok_or_else(|| LlmError::code("STREAM_GONE", "stream handle already consumed")),
        None => {
            if let Some(err) = result.get("error") {
                let failure = serde_json::from_value(err.clone())
                    .unwrap_or_else(|_| crate::types::LlmFailure::new("UNKNOWN", err.to_string()));
                Err(LlmError::new(failure))
            } else {
                Err(LlmError::code(
                    "NO_STREAM",
                    "llm/stream waterfall produced no stream_id",
                ))
            }
        }
    }
}

impl From<crate::types::LlmError> for cordis::Error {
    fn from(err: crate::types::LlmError) -> Self {
        cordis::Error::msg(err.to_string())
    }
}
