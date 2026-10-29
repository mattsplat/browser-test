//! The Dart language pack: runs dart2wasm output in wasmtime, with no JS engine.
//!
//! Dart's wasm treats strings and other values as opaque host references (externref) and
//! calls small JS snippets for helpers. We provide those in Rust: values are `HostValue`s
//! inside externrefs, and each dart2wasm import is recognised by its snippet in the import
//! table (read as data, never executed). DOM access comes from `package:web`, whose snippets
//! have four shapes - read a global, call a method, get or set a property - which go to the
//! `Host` by name. Unknown snippets trap with their text.
//!
//! The wasm must be built with `packs/dart/build.sh`, which converts dart2wasm's legacy
//! exception instructions into the standard ones wasmtime implements, and embeds the import
//! table in a `dart2wasm-imports` custom section so no `.mjs` is shipped.

use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use wasmtime::*;

use crate::host::{Host, HostValue, format_number};
use crate::page_document::{PackDocument, PackRuntime, RAN_TOO_LONG, STOPPED, TIME_LIMIT_TICKS, engine, interrupted, memory_limits};
use crate::source_map::SourceMap;

/// Something the page asked the host to call later.
#[derive(Clone)]
enum Callback {
    /// A Dart function wrapped with `.toJS`: its trampoline and closure.
    Function(Func, OwnedRooted<AnyRef>),
    /// A task dart2wasm scheduled (a timer); run with `$invokeCallback`.
    Task(HostValue),
}

/// A loaded Dart program.
pub struct DartRuntime {
    store: Store<State>,
    instance: Instance,
    /// Stopped for running too long. Dart's runtime state is unknown after that (an
    /// interrupted call may be mid-update), so the program doesn't run again.
    stopped: bool,
}

struct State {
    host: Box<dyn Host>,
    /// Callbacks by id. A released slot is `None`; ids are never reused.
    callbacks: Vec<Option<Callback>>,
    /// Work Dart queued with `queueMicrotask`; run after every entry into Dart.
    microtasks: std::collections::VecDeque<HostValue>,
    /// From the `dart2wasm-sourcemap` section, when the build embedded one.
    source_map: Option<SourceMap>,
    /// The function `build.sh`'s entry point registers to format errors with Dart's toString.
    describe_error: Option<usize>,
    limits: StoreLimits,
}

impl DartRuntime {
    /// Instantiate a Dart program (`module` is `wasm`, compiled). Does not run `main`.
    pub fn load(wasm: &[u8], module: &Module, host: Box<dyn Host>) -> Result<Self> {
        let table = read_import_table(&embedded_import_table(wasm)?)?;
        // A broken map only costs readable stack traces, so don't fail the page over it.
        let source_map = custom_section(wasm, "dart2wasm-sourcemap")?
            .and_then(|json| SourceMap::parse(&String::from_utf8_lossy(json)).inspect_err(|e| tracing::warn!("{e:#}")).ok());
        let state = State { host, callbacks: Vec::new(), microtasks: Default::default(), source_map, describe_error: None, limits: memory_limits() };
        let mut store = Store::new(engine(), state);
        store.limiter(|state| &mut state.limits);
        // Instantiating may run code too.
        store.set_epoch_deadline(TIME_LIMIT_TICKS);

        let mut imports: Vec<Extern> = Vec::new();
        for import in module.imports() {
            let (m, name) = (import.module(), import.name());
            let ext: Extern = match import.ty() {
                ExternType::Func(ty) => {
                    let (m, name) = (m.to_string(), name.to_string());
                    let helper = if m == "dart2wasm" {
                        Some(table.get(&name).cloned().ok_or_else(|| anyhow!("import {name} not in .mjs"))?)
                    } else {
                        None
                    };
                    let fty = ty.clone();
                    Func::new(&mut store, ty, move |mut caller, params, results| {
                        let outcome = match (m.as_str(), &helper) {
                            ("dart2wasm", Some(h)) => call_helper(h, &mut caller, params, results, &fty)
                                .with_context(|| format!("in {h:?}")),
                            ("wasm:js-string", _) => js_string(&name, &mut caller, params, results, &fty),
                            _ => Err(anyhow!("unsupported import {m}.{name}")),
                        };
                        // wasmtime has its own error type; keep the full message chain.
                        outcome.map_err(|e| wasmtime::Error::msg(format!("{e:#}")))
                    })
                    .into()
                }
                // String constants: the field name is the string itself.
                ExternType::Global(ty) if m.is_empty() => {
                    let s = ExternRef::new(&mut store, HostValue::str(name))?;
                    Global::new(&mut store, ty, Val::ExternRef(Some(s)))?.into()
                }
                // The JS exceptions tag: our own tag with the same type.
                ExternType::Tag(ty) => Tag::new(&mut store, &ty)?.into(),
                other => bail!("unsupported import {m}.{name}: {other:?}"),
            };
            imports.push(ext);
        }

        let instance = Instance::new(&mut store, &module, &imports)?;
        Ok(Self { store, instance, stopped: false })
    }

    pub fn stopped(&self) -> bool {
        self.stopped
    }

    /// Start a call into Dart: refused once stopped, otherwise with a fresh time limit.
    fn enter(&mut self) -> Result<()> {
        if self.stopped {
            bail!("{STOPPED}");
        }
        self.store.set_epoch_deadline(TIME_LIMIT_TICKS);
        Ok(())
    }

    pub fn run_main(&mut self) -> Result<()> {
        self.enter()?;
        let main = self.instance.get_func(&mut self.store, "$invokeMain").context("no $invokeMain export")?;
        let args = ExternRef::new(&mut self.store, HostValue::Undefined)?;
        let ran = main.call(&mut self.store, &[Val::ExternRef(Some(args))], &mut []);
        ran.map_err(|e| self.explain(e))?;
        self.run_microtasks()
    }

    /// Call a Dart function the page handed over and return its result, without running
    /// microtasks or explaining errors (used while explaining one).
    fn call_function(&mut self, id: usize, args: &[HostValue]) -> Result<HostValue> {
        let Some(Callback::Function(func, closure)) = self.store.data().callbacks.get(id).cloned().flatten() else {
            bail!("callback {id} is not a Dart function");
        };
        let ty = func.ty(&self.store);
        let mut values = vec![HostValue::Opaque(closure), HostValue::Num(args.len() as f64)];
        values.extend(args.iter().cloned());
        let params: Vec<ValType> = ty.params().collect();
        let mut call_args = Vec::new();
        for (i, pty) in params.iter().enumerate() {
            call_args.push(from_host(&mut self.store, values.get(i).cloned().unwrap_or(HostValue::Undefined), pty)?);
        }
        let mut out: Vec<Val> = ty.results().map(|_| Val::null_extern_ref()).collect();
        func.call(&mut self.store, &call_args, &mut out).map_err(|e| anyhow!("{e:#}"))?;
        match out.first() {
            Some(v) => to_host(&mut self.store, v),
            None => Ok(HostValue::Undefined),
        }
    }

    /// Invoke a callback the page registered, e.g. an event listener, with `args`.
    pub fn fire_callback(&mut self, id: usize, args: &[HostValue]) -> Result<()> {
        self.enter()?;
        let (func, closure) = match self.store.data().callbacks.get(id).cloned().flatten() {
            Some(Callback::Function(func, closure)) => (func, closure),
            Some(Callback::Task(task)) => return self.run_task(task),
            None => bail!("callback {id} was released"),
        };
        // The trampoline is `wasmFunction(f, arguments.length, x0, ...)`: closure, argument
        // count, then the arguments, converted to whatever types the trampoline declares.
        let ty = func.ty(&self.store);
        let mut values = vec![HostValue::Opaque(closure), HostValue::Num(args.len() as f64)];
        values.extend(args.iter().cloned());
        let params: Vec<ValType> = ty.params().collect();
        let mut call_args = Vec::new();
        for (i, pty) in params.iter().enumerate() {
            let value = values.get(i).cloned().unwrap_or(HostValue::Undefined);
            call_args.push(from_host(&mut self.store, value, pty)?);
        }
        let mut out: Vec<Val> = ty.results().map(|_| Val::null_extern_ref()).collect();
        let ran = func.call(&mut self.store, &call_args, &mut out);
        ran.map_err(|e| self.explain(e))?;
        self.run_microtasks()
    }

    /// Forget a callback the host no longer needs (a fired timeout, a removed listener).
    pub fn release_callback(&mut self, id: usize) {
        if let Some(slot) = self.store.data_mut().callbacks.get_mut(id) {
            *slot = None;
        }
    }

    /// Run one task dart2wasm scheduled (a timer firing), then its microtasks.
    fn run_task(&mut self, task: HostValue) -> Result<()> {
        let invoke = self
            .instance
            .get_func(&mut self.store, "$invokeCallback")
            .context("no $invokeCallback export")?;
        let pty = invoke.ty(&self.store).params().next().context("$invokeCallback takes no arguments")?;
        let arg = from_host(&mut self.store, task, &pty)?;
        // Some programs' $invokeCallback returns a value; give it somewhere to go.
        let mut out = result_slots(&self.store, &invoke);
        let ran = invoke.call(&mut self.store, &[arg], &mut out);
        ran.map_err(|e| self.explain(e))?;
        self.run_microtasks()
    }

    /// Run everything Dart queued with `queueMicrotask`, including work queued meanwhile.
    fn run_microtasks(&mut self) -> Result<()> {
        let invoke = self
            .instance
            .get_func(&mut self.store, "$invokeCallback")
            .context("no $invokeCallback export")?;
        let pty = invoke.ty(&self.store).params().next().context("$invokeCallback takes no arguments")?;
        while let Some(task) = self.store.data_mut().microtasks.pop_front() {
            let arg = from_host(&mut self.store, task, &pty)?;
            let mut out = result_slots(&self.store, &invoke);
            let ran = invoke.call(&mut self.store, &[arg], &mut out);
            ran.map_err(|e| self.explain(e))?;
        }
        Ok(())
    }

    /// Turn an error out of Dart into something a developer can act on: the thrown Dart
    /// error's message and a stack trace in Dart source lines.
    fn explain(&mut self, err: wasmtime::Error) -> anyhow::Error {
        if interrupted(&err) {
            self.stopped = true;
            return anyhow!("{RAN_TOO_LONG} (Dart)");
        }
        let mut strings = Vec::new();
        let mut described = None;
        if err.downcast_ref::<ThrownException>().is_some() {
            if let Some(exn) = self.store.take_pending_exception() {
                // Best: Dart's own toString, via the function build.sh's entry registered.
                if let (Some(describe), Ok(Val::AnyRef(Some(thrown)))) = (self.store.data().describe_error, exn.field(&mut self.store, 0)) {
                    if let Ok(owned) = thrown.to_owned_rooted(&mut self.store) {
                        if let Ok(HostValue::Str(s)) = self.call_function(describe, &[HostValue::Opaque(owned)]) {
                            described = Some(String::from_utf16_lossy(&s));
                        }
                    }
                }
                // The host can't call Dart's toString, so collect the strings the error object
                // holds: its message, and the stack trace Dart captured when it was thrown.
                for i in 0..4 {
                    match exn.field(&mut self.store, i) {
                        Ok(field) => collect_strings(&mut self.store, &field, 0, &mut strings),
                        Err(_) => break,
                    }
                }
            }
        }
        let (stacks, messages): (Vec<String>, Vec<String>) = strings.into_iter().partition(|s| s.contains("    at "));
        let stack = stacks.into_iter().next().map(|s| trim_js_stack(&s)).or_else(|| {
            let map = self.store.data().source_map.as_ref();
            err.downcast_ref::<WasmBacktrace>().map(|bt| format_stack(map, bt))
        });
        // A trap (a stack overflow, running out of memory) isn't a Dart error: Dart's code
        // stopped part-way without unwinding, so, as for the interpreters, it doesn't run again.
        let trapped = err.downcast_ref::<Trap>();
        if trapped.is_some() {
            self.stopped = true;
        }
        let headline = match (described, messages.is_empty(), trapped) {
            (Some(text), _, _) => format!("Uncaught Dart error: {text}"),
            (None, _, Some(trap)) => format!("the Dart pack crashed ({trap}); reload the page to run it again"),
            (None, true, None) => format!("Dart error: {}", err.root_cause()),
            (None, false, None) => format!("Uncaught Dart error: {}", messages.join(" | ")),
        };
        anyhow!("{headline}\n{}", stack.unwrap_or_default())
    }
}

/// Load a Dart program against the page `doc` and run its `main`. An error out of `main`
/// fails the start: the loader traces it and shows it in place of the page.
pub fn start(doc: blitz_dom::BaseDocument, wasm: &[u8]) -> Result<PackDocument> {
    let origin = crate::settings::origin_of(doc.url());
    let module = compiled(wasm, &origin, crate::profile::dir())?;
    let (host, startup) = PackDocument::starting(doc, "dart");
    let mut runtime = DartRuntime::load(wasm, &module, Box::new(host))?;
    if let Err(e) = runtime.run_main() {
        // A snapshot keeps the page's state from just before.
        startup.snapshot("main", String::new(), &e);
        return Err(e);
    }
    Ok(startup.finish(Box::new(runtime)))
}

/// Compiled Dart programs kept per profile, at most this many (the oldest go first).
const KEEP_COMPILED: usize = 32;

/// A page's Dart program, compiled. Compiled code is cached on disk like the interpreters'
/// (`<profile>/compiled-dart/<key>.cwasm`, keyed by the wasm, wasmtime's compatibility hash
/// and the page's origin), so reloads, other tabs and later runs of the same site skip
/// compiling. Unlike the interpreters', it's partitioned by site and kept in the profile: a
/// cache hit loads measurably faster, and a site could time it to learn whether this
/// machine ran some other site's program, or ran it in another profile. Only compiled code
/// is shared; every page still gets its own instance and store. With no profile folder
/// (tests), nothing is saved.
fn compiled(wasm: &[u8], origin: &str, profile: Option<&std::path::Path>) -> Result<Module> {
    let dir = profile.map(|p| p.join("compiled-dart"));
    let path = dir.as_ref().map(|d| d.join(format!("{:016x}.cwasm", crate::page_document::code_key(wasm, origin))));
    let (module, fresh) = crate::page_document::load_or_compile(wasm, path.as_deref(), "Dart program")?;
    if let (true, Some(dir)) = (fresh, &dir) {
        prune_compiled(dir, KEEP_COMPILED);
    }
    Ok(module)
}

/// Keep the `keep` most recently written compiled programs in `dir`.
fn prune_compiled(dir: &std::path::Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "cwasm"))
        .filter_map(|e| Some((e.metadata().and_then(|m| m.modified()).ok()?, e.path())))
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in files.into_iter().skip(keep) {
        let _ = std::fs::remove_file(path);
    }
}

impl PackRuntime for DartRuntime {
    fn fire_callback(&mut self, id: usize, args: &[HostValue]) -> Result<()> {
        DartRuntime::fire_callback(self, id, args)
    }
    fn release_callback(&mut self, id: usize) {
        DartRuntime::release_callback(self, id)
    }
    fn can_eval(&self) -> bool {
        false
    }
    fn eval(&mut self, _source: &str) -> Result<String> {
        bail!("Dart pages can't evaluate code: dart2wasm compiles ahead of time and the page ships no compiler. Inspect the page in Elements, or print from the Dart source.")
    }
    fn stopped(&self) -> bool {
        DartRuntime::stopped(self)
    }
}

/// Placeholder result values for calling `func`, one per declared result.
fn result_slots(store: &Store<State>, func: &Func) -> Vec<Val> {
    func.ty(store).results().map(|_| Val::null_extern_ref()).collect()
}

/// Collect host strings inside a Dart value: the strings an error object holds.
fn collect_strings(store: &mut Store<State>, value: &Val, depth: usize, out: &mut Vec<String>) {
    if depth > 3 || out.len() > 8 {
        return;
    }
    match value {
        Val::ExternRef(Some(_)) => {
            if let Ok(HostValue::Str(s)) = to_host(store, value) {
                let s = String::from_utf16_lossy(&s);
                if !s.is_empty() && !out.contains(&s) {
                    out.push(s);
                }
            }
        }
        Val::AnyRef(Some(any)) => {
            let Ok(Some(object)) = any.as_struct(&*store) else { return };
            let count = object.ty(&*store).map(|t| t.fields().len()).unwrap_or(0);
            for i in 0..count {
                if let Ok(field) = object.field(&mut *store, i) {
                    collect_strings(store, &field, depth + 1, out);
                }
            }
        }
        _ => {}
    }
}

/// What dart2wasm's stack helper does to `new Error().stack`: drop the first four frames
/// (the capture helpers), plus the leading "Error" line. Then drop Dart's own error-throwing
/// frames, which JS engines don't show either.
fn trim_js_stack(stack: &str) -> String {
    let lines: Vec<&str> = stack.lines().collect();
    let drop = if lines.first().is_some_and(|l| l.starts_with("Error")) { 5 } else { 4 };
    lines
        .into_iter()
        .skip(drop)
        .skip_while(|l| l.contains("dart-sdk/_internal/"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A backtrace as Dart source lines, like a JS engine's `Error().stack`.
fn format_stack(map: Option<&SourceMap>, backtrace: &WasmBacktrace) -> String {
    let mut lines = Vec::new();
    for frame in backtrace.frames() {
        let location = frame.module_offset().and_then(|o| map?.lookup(o as u32));
        let line = match location {
            Some(loc) => format!("    at {loc}"),
            None => format!("    at wasm-function[{}]:{:#x}", frame.func_index(), frame.module_offset().unwrap_or(0)),
        };
        // Inlined helpers can map several frames to one line; show it once.
        if lines.last() != Some(&line) {
            lines.push(line);
        }
    }
    lines.join("\n")
}

/// What a dart2wasm import does, recognised from its JS snippet.
#[derive(Clone, Debug)]
enum Helper {
    /// `() => globalThis.name`
    Global(String),
    /// `(x0, ...) => x0.name(x1, ...)`
    Method(String),
    /// `x0 => x0.name`
    Get(String),
    /// `(x0,x1) => { x0.name = x1 }`
    Set(String),
    /// `x0 => { globalThis.name = x0 }`
    SetGlobal(String),
    Truthy,
    StrictEquals,
    LastIndexOf,
    QueueMicrotask,
    Print,
    NumToString,
    IndexOf,
    /// `new Error().stack`: the current stack, used for Dart's `StackTrace.current`.
    Stack,
    /// Dart's trimming of a captured stack: drop the "Error" line and capture frames.
    TrimStack,
    /// Stack of a JS error object; there are none here.
    ExnStack,
    IsUndefined,
    ToStr,
    JsonStringify,
    Identity,
    WrapCallback,
    /// `o => typeof o === 'function' && o[jsWrappedDartFunctionSymbol] === true`
    IsDartFunction,
    /// `f => f.dartFunction`
    DartFunction,
    ClassifyNumber,
    /// `0` null/undefined, `1` boolean, `2` other.
    ClassifyBool,
    /// `0` null/undefined, `1` string, `2` other.
    ClassifyString,
    /// dart2wasm's type switch: 1 undefined, 2 boolean, 3 number, 4 string, 19 other object.
    TypeCode,
    /// `(o, i) => o[i]`
    Index,
    /// `(string, times) => string.repeat(times)`
    Repeat,
    DateNow,
    /// `(ms, c) => setTimeout(...)` / `setInterval(...)`: run task `c` later.
    SetTimer { repeat: bool },
    /// `clearTimeout(handle)` / `clearInterval(handle)`
    ClearTimer,
    Unsupported(String),
}

fn classify(body: &str) -> Helper {
    // The four shapes package:web's generated bindings compile to.
    static SHAPES: OnceLock<[(Regex, fn(String) -> Helper); 5]> = OnceLock::new();
    let shapes = SHAPES.get_or_init(|| {
        [
            (Regex::new(r"^\(\) => globalThis\.(\w+)$").unwrap(), Helper::Global as fn(String) -> Helper),
            (Regex::new(r"^\(?x0(?:,x\d+)*\)? => x0\.(\w+)\((?:x\d+(?:,x\d+)*)?\)$").unwrap(), Helper::Method),
            (Regex::new(r"^x0 => x0\.(\w+)$").unwrap(), Helper::Get),
            (Regex::new(r"^\(x0,x1\) => \{ x0\.(\w+) = x1 \}$").unwrap(), Helper::Set),
            (Regex::new(r"^x0 => \{ globalThis\.(\w+) = x0 \}$").unwrap(), Helper::SetGlobal),
        ]
    });
    let b = body.trim().trim_end_matches(',');
    for (shape, helper) in shapes {
        if let Some(c) = shape.captures(b) {
            return helper(c[1].to_string());
        }
    }
    match b {
        "b => !!b" => Helper::Truthy,
        "(l, r) => l === r" => Helper::StrictEquals,
        "(s, p, i) => s.lastIndexOf(p, i)" => Helper::LastIndexOf,
        _ if b.contains("queueMicrotask(") => Helper::QueueMicrotask,
        "s => printToConsole(s)" => Helper::Print,
        "Function.prototype.call.bind(Number.prototype.toString)"
        | "Function.prototype.call.bind(BigInt.prototype.toString)" => Helper::NumToString,
        "Function.prototype.call.bind(String.prototype.indexOf)" => Helper::IndexOf,
        "() => new Error().stack" => Helper::Stack,
        "o => o === undefined" => Helper::IsUndefined,
        "o => String(o)" => Helper::ToStr,
        "s => JSON.stringify(s)" => Helper::JsonStringify,
        "o => o" => Helper::Identity,
        "o => o.length" => Helper::Get("length".into()),
        "(o, i) => o[i]" => Helper::Index,
        "(string, times) => string.repeat(times)" => Helper::Repeat,
        // DateTime.now() imports `Date.now` itself.
        "() => Date.now()" | "Date.now" => Helper::DateNow,
        "f => f.dartFunction" => Helper::DartFunction,
        "(handle) => clearTimeout(handle)" | "(handle) => clearInterval(handle)" => Helper::ClearTimer,
        _ if b.starts_with("(ms, c) =>") && b.contains("setTimeout(") => Helper::SetTimer { repeat: false },
        _ if b.starts_with("(ms, c) =>") && b.contains("setInterval(") => Helper::SetTimer { repeat: true },
        _ if b.contains("o[jsWrappedDartFunctionSymbol] === true") => Helper::IsDartFunction,
        _ if b.contains("ArrayBuffer.isView(o)") && b.contains("return 19") => Helper::TypeCode,
        _ if b.contains("typeof o === 'boolean'") && b.contains("return 2") => Helper::ClassifyBool,
        _ if b.contains("typeof(o) === 'string'") && b.contains("return 2") => Helper::ClassifyString,
        _ if b.starts_with("(exn) =>") && b.contains("frames.slice(drop)") => Helper::TrimStack,
        _ if b.starts_with("(exn) =>") => Helper::ExnStack,
        _ if b.starts_with("(wasmFunction,f) => finalizeWrapper") => Helper::WrapCallback,
        _ if b.contains("typeof o === 'number'") => Helper::ClassifyNumber,
        _ => Helper::Unsupported(b.to_string()),
    }
}

/// The import table `packs/dart/embed_imports.dart` stored in the wasm's custom section.
fn embedded_import_table(wasm: &[u8]) -> Result<String> {
    match custom_section(wasm, "dart2wasm-imports")? {
        Some(table) => Ok(String::from_utf8_lossy(table).into_owned()),
        None => bail!("no dart2wasm-imports section: build this page's Dart with packs/dart/build.sh"),
    }
}

/// The contents of the custom section called `name`, if the module has one.
fn custom_section<'a>(wasm: &'a [u8], name: &str) -> Result<Option<&'a [u8]>> {
    let leb = |i: &mut usize| -> Result<usize> {
        let (mut value, mut shift) = (0usize, 0);
        loop {
            let byte = *wasm.get(*i).context("truncated wasm")?;
            *i += 1;
            value |= ((byte & 0x7f) as usize) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
        }
    };
    // Skip the 8-byte header, then walk sections: id, size, contents.
    let mut i = 8;
    while i < wasm.len() {
        let id = wasm[i];
        i += 1;
        let size = leb(&mut i)?;
        let end = i + size;
        if id == 0 {
            let mut j = i;
            let name_len = leb(&mut j)?;
            if wasm.get(j..j + name_len) == Some(name.as_bytes()) {
                return wasm.get(j + name_len..end).context("truncated custom section").map(Some);
            }
        }
        i = end;
    }
    Ok(None)
}

/// Read `const dart2wasm = { X: body, ... }`, the import table dart2wasm writes.
fn read_import_table(source: &str) -> Result<HashMap<String, Helper>> {
    let start = source.find("const dart2wasm = {").context("no dart2wasm import table")?;
    let rest = &source[start..];
    let end = rest.find("\n    };").context("unterminated dart2wasm import table")?;
    let entry = Regex::new(r"^\s+([A-Za-z_$][\w$]*): (.*)$").unwrap();

    let mut table: HashMap<String, String> = HashMap::new();
    let mut current: Option<String> = None;
    for line in rest[..end].lines().skip(1) {
        // Each entry starts with a `name: body` line; anything else continues the previous body.
        match entry.captures(line) {
            Some(c) => {
                current = Some(c[1].to_string());
                table.insert(c[1].to_string(), c[2].to_string());
            }
            None => {
                if let Some(k) = &current {
                    let body = table.get_mut(k).unwrap();
                    body.push('\n');
                    body.push_str(line.trim());
                }
            }
        }
    }
    Ok(table.into_iter().map(|(k, v)| (k, classify(&v))).collect())
}

fn to_host(store: &mut impl AsContextMut, val: &Val) -> Result<HostValue> {
    Ok(match val {
        Val::I32(x) => HostValue::Num(*x as f64),
        Val::I64(x) => HostValue::Num(*x as f64),
        Val::F32(x) => HostValue::Num(f32::from_bits(*x) as f64),
        Val::F64(x) => HostValue::Num(f64::from_bits(*x)),
        Val::ExternRef(None) => HostValue::Null,
        Val::ExternRef(Some(r)) => match r.data(&mut *store)? {
            Some(data) => data.downcast_ref::<HostValue>().context("externref is not a HostValue")?.clone(),
            // No host data: a Dart value converted with extern.convert_any.
            None => {
                let any = AnyRef::convert_extern(&mut *store, *r)?;
                any_to_host(store, &any)?
            }
        },
        Val::AnyRef(None) => HostValue::Null,
        Val::AnyRef(Some(r)) => any_to_host(store, r)?,
        other => bail!("cannot convert {other:?} to a host value"),
    })
}

/// A wasm GC reference as the host sees it. Small ints arrive as i31refs, which a JS engine
/// turns into Numbers; anything else stays an opaque Dart object.
fn any_to_host(store: &mut impl AsContextMut, any: &Rooted<AnyRef>) -> Result<HostValue> {
    Ok(match any.as_i31(&mut *store)? {
        Some(i) => HostValue::Num(i.get_i32() as f64),
        None => HostValue::Opaque(any.to_owned_rooted(&mut *store)?),
    })
}

fn from_host(store: &mut impl AsContextMut, value: HostValue, ty: &ValType) -> Result<Val> {
    let num = |v: &HostValue| match v {
        HostValue::Num(n) => *n,
        HostValue::Bool(b) => *b as i32 as f64,
        _ => f64::NAN,
    };
    Ok(match ty {
        ValType::I32 => Val::I32(num(&value) as i64 as i32),
        ValType::I64 => Val::I64(num(&value) as i64),
        ValType::F32 => Val::F32((num(&value) as f32).to_bits()),
        ValType::F64 => Val::F64(num(&value).to_bits()),
        ValType::Ref(r) if r.heap_type().is_extern() => match value {
            HostValue::Null => Val::ExternRef(None),
            HostValue::Opaque(a) => {
                let a = a.to_rooted(&mut *store);
                Val::ExternRef(Some(ExternRef::convert_any(&mut *store, a)?))
            }
            other => Val::ExternRef(Some(ExternRef::new(store, other)?)),
        },
        ValType::Ref(_) => match value {
            HostValue::Opaque(a) => Val::AnyRef(Some(a.to_rooted(&mut *store))),
            HostValue::Undefined | HostValue::Null => Val::AnyRef(None),
            other => bail!("cannot return {other:?} as {ty:?}"),
        },
        other => bail!("cannot return a host value as {other:?}"),
    })
}

fn set_result(store: &mut impl AsContextMut, results: &mut [Val], ty: &FuncType, value: HostValue) -> Result<()> {
    if let (Some(slot), Some(rt)) = (results.first_mut(), ty.results().next()) {
        *slot = from_host(store, value, &rt)?;
    }
    Ok(())
}

fn i16_array_call(caller: &mut Caller<'_, State>, export: &str, args: &[Val], want_result: bool) -> Result<i32> {
    let f = caller
        .get_export(export)
        .and_then(|e| e.into_func())
        .with_context(|| format!("missing export {export}"))?;
    let mut out = [Val::I32(0)];
    f.call(&mut *caller, args, if want_result { &mut out } else { &mut [] })?;
    Ok(out[0].unwrap_i32())
}

/// `wasm:js-string` builtins, implemented over host strings.
fn js_string(name: &str, caller: &mut Caller<'_, State>, params: &[Val], results: &mut [Val], ty: &FuncType) -> Result<()> {
    let s = |c: &mut Caller<'_, State>, v: &Val| -> Result<Vec<u16>> {
        match to_host(c, v)? {
            HostValue::Str(s) => Ok(s),
            other => Ok(other.display().encode_utf16().collect()),
        }
    };
    let value = match name {
        "length" => HostValue::Num(s(caller, &params[0])?.len() as f64),
        "charCodeAt" => {
            let st = s(caller, &params[0])?;
            HostValue::Num(st.get(params[1].unwrap_i32() as usize).copied().unwrap_or(0) as f64)
        }
        "concat" => {
            let mut a = s(caller, &params[0])?;
            a.extend(s(caller, &params[1])?);
            HostValue::Str(a)
        }
        "equals" => HostValue::Bool(s(caller, &params[0])? == s(caller, &params[1])?),
        "compare" => HostValue::Num(match s(caller, &params[0])?.cmp(&s(caller, &params[1])?) {
            std::cmp::Ordering::Less => -1.0,
            std::cmp::Ordering::Equal => 0.0,
            std::cmp::Ordering::Greater => 1.0,
        }),
        "fromCharCode" => HostValue::Str(vec![params[0].unwrap_i32() as u16]),
        "substring" => {
            let st = s(caller, &params[0])?;
            let a = (params[1].unwrap_i32().max(0) as usize).min(st.len());
            let b = (params[2].unwrap_i32().max(0) as usize).min(st.len());
            HostValue::Str(st[a.min(b)..a.max(b)].to_vec())
        }
        "test" => HostValue::Bool(matches!(to_host(caller, &params[0]), Ok(HostValue::Str(_)))),
        "fromCharCodeArray" => {
            let (start, end) = (params[1].unwrap_i32(), params[2].unwrap_i32());
            let mut out = Vec::new();
            for i in start..end {
                out.push(i16_array_call(caller, "$wasmI16ArrayGet", &[params[0].clone(), Val::I32(i)], true)? as u16);
            }
            HostValue::Str(out)
        }
        "intoCharCodeArray" => {
            let st = s(caller, &params[0])?;
            let start = params[2].unwrap_i32();
            for (i, unit) in st.iter().enumerate() {
                let args = [params[1].clone(), Val::I32(start + i as i32), Val::I32(*unit as i32)];
                i16_array_call(caller, "$wasmI16ArraySet", &args, false)?;
            }
            HostValue::Num(st.len() as f64)
        }
        other => bail!("unsupported wasm:js-string builtin {other}"),
    };
    set_result(caller, results, ty, value)
}

/// Convert arguments for the host.
fn host_args(caller: &mut Caller<'_, State>, params: &[Val]) -> Result<Vec<HostValue>> {
    params.iter().map(|p| to_host(caller, p)).collect()
}

fn add_callback(caller: &mut Caller<'_, State>, callback: Callback) -> usize {
    caller.data_mut().callbacks.push(Some(callback));
    caller.data().callbacks.len() - 1
}

fn call_helper(h: &Helper, caller: &mut Caller<'_, State>, params: &[Val], results: &mut [Val], ty: &FuncType) -> Result<()> {
    let value = match h {
        Helper::Global(name) => caller.data_mut().host.global(name)?,
        Helper::Get(prop) => match (to_host(caller, &params[0])?, prop.as_str()) {
            // Strings are values, not host objects.
            (HostValue::Str(s), "length") => HostValue::Num(s.len() as f64),
            (target, _) => caller.data_mut().host.get(&target, prop)?,
        },
        Helper::Index => {
            let (target, index) = (to_host(caller, &params[0])?, to_host(caller, &params[1])?);
            match target {
                HostValue::Str(s) => match index.as_num().and_then(|i| s.get(i as usize)) {
                    Some(unit) => HostValue::Str(vec![*unit]),
                    None => HostValue::Undefined,
                },
                other => caller.data_mut().host.get(&other, &index.display())?,
            }
        }
        Helper::Repeat => {
            let s = to_host(caller, &params[0])?.display();
            let times = to_host(caller, &params[1])?.as_num().unwrap_or(0.0).max(0.0) as usize;
            HostValue::str(&s.repeat(times))
        }
        Helper::DateNow => HostValue::Num(caller.data_mut().host.now()),
        Helper::SetTimer { repeat } => {
            let ms = to_host(caller, &params[0])?;
            let task = to_host(caller, &params[1])?;
            let id = add_callback(caller, Callback::Task(task));
            let host = &mut caller.data_mut().host;
            let window = host.global("window")?;
            let method = if *repeat { "setInterval" } else { "setTimeout" };
            host.call(&window, method, &[HostValue::CallbackId(id), ms])?
        }
        Helper::ClearTimer => {
            let handle = to_host(caller, &params[0])?;
            let host = &mut caller.data_mut().host;
            let window = host.global("window")?;
            host.call(&window, "clearTimeout", &[handle])?
        }
        Helper::IsDartFunction => HostValue::Bool(matches!(to_host(caller, &params[0])?, HostValue::CallbackId(_))),
        Helper::DartFunction => match to_host(caller, &params[0])? {
            HostValue::CallbackId(id) => match caller.data().callbacks.get(id).cloned().flatten() {
                Some(Callback::Function(_, closure)) => HostValue::Opaque(closure),
                _ => bail!("callback {id} is not a Dart function"),
            },
            other => bail!("{} is not a Dart function", other.display()),
        },
        Helper::ClassifyBool => HostValue::Num(match to_host(caller, &params[0])? {
            HostValue::Undefined | HostValue::Null => 0.0,
            HostValue::Bool(_) => 1.0,
            _ => 2.0,
        }),
        Helper::ClassifyString => HostValue::Num(match to_host(caller, &params[0])? {
            HostValue::Undefined | HostValue::Null => 0.0,
            HostValue::Str(_) => 1.0,
            _ => 2.0,
        }),
        Helper::TypeCode => HostValue::Num(match to_host(caller, &params[0])? {
            HostValue::Undefined => 1.0,
            HostValue::Bool(_) => 2.0,
            HostValue::Num(_) => 3.0,
            HostValue::Str(_) => 4.0,
            _ => 19.0,
        }),
        Helper::Set(prop) => {
            let args = host_args(caller, params)?;
            caller.data_mut().host.set(&args[0], prop, &args[1])?;
            HostValue::Undefined
        }
        Helper::Method(method) => {
            let args = host_args(caller, params)?;
            caller.data_mut().host.call(&args[0], method, &args[1..])?
        }
        Helper::SetGlobal(name) => match (name.as_str(), to_host(caller, &params[0])?) {
            ("__dartPackDescribeError", HostValue::CallbackId(id)) => {
                caller.data_mut().describe_error = Some(id);
                HostValue::Undefined
            }
            (other, _) => bail!("setting globalThis.{other} is not supported yet"),
        },
        Helper::Truthy => HostValue::Bool(to_host(caller, &params[0])?.truthy()),
        Helper::StrictEquals => {
            let (l, r) = (to_host(caller, &params[0])?, to_host(caller, &params[1])?);
            HostValue::Bool(l.strict_equals(&r))
        }
        Helper::LastIndexOf => {
            let hay: Vec<u16> = to_host(caller, &params[0])?.display().encode_utf16().collect();
            let needle: Vec<u16> = to_host(caller, &params[1])?.display().encode_utf16().collect();
            let from = to_host(caller, &params[2])?.as_num().unwrap_or(f64::INFINITY);
            let last_start = hay.len().saturating_sub(needle.len()).min(from.max(0.0) as usize);
            let found = (0..=last_start).rev().find(|&i| hay[i..].starts_with(&needle));
            HostValue::Num(found.map(|i| i as f64).unwrap_or(-1.0))
        }
        Helper::QueueMicrotask => {
            let task = to_host(caller, &params[0])?;
            caller.data_mut().microtasks.push_back(task);
            HostValue::Undefined
        }
        Helper::Print => {
            let text = to_host(caller, &params[0])?.display();
            caller.data_mut().host.console(&text);
            HostValue::Undefined
        }
        Helper::NumToString => {
            let n = to_host(caller, &params[0])?.as_num().context("toString on a non-number")?;
            match params.get(1).map(|r| to_host(caller, r)).transpose()? {
                Some(HostValue::Num(r)) if r != 10.0 => HostValue::str(&to_radix(n as i64, r as u32)),
                _ => HostValue::str(&format_number(n)),
            }
        }
        Helper::IndexOf => {
            let hay = to_host(caller, &params[0])?.display();
            let needle = to_host(caller, &params[1])?.display();
            HostValue::Num(hay.find(&needle).map(|i| hay[..i].encode_utf16().count() as f64).unwrap_or(-1.0))
        }
        Helper::Stack => {
            // Shaped like a JS engine's: an "Error" line and the helper's own frames, which
            // Dart's stack trimming (`TrimStack`) expects to drop.
            let backtrace = WasmBacktrace::capture(&*caller);
            let frames = format_stack(caller.data().source_map.as_ref(), &backtrace);
            HostValue::str(&format!("Error\n    at <host stack helper>\n    at <host import>\n{frames}"))
        }
        Helper::TrimStack => HostValue::str(&trim_js_stack(&to_host(caller, &params[0])?.display())),
        Helper::ExnStack => HostValue::Null,
        Helper::IsUndefined => HostValue::Bool(matches!(to_host(caller, &params[0])?, HostValue::Undefined)),
        Helper::ToStr => HostValue::str(&to_host(caller, &params[0])?.display()),
        Helper::JsonStringify => HostValue::str(&format!("{:?}", to_host(caller, &params[0])?.display())),
        Helper::Identity => to_host(caller, &params[0])?,
        Helper::ClassifyNumber => HostValue::Num(match to_host(caller, &params[0])? {
            HostValue::Undefined | HostValue::Null => 0.0,
            HostValue::Num(_) => 1.0,
            _ => 2.0,
        }),
        Helper::WrapCallback => {
            let func = match &params[0] {
                Val::FuncRef(Some(f)) => *f,
                other => bail!("callback trampoline is {other:?}"),
            };
            let closure = match to_host(caller, &params[1])? {
                HostValue::Opaque(r) => r,
                other => bail!("callback closure is {other:?}"),
            };
            HostValue::CallbackId(add_callback(caller, Callback::Function(func, closure)))
        }
        Helper::Unsupported(body) => bail!("unsupported dart2wasm import: {body}"),
    };
    set_result(caller, results, ty, value)
}

fn to_radix(mut n: i64, radix: u32) -> String {
    if n == 0 {
        return "0".into();
    }
    let neg = n < 0;
    let mut digits = Vec::new();
    while n != 0 {
        digits.push(std::char::from_digit((n % radix as i64).unsigned_abs() as u32, radix).unwrap());
        n /= radix as i64;
    }
    if neg {
        digits.push('-');
    }
    digits.iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A site's compiled program is saved once and loaded after; another site gets its own
    /// entry, never the first one's; old entries are pruned.
    #[test]
    fn compiled_programs_are_cached_per_site_and_pruned() {
        let wasm = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/throws.wasm")).unwrap();
        let profile = std::env::temp_dir().join(format!("dart-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&profile);
        let dir = profile.join("compiled-dart");
        let files = || {
            let mut f: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| (e.file_name(), e.metadata().unwrap().modified().unwrap())).collect();
            f.sort();
            f
        };
        compiled(&wasm, "https://a.test", Some(&profile)).unwrap();
        let first = files();
        assert_eq!(first.len(), 1);
        let again = compiled(&wasm, "https://a.test", Some(&profile)).unwrap();
        assert_eq!(files(), first, "loaded, not compiled and written again");
        assert!(again.imports().len() > 0, "a usable module");
        // Another site's key differs, so it can never hit this entry.
        assert_ne!(crate::page_document::code_key(&wasm, "https://a.test"), crate::page_document::code_key(&wasm, "https://b.test"));
        std::fs::write(dir.join("another.cwasm"), "x").unwrap();
        prune_compiled(&dir, 1);
        assert_eq!(files().len(), 1);
        let _ = std::fs::remove_dir_all(&profile);
    }

    #[test]
    fn wasm_without_embedded_table_is_rejected_with_a_hint() {
        // A valid, empty wasm module: magic + version, no sections.
        let empty = b"\0asm\x01\0\0\0";
        let err = embedded_import_table(empty).unwrap_err().to_string();
        assert!(err.contains("packs/dart/build.sh"), "{err}");
    }

    #[test]
    fn recognises_package_web_shapes() {
        assert!(matches!(classify("() => globalThis.document,"), Helper::Global(n) if n == "document"));
        assert!(matches!(classify("(x0,x1) => x0.querySelector(x1),"), Helper::Method(n) if n == "querySelector"));
        assert!(matches!(classify("x0 => x0.random(),"), Helper::Method(n) if n == "random"));
        assert!(matches!(classify("(x0,x1,x2,x3) => x0.addEventListener(x1,x2,x3),"), Helper::Method(n) if n == "addEventListener"));
        assert!(matches!(classify("x0 => x0.textContent,"), Helper::Get(n) if n == "textContent"));
        assert!(matches!(classify("(x0,x1) => { x0.textContent = x1 },"), Helper::Set(n) if n == "textContent"));
        assert!(matches!(classify("b => !!b,"), Helper::Truthy));
        assert!(matches!(classify("o => o"), Helper::Identity));
        assert!(matches!(classify("x0 => eval(x0)"), Helper::Unsupported(_)));
    }

    #[test]
    fn js_semantics_for_truthiness_and_equality() {
        assert!(!HostValue::Null.truthy() && !HostValue::Undefined.truthy());
        assert!(!HostValue::Num(0.0).truthy() && !HostValue::str("").truthy());
        assert!(HostValue::Object(1).truthy() && HostValue::str("a").truthy());
        assert!(HostValue::Object(2).strict_equals(&HostValue::Object(2)));
        assert!(!HostValue::Null.strict_equals(&HostValue::Undefined));
    }
}
