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
use std::collections::HashMap;
use std::rc::Rc;

use boa_engine::builtins::promise::PromiseState;
use boa_engine::module::{ModuleLoader, ModuleRequest, Referrer};
use boa_engine::{Context, JsError, JsNativeError, JsResult, JsValue, Module, NativeFunction, Source, js_string};

#[link(wasm_import_module = "host")]
unsafe extern "C" {
    /// Send a JSON request; returns a packed (ptr << 32 | len) JSON reply, allocated with
    /// `js_alloc`, that we free.
    fn request(ptr: *const u8, len: u32) -> u64;
}

thread_local! {
    static CONTEXT: RefCell<Option<Context>> = const { RefCell::new(None) };
    /// Module sources the host has fetched, by absolute URL.
    static MODULE_SOURCES: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
}

/// Resolves `import`s against the importing module's URL, from sources the host fetched.
/// Unknown URLs are recorded in `missing` so the host can fetch them and try again.
#[derive(Default)]
struct PageModuleLoader {
    /// One Module per URL, so a module imported twice is evaluated once.
    modules: RefCell<HashMap<String, Module>>,
    missing: RefCell<Vec<String>>,
}

impl PageModuleLoader {
    fn module(&self, url: &str, ctx: &mut Context) -> JsResult<Module> {
        if let Some(m) = self.modules.borrow().get(url) {
            return Ok(m.clone());
        }
        let Some(source) = MODULE_SOURCES.with(|s| s.borrow().get(url).cloned()) else {
            self.missing.borrow_mut().push(url.to_string());
            return Err(JsNativeError::typ().with_message(format!("module {url} is not loaded yet")).into());
        };
        let module = Module::parse(Source::from_bytes(source.as_bytes()).with_path(std::path::Path::new(url)), None, ctx)?;
        self.modules.borrow_mut().insert(url.to_string(), module.clone());
        Ok(module)
    }
}

impl ModuleLoader for PageModuleLoader {
    async fn load_imported_module(self: Rc<Self>, referrer: Referrer, request: ModuleRequest, context: &RefCell<&mut Context>) -> JsResult<Module> {
        let specifier = request.specifier().to_std_string_escaped();
        let base = match &referrer {
            Referrer::Module(m) => m.path().map(|p| p.to_string_lossy().into_owned()),
            _ => None,
        };
        let Some(base) = base else {
            return Err(JsNativeError::typ().with_message("import() from a classic script is not supported yet").into());
        };
        if !(specifier.starts_with("./") || specifier.starts_with("../") || specifier.starts_with('/') || specifier.contains("://")) {
            return Err(JsNativeError::typ().with_message(format!("bare import \"{specifier}\" needs an import map, which isn't supported yet")).into());
        }
        let url = url::Url::parse(&base)
            .and_then(|b| b.join(&specifier))
            .map_err(|e| JsNativeError::typ().with_message(format!("bad import \"{specifier}\": {e}")))?;
        self.module(url.as_str(), &mut context.borrow_mut())
    }
}

fn new_context() -> Result<Context, String> {
    Context::builder().module_loader(Rc::new(PageModuleLoader::default())).build().map_err(|e| e.to_string())
}

fn rejection(value: JsValue) -> String {
    format!("Uncaught {}", JsError::from_opaque(value))
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
            let mut ctx = match new_context() {
                Ok(ctx) => ctx,
                Err(e) => return pack_string(format!("JS pack setup failed: {e}")),
            };
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

/// Run a classic script. `name` (its URL, or the page and "inline script N") appears in
/// error locations and stack traces.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn js_run(ptr: *const u8, len: u32, name_ptr: *const u8, name_len: u32) -> u64 {
    let source = unsafe { read_str(ptr, len) };
    let name = unsafe { read_str(name_ptr, name_len) };
    with_context(|ctx| {
        let result = ctx.eval(Source::from_bytes(source.as_bytes()).with_path(std::path::Path::new(&name))).map(drop);
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

/// Give the engine the source of module `url` (fetched by the host through the gate).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn js_add_module(url_ptr: *const u8, url_len: u32, src_ptr: *const u8, src_len: u32) {
    let (url, source) = unsafe { (read_str(url_ptr, url_len), read_str(src_ptr, src_len)) };
    MODULE_SOURCES.with(|s| s.borrow_mut().insert(url, source));
}

/// Find the imports of module graph `url` that aren't loaded yet. Uses a scratch context with
/// no page access, so nothing runs. Returns 0 when complete, or a packed JSON reply:
/// `{"missing": [urls]}` or `{"error": message}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn js_discover(url_ptr: *const u8, url_len: u32) -> u64 {
    let url = unsafe { read_str(url_ptr, url_len) };
    let mut ctx = match new_context() {
        Ok(ctx) => ctx,
        Err(e) => return pack_string(format!("{{\"error\":{}}}", serde_json_string(&e))),
    };
    let loader = ctx.downcast_module_loader::<PageModuleLoader>().expect("our loader");
    let entry = match loader.module(&url, &mut ctx) {
        Ok(m) => m,
        Err(e) => return pack_string(format!("{{\"error\":{}}}", serde_json_string(&e.to_string()))),
    };
    let promise = entry.load(&mut ctx);
    let _ = ctx.run_jobs();
    let missing = loader.missing.borrow().clone();
    if !missing.is_empty() {
        let list = missing.iter().map(|m| serde_json_string(m)).collect::<Vec<_>>().join(",");
        return pack_string(format!("{{\"missing\":[{list}]}}"));
    }
    match promise.state() {
        PromiseState::Rejected(v) => pack_string(format!("{{\"error\":{}}}", serde_json_string(&rejection(v)))),
        _ => 0,
    }
}

/// Load, link and evaluate module graph `url` in the page's context (after `js_discover`
/// reported it complete). 0 on success, else a packed error message.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn js_run_module(url_ptr: *const u8, url_len: u32) -> u64 {
    let url = unsafe { read_str(url_ptr, url_len) };
    with_context(|ctx| {
        let loader = ctx.downcast_module_loader::<PageModuleLoader>().expect("our loader");
        let module = loader.module(&url, ctx).map_err(|e| format!("Uncaught {e}"))?;
        let promise = module.load_link_evaluate(ctx);
        let _ = ctx.run_jobs();
        match promise.state() {
            PromiseState::Rejected(v) => Err(rejection(v)),
            _ => Ok(()),
        }
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
