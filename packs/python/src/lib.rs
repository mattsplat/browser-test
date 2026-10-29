//! The Python interpreter for the browser's Python pack: RustPython, compiled to wasm32-wasip1.
//!
//! The browser runs this in wasmtime with no files, network or environment, and without
//! Python's standard library (only the modules built into the interpreter). Everything the page
//! can reach goes through one import, `host.request`, which takes a JSON request and returns a
//! JSON reply, the same shapes as the JS pack's (see prelude.py).
//!
//! Exports, the same ABI as the JS pack's with a `py_` prefix: `py_alloc`/`py_free` (memory
//! for strings), `py_run` (run a page script), `py_fire` (call a listener or timer) and
//! `py_release` (forget one). `py_run` and `py_fire` return 0 on success, or a packed
//! (ptr << 32 | len) error message the host must `py_free`. `py_eval` runs console input and
//! always returns a packed JSON reply.

use std::cell::RefCell;

use rustpython_vm::builtins::PyBaseExceptionRef;
use rustpython_vm::compiler::Mode;
use rustpython_vm::scope::Scope;
use rustpython_vm::{Interpreter, PyObjectRef, PyResult, VirtualMachine, py_serde};

#[link(wasm_import_module = "host")]
unsafe extern "C" {
    /// Send a JSON request; returns a packed (ptr << 32 | len) JSON reply, allocated with
    /// `py_alloc`, that we free.
    fn request(ptr: *const u8, len: u32) -> u64;
}

/// The page's interpreter: its global namespace (`__main__`, shared by all its scripts, like a
/// browser's global scope) and the prelude's, which holds the bridge.
struct Page {
    interpreter: Interpreter,
    globals: Scope,
    prelude: Scope,
}

thread_local! {
    static PAGE: RefCell<Option<Page>> = const { RefCell::new(None) };
}

/// Frames in the prelude are the bridge, not the page's code, so tracebacks leave them out.
const PRELUDE_NAME: &str = "<prelude>";

#[unsafe(no_mangle)]
pub extern "C" fn py_alloc(len: u32) -> *mut u8 {
    let mut buf = Vec::<u8>::with_capacity(len as usize);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn py_free(ptr: *mut u8, len: u32) {
    drop(unsafe { Vec::from_raw_parts(ptr, 0, len as usize) });
}

unsafe fn read_str(ptr: *const u8, len: u32) -> String {
    String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(ptr, len as usize) }).into_owned()
}

/// Read a string the host wrote into our memory (with `py_alloc`) and free it.
unsafe fn take_input(ptr: *const u8, len: u32) -> String {
    let s = unsafe { read_str(ptr, len) };
    unsafe { py_free(ptr as *mut u8, len.max(1)) };
    s
}

fn pack_string(s: String) -> u64 {
    let len = s.len() as u32;
    let ptr = py_alloc(len.max(1));
    unsafe { std::ptr::copy_nonoverlapping(s.as_ptr(), ptr, s.len()) };
    ((ptr as u64) << 32) | len as u64
}

/// `host_request(message)`: the prelude's only way out. The message (dicts, lists, strings,
/// numbers) crosses as JSON, and so does the reply.
fn host_request(message: PyObjectRef, vm: &VirtualMachine) -> PyResult {
    let json = serde_json::to_string(&py_serde::PyObjectSerializer::new(vm, &message)).map_err(|e| vm.new_type_error(e.to_string()))?;
    let packed = unsafe { request(json.as_ptr(), json.len() as u32) };
    let (ptr, len) = ((packed >> 32) as *mut u8, packed as u32);
    let reply = unsafe { read_str(ptr, len) };
    unsafe { py_free(ptr, len) };
    from_json(vm, &reply)
}

fn from_json(vm: &VirtualMachine, json: &str) -> PyResult {
    py_serde::deserialize(vm, &mut serde_json::Deserializer::from_str(json)).map_err(|e| vm.new_value_error(e.to_string()))
}

/// An uncaught exception, read like a browser console's: `Uncaught ValueError: boom`, then
/// Python's own traceback (file, line and function of each frame), without the prelude's.
fn describe(vm: &VirtualMachine, exc: &PyBaseExceptionRef) -> String {
    let mut text = String::new();
    let _ = vm.write_exception(&mut text, exc);
    let prelude_frame = format!("  File \"{PRELUDE_NAME}\"");
    // A runaway recursion repeats one frame a thousand times; say so once, as CPython does.
    let mut lines: Vec<String> = Vec::new();
    let mut repeats = 0;
    for line in text.lines().filter(|l| !l.starts_with(&prelude_frame)) {
        if lines.last().is_some_and(|last| last == line) {
            repeats += 1;
            continue;
        }
        if repeats > 0 {
            lines.push(format!("  [Previous line repeated {repeats} more times]"));
            repeats = 0;
        }
        lines.push(line.to_string());
    }
    match lines.split_last() {
        Some((summary, traceback)) if !traceback.is_empty() => format!("Uncaught {summary}\n{}", traceback.join("\n")),
        Some((summary, _)) => format!("Uncaught {summary}"),
        None => "Uncaught exception".into(),
    }
}

/// Compile and run `source` (named `name` in tracebacks) in `scope`.
fn exec(vm: &VirtualMachine, source: &str, name: &str, mode: Mode, scope: Scope) -> Result<PyObjectRef, String> {
    let code = vm.compile(source, mode, name).map_err(|e| describe(vm, &e.into_pyexception(vm, Some(source))))?;
    vm.run_code_obj(code, scope).map_err(|e| describe(vm, &e))
}

fn start() -> Result<Page, String> {
    let interpreter = Interpreter::without_stdlib(Default::default());
    let (globals, prelude) = interpreter.enter(|vm| {
        let globals = vm.new_scope_with_main().map_err(|e| describe(vm, &e))?;
        let prelude = vm.new_scope_with_builtins();
        let host = vm.new_function("host_request", host_request);
        prelude.globals.set_item("host_request", host.into(), vm).map_err(|e| describe(vm, &e))?;
        exec(vm, include_str!("prelude.py"), PRELUDE_NAME, Mode::Exec, prelude.clone())?;
        Ok::<_, String>((globals, prelude))
    })?;
    Ok(Page { interpreter, globals, prelude })
}

/// Run `f` in the page's interpreter, starting it on first use. 0, or a packed error message.
fn with_page(f: impl FnOnce(&VirtualMachine, &Page) -> Result<(), String>) -> u64 {
    PAGE.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            match start() {
                Ok(page) => *slot = Some(page),
                Err(e) => return pack_string(format!("Python pack setup failed: {e}")),
            }
        }
        let page = slot.as_ref().unwrap();
        match page.interpreter.enter(|vm| f(vm, page)) {
            Ok(()) => 0,
            Err(e) => pack_string(e),
        }
    })
}

/// Call prelude function `name` with `args`.
fn call_prelude(vm: &VirtualMachine, page: &Page, name: &str, args: Vec<PyObjectRef>) -> Result<PyObjectRef, String> {
    let f = page.prelude.globals.get_item(name, vm).map_err(|e| describe(vm, &e))?;
    f.call(args, vm).map_err(|e| describe(vm, &e))
}

/// Remove the indentation every line shares, as `textwrap.dedent` does: an inline
/// `<script>` is usually indented to match the HTML around it. Line numbers don't change.
fn dedent(source: &str) -> String {
    let indent = |line: &str| line.len() - line.trim_start_matches([' ', '\t']).len();
    let lines = || source.lines().filter(|l| !l.trim().is_empty());
    let Some(first) = lines().next() else { return source.to_string() };
    let mut common = &first[..indent(first)];
    for line in lines() {
        while !line.starts_with(common) {
            common = &common[..common.len() - 1];
        }
    }
    source.lines().map(|l| l.strip_prefix(common).unwrap_or(l.trim_start())).collect::<Vec<_>>().join("\n")
}

/// Run a page script. `name` (its URL, or the page and "inline script N") appears in
/// tracebacks.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn py_run(ptr: *const u8, len: u32, name_ptr: *const u8, name_len: u32) -> u64 {
    let source = dedent(&unsafe { take_input(ptr, len) });
    let name = unsafe { take_input(name_ptr, name_len) };
    with_page(|vm, page| exec(vm, &source, &name, Mode::Exec, page.globals.clone()).map(drop))
}

/// Call listener or timer `id` with `args` (a JSON array of host values).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn py_fire(id: u32, args_ptr: *const u8, args_len: u32) -> u64 {
    let args = unsafe { take_input(args_ptr, args_len) };
    with_page(|vm, page| {
        let args = from_json(vm, &args).map_err(|e| describe(vm, &e))?;
        call_prelude(vm, page, "fire", vec![vm.ctx.new_int(id).into(), args]).map(drop)
    })
}

/// Evaluate console input in the page's namespace (so it sees the page's globals); the value
/// of the last statement is shown with `repr`, as the Python REPL does. Returns a packed JSON
/// reply: `{"ok": text}` or `{"err": message}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn py_eval(ptr: *const u8, len: u32) -> u64 {
    let source = unsafe { take_input(ptr, len) };
    let mut shown = String::new();
    let status = with_page(|vm, page| {
        let value = exec(vm, &source, "console", Mode::BlockExpr, page.globals.clone())?;
        let text = value.repr(vm).map_err(|e| describe(vm, &e))?;
        shown = text.to_string_lossy().into_owned();
        Ok(())
    });
    let reply = if status == 0 {
        serde_json::json!({ "ok": shown })
    } else {
        let (p, l) = ((status >> 32) as *mut u8, status as u32);
        let message = unsafe { read_str(p, l) };
        unsafe { py_free(p, l) };
        serde_json::json!({ "err": message })
    };
    pack_string(reply.to_string())
}

/// Forget callback `id`: the host no longer refers to it.
#[unsafe(no_mangle)]
pub extern "C" fn py_release(id: u32) {
    let _ = with_page(|vm, page| call_prelude(vm, page, "release", vec![vm.ctx.new_int(id).into()]).map(drop));
}
