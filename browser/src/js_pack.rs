//! The JS language pack: a JS engine (Boa, built from `packs/js`) compiled to WebAssembly,
//! run in wasmtime with the same `Host` as the Dart pack. JS gets nothing the Dart pack
//! doesn't (principle 1), and is off unless a site is allowed.
//!
//! The engine reaches the page through one import, `host.request`, carrying JSON
//! (`{op: "global"|"get"|"set"|"call"|"console", ...}`). It also imports a handful of WASI
//! functions, implemented here rather than with a WASI runtime so the clock and randomness
//! come from the host (principle 4); there are no files, network or environment.

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use wasmtime::*;

use crate::dart_pack::{Host, HostValue, RAN_TOO_LONG, STOPPED, TIME_LIMIT_TICKS, interrupted};
use crate::page_document::PackRuntime;

/// Where `packs/js/build.sh` puts the engine.
const ENGINE_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../packs/js/target/wasm32-wasip1/release/js_pack.wasm");

struct State {
    host: Box<dyn Host>,
    rng: u64,
    started: Instant,
}

pub struct JsRuntime {
    store: Store<State>,
    instance: Instance,
    /// Stopped for running too long. The engine was interrupted mid-call (its context is
    /// left borrowed), so it can't run again.
    stopped: bool,
}

/// The engine compiles once per process; each page gets a fresh instance.
fn engine_module() -> Result<&'static Module> {
    static MODULE: OnceLock<Module> = OnceLock::new();
    if let Some(m) = MODULE.get() {
        return Ok(m);
    }
    let wasm = std::fs::read(ENGINE_PATH).with_context(|| format!("JS engine not built: run packs/js/build.sh ({ENGINE_PATH})"))?;
    let module = cached_compile(&wasm)?;
    Ok(MODULE.get_or_init(|| module))
}

/// Compiling the engine takes seconds, so keep the compiled code on disk, keyed by a hash of
/// the engine's bytes and of wasmtime's version and configuration (a rebuilt engine or a
/// changed config gets a new entry). Every profile's process shares the file and maps it
/// into memory, so it's only ever replaced whole (`write_atomic`), never rewritten in place.
fn cached_compile(wasm: &[u8]) -> Result<Module> {
    use std::hash::{Hash, Hasher};
    let engine = crate::dart_pack::engine();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    wasm.hash(&mut hasher);
    engine.precompile_compatibility_hash().hash(&mut hasher);
    let path = directories::ProjectDirs::from("dev", "mattsplat", "browser-test")
        .map(|d| d.cache_dir().join(format!("js-engine-{:016x}.cwasm", hasher.finish())));
    if let Some(path) = &path {
        if path.exists() {
            // Safety: this file was written by `Module::serialize` below, for this engine
            // configuration, into our own cache folder.
            match unsafe { Module::deserialize_file(engine, path) } {
                Ok(module) => return Ok(module),
                Err(e) => tracing::warn!("ignoring stale compiled JS engine: {e:#}"),
            }
        }
    }
    let module = Module::new(engine, wasm)?;
    if let Some(path) = &path {
        let saved = path.parent().map(std::fs::create_dir_all).transpose().and_then(|_| {
            module.serialize().map_err(std::io::Error::other).and_then(|bytes| crate::profile::write_atomic(path, &bytes))
        });
        if let Err(e) = saved {
            tracing::warn!("could not cache the compiled JS engine: {e}");
        }
    }
    Ok(module)
}

impl JsRuntime {
    pub fn load(host: Box<dyn Host>) -> Result<Self> {
        let module = engine_module()?;
        let seed = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
        let mut store = Store::new(crate::dart_pack::engine(), State { host, rng: seed | 1, started: Instant::now() });
        store.set_epoch_deadline(TIME_LIMIT_TICKS);
        let mut linker = Linker::new(crate::dart_pack::engine());
        linker.func_wrap("host", "request", host_request)?;
        add_wasi(&mut linker)?;
        let instance = linker.instantiate(&mut store, module)?;
        // A wasip1 library ("reactor") initialises itself here.
        if let Some(init) = instance.get_typed_func::<(), ()>(&mut store, "_initialize").ok() {
            init.call(&mut store, ())?;
        }
        Ok(Self { store, instance, stopped: false })
    }

    /// Start a call into the engine: refused once stopped, otherwise with a fresh time limit.
    fn enter(&mut self) -> Result<()> {
        if self.stopped {
            bail!("{STOPPED}");
        }
        self.store.set_epoch_deadline(TIME_LIMIT_TICKS);
        Ok(())
    }

    /// Call engine export `name`, reporting the time limit stopping it.
    fn call<P: WasmParams, R: WasmResults>(&mut self, name: &str, params: P) -> Result<R> {
        let f = self.instance.get_typed_func::<P, R>(&mut self.store, name)?;
        f.call(&mut self.store, params).map_err(|e| {
            if interrupted(&e) {
                self.stopped = true;
                anyhow!("{RAN_TOO_LONG} (JS)")
            } else {
                anyhow!("{e:#}")
            }
        })
    }

    /// Run a page script; `name` shows in error locations and stack traces.
    pub fn run(&mut self, name: &str, source: &str) -> Result<()> {
        self.enter()?;
        let (ptr, len) = write_str(&mut self.store, &self.instance, source)?;
        let (name_ptr, name_len) = write_str(&mut self.store, &self.instance, name)?;
        let status = self.call::<(u32, u32, u32, u32), u64>("js_run", (ptr, len, name_ptr, name_len))?;
        self.check(status)
    }

    /// Give the engine module `url`'s source.
    pub fn add_module(&mut self, url: &str, source: &str) -> Result<()> {
        self.enter()?;
        let (u, ul) = write_str(&mut self.store, &self.instance, url)?;
        let (s, sl) = write_str(&mut self.store, &self.instance, source)?;
        self.call::<(u32, u32, u32, u32), ()>("js_add_module", (u, ul, s, sl))
    }

    /// The imports of module graph `url` that aren't loaded yet (empty when complete).
    pub fn discover(&mut self, url: &str) -> Result<Vec<String>> {
        self.enter()?;
        let (u, ul) = write_str(&mut self.store, &self.instance, url)?;
        let status = self.call::<(u32, u32), u64>("js_discover", (u, ul))?;
        if status == 0 {
            return Ok(Vec::new());
        }
        let reply: Value = serde_json::from_str(&take_str(&mut self.store, &self.instance, status)?)?;
        if let Some(error) = reply.get("error").and_then(|e| e.as_str()) {
            bail!("{error}");
        }
        Ok(reply["missing"].as_array().into_iter().flatten().filter_map(|m| m.as_str().map(String::from)).collect())
    }

    /// Link and evaluate module graph `url` (once `discover` reports it complete).
    pub fn run_module(&mut self, url: &str) -> Result<()> {
        self.enter()?;
        let (u, ul) = write_str(&mut self.store, &self.instance, url)?;
        let status = self.call::<(u32, u32), u64>("js_run_module", (u, ul))?;
        self.check(status)
    }

    /// 0 is success; anything else is a packed error message.
    fn check(&mut self, status: u64) -> Result<()> {
        if status == 0 {
            return Ok(());
        }
        let message = take_str(&mut self.store, &self.instance, status)?;
        Err(anyhow!("{message}"))
    }
}

impl PackRuntime for JsRuntime {
    fn eval(&mut self, source: &str) -> Result<String> {
        self.enter()?;
        let (ptr, len) = write_str(&mut self.store, &self.instance, source)?;
        let packed = self.call::<(u32, u32), u64>("js_eval", (ptr, len))?;
        let reply: Value = serde_json::from_str(&take_str(&mut self.store, &self.instance, packed)?)?;
        match (reply["ok"].as_str(), reply["err"].as_str()) {
            (Some(shown), _) => Ok(shown.to_string()),
            (None, Some(error)) => Err(anyhow!("{error}")),
            _ => bail!("unexpected reply from the JS engine: {reply}"),
        }
    }

    fn release_callback(&mut self, id: usize) {
        if self.enter().is_ok() {
            let _ = self.call::<u32, ()>("js_release", id as u32);
        }
    }

    fn fire_callback(&mut self, id: usize, args: &[HostValue]) -> Result<()> {
        self.enter()?;
        let encoded = Value::Array(args.iter().map(encode).collect::<Result<_>>()?).to_string();
        let (ptr, len) = write_str(&mut self.store, &self.instance, &encoded)?;
        let status = self.call::<(u32, u32, u32), u64>("js_fire", (id as u32, ptr, len))?;
        self.check(status)
    }

    fn stopped(&self) -> bool {
        self.stopped
    }
}

/// Copy `s` into engine memory (allocated with its `js_alloc`); returns (ptr, len).
fn write_str(store: &mut Store<State>, instance: &Instance, s: &str) -> Result<(u32, u32)> {
    let alloc = instance.get_typed_func::<u32, u32>(&mut *store, "js_alloc")?;
    let ptr = alloc.call(&mut *store, s.len().max(1) as u32)?;
    instance.get_memory(&mut *store, "memory").context("JS engine has no memory")?.write(&mut *store, ptr as usize, s.as_bytes())?;
    Ok((ptr, s.len() as u32))
}

/// Read and free a packed (ptr << 32 | len) string the engine returned.
fn take_str(store: &mut Store<State>, instance: &Instance, packed: u64) -> Result<String> {
    let (ptr, len) = ((packed >> 32) as u32, packed as u32);
    let mut buf = vec![0u8; len as usize];
    instance.get_memory(&mut *store, "memory").context("JS engine has no memory")?.read(&mut *store, ptr as usize, &mut buf)?;
    let free = instance.get_typed_func::<(u32, u32), ()>(&mut *store, "js_free")?;
    free.call(&mut *store, (ptr, len.max(1)))?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn caller_memory(caller: &mut Caller<'_, State>) -> Result<Memory> {
    caller.get_export("memory").and_then(|e| e.into_memory()).context("JS engine has no memory export")
}

fn read_caller(caller: &mut Caller<'_, State>, ptr: u32, len: u32) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len as usize];
    caller_memory(caller)?.read(&*caller, ptr as usize, &mut buf)?;
    Ok(buf)
}

/// `host.request`: one JSON request in, one JSON reply out (allocated in engine memory).
fn host_request(mut caller: Caller<'_, State>, ptr: u32, len: u32) -> wasmtime::Result<u64> {
    let request = read_caller(&mut caller, ptr, len).map_err(|e| wasmtime::Error::msg(format!("{e:#}")))?;
    let reply = match serde_json::from_slice::<Value>(&request).map_err(anyhow::Error::from).and_then(|r| dispatch(caller.data_mut().host.as_mut(), &r)) {
        Ok(value) => json!({ "ok": value }),
        Err(e) => json!({ "err": format!("{e:#}") }),
    }
    .to_string();
    let alloc = caller.get_export("js_alloc").and_then(|e| e.into_func()).ok_or_else(|| wasmtime::Error::msg("no js_alloc"))?;
    let alloc = alloc.typed::<u32, u32>(&caller)?;
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

/// JSON → host value. Undefined, host objects and callbacks use small tagged objects.
fn decode(v: &Value) -> Result<HostValue> {
    Ok(match v {
        Value::Null => HostValue::Null,
        Value::Bool(b) => HostValue::Bool(*b),
        Value::Number(n) => HostValue::Num(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => HostValue::str(s),
        Value::Object(o) if o.contains_key("u") => HostValue::Undefined,
        Value::Object(o) if o.contains_key("o") => HostValue::Object(o["o"].as_u64().context("bad handle")? as u32),
        Value::Object(o) if o.contains_key("cb") => HostValue::CallbackId(o["cb"].as_u64().context("bad callback")? as usize),
        other => bail!("JS value {other} cannot be passed to the host yet"),
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
        other => bail!("{} cannot be passed to JS", other.display()),
    })
}

/// The WASI calls the engine makes, answered by the host.
fn add_wasi(linker: &mut Linker<State>) -> Result<()> {
    const WASI: &str = "wasi_snapshot_preview1";
    const ENOSYS: i32 = 52;
    let wasm_err = |e: anyhow::Error| wasmtime::Error::msg(format!("{e:#}"));

    linker.func_wrap(WASI, "random_get", move |mut caller: Caller<'_, State>, buf: u32, len: u32| -> wasmtime::Result<i32> {
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
    // No environment.
    linker.func_wrap(WASI, "environ_sizes_get", move |mut caller: Caller<'_, State>, count: u32, size: u32| -> wasmtime::Result<i32> {
        let memory = caller_memory(&mut caller).map_err(wasm_err)?;
        memory.write(&mut caller, count as usize, &0u32.to_le_bytes())?;
        memory.write(&mut caller, size as usize, &0u32.to_le_bytes())?;
        Ok(0)
    })?;
    linker.func_wrap(WASI, "environ_get", |_: u32, _: u32| -> i32 { 0 })?;
    // stdout/stderr (e.g. a Rust panic message in the engine) go to the trace.
    linker.func_wrap(WASI, "fd_write", move |mut caller: Caller<'_, State>, _fd: u32, iovs: u32, iovs_len: u32, written: u32| -> wasmtime::Result<i32> {
        let memory = caller_memory(&mut caller).map_err(wasm_err)?;
        let mut text = Vec::new();
        for i in 0..iovs_len {
            let mut iov = [0u8; 8];
            memory.read(&caller, (iovs + i * 8) as usize, &mut iov)?;
            let (ptr, len) = (u32::from_le_bytes(iov[..4].try_into()?), u32::from_le_bytes(iov[4..].try_into()?));
            let mut chunk = vec![0u8; len as usize];
            memory.read(&caller, ptr as usize, &mut chunk)?;
            text.extend(chunk);
        }
        memory.write(&mut caller, written as usize, &(text.len() as u32).to_le_bytes())?;
        caller.data_mut().host.console(&String::from_utf8_lossy(&text));
        Ok(0)
    })?;
    linker.func_wrap(WASI, "poll_oneoff", |_: u32, _: u32, _: u32, _: u32| -> i32 { ENOSYS })?;
    linker.func_wrap(WASI, "sched_yield", || -> i32 { 0 })?;
    linker.func_wrap(WASI, "proc_exit", |code: u32| -> wasmtime::Result<()> { Err(wasmtime::Error::msg(format!("JS engine exited with code {code}"))) })?;
    Ok(())
}
