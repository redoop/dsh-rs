//! Plugin manifest — the declarative contract every plugin registers at
//! activation. Enables `dsh --dump-config` and startup dependency coverage
//! validation.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use crate::types::ToolSchema;
use serde_json::Value;

/// One service a plugin provides (or a dependency it requires).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceDecl {
    pub name: String,
    /// e.g. "the event-sourced session store (ctx.sessions)".
    pub description: String,
}

/// The declarative contract of one plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub description: String,
    /// Services this plugin provides on the context.
    pub provides: Vec<ServiceDecl>,
    /// Service names this plugin requires (its `inject` list).
    pub requires: Vec<String>,
    /// Tools this plugin registers on `ctx.tools`.
    pub tools: Vec<ToolSchema>,
    /// Raw config accepted by the plugin (schema is caller-validated).
    pub config: Value,
}

impl PluginManifest {
    pub fn new(name: impl Into<String>) -> Self {
        PluginManifest {
            name: name.into(),
            description: String::new(),
            provides: Vec::new(),
            requires: Vec::new(),
            tools: Vec::new(),
            config: Value::Null,
        }
    }

    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    pub fn provides(mut self, name: impl Into<String>, description: impl Into<String>) -> Self {
        self.provides.push(ServiceDecl {
            name: name.into(),
            description: description.into(),
        });
        self
    }

    pub fn requires(mut self, name: impl Into<String>) -> Self {
        self.requires.push(name.into());
        self
    }

    pub fn tools(mut self, tools: Vec<ToolSchema>) -> Self {
        self.tools = tools;
        self
    }
}

/// The manifest registry service (`ctx.manifest`).
pub trait ManifestApi: Send + Sync + 'static {
    /// Register one plugin's manifest (idempotent per plugin name).
    fn register(&self, manifest: PluginManifest);
    /// All registered manifests, in registration order.
    fn list(&self) -> Vec<PluginManifest>;
    /// Flattened list of every service provided by any plugin.
    fn provided_services(&self) -> Vec<ServiceDecl>;
}

/// In-memory manifest registry; provided by the `manifest` plugin.
#[derive(Default)]
pub struct ManifestRegistry {
    manifests: Mutex<Vec<PluginManifest>>,
}

impl ManifestRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Validate dependency coverage: every required service must be provided
    /// by at least one registered manifest. Returns the missing services.
    pub fn validate_coverage(&self, manifests: &[PluginManifest]) -> Vec<String> {
        let provided: HashMap<&str, ()> = manifests
            .iter()
            .flat_map(|m| m.provides.iter().map(|s| (s.name.as_str(), ())))
            .collect();
        let mut missing = Vec::new();
        for manifest in manifests {
            for required in &manifest.requires {
                if !provided.contains_key(required.as_str()) {
                    missing.push(format!("{} requires `{required}`", manifest.name));
                }
            }
        }
        missing
    }
}

impl ManifestApi for ManifestRegistry {
    fn register(&self, manifest: PluginManifest) {
        let mut manifests = self.manifests.lock().unwrap();
        if let Some(existing) = manifests.iter_mut().find(|m| m.name == manifest.name) {
            *existing = manifest;
        } else {
            manifests.push(manifest);
        }
    }

    fn list(&self) -> Vec<PluginManifest> {
        self.manifests.lock().unwrap().clone()
    }

    fn provided_services(&self) -> Vec<ServiceDecl> {
        let manifests = self.manifests.lock().unwrap();
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for manifest in manifests.iter() {
            for service in &manifest.provides {
                if seen.insert(service.name.clone()) {
                    out.push(service.clone());
                }
            }
        }
        out
    }
}
