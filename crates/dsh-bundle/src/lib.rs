//! # dsh-bundle
//!
//! Composition of the dsh-rs **base bundle** — the first layer of every
//! profile, mirroring the reference harness's `dsh-base`: model adapters,
//! tools, persistence, and the agent loop, all as plugins on the cordis
//! kernel. A profile is just a named composition; the base bundle is the one
//! every profile stacks first.

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
    ///   "openai": { "providers": [...], "base_url": "...", "api_key": "..." }
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
        if let Some(provider) = value.get("provider").and_then(|p| p.as_str()) {
            config.default_provider = Some(provider.to_string());
        }
        if let Some(model) = value.get("model").and_then(|m| m.as_str()) {
            config.default_model = Some(model.to_string());
        }
        config
    }
}

/// Start the base bundle on `ctx` and wait for every fiber to converge.
pub async fn install_base(ctx: &Context, config: BaseConfig) -> Result<Vec<FiberHandle>, String> {
    let mut entries: Vec<BundleEntry> = Vec::new();

    // 1. LLM seam: the mock adapter is always registered; openai optional.
    let mut llm_config = json!({});
    if let Some(openai) = &config.openai {
        llm_config["adapters"]["openai"] = openai.clone();
    }
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
        handle
            .join()
            .await
            .map_err(|err| format!("bundle plugin {} failed to start: {err}", handle.name()))?;
    }
    Ok(handles)
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
