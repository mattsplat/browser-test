//! Page code in Rust for the Unscripted browser.
//!
//! The browser's objects (`document`, elements, events, responses...) are [`Object`]s: handles
//! the browser holds, read and changed through its host with the web platform's own names, as
//! the JS, Python and PHP packs do. A page is a `cdylib` crate built for `wasm32-wasip1`
//! (`packs/rust/build.sh`) that names its entry point with [`main!`]:
//!
//! ```ignore
//! use unscripted::{document, Object};
//!
//! fn start() {
//!     let count = document().query("#count").unwrap();
//!     document().query("#add").unwrap().on("click", move |_event: Object| {
//!         count.set_text("clicked");
//!     });
//! }
//!
//! unscripted::main!(start);
//! ```
//!
//! Everything goes through one import, `host.request`, carrying JSON (see "Language packs" in
//! docs/packs.md). `println!` and panics go to the page's console; time and randomness come
//! from the host, so a recording replays them.

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use serde_json::{Map, Value as Json, json};

#[link(wasm_import_module = "host")]
unsafe extern "C" {
    #[link_name = "request"]
    fn host_request(ptr: *const u8, len: u32) -> u64;
}

/// A value crossing between the page and the browser.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    /// A web object the browser holds.
    Object(Object),
    /// A function the browser can call later (from [`callback`]).
    Callback(u32),
    /// Plain data: a `fetch` init, a `history` state.
    Data(Json),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&Object> {
        match self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }

    /// `null` or `undefined`.
    pub fn is_nullish(&self) -> bool {
        matches!(self, Value::Null | Value::Undefined)
    }

    /// The value as text, as JS's `String(value)` would show it.
    pub fn to_text(&self) -> String {
        match self {
            Value::Undefined => "undefined".into(),
            Value::Null => "null".into(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) if n.fract() == 0.0 && n.abs() < 1e15 => format!("{}", *n as i64),
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            Value::Object(o) => format!("[object #{}]", o.handle()),
            Value::Callback(id) => format!("[function {id}]"),
            Value::Data(d) => d.to_string(),
        }
    }

    fn encode(&self) -> Json {
        match self {
            Value::Undefined => json!({ "u": 1 }),
            Value::Null => Json::Null,
            Value::Bool(b) => json!(b),
            Value::Number(n) => json!(n),
            Value::String(s) => json!(s),
            Value::Object(o) => json!({ "o": o.handle() }),
            Value::Callback(id) => json!({ "cb": id }),
            Value::Data(d) => encode_data(d),
        }
    }

    fn decode(v: Json) -> Value {
        match v {
            Json::Null => Value::Null,
            Json::Bool(b) => Value::Bool(b),
            Json::Number(n) => Value::Number(n.as_f64().unwrap_or(f64::NAN)),
            Json::String(s) => Value::String(s),
            Json::Object(fields) if fields.contains_key("u") => Value::Undefined,
            Json::Object(fields) if fields.contains_key("o") => {
                let id = fields["o"].as_u64().unwrap_or(0) as u32;
                Value::Object(Object(Rc::new(Handle { id, releasable: fields.contains_key("r") })))
            }
            other => Value::Data(other),
        }
    }
}

/// Plain data as the host takes it: arrays as `{arr: [...]}`, objects as `{obj: {...}}`.
fn encode_data(d: &Json) -> Json {
    match d {
        Json::Array(items) => json!({ "arr": items.iter().map(encode_data).collect::<Vec<_>>() }),
        Json::Object(fields) => json!({ "obj": fields.iter().map(|(k, v)| (k.clone(), encode_data(v))).collect::<Map<_, _>>() }),
        scalar => scalar.clone(),
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::String(s.to_string())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::String(s)
    }
}

impl From<f64> for Value {
    fn from(n: f64) -> Self {
        Value::Number(n)
    }
}

impl From<i32> for Value {
    fn from(n: i32) -> Self {
        Value::Number(n as f64)
    }
}

impl From<u32> for Value {
    fn from(n: u32) -> Self {
        Value::Number(n as f64)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<Object> for Value {
    fn from(o: Object) -> Self {
        Value::Object(o)
    }
}

impl From<&Object> for Value {
    fn from(o: &Object) -> Self {
        Value::Object(o.clone())
    }
}

/// What the browser refused: an unsupported member, a bad argument, a page error.
#[derive(Clone, Debug, PartialEq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn request(message: Json) -> Result<Value> {
    let text = message.to_string();
    let packed = unsafe { host_request(text.as_ptr(), text.len() as u32) };
    let (ptr, len) = ((packed >> 32) as usize, packed as u32 as usize);
    // The reply was allocated with `rust_alloc`; it's ours to free.
    let reply = unsafe { Vec::from_raw_parts(ptr as *mut u8, len, len.max(1)) };
    let mut reply: Json = serde_json::from_slice(&reply).map_err(|e| Error(format!("bad reply from the browser: {e}")))?;
    if let Some(err) = reply.get("err") {
        return Err(Error(err.as_str().unwrap_or("error").to_string()));
    }
    Ok(Value::decode(reply.get_mut("ok").map(Json::take).unwrap_or(Json::Null)))
}

struct Handle {
    id: u32,
    /// Made for the page (a response, parsed data): the browser frees it when the last
    /// [`Object`] for it is dropped.
    releasable: bool,
}

impl Drop for Handle {
    fn drop(&mut self) {
        if self.releasable {
            let _ = request(json!({ "op": "release", "target": self.id }));
        }
    }
}

/// A web object the browser holds: `window`, `document`, an element, an event, a response.
/// Clones share one handle.
#[derive(Clone)]
pub struct Object(Rc<Handle>);

impl PartialEq for Object {
    fn eq(&self, other: &Self) -> bool {
        self.handle() == other.handle()
    }
}

impl fmt::Debug for Object {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Object(#{})", self.handle())
    }
}

impl Object {
    fn handle(&self) -> u32 {
        self.0.id
    }

    /// `object.name`.
    pub fn get(&self, name: &str) -> Result<Value> {
        request(json!({ "op": "get", "target": self.handle(), "name": name }))
    }

    /// `object.name = value`.
    pub fn set(&self, name: &str, value: impl Into<Value>) -> Result<()> {
        request(json!({ "op": "set", "target": self.handle(), "name": name, "value": value.into().encode() })).map(drop)
    }

    /// `object.name(args...)`.
    pub fn call(&self, name: &str, args: &[Value]) -> Result<Value> {
        let args: Vec<Json> = args.iter().map(Value::encode).collect();
        request(json!({ "op": "call", "target": self.handle(), "name": name, "args": args }))
    }

    /// `object.name` as an object, if it is one.
    pub fn object(&self, name: &str) -> Option<Object> {
        self.get(name).ok()?.as_object().cloned()
    }

    /// `object.name` as text (empty if it isn't a string).
    pub fn text_of(&self, name: &str) -> String {
        self.get(name).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
    }

    /// `querySelector`: the first match, or None.
    pub fn query(&self, selector: &str) -> Option<Object> {
        self.call("querySelector", &[selector.into()]).ok()?.as_object().cloned()
    }

    /// `querySelectorAll`, as a Vec.
    pub fn query_all(&self, selector: &str) -> Vec<Object> {
        self.call("querySelectorAll", &[selector.into()]).ok().and_then(|v| v.as_object().cloned()).map(|list| list.items()).unwrap_or_default()
    }

    /// A list's items (a NodeList, an array of objects), by `length` and index.
    pub fn items(&self) -> Vec<Object> {
        let length = self.get("length").ok().and_then(|v| v.as_f64()).unwrap_or(0.0) as usize;
        (0..length).filter_map(|i| self.object(&i.to_string())).collect()
    }

    /// `textContent`.
    pub fn text(&self) -> String {
        self.text_of("textContent")
    }

    /// `textContent = text`.
    pub fn set_text(&self, text: &str) {
        let _ = self.set("textContent", text);
    }

    /// `appendChild`.
    pub fn append(&self, child: &Object) {
        let _ = self.call("appendChild", &[child.into()]);
    }

    /// `addEventListener(event, f)`; `f` gets the event.
    pub fn on(&self, event: &str, mut f: impl FnMut(Object) + 'static) {
        let listener = callback(move |args| {
            if let Some(event) = args.into_iter().next().and_then(|a| a.as_object().cloned()) {
                f(event);
            }
        });
        let _ = self.call("addEventListener", &[event.into(), listener]);
    }

    /// A promise's `then`: one of `ok` and `err` is called later, from the event loop.
    pub fn then(&self, ok: impl FnOnce(Value) + 'static, err: impl FnOnce(String) + 'static) {
        let (mut ok, mut err) = (Some(ok), Some(err));
        let on_value = callback(move |args| {
            if let Some(ok) = ok.take() {
                ok(args.into_iter().next().unwrap_or(Value::Undefined));
            }
        });
        let on_error = callback(move |args| {
            if let Some(err) = err.take() {
                err(args.first().map(Value::to_text).unwrap_or_default());
            }
        });
        let _ = self.call("then", &[on_value, on_error]);
    }
}

fn global(name: &str) -> Object {
    thread_local! {
        static GLOBALS: RefCell<Vec<(String, Object)>> = const { RefCell::new(Vec::new()) };
    }
    if let Some(found) = GLOBALS.with(|g| g.borrow().iter().find(|(n, _)| n == name).map(|(_, o)| o.clone())) {
        return found;
    }
    let object = request(json!({ "op": "global", "name": name })).ok().and_then(|v| v.as_object().cloned()).unwrap_or_else(|| panic!("the browser has no {name}"));
    GLOBALS.with(|g| g.borrow_mut().push((name.to_string(), object.clone())));
    object
}

pub fn window() -> Object {
    global("window")
}

pub fn document() -> Object {
    global("document")
}

pub fn console() -> Object {
    global("console")
}

/// `console.log(text)`.
pub fn log(text: impl fmt::Display) {
    let _ = console().call("log", &[text.to_string().into()]);
}

/// `document.createElement(tag)`.
pub fn create(tag: &str) -> Object {
    document().call("createElement", &[tag.into()]).ok().and_then(|v| v.as_object().cloned()).unwrap_or_else(|| panic!("couldn't create <{tag}>"))
}

/// `Math.random()`: from the browser, so a recording replays the same numbers.
pub fn random() -> f64 {
    global("Math").call("random", &[]).ok().and_then(|v| v.as_f64()).unwrap_or(0.0)
}

/// `performance.now()`: milliseconds since the page started.
pub fn now() -> f64 {
    window().object("performance").and_then(|p| p.call("now", &[]).ok()).and_then(|v| v.as_f64()).unwrap_or(0.0)
}

pub fn set_timeout(f: impl FnOnce() + 'static, ms: u32) -> i32 {
    let mut f = Some(f);
    let id = window().call("setTimeout", &[callback(move |_| f.take().map_or((), |f| f())), ms.into()]);
    id.ok().and_then(|v| v.as_f64()).unwrap_or(0.0) as i32
}

pub fn set_interval(mut f: impl FnMut() + 'static, ms: u32) -> i32 {
    let id = window().call("setInterval", &[callback(move |_| f()), ms.into()]);
    id.ok().and_then(|v| v.as_f64()).unwrap_or(0.0) as i32
}

pub fn clear_timeout(id: i32) {
    let _ = window().call("clearTimeout", &[id.into()]);
}

pub fn clear_interval(id: i32) {
    let _ = window().call("clearInterval", &[id.into()]);
}

/// `requestAnimationFrame(f)`; `f` gets the frame's timestamp.
pub fn request_animation_frame(f: impl FnOnce(f64) + 'static) -> i32 {
    let mut f = Some(f);
    let frame = callback(move |args| {
        if let Some(f) = f.take() {
            f(args.first().and_then(Value::as_f64).unwrap_or(0.0));
        }
    });
    window().call("requestAnimationFrame", &[frame]).ok().and_then(|v| v.as_f64()).unwrap_or(0.0) as i32
}

/// `fetch(url, init)`, through the page's gate: a promise of a Response (see
/// [`Object::then`]). `init` is plain data, e.g. `json!({"method": "POST", "body": "x"})`.
pub fn fetch(url: &str, init: Option<Json>) -> Object {
    let init = init.map_or(Value::Undefined, Value::Data);
    window().call("fetch", &[url.into(), init]).ok().and_then(|v| v.as_object().cloned()).expect("fetch returned no promise")
}

enum Slot {
    Ready(Box<dyn FnMut(Vec<Value>)>),
    /// Being called right now (it may register more callbacks while it runs).
    Running,
    /// Released while it ran: dropped when it returns.
    Released,
    Empty,
}

thread_local! {
    static CALLBACKS: RefCell<Vec<Slot>> = const { RefCell::new(Vec::new()) };
}

/// A function the browser can call later: a listener, a timer, a promise callback. The
/// browser says when it's done with it, and it's dropped then.
pub fn callback(f: impl FnMut(Vec<Value>) + 'static) -> Value {
    CALLBACKS.with(|c| {
        let mut slots = c.borrow_mut();
        let id = match slots.iter().position(|s| matches!(s, Slot::Empty)) {
            Some(free) => free,
            None => {
                slots.push(Slot::Empty);
                slots.len() - 1
            }
        };
        slots[id] = Slot::Ready(Box::new(f));
        Value::Callback(id as u32)
    })
}

fn pack(text: String) -> u64 {
    let bytes = text.into_bytes().into_boxed_slice();
    let len = bytes.len();
    let ptr = Box::into_raw(bytes) as *mut u8;
    ((ptr as u64) << 32) | len as u64
}

/// The exports the browser calls; [`main!`] defines them in the page's crate, so they're
/// exported whatever the linker does with a library's own.
#[doc(hidden)]
pub mod abi {
    use super::*;

    pub fn alloc(len: u32) -> *mut u8 {
        let mut buf = Vec::<u8>::with_capacity(len.max(1) as usize);
        let ptr = buf.as_mut_ptr();
        std::mem::forget(buf);
        ptr
    }

    /// # Safety
    /// `ptr` came from `alloc(len)`.
    pub unsafe fn free(ptr: *mut u8, len: u32) {
        drop(unsafe { Vec::from_raw_parts(ptr, 0, len.max(1) as usize) });
    }

    pub fn host_api() -> u32 {
        include_str!("../../HOST_API").trim().parse().unwrap_or(0)
    }

    /// Run the page's entry point. Panics are reported to the console by the hook set here,
    /// then trap (wasm32-wasip1 aborts on panic): the browser stops the page.
    pub fn run_main(main: fn()) -> u64 {
        std::panic::set_hook(Box::new(|info| {
            let _ = console().call("error", &[format!("Rust panic: {info}").into()]);
        }));
        main();
        0
    }

    /// Call callback `id` with the JSON array of arguments at `ptr..ptr + len`.
    ///
    /// # Safety
    /// `ptr..ptr + len` was written by the browser into memory from `alloc`.
    pub unsafe fn fire(id: u32, ptr: *mut u8, len: u32) -> u64 {
        let bytes = unsafe { Vec::from_raw_parts(ptr, len as usize, len.max(1) as usize) };
        let args: Vec<Value> = match serde_json::from_slice::<Json>(&bytes) {
            Ok(Json::Array(items)) => items.into_iter().map(Value::decode).collect(),
            _ => return pack("bad arguments from the browser".into()),
        };
        let id = id as usize;
        let taken = CALLBACKS.with(|c| c.borrow_mut().get_mut(id).map(|s| std::mem::replace(s, Slot::Running)));
        let Some(Slot::Ready(mut f)) = taken else {
            // Not ready (gone, or already running): put back whatever was there.
            if let Some(slot) = taken {
                CALLBACKS.with(|c| c.borrow_mut()[id] = slot);
            }
            return 0;
        };
        f(args);
        CALLBACKS.with(|c| {
            let mut slots = c.borrow_mut();
            slots[id] = match slots[id] {
                Slot::Released => Slot::Empty,
                _ => Slot::Ready(f),
            };
        });
        0
    }

    pub fn release(id: u32) {
        CALLBACKS.with(|c| {
            if let Some(slot) = c.borrow_mut().get_mut(id as usize) {
                *slot = match slot {
                    Slot::Running => Slot::Released,
                    _ => Slot::Empty,
                };
            }
        });
    }
}

/// Name the page's entry point, and export what the browser calls into the page.
#[macro_export]
macro_rules! main {
    ($main:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn rust_main() -> u64 {
            $crate::abi::run_main($main)
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn rust_alloc(len: u32) -> *mut u8 {
            $crate::abi::alloc(len)
        }
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn rust_free(ptr: *mut u8, len: u32) {
            unsafe { $crate::abi::free(ptr, len) }
        }
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn rust_fire(id: u32, ptr: *mut u8, len: u32) -> u64 {
            unsafe { $crate::abi::fire(id, ptr, len) }
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn rust_release(id: u32) {
            $crate::abi::release(id)
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn rust_host_api() -> u32 {
            $crate::abi::host_api()
        }
    };
}
