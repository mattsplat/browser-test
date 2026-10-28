//! Spike: run dart2wasm output in wasmtime with no JS engine.
//!
//! usage: dart-host <app.wasm>   (expects <app.mjs> next to it, read only as an import table)
//!
//! Dart's wasm treats strings and other values as opaque host references (externref) and
//! calls small JS snippets for helpers. We provide those in Rust: values are `HostValue`s
//! inside externrefs, and each dart2wasm import is recognised by its snippet in the .mjs.
//! Calls to `globalThis.host.<name>(...)` go to the host API; unknown snippets trap.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use wasmtime::*;

/// A JS-like value as seen by the host. Lives inside a wasm externref.
#[derive(Clone, Debug)]
enum HostValue {
    Str(Vec<u16>),
    Num(f64),
    Bool(bool),
    Undefined,
    /// A Dart closure wrapped so the host can call it later: (trampoline, closure).
    Callback(Func, OwnedRooted<AnyRef>),
    /// A Dart object passed out by reference. The host can hold it and hand it back,
    /// but not look inside, just as a JS engine treats it.
    Opaque(OwnedRooted<AnyRef>),
}

impl HostValue {
    fn str(s: &str) -> Self {
        HostValue::Str(s.encode_utf16().collect())
    }

    fn display(&self) -> String {
        match self {
            HostValue::Str(s) => String::from_utf16_lossy(s),
            HostValue::Num(n) => format_number(*n),
            HostValue::Bool(b) => b.to_string(),
            HostValue::Undefined => "undefined".into(),
            HostValue::Callback(..) => "function".into(),
            HostValue::Opaque(_) => "[object]".into(),
        }
    }
}

fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e21 { format!("{}", n as i64) } else { format!("{n}") }
}

/// What a dart2wasm import does, recognised from its JS snippet.
#[derive(Clone, Debug)]
enum Helper {
    Host(String),
    Print,
    NumToString,
    IndexOf,
    Stack,
    ExnStack,
    IsUndefined,
    ToStr,
    JsonStringify,
    Identity,
    WrapCallback,
    ClassifyNumber,
    Unsupported(String),
}

fn classify(body: &str) -> Helper {
    let host_call = Regex::new(r"=> globalThis\.host\.(\w+)\(").unwrap();
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
        _ if b.starts_with("(exn) =>") => Helper::ExnStack,
        _ if b.starts_with("(wasmFunction,f) => finalizeWrapper") => Helper::WrapCallback,
        _ if b.contains("typeof o === 'number'") => Helper::ClassifyNumber,
        _ => Helper::Unsupported(b.to_string()),
    }
}

/// Read `const dart2wasm = { X: body, ... }` out of the generated loader. The .mjs is only
/// read as data here; it is never executed.
fn read_import_table(mjs: &str) -> Result<HashMap<String, Helper>> {
    let start = mjs.find("const dart2wasm = {").context("no dart2wasm import table in .mjs")?;
    let rest = &mjs[start..];
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
            _ => {
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

/// The host API that Dart code reaches through `@JS('host.<name>')`. The spike's version
/// is a fake DOM that just logs; the browser will back this with Blitz.
trait HostApi {
    fn call(&mut self, name: &str, args: &[HostValue]) -> Result<HostValue>;
}

struct State {
    api: Box<dyn HostApi + Send>,
    /// Callbacks the page registered, by id, so the host can fire them later.
    callbacks: Vec<HostValue>,
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

fn result(store: &mut impl AsContextMut, results: &mut [Val], ty: &FuncType, value: HostValue) -> Result<()> {
    if let (Some(slot), Some(rt)) = (results.first_mut(), ty.results().next()) {
        *slot = from_host(store, value, &rt)?;
    }
    Ok(())
}

fn i16_array_call(caller: &mut Caller<'_, State>, export: &str, args: &[Val], want_result: bool) -> Result<i32> {
    let f = caller.get_export(export).and_then(|e| e.into_func()).with_context(|| format!("missing export {export}"))?;
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
                i16_array_call(caller, "$wasmI16ArraySet", &[params[1].clone(), Val::I32(start + i as i32), Val::I32(*unit as i32)], false)?;
            }
            HostValue::Num(st.len() as f64)
        }
        other => bail!("unsupported wasm:js-string builtin {other}"),
    };
    result(caller, results, ty, value)
}

fn helper(h: &Helper, caller: &mut Caller<'_, State>, params: &[Val], results: &mut [Val], ty: &FuncType) -> Result<()> {
    if std::env::var_os("DART_HOST_TRACE").is_some() {
        let shown = params.iter().map(|p| to_host(caller, p).map(|v| format!("{v:?}")).unwrap_or_else(|e| e.to_string())).collect::<Vec<_>>();
        eprintln!("[trace] {h:?} {ty} <- {shown:?}");
    }
    let value = match h {
        Helper::Host(name) => {
            let args = params.iter().map(|p| to_host(caller, p)).collect::<Result<Vec<_>>>()?;
            // Callbacks are kept by the host and handed to the API as ids.
            let args = args
                .into_iter()
                .map(|a| match a {
                    HostValue::Callback(..) => {
                        caller.data_mut().callbacks.push(a);
                        HostValue::Num((caller.data().callbacks.len() - 1) as f64)
                    }
                    other => other,
                })
                .collect::<Vec<_>>();
            caller.data_mut().api.call(name, &args)?
        }
        Helper::Print => {
            println!("[dart] {}", to_host(caller, &params[0])?.display());
            HostValue::Undefined
        }
        Helper::NumToString => {
            let n = match to_host(caller, &params[0])? { HostValue::Num(n) => n, v => bail!("toString on {v:?}") };
            let radix = params.get(1).map(|r| to_host(caller, r)).transpose()?;
            match radix {
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
        Helper::ExnStack => HostValue::str(""),
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
            let func = match &params[0] { Val::FuncRef(Some(f)) => *f, other => bail!("callback trampoline is {other:?}") };
            let closure = match to_host(caller, &params[1])? {
                HostValue::Opaque(r) => r,
                other => bail!("callback closure is {other:?}"),
            };
            HostValue::Callback(func, closure)
        }
        Helper::Unsupported(body) => bail!("unsupported dart2wasm import: {body}"),
    };
    result(caller, results, ty, value)
}

fn to_radix(mut n: i64, radix: u32) -> String {
    if n == 0 { return "0".into(); }
    let neg = n < 0;
    let mut digits = Vec::new();
    while n != 0 {
        digits.push(std::char::from_digit((n % radix as i64).unsigned_abs() as u32, radix).unwrap());
        n /= radix as i64;
    }
    if neg { digits.push('-'); }
    digits.iter().rev().collect()
}

/// Invoke a callback the page registered, as the browser would on an event.
fn fire_callback(store: &mut Store<State>, id: usize) -> Result<()> {
    let HostValue::Callback(func, closure) = store.data().callbacks[id].clone() else { bail!("not a callback") };
    // The trampoline is `wasmFunction(f, arguments.length)`: closure plus argument count,
    // converted to whatever types the trampoline declares.
    let ty = func.ty(&*store);
    let values = [HostValue::Opaque(closure), HostValue::Num(0.0)];
    let mut args = Vec::new();
    for (value, pty) in values.into_iter().zip(ty.params()) {
        args.push(from_host(&mut *store, value, &pty)?);
    }
    let mut out: Vec<Val> = ty.results().map(|_| Val::null_extern_ref()).collect();
    func.call(&mut *store, &args, &mut out)?;
    Ok(())
}

/// The spike's fake DOM: logs calls and returns made-up node ids.
struct FakeDom;
impl HostApi for FakeDom {
    fn call(&mut self, name: &str, args: &[HostValue]) -> Result<HostValue> {
        let shown: Vec<String> = args.iter().map(|a| a.display()).collect();
        println!("[host] {name}({})", shown.join(", "));
        Ok(match name {
            "domQuery" => HostValue::Num(7.0),
            _ => HostValue::Undefined,
        })
    }
}

fn main() -> Result<()> {
    let wasm_path = std::env::args().nth(1).context("usage: dart-host <app.wasm>")?;
    let mjs = std::fs::read_to_string(wasm_path.replace(".wasm", ".mjs"))?;
    let table = read_import_table(&mjs)?;

    let mut config = Config::new();
    config.wasm_gc(true).wasm_function_references(true).wasm_exceptions(true);
    let engine = Engine::new(&config)?;
    let module = Module::from_file(&engine, &wasm_path)?;
    let mut store = Store::new(&engine, State { api: Box::new(FakeDom), callbacks: Vec::new() });

    let mut imports: Vec<Extern> = Vec::new();
    for import in module.imports() {
        let (m, name) = (import.module(), import.name());
        let ext: Extern = match import.ty() {
            ExternType::Func(ty) => {
                let (m, name) = (m.to_string(), name.to_string());
                let h = if m == "dart2wasm" {
                    Some(table.get(&name).cloned().ok_or_else(|| anyhow!("import {name} not in .mjs"))?)
                } else {
                    None
                };
                let fty = ty.clone();
                Func::new(&mut store, ty, move |mut caller, params, results| {
                    let outcome = match (m.as_str(), &h) {
                        ("dart2wasm", Some(h)) => helper(h, &mut caller, params, results, &fty)
                            .with_context(|| format!("in dart2wasm.{name} ({h:?}) with {params:?}")),
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
            // JS exceptions tag: our own tag with the same type.
            ExternType::Tag(ty) => Tag::new(&mut store, &ty)?.into(),
            other => bail!("unsupported import {m}.{name}: {other:?}"),
        };
        imports.push(ext);
    }

    let instance = Instance::new(&mut store, &module, &imports)?;
    let main = instance.get_func(&mut store, "$invokeMain").context("no $invokeMain export")?;
    let args = ExternRef::new(&mut store, HostValue::Undefined)?;
    main.call(&mut store, &[Val::ExternRef(Some(args))], &mut [])?;

    // Simulate the browser delivering two clicks to whatever the page registered.
    for _ in 0..2 {
        for id in 0..store.data().callbacks.len() {
            println!("[host] firing callback {id}");
            fire_callback(&mut store, id)?;
        }
    }
    Ok(())
}
