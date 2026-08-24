//! Minimal stable-surface contract between the dsh-rs host and a dynamically
//! loaded plugin.
//!
//! Deliberately contains NO cordis and NO tokio: both sides speak only raw
//! C-ABI functions and caller-owned buffers. A cdylib statically links its
//! own copies of every crate it uses, so any framework call made from
//! library-side code would panic ("no reactor running") or duplicate state —
//! instead the HOST implements the framework and the PLUGIN only computes.
//!
//! The plugin declares itself as JSON through `describe`:
//! ```json
//! {
//!   "inject": ["tools"],
//!   "tools": [
//!     { "name": "dsh_hello", "description": "...", "parameters": {},
//!       "exec_id": "hello" }
//!   ]
//! }
//! ```
//! and answers operation calls through `invoke` (op = `exec_id`, args =
//! UTF-8 JSON, result = UTF-8 JSON). The host wraps the declared tools as
//! [`dsh_tools::ToolDefinition`]s whose `execute` bodies call back into the
//! library.

/// One loaded plugin, as seen by the host. All functions are infallible from
/// the ABI perspective; errors are reported through the returned status.
#[repr(C)]
pub struct DshPluginExports {
    /// ABI version, bumped on incompatible changes. Host rejects mismatches.
    pub abi_version: u32,

    /// Write the plugin display name into out_buf (capacity cap); set *out_len.
    pub name: unsafe extern "C" fn(out_buf: *mut u8, cap: usize, out_len: *mut usize),

    /// Allocate plugin-private state; returned handle is opaque to the host.
    /// May return null to signal init failure.
    pub setup: unsafe extern "C" fn() -> *mut usize,

    /// Write the plugin's JSON declaration (see module docs).
    pub describe:
        unsafe extern "C" fn(handle: *mut usize, out_buf: *mut u8, cap: usize, out_len: *mut usize) -> i32,

    /// Invoke one operation. `op` is the UTF-8 `exec_id`; `args` is UTF-8
    /// JSON. The result is written as UTF-8 JSON:
    /// `{"kind":"success","content":"...","value":{...}}` or
    /// `{"kind":"error","message":"...","code":"..."}`.
    pub invoke: unsafe extern "C" fn(
        handle: *mut usize,
        op: *const u8,
        op_len: usize,
        args: *const u8,
        args_len: usize,
        out_buf: *mut u8,
        cap: usize,
        out_len: *mut usize,
    ) -> i32,

    /// Release plugin-private state. Called exactly once per successful setup.
    pub teardown: unsafe extern "C" fn(handle: *mut usize),
}

pub const ABI_VERSION: u32 = 1;

/// Helper for plugins: copy a byte slice into an out-buffer protocol.
/// Returns false when the buffer is too small.
///
/// # Safety
/// `out_buf`/`out_len` must point to a writable buffer of at least `cap`
/// bytes for the duration of the call.
pub unsafe fn write_out(src: &[u8], out_buf: *mut u8, cap: usize, out_len: *mut usize) -> bool {
    if src.len() > cap {
        return false;
    }
    std::ptr::copy_nonoverlapping(src.as_ptr(), out_buf, src.len());
    *out_len = src.len();
    true
}

/// Helper for plugins: read a `*const u8`/len pair back into a slice.
///
/// # Safety
/// `ptr`/`len` must describe a valid buffer for the duration of the call.
pub unsafe fn read_in<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() || len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }
}
