//! The Dart language pack: runs dart2wasm output in wasmtime, with no JS engine.
//!
//! Dart's wasm treats strings and other values as opaque host references (externref) and
//! calls small JS snippets for helpers. We provide those in Rust: values are `HostValue`s
//! inside externrefs, and each dart2wasm import is recognised by its snippet in the import
//! table (read as data, never executed). Calls to `globalThis.host.<name>(...)` go to the
//! `Host`; unknown snippets trap with their text.
//!
//! The wasm must be built with `packs/dart/build.sh`, which converts dart2wasm's legacy
//! exception instructions into the standard ones wasmtime implements, and embeds the import
//! table in a `dart2wasm-imports` custom section so no `.mjs` is shipped.

use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use wasmtime::*;

/// A JS-like value as seen by the host. Lives inside a wasm externref.
#[derive(Clone, Debug)]
pub enum HostValue {
    Str(Vec<u16>),
    Num(f64),
    Bool(bool),
    Undefined,
    /// A Dart closure wrapped so the host can call it later: (trampoline, closure).
    /// The host never sees this: it is swapped for a callback id before `Host::call`.
    Callback(Func, OwnedRooted<AnyRef>),
    /// A Dart object passed out by reference. The host can hold it and hand it back,
    /// but not look inside, just as a JS engine treats it.
    Opaque(OwnedRooted<AnyRef>),
}

impl HostValue {
    pub fn str(s: &str) -> Self {
        HostValue::Str(s.encode_utf16().collect())
    }

    pub fn display(&self) -> String {
        match self {
            HostValue::Str(s) => String::from_utf16_lossy(s),
            HostValue::Num(n) => format_number(*n),
            HostValue::Bool(b) => b.to_string(),
            HostValue::Undefined => "undefined".into(),
            HostValue::Callback(..) => "function".into(),
            HostValue::Opaque(_) => "[object]".into(),
        }
    }

    pub fn as_num(&self) -> Option<f64> {
        match self {
            HostValue::Num(n) => Some(*n),
            _ => None,
        }
    }
}

fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e21 { format!("{}", n as i64) } else { format!("{n}") }
}

/// What the pack can reach outside its sandbox. Every call goes through here.
pub trait Host {
    /// A `host.<name>(...)` call. Callback arguments arrive as their callback id.
    fn call(&mut self, name: &str, args: &[HostValue]) -> Result<HostValue>;
    /// Dart's `print`.
    fn console(&mut self, text: &str);
}

/// A loaded Dart program.
pub struct DartRuntime {
    store: Store<State>,
    instance: Instance,
}

struct State {
    host: Box<dyn Host>,
    /// Callbacks the page registered, by id, so the host can fire them later.
    callbacks: Vec<HostValue>,
}

fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut config = Config::new();
        config.wasm_gc(true).wasm_function_references(true).wasm_exceptions(true);
        Engine::new(&config).expect("wasmtime engine with GC and exceptions")
    })
}

impl DartRuntime {
    /// Compile and instantiate a Dart program. Does not run `main`.
    pub fn load(wasm: &[u8], host: Box<dyn Host>) -> Result<Self> {
        let table = read_import_table(&embedded_import_table(wasm)?)?;
        let module = Module::new(engine(), wasm)?;
        let mut store = Store::new(engine(), State { host, callbacks: Vec::new() });

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
                                .with_context(|| format!("in dart2wasm.{name} ({h:?})")),
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
        Ok(Self { store, instance })
    }

    pub fn run_main(&mut self) -> Result<()> {
        let main = self.instance.get_func(&mut self.store, "$invokeMain").context("no $invokeMain export")?;
        let args = ExternRef::new(&mut self.store, HostValue::Undefined)?;
        main.call(&mut self.store, &[Val::ExternRef(Some(args))], &mut [])?;
        Ok(())
    }

    /// Invoke a callback the page registered, e.g. on a click.
    pub fn fire_callback(&mut self, id: usize) -> Result<()> {
        let Some(HostValue::Callback(func, closure)) = self.store.data().callbacks.get(id).cloned() else {
            bail!("no callback {id}");
        };
        // The trampoline is `wasmFunction(f, arguments.length)`: closure plus argument count,
        // converted to whatever types the trampoline declares.
        let ty = func.ty(&self.store);
        let values = [HostValue::Opaque(closure), HostValue::Num(0.0)];
        let mut args = Vec::new();
        for (value, pty) in values.into_iter().zip(ty.params()) {
            args.push(from_host(&mut self.store, value, &pty)?);
        }
        let mut out: Vec<Val> = ty.results().map(|_| Val::null_extern_ref()).collect();
        func.call(&mut self.store, &args, &mut out)?;
        Ok(())
    }
}

/// What a dart2wasm import does, recognised from its JS snippet.
#[derive(Clone, Debug)]
enum Helper {
    Host(String),
    Print,
    NumToString,
    IndexOf,
    Stack,
    IsUndefined,
    ToStr,
    JsonStringify,
    Identity,
    WrapCallback,
    ClassifyNumber,
    Unsupported(String),
}

fn classify(body: &str) -> Helper {
    static HOST_CALL: OnceLock<Regex> = OnceLock::new();
    let host_call = HOST_CALL.get_or_init(|| Regex::new(r"=> globalThis\.host\.(\w+)\(").unwrap());
    let b = body.trim().trim_end_matches(',');
    if let Some(c) = host_call.captures(b) {
        return Helper::Host(c[1].to_string());
    }
    match b {
        "s => printToConsole(s)" => Helper::Print,
        "Function.prototype.call.bind(Number.prototype.toString)"
        | "Function.prototype.call.bind(BigInt.prototype.toString)" => Helper::NumToString,
        "Function.prototype.call.bind(String.prototype.indexOf)" => Helper::IndexOf,
        "() => new Error().stack" => Helper::Stack,
        "o => o === undefined" => Helper::IsUndefined,
        "o => String(o)" => Helper::ToStr,
        "s => JSON.stringify(s)" => Helper::JsonStringify,
        "o => o" => Helper::Identity,
        _ if b.starts_with("(exn) =>") => Helper::Stack,
        _ if b.starts_with("(wasmFunction,f) => finalizeWrapper") => Helper::WrapCallback,
        _ if b.contains("typeof o === 'number'") => Helper::ClassifyNumber,
        _ => Helper::Unsupported(b.to_string()),
    }
}

/// The import table `packs/dart/embed_imports.dart` stored in the wasm's custom section.
fn embedded_import_table(wasm: &[u8]) -> Result<String> {
    const SECTION: &[u8] = b"dart2wasm-imports";
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
            if wasm.get(j..j + name_len) == Some(SECTION) {
                let payload = wasm.get(j + name_len..end).context("truncated custom section")?;
                return Ok(String::from_utf8_lossy(payload).into_owned());
            }
        }
        i = end;
    }
    bail!("no dart2wasm-imports section: build this page's Dart with packs/dart/build.sh")
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
        Val::ExternRef(None) => HostValue::Undefined,
        Val::ExternRef(Some(r)) => match r.data(&mut *store)? {
            Some(data) => data.downcast_ref::<HostValue>().context("externref is not a HostValue")?.clone(),
            // No host data: a Dart value converted with extern.convert_any.
            None => {
                let any = AnyRef::convert_extern(&mut *store, *r)?;
                any_to_host(store, &any)?
            }
        },
        Val::AnyRef(None) => HostValue::Undefined,
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
            HostValue::Opaque(a) => {
                let a = a.to_rooted(&mut *store);
                Val::ExternRef(Some(ExternRef::convert_any(&mut *store, a)?))
            }
            other => Val::ExternRef(Some(ExternRef::new(store, other)?)),
        },
        ValType::Ref(_) => match value {
            HostValue::Opaque(a) => Val::AnyRef(Some(a.to_rooted(&mut *store))),
            HostValue::Undefined => Val::AnyRef(None),
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

fn call_helper(h: &Helper, caller: &mut Caller<'_, State>, params: &[Val], results: &mut [Val], ty: &FuncType) -> Result<()> {
    let value = match h {
        Helper::Host(name) => {
            let mut args = Vec::new();
            for p in params {
                // Callbacks are kept here and handed to the host as ids.
                args.push(match to_host(caller, p)? {
                    cb @ HostValue::Callback(..) => {
                        caller.data_mut().callbacks.push(cb);
                        HostValue::Num((caller.data().callbacks.len() - 1) as f64)
                    }
                    other => other,
                });
            }
            caller.data_mut().host.call(name, &args)?
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
        Helper::Stack => HostValue::str(""),
        Helper::IsUndefined => HostValue::Bool(matches!(to_host(caller, &params[0])?, HostValue::Undefined)),
        Helper::ToStr => HostValue::str(&to_host(caller, &params[0])?.display()),
        Helper::JsonStringify => HostValue::str(&format!("{:?}", to_host(caller, &params[0])?.display())),
        Helper::Identity => to_host(caller, &params[0])?,
        Helper::ClassifyNumber => HostValue::Num(match to_host(caller, &params[0])? {
            HostValue::Undefined => 0.0,
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
            HostValue::Callback(func, closure)
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
    use std::sync::{Arc, Mutex};

    /// Records host calls and plays a tiny DOM: every query and create returns a new handle.
    struct FakeHost {
        calls: Arc<Mutex<Vec<String>>>,
        next_handle: f64,
    }

    impl Host for FakeHost {
        fn call(&mut self, name: &str, args: &[HostValue]) -> Result<HostValue> {
            let shown: Vec<String> = args.iter().map(|a| a.display()).collect();
            self.calls.lock().unwrap().push(format!("{name}({})", shown.join(", ")));
            Ok(match name {
                "domQuery" | "domCreate" => {
                    self.next_handle += 1.0;
                    HostValue::Num(self.next_handle)
                }
                _ => HostValue::Undefined,
            })
        }
        fn console(&mut self, text: &str) {
            self.calls.lock().unwrap().push(format!("console({text})"));
        }
    }

    /// Runs the real counter example (build it first with packs/dart/build.sh).
    #[test]
    fn counter_example_runs_and_handles_clicks() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/counter");
        let Ok(wasm) = std::fs::read(dir.join("main.wasm")) else {
            panic!("build the example first: packs/dart/build.sh examples/counter/main.dart");
        };
        let calls = Arc::new(Mutex::new(Vec::new()));
        let host = FakeHost { calls: calls.clone(), next_handle: 0.0 };
        let mut runtime = DartRuntime::load(&wasm, Box::new(host)).unwrap();

        runtime.run_main().unwrap();
        runtime.fire_callback(0).unwrap();
        runtime.fire_callback(0).unwrap();

        assert_eq!(
            *calls.lock().unwrap(),
            [
                "domQuery(#status)",
                "domSetText(1, Dart is running. No JavaScript on this page.)",
                "domQuery(#count)",
                "domQuery(#log)",
                "domQuery(#increment)",
                "domOnClick(4, 0)",
                "domSetText(2, 1)",
                "domCreate(li)",
                "domSetText(5, Click 1, handled in Dart)",
                "domAppend(3, 5)",
                "domSetText(2, 2)",
                "domCreate(li)",
                "domSetText(6, Click 2, handled in Dart)",
                "domAppend(3, 6)",
            ]
        );
    }

    #[test]
    fn wasm_without_embedded_table_is_rejected_with_a_hint() {
        // A valid, empty wasm module: magic + version, no sections.
        let empty = b"\0asm\x01\0\0\0";
        let err = embedded_import_table(empty).unwrap_err().to_string();
        assert!(err.contains("packs/dart/build.sh"), "{err}");
    }

    #[test]
    fn recognises_host_calls_and_traps_unknown_snippets() {
        assert!(matches!(classify("(x0,x1) => globalThis.host.domAppend(x0,x1),"), Helper::Host(n) if n == "domAppend"));
        assert!(matches!(classify("o => o"), Helper::Identity));
        assert!(matches!(classify("x0 => document.cookie"), Helper::Unsupported(_)));
    }
}
