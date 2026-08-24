//! dsh-plugin-hello: a dynamically loaded dsh-rs plugin.
//!
//! Built as a standalone cdylib (`cargo build -p dsh-plugin-hello`), then
//! loaded into a running harness with `dsh plugin load target/debug/libdsh_plugin_hello.dylib`.
//! It registers one tool, `dsh_hello`, on the host's `ctx.tools` registry.
//!
//! Note the dependency list: only `dsh-plugin-contract` and `serde_json` —
//! NO cordis, NO tokio. The host supplies all framework semantics; this
//! library only computes.

use dsh_plugin_contract::{read_in, write_out, DshPluginExports, ABI_VERSION};

struct State {
    invocations: usize,
}

/// JSON declaration: require the host's `tools` service and register one tool.
fn declaration() -> String {
    serde_json::json!({
        "inject": ["tools"],
        "tools": [
            {
                "name": "dsh_hello",
                "description": "Say hello from a dynamically loaded plugin. Returns a greeting and the number of invocations.",
                "parameters": { "type": "object", "properties": {}, "required": [] },
                "exec_id": "hello"
            }
        ]
    })
    .to_string()
}

/// Answer one operation call with a JSON ToolExecutionResult.
fn run_op(state: &mut State, op: &[u8], args: &[u8]) -> String {
    let _args = String::from_utf8_lossy(args);
    match op {
        b"hello" => {
            state.invocations += 1;
            serde_json::json!({
                "kind": "success",
                "content": format!("hello from dynamic plugin (call #{})", state.invocations),
                "value": {
                    "message": "hello from dynamic plugin",
                    "invocations": state.invocations
                }
            })
            .to_string()
        }
        other => serde_json::json!({
            "kind": "error",
            "code": "UNKNOWN_OP",
            "message": format!("unknown op: {}", String::from_utf8_lossy(other))
        })
        .to_string(),
    }
}

unsafe extern "C" fn name(out_buf: *mut u8, cap: usize, out_len: *mut usize) {
    let _ = unsafe { write_out(b"dsh-plugin-hello", out_buf, cap, out_len) };
}

unsafe extern "C" fn setup() -> *mut usize {
    let state = Box::new(State { invocations: 0 });
    Box::into_raw(state) as *mut usize
}

unsafe extern "C" fn describe(
    handle: *mut usize,
    out_buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    let _state = unsafe { &mut *(handle as *mut State) };
    if unsafe { write_out(declaration().as_bytes(), out_buf, cap, out_len) } {
        0
    } else {
        -1
    }
}

unsafe extern "C" fn invoke(
    handle: *mut usize,
    op: *const u8,
    op_len: usize,
    args: *const u8,
    args_len: usize,
    out_buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    let state = unsafe { &mut *(handle as *mut State) };
    let op = unsafe { read_in(op, op_len) };
    let args = unsafe { read_in(args, args_len) };
    let result = run_op(state, op, args);
    if unsafe { write_out(result.as_bytes(), out_buf, cap, out_len) } {
        0
    } else {
        -1
    }
}

unsafe extern "C" fn teardown(handle: *mut usize) {
    if handle.is_null() {
        return;
    }
    let state = unsafe { Box::from_raw(handle as *mut State) };
    eprintln!(
        "[dsh-plugin-hello] teardown; served {} invocation(s)",
        state.invocations
    );
}

#[no_mangle]
pub static dsh_plugin_exports: DshPluginExports = DshPluginExports {
    abi_version: ABI_VERSION,
    name,
    setup,
    describe,
    invoke,
    teardown,
};
