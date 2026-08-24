//! The tool registry and guarded execution pipeline — the Rust analogue of
//! the reference harness's `packages/core/tools`.
//!
//! A [`ToolDefinition`] pairs a model-facing [`ToolSchema`] with an `execute`
//! body. [`ToolRegistry::execute`] runs one accepted call through the guarded
//! pipeline: `tools/pre-execute` (allow/deny decision waterfall) → monotonic
//! guards → `tools/execute` (around-dispatch wrapper waterfall) → the tool
//! body → `tools/post-execute` (result waterfall).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use cordis::{plugin, Context, Plugin};
use serde_json::{json, Value};

use cordis::plugin::BoxFuture;
use crate::llm::ToolSchema;
use crate::types::{ToolCallArgs, ToolExecutionResult, ToolRunContext};

/// The `tools` service key.
pub const TOOLS_SERVICE: &str = "tools";

/// One registered tool: schema plus the execution function.
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema object for the arguments.
    pub parameters: Value,
    pub execute: Arc<
        dyn Fn(ToolCallArgs, ToolRunContext) -> BoxFuture<ToolExecutionResult>
            + Send
            + Sync,
    >,
    pub is_concurrency_safe: bool,
}

impl ToolDefinition {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        execute: impl Fn(ToolCallArgs, ToolRunContext) -> BoxFuture<ToolExecutionResult>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        ToolDefinition {
            name: name.into(),
            description: description.into(),
            parameters,
            execute: Arc::new(execute),
            is_concurrency_safe: false,
        }
    }

    pub fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }
}

/// A monotonic execution guard: returning `Some(reason)` denies the call;
/// `None` leaves it unchanged. Guards have no allow result, so listener
/// ordering cannot turn a denial back into permission.
pub type ToolGuard = Arc<dyn Fn(&ToolCallArgs) -> Option<String> + Send + Sync>;

struct ToolRegistryInner {
    tools: Mutex<HashMap<String, Arc<ToolDefinition>>>,
    guards: Mutex<Vec<ToolGuard>>,
    ctx: Context,
}

/// Scoped tool registry (`ctx.tools`). Cheap-clone service handle.
#[derive(Clone)]
pub struct ToolRegistry {
    inner: Arc<ToolRegistryInner>,
}

impl ToolRegistry {
    pub fn new(ctx: Context) -> Self {
        ToolRegistry {
            inner: Arc::new(ToolRegistryInner {
                tools: Mutex::new(HashMap::new()),
                guards: Mutex::new(Vec::new()),
                ctx,
            }),
        }
    }

    /// Register one tool; a duplicate name is replaced (last wins).
    pub fn register(&self, tool: Arc<ToolDefinition>) {
        self.inner
            .tools
            .lock()
            .unwrap()
            .insert(tool.name.clone(), tool);
    }

    pub fn unregister(&self, name: &str) {
        self.inner.tools.lock().unwrap().remove(name);
    }

    /// Add a monotonic guard evaluated before every dispatch.
    pub fn add_guard(&self, guard: ToolGuard) {
        self.inner.guards.lock().unwrap().push(guard);
    }

    /// The model-facing schemas of every registered tool.
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.inner
            .tools
            .lock()
            .unwrap()
            .values()
            .map(|tool| tool.schema())
            .collect()
    }

    pub fn list(&self) -> Vec<String> {
        let mut names: Vec<String> = self.inner.tools.lock().unwrap().keys().cloned().collect();
        names.sort();
        names
    }

    pub fn get(&self, name: &str) -> Option<Arc<ToolDefinition>> {
        self.inner.tools.lock().unwrap().get(name).cloned()
    }

    /// Run one accepted call through the guarded pipeline.
    pub async fn execute(
        &self,
        call_id: String,
        name: String,
        arguments: Value,
        run_ctx: ToolRunContext,
    ) -> ToolExecutionResult {
        let args = ToolCallArgs {
            call_id,
            name: name.clone(),
            arguments,
        };
        let Some(tool) = self.get(&name) else {
            return ToolExecutionResult::error(
                "UNKNOWN_TOOL",
                format!("no tool registered with name \"{name}\""),
            );
        };

        // 1. tools/pre-execute: allow/deny/ask decision waterfall.
        let pre_payload = json!({ "name": name, "arguments": args.arguments.clone() });
        let pre_decision = match self
            .inner
            .ctx
            .waterfall("tools/pre-execute", pre_payload, |_| {
                Box::pin(async move { Ok(json!({ "kind": "allow" })) })
            })
            .await
        {
            Ok(decision) => decision,
            Err(err) => {
                return ToolExecutionResult::error("PRE_EXECUTE", err.to_string());
            }
        };
        match pre_decision.get("kind").and_then(|k| k.as_str()) {
            Some("allow") | None => {}
            Some("deny") => {
                let reason = pre_decision
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("denied by policy");
                return ToolExecutionResult::error("DENIED", reason);
            }
            Some("ask") => {
                return ToolExecutionResult::error("APPROVAL", "tool call requires approval");
            }
            Some(_) => {}
        }

        // 2. Monotonic guards.
        let guards = self.inner.guards.lock().unwrap().clone();
        for guard in &guards {
            if let Some(reason) = guard(&args) {
                return ToolExecutionResult::error("DENIED", reason);
            }
        }

        // 3. tools/execute around-dispatch wrapper waterfall; terminal
        //    continuation runs the tool body.
        let tool_for_exec = tool.clone();
        let run_ctx_for_body = run_ctx.clone();
        let execute_payload = json!({ "name": name, "arguments": args.arguments.clone() });
        let ctx = self.inner.ctx.clone();
        let raw_result = ctx
            .waterfall("tools/execute", execute_payload, move |payload| {
                let tool = tool_for_exec.clone();
                let args = args.clone();
                let run_ctx = run_ctx_for_body.clone();
                Box::pin(async move {
                    let result = (tool.execute)(
                        ToolCallArgs {
                            call_id: args.call_id,
                            name: args.name,
                            arguments: payload.get("arguments").cloned().unwrap_or(args.arguments),
                        },
                        run_ctx,
                    )
                    .await;
                    serde_json::to_value(&result)
                        .map_err(|err| cordis::Error::msg(format!("tool result serialize: {err}")))
                })
            })
            .await;

        let result: ToolExecutionResult = match raw_result {
            Ok(value) => match serde_json::from_value(value) {
                Ok(result) => result,
                Err(err) => {
                    return ToolExecutionResult::error("BAD_RESULT", err.to_string());
                }
            },
            Err(err) => return ToolExecutionResult::error("EXECUTE", err.to_string()),
        };

        // 4. tools/post-execute: inspect/replace the result.
        let post_payload = json!({ "name": name, "result": result });
        match self
            .inner
            .ctx
            .waterfall("tools/post-execute", post_payload, |payload| {
                Box::pin(async move { Ok(payload) })
            })
            .await
        {
            Ok(payload) => match serde_json::from_value(payload.get("result").cloned().unwrap_or(Value::Null)) {
                Ok(result) => result,
                Err(err) => ToolExecutionResult::error("BAD_RESULT", err.to_string()),
            },
            Err(err) => ToolExecutionResult::error("POST_EXECUTE", err.to_string()),
        }
    }
}

/// The `tools` plugin: provides `ctx.tools` and registers the built-in tools.
pub fn tools_plugin() -> Arc<dyn Plugin> {
    plugin("tools", |ctx, _config: Value| async move {
        let registry = ToolRegistry::new(ctx.clone());
        let api: Arc<dyn crate::api::services::ToolRegistryApi> = Arc::new(registry.clone());
        ctx.provide(TOOLS_SERVICE, crate::api::services::ToolsService::new(api))
            .await?;
        crate::tools::builtin::register_builtin_tools(&registry)?;
        Ok(())
    })
}


impl crate::api::services::ToolRegistryApi for ToolRegistry {
    fn schemas(&self) -> Vec<crate::llm::ToolSchema> {
        self.schemas()
    }

    fn list(&self) -> Vec<String> {
        self.list()
    }

    fn execute(
        &self,
        call_id: String,
        name: String,
        arguments: serde_json::Value,
        run_ctx: crate::types::ToolRunContext,
    ) -> crate::api::services::BoxFuture<crate::types::ToolExecutionResult> {
        let registry = self.clone();
        Box::pin(async move {
            registry
                .execute(call_id, name, arguments, run_ctx)
                .await
        })
    }

    fn register_dynamic_tool(&self, spec: crate::api::services::DynamicToolSpec) {
        let exec = spec.exec.clone();
        let definition = ToolDefinition::new(
            spec.name.clone(),
            spec.description.clone(),
            spec.parameters.clone(),
            move |args: crate::types::ToolCallArgs, _run_ctx: crate::types::ToolRunContext| {
                let exec = exec.clone();
                Box::pin(async move { exec(args.arguments).await })
            },
        );
        self.register(Arc::new(definition));
    }

    fn unregister_dynamic_tool(&self, name: &str) {
        self.unregister(name)
    }
}
