//! The host dispatcher: the only way a language pack reaches the page. Packs see web
//! platform objects (`window`, `document`, elements, events, `Math`, `console`) as small
//! integer handles; the real nodes stay here. Every call is recorded in the trace (the
//! boundary tracer), with its arguments, result and timing.
//!
//! Lifetimes: element handles last as long as the page; event objects are released when
//! their dispatch ends; handles are never reused, so a stale one fails instead of aliasing.
//! Timers live here too, so time comes from the host (principle 4) and every pack shares them.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use crate::gate::{FetchResponse, Gate, PageFetch};
use crate::replay::Read;
use blitz_dom::{BaseDocument, LocalName, NodeId, QualName, ns};
use blitz_traits::net::Url;
use wasmtime::{AnyRef, OwnedRooted};

/// A JS-like value as seen by the host. Lives inside a wasm externref.
#[derive(Clone, Debug)]
pub enum HostValue {
    Str(Vec<u16>),
    Num(f64),
    Bool(bool),
    Undefined,
    /// JavaScript `null`: a null reference.
    Null,
    /// A function the page handed over (a listener, a timer task); fire it with the pack's
    /// `fire_callback`. The same function always has the same id, so it can be removed.
    CallbackId(usize),
    /// A Dart object passed out by reference. The host can hold it and hand it back,
    /// but not look inside, just as a JS engine treats it.
    Opaque(OwnedRooted<AnyRef>),
    /// A host object (document, element, event...), by handle. Only the host knows what it is.
    Object(u32),
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
            HostValue::Null => "null".into(),
            HostValue::CallbackId(id) => format!("function#{id}"),
            HostValue::Opaque(_) => "[dart object]".into(),
            HostValue::Object(h) => format!("#{h}"),
        }
    }

    /// JavaScript truthiness (`!!value`).
    pub(crate) fn truthy(&self) -> bool {
        match self {
            HostValue::Undefined | HostValue::Null => false,
            HostValue::Bool(b) => *b,
            HostValue::Num(n) => *n != 0.0 && !n.is_nan(),
            HostValue::Str(s) => !s.is_empty(),
            _ => true,
        }
    }

    /// JavaScript `===`.
    pub(crate) fn strict_equals(&self, other: &HostValue) -> bool {
        match (self, other) {
            (HostValue::Undefined, HostValue::Undefined) | (HostValue::Null, HostValue::Null) => true,
            (HostValue::Bool(a), HostValue::Bool(b)) => a == b,
            (HostValue::Num(a), HostValue::Num(b)) => a == b,
            (HostValue::Str(a), HostValue::Str(b)) => a == b,
            (HostValue::Object(a), HostValue::Object(b)) => a == b,
            _ => false,
        }
    }

    pub fn as_num(&self) -> Option<f64> {
        match self {
            HostValue::Num(n) => Some(*n),
            _ => None,
        }
    }
}

pub(crate) fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e21 { format!("{}", n as i64) } else { format!("{n}") }
}

/// What the pack can reach outside its sandbox. Every call goes through here, named the way
/// the web platform names it (`document`, `querySelector`, `textContent`...).
pub trait Host {
    /// `globalThis.<name>`, e.g. `document`.
    fn global(&mut self, name: &str) -> Result<HostValue>;
    /// `target.<prop>`.
    fn get(&mut self, target: &HostValue, prop: &str) -> Result<HostValue>;
    /// `target.<prop> = value`.
    fn set(&mut self, target: &HostValue, prop: &str, value: &HostValue) -> Result<()>;
    /// `target.<method>(args)`. Callback arguments arrive as `HostValue::CallbackId`.
    fn call(&mut self, target: &HostValue, method: &str, args: &[HostValue]) -> Result<HostValue>;
    /// Dart's `print`.
    fn console(&mut self, text: &str);
    /// The current time in milliseconds since the Unix epoch (`Date.now()`).
    fn now(&mut self) -> f64;
    /// Nanoseconds on WASI clock `id`: 0 is the real time since the Unix epoch, anything else
    /// a monotonic clock from the page's start. An interpreter's own clock (`clock_time_get`).
    fn clock(&mut self, id: u32) -> u64;
    /// Random bits for a pack's own randomness (an interpreter's `random_get`).
    fn random_u64(&mut self) -> u64;
    /// A plain object the page built (`{method: "POST", body: "x"}`, a `fetch` init or a
    /// history state): its fields' values must be data (strings, numbers, booleans, null, or
    /// other plain objects). Undefined fields are left out, as `JSON.stringify` does.
    fn object(&mut self, fields: Vec<(String, HostValue)>) -> Result<HostValue>;
    /// An array of data, as `object` (Dart's `String.split` builds one).
    fn array(&mut self, items: Vec<HostValue>) -> Result<HostValue>;
    /// `value instanceof Array`.
    fn is_array(&mut self, value: &HostValue) -> bool;
}

/// Mouse and keyboard details of an event, as the DOM exposes them.
#[derive(Clone, Debug, Default)]
pub struct EventDetails {
    pub client_x: Option<f64>,
    pub client_y: Option<f64>,
    pub page_x: Option<f64>,
    pub page_y: Option<f64>,
    pub screen_x: Option<f64>,
    pub screen_y: Option<f64>,
    pub button: Option<f64>,
    pub buttons: Option<f64>,
    pub key: Option<String>,
    pub code: Option<String>,
    pub repeat: Option<bool>,
    pub alt_key: bool,
    pub ctrl_key: bool,
    pub shift_key: bool,
    pub meta_key: bool,
    /// `popstate`'s `state`, as JSON text.
    pub state: Option<String>,
    /// `hashchange`'s `oldURL` and `newURL`.
    pub old_url: Option<String>,
    pub new_url: Option<String>,
}

#[derive(Clone, Debug)]
pub struct EventObj {
    pub kind: String,
    pub target: NodeId,
    pub current_target: NodeId,
    pub details: EventDetails,
    pub default_prevented: bool,
    pub propagation_stopped: bool,
    pub immediate_propagation_stopped: bool,
}

/// A web platform object a pack holds a handle to.
#[derive(Clone, Debug)]
pub enum Obj {
    Window,
    Document,
    Math,
    Console,
    Element(NodeId),
    Event(EventObj),
    /// A promise the host settles later (`fetch`, `Response.text()`); see `Objects::settle`.
    Promise,
    Response(Rc<FetchResponse>),
    Headers(Rc<Vec<(String, String)>>),
    /// A plain data object or array: one the page built, or parsed JSON (`Response.json()`).
    Json(serde_json::Value),
    /// `localStorage` (true) or `sessionStorage` (false).
    Storage(bool),
    Location,
    History,
    Navigator,
}

impl Obj {
    pub(crate) fn interface(&self) -> &'static str {
        match self {
            Obj::Window => "Window",
            Obj::Document => "Document",
            Obj::Math => "Math",
            Obj::Console => "Console",
            Obj::Element(_) => "Element",
            Obj::Event(_) => "Event",
            Obj::Promise => "Promise",
            Obj::Response(_) => "Response",
            Obj::Headers(_) => "Headers",
            Obj::Json(v) if v.is_array() => "Array",
            Obj::Json(_) => "Object",
            Obj::Storage(_) => "Storage",
            Obj::Location => "Location",
            Obj::History => "History",
            Obj::Navigator => "Navigator",
        }
    }
}

/// `navigator.language`. Fixed rather than the system's, so pages can't use it to tell
/// machines apart beyond what the User-Agent already says.
const LANGUAGE: &str = "en-US";

/// What a page reaches beyond its own document. The loader gives each page its tab's; tests
/// and replays use the default, which has no network.
#[derive(Clone, Default)]
pub struct PageEnv {
    /// The gate the page's `fetch` goes through; None: the page has no network.
    pub gate: Option<std::sync::Arc<Gate>>,
    /// The tab's `sessionStorage` (a fresh one: nothing is shared).
    pub session: crate::storage::Session,
    /// The tab's history, for `location` and `history`; None outside a tab (navigations do
    /// nothing).
    pub nav: Option<Rc<dyn TabNav>>,
}

/// What a page's `location` and `history` change in its tab (`history::TabHistory`).
pub trait TabNav {
    /// Load `url` as a new document: a new entry, or in place of the current one.
    fn navigate(&self, url: Url, replace: bool);
    fn reload(&self);
    /// A new entry, or the current one replaced, in the same document (pushState,
    /// replaceState, a fragment), with its `history.state` as JSON text.
    fn set_entry(&self, url: Url, state: Option<String>, replace: bool);
    /// `history.go(delta)`: the tab moves, and tells the page if it stays in this document.
    fn go(&self, delta: i64);
    /// The current entry's state, and how many entries there are.
    fn state(&self) -> Option<String>;
    fn length(&self) -> usize;
}

/// A promise's state: its outcome once settled, and the callbacks waiting for it.
#[derive(Default)]
struct PromiseState {
    outcome: Option<Result<HostValue, HostValue>>,
    /// (on fulfilled, on rejected), in the order `then` was called.
    reactions: Vec<(Option<usize>, Option<usize>)>,
}

/// A finished `fetch`, waiting to be delivered to the page on the UI thread.
pub type Fetched = (u32, Result<FetchResponse, String>);

struct Timer {
    due: Instant,
    interval: Option<Duration>,
    callback: usize,
    /// requestAnimationFrame: called with a timestamp.
    frame: bool,
    /// A promise reaction: called with the promise's value (`args`).
    args: Option<Vec<HostValue>>,
}

/// A pending timer, as the Inspector's Handles view shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct TimerInfo {
    pub handle: u32,
    /// "timeout", "interval", "frame" or "promise".
    pub kind: &'static str,
    /// Milliseconds until it's due (negative: overdue, e.g. while paused).
    pub due_in_ms: f64,
    pub callback: usize,
}

/// A timer callback that is due, as `take_due_timers` hands it out.
pub struct DueTimer {
    pub handle: u32,
    pub callback: usize,
    pub args: Vec<HostValue>,
    /// What it is, for the trace: "timer", "animation frame" or "promise".
    pub kind: &'static str,
}

impl Timer {
    fn kind(&self) -> &'static str {
        match (self.frame, &self.args) {
            (true, _) => "animation frame",
            (_, Some(_)) => "promise",
            _ => "timer",
        }
    }
}

/// Objects handed to the pack, its event listeners and its timers. Shared between the host
/// (which the pack calls) and the document (which delivers events and runs timers).
pub struct Objects {
    objects: HashMap<u32, Obj>,
    next_handle: u32,
    /// One handle per node, so the same element always compares equal.
    element_handles: HashMap<NodeId, u32>,
    singletons: HashMap<&'static str, u32>,
    /// Callback ids registered with addEventListener, by (node, event type), in order.
    pub listeners: HashMap<(NodeId, String), Vec<usize>>,
    timers: BTreeMap<u32, Timer>,
    next_timer: u32,
    started: Instant,
    /// Callbacks nothing refers to any more; the document tells the pack to release them.
    released_callbacks: Vec<usize>,
    /// Who the pack is running for, when not the page: console input ("console") or AI
    /// eval ("ai"). Host calls are traced under it.
    pub caller: std::cell::Cell<Option<&'static str>>,
    /// Time spent in host calls so far, in microseconds, so time inside the pack can be told
    /// apart from time crossing into the host (the Inspector's profiler).
    pub host_micros: std::cell::Cell<u64>,
    /// The page's nondeterminism: live, recorded or replayed (`replay.rs`).
    pub tape: crate::replay::Tape,
    promises: HashMap<u32, PromiseState>,
    /// Fetches finished on the network's threads, for the UI thread to settle (`take_fetched`).
    fetched: std::sync::Arc<Mutex<Vec<Fetched>>>,
    /// Fetches started and not yet taken.
    fetching: usize,
    /// Wakes the page's event loop: a timer is due, or a fetch finished.
    pub waker: std::sync::Arc<Mutex<Option<std::task::Waker>>>,
    /// `history.state` for the page's current entry, as JSON text; None until first read
    /// from the tab.
    pub history_state: Option<Option<String>>,
    /// Events for `window` the page caused (`hashchange` after setting `location.hash`), to
    /// dispatch as a task once its code returns.
    pub queued_events: Vec<(String, EventDetails)>,
}

impl Default for Objects {
    fn default() -> Self {
        Self {
            objects: HashMap::new(),
            next_handle: 1,
            element_handles: HashMap::new(),
            singletons: HashMap::new(),
            listeners: HashMap::new(),
            timers: BTreeMap::new(),
            next_timer: 1,
            started: Instant::now(),
            released_callbacks: Vec::new(),
            caller: Default::default(),
            host_micros: Default::default(),
            tape: Default::default(),
            promises: HashMap::new(),
            fetched: Default::default(),
            fetching: 0,
            waker: Default::default(),
            history_state: None,
            queued_events: Vec::new(),
        }
    }
}

impl Objects {
    fn add(&mut self, obj: Obj) -> u32 {
        let handle = self.next_handle;
        self.next_handle += 1;
        self.objects.insert(handle, obj);
        handle
    }

    fn element(&mut self, node: NodeId) -> HostValue {
        if let Some(h) = self.element_handles.get(&node) {
            return HostValue::Object(*h);
        }
        let handle = self.add(Obj::Element(node));
        self.element_handles.insert(node, handle);
        HostValue::Object(handle)
    }

    fn singleton(&mut self, name: &'static str, obj: Obj) -> HostValue {
        let handle = match self.singletons.get(name) {
            Some(h) => *h,
            None => {
                let h = self.add(obj);
                self.singletons.insert(name, h);
                h
            }
        };
        HostValue::Object(handle)
    }

    /// A new event object for one dispatch. Release it with `release` when the dispatch ends.
    pub fn event(&mut self, kind: &str, target: NodeId, details: EventDetails) -> u32 {
        self.add(Obj::Event(EventObj {
            kind: kind.to_string(),
            target,
            current_target: target,
            details,
            default_prevented: false,
            propagation_stopped: false,
            immediate_propagation_stopped: false,
        }))
    }

    pub fn event_mut(&mut self, handle: u32) -> Option<&mut EventObj> {
        match self.objects.get_mut(&handle) {
            Some(Obj::Event(e)) => Some(e),
            _ => None,
        }
    }

    pub fn release(&mut self, handle: u32) {
        self.objects.remove(&handle);
    }

    #[cfg(test)]
    pub fn live_objects(&self) -> usize {
        self.objects.len()
    }

    /// A one-line summary for an exception snapshot: what the pack holds.
    pub fn summary(&self) -> String {
        let listeners: usize = self.listeners.values().map(Vec::len).sum();
        format!("{} live handles, {listeners} listeners, {} timers pending", self.objects.len(), self.timers.len())
    }

    /// Every object the pack holds a handle to, oldest first.
    pub fn held(&self) -> Vec<(u32, Obj)> {
        let mut held: Vec<(u32, Obj)> = self.objects.iter().map(|(h, o)| (*h, o.clone())).collect();
        held.sort_by_key(|(h, _)| *h);
        held
    }

    /// The timers waiting to run, soonest first.
    pub fn pending_timers(&self, now: Instant) -> Vec<TimerInfo> {
        let mut timers: Vec<TimerInfo> = self
            .timers
            .iter()
            .map(|(handle, t)| TimerInfo {
                handle: *handle,
                kind: if t.frame { "frame" } else if t.args.is_some() { "promise" } else if t.interval.is_some() { "interval" } else { "timeout" },
                due_in_ms: if t.due >= now { (t.due - now).as_secs_f64() * 1000.0 } else { -((now - t.due).as_secs_f64() * 1000.0) },
                callback: t.callback,
            })
            .collect();
        timers.sort_by(|a, b| a.due_in_ms.total_cmp(&b.due_in_ms));
        timers
    }

    /// A callback can be released once no listener or timer refers to it.
    fn release_if_unused(&mut self, callback: usize) {
        let in_listeners = self.listeners.values().any(|ids| ids.contains(&callback));
        let in_timers = self.timers.values().any(|t| t.callback == callback);
        let in_promises = self.promises.values().flat_map(|p| &p.reactions).any(|(f, r)| *f == Some(callback) || *r == Some(callback));
        if !in_listeners && !in_timers && !in_promises {
            self.released_callbacks.push(callback);
        }
    }

    pub fn take_released_callbacks(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.released_callbacks)
    }

    fn get(&self, value: &HostValue) -> Result<Obj> {
        match value {
            HostValue::Object(h) => self.objects.get(h).cloned().context("this object is no longer available"),
            other => bail!("expected a web object, got {}", other.display()),
        }
    }

    fn add_timer(&mut self, callback: usize, delay: Duration, interval: Option<Duration>, frame: bool) -> HostValue {
        let handle = self.next_timer;
        self.next_timer += 1;
        self.timers.insert(handle, Timer { due: Instant::now() + delay, interval, callback, frame, args: None });
        HostValue::Num(handle as f64)
    }

    // ---- Promises

    fn promise(&mut self) -> u32 {
        let handle = self.add(Obj::Promise);
        self.promises.insert(handle, PromiseState::default());
        handle
    }

    /// `promise.then(on_fulfilled, on_rejected)`. Each callback runs later, as a task (a
    /// zero-delay timer with the value), never during the call, as for a real promise.
    fn then(&mut self, promise: u32, on_fulfilled: Option<usize>, on_rejected: Option<usize>) -> Result<()> {
        let state = self.promises.get_mut(&promise).context("this promise is no longer available")?;
        state.reactions.push((on_fulfilled, on_rejected));
        if state.outcome.is_some() {
            self.react(promise);
        }
        Ok(())
    }

    /// Settle `promise` with `outcome` (Ok: fulfilled, Err: rejected) and run its reactions.
    /// A promise settles once; later outcomes are ignored.
    pub fn settle(&mut self, promise: u32, outcome: Result<HostValue, HostValue>) {
        if let Some(state) = self.promises.get_mut(&promise) {
            if state.outcome.is_none() {
                state.outcome = Some(outcome);
                self.react(promise);
            }
        }
    }

    /// Queue a settled promise's waiting callbacks; the ones for the other outcome go.
    fn react(&mut self, promise: u32) {
        let Some(state) = self.promises.get_mut(&promise) else { return };
        let Some(outcome) = state.outcome.clone() else { return };
        let reactions = std::mem::take(&mut state.reactions);
        for (on_fulfilled, on_rejected) in reactions {
            let (run, unused, value) = match &outcome {
                Ok(v) => (on_fulfilled, on_rejected, v.clone()),
                Err(e) => (on_rejected, on_fulfilled, e.clone()),
            };
            if let Some(callback) = run {
                let handle = self.next_timer;
                self.next_timer += 1;
                self.timers.insert(handle, Timer { due: Instant::now(), interval: None, callback, frame: false, args: Some(vec![value]) });
            }
            if let Some(callback) = unused {
                self.release_if_unused(callback);
            }
        }
    }

    /// Fetches that finished since last asked, to settle on the UI thread.
    pub fn take_fetched(&mut self) -> Vec<Fetched> {
        let done = std::mem::take(&mut *self.fetched.lock().unwrap());
        self.fetching -= done.len().min(self.fetching);
        done
    }

    /// Whether a `fetch` is still on its way.
    #[cfg(test)]
    pub fn fetching(&self) -> bool {
        self.fetching > 0
    }

    /// Settle `promise` with a finished fetch: a new `Response`, or a TypeError message.
    pub fn deliver_fetch(&mut self, promise: u32, result: Result<FetchResponse, String>) {
        let outcome = match result {
            Ok(response) => Ok(HostValue::Object(self.add(Obj::Response(Rc::new(response))))),
            Err(message) => Err(HostValue::str(&message)),
        };
        self.settle(promise, outcome);
    }

    /// A promise already settled with `outcome` (`Response.text()`).
    fn settled(&mut self, outcome: Result<HostValue, HostValue>) -> HostValue {
        let promise = self.promise();
        self.settle(promise, outcome);
        HostValue::Object(promise)
    }

    /// A JSON value as the page sees it: scalars as values, objects and arrays as handles.
    fn from_json(&mut self, value: serde_json::Value) -> HostValue {
        use serde_json::Value;
        match value {
            Value::Null => HostValue::Null,
            Value::Bool(b) => HostValue::Bool(b),
            Value::Number(n) => HostValue::Num(n.as_f64().unwrap_or(f64::NAN)),
            Value::String(s) => HostValue::str(&s),
            other => HostValue::Object(self.add(Obj::Json(other))),
        }
    }

    /// A value the page passed, as JSON data: scalars, and plain objects by handle.
    fn to_json(&self, value: &HostValue) -> Result<Option<serde_json::Value>> {
        use serde_json::Value;
        Ok(Some(match value {
            HostValue::Undefined => return Ok(None),
            HostValue::Null => Value::Null,
            HostValue::Bool(b) => Value::Bool(*b),
            HostValue::Num(n) => serde_json::Number::from_f64(*n).map(Value::Number).unwrap_or(Value::Null),
            HostValue::Str(_) => Value::String(value.display()),
            HostValue::Object(_) => match self.get(value)? {
                Obj::Json(v) => v,
                other => bail!("a {} can't be used as data", other.interface()),
            },
            HostValue::CallbackId(_) | HostValue::Opaque(_) => bail!("a function or object of the page's own can't be used as data"),
        }))
    }

    fn clear_timer(&mut self, handle: u32) {
        if let Some(timer) = self.timers.remove(&handle) {
            self.release_if_unused(timer.callback);
        }
    }

    /// When the next timer is due, if any.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.timers.values().map(|t| t.due).min()
    }

    /// Timers due at `now`, in due order. Intervals are rescheduled; the rest are removed.
    pub fn take_due_timers(&mut self, now: Instant) -> Vec<DueTimer> {
        let mut due: Vec<(Instant, u32)> = self.timers.iter().filter(|(_, t)| t.due <= now).map(|(h, t)| (t.due, *h)).collect();
        due.sort();
        let elapsed_ms = self.started.elapsed().as_secs_f64() * 1000.0;
        let mut out = Vec::new();
        for (_, handle) in due {
            let timer = self.timers.get_mut(&handle).unwrap();
            let fired = DueTimer {
                handle,
                callback: timer.callback,
                args: if timer.frame { vec![HostValue::Num(elapsed_ms)] } else { timer.args.clone().unwrap_or_default() },
                kind: timer.kind(),
            };
            match timer.interval {
                Some(every) => timer.due = now + every,
                None => {
                    // Released after it runs, so it must leave the table first.
                    self.timers.remove(&handle);
                }
            }
            out.push(fired);
        }
        out
    }

    /// Timers `handles` as if they were due, with the arguments given (a replay's recorded
    /// batch). None if one of them isn't pending: the replay has diverged.
    pub fn take_timers(&mut self, fired: &[(u32, Option<f64>)]) -> Option<Vec<DueTimer>> {
        if !fired.iter().all(|(h, _)| self.timers.contains_key(h)) {
            return None;
        }
        let now = Instant::now();
        let mut out = Vec::new();
        for (handle, frame) in fired {
            let timer = self.timers.get_mut(handle)?;
            let args = match frame {
                Some(t) => vec![HostValue::Num(*t)],
                None => timer.args.clone().unwrap_or_default(),
            };
            out.push(DueTimer { handle: *handle, callback: timer.callback, args, kind: timer.kind() });
            match timer.interval {
                Some(every) => timer.due = now + every,
                None => {
                    self.timers.remove(handle);
                }
            }
        }
        Some(out)
    }

    /// One-shot timer callbacks that have just run and are referenced by nothing else.
    pub fn after_timers_ran(&mut self, ran: &[DueTimer]) {
        for t in ran {
            self.release_if_unused(t.callback);
        }
    }
}

/// The page as a pack sees it.
pub struct PageHost {
    pub pack: &'static str,
    /// The page's URL, for the trace.
    pub page: String,
    pub doc: Rc<RefCell<BaseDocument>>,
    pub objects: Rc<RefCell<Objects>>,
    env: PageEnv,
    rng: u64,
}

impl PageHost {
    pub fn new(pack: &'static str, doc: Rc<RefCell<BaseDocument>>, objects: Rc<RefCell<Objects>>, env: PageEnv) -> Self {
        // Randomness comes from the host so a recording can replay it (principle 4).
        let seed = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
        let page = doc.borrow().url().to_string();
        Self { pack, page, doc, objects, env, rng: seed | 1 }
    }

    /// `history.state`: the tab's, read once (on the tape, since it comes from the tab), then
    /// what the page set or traversed to.
    fn history_state(&mut self) -> Option<String> {
        if let Some(state) = self.objects.borrow().history_state.clone() {
            return state;
        }
        let live = self.env.nav.as_ref().and_then(|n| n.state());
        let text = self.objects.borrow_mut().tape.read_text(Read::State, serde_json::to_string(&live).unwrap_or_default());
        let state: Option<String> = serde_json::from_str(&text).unwrap_or(None);
        self.objects.borrow_mut().history_state = Some(state.clone());
        state
    }

    /// `location.href = target`, `location.assign` and `replace`: a fragment of this page
    /// stays in the document (a new entry, then `popstate` and `hashchange`); anything else is
    /// a navigation the tab loads through the gate, like a link. A replay doesn't navigate.
    fn go_to(&mut self, target: &str, replace: bool) -> Result<()> {
        let current = self.doc.borrow().url().clone();
        let url = current.join(target).map_err(|e| anyhow::anyhow!("SyntaxError: bad URL {target:?} ({e})"))?;
        if !matches!(url.scheme(), "http" | "https" | "file" | "about") {
            bail!("{}: URLs can't be navigated to from a page", url.scheme());
        }
        let replaying = self.objects.borrow().tape.replaying();
        if crate::history::same_document(&current, &url) {
            self.doc.borrow_mut().set_base_url(url.as_str());
            let mut objects = self.objects.borrow_mut();
            objects.history_state = Some(None);
            if !replaying {
                let details = EventDetails { old_url: Some(current.to_string()), new_url: Some(url.to_string()), ..Default::default() };
                objects.queued_events.push(("popstate".into(), EventDetails::default()));
                objects.queued_events.push(("hashchange".into(), details));
                if let Some(w) = objects.waker.lock().unwrap().as_ref() {
                    w.wake_by_ref();
                }
            }
            drop(objects);
            if let (Some(nav), false) = (&self.env.nav, replaying) {
                nav.set_entry(url, None, replace);
            }
        } else if let (Some(nav), false) = (&self.env.nav, replaying) {
            nav.navigate(url, replace);
        }
        Ok(())
    }

    /// The origin the page's storage is kept under. Pages with an opaque origin (about:blank,
    /// data:) have none; `file://` pages share one.
    fn storage_origin(&self) -> Result<String> {
        let doc = self.doc.borrow();
        let url = doc.url();
        if url.scheme() != "file" && !url.origin().is_tuple() {
            bail!("SecurityError: storage isn't available on {url}");
        }
        Ok(crate::settings::origin_of(url))
    }

    /// A read of the page's storage, as JSON. It goes on the tape: another tab can change
    /// local storage at any time.
    fn storage_read(&mut self, local: bool, f: impl FnOnce(&crate::storage::Area) -> serde_json::Value) -> Result<HostValue> {
        let origin = self.storage_origin()?;
        let live = match local {
            true => crate::storage::local().read(&origin, f),
            false => f(self.env.session.borrow_mut().entry(origin).or_default()),
        };
        let mut objects = self.objects.borrow_mut();
        let text = objects.tape.read_text(Read::Storage, live.to_string());
        Ok(objects.from_json(serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)))
    }

    /// A change to the page's storage. A replay changes nothing (its reads come from the tape).
    fn storage_write(&mut self, local: bool, f: impl FnOnce(&mut crate::storage::Area) -> Result<(), String>) -> Result<HostValue> {
        let origin = self.storage_origin()?;
        if self.objects.borrow().tape.replaying() {
            return Ok(HostValue::Undefined);
        }
        match local {
            true => crate::storage::local().write(&origin, f),
            false => f(self.env.session.borrow_mut().entry(origin).or_default()),
        }
        .map_err(|e| anyhow::anyhow!(e))?;
        Ok(HostValue::Undefined)
    }

    /// `window.fetch(input, init)`: a promise, settled when the request through the gate
    /// finishes (`RunningPack` delivers it on the UI thread). Replaying, nothing is sent: the
    /// recorded response arrives as a step.
    fn fetch(&mut self, args: &[HostValue]) -> Result<HostValue> {
        let input = args.first().context("fetch: missing argument 0")?.display();
        let page = self.doc.borrow().url().clone();
        let promise = self.objects.borrow_mut().promise();
        let request = (|| -> Result<PageFetch> {
            let url = page.join(&input).map_err(|e| anyhow::anyhow!("TypeError: Failed to fetch: bad URL {input:?} ({e})"))?;
            let mut request = PageFetch::new(url);
            let init = match args.get(1) {
                Some(v @ HostValue::Object(_)) => self.objects.borrow().to_json(v)?,
                _ => None,
            };
            if let Some(init) = init {
                let text = |name: &str| init.get(name).and_then(|v| v.as_str()).map(str::to_string);
                if let Some(method) = text("method") {
                    request.method = method.to_ascii_uppercase();
                }
                for (name, value) in init.get("headers").and_then(|h| h.as_object()).into_iter().flatten() {
                    request.header(name, &value.as_str().map(str::to_string).unwrap_or_else(|| value.to_string()));
                }
                request.body = text("body");
                if let Some(credentials) = text("credentials") {
                    request.credentials = credentials;
                }
            }
            Ok(request)
        })();
        let mut objects = self.objects.borrow_mut();
        if objects.tape.replaying() {
            return Ok(HostValue::Object(promise));
        }
        let (request, gate, runtime) = match (request, &self.env.gate, tokio::runtime::Handle::try_current()) {
            (Ok(request), Some(gate), Ok(runtime)) => (request, gate.clone(), runtime),
            (Err(e), _, _) => {
                objects.settle(promise, Err(HostValue::str(&format!("{e:#}"))));
                return Ok(HostValue::Object(promise));
            }
            _ => {
                objects.settle(promise, Err(HostValue::str("TypeError: Failed to fetch: this page has no network")));
                return Ok(HostValue::Object(promise));
            }
        };
        objects.fetching += 1;
        let (fetched, waker) = (objects.fetched.clone(), objects.waker.clone());
        runtime.spawn(async move {
            let result = gate.fetch_from_page(request, &page).await;
            fetched.lock().unwrap().push((promise, result));
            if let Some(w) = waker.lock().unwrap().as_ref() {
                w.wake_by_ref();
            }
        });
        Ok(HostValue::Object(promise))
    }

    /// xorshift64: fast, and good enough for a page's Math.random.
    fn next_random(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    /// The pack name trace entries go under: the page's pack, or whoever it's running for.
    fn trace_pack(&self) -> &'static str {
        self.objects.borrow().caller.get().unwrap_or(self.pack)
    }

    /// Run `f` and record it in the trace as `label(args)`.
    fn traced<T>(&mut self, label: String, args: String, f: impl FnOnce(&mut Self) -> Result<T>, show: impl Fn(&T) -> String) -> Result<T> {
        let started = Instant::now();
        let result = f(self);
        let micros = started.elapsed().as_micros() as u64;
        let objects = self.objects.borrow();
        objects.host_micros.set(objects.host_micros.get() + micros);
        drop(objects);
        let (kind, shown) = match &result {
            Ok(v) => (TraceKind::Call, show(v)),
            Err(e) => (TraceKind::Error, format!("ERROR {e:#}")),
        };
        record(TraceEntry { pack: self.trace_pack(), kind, page: self.page.clone(), call: label, args, result: shown, micros });
        result
    }

    fn label(&self, target: &HostValue, member: &str) -> String {
        match self.objects.borrow().get(target) {
            Ok(obj) => format!("{}.{member}", obj.interface()),
            Err(_) => format!("?.{member}"),
        }
    }

    fn element(&self, value: &HostValue) -> Result<NodeId> {
        match self.objects.borrow().get(value)? {
            Obj::Element(id) if self.doc.borrow().get_node(id).is_some() => Ok(id),
            Obj::Element(_) => bail!("element was removed from the document"),
            other => bail!("expected an Element, got {}", other.interface()),
        }
    }

    /// The node a query or listener on `obj` is scoped to. Window listeners sit on the root.
    fn scope(&self, obj: &Obj) -> Option<NodeId> {
        match obj {
            Obj::Document | Obj::Window => Some(self.doc.borrow().root_node().id),
            Obj::Element(id) => Some(*id),
            _ => None,
        }
    }

    fn event_prop(&mut self, e: &EventObj, prop: &str) -> Option<HostValue> {
        let num = |v: Option<f64>| v.map(HostValue::Num).unwrap_or(HostValue::Undefined);
        let d = &e.details;
        Some(match prop {
            "type" => HostValue::str(&e.kind),
            "target" => self.objects.borrow_mut().element(e.target),
            "currentTarget" => self.objects.borrow_mut().element(e.current_target),
            "defaultPrevented" => HostValue::Bool(e.default_prevented),
            "clientX" | "x" => num(d.client_x),
            "clientY" | "y" => num(d.client_y),
            "pageX" => num(d.page_x),
            "pageY" => num(d.page_y),
            "screenX" => num(d.screen_x),
            "screenY" => num(d.screen_y),
            "button" => num(d.button),
            "buttons" => num(d.buttons),
            "key" => d.key.as_deref().map(HostValue::str).unwrap_or(HostValue::Undefined),
            "code" => d.code.as_deref().map(HostValue::str).unwrap_or(HostValue::Undefined),
            "repeat" => d.repeat.map(HostValue::Bool).unwrap_or(HostValue::Undefined),
            "altKey" => HostValue::Bool(d.alt_key),
            "ctrlKey" => HostValue::Bool(d.ctrl_key),
            "shiftKey" => HostValue::Bool(d.shift_key),
            "metaKey" => HostValue::Bool(d.meta_key),
            "state" if e.kind == "popstate" => {
                let state = d.state.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(serde_json::Value::Null);
                self.objects.borrow_mut().from_json(state)
            }
            "oldURL" => HostValue::str(d.old_url.as_deref().unwrap_or_default()),
            "newURL" => HostValue::str(d.new_url.as_deref().unwrap_or_default()),
            _ => return None,
        })
    }

    fn do_get(&mut self, target: &HostValue, prop: &str) -> Result<HostValue> {
        let obj = self.objects.borrow().get(target)?;
        Ok(match (&obj, prop) {
            (Obj::Window, "document") => self.objects.borrow_mut().singleton("document", Obj::Document),
            (Obj::Window, "console") => self.objects.borrow_mut().singleton("console", Obj::Console),
            (Obj::Window | Obj::Document, "location") => self.objects.borrow_mut().singleton("location", Obj::Location),
            (Obj::Window, "innerWidth" | "innerHeight" | "devicePixelRatio" | "scrollX" | "scrollY" | "pageXOffset" | "pageYOffset") => {
                // The window can change size and scroll at any time: on the tape.
                let doc = self.doc.borrow();
                let (width, height) = doc.viewport().logical_size();
                let scroll = doc.viewport_scroll();
                let live = match prop {
                    "innerWidth" => width.round() as f64,
                    "innerHeight" => height.round() as f64,
                    "devicePixelRatio" => doc.viewport().scale_f64(),
                    "scrollX" | "pageXOffset" => scroll.x,
                    _ => scroll.y,
                };
                drop(doc);
                HostValue::Num(f64::from_bits(self.objects.borrow_mut().tape.read(Read::Viewport, live.to_bits())))
            }
            (Obj::Window, "navigator") => self.objects.borrow_mut().singleton("navigator", Obj::Navigator),
            (Obj::Navigator, "userAgent") => HostValue::str(blitz_net::USER_AGENT),
            (Obj::Navigator, "language") => HostValue::str(LANGUAGE),
            (Obj::Window, "history") => self.objects.borrow_mut().singleton("history", Obj::History),
            (Obj::Location, _) => {
                let url = self.doc.borrow().url().clone();
                let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
                let host = url.host_str().unwrap_or_default();
                HostValue::str(&match prop {
                    "href" => url.to_string(),
                    "protocol" => format!("{}:", url.scheme()),
                    "host" => format!("{host}{port}"),
                    "hostname" => host.to_string(),
                    "port" => port.trim_start_matches(':').to_string(),
                    "pathname" => url.path().to_string(),
                    "search" => url.query().filter(|q| !q.is_empty()).map(|q| format!("?{q}")).unwrap_or_default(),
                    "hash" => url.fragment().filter(|f| !f.is_empty()).map(|f| format!("#{f}")).unwrap_or_default(),
                    "origin" => url.origin().ascii_serialization(),
                    _ => bail!("Location.{prop} is not supported yet"),
                })
            }
            (Obj::History, "state") => {
                let state = self.history_state();
                self.objects.borrow_mut().from_json(state.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(serde_json::Value::Null))
            }
            (Obj::History, "length") => {
                let live = self.env.nav.as_ref().map_or(1, |n| n.length());
                let text = self.objects.borrow_mut().tape.read_text(Read::State, live.to_string());
                HostValue::Num(text.parse().unwrap_or(1.0))
            }
            (Obj::Window, "localStorage") => self.objects.borrow_mut().singleton("localStorage", Obj::Storage(true)),
            (Obj::Window, "sessionStorage") => self.objects.borrow_mut().singleton("sessionStorage", Obj::Storage(false)),
            (Obj::Storage(local), "length") => self.storage_read(*local, |a| a.len().into())?,
            (Obj::Document, "body") => match self.doc.borrow().query_selector("body").ok().flatten() {
                Some(id) => self.objects.borrow_mut().element(id),
                None => HostValue::Null,
            },
            // The page's cookies (never HttpOnly ones), from the profile's jar; on the tape, as
            // another tab or a response may change them at any time.
            (Obj::Document, "cookie") => {
                let live = crate::cookies::jar().document_cookie(self.doc.borrow().url());
                HostValue::str(&self.objects.borrow_mut().tape.read_text(Read::Cookie, live))
            }
            (Obj::Element(id), "textContent") => HostValue::str(&self.doc.borrow().get_node(*id).context("removed")?.text_content()),
            (Obj::Element(id), "tagName") => {
                let doc = self.doc.borrow();
                let el = doc.get_node(*id).and_then(|n| n.element_data()).context("not an element")?;
                HostValue::str(&el.name.local.to_ascii_uppercase())
            }
            (Obj::Element(id), "id") => HostValue::str(self.doc.borrow().get_node(*id).and_then(|n| n.attr(LocalName::from("id"))).unwrap_or("")),
            (Obj::Response(r), _) => match prop {
                "status" => HostValue::Num(r.status as f64),
                "ok" => HostValue::Bool((200..300).contains(&r.status)),
                "statusText" => HostValue::str(&r.status_text),
                "url" => HostValue::str(&r.url),
                "redirected" => HostValue::Bool(r.redirected),
                "type" => HostValue::str(if r.cross_origin { "cors" } else { "basic" }),
                "bodyUsed" => HostValue::Bool(false),
                "headers" => HostValue::Object(self.objects.borrow_mut().add(Obj::Headers(Rc::new(r.headers.clone())))),
                _ => bail!("Response.{prop} is not supported yet"),
            },
            (Obj::Json(value), _) => {
                let field = match value {
                    serde_json::Value::Array(items) if prop == "length" => Some(serde_json::Value::from(items.len())),
                    serde_json::Value::Array(items) => prop.parse::<usize>().ok().and_then(|i| items.get(i).cloned()),
                    serde_json::Value::Object(fields) => fields.get(prop).cloned(),
                    _ => None,
                };
                match field {
                    Some(v) => self.objects.borrow_mut().from_json(v),
                    None => HostValue::Undefined,
                }
            }
            (Obj::Event(e), _) => match self.event_prop(e, prop) {
                Some(v) => v,
                None => bail!("Event.{prop} is not supported yet"),
            },
            _ => bail!("{}.{prop} is not supported yet", obj.interface()),
        })
    }

    fn do_set(&mut self, target: &HostValue, prop: &str, value: &HostValue) -> Result<()> {
        let obj = self.objects.borrow().get(target)?;
        match (&obj, prop) {
            (Obj::Element(_), _) => {}
            (Obj::Json(_), _) => {
                let field = self.objects.borrow().to_json(value)?;
                let HostValue::Object(handle) = target else { bail!("not an object") };
                if let (Some(Obj::Json(serde_json::Value::Object(fields))), Some(field)) = (self.objects.borrow_mut().objects.get_mut(handle), field) {
                    fields.insert(prop.to_string(), field);
                }
                return Ok(());
            }
            (Obj::Location, "href" | "hash") | (Obj::Window | Obj::Document, "location") => {
                let text = value.display();
                let target = match prop {
                    "hash" => format!("#{}", text.trim_start_matches('#')),
                    _ => text,
                };
                self.go_to(&target, false)?;
                return Ok(());
            }
            (Obj::Document, "cookie") => {
                // A replay reads cookies from its tape; it mustn't change the real jar.
                if !self.objects.borrow().tape.replaying() {
                    crate::cookies::jar().set_document_cookie(self.doc.borrow().url(), &value.display());
                }
                return Ok(());
            }
            _ => bail!("{}.{prop} = ... is not supported yet", obj.interface()),
        }
        let id = self.element(target).with_context(|| format!("setting {prop}"))?;
        let mut doc = self.doc.borrow_mut();
        let mut m = doc.mutate();
        match prop {
            // Replaces the element's children with one text node.
            "textContent" => {
                m.remove_and_drop_all_children(id);
                let text = m.create_text_node(&value.display());
                m.append_children(id, &[text]);
            }
            "id" | "className" => {
                let attr = if prop == "id" { "id" } else { "class" };
                m.set_attribute(id, QualName::new(None, ns!(), LocalName::from(attr)), &value.display());
            }
            _ => bail!("Element.{prop} = ... is not supported yet"),
        }
        Ok(())
    }

    fn do_call(&mut self, target: &HostValue, method: &str, args: &[HostValue]) -> Result<HostValue> {
        let obj = self.objects.borrow().get(target)?;
        let text = |i: usize| args.get(i).map(|v| v.display()).with_context(|| format!("{method}: missing argument {i}"));
        let attr_name = |i: usize| text(i).map(|n| QualName::new(None, ns!(), LocalName::from(n.as_str())));
        let callback = |i: usize| match args.get(i) {
            Some(HostValue::CallbackId(id)) => Ok(*id),
            _ => bail!("{method}: argument {i} is not a function"),
        };
        let millis = |i: usize| Duration::from_secs_f64(args.get(i).and_then(|v| v.as_num()).unwrap_or(0.0).max(0.0) / 1000.0);

        Ok(match (&obj, method) {
            (Obj::Document | Obj::Element(_), "querySelector") => {
                let scope = self.scope(&obj).context("no scope")?;
                let selector = text(0)?;
                let found = self.doc.borrow().query_selector_in(scope, &selector).map_err(|e| anyhow::anyhow!("bad selector {selector:?}: {e:?}"))?;
                match found {
                    Some(id) => self.objects.borrow_mut().element(id),
                    None => HostValue::Null,
                }
            }
            (Obj::Window | Obj::Document | Obj::Element(_), "addEventListener") => {
                let node = self.scope(&obj).context("no scope")?;
                let (kind, id) = (text(0)?, callback(1)?);
                let mut objects = self.objects.borrow_mut();
                let ids = objects.listeners.entry((node, kind)).or_default();
                // Adding the same listener twice has no effect (DOM spec).
                if !ids.contains(&id) {
                    ids.push(id);
                }
                HostValue::Undefined
            }
            (Obj::Window | Obj::Document | Obj::Element(_), "removeEventListener") => {
                let node = self.scope(&obj).context("no scope")?;
                let (kind, id) = (text(0)?, callback(1)?);
                let mut objects = self.objects.borrow_mut();
                if let Some(ids) = objects.listeners.get_mut(&(node, kind)) {
                    ids.retain(|l| *l != id);
                }
                objects.release_if_unused(id);
                HostValue::Undefined
            }
            (Obj::Document, "createElement") => {
                let tag = text(0)?;
                let name = QualName::new(None, ns!(html), LocalName::from(tag.as_str()));
                let id = self.doc.borrow_mut().mutate().create_element(name, Vec::new());
                self.objects.borrow_mut().element(id)
            }
            (Obj::Element(parent), "appendChild") => {
                let child_value = args.first().context("appendChild: missing child")?;
                let child = self.element(child_value)?;
                self.doc.borrow_mut().mutate().append_children(*parent, &[child]);
                child_value.clone()
            }
            (Obj::Element(id), "remove") => {
                self.doc.borrow_mut().mutate().remove_node(*id);
                HostValue::Undefined
            }
            (Obj::Element(id), "setAttribute") => {
                let (name, value) = (attr_name(0)?, text(1)?);
                self.doc.borrow_mut().mutate().set_attribute(*id, name, &value);
                HostValue::Undefined
            }
            (Obj::Element(id), "removeAttribute") => {
                self.doc.borrow_mut().mutate().clear_attribute(*id, attr_name(0)?);
                HostValue::Undefined
            }
            (Obj::Element(id), "getAttribute") => {
                let name = text(0)?;
                match self.doc.borrow().get_node(*id).and_then(|n| n.attr(LocalName::from(name.as_str()))) {
                    Some(v) => HostValue::str(v),
                    None => HostValue::Null,
                }
            }
            (Obj::Event(_), "preventDefault" | "stopPropagation" | "stopImmediatePropagation") => {
                let HostValue::Object(handle) = target else { bail!("not an event") };
                let mut objects = self.objects.borrow_mut();
                let e = objects.event_mut(*handle).context("this event is no longer available")?;
                match method {
                    "preventDefault" => e.default_prevented = true,
                    "stopPropagation" => e.propagation_stopped = true,
                    _ => {
                        e.propagation_stopped = true;
                        e.immediate_propagation_stopped = true;
                    }
                }
                HostValue::Undefined
            }
            (Obj::Window, "setTimeout") => self.objects.borrow_mut().add_timer(callback(0)?, millis(1), None, false),
            (Obj::Window, "setInterval") => {
                // A zero interval would spin; browsers clamp nested timers to ~4 ms.
                let every = millis(1).max(Duration::from_millis(4));
                self.objects.borrow_mut().add_timer(callback(0)?, every, Some(every), false)
            }
            (Obj::Window, "requestAnimationFrame") => {
                self.objects.borrow_mut().add_timer(callback(0)?, Duration::from_millis(16), None, true)
            }
            (Obj::Window, "clearTimeout" | "clearInterval" | "cancelAnimationFrame") => {
                if let Some(handle) = args.first().and_then(|v| v.as_num()) {
                    self.objects.borrow_mut().clear_timer(handle as u32);
                }
                HostValue::Undefined
            }
            (Obj::Console, "log" | "info" | "warn" | "error" | "debug") => {
                let line = args.iter().map(|a| a.display()).collect::<Vec<_>>().join(" ");
                // The level travels in `call` ("console.warn"), so the console can filter by it.
                let call = if method == "log" { "console".to_string() } else { format!("console.{method}") };
                record(TraceEntry { pack: self.trace_pack(), kind: TraceKind::Console, page: self.page.clone(), call, args: line, result: String::new(), micros: 0 });
                HostValue::Undefined
            }
            (Obj::Window, "fetch") => self.fetch(args)?,
            (Obj::Window, "scrollTo" | "scroll" | "scrollBy") => {
                // scrollTo(x, y) or scrollTo({left, top}); scrollBy is relative.
                let current = self.doc.borrow().viewport_scroll();
                let options = match args.first() {
                    Some(v @ HostValue::Object(_)) => self.objects.borrow().to_json(v)?,
                    _ => None,
                };
                let (x, y) = match &options {
                    Some(o) => (o.get("left").and_then(|v| v.as_f64()), o.get("top").and_then(|v| v.as_f64())),
                    None => (args.first().and_then(|v| v.as_num()), args.get(1).and_then(|v| v.as_num())),
                };
                let (x, y) = match method {
                    "scrollBy" => (current.x + x.unwrap_or(0.0), current.y + y.unwrap_or(0.0)),
                    _ => (x.unwrap_or(current.x), y.unwrap_or(current.y)),
                };
                let clean = |v: f64| if v.is_finite() { v.max(0.0) } else { 0.0 };
                self.doc.borrow_mut().set_viewport_scroll(blitz_dom::Point { x: clean(x), y: clean(y) });
                HostValue::Undefined
            }
            (Obj::Location, "assign" | "replace") => {
                self.go_to(&text(0)?, method == "replace")?;
                HostValue::Undefined
            }
            (Obj::Location, "reload") => {
                if let (Some(nav), false) = (&self.env.nav, self.objects.borrow().tape.replaying()) {
                    nav.reload();
                }
                HostValue::Undefined
            }
            (Obj::Location, "toString") => HostValue::str(self.doc.borrow().url().as_str()),
            (Obj::History, "pushState" | "replaceState") => {
                let state = match args.first() {
                    Some(v) => self.objects.borrow().to_json(v)?.map(|v| v.to_string()),
                    None => None,
                };
                let current = self.doc.borrow().url().clone();
                let url = match args.get(2) {
                    Some(HostValue::Undefined | HostValue::Null) | None => current.clone(),
                    Some(v) => current.join(&v.display()).map_err(|e| anyhow::anyhow!("SyntaxError: bad URL {:?} ({e})", v.display()))?,
                };
                if url.origin() != current.origin() {
                    bail!("SecurityError: {method} can't change the page's origin (to {url})");
                }
                self.doc.borrow_mut().set_base_url(url.as_str());
                self.objects.borrow_mut().history_state = Some(state.clone());
                if let (Some(nav), false) = (&self.env.nav, self.objects.borrow().tape.replaying()) {
                    nav.set_entry(url, state, method == "replaceState");
                }
                HostValue::Undefined
            }
            (Obj::History, "back" | "forward" | "go") => {
                let delta = match method {
                    "back" => -1,
                    "forward" => 1,
                    _ => args.first().and_then(|v| v.as_num()).unwrap_or(0.0) as i64,
                };
                if let (Some(nav), false) = (&self.env.nav, self.objects.borrow().tape.replaying()) {
                    nav.go(delta);
                }
                HostValue::Undefined
            }
            (Obj::Storage(local), "getItem") => {
                let key = text(0)?;
                self.storage_read(*local, |a| a.get(&key).into())?
            }
            (Obj::Storage(local), "key") => {
                let index = args.first().and_then(|v| v.as_num()).unwrap_or(0.0);
                self.storage_read(*local, |a| if index >= 0.0 { a.key(index as usize).into() } else { serde_json::Value::Null })?
            }
            (Obj::Storage(local), "setItem") => {
                let (key, value) = (text(0)?, text(1)?);
                self.storage_write(*local, |a| a.set(&key, &value))?
            }
            (Obj::Storage(local), "removeItem") => {
                let key = text(0)?;
                self.storage_write(*local, |a| Ok(a.remove(&key)))?
            }
            (Obj::Storage(local), "clear") => self.storage_write(*local, |a| Ok(a.clear()))?,
            (Obj::Promise, "then") => {
                let HostValue::Object(promise) = target else { bail!("not a promise") };
                let function = |i: usize| match args.get(i) {
                    Some(HostValue::CallbackId(id)) => Some(*id),
                    _ => None,
                };
                self.objects.borrow_mut().then(*promise, function(0), function(1))?;
                HostValue::Undefined
            }
            (Obj::Response(r), "text") => self.objects.borrow_mut().settled(Ok(HostValue::str(&r.body))),
            (Obj::Response(r), "json") => {
                let parsed = serde_json::from_str::<serde_json::Value>(&r.body);
                let mut objects = self.objects.borrow_mut();
                let outcome = match parsed {
                    Ok(value) => Ok(objects.from_json(value)),
                    Err(e) => Err(HostValue::str(&format!("SyntaxError: the response isn't JSON ({e})"))),
                };
                objects.settled(outcome)
            }
            (Obj::Headers(headers), "get" | "has") => {
                let name = text(0)?.to_ascii_lowercase();
                let values: Vec<&str> = headers.iter().filter(|(n, _)| *n == name).map(|(_, v)| v.as_str()).collect();
                match (method, values.is_empty()) {
                    ("has", empty) => HostValue::Bool(!empty),
                    (_, true) => HostValue::Null,
                    (_, false) => HostValue::str(&values.join(", ")),
                }
            }
            (Obj::Math, "random") => {
                let live = (self.next_random() >> 11) as f64 / (1u64 << 53) as f64;
                HostValue::Num(f64::from_bits(self.objects.borrow_mut().tape.read(Read::Random, live.to_bits())))
            }
            _ => bail!("{}.{method}() is not supported yet", obj.interface()),
        })
    }
}

fn show_args(args: &[HostValue]) -> String {
    args.iter().map(|a| a.display()).collect::<Vec<_>>().join(", ")
}

impl Host for PageHost {
    fn global(&mut self, name: &str) -> Result<HostValue> {
        // Looking up `window` happens on every timer call; don't flood the trace with it.
        if name == "window" {
            return Ok(self.objects.borrow_mut().singleton("window", Obj::Window));
        }
        self.traced(
            "globalThis".into(),
            name.to_string(),
            |h| match name {
                "document" => Ok(h.objects.borrow_mut().singleton("document", Obj::Document)),
                "Math" => Ok(h.objects.borrow_mut().singleton("Math", Obj::Math)),
                "console" => Ok(h.objects.borrow_mut().singleton("console", Obj::Console)),
                other => bail!("globalThis.{other} is not supported yet"),
            },
            |v| v.display(),
        )
    }

    fn get(&mut self, target: &HostValue, prop: &str) -> Result<HostValue> {
        let label = self.label(target, prop);
        self.traced(label, target.display(), |h| h.do_get(target, prop), |v| v.display())
    }

    fn set(&mut self, target: &HostValue, prop: &str, value: &HostValue) -> Result<()> {
        let label = format!("{} =", self.label(target, prop));
        self.traced(label, format!("{}, {}", target.display(), value.display()), |h| h.do_set(target, prop, value), |_| String::new())
    }

    fn call(&mut self, target: &HostValue, method: &str, args: &[HostValue]) -> Result<HostValue> {
        let label = self.label(target, method);
        self.traced(label, format!("{} ← {}", target.display(), show_args(args)), |h| h.do_call(target, method, args), |v| v.display())
    }

    fn console(&mut self, text: &str) {
        record(TraceEntry { pack: self.trace_pack(), kind: TraceKind::Console, page: self.page.clone(), call: "console".into(), args: text.to_string(), result: String::new(), micros: 0 });
    }

    fn now(&mut self) -> f64 {
        let live = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0);
        f64::from_bits(self.objects.borrow_mut().tape.read(Read::Now, live.to_bits()))
    }

    fn clock(&mut self, id: u32) -> u64 {
        let live = match id {
            0 => SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0),
            _ => self.objects.borrow().started.elapsed().as_nanos() as u64,
        };
        self.objects.borrow_mut().tape.read(Read::Clock, live)
    }

    fn random_u64(&mut self) -> u64 {
        let live = self.next_random();
        self.objects.borrow_mut().tape.read(Read::Bits, live)
    }

    fn object(&mut self, fields: Vec<(String, HostValue)>) -> Result<HostValue> {
        let mut objects = self.objects.borrow_mut();
        let mut map = serde_json::Map::new();
        for (name, value) in fields {
            if let Some(v) = objects.to_json(&value)? {
                map.insert(name, v);
            }
        }
        Ok(HostValue::Object(objects.add(Obj::Json(serde_json::Value::Object(map)))))
    }

    fn is_array(&mut self, value: &HostValue) -> bool {
        matches!(self.objects.borrow().get(value), Ok(Obj::Json(serde_json::Value::Array(_))))
    }

    fn array(&mut self, items: Vec<HostValue>) -> Result<HostValue> {
        let mut objects = self.objects.borrow_mut();
        let items = items.iter().map(|v| Ok(objects.to_json(v)?.unwrap_or(serde_json::Value::Null))).collect::<Result<Vec<_>>>()?;
        Ok(HostValue::Object(objects.add(Obj::Json(serde_json::Value::Array(items)))))
    }
}

/// How many trace entries are kept; older ones are dropped.
const TRACE_CAPACITY: usize = 2000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TraceKind {
    /// A call through the host.
    Call,
    /// Something the page logged (`console.log`, Dart's `print`).
    Console,
    /// A failed call, or an error the page didn't catch.
    Error,
    /// A pack loading, or being skipped.
    Load,
    /// A call into the pack (a listener, a timer, its start), with the time spent inside the
    /// pack: its own host calls are entries of their own and aren't counted.
    Pack,
}

#[derive(Clone, Debug)]
pub struct TraceEntry {
    pub pack: &'static str,
    pub kind: TraceKind,
    /// The URL of the page that made the call.
    pub page: String,
    pub call: String,
    pub args: String,
    pub result: String,
    pub micros: u64,
}

static TRACE: Mutex<VecDeque<TraceEntry>> = Mutex::new(VecDeque::new());
static TRACE_VERSION: AtomicU64 = AtomicU64::new(0);

pub fn record(entry: TraceEntry) {
    let mut trace = TRACE.lock().unwrap();
    if trace.len() == TRACE_CAPACITY {
        trace.pop_front();
    }
    trace.push_back(entry);
    drop(trace);
    TRACE_VERSION.fetch_add(1, Ordering::Relaxed);
}

/// The trace, newest first.
pub fn trace() -> Vec<TraceEntry> {
    TRACE.lock().unwrap().iter().rev().cloned().collect()
}

pub fn trace_version() -> u64 {
    TRACE_VERSION.load(Ordering::Relaxed)
}

pub fn clear_trace() {
    TRACE.lock().unwrap().clear();
    TRACE_VERSION.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;

    fn page(html: &str) -> PageHost {
        let doc = HtmlDocument::from_html(html, DocumentConfig::default()).into_inner();
        PageHost::new("test", Rc::new(RefCell::new(doc)), Default::default(), Default::default())
    }

    fn s(v: &str) -> HostValue {
        HostValue::str(v)
    }

    fn text_of(host: &PageHost, selector: &str) -> String {
        let doc = host.doc.borrow();
        let id = doc.query_selector(selector).unwrap().unwrap();
        doc.get_node(id).unwrap().text_content()
    }

    #[test]
    fn document_query_set_text_create_and_append() {
        let mut host = page("<p id=a>old</p><ul id=list></ul>");
        let document = host.global("document").unwrap();
        let a = host.call(&document, "querySelector", &[s("#a")]).unwrap();
        host.set(&a, "textContent", &s("new")).unwrap();
        assert_eq!(text_of(&host, "#a"), "new");
        assert_eq!(host.get(&a, "textContent").unwrap().display(), "new");

        let list = host.call(&document, "querySelector", &[s("#list")]).unwrap();
        let li = host.call(&document, "createElement", &[s("li")]).unwrap();
        host.set(&li, "textContent", &s("one")).unwrap();
        let returned = host.call(&list, "appendChild", &[li.clone()]).unwrap();
        assert_eq!(returned.display(), li.display());
        assert_eq!(text_of(&host, "#list li"), "one");
    }

    #[test]
    fn missing_element_is_null_and_unknown_members_error_clearly() {
        let mut host = page("<p>x</p>");
        let document = host.global("document").unwrap();
        assert!(matches!(host.call(&document, "querySelector", &[s("#nope")]).unwrap(), HostValue::Null));
        let err = host.call(&document, "fooBar", &[]).unwrap_err().to_string();
        assert_eq!(err, "Document.fooBar() is not supported yet");
        assert!(host.global("indexedDB").is_err());
    }

    #[test]
    fn same_element_gets_same_handle() {
        let mut host = page("<p id=a>x</p>");
        let document = host.global("document").unwrap();
        let first = host.call(&document, "querySelector", &[s("#a")]).unwrap();
        let again = host.call(&document, "querySelector", &[s("p")]).unwrap();
        assert_eq!(first.display(), again.display());
        assert_eq!(host.global("document").unwrap().display(), document.display());
    }

    #[test]
    fn attributes_round_trip() {
        let mut host = page("<p id=a>x</p>");
        let document = host.global("document").unwrap();
        let a = host.call(&document, "querySelector", &[s("#a")]).unwrap();
        host.call(&a, "setAttribute", &[s("data-n"), s("3")]).unwrap();
        assert_eq!(host.call(&a, "getAttribute", &[s("data-n")]).unwrap().display(), "3");
        host.call(&a, "removeAttribute", &[s("data-n")]).unwrap();
        assert!(matches!(host.call(&a, "getAttribute", &[s("data-n")]).unwrap(), HostValue::Null));
    }

    #[test]
    fn add_event_listener_registers_by_node_and_type() {
        let mut host = page("<button id=b>x</button>");
        let document = host.global("document").unwrap();
        let b = host.call(&document, "querySelector", &[s("#b")]).unwrap();
        host.call(&b, "addEventListener", &[s("click"), HostValue::CallbackId(3), HostValue::Undefined]).unwrap();
        let objects = host.objects.borrow();
        let ((_, kind), ids) = objects.listeners.iter().next().unwrap();
        assert_eq!((kind.as_str(), ids.as_slice()), ("click", &[3][..]));
    }

    #[test]
    fn math_random_is_in_range() {
        let mut host = page("<p>x</p>");
        let math = host.global("Math").unwrap();
        for _ in 0..100 {
            let n = host.call(&math, "random", &[]).unwrap().as_num().unwrap();
            assert!((0.0..1.0).contains(&n));
        }
    }

    #[test]
    fn every_call_is_traced_with_its_interface() {
        let mut host = page("<p id=a>x</p>");
        let document = host.global("document").unwrap();
        host.call(&document, "querySelector", &[s("#a")]).unwrap();
        let _ = host.call(&document, "nope", &[]);
        let trace = trace();
        assert!(trace.iter().any(|e| e.call == "Document.querySelector" && e.args.ends_with("#a")));
        assert!(trace.iter().any(|e| e.call == "Document.nope" && e.result.starts_with("ERROR")));
    }
}
