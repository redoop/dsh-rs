//! # dsh-bundle
//!
//! Composition of the dsh-rs **base bundle** — the first layer of every
//! profile, mirroring the reference harness's `dsh-base`: model adapters,
//! tools, persistence, and the agent loop, all as plugins on the cordis
//! kernel. A profile is just a named composition; the base bundle is the one
//! every profile stacks first.

pub mod dynamic;

use std::path::PathBuf;
use std::sync::Arc;

use cordis::{Context, FiberHandle, Plugin};
use serde_json::{json, Value};

/// One row of a bundle: a plugin plus its config.
pub struct BundleEntry {
    pub name: &'static str,
    pub plugin: Arc<dyn Plugin>,
    pub config: Option<Value>,
}

/// Configuration for the base bundle.
#[derive(Debug, Clone, Default)]
pub struct BaseConfig {
    /// Session persistence directory; `None` keeps sessions in memory only.
    pub store_dir: Option<PathBuf>,
    /// OpenAI-compatible provider section (see `dsh_llm`'s openai adapter).
    pub openai: Option<Value>,
    /// Additional OpenAI-compatible endpoints, one adapter per segment:
    /// `{ "opencode": { "base_url", "api_key", "model" }, ... }`. Merged
    /// into the `llm` plugin's `adapters` map alongside `openai`.
    pub adapters: Option<Value>,
    /// Default provider route for agents (falls back to `mock`).
    pub default_provider: Option<String>,
    /// Default model id for agents (falls back per provider).
    pub default_model: Option<String>,
}

impl BaseConfig {
    /// Parse a `config` object (the shape used by profiles and
    /// `~/.dsh/config.json`):
    ///
    /// ```json
    /// {
    ///   "store_dir": "/tmp/dsh-sessions",
    ///   "provider": "deepseek",
    ///   "model": "deepseek-chat",
    ///   "openai": { "providers": [...], "base_url": "...", "api_key": "..." },
    ///   "adapters": { "opencode": { "base_url": "...", "api_key": "..." } }
    /// }
    /// ```
    pub fn from_value(value: &Value) -> BaseConfig {
        let mut config = BaseConfig::default();
        if let Some(dir) = value.get("store_dir").and_then(|d| d.as_str()) {
            config.store_dir = Some(PathBuf::from(dir));
        }
        if let Some(openai) = value.get("openai") {
            config.openai = Some(openai.clone());
        }
        if let Some(adapters) = value.get("adapters") {
            config.adapters = Some(adapters.clone());
        }
        if let Some(provider) = value.get("provider").and_then(|p| p.as_str()) {
            config.default_provider = Some(provider.to_string());
        }
        if let Some(model) = value.get("model").and_then(|m| m.as_str()) {
            config.default_model = Some(model.to_string());
        }
        config
    }
}

/// The declarative service contract of one plugin.
pub use dsh_api::manifest::{ManifestApi, ManifestRegistry, PluginManifest};

/// The manifest plugin: provides `ctx.manifest`, the contract registry.
pub fn manifest_plugin() -> Arc<dyn Plugin> {
    cordis::plugin::plugin("manifest", |ctx, _config: Value| async move {
        let registry = Arc::new(ManifestRegistry::new());
        let api: Arc<dyn dsh_api::manifest::ManifestApi> = registry.clone();
        ctx.provide(
            dsh_api::MANIFEST_SERVICE,
            dsh_api::services::ManifestService::new(api),
        )
        .await?;
        Ok(())
    })
}

/// Register every bundle plugin's manifest on the manifest service. This is
/// the declarative contract (`dsh dump-config` / coverage validation):
/// what each plugin provides, requires, and which tools it registers.
///
/// The contract of a plugin is what it DECLARES, independent of whether it
/// was activated (e.g. `session-persistence` is only installed when a store
/// dir is configured, but its contract is part of the base bundle). The
/// `tools` manifest is filled from the live registry to avoid drift between
/// this list and the actually registered built-ins.
pub fn register_manifests(ctx: &Context) {
    let Some(manifest) = ctx.get::<dsh_api::services::ManifestService>(dsh_api::MANIFEST_SERVICE)
    else {
        return;
    };

    // The contract registry itself.
    manifest.register(
        PluginManifest::new("manifest")
            .describe("declarative plugin contracts and coverage validation")
            .provides(dsh_api::MANIFEST_SERVICE, "manifest registry (ctx.manifest)"),
    );

    manifest.register(
        PluginManifest::new("llm")
            .describe("model adapters and the LLM adapter seam")
            .provides(dsh_api::LLM_SERVICE, "provider routing and streaming (ctx.llm)")
            .provides(dsh_api::LLM_STREAMS_SERVICE, "in-process stream handles"),
    );
    manifest.register(
        PluginManifest::new("sessions")
            .describe("event-sourced session log and store")
            .provides(dsh_api::SESSIONS_SERVICE, "session create/get/list/fork/flush"),
    );
    manifest.register(
        PluginManifest::new("session-persistence")
            .describe("JSONL durability backend")
            .provides(dsh_api::SESSION_PERSISTENCE_SERVICE, "JSONL backend")
            .requires(dsh_api::SESSIONS_SERVICE),
    );
    manifest.register(
        PluginManifest::new("tools")
            .describe("scoped tool registry and guarded execution pipeline")
            .provides(dsh_api::TOOLS_SERVICE, "tool registry (ctx.tools)")
            .tools(
                ctx.get::<dsh_api::services::ToolsService>(dsh_api::TOOLS_SERVICE)
                    .map(|registry| registry.schemas())
                    .unwrap_or_default(),
            ),
    );
    manifest.register(
        PluginManifest::new("system-prompt")
            .describe("prompt-section and variable assembly")
            .provides(
                dsh_api::SYSTEM_PROMPT_SERVICE,
                "system-prompt assembly (ctx.systemPrompt)",
            ),
    );
    manifest.register(
        PluginManifest::new("agent-loop")
            .describe("the default agent driver")
            .provides(dsh_api::AGENTS_SERVICE, "agent registry (ctx.agents)")
            .requires(dsh_api::SESSIONS_SERVICE)
            .requires(dsh_api::SYSTEM_PROMPT_SERVICE)
            .requires(dsh_api::TOOLS_SERVICE)
            .requires(dsh_api::LLM_SERVICE)
            .requires(dsh_api::LLM_STREAMS_SERVICE),
    );
}

/// Start the base bundle on `ctx` and wait for every fiber to converge.
pub async fn install_base(ctx: &Context, config: BaseConfig) -> Result<Vec<FiberHandle>, String> {
    let mut entries: Vec<BundleEntry> = Vec::new();

    // 0. The manifest registry (contract layer) is always present first: its
    // `ctx.manifest` service is what `register_manifests` writes into later.
    entries.push(BundleEntry {
        name: "manifest",
        plugin: manifest_plugin(),
        config: None,
    });

    // 1. LLM seam: the mock adapter is always registered; every
    // OpenAI-compatible endpoint from `adapters` (plus the legacy `openai`
    // section) is mounted as its own adapter.
    let mut llm_config = json!({});
    let mut adapters = config.adapters.clone().unwrap_or_else(|| json!({}));
    if let Some(openai) = &config.openai {
        if let Some(map) = adapters.as_object_mut() {
            map.insert("openai".to_string(), openai.clone());
        }
    }
    llm_config["adapters"] = adapters;
    entries.push(BundleEntry {
        name: "llm",
        plugin: dsh_llm::llm_plugin(),
        config: Some(llm_config),
    });

    // 2. Event-sourced sessions.
    entries.push(BundleEntry {
        name: "sessions",
        plugin: dsh_session::session_plugin(),
        config: None,
    });

    // 3. Tools registry + built-in tools.
    entries.push(BundleEntry {
        name: "tools",
        plugin: dsh_tools::tools_plugin(),
        config: Some(Value::Null),
    });

    // 4. System-prompt assembly.
    entries.push(BundleEntry {
        name: "system-prompt",
        plugin: dsh_core::prompt::system_prompt_plugin(),
        config: None,
    });

    let mut handles: Vec<FiberHandle> = Vec::new();
    for entry in &entries {
        handles.push(ctx.plugin(entry.plugin.clone(), entry.config.clone()));
    }

    // 5. JSONL persistence attaches to the session store (needs sessions up).
    if let Some(dir) = &config.store_dir {
        handles.push(ctx.plugin(dsh_session::jsonl_persistence_plugin(dir.clone()), None));
    }

    // 6. The agent loop wires sessions + prompt + tools + llm into agents.
    handles.push(ctx.plugin(dsh_core::agent_loop_plugin(), None));

    // Wait for convergence; surface the first failure.
    for handle in &handles {
        eprintln!("[bundle] joining {}", handle.name());
        handle
            .join()
            .await
            .map_err(|err| format!("bundle plugin {} failed to start: {err}", handle.name()))?;
        eprintln!("[bundle] joined {}", handle.name());
    }

    // Declare every plugin's contract on the manifest registry.
    register_manifests(ctx);

    Ok(handles)
}

/// Convenience: load a dynamic plugin cdylib (see [`dynamic`]).
pub fn load_dynamic_plugin(path: &std::path::Path) -> dynamic::Result<Arc<dynamic::DynamicPlugin>> {
    dynamic::load_dynamic_plugin(path)
}

/// Convenience: install the base bundle with an in-memory store.
pub async fn install_base_default(ctx: &Context) -> Result<Vec<FiberHandle>, String> {
    install_base(ctx, BaseConfig::default()).await
}

/// Read a JSON profile and install the bundles it stacks. Currently the
/// only supported bundle is `base`; unknown bundles are an error.
///
/// ```json
/// {
///   "bundles": ["base"],
///   "config": {
///     "store_dir": "/tmp/dsh-sessions",
///     "openai": { "base_url": "...", "api_key": "..." }
///   }
/// }
/// ```
pub async fn install_profile(ctx: &Context, profile: &Value) -> Result<Vec<FiberHandle>, String> {
    let bundles = profile
        .get("bundles")
        .and_then(|b| b.as_array())
        .cloned()
        .unwrap_or_else(|| vec![json!("base")]);
    let config = profile.get("config").cloned().unwrap_or_else(|| json!({}));
    let base = BaseConfig::from_value(&config);
    let mut handles = Vec::new();
    for bundle in &bundles {
        match bundle.as_str() {
            Some("base") => {
                handles.extend(install_base(ctx, base.clone()).await?);
            }
            other => {
                return Err(format!(
                    "unknown bundle \"{}\" (supported: \"base\")",
                    other.unwrap_or("?")
                ));
            }
        }
    }
    Ok(handles)
}
