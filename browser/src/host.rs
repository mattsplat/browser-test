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
    /// The pack no longer holds `handle` (its wrapper was collected). Only values made for
    /// the pack (responses, headers, parsed data, promises) are freed; others are kept.
    fn release(&mut self, handle: u32);
    /// Whether `release` would free `handle`, so the pack knows to report it.
    fn releasable(&mut self, handle: u32) -> bool;
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
    /// A WebSocket `message`'s `data`.
    pub data: Option<String>,
    /// A WebSocket `close`'s `code`, `reason` and `wasClean`.
    pub close_code: Option<f64>,
    pub reason: Option<String>,
    pub was_clean: Option<bool>,
}

#[derive(Clone, Debug)]
pub struct EventObj {
    pub kind: String,
    /// The node it's dispatched at; for an object that isn't a node (a WebSocket), the
    /// document root, with the object in `source`.
    pub target: NodeId,
    pub source: Option<u32>,
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
    /// `querySelectorAll`, `children`, `childNodes`: a snapshot of nodes (not live).
    NodeList(Rc<Vec<NodeId>>),
    /// An element's `classList`, `style` and `dataset`: views of its attributes, one each per
    /// element (`Objects::views`), so they live as long as the element does.
    ClassList(NodeId),
    Style(NodeId),
    Dataset(NodeId),
    /// A `WebSocket`; its state is in `Objects::sockets`.
    WebSocket,
    /// IndexedDB (`idb.rs`): `indexedDB`, a request (its state in `Idb::requests`), a
    /// connection to a database, a transaction (`Idb::transactions`) and an object store in
    /// one (None: the upgrade's).
    IdbFactory,
    IdbRequest,
    IdbDatabase(String),
    IdbTransaction,
    IdbObjectStore { db: String, store: String, tx: Option<u32> },
}

/// A page's IndexedDB requests and transactions, by handle.
#[derive(Default)]
pub struct Idb {
    requests: HashMap<u32, IdbRequest>,
    transactions: HashMap<u32, IdbTransaction>,
    /// Connections whose `upgradeneeded` is under way (they may change their stores).
    upgrading: std::collections::HashSet<u32>,
    /// Connections opened. They aren't releasable (a request's `result` holds one), so
    /// they count against the limit.
    connections: usize,
}

struct IdbRequest {
    tx: Option<u32>,
    /// What `result` reads: a value or handle (an open's connection), or data, which each
    /// read hands over as a new object (so the pack releasing one read's copy can't free
    /// what a later read gets).
    result: Result<HostValue, serde_json::Value>,
    /// `error`'s name and message.
    error: Option<(String, String)>,
    done: bool,
    /// Its last event has been delivered: the pack may free it now.
    finished: bool,
    /// The connection an `open` request makes.
    opens: Option<u32>,
    /// The pack let go of it before its events: freed once they're delivered.
    release_when_done: bool,
}

struct IdbTransaction {
    db: String,
    stores: Vec<String>,
    mode: String,
    /// Requests whose events haven't been delivered: it completes after them.
    pending: usize,
    done: bool,
    finished: bool,
    release_when_done: bool,
}

/// The most IndexedDB requests and transactions a page may hold at once.
const MAX_IDB_OBJECTS: usize = 256 * 1024;

impl Obj {
    /// Objects that get events but aren't nodes: their listeners are in
    /// `Objects::target_listeners`, their `on<event>` handlers in `Objects::handlers`.
    fn is_event_target(&self) -> bool {
        matches!(self, Obj::WebSocket | Obj::IdbRequest | Obj::IdbTransaction | Obj::IdbDatabase(_))
    }

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
            Obj::NodeList(_) => "NodeList",
            Obj::ClassList(_) => "DOMTokenList",
            Obj::Style(_) => "CSSStyleDeclaration",
            Obj::Dataset(_) => "DOMStringMap",
            Obj::WebSocket => "WebSocket",
            Obj::IdbFactory => "IDBFactory",
            Obj::IdbRequest => "IDBRequest",
            Obj::IdbDatabase(_) => "IDBDatabase",
            Obj::IdbTransaction => "IDBTransaction",
            Obj::IdbObjectStore { .. } => "IDBObjectStore",
        }
    }
}

/// Whether `history.pushState` may change the page's URL from `current` to `url` (the HTML
/// spec's rule): the same scheme, username, password, host and port, so no userinfo
/// spoofing (`https://bank.test@evil.test/`), and on a `file:` page the same path too.
fn can_rewrite(current: &Url, url: &Url) -> bool {
    current.scheme() == url.scheme()
        && current.username() == url.username()
        && current.password() == url.password()
        && current.host() == url.host()
        && current.port() == url.port()
        && (url.scheme() != "file" || current.path() == url.path())
}

/// The most response body a page may hold at once (in `Response` objects it hasn't released).
const MAX_HELD_BODIES: usize = 256 << 20;

/// The most nodes a page may hold in `NodeList`s at once (about 32 MB of node ids).
const MAX_LISTED: usize = 4 << 20;

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
    /// Aborted when the page goes (the tab navigates or closes): its fetches stop.
    pub signal: Option<blitz_traits::net::AbortSignal>,
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
    pub(crate) fetching: usize,
    /// Bytes of response bodies the page holds (live `Response` objects).
    body_bytes: usize,
    /// Each response's one `Headers` object, once read.
    headers_of: HashMap<u32, u32>,
    /// Wakes the page's event loop: a timer is due, or a fetch finished.
    pub waker: std::sync::Arc<Mutex<Option<std::task::Waker>>>,
    /// `history.state` for the page's current entry, as JSON text; None until first read
    /// from the tab.
    pub history_state: Option<Option<String>>,
    /// Events the page caused (`hashchange` after setting `location.hash` on `window`, `focus`
    /// and `blur` after `element.focus()`), by target node (the root for `window`), to
    /// dispatch as a task once its code returns.
    pub queued_events: Vec<(NodeId, String, EventDetails)>,
    /// Each element's `classList`, `style` and `dataset` objects, once made.
    views: HashMap<(NodeId, &'static str), u32>,
    /// Nodes listed in the `NodeList`s the page holds, which are host memory: bounded.
    listed: usize,
    /// Listeners on objects that aren't nodes (`Obj::is_event_target`), by (handle, type).
    pub target_listeners: HashMap<(u32, String), Vec<usize>>,
    /// Their `on<event>` handlers, by (handle, event type).
    pub handlers: HashMap<(u32, String), usize>,
    /// The page's WebSockets, by handle, and how many it has made.
    pub sockets: HashMap<u32, crate::websocket::Socket>,
    sockets_made: usize,
    /// What the page's connections received, for the UI thread to deliver.
    pub inbox: crate::websocket::Inbox,
    pub idb: Idb,
    /// Events for objects that aren't nodes the page caused (an IndexedDB request's
    /// `success`), by handle, to dispatch once its code returns.
    pub queued_object_events: Vec<(u32, String)>,
}

impl Default for Objects {
    fn default() -> Self {
        let waker: std::sync::Arc<Mutex<Option<std::task::Waker>>> = Default::default();
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
            body_bytes: 0,
            headers_of: HashMap::new(),
            waker: waker.clone(),
            history_state: None,
            queued_events: Vec::new(),
            views: HashMap::new(),
            listed: 0,
            target_listeners: HashMap::new(),
            handlers: HashMap::new(),
            sockets: HashMap::new(),
            sockets_made: 0,
            inbox: crate::websocket::Inbox { waker, ..Default::default() },
            idb: Idb::default(),
            queued_object_events: Vec::new(),
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
            source: None,
            current_target: target,
            details,
            default_prevented: false,
            propagation_stopped: false,
            immediate_propagation_stopped: false,
        }))
    }

    /// An event object for an object that isn't a node (`source`), dispatched with `root`.
    pub fn object_event(&mut self, kind: &str, source: u32, root: NodeId, details: EventDetails) -> u32 {
        let handle = self.event(kind, root, details);
        if let Some(e) = self.event_mut(handle) {
            e.source = Some(source);
        }
        handle
    }

    /// What runs for event `kind` on object `target`: its `on<kind>` handler, then its
    /// listeners in order.
    pub fn object_listeners(&self, target: u32, kind: &str) -> Vec<usize> {
        let handler = self.handlers.get(&(target, kind.to_string())).copied();
        handler.into_iter().chain(self.target_listeners.get(&(target, kind.to_string())).cloned().unwrap_or_default()).collect()
    }

    /// Object `target` gets no more events (a closed socket): its listeners and handlers go.
    pub fn forget_target(&mut self, target: u32) {
        let ids: Vec<usize> = self.handlers.iter().filter(|((h, _), _)| *h == target).map(|(_, id)| *id).collect();
        self.handlers.retain(|(h, _), _| *h != target);
        let listed: Vec<usize> = self.target_listeners.iter().filter(|((h, _), _)| *h == target).flat_map(|(_, ids)| ids.clone()).collect();
        self.target_listeners.retain(|(h, _), _| *h != target);
        for id in ids.into_iter().chain(listed) {
            self.release_if_unused(id);
        }
    }

    /// Apply what happened on socket `handle` to its state. Returns the event to dispatch
    /// (type and details), if any: an `open` for a socket the page has already closed isn't.
    pub fn socket_changed(&mut self, handle: u32, event: &crate::websocket::SocketEvent) -> Option<(&'static str, EventDetails)> {
        use crate::websocket::SocketEvent;
        let socket = self.sockets.get_mut(&handle)?;
        if socket.state == 3 {
            return None;
        }
        Some(match event {
            SocketEvent::Open { protocol } => {
                if socket.state != 0 {
                    return None;
                }
                socket.state = 1;
                socket.protocol = protocol.clone();
                ("open", EventDetails::default())
            }
            SocketEvent::Message(text) => ("message", EventDetails { data: Some(text.clone()), ..Default::default() }),
            SocketEvent::Error => ("error", EventDetails::default()),
            SocketEvent::Close { code, reason, clean } => {
                socket.state = 3;
                socket.commands = None;
                ("close", EventDetails { close_code: Some(*code as f64), reason: Some(reason.clone()), was_clean: Some(*clean), ..Default::default() })
            }
        })
    }

    /// Queue event `kind` for object `target`, to dispatch once the page's code returns.
    pub(crate) fn queue_object_event(&mut self, target: u32, kind: &str) {
        self.queued_object_events.push((target, kind.to_string()));
        if let Some(w) = self.waker.lock().unwrap().as_ref() {
            w.wake_by_ref();
        }
    }

    /// Whether a queued `complete` for transaction `target` must wait for its requests.
    pub fn transaction_waiting(&self, target: u32, kind: &str) -> bool {
        kind == "complete" && self.idb.transactions.get(&target).is_some_and(|t| t.pending > 0)
    }

    /// Before an IndexedDB event is dispatched: the request it ends is done, and an open's
    /// `success` ends its upgrade.
    pub fn idb_before(&mut self, target: u32, kind: &str) {
        if let Some(r) = self.idb.requests.get_mut(&target) {
            if matches!(kind, "success" | "error") {
                r.done = true;
                if let Some(db) = r.opens {
                    self.idb.upgrading.remove(&db);
                }
            }
        }
        if let Some(t) = self.idb.transactions.get_mut(&target) {
            t.done = true;
        }
    }

    /// After an IndexedDB event: its transaction has one request fewer to wait for, and a
    /// request or transaction that's finished and that the pack let go of is freed.
    pub fn idb_after(&mut self, target: u32) {
        let (tx, free) = match (self.idb.requests.get_mut(&target), self.idb.transactions.get_mut(&target)) {
            (Some(r), _) if r.done => {
                r.finished = true;
                (r.tx, r.release_when_done)
            }
            (_, Some(t)) if t.done => {
                t.finished = true;
                (None, t.release_when_done)
            }
            _ => return,
        };
        if let Some(t) = tx.and_then(|tx| self.idb.transactions.get_mut(&tx)) {
            t.pending -= t.pending.min(1);
        }
        self.forget_target(target);
        if free {
            self.release(target);
        }
    }

    pub fn event_mut(&mut self, handle: u32) -> Option<&mut EventObj> {
        match self.objects.get_mut(&handle) {
            Some(Obj::Event(e)) => Some(e),
            _ => None,
        }
    }

    pub fn release(&mut self, handle: u32) {
        // A request or transaction whose events are still to come stays until they're done.
        if let Some(r) = self.idb.requests.get_mut(&handle).filter(|r| !r.finished) {
            r.release_when_done = true;
            return;
        }
        if let Some(t) = self.idb.transactions.get_mut(&handle).filter(|t| !t.finished) {
            t.release_when_done = true;
            return;
        }
        self.idb.requests.remove(&handle);
        self.idb.transactions.remove(&handle);
        self.idb.upgrading.remove(&handle);
        match self.objects.remove(&handle) {
            Some(Obj::Response(r)) => {
                self.body_bytes -= r.body.len().min(self.body_bytes);
                if let Some(headers) = self.headers_of.remove(&handle) {
                    self.objects.remove(&headers);
                }
            }
            Some(Obj::NodeList(nodes)) => self.listed -= nodes.len().min(self.listed),
            _ => {}
        }
        self.promises.remove(&handle);
        self.headers_of.retain(|_, h| *h != handle);
    }

    /// Whether the pack may release `handle` when it no longer holds it (`Host::release`):
    /// values made for it (responses, their headers, parsed data, promises), not the page's
    /// elements or the singletons.
    pub fn releasable(&self, handle: u32) -> bool {
        matches!(
            self.objects.get(&handle),
            Some(Obj::Promise | Obj::Response(_) | Obj::Headers(_) | Obj::Json(_) | Obj::NodeList(_) | Obj::IdbRequest | Obj::IdbTransaction | Obj::IdbObjectStore { .. })
        )
    }

    /// A `NodeList` of `nodes`, unless the page already holds too many listed nodes (host
    /// memory; Dart never releases them).
    fn node_list(&mut self, nodes: Vec<NodeId>) -> Result<HostValue> {
        if self.listed + nodes.len() > MAX_LISTED {
            bail!("the page holds over {MAX_LISTED} nodes in NodeLists; release some first");
        }
        self.listed += nodes.len();
        Ok(HostValue::Object(self.add(Obj::NodeList(Rc::new(nodes)))))
    }

    /// Element `node`'s `classList`, `style` or `dataset` (`kind`): one object per element.
    fn view(&mut self, node: NodeId, kind: &'static str) -> HostValue {
        if let Some(h) = self.views.get(&(node, kind)) {
            return HostValue::Object(*h);
        }
        let handle = self.add(match kind {
            "classList" => Obj::ClassList(node),
            "style" => Obj::Style(node),
            _ => Obj::Dataset(node),
        });
        self.views.insert((node, kind), handle);
        HostValue::Object(handle)
    }

    /// Nodes the page dropped from the document for good (their parent's `textContent` or
    /// `innerHTML` was replaced): their handles, views and listeners go, so a stale handle
    /// fails instead of reaching a new node Blitz gives the same id.
    fn forget_nodes(&mut self, dropped: &[NodeId]) {
        for node in dropped {
            if let Some(h) = self.element_handles.remove(node) {
                self.objects.remove(&h);
            }
            for kind in ["classList", "style", "dataset"] {
                if let Some(h) = self.views.remove(&(*node, kind)) {
                    self.objects.remove(&h);
                }
            }
        }
        let gone: Vec<(NodeId, String)> = self.listeners.keys().filter(|(n, _)| dropped.contains(n)).cloned().collect();
        for key in gone {
            for id in self.listeners.remove(&key).unwrap_or_default() {
                self.release_if_unused(id);
            }
        }
    }

    #[cfg(test)]
    pub fn live_objects(&self) -> usize {
        self.objects.len()
    }

    /// How many live objects are of `interface` (`"Response"`, `"Promise"`...).
    #[cfg(test)]
    pub fn live_of(&self, interface: &str) -> usize {
        self.objects.values().filter(|o| o.interface() == interface).count() + if interface == "Promise" { self.promises.len() } else { 0 }
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
        let on_objects = self.target_listeners.values().any(|ids| ids.contains(&callback)) || self.handlers.values().any(|id| *id == callback);
        if !in_listeners && !in_timers && !in_promises && !on_objects {
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
        let had_reactions = !reactions.is_empty();
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
        // Its callbacks are queued with the value: nothing needs the promise any more (each
        // pack calls `then` once per promise).
        if had_reactions {
            self.promises.remove(&promise);
            self.objects.remove(&promise);
        }
    }

    /// Fetches that finished since last asked, to settle on the UI thread.
    pub fn take_fetched(&mut self) -> Vec<Fetched> {
        let done = std::mem::take(&mut *self.fetched.lock().unwrap());
        self.fetching -= done.len().min(self.fetching);
        done
    }

    /// Settle `promise` with a finished fetch: a new `Response`, or a TypeError message.
    pub fn deliver_fetch(&mut self, promise: u32, result: Result<FetchResponse, String>) {
        let outcome = match result {
            // Bodies the page still holds are host memory, outside the pack's cap: bounded too.
            Ok(response) if self.body_bytes + response.body.len() > MAX_HELD_BODIES => {
                Err(HostValue::str(&format!("TypeError: Failed to fetch {}: the page holds over {} MB of responses", response.url, MAX_HELD_BODIES >> 20)))
            }
            Ok(response) => {
                self.body_bytes += response.body.len();
                Ok(HostValue::Object(self.add(Obj::Response(Rc::new(response)))))
            }
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
                let root = self.doc.borrow().root_node().id;
                objects.queued_events.push((root, "popstate".into(), EventDetails::default()));
                objects.queued_events.push((root, "hashchange".into(), details));
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
            request.signal = self.env.signal.clone();
            Ok(request)
        })();
        let mut objects = self.objects.borrow_mut();
        // A bad URL fails the same way every time, replayed or not.
        let request = match request {
            Ok(request) => request,
            Err(e) => {
                objects.settle(promise, Err(HostValue::str(&format!("{e:#}"))));
                return Ok(HostValue::Object(promise));
            }
        };
        // Replaying, the recorded outcome arrives as a step.
        if objects.tape.replaying() {
            return Ok(HostValue::Object(promise));
        }
        let (fetched, waker) = (objects.fetched.clone(), objects.waker.clone());
        objects.fetching += 1;
        let (gate, runtime) = match (&self.env.gate, tokio::runtime::Handle::try_current()) {
            (Some(gate), Ok(runtime)) => (gate.clone(), runtime),
            _ => {
                // No network: it fails like a request that went nowhere, delivered (and
                // recorded) as any other outcome is, so a replay matches.
                fetched.lock().unwrap().push((promise, Err("TypeError: Failed to fetch: this page has no network".into())));
                if let Some(w) = waker.lock().unwrap().as_ref() {
                    w.wake_by_ref();
                }
                return Ok(HostValue::Object(promise));
            }
        };
        runtime.spawn(async move {
            let result = gate.fetch_from_page(request, &page).await;
            fetched.lock().unwrap().push((promise, result));
            if let Some(w) = waker.lock().unwrap().as_ref() {
                w.wake_by_ref();
            }
        });
        Ok(HostValue::Object(promise))
    }

    /// Run IndexedDB operation `f` on the page's origin (`idb.rs`), changing it if `write`.
    /// Its outcome is on the tape (another tab may change the databases); a replay changes
    /// nothing and answers from the tape. The inner error is `"Name: message"`.
    fn idb_op(&mut self, write: bool, f: impl FnOnce(&mut crate::idb::Origin) -> Result<serde_json::Value, String>) -> Result<Result<serde_json::Value, String>> {
        let origin = self.storage_origin()?;
        let live = match self.objects.borrow().tape.replaying() {
            true => String::new(),
            false => match crate::idb::local().run(&origin, write, f) {
                Ok(v) => serde_json::json!({ "ok": v }),
                Err(e) => serde_json::json!({ "err": e }),
            }
            .to_string(),
        };
        let text = self.objects.borrow_mut().tape.read_text(Read::Idb, live);
        let outcome: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        Ok(match outcome.get("ok") {
            Some(v) => Ok(v.clone()),
            None => Err(outcome.get("err").and_then(|e| e.as_str()).unwrap_or("UnknownError: the operation failed").to_string()),
        })
    }

    /// A request with its outcome already known: its `success` (after `upgradeneeded`, for an
    /// open that upgrades) or `error` is queued for once the page's code returns. A replay
    /// gets them as recorded steps.
    fn idb_request(&mut self, tx: Option<u32>, outcome: Result<Result<HostValue, serde_json::Value>, String>, opens: Option<u32>, upgrade: bool) -> Result<HostValue> {
        let mut objects = self.objects.borrow_mut();
        if objects.idb.requests.len() + objects.idb.transactions.len() + objects.idb.connections >= MAX_IDB_OBJECTS {
            bail!("the page holds too many IndexedDB requests");
        }
        let handle = objects.add(Obj::IdbRequest);
        let (result, error) = match outcome {
            Ok(v) => (v, None),
            Err(e) => {
                let (name, message) = e.split_once(": ").map(|(n, m)| (n.to_string(), m.to_string())).unwrap_or(("UnknownError".into(), e.clone()));
                (Ok(HostValue::Undefined), Some((name, message)))
            }
        };
        let failed = error.is_some();
        objects.idb.requests.insert(handle, IdbRequest { tx, result, error, done: false, finished: false, opens, release_when_done: false });
        if let Some(t) = tx.and_then(|tx| objects.idb.transactions.get_mut(&tx)) {
            t.pending += 1;
        }
        if !objects.tape.replaying() {
            if upgrade {
                objects.queue_object_event(handle, "upgradeneeded");
            }
            objects.queue_object_event(handle, if failed { "error" } else { "success" });
        }
        Ok(HostValue::Object(handle))
    }

    /// `new WebSocket(url, protocols)`: a socket that connects through the gate on a thread of
    /// its own (`websocket.rs`); what happens arrives as events (`deliver_sockets`). Replaying,
    /// nothing connects: the recorded events arrive as steps.
    fn web_socket(&mut self, args: &[HostValue]) -> Result<HostValue> {
        use crate::websocket::{self, Socket, SocketEvent};
        let page = self.doc.borrow().url().clone();
        let input = args.first().context("WebSocket: missing argument 0")?.display();
        let mut url = page.join(&input).map_err(|e| anyhow::anyhow!("SyntaxError: bad URL {input:?} ({e})"))?;
        let scheme = match url.scheme() {
            "ws" | "http" => "ws",
            "wss" | "https" => "wss",
            other => bail!("SyntaxError: a WebSocket URL can't be {other}:"),
        };
        let _ = url.set_scheme(scheme);
        if url.fragment().is_some() {
            bail!("SyntaxError: a WebSocket URL can't have a fragment");
        }
        // Mixed content: a secure page's sockets are secure too.
        if page.scheme() == "https" && scheme == "ws" {
            bail!("SecurityError: an https page can't open an insecure WebSocket ({url})");
        }
        let protocols: Vec<String> = match args.get(1) {
            Some(v @ HostValue::Object(_)) => match self.objects.borrow().to_json(v)? {
                Some(serde_json::Value::Array(items)) => items.iter().map(|i| i.as_str().map(str::to_string).unwrap_or_else(|| i.to_string())).collect(),
                _ => Vec::new(),
            },
            Some(HostValue::Str(_)) => vec![args[1].display()],
            _ => Vec::new(),
        };
        let mut objects = self.objects.borrow_mut();
        if objects.sockets.values().filter(|s| s.state < 3).count() >= websocket::MAX_OPEN || objects.sockets_made >= websocket::MAX_SOCKETS {
            bail!("the page has too many WebSockets");
        }
        objects.sockets_made += 1;
        let handle = objects.add(Obj::WebSocket);
        let inbox = objects.inbox.clone();
        let commands = match (&self.env.gate, objects.tape.replaying()) {
            (_, true) => None,
            (Some(gate), false) => match gate.allow(&url, Some(&page)) {
                Ok(()) => Some(websocket::connect(handle, url.clone(), &page, protocols, inbox.clone())),
                Err(_) => None,
            },
            (None, false) => None,
        };
        // Refused or no network: it fails like a connection that went nowhere, delivered (and
        // recorded) as any other outcome is.
        if commands.is_none() && !objects.tape.replaying() {
            inbox.push(handle, SocketEvent::Error);
            inbox.push(handle, SocketEvent::Close { code: 1006, reason: String::new(), clean: false });
        }
        objects.sockets.insert(handle, Socket { url: url.to_string(), state: 0, protocol: String::new(), commands });
        Ok(HostValue::Object(handle))
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

    /// A node as the page sees it, or null.
    fn node_or_null(&mut self, node: Option<NodeId>) -> HostValue {
        match node {
            Some(id) => self.objects.borrow_mut().element(id),
            None => HostValue::Null,
        }
    }

    /// The node `id`'s parent, first or last child, or next or previous sibling (`which`, by
    /// its DOM name; the `...Element...` forms skip text).
    fn relative(&self, id: NodeId, which: &str) -> Result<Option<NodeId>> {
        let doc = self.doc.borrow();
        let node = doc.get_node(id).context("this node was removed")?;
        let elements_only = which.contains("Element") && which != "parentElement";
        let is_wanted = |n: &NodeId| !elements_only || doc.get_node(*n).is_some_and(|n| n.is_element());
        let siblings = || node.parent.and_then(|p| doc.get_node(p)).map(|p| p.children.as_slice()).unwrap_or_default();
        let position = || siblings().iter().position(|c| *c == id).unwrap_or(0);
        Ok(match which {
            "parentElement" | "parentNode" => node.parent,
            "firstChild" | "firstElementChild" => node.children.iter().copied().find(is_wanted),
            "lastChild" | "lastElementChild" => node.children.iter().rev().copied().find(is_wanted),
            "nextSibling" | "nextElementSibling" => siblings().iter().skip(position() + 1).copied().find(is_wanted),
            _ => siblings()[..position()].iter().rev().copied().find(is_wanted),
        })
    }

    /// Attribute `name` of element `id`, or empty.
    fn attr(&self, id: NodeId, name: &str) -> String {
        self.doc.borrow().get_node(id).and_then(|n| n.attr(LocalName::from(name))).unwrap_or_default().to_string()
    }

    /// Set attribute `name` of element `id` (None removes it).
    fn set_attr(&self, id: NodeId, name: &str, value: Option<&str>) {
        let name = QualName::new(None, ns!(), LocalName::from(name));
        let mut doc = self.doc.borrow_mut();
        match value {
            Some(v) => doc.mutate().set_attribute(id, name, v),
            None => doc.mutate().clear_attribute(id, name),
        }
    }

    /// Drop element `id`'s children for good, forgetting their handles (see `forget_nodes`).
    fn drop_children(&self, id: NodeId) {
        let mut dropped = Vec::new();
        let mut doc = self.doc.borrow_mut();
        let children: Vec<NodeId> = doc.get_node(id).map(|n| n.children.to_vec()).unwrap_or_default();
        let mut m = doc.mutate();
        for child in children {
            m.remove_and_drop_node_with(child, &mut |n| dropped.push(n));
        }
        drop(m);
        drop(doc);
        self.objects.borrow_mut().forget_nodes(&dropped);
    }

    /// Whether `child` may go into `parent`: not into itself or its own subtree, which would
    /// make the tree a loop (`HierarchyRequestError`), and not the document's root.
    fn check_insert(&self, parent: NodeId, child: NodeId) -> Result<()> {
        let doc = self.doc.borrow();
        if doc.node_chain(parent).contains(&child) || child == doc.root_node().id {
            bail!("HierarchyRequestError: the new child contains the parent");
        }
        Ok(())
    }

    /// Whether `child` is a child of `parent` (`NotFoundError` otherwise).
    fn check_child(&self, parent: NodeId, child: NodeId) -> Result<()> {
        if self.doc.borrow().get_node(child).and_then(|n| n.parent) != Some(parent) {
            bail!("NotFoundError: the node is not a child of this node");
        }
        Ok(())
    }

    /// Move the focus to element `to` (if it can take focus), or nowhere, as `focus()` and
    /// `blur()` do. The `blur`/`focusout` and `focus`/`focusin` events go once the page's
    /// code returns (a replay gets them as recorded steps).
    fn focus(&mut self, to: Option<NodeId>) {
        let mut doc = self.doc.borrow_mut();
        let root_element = doc.try_root_element().map(|n| n.id);
        let focused = |doc: &BaseDocument| doc.get_focussed_node_id().filter(|f| Some(*f) != root_element);
        let old = focused(&doc);
        match to {
            Some(id) if doc.get_node(id).and_then(|n| n.element_data()).is_some_and(|el| el.is_focussable) => {
                doc.set_focus_to(id);
            }
            Some(_) => return,
            None => doc.clear_focus(),
        }
        let new = focused(&doc);
        drop(doc);
        let mut objects = self.objects.borrow_mut();
        if old == new || objects.tape.replaying() {
            return;
        }
        for (node, kinds) in [(old, ["blur", "focusout"]), (new, ["focus", "focusin"])] {
            if let Some(node) = node {
                for kind in kinds {
                    objects.queued_events.push((node, kind.into(), EventDetails::default()));
                }
            }
        }
        if let Some(w) = objects.waker.lock().unwrap().as_ref() {
            w.wake_by_ref();
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
            "target" | "currentTarget" if e.source.is_some() => HostValue::Object(e.source.unwrap_or_default()),
            "target" => self.objects.borrow_mut().element(e.target),
            "currentTarget" => self.objects.borrow_mut().element(e.current_target),
            "data" if d.data.is_some() => HostValue::str(d.data.as_deref().unwrap_or_default()),
            "code" if d.close_code.is_some() => num(d.close_code),
            "reason" if d.reason.is_some() => HostValue::str(d.reason.as_deref().unwrap_or_default()),
            "wasClean" if d.was_clean.is_some() => HostValue::Bool(d.was_clean.unwrap_or_default()),
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
            (Obj::Document, "documentElement" | "head") => {
                let doc = self.doc.borrow();
                let found = match prop {
                    "head" => doc.find_head_node().map(|n| n.id),
                    _ => doc.try_root_element().map(|n| n.id),
                };
                drop(doc);
                self.node_or_null(found)
            }
            (Obj::Element(id), "children" | "childNodes") => {
                let doc = self.doc.borrow();
                let node = doc.get_node(*id).context("this node was removed")?;
                let nodes = node.children.iter().copied().filter(|c| prop == "childNodes" || doc.get_node(*c).is_some_and(|n| n.is_element())).collect();
                drop(doc);
                self.objects.borrow_mut().node_list(nodes)?
            }
            (Obj::Element(id), "parentElement" | "parentNode" | "firstChild" | "lastChild" | "nextSibling" | "previousSibling"
                | "firstElementChild" | "lastElementChild" | "nextElementSibling" | "previousElementSibling") => {
                let found = self.relative(*id, prop)?;
                // The document node is `parentNode` of the root element, never `parentElement`.
                if found == Some(self.doc.borrow().root_node().id) {
                    return Ok(match prop {
                        "parentNode" => self.objects.borrow_mut().singleton("document", Obj::Document),
                        _ => HostValue::Null,
                    });
                }
                self.node_or_null(found)
            }
            (Obj::Element(id), "childElementCount") => {
                let doc = self.doc.borrow();
                let node = doc.get_node(*id).context("this node was removed")?;
                HostValue::Num(node.children.iter().filter(|c| doc.get_node(**c).is_some_and(|n| n.is_element())).count() as f64)
            }
            (Obj::Element(id), "nodeType" | "nodeName" | "nodeValue" | "data") => {
                let doc = self.doc.borrow();
                let node = doc.get_node(*id).context("this node was removed")?;
                match (prop, node.element_data(), node.text_data()) {
                    ("nodeType", Some(_), _) => HostValue::Num(1.0),
                    ("nodeType", _, Some(_)) => HostValue::Num(3.0),
                    ("nodeType", _, _) => HostValue::Num(8.0),
                    ("nodeName", Some(el), _) => HostValue::str(&el.name.local.to_ascii_uppercase()),
                    ("nodeName", _, Some(_)) => HostValue::str("#text"),
                    ("nodeName", _, _) => HostValue::str("#comment"),
                    (_, _, Some(text)) => HostValue::str(&text.content),
                    ("nodeValue", _, _) => HostValue::Null,
                    _ => HostValue::Undefined,
                }
            }
            (Obj::Element(id), "className") => HostValue::str(self.doc.borrow().get_node(*id).and_then(|n| n.attr(LocalName::from("class"))).unwrap_or("")),
            (Obj::Element(id), "classList" | "style" | "dataset") => {
                let kind = match prop {
                    "classList" => "classList",
                    "style" => "style",
                    _ => "dataset",
                };
                self.element(target)?;
                self.objects.borrow_mut().view(*id, kind)
            }
            (Obj::Element(id), "innerHTML" | "outerHTML") => {
                let doc = self.doc.borrow();
                doc.get_node(*id).context("this node was removed")?;
                HostValue::str(&serialize(&doc, *id, prop == "outerHTML"))
            }
            // A form field's state: the user changes it by typing and clicking, which a replay
            // doesn't repeat, so reads are on the tape.
            (Obj::Element(id), "value" | "checked") => {
                ensure_field_state(&mut self.doc.borrow_mut(), *id);
                let doc = self.doc.borrow();
                let el = doc.get_node(*id).and_then(|n| n.element_data()).context("not an element")?;
                let live = match prop {
                    "checked" => el.checkbox_input_checked().unwrap_or(false).to_string(),
                    _ => field_value(el),
                };
                drop(doc);
                let text = self.objects.borrow_mut().tape.read_text(Read::Field, live);
                match prop {
                    "checked" => HostValue::Bool(text == "true"),
                    _ => HostValue::str(&text),
                }
            }
            (Obj::Element(id), "disabled") => HostValue::Bool(self.doc.borrow().get_node(*id).and_then(|n| n.attr(LocalName::from("disabled"))).is_some()),
            (Obj::NodeList(nodes), _) => match prop {
                "length" => HostValue::Num(nodes.len() as f64),
                _ => match prop.parse::<usize>().ok().and_then(|i| nodes.get(i)) {
                    Some(node) => self.objects.borrow_mut().element(*node),
                    None => HostValue::Undefined,
                },
            },
            (Obj::ClassList(id), "length" | "value") => {
                let classes = self.attr(*id, "class");
                match prop {
                    "length" => HostValue::Num(classes.split_ascii_whitespace().count() as f64),
                    _ => HostValue::str(&classes),
                }
            }
            (Obj::Dataset(id), _) => match self.doc.borrow().get_node(*id).and_then(|n| n.attr(LocalName::from(data_attribute(prop).as_str()))) {
                Some(v) => HostValue::str(v),
                None => HostValue::Undefined,
            },
            (Obj::Style(id), _) => {
                let declarations = parse_style(&self.attr(*id, "style"));
                match prop {
                    "cssText" => HostValue::str(&write_style(&declarations)),
                    "length" => HostValue::Num(declarations.len() as f64),
                    _ => HostValue::str(&style_value(&declarations, &kebab(prop))),
                }
            }
            (Obj::Window, "indexedDB") => self.objects.borrow_mut().singleton("indexedDB", Obj::IdbFactory),
            (Obj::IdbRequest | Obj::IdbTransaction, _) => {
                let HostValue::Object(handle) = target else { bail!("not an object") };
                let objects = self.objects.borrow();
                let (request, transaction) = (objects.idb.requests.get(handle), objects.idb.transactions.get(handle));
                match (prop, request, transaction) {
                    ("result", Some(r), _) => match r.result.clone() {
                        Ok(v) => v,
                        Err(data) => {
                            drop(objects);
                            self.objects.borrow_mut().from_json(data)
                        }
                    },
                    ("error", Some(r), _) => match r.error.clone() {
                        Some((name, message)) => {
                            drop(objects);
                            self.objects.borrow_mut().from_json(serde_json::json!({"name": name, "message": message}))
                        }
                        None => HostValue::Null,
                    },
                    ("readyState", Some(r), _) => HostValue::str(if r.done { "done" } else { "pending" }),
                    ("mode", _, Some(t)) => HostValue::str(&t.mode),
                    (p, _, _) if p.starts_with("on") => HostValue::Null,
                    _ => bail!("{}.{prop} is not supported yet", obj.interface()),
                }
            }
            (Obj::IdbDatabase(name), _) => match prop {
                "name" => HostValue::str(name),
                "version" => match self.idb_op(false, |o| Ok(o.version(name).into()))? {
                    Ok(v) => self.objects.borrow_mut().from_json(v),
                    Err(e) => bail!("{e}"),
                },
                "objectStoreNames" => match self.idb_op(false, |o| Ok(o.store_names(name).into()))? {
                    Ok(v) => self.objects.borrow_mut().from_json(v),
                    Err(e) => bail!("{e}"),
                },
                p if p.starts_with("on") => HostValue::Null,
                _ => bail!("IDBDatabase.{prop} is not supported yet"),
            },
            (Obj::IdbObjectStore { db, store, .. }, _) => match prop {
                "name" => HostValue::str(store),
                "keyPath" | "autoIncrement" => {
                    let info = self.idb_op(false, |o| o.store_info(db, store).map(|(path, auto)| serde_json::json!([path, auto])))?;
                    let info = info.map_err(|e| anyhow::anyhow!(e))?;
                    self.objects.borrow_mut().from_json(info[if prop == "keyPath" { 0 } else { 1 }].clone())
                }
                _ => bail!("IDBObjectStore.{prop} is not supported yet"),
            },
            (Obj::WebSocket, _) => {
                let HostValue::Object(handle) = target else { bail!("not a socket") };
                let objects = self.objects.borrow();
                let socket = objects.sockets.get(handle).context("this socket is no longer available")?;
                match prop {
                    "url" => HostValue::str(&socket.url),
                    "readyState" => HostValue::Num(socket.state as f64),
                    "protocol" => HostValue::str(&socket.protocol),
                    "extensions" => HostValue::str(""),
                    "bufferedAmount" => HostValue::Num(0.0),
                    "binaryType" => HostValue::str("blob"),
                    "CONNECTING" => HostValue::Num(0.0),
                    "OPEN" => HostValue::Num(1.0),
                    "CLOSING" => HostValue::Num(2.0),
                    "CLOSED" => HostValue::Num(3.0),
                    p if p.starts_with("on") => HostValue::Null,
                    _ => bail!("WebSocket.{prop} is not supported yet"),
                }
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
                "headers" => {
                    let HostValue::Object(response) = target else { bail!("not a response") };
                    let mut objects = self.objects.borrow_mut();
                    let handle = match objects.headers_of.get(response) {
                        Some(h) => *h,
                        None => {
                            let h = objects.add(Obj::Headers(Rc::new(r.headers.clone())));
                            objects.headers_of.insert(*response, h);
                            h
                        }
                    };
                    HostValue::Object(handle)
                }
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
            // `socket.onmessage = f`: one handler per event type, run before the listeners.
            (o, p) if o.is_event_target() && p.starts_with("on") => {
                let HostValue::Object(handle) = target else { bail!("not an object") };
                let key = (*handle, p["on".len()..].to_string());
                let mut objects = self.objects.borrow_mut();
                let old = match value {
                    HostValue::CallbackId(id) => objects.handlers.insert(key, *id),
                    _ => objects.handlers.remove(&key),
                };
                if let Some(old) = old.filter(|old| Some(old) != match value { HostValue::CallbackId(id) => Some(id), _ => None }) {
                    objects.release_if_unused(old);
                }
                return Ok(());
            }
            (Obj::WebSocket, "binaryType") => return Ok(()),
            (Obj::Dataset(id), _) => {
                self.set_attr(*id, &data_attribute(prop), Some(&value.display()));
                return Ok(());
            }
            (Obj::Style(id), _) => {
                let text = match prop {
                    "cssText" => write_style(&parse_style(&value.display())),
                    _ => {
                        let mut declarations = parse_style(&self.attr(*id, "style"));
                        set_style(&mut declarations, &kebab(prop), &value.display());
                        write_style(&declarations)
                    }
                };
                self.set_attr(*id, "style", Some(&text));
                return Ok(());
            }
            (Obj::ClassList(id), "value") => {
                self.set_attr(*id, "class", Some(&value.display()));
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
        match prop {
            // Replaces the element's children with one text node.
            "textContent" => {
                self.drop_children(id);
                let mut doc = self.doc.borrow_mut();
                let mut m = doc.mutate();
                let text = m.create_text_node(&value.display());
                m.append_children(id, &[text]);
            }
            // Parsed by Blitz's HTML parser. Nothing in it runs: a page picks its pack and runs
            // its scripts once, at load, so an inserted `<script>` is inert, as in browsers.
            // What it loads (images, stylesheets) goes through the page's gate like any other
            // subresource.
            "innerHTML" => {
                self.drop_children(id);
                let mut doc = self.doc.borrow_mut();
                let mut m = doc.mutate();
                blitz_html::DocumentHtmlParser::parse_inner_html_into_mutator(&mut m, id, &value.display());
            }
            "id" | "className" => {
                let attr = if prop == "id" { "id" } else { "class" };
                self.set_attr(id, attr, Some(&value.display()));
            }
            "disabled" => self.set_attr(id, "disabled", value.truthy().then_some("")),
            "value" => {
                let mut doc = self.doc.borrow_mut();
                ensure_field_state(&mut doc, id);
                let el = doc.get_node_mut(id).and_then(|n| n.element_data_mut()).context("not an element")?;
                match el.text_input_data_mut() {
                    // The caret goes to the end, as in browsers.
                    Some(input) => {
                        input.editor.set_text(&value.display());
                        doc.with_text_input(id, |mut driver| driver.move_to_text_end());
                    }
                    // Not a text field: the attribute is its value.
                    None => {
                        drop(doc);
                        self.set_attr(id, "value", Some(&value.display()));
                    }
                }
            }
            "checked" => {
                let mut doc = self.doc.borrow_mut();
                ensure_field_state(&mut doc, id);
                if let Some(el) = doc.get_node_mut(id).and_then(|n| n.element_data_mut()) {
                    el.set_checkbox_input_checked(value.truthy());
                }
                doc.shell_provider.request_redraw();
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
            (o, "addEventListener" | "removeEventListener") if o.is_event_target() => {
                let HostValue::Object(handle) = target else { bail!("not an object") };
                let (kind, id) = (text(0)?, callback(1)?);
                let mut objects = self.objects.borrow_mut();
                let ids = objects.target_listeners.entry((*handle, kind)).or_default();
                match method {
                    "addEventListener" if !ids.contains(&id) => ids.push(id),
                    "addEventListener" => {}
                    _ => {
                        ids.retain(|l| *l != id);
                        objects.release_if_unused(id);
                    }
                }
                HostValue::Undefined
            }
            (Obj::Window, "WebSocket") => self.web_socket(args)?,
            (Obj::IdbFactory, "open") => {
                let name = text(0)?;
                let version = args.get(1).and_then(HostValue::as_num);
                let outcome = self.idb_op(true, |o| o.open(&name, version).map(|(old, new)| serde_json::json!([old, new])))?;
                match outcome {
                    Ok(versions) => {
                        let mut objects = self.objects.borrow_mut();
                        objects.idb.connections += 1;
                        let db = objects.add(Obj::IdbDatabase(name));
                        drop(objects);
                        let upgrade = versions[0].as_u64() < versions[1].as_u64();
                        if upgrade {
                            self.objects.borrow_mut().idb.upgrading.insert(db);
                        }
                        self.idb_request(None, Ok(Ok(HostValue::Object(db))), Some(db), upgrade)?
                    }
                    Err(e) => self.idb_request(None, Err(e), None, false)?,
                }
            }
            (Obj::IdbFactory, "deleteDatabase") => {
                let name = text(0)?;
                let outcome = self.idb_op(true, |o| Ok(o.delete_database(&name)).map(|_| serde_json::Value::Null))?;
                self.idb_request(None, outcome.map(|_| Ok(HostValue::Undefined)), None, false)?
            }
            (Obj::IdbDatabase(db), "createObjectStore" | "deleteObjectStore") => {
                let HostValue::Object(handle) = target else { bail!("not a database") };
                if !self.objects.borrow().idb.upgrading.contains(handle) {
                    bail!("InvalidStateError: object stores can only change during upgradeneeded");
                }
                let name = text(0)?;
                let options = match args.get(1) {
                    Some(v @ HostValue::Object(_)) => self.objects.borrow().to_json(v)?.unwrap_or_default(),
                    _ => serde_json::Value::Null,
                };
                let key_path = options.get("keyPath").and_then(|k| k.as_str()).map(str::to_string);
                let auto_increment = options.get("autoIncrement").and_then(|a| a.as_bool()).unwrap_or(false);
                let outcome = self.idb_op(true, |o| match method {
                    "createObjectStore" => o.create_store(db, &name, key_path, auto_increment),
                    _ => o.delete_store(db, &name),
                }
                .map(|_| serde_json::Value::Null))?;
                outcome.map_err(|e| anyhow::anyhow!(e))?;
                match method {
                    "createObjectStore" => HostValue::Object(self.objects.borrow_mut().add(Obj::IdbObjectStore { db: db.clone(), store: name, tx: None })),
                    _ => HostValue::Undefined,
                }
            }
            (Obj::IdbDatabase(db), "transaction") => {
                let stores: Vec<String> = match args.first() {
                    Some(v @ HostValue::Object(_)) => match self.objects.borrow().to_json(v)? {
                        Some(serde_json::Value::Array(items)) => items.iter().filter_map(|i| i.as_str().map(str::to_string)).collect(),
                        _ => Vec::new(),
                    },
                    Some(v) => vec![v.display()],
                    None => Vec::new(),
                };
                let mode = match args.get(1) {
                    Some(HostValue::Undefined) | None => "readonly".to_string(),
                    Some(m) if matches!(m.display().as_str(), "readonly" | "readwrite") => m.display(),
                    Some(m) => bail!("TypeError: {:?} isn't a transaction mode", m.display()),
                };
                let names = self.idb_op(false, |o| Ok(o.store_names(db).into()))?.map_err(|e| anyhow::anyhow!(e))?;
                if stores.is_empty() {
                    bail!("InvalidAccessError: a transaction needs at least one object store");
                }
                if let Some(missing) = stores.iter().find(|s| !names.as_array().is_some_and(|n| n.iter().any(|n| n == *s))) {
                    bail!("NotFoundError: there's no object store {missing:?}");
                }
                let mut objects = self.objects.borrow_mut();
                if objects.idb.requests.len() + objects.idb.transactions.len() + objects.idb.connections >= MAX_IDB_OBJECTS {
                    bail!("the page holds too many IndexedDB requests");
                }
                let tx = objects.add(Obj::IdbTransaction);
                objects.idb.transactions.insert(tx, IdbTransaction { db: db.clone(), stores, mode, pending: 0, done: false, finished: false, release_when_done: false });
                // It completes once its requests' events have gone (`transaction_waiting`).
                if !objects.tape.replaying() {
                    objects.queue_object_event(tx, "complete");
                }
                HostValue::Object(tx)
            }
            (Obj::IdbDatabase(_), "close") => HostValue::Undefined,
            (Obj::IdbTransaction, "objectStore") => {
                let HostValue::Object(handle) = target else { bail!("not a transaction") };
                let name = text(0)?;
                let mut objects = self.objects.borrow_mut();
                let t = objects.idb.transactions.get(handle).context("this transaction is no longer available")?;
                if t.done {
                    bail!("InvalidStateError: the transaction has finished");
                }
                if !t.stores.contains(&name) {
                    bail!("NotFoundError: {name:?} isn't in this transaction");
                }
                let db = t.db.clone();
                HostValue::Object(objects.add(Obj::IdbObjectStore { db, store: name, tx: Some(*handle) }))
            }
            (Obj::IdbObjectStore { db, store, tx }, "put" | "add" | "get" | "delete" | "getAll" | "count" | "clear") => {
                let write = matches!(method, "put" | "add" | "delete" | "clear");
                if let Some(t) = tx.and_then(|tx| self.objects.borrow().idb.transactions.get(&tx).map(|t| (t.done, t.mode.clone()))) {
                    if t.0 {
                        bail!("TransactionInactiveError: the transaction has finished");
                    }
                    if write && t.1 == "readonly" {
                        bail!("ReadOnlyError: the transaction is read-only");
                    }
                }
                let data = |i: usize| -> Result<Option<serde_json::Value>> {
                    match args.get(i) {
                        Some(v) => self.objects.borrow().to_json(v),
                        None => Ok(None),
                    }
                };
                let (first, second) = (data(0)?, data(1)?);
                let outcome = self.idb_op(write, |o| match method {
                    "put" | "add" => o.put(db, store, first.unwrap_or_default(), second, method == "add"),
                    "get" => o.get(db, store, &first.unwrap_or_default()).map(|v| v.unwrap_or(serde_json::json!({"u": 1}))),
                    "delete" => o.delete(db, store, &first.unwrap_or_default()).map(|_| serde_json::json!({"u": 1})),
                    "getAll" => o.get_all(db, store).map(serde_json::Value::from),
                    "count" => o.count(db, store).map(serde_json::Value::from),
                    _ => o.clear(db, store).map(|_| serde_json::json!({"u": 1})),
                })?;
                // `{"u": 1}` stands for undefined (a missing record), which JSON can't say.
                let result = outcome.map(|v| match v {
                    serde_json::Value::Object(ref o) if o.len() == 1 && o.get("u") == Some(&serde_json::json!(1)) => Ok(HostValue::Undefined),
                    v @ (serde_json::Value::Object(_) | serde_json::Value::Array(_)) => Err(v),
                    v => Ok(self.objects.borrow_mut().from_json(v)),
                });
                self.idb_request(*tx, result, None, false)?
            }
            (Obj::WebSocket, "send" | "close") => {
                let HostValue::Object(handle) = target else { bail!("not a socket") };
                let mut objects = self.objects.borrow_mut();
                let socket = objects.sockets.get_mut(handle).context("this socket is no longer available")?;
                match (method, socket.state) {
                    ("send", 0) => bail!("InvalidStateError: the WebSocket is still connecting"),
                    ("send", 1) => {
                        let data = text(0)?;
                        if data.len() > crate::websocket::MAX_MESSAGE {
                            bail!("the message is over {} MB", crate::websocket::MAX_MESSAGE >> 20);
                        }
                        if let Some(commands) = &socket.commands {
                            let _ = commands.send(crate::websocket::Command::Send(data));
                        }
                    }
                    // Closing or closed: sends are dropped, as in browsers.
                    ("send", _) | ("close", 2 | 3) => {}
                    _ => {
                        let code = match args.first() {
                            Some(HostValue::Num(code)) if *code == 1000.0 || (3000.0..5000.0).contains(code) => *code as u16,
                            None | Some(HostValue::Undefined) => 1000,
                            Some(other) => bail!("InvalidAccessError: {} isn't a close code a page may use", other.display()),
                        };
                        let reason = args.get(1).filter(|v| !matches!(v, HostValue::Undefined)).map(HostValue::display).unwrap_or_default();
                        if reason.len() > 123 {
                            bail!("SyntaxError: the close reason is over 123 bytes");
                        }
                        socket.state = 2;
                        if let Some(commands) = &socket.commands {
                            let _ = commands.send(crate::websocket::Command::Close { code, reason });
                        }
                    }
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
                self.check_insert(*parent, child)?;
                self.doc.borrow_mut().mutate().append_children(*parent, &[child]);
                child_value.clone()
            }
            (Obj::Document, "getElementById") => {
                let found = self.doc.borrow().get_element_by_id(&text(0)?);
                self.node_or_null(found)
            }
            (Obj::Document | Obj::Element(_), "querySelectorAll") => {
                let scope = self.scope(&obj).context("no scope")?;
                let selector = text(0)?;
                let found = self.doc.borrow().query_selector_all_in(scope, &selector).map_err(|e| anyhow::anyhow!("SyntaxError: bad selector {selector:?}: {e:?}"))?;
                self.objects.borrow_mut().node_list(found.into_iter().collect())?
            }
            (Obj::Document, "createTextNode") => {
                let id = self.doc.borrow_mut().mutate().create_text_node(&text(0)?);
                self.objects.borrow_mut().element(id)
            }
            (Obj::Element(parent), "insertBefore") => {
                let new_value = args.first().context("insertBefore: missing node")?;
                let new = self.element(new_value)?;
                self.check_insert(*parent, new)?;
                match args.get(1) {
                    Some(r @ HostValue::Object(_)) => {
                        let anchor = self.element(r)?;
                        self.check_child(*parent, anchor)?;
                        if anchor != new {
                            self.doc.borrow_mut().mutate().insert_nodes_before(anchor, &[new]);
                        }
                    }
                    _ => self.doc.borrow_mut().mutate().append_children(*parent, &[new]),
                }
                new_value.clone()
            }
            (Obj::Element(parent), "replaceChild") => {
                let (new, old) = (self.element(args.first().context("replaceChild: missing node")?)?, self.element(args.get(1).context("replaceChild: missing child")?)?);
                self.check_insert(*parent, new)?;
                self.check_child(*parent, old)?;
                if new != old {
                    self.doc.borrow_mut().mutate().replace_node_with(old, &[new]);
                }
                args[1].clone()
            }
            (Obj::Element(parent), "removeChild") => {
                let child = self.element(args.first().context("removeChild: missing child")?)?;
                self.check_child(*parent, child)?;
                self.doc.borrow_mut().mutate().remove_node(child);
                args[0].clone()
            }
            (Obj::Element(id), "cloneNode") => {
                let deep = args.first().is_some_and(HostValue::truthy);
                let mut doc = self.doc.borrow_mut();
                let mut m = doc.mutate();
                let copy = match (deep, m.element_name(*id).cloned()) {
                    (true, _) => m.deep_clone_node(*id),
                    (false, Some(name)) => {
                        let attrs = m.doc.get_node(*id).and_then(|n| n.element_data()).map(|el| el.attrs().to_vec()).unwrap_or_default();
                        m.create_element(name, attrs)
                    }
                    (false, None) => {
                        let text = m.doc.get_node(*id).and_then(|n| n.text_data()).map(|t| t.content.clone()).unwrap_or_default();
                        m.create_text_node(&text)
                    }
                };
                drop(m);
                drop(doc);
                self.objects.borrow_mut().element(copy)
            }
            (Obj::Element(id), "hasAttribute") => {
                let name = text(0)?;
                HostValue::Bool(self.doc.borrow().get_node(*id).and_then(|n| n.attr(LocalName::from(name.as_str()))).is_some())
            }
            (Obj::Element(id), "contains") => match args.first() {
                Some(v @ HostValue::Object(_)) => {
                    let other = self.element(v)?;
                    HostValue::Bool(self.doc.borrow().node_chain(other).contains(id))
                }
                _ => HostValue::Bool(false),
            },
            // From the layout Blitz last computed (the last frame): no layout is forced, so
            // right after a change it's out of date. On the tape: layout depends on the window
            // and the fonts.
            (Obj::Element(id), "getBoundingClientRect") => {
                let doc = self.doc.borrow();
                let node = doc.get_node(*id).context("this node was removed")?;
                let (at, size, scroll) = (node.absolute_position(0.0, 0.0), node.final_layout().size, doc.viewport_scroll());
                let (x, y) = (at.x as f64 - scroll.x, at.y as f64 - scroll.y);
                let (w, h) = (size.width as f64, size.height as f64);
                drop(doc);
                let live = serde_json::json!({"x": x, "y": y, "width": w, "height": h, "left": x, "top": y, "right": x + w, "bottom": y + h});
                let mut objects = self.objects.borrow_mut();
                let text = objects.tape.read_text(Read::Layout, live.to_string());
                objects.from_json(serde_json::from_str(&text).unwrap_or(serde_json::Value::Null))
            }
            (Obj::Element(id), "focus") => {
                self.focus(Some(*id));
                HostValue::Undefined
            }
            (Obj::Element(id), "blur") => {
                if self.doc.borrow().get_focussed_node_id() == Some(*id) {
                    self.focus(None);
                }
                HostValue::Undefined
            }
            (Obj::NodeList(nodes), "item") => match args.first().and_then(|v| v.as_num()).filter(|i| *i >= 0.0).and_then(|i| nodes.get(i as usize)) {
                Some(node) => self.objects.borrow_mut().element(*node),
                None => HostValue::Null,
            },
            (Obj::ClassList(id), "add" | "remove" | "toggle" | "contains" | "item") => {
                let mut classes: Vec<String> = self.attr(*id, "class").split_ascii_whitespace().map(str::to_string).collect();
                let before = classes.clone();
                let result = match method {
                    "contains" => HostValue::Bool(classes.contains(&text(0)?)),
                    "item" => match args.first().and_then(|v| v.as_num()).filter(|i| *i >= 0.0).and_then(|i| classes.get(i as usize)) {
                        Some(c) => HostValue::str(c),
                        None => HostValue::Null,
                    },
                    "toggle" => {
                        let token = text(0)?;
                        let on = match args.get(1) {
                            Some(HostValue::Undefined) | None => !classes.contains(&token),
                            Some(force) => force.truthy(),
                        };
                        match on {
                            true if !classes.contains(&token) => classes.push(token),
                            true => {}
                            false => classes.retain(|c| *c != token),
                        }
                        HostValue::Bool(on)
                    }
                    _ => {
                        for token in args.iter().map(HostValue::display) {
                            if token.is_empty() || token.contains(char::is_whitespace) {
                                bail!("SyntaxError: {token:?} isn't a valid class name");
                            }
                            match method {
                                "add" if !classes.contains(&token) => classes.push(token),
                                "add" => {}
                                _ => classes.retain(|c| *c != token),
                            }
                        }
                        HostValue::Undefined
                    }
                };
                if classes != before {
                    self.set_attr(*id, "class", Some(&classes.join(" ")));
                }
                result
            }
            (Obj::Style(id), "getPropertyValue") => HostValue::str(&style_value(&parse_style(&self.attr(*id, "style")), &text(0)?.to_ascii_lowercase())),
            (Obj::Style(id), "setProperty" | "removeProperty") => {
                let mut declarations = parse_style(&self.attr(*id, "style"));
                let name = text(0)?.to_ascii_lowercase();
                let old = style_value(&declarations, &name);
                let value = match (method, args.get(1)) {
                    ("setProperty", Some(v)) if !matches!(v, HostValue::Null | HostValue::Undefined) => v.display(),
                    _ => String::new(),
                };
                let important = args.get(2).is_some_and(|p| p.display().eq_ignore_ascii_case("important"));
                set_style(&mut declarations, &name, &if important && !value.is_empty() { format!("{value} !important") } else { value });
                self.set_attr(*id, "style", Some(&write_style(&declarations)));
                match method {
                    "removeProperty" => HostValue::str(&old),
                    _ => HostValue::Undefined,
                }
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
                if !can_rewrite(&current, &url) {
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
            (Obj::Json(serde_json::Value::Array(_)), "push") => {
                let items = args.iter().map(|v| Ok(self.objects.borrow().to_json(v)?.unwrap_or(serde_json::Value::Null))).collect::<Result<Vec<_>>>()?;
                let HostValue::Object(handle) = target else { bail!("not an array") };
                let mut objects = self.objects.borrow_mut();
                let Some(Obj::Json(serde_json::Value::Array(array))) = objects.objects.get_mut(handle) else { bail!("not an array") };
                array.extend(items);
                HostValue::Num(array.len() as f64)
            }
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

// ---- DOM helpers: serialising, form fields, inline styles, `dataset` names

/// Elements with no end tag, which never have children in HTML.
const VOID_ELEMENTS: &[&str] = &["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track", "wbr"];

/// Node `id` as HTML, as `outerHTML` (`outer`) or `innerHTML` serialise it.
fn serialize(doc: &BaseDocument, id: NodeId, outer: bool) -> String {
    fn escape(text: &str, attribute: bool, out: &mut String) {
        for c in text.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '\u{a0}' => out.push_str("&nbsp;"),
                '"' if attribute => out.push_str("&quot;"),
                '<' if !attribute => out.push_str("&lt;"),
                '>' if !attribute => out.push_str("&gt;"),
                c => out.push(c),
            }
        }
    }
    fn write(doc: &BaseDocument, id: NodeId, out: &mut String) {
        let Some(node) = doc.get_node(id) else { return };
        if let Some(text) = node.text_data() {
            let raw = node.parent.and_then(|p| doc.get_node(p)).and_then(|p| p.element_data()).is_some_and(|el| {
                matches!(el.name.local.as_ref(), "style" | "script" | "xmp" | "iframe" | "noembed" | "noframes" | "plaintext")
            });
            match raw {
                true => out.push_str(&text.content),
                false => escape(&text.content, false, out),
            }
        } else if let Some(el) = node.element_data() {
            let tag = el.name.local.as_ref();
            out.push('<');
            out.push_str(tag);
            for attr in el.attrs() {
                out.push(' ');
                out.push_str(&attr.name.local);
                out.push_str("=\"");
                escape(&attr.value, true, out);
                out.push('"');
            }
            out.push('>');
            if VOID_ELEMENTS.contains(&tag) {
                return;
            }
            for child in &node.children {
                write(doc, *child, out);
            }
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
    }
    let mut out = String::new();
    match outer {
        true => write(doc, id, &mut out),
        false => {
            if let Some(node) = doc.get_node(id) {
                for child in &node.children {
                    write(doc, *child, &mut out);
                }
            }
        }
    }
    out
}

/// Blitz makes a form field's state (a text field's editor, a checkbox's checked state) when
/// it first lays the field out, but page code can reach the field before that: make it the
/// same way first (as `blitz_dom`'s layout construction does), and layout keeps it.
fn ensure_field_state(doc: &mut BaseDocument, id: NodeId) {
    use blitz_dom::node::{SpecialElementData, TextInputData};
    let Some(node) = doc.get_node_mut(id) else { return };
    let text = node.text_content();
    let Some(el) = node.element_data_mut() else { return };
    if !matches!(el.special_data, SpecialElementData::None) {
        return;
    }
    let text_field = |initial: &str, multiline: bool| {
        let mut data = TextInputData::new(multiline);
        data.editor.set_text(initial);
        SpecialElementData::TextInput(data)
    };
    el.special_data = match (el.name.local.as_ref(), el.attr(LocalName::from("type"))) {
        ("textarea", _) => text_field(&text, true),
        ("input", None | Some("text" | "password" | "email" | "number" | "search" | "tel" | "url")) => {
            let value = el.attr(LocalName::from("value")).unwrap_or_default().to_string();
            text_field(&value, false)
        }
        ("input", Some("checkbox" | "radio")) => SpecialElementData::CheckboxInput(el.attr(LocalName::from("checked")).is_some()),
        _ => return,
    };
}

/// A form field's `value`: a text field's text, otherwise its `value` attribute (a checkbox's
/// defaults to "on").
pub(crate) fn field_value(el: &blitz_dom::node::ElementData) -> String {
    if let Some(input) = el.text_input_data() {
        return input.editor.raw_text().to_string();
    }
    match el.attr(LocalName::from("value")) {
        Some(v) => v.to_string(),
        None if el.checkbox_input_checked().is_some() => "on".into(),
        None => String::new(),
    }
}

/// A camelCase name as CSS or a `data-` attribute writes it: `backgroundColor` →
/// `background-color`.
fn kebab(name: &str) -> String {
    if name == "cssFloat" {
        return "float".into();
    }
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            out.push('-');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The attribute behind `dataset.<name>`: `userId` → `data-user-id`.
fn data_attribute(name: &str) -> String {
    format!("data-{}", kebab(name))
}

/// An inline style attribute's declarations, in order, as written: `(name, value)`. Values
/// keep `!important`. (A `;` inside a quoted string or `url(...)` would split it: rare in
/// inline styles.)
fn parse_style(text: &str) -> Vec<(String, String)> {
    text.split(';')
        .filter_map(|d| d.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .filter(|(n, v)| !n.is_empty() && !v.is_empty())
        .collect()
}

/// Declarations back as `cssText` writes them: `color: red; width: 10px;`.
fn write_style(declarations: &[(String, String)]) -> String {
    declarations.iter().map(|(n, v)| format!("{n}: {v};")).collect::<Vec<_>>().join(" ")
}

/// `getPropertyValue`: the value without `!important`, or empty.
fn style_value(declarations: &[(String, String)], name: &str) -> String {
    let value = declarations.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v.as_str()).unwrap_or_default();
    value.trim_end_matches("!important").trim_end().to_string()
}

/// Set (or, with an empty value, remove) one declaration, keeping its place.
fn set_style(declarations: &mut Vec<(String, String)>, name: &str, value: &str) {
    match (declarations.iter().position(|(n, _)| n == name), value.trim().is_empty()) {
        (Some(i), false) => declarations[i].1 = value.trim().to_string(),
        (None, false) => declarations.push((name.to_string(), value.trim().to_string())),
        (_, true) => declarations.retain(|(n, _)| n != name),
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

    fn release(&mut self, handle: u32) {
        let mut objects = self.objects.borrow_mut();
        if objects.releasable(handle) {
            objects.release(handle);
        }
    }

    fn releasable(&mut self, handle: u32) -> bool {
        self.objects.borrow().releasable(handle)
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
    fn push_state_can_only_rewrite_within_the_page() {
        let url = |s: &str| Url::parse(s).unwrap();
        let page = url("https://a.test/app?x=1");
        assert!(can_rewrite(&page, &url("https://a.test/other#h")));
        assert!(!can_rewrite(&page, &url("https://paypal.com@a.test/")), "userinfo spoofing");
        assert!(!can_rewrite(&page, &url("https://b.test/")));
        assert!(!can_rewrite(&page, &url("http://a.test/app")));
        assert!(!can_rewrite(&page, &url("https://a.test:8443/app")));
        let file = url("file:///Users/me/site/index.html");
        assert!(can_rewrite(&file, &url("file:///Users/me/site/index.html?q=1#x")), "a file page may change its query and fragment");
        assert!(!can_rewrite(&file, &url("file:///Users/me/site/other.html")));
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
