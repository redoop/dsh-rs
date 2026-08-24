//! Host-side dynamic plugin loading — the dsh-rs analogue of cordis-rs's
//! `dynhost`. A plugin is a standalone cdylib speaking the zero-dep
//! `dsh-plugin-contract`; this module dlopens it, adapts the raw exports to
//! the cordis [`Plugin`] trait, and registers the declared tools on
//! `ctx.tools` — the library itself never touches cordis or tokio.

use std::path::Path;
use std::sync::Arc;

use cordis::plugin::{BoxFuture, Injection, Plugin};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use dsh_plugin_contract::{DshPluginExports, ABI_VERSION};

/// Error from loading or driving a dynamic plugin.
#[derive(Debug, thiserror::Error)]
pub enum DynamicPluginError {
    #[error("cannot open {path}: {source}")]
    Open {
        path: String,
        source: libloading::Error,
    },
    #[error("symbol `dsh_plugin_exports` missing in {path}: {source}")]
    Symbol {
        path: String,
        source: libloading::Error,
    },
    #[error("plugin {path} exported a null export table")]
    NullTable { path: String },
    #[error(
        "plugin {path} speaks ABI v{actual}, host expects v{expected} (recompile the plugin)"
    )]
    AbiMismatch {
        path: String,
        actual: u32,
        expected: u32,
    },
    #[error("bad plugin declaration: {0}")]
    BadDeclaration(String),
    #[error("plugin call failed: {0}")]
    Call(String),
}

pub type Result<T> = std::result::Result<T, DynamicPluginError>;

/// Opaque plugin state pointer. The raw pointer crosses threads only inside
/// host-serialized effect code and spawn_blocking tool bodies.
#[derive(Clone, Copy)]
struct Handle(*mut usize);
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

/// The exports table points at immutable code in the mapped library; sharing
/// it across threads is sound (calls are serialized by the host).
#[derive(Clone)]
struct Exports {
    table: *mut DshPluginExports,
    /// Kept alive so the code stays mapped while in use.
    _lib: Arc<libloading::Library>,
}

// Raw pointers into a mapped library: Send/Sync are sound because the
// library is never unmapped while any handle exists (process exit reclaims).
unsafe impl Send for Exports {}
unsafe impl Sync for Exports {}

impl Exports {
    fn setup(&self) -> Handle {
        Handle(unsafe { ((*self.table).setup)() })
    }

    fn describe(&self, handle: Handle, buf: &mut [u8]) -> Result<String> {
        let mut len = 0usize;
        let rc = unsafe { ((*self.table).describe)(handle.0, buf.as_mut_ptr(), buf.len(), &mut len) };
        if rc != 0 {
            return Err(DynamicPluginError::Call(
                "describe reported a buffer-too-small error".to_string(),
            ));
        }
        Ok(String::from_utf8_lossy(&buf[..len]).into_owned())
    }

    /// Synchronous FFI call; use inside `spawn_blocking`.
    fn invoke_sync(
        &self,
        handle: Handle,
        op: &str,
        args_json: &str,
        buf: &mut [u8],
    ) -> Result<String> {
        let mut len = 0usize;
        let rc = unsafe {
            ((*self.table).invoke)(
                handle.0,
                op.as_ptr(),
                op.len(),
                args_json.as_ptr(),
                args_json.len(),
                buf.as_mut_ptr(),
                buf.len(),
                &mut len,
            )
        };
        if rc != 0 {
            return Err(DynamicPluginError::Call(format!(
                "invoke({op}) returned an error"
            )));
        }
        Ok(String::from_utf8_lossy(&buf[..len]).into_owned())
    }

    fn teardown(&self, handle: Handle) {
        unsafe { ((*self.table).teardown)(handle.0) };
    }
}

/// One tool declared by a dynamic plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DynamicToolDecl {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub parameters: Value,
    /// The op string passed back into `invoke`.
    pub exec_id: String,
}

/// The plugin's JSON declaration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DynamicDeclaration {
    #[serde(default)]
    pub inject: Vec<String>,
    #[serde(default)]
    pub tools: Vec<DynamicToolDecl>,
}

/// A loaded dynamic plugin, adapted to the cordis [`Plugin`] trait.
pub struct DynamicPlugin {
    name: String,
    exports: Exports,
    declaration: DynamicDeclaration,
}

impl std::fmt::Debug for DynamicPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynamicPlugin")
            .field("name", &self.name)
            .field("declaration", &self.declaration)
            .finish()
    }
}

/// dlopen a plugin cdylib, check the ABI, and read its JSON declaration.
pub fn load_dynamic_plugin(path: &Path) -> Result<Arc<DynamicPlugin>> {
    let lib = Arc::new(unsafe { libloading::Library::new(path) }.map_err(|source| {
        DynamicPluginError::Open {
            path: path.display().to_string(),
            source,
        }
    })?);

    let symbol: libloading::Symbol<*mut DshPluginExports> =
        unsafe { lib.get(b"dsh_plugin_exports") }.map_err(|source| DynamicPluginError::Symbol {
            path: path.display().to_string(),
            source,
        })?;
    let table = *symbol;
    if table.is_null() {
        return Err(DynamicPluginError::NullTable {
            path: path.display().to_string(),
        });
    }
    let abi = unsafe { (*table).abi_version };
    if abi != ABI_VERSION {
        return Err(DynamicPluginError::AbiMismatch {
            path: path.display().to_string(),
            actual: abi,
            expected: ABI_VERSION,
        });
    }

    let mut name_buf = [0u8; 256];
    let mut name_len = 0usize;
    unsafe { ((*table).name)(name_buf.as_mut_ptr(), name_buf.len(), &mut name_len) };
    let name = String::from_utf8_lossy(&name_buf[..name_len]).into_owned();

    let exports = Exports { table, _lib: lib };

    // Probe the declaration through a throwaway setup/teardown cycle.
    let handle = exports.setup();
    if handle.0.is_null() {
        return Err(DynamicPluginError::Call("plugin setup returned null".to_string()));
    }
    let mut decl_buf = [0u8; 64 * 1024];
    let decl_json = exports.describe(handle, &mut decl_buf)?;
    exports.teardown(handle);

    let declaration: DynamicDeclaration = serde_json::from_str(&decl_json)
        .map_err(|e| DynamicPluginError::BadDeclaration(e.to_string()))?;

    Ok(Arc::new(DynamicPlugin {
        name,
        exports,
        declaration,
    }))
}

impl DynamicPlugin {
    /// The tool declarations contributed by this plugin.
    pub fn tools(&self) -> &[DynamicToolDecl] {
        &self.declaration.tools
    }
}

/// Convert one dynamic-plugin result JSON into a [`crate::types::ToolExecutionResult`].
fn to_tool_result(value: Value) -> crate::types::ToolExecutionResult {
    match value.get("kind").and_then(|k| k.as_str()) {
        Some("success") => {
            let content = value
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("ok")
                .to_string();
            let value_out = value.get("value").cloned().unwrap_or(Value::Null);
            crate::types::ToolExecutionResult::success_text(content, value_out)
        }
        _ => {
            let message = value
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("dynamic plugin error")
                .to_string();
            let code = value
                .get("code")
                .and_then(|c| c.as_str())
                .unwrap_or("PLUGIN_ERROR")
                .to_string();
            crate::types::ToolExecutionResult::error(code, message)
        }
    }
}

impl Plugin for DynamicPlugin {
    fn name(&self) -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Owned(self.name.clone())
    }

    fn inject(&self) -> Vec<Injection> {
        self.declaration
            .inject
            .iter()
            .map(|name| Injection::new(name.to_string()))
            .collect()
    }

    fn apply(&self, ctx: cordis::Context, _config: Value) -> BoxFuture<cordis::Result<()>> {
        let name = self.name.clone();
        let exports = self.exports.clone();
        let tools = self.tools().to_vec();

        Box::pin(async move {
            let handle = exports.setup();
            if handle.0.is_null() {
                return Err(cordis::Error::msg("dynamic plugin setup failed"));
            }

            // Register every declared tool through the (interface) tool
            // registry as a fiber EFFECT, so unloading unregisters them
            // automatically. Each tool's exec calls back into the library
            // (spawn_blocking: FFI is sync).
            let registry = ctx.get::<crate::api::services::ToolsService>("tools");
            let exports_for_teardown = exports.clone();
            let tools_count = tools.len();
            ctx.effect("dynamic plugin tools", async move {
                let registry = registry.clone();
                let exports = exports.clone();
                let handle = handle;
                if let Some(registry) = &registry {
                    for tool in &tools {
                        let exports = exports.clone();
                        let exec_id = tool.exec_id.clone();
                        let spec = crate::api::services::DynamicToolSpec {
                            name: tool.name.clone(),
                            description: tool.description.clone(),
                            parameters: tool.parameters.clone(),
                            exec: std::sync::Arc::new(
                                move |arguments: Value| {
                                    let exports = exports.clone();
                                    let handle = handle;
                                    let exec_id = exec_id.clone();
                                    Box::pin(async move {
                                        tokio::task::spawn_blocking(move || {
                                            let mut buf = vec![0u8; 64 * 1024];
                                            let args_json = serde_json::to_string(&arguments)
                                                .unwrap_or_else(|_| "null".to_string());
                                            exports
                                                .invoke_sync(handle, &exec_id, &args_json, &mut buf)
                                        })
                                        .await
                                        .map_err(|e| {
                                            crate::types::ToolExecutionResult::error(
                                                "PLUGIN_JOIN",
                                                e.to_string(),
                                            )
                                        })
                                        .and_then(|result| {
                                            result.map_err(|e| {
                                                crate::types::ToolExecutionResult::error(
                                                    "PLUGIN_CALL",
                                                    e.to_string(),
                                                )
                                            })
                                        })
                                        .map(|json| {
                                            serde_json::from_str::<Value>(&json)
                                                .map(to_tool_result)
                                                .unwrap_or_else(|e| {
                                                    crate::types::ToolExecutionResult::error(
                                                        "PLUGIN_RESULT",
                                                        e.to_string(),
                                                    )
                                                })
                                        })
                                        .unwrap_or_else(|err| err)
                                    })
                                },
                            ),
                        };
                        registry.register_dynamic_tool(spec);
                    }
                }

                // Unload unregisters every tool this plugin registered.
                let disposer: cordis::fiber::Disposer = Box::new(move || {
                    let registry = registry.clone();
                    let tools = tools.clone();
                    Box::pin(async move {
                        if let Some(registry) = &registry {
                            for tool in &tools {
                                registry.unregister_dynamic_tool(&tool.name);
                            }
                        }
                    })
                });
                Ok(Some(disposer))
            })
            .await?;

            // Teardown is a fiber effect: unload disposes the plugin state.
            let exports_for_effect = exports_for_teardown.clone();
            ctx.effect("dynamic plugin teardown", async move {
                let disposer: cordis::fiber::Disposer = Box::new(move || {
                    Box::pin(async move {
                        exports_for_effect.teardown(handle);
                    })
                });
                Ok(Some(disposer))
            })
            .await?;

            ctx.logger().log_event(
                cordis::logger::LogLevel::Info,
                "dynamic-plugin".to_string(),
                None,
                format!("{name} active ({tools_count} tool(s))"),
            );
            Ok(())
        })
    }
}
