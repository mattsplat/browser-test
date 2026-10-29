//! What the JS and Python packs share: an interpreter compiled to `wasm32-wasip1` (Boa from
//! `packs/js`, RustPython from `packs/python`), run in wasmtime with the same `Host` as the
//! Dart pack. The page ships source, not Wasm.
//!
//! The interpreter reaches the page through one import, `host.request`, carrying JSON
//! (`{op: "global"|"get"|"set"|"call"|"console", ...}`; values are tagged as below). Its
//! only other imports are WASI functions, implemented here rather than with a WASI runtime so
//! the clock and randomness come from the host (principle 4); there are no files, network or
//! environment, and every file call fails.
//!
//! Each interpreter exports the same ABI under its own prefix (`js_`, `py_`): `alloc`/`free`
//! for strings passed in and out, `run` (a page script), `fire` (a listener or timer),
//! `release` (forget a callback) and `eval` (console input). See "Language packs" in DESIGN.md.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use wasmtime::*;

use crate::host::{Host, HostValue};
use crate::page_document::{PackRuntime, RAN_TOO_LONG, STOPPED, Startup, TIME_LIMIT_TICKS, code_key, engine, interrupted, load_or_compile, memory_limits};

/// One interpreter pack: where its build puts it and what it calls itself.
pub struct Language {
    /// For messages: "JS", "Python".
    pub label: &'static str,
    /// Its exports' prefix: "js" for `js_run` and so on.
    pub prefix: &'static str,
    /// The `.wasm` its build script produces.
    pub path: &'static str,
    /// How to build it, for the error when it's missing.
    pub build: &'static str,
    /// The compiled module, once per process: each page gets a fresh instance of it.
    pub module: std::sync::OnceLock<Module>,
}

/// A page script for an interpreter pack: its name (URL, or the page and a position for
/// inline scripts), its source, and whether it's a module (JS only).
#[derive(Clone, Debug)]
pub struct Script {
    pub name: String,
    pub source: String,
    pub module: bool,
}

struct State {
    lang: &'static Language,
    host: Box<dyn Host>,
    rng: u64,
    started: Instant,
    limits: StoreLimits,
}

/// A page's instance of an interpreter.
pub struct Interpreter {
    lang: &'static Language,
    store: Store<State>,
    instance: Instance,
    /// Stopped for running too long, or crashed. Either way the interpreter left a call
    /// part-way (its state is half-updated, its context still borrowed), so it can't run again.
    stopped: bool,
}

/// The interpreter compiles once per process; each page gets a fresh instance.
fn module(lang: &'static Language) -> Result<&'static Module> {
    if let Some(m) = lang.module.get() {
        return Ok(m);
    }
    let wasm = std::fs::read(lang.path).with_context(|| format!("{} pack not built: run {} ({})", lang.label, lang.build, lang.path))?;
    let module = cached_compile(lang, &wasm)?;
    Ok(lang.module.get_or_init(|| module))
}

/// Compiling an interpreter takes seconds, so keep the compiled code on disk, keyed by a hash
/// of its bytes and of wasmtime's version and configuration (a rebuilt interpreter or a
/// changed config gets a new entry). Every profile's process shares the file: it holds no
/// user data, and every browser has the same interpreters, so a cache hit reveals nothing.
fn cached_compile(lang: &Language, wasm: &[u8]) -> Result<Module> {
    let path = directories::ProjectDirs::from("dev", "mattsplat", "browser-test")
        .map(|d| d.cache_dir().join(format!("{}-engine-{:016x}.cwasm", lang.prefix, code_key(wasm, ""))));
    let (module, compiled) = load_or_compile(wasm, path.as_deref(), &format!("{} pack", lang.label))?;
    if let Some(path) = path.filter(|p| compiled && p.exists()) {
        remove_stale_caches(lang, &path);
    }
    Ok(module)
}

/// After saving a new compiled pack, delete this pack's older ones (an earlier build or
/// wasmtime). A process still running one keeps its mapping: on macOS and Linux, deleting a
/// mapped file only removes its name.
fn remove_stale_caches(lang: &Language, current: &std::path::Path) {
    let Some(dir) = current.parent() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let prefix = format!("{}-engine-", lang.prefix);
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) && name.ends_with(".cwasm") && path != current {
            let _ = std::fs::remove_file(&path);
        }
    }
}

impl Interpreter {
    pub fn load(lang: &'static Language, host: Box<dyn Host>) -> Result<Self> {
        let module = module(lang)?;
        let seed = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
        let mut store = Store::new(engine(), State { lang, host, rng: seed | 1, started: Instant::now(), limits: memory_limits() });
        store.limiter(|state| &mut state.limits);
        store.set_epoch_deadline(TIME_LIMIT_TICKS);
        let mut linker = Linker::new(engine());
        linker.func_wrap("host", "request", host_request)?;
        add_wasi(&mut linker)?;
        let instance = linker.instantiate(&mut store, module)?;
        // A wasip1 library ("reactor") initialises itself here.
        if let Ok(init) = instance.get_typed_func::<(), ()>(&mut store, "_initialize") {
            init.call(&mut store, ())?;
        }
        Ok(Self { lang, store, instance, stopped: false })
    }

    /// Start a call into the interpreter: refused once stopped, otherwise with a fresh time
    /// limit.
    pub(crate) fn enter(&mut self) -> Result<()> {
        if self.stopped {
            bail!("{STOPPED}");
        }
        self.store.set_epoch_deadline(TIME_LIMIT_TICKS);
        Ok(())
    }

    /// Call export `<prefix>_<name>`. The interpreter reports the page's own errors as
    /// return values, so a call that fails didn't return: the time limit stopped it, or it
    /// trapped (a panic in the interpreter, a stack overflow, running out of memory). Either
    /// way it's stopped from then on.
    pub(crate) fn call<P: WasmParams, R: WasmResults>(&mut self, name: &str, params: P) -> Result<R> {
        let export = format!("{}_{name}", self.lang.prefix);
        let f = self.instance.get_typed_func::<P, R>(&mut self.store, &export)?;
        f.call(&mut self.store, params).map_err(|e| {
            self.stopped = true;
            if interrupted(&e) {
                return anyhow!("{RAN_TOO_LONG} ({})", self.lang.label);
            }
            // The wasm backtrace is for us, not the page's console.
            tracing::warn!("the {} pack crashed: {e:?}", self.lang.label);
            match e.downcast_ref::<Trap>() {
                Some(trap) => anyhow!("the {} pack crashed ({trap}); reload the page to run it again", self.lang.label),
                None => anyhow!("the {} pack crashed; reload the page to run it again", self.lang.label),
            }
        })
    }

    /// Copy `s` into interpreter memory (allocated with its `alloc`); returns (ptr, len). The
    /// interpreter frees it.
    pub(crate) fn write_str(&mut self, s: &str) -> Result<(u32, u32)> {
        let ptr = self.call::<u32, u32>("alloc", s.len().max(1) as u32)?;
        self.memory()?.write(&mut self.store, ptr as usize, s.as_bytes())?;
        Ok((ptr, s.len() as u32))
    }

    /// Read and free a packed (ptr << 32 | len) string the interpreter returned.
    pub(crate) fn take_str(&mut self, packed: u64) -> Result<String> {
        let (ptr, len) = ((packed >> 32) as u32, packed as u32);
        let mut buf = vec![0u8; len as usize];
        self.memory()?.read(&self.store, ptr as usize, &mut buf)?;
        self.call::<(u32, u32), ()>("free", (ptr, len.max(1)))?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    fn memory(&mut self) -> Result<Memory> {
        self.instance.get_memory(&mut self.store, "memory").with_context(|| format!("{} pack has no memory", self.lang.label))
    }

    /// 0 is success; anything else is a packed error message.
    pub(crate) fn check(&mut self, status: u64) -> Result<()> {
        if status == 0 {
            return Ok(());
        }
        let message = self.take_str(status)?;
        Err(anyhow!("{message}"))
    }

    /// Run a page script; `name` shows in error locations and stack traces.
    pub fn run(&mut self, name: &str, source: &str) -> Result<()> {
        self.enter()?;
        let (ptr, len) = self.write_str(source)?;
        let (name_ptr, name_len) = self.write_str(name)?;
        let status = self.call::<(u32, u32, u32, u32), u64>("run", (ptr, len, name_ptr, name_len))?;
        self.check(status)
    }

    /// Run the page's classic scripts in order. Like a browser, one failing is reported and
    /// the next still runs.
    pub fn run_scripts(&mut self, scripts: &[Script], startup: &mut Startup) {
        for script in scripts.iter().filter(|s| !s.module) {
            if let Err(e) = self.run(&script.name, &script.source) {
                startup.uncaught("script", script.name.clone(), &e);
            }
        }
    }
}

impl PackRuntime for Interpreter {
    fn eval(&mut self, source: &str) -> Result<String> {
        self.enter()?;
        let (ptr, len) = self.write_str(source)?;
        let packed = self.call::<(u32, u32), u64>("eval", (ptr, len))?;
        let reply: Value = serde_json::from_str(&self.take_str(packed)?)?;
        match (reply["ok"].as_str(), reply["err"].as_str()) {
            (Some(shown), _) => Ok(shown.to_string()),
            (None, Some(error)) => Err(anyhow!("{error}")),
            _ => bail!("unexpected reply from the {} pack: {reply}", self.lang.label),
        }
    }

    fn release_callback(&mut self, id: usize) {
        if self.enter().is_ok() {
            let _ = self.call::<u32, ()>("release", id as u32);
        }
    }

    fn fire_callback(&mut self, id: usize, args: &[HostValue]) -> Result<()> {
        self.enter()?;
        let encoded = Value::Array(args.iter().map(encode).collect::<Result<_>>()?).to_string();
        let (ptr, len) = self.write_str(&encoded)?;
        let status = self.call::<(u32, u32, u32), u64>("fire", (id as u32, ptr, len))?;
        self.check(status)
    }

    fn stopped(&self) -> bool {
        self.stopped
    }
}

fn caller_memory(caller: &mut Caller<'_, State>) -> Result<Memory> {
    caller.get_export("memory").and_then(|e| e.into_memory()).context("the pack has no memory export")
}

/// The pack's bytes at `ptr..ptr + len`, checked against its memory before anything is
/// allocated (a bad length from the pack can't make the host allocate gigabytes).
fn read_caller(caller: &mut Caller<'_, State>, ptr: u32, len: u32) -> Result<Vec<u8>> {
    let memory = caller_memory(caller)?;
    let end = (ptr as usize).checked_add(len as usize).context("pointer out of range")?;
    Ok(memory.data(&*caller).get(ptr as usize..end).context("pointer out of range")?.to_vec())
}

/// Most bytes one `fd_write` sends to the trace, and one `random_get` fills.
const MAX_IO: u32 = 64 * 1024;

/// `host.request`: one JSON request in, one JSON reply out (allocated in interpreter memory
/// with its `alloc`, which it frees).
fn host_request(mut caller: Caller<'_, State>, ptr: u32, len: u32) -> wasmtime::Result<u64> {
    let request = read_caller(&mut caller, ptr, len).map_err(|e| wasmtime::Error::msg(format!("{e:#}")))?;
    let reply = match serde_json::from_slice::<Value>(&request).map_err(anyhow::Error::from).and_then(|r| dispatch(caller.data_mut().host.as_mut(), &r)) {
        Ok(value) => json!({ "ok": value }),
        Err(e) => json!({ "err": format!("{e:#}") }),
    }
    .to_string();
    let alloc_export = format!("{}_alloc", caller.data().lang.prefix);
    let alloc = caller.get_export(&alloc_export).and_then(|e| e.into_func());
    let alloc = alloc.ok_or_else(|| wasmtime::Error::msg(format!("the pack has no {alloc_export} export")))?.typed::<u32, u32>(&caller)?;
    let out = alloc.call(&mut caller, reply.len().max(1) as u32)?;
    caller_memory(&mut caller).map_err(|e| wasmtime::Error::msg(format!("{e:#}")))?.write(&mut caller, out as usize, reply.as_bytes())?;
    Ok(((out as u64) << 32) | reply.len() as u64)
}

fn dispatch(host: &mut dyn Host, request: &Value) -> Result<Value> {
    let field = |name: &str| request.get(name).with_context(|| format!("request missing {name}"));
    let name = || field("name").and_then(|n| n.as_str().context("name is not a string")).map(str::to_string);
    let target = || field("target").and_then(|t| t.as_u64().context("target is not a handle")).map(|h| HostValue::Object(h as u32));
    let value = match field("op")?.as_str() {
        Some("global") => host.global(&name()?)?,
        Some("get") => host.get(&target()?, &name()?)?,
        Some("set") => {
            host.set(&target()?, &name()?, &decode(field("value")?)?)?;
            HostValue::Undefined
        }
        Some("call") => {
            let args = field("args")?.as_array().context("args is not an array")?.iter().map(decode).collect::<Result<Vec<_>>>()?;
            host.call(&target()?, &name()?, &args)?
        }
        Some("console") => {
            host.console(field("text")?.as_str().unwrap_or_default());
            HostValue::Undefined
        }
        other => bail!("unknown request op {other:?}"),
    };
    encode(&value)
}

/// JSON → host value. Undefined, host objects and callbacks use small tagged objects:
/// `{"u": 1}`, `{"o": handle}`, `{"cb": id}`.
fn decode(v: &Value) -> Result<HostValue> {
    Ok(match v {
        Value::Null => HostValue::Null,
        Value::Bool(b) => HostValue::Bool(*b),
        Value::Number(n) => HostValue::Num(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => HostValue::str(s),
        Value::Object(o) if o.contains_key("u") => HostValue::Undefined,
        Value::Object(o) if o.contains_key("o") => HostValue::Object(o["o"].as_u64().context("bad handle")? as u32),
        Value::Object(o) if o.contains_key("cb") => HostValue::CallbackId(o["cb"].as_u64().context("bad callback")? as usize),
        other => bail!("the value {other} cannot be passed to the host yet"),
    })
}

fn encode(v: &HostValue) -> Result<Value> {
    Ok(match v {
        HostValue::Null => Value::Null,
        HostValue::Undefined => json!({ "u": 1 }),
        HostValue::Bool(b) => json!(b),
        HostValue::Num(n) => json!(n),
        HostValue::Str(_) => Value::String(v.display()),
        HostValue::Object(h) => json!({ "o": h }),
        HostValue::CallbackId(id) => json!({ "cb": id }),
        other => bail!("{} cannot be passed to the pack", other.display()),
    })
}

/// The WASI calls the interpreters make, answered by the host. Clock and randomness come
/// from the host; output goes to the trace; there is no environment, no arguments, and no
/// files: no directory is preopened and every file call fails with EBADF, so nothing the
/// interpreter's standard library tries (opening a path, reading stdin) reaches the machine.
fn add_wasi(linker: &mut Linker<State>) -> Result<()> {
    const WASI: &str = "wasi_snapshot_preview1";
    const EBADF: i32 = 8;
    const EINVAL: i32 = 28;
    const ENOSYS: i32 = 52;
    let wasm_err = |e: anyhow::Error| wasmtime::Error::msg(format!("{e:#}"));

    linker.func_wrap(WASI, "random_get", move |mut caller: Caller<'_, State>, buf: u32, len: u32| -> wasmtime::Result<i32> {
        if len > MAX_IO {
            return Ok(EINVAL);
        }
        let mut bytes = vec![0u8; len as usize];
        for b in bytes.iter_mut() {
            let rng = &mut caller.data_mut().rng;
            *rng ^= *rng << 13;
            *rng ^= *rng >> 7;
            *rng ^= *rng << 17;
            *b = *rng as u8;
        }
        caller_memory(&mut caller).map_err(wasm_err)?.write(&mut caller, buf as usize, &bytes)?;
        Ok(0)
    })?;
    linker.func_wrap(WASI, "clock_time_get", move |mut caller: Caller<'_, State>, id: u32, _precision: u64, out: u32| -> wasmtime::Result<i32> {
        // 0 = realtime, otherwise monotonic since the page started.
        let nanos = match id {
            0 => SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0),
            _ => caller.data().started.elapsed().as_nanos() as u64,
        };
        caller_memory(&mut caller).map_err(wasm_err)?.write(&mut caller, out as usize, &nanos.to_le_bytes())?;
        Ok(0)
    })?;
    linker.func_wrap(WASI, "clock_res_get", move |mut caller: Caller<'_, State>, _id: u32, out: u32| -> wasmtime::Result<i32> {
        caller_memory(&mut caller).map_err(wasm_err)?.write(&mut caller, out as usize, &1_000u64.to_le_bytes())?;
        Ok(0)
    })?;
    // No environment and no arguments: both are empty lists.
    for (sizes, get) in [("environ_sizes_get", "environ_get"), ("args_sizes_get", "args_get")] {
        linker.func_wrap(WASI, sizes, move |mut caller: Caller<'_, State>, count: u32, size: u32| -> wasmtime::Result<i32> {
            let memory = caller_memory(&mut caller).map_err(wasm_err)?;
            memory.write(&mut caller, count as usize, &0u32.to_le_bytes())?;
            memory.write(&mut caller, size as usize, &0u32.to_le_bytes())?;
            Ok(0)
        })?;
        linker.func_wrap(WASI, get, |_: u32, _: u32| -> i32 { 0 })?;
    }
    // stdout goes to the trace, as console output. stderr is only ever the interpreter's own
    // (a Rust panic message), so it goes to the browser's log, not the page's console.
    linker.func_wrap(WASI, "fd_write", move |mut caller: Caller<'_, State>, fd: u32, iovs: u32, iovs_len: u32, written: u32| -> wasmtime::Result<i32> {
        let mut text = Vec::new();
        let mut total = 0u32;
        for i in 0..iovs_len {
            let at = i.checked_mul(8).and_then(|o| iovs.checked_add(o)).ok_or_else(|| wasmtime::Error::msg("iovec out of range"))?;
            let iov = read_caller(&mut caller, at, 8).map_err(wasm_err)?;
            let (ptr, len) = (u32::from_le_bytes(iov[..4].try_into()?), u32::from_le_bytes(iov[4..].try_into()?));
            total = total.saturating_add(len);
            // Past the cap the rest is dropped, but reported written so the writer moves on.
            let keep = len.min(MAX_IO.saturating_sub(text.len() as u32));
            text.extend(read_caller(&mut caller, ptr, keep).map_err(wasm_err)?);
        }
        let memory = caller_memory(&mut caller).map_err(wasm_err)?;
        memory.write(&mut caller, written as usize, &total.to_le_bytes())?;
        let text = String::from_utf8_lossy(&text).into_owned();
        match fd {
            2 => tracing::warn!("{} pack stderr: {text}", caller.data().lang.label),
            _ => caller.data_mut().host.console(&text),
        }
        Ok(0)
    })?;
    // No files. `fd_prestat_get` failing tells WASI libc there are no preopened directories,
    // so it can't resolve any path; the rest fail if called anyway.
    linker.func_wrap(WASI, "fd_prestat_get", |_: u32, _: u32| -> i32 { EBADF })?;
    linker.func_wrap(WASI, "fd_prestat_dir_name", |_: u32, _: u32, _: u32| -> i32 { EBADF })?;
    linker.func_wrap(WASI, "fd_close", |_: u32| -> i32 { EBADF })?;
    linker.func_wrap(WASI, "fd_fdstat_get", |_: u32, _: u32| -> i32 { EBADF })?;
    linker.func_wrap(WASI, "fd_filestat_get", |_: u32, _: u32| -> i32 { EBADF })?;
    linker.func_wrap(WASI, "fd_read", |_: u32, _: u32, _: u32, _: u32| -> i32 { EBADF })?;
    linker.func_wrap(WASI, "fd_readdir", |_: u32, _: u32, _: u32, _: u64, _: u32| -> i32 { EBADF })?;
    linker.func_wrap(WASI, "fd_seek", |_: u32, _: i64, _: u32, _: u32| -> i32 { EBADF })?;
    linker.func_wrap(WASI, "path_filestat_get", |_: u32, _: u32, _: u32, _: u32, _: u32| -> i32 { EBADF })?;
    linker.func_wrap(WASI, "path_open", |_: u32, _: u32, _: u32, _: u32, _: u32, _: u64, _: u64, _: u32, _: u32| -> i32 { EBADF })?;
    linker.func_wrap(WASI, "poll_oneoff", |_: u32, _: u32, _: u32, _: u32| -> i32 { ENOSYS })?;
    linker.func_wrap(WASI, "sched_yield", || -> i32 { 0 })?;
    linker.func_wrap(WASI, "proc_exit", |code: u32| -> wasmtime::Result<()> { Err(wasmtime::Error::msg(format!("the pack exited with code {code}"))) })?;
    Ok(())
}
