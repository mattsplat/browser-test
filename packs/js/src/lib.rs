//! The JS engine for the browser's JS pack: Boa, compiled to wasm32-wasip1.
//!
//! The browser runs this in wasmtime with no files, network or environment. Everything the
//! page can reach goes through one import, `host.request`, which takes a JSON request and
//! returns a JSON reply (see prelude.js for the shapes).
//!
//! Exports: `js_alloc`/`js_free` (memory for strings), `js_run` (run a page script),
//! `js_fire` (call a listener or timer) and `js_release` (forget one). `js_run` and `js_fire` return 0 on success, or a packed
//! (ptr << 32 | len) error message the host must `js_free`.

use std::cell::RefCell;

use boa_engine::{Context, JsResult, JsValue, NativeFunction, Source, js_string};

#[link(wasm_import_module = "host")]
unsafe extern "C" {
    /// Send a JSON request; returns a packed (ptr << 32 | len) JSON reply, allocated with
    /// `js_alloc`, that we free.
    fn request(ptr: *const u8, len: u32) -> u64;
}

thread_local! {
    static CONTEXT: RefCell<Option<Context>> = const { RefCell::new(None) };
}

#[unsafe(no_mangle)]
pub extern "C" fn js_alloc(len: u32) -> *mut u8 {
    let mut buf = Vec::<u8>::with_capacity(len as usize);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn js_free(ptr: *mut u8, len: u32) {
    drop(unsafe { Vec::from_raw_parts(ptr, 0, len as usize) });
}

unsafe fn read_str(ptr: *const u8, len: u32) -> String {
    String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(ptr, len as usize) }).into_owned()
}

fn pack_string(s: String) -> u64 {
    let len = s.len() as u32;
    let ptr = js_alloc(len.max(1));
    unsafe { std::ptr::copy_nonoverlapping(s.as_ptr(), ptr, s.len()) };
    ((ptr as u64) << 32) | len as u64
}

/// `__host(json)`: the only native function pages can reach.
fn host_request(_this: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let message = args.first().cloned().unwrap_or_default().to_string(ctx)?.to_std_string_escaped();
    let packed = unsafe { request(message.as_ptr(), message.len() as u32) };
    let (ptr, len) = ((packed >> 32) as *mut u8, packed as u32);
    let reply = unsafe { read_str(ptr, len) };
    unsafe { js_free(ptr, len) };
    Ok(JsValue::from(js_string!(reply.as_str())))
}

fn with_context(f: impl FnOnce(&mut Context) -> Result<(), String>) -> u64 {
    CONTEXT.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let mut ctx = Context::default();
            let setup = ctx
                .register_global_callable(js_string!("__host"), 1, NativeFunction::from_fn_ptr(host_request))
                .map_err(|e| e.to_string())
                .and_then(|_| ctx.eval(Source::from_bytes(include_str!("prelude.js"))).map(drop).map_err(|e| e.to_string()));
            if let Err(e) = setup {
                return pack_string(format!("JS pack setup failed: {e}"));
            }
            *slot = Some(ctx);
        }
        match f(slot.as_mut().unwrap()) {
            Ok(()) => 0,
            Err(e) => pack_string(e),
        }
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn js_run(ptr: *const u8, len: u32) -> u64 {
    let source = unsafe { read_str(ptr, len) };
    with_context(|ctx| {
        let result = ctx.eval(Source::from_bytes(source.as_bytes())).map(drop);
        // Promise jobs (e.g. `.then`) run before control returns to the host.
        let _ = ctx.run_jobs();
        result.map_err(|e| format!("Uncaught {e}"))
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn js_fire(id: u32, args_ptr: *const u8, args_len: u32) -> u64 {
    let args = unsafe { read_str(args_ptr, args_len) };
    with_context(|ctx| {
        let call = format!("__fire({id}, {})", serde_json_string(&args));
        let result = ctx.eval(Source::from_bytes(call.as_bytes())).map(drop);
        let _ = ctx.run_jobs();
        result.map_err(|e| format!("Uncaught {e}"))
    })
}

/// Forget callback `id`: the host no longer refers to it.
#[unsafe(no_mangle)]
pub extern "C" fn js_release(id: u32) {
    let _ = with_context(|ctx| {
        ctx.eval(Source::from_bytes(format!("__release({id})").as_bytes())).map(drop).map_err(|e| e.to_string())
    });
}

/// `s` as a JS string literal.
fn serde_json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
