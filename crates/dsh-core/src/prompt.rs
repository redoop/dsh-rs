//! System-prompt assembly — the Rust analogue of the reference harness's
//! `packages/core/system-prompt`. Plugins contribute ordered [`PromptSection`]s
//! and [`PromptContext`]s; one assembly call concatenates them, interpolates
//! `{{variables}}`, and yields the `system` slot for the next request.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use cordis::{plugin, Context, Plugin};
use dsh_types::{PromptAssembly, VariableProvider};
use serde_json::Value;

/// The `systemPrompt` service key.
pub const SYSTEM_PROMPT_SERVICE: &str = "systemPrompt";

/// Static text or a provider evaluated at each assembly.
#[derive(Clone)]
pub enum PromptText {
    Static(String),
    Dynamic(Arc<dyn Fn() -> String + Send + Sync>),
}

impl PromptText {
    pub fn resolve(&self) -> String {
        match self {
            PromptText::Static(text) => text.clone(),
            PromptText::Dynamic(provider) => provider(),
        }
    }
}

impl From<String> for PromptText {
    fn from(value: String) -> Self {
        PromptText::Static(value)
    }
}

impl From<&str> for PromptText {
    fn from(value: &str) -> Self {
        PromptText::Static(value.to_string())
    }
}

/// One contributed section of the system prompt.
#[derive(Clone)]
pub struct PromptSection {
    /// Unique name; a duplicate registration replaces the earlier one.
    pub name: String,
    /// Sections concatenate in ascending order. Convention: `-100` is the
    /// harness identity, `0` the deployment persona, tool guidance 100–199.
    pub order: i32,
    pub text: PromptText,
    /// Treat this contribution as the complete system prompt.
    pub complete: bool,
}

impl PromptSection {
    pub fn new(name: impl Into<String>, order: i32, text: impl Into<PromptText>) -> Self {
        PromptSection {
            name: name.into(),
            order,
            text: text.into(),
            complete: false,
        }
    }

    pub fn complete(mut self) -> Self {
        self.complete = true;
        self
    }
}

/// Dynamic model context materialized as a durable user-role snapshot.
#[derive(Clone)]
pub struct PromptContext {
    pub name: String,
    /// Contexts are joined in ascending order.
    pub order: i32,
    pub text: PromptText,
}

impl PromptContext {
    pub fn new(name: impl Into<String>, order: i32, text: impl Into<PromptText>) -> Self {
        PromptContext {
            name: name.into(),
            order,
            text: text.into(),
        }
    }
}

struct SystemPromptInner {
    sections: Mutex<Vec<PromptSection>>,
    contexts: Mutex<Vec<PromptContext>>,
    variables: Mutex<HashMap<String, VariableProvider>>,
    ctx: Context,
}

/// The `ctx.systemPrompt` service. Cheap-clone handle.
#[derive(Clone)]
pub struct SystemPromptService {
    inner: Arc<SystemPromptInner>,
}

impl SystemPromptService {
    pub fn new(ctx: Context) -> Self {
        SystemPromptService {
            inner: Arc::new(SystemPromptInner {
                sections: Mutex::new(Vec::new()),
                contexts: Mutex::new(Vec::new()),
                variables: Mutex::new(HashMap::new()),
                ctx,
            }),
        }
    }

    /// Register an ordered prompt section (replaces same-name duplicates).
    pub fn section(&self, section: PromptSection) {
        let mut sections = self.inner.sections.lock().unwrap();
        if let Some(existing) = sections.iter_mut().find(|s| s.name == section.name) {
            *existing = section;
        } else {
            sections.push(section);
        }
        drop(sections);
        self.inner.ctx.emit("system-prompt/change", Value::Null);
    }

    /// Register ordered dynamic context.
    pub fn context(&self, context: PromptContext) {
        let mut contexts = self.inner.contexts.lock().unwrap();
        if let Some(existing) = contexts.iter_mut().find(|c| c.name == context.name) {
            *existing = context;
        } else {
            contexts.push(context);
        }
        drop(contexts);
        self.inner.ctx.emit("system-prompt/change", Value::Null);
    }

    /// Register a prompt variable. The provider may return `None`, in which
    /// case rendering a section that references it leaves the placeholder.
    pub fn variable(
        &self,
        name: &str,
        provider: impl Fn() -> Option<String> + Send + Sync + 'static,
    ) {
        self.inner
            .variables
            .lock()
            .unwrap()
            .insert(name.to_string(), Arc::new(provider) as VariableProvider);
    }

    /// Assemble the system prompt: order sections, concatenate, interpolate
    /// `{{variables}}`.
    pub fn assemble(&self) -> PromptAssembly {
        let mut sections: Vec<PromptSection> = self.inner.sections.lock().unwrap().clone();
        sections.sort_by_key(|s| s.order);
        // An effective complete section is the sole prompt section.
        if let Some(complete) = sections.iter().find(|s| s.complete) {
            return PromptAssembly { system: complete.text.resolve() };
        }
        let mut text = sections
            .iter()
            .map(|s| s.text.resolve())
            .filter(|t| !t.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        let variables = self.inner.variables.lock().unwrap().clone();
        for (name, provider) in &variables {
            let placeholder = format!("{{{{{name}}}}}");
            if text.contains(&placeholder) {
                if let Some(value) = provider() {
                    text = text.replace(&placeholder, &value);
                }
            }
        }
        PromptAssembly { system: text }
    }
}

/// The `systemPrompt` plugin: provides the `ctx.systemPrompt` service.
pub fn system_prompt_plugin() -> Arc<dyn Plugin> {
    plugin("system-prompt", |ctx, _config: Value| async move {
        let service = SystemPromptService::new(ctx.clone());
        let api: Arc<dyn dsh_api::services::SystemPromptApi> = Arc::new(service.clone());
        ctx.provide(
            dsh_api::SYSTEM_PROMPT_SERVICE,
            dsh_api::services::SystemPromptService::new(api),
        )
        .await?;
        Ok(())
    })
}

impl dsh_api::services::SystemPromptApi for SystemPromptService {
    fn section(&self, name: &str, order: i32, text: &str, complete: bool) {
        self.section(PromptSection {
            name: name.to_string(),
            order,
            text: PromptText::Static(text.to_string()),
            complete,
        });
    }

    fn variable(&self, name: &str, provider: dsh_types::VariableProvider) {
        self.variable(name, move || provider());
    }

    fn assemble(&self) -> dsh_types::PromptAssembly {
        self.assemble()
    }
}
