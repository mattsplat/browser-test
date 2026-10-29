//! A page document with a running language pack (Dart, JS or Python). It delivers DOM events
//! to the listeners the pack registered, runs the pack's timers, and tells the pack when
//! callbacks are no longer needed. Every pack shares the host, so this behaves identically
//! for all of them.
//!
//! This module is the pack contract (see "Language packs" in DESIGN.md): `PackRuntime` is
//! what a pack implements; `PackDocument::starting` and `Startup` are how it starts; and the
//! wasmtime engine and time limit below are what every pack runs under. Nothing here knows
//! which language a pack is.
//!
//! The browser's tools reach a running page through `PageRef`: the Inspector's console
//! evaluates in its pack, and pause on exceptions stops delivering it events and timers.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context as TaskContext, Waker};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use blitz_dom::{BaseDocument, DocGuard, DocGuardMut, Document, EventDriver, EventHandler, NodeId};
use blitz_traits::events::{
    BlitzKeyEvent, BlitzPointerEvent, BlitzPointerId, DomEvent, DomEventData, EventState, MouseEventButton, MouseEventButtons, PointerCoords, PointerDetails, UiEvent,
};
use keyboard_types::Modifiers;
use wasmtime::{Config, Engine, Trap};

use crate::debug;
use crate::replay;
use crate::host::{self, EventDetails, HostValue, Obj, Objects, PageEnv, PageHost, TimerInfo, TraceEntry, TraceKind};

/// What the document needs from a pack. Every call runs on the UI thread, one at a time,
/// under the time limit (`TIME_LIMIT_TICKS`).
pub trait PackRuntime {
    /// Call a function the page handed over (a listener, a timer) with `args`. An error is
    /// one the page didn't catch, already formatted to read (message, then its stack).
    fn fire_callback(&mut self, id: usize, args: &[HostValue]) -> Result<()>;
    /// The host no longer refers to this callback; the pack may free it.
    fn release_callback(&mut self, id: usize);
    /// Whether `eval` can run console input (a compiled language may ship no compiler).
    fn can_eval(&self) -> bool {
        true
    }
    /// Evaluate console input in the page's running program; the result formatted to read.
    fn eval(&mut self, source: &str) -> Result<String>;
    /// The pack was stopped (it ran too long, or crashed) and can't run again.
    fn stopped(&self) -> bool;
}

/// The wasmtime engine every pack runs on (GC and exceptions enabled), with epoch
/// interruption so a pack that never returns can be stopped (see `TIME_LIMIT_TICKS`).
pub(crate) fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut config = Config::new();
        config.wasm_gc(true).wasm_function_references(true).wasm_exceptions(true).epoch_interruption(true);
        let engine = Engine::new(&config).expect("wasmtime engine with GC and exceptions");
        let ticking = engine.clone();
        std::thread::Builder::new()
            .name("pack-clock".into())
            .spawn(move || loop {
                std::thread::sleep(EPOCH_TICK);
                ticking.increment_epoch();
            })
            .expect("failed to spawn the pack clock");
        engine
    })
}

/// How often the engine's epoch advances.
const EPOCH_TICK: Duration = Duration::from_millis(100);
/// How long one call into a pack may run, in ticks: about 5 s (2 s in tests, so the
/// runaway-script tests stay quick). Packs run on the UI thread, so a call that never
/// returns would hang the browser. A pack sets this deadline on its store before every call
/// in, and reports a call that hits it with `RAN_TOO_LONG` and is `stopped` from then on.
pub(crate) const TIME_LIMIT_TICKS: u64 = if cfg!(test) { 20 } else { 50 };

pub(crate) const RAN_TOO_LONG: &str = "script ran too long and was stopped";
pub(crate) const STOPPED: &str = "this page's script was stopped (it ran too long or crashed); reload the page to run it again";

/// The most memory one page's pack may use. Generous, but a page allocating without end
/// (`'x' * 2**30`) fails inside its pack instead of exhausting the browser's memory.
pub(crate) const MEMORY_LIMIT: usize = 512 << 20;

/// The resource limits for a pack's store (`Store::limiter`).
pub(crate) fn memory_limits() -> wasmtime::StoreLimits {
    wasmtime::StoreLimitsBuilder::new().memory_size(MEMORY_LIMIT).build()
}

/// A key for compiled code: a SHA-256 of the Wasm's bytes, wasmtime's version and
/// configuration (`precompile_compatibility_hash`), and `partition` (empty for code anyone may
/// share), as hex. A cryptographic hash, so a page can't craft a program that collides with
/// another's cached code.
pub(crate) fn code_key(wasm: &[u8], partition: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::hash::{Hash, Hasher};
    // The compatibility hash is only `Hash`; fold it to bytes first.
    let mut compat = std::collections::hash_map::DefaultHasher::new();
    engine().precompile_compatibility_hash().hash(&mut compat);
    let mut sha = Sha256::new();
    sha.update(wasm);
    sha.update(compat.finish().to_le_bytes());
    sha.update(partition.as_bytes());
    sha.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Compiled code for `wasm`: loaded from `path` if it's there and loads, otherwise compiled
/// and saved there. Returns whether it was compiled now. Other processes (profiles) may have
/// the file memory-mapped, so it's only ever replaced whole (`write_atomic`).
pub(crate) fn load_or_compile(wasm: &[u8], path: Option<&std::path::Path>, what: &str) -> Result<(wasmtime::Module, bool)> {
    if let Some(path) = path.filter(|p| p.exists()) {
        // Safety: the file was written by `Module::serialize` below, into our own cache, and
        // its name carries the engine's compatibility hash.
        match unsafe { wasmtime::Module::deserialize_file(engine(), path) } {
            Ok(module) => return Ok((module, false)),
            Err(e) => tracing::warn!("ignoring stale compiled {what}: {e:#}"),
        }
    }
    let module = wasmtime::Module::new(engine(), wasm)?;
    if let Some(path) = path {
        let saved = path.parent().map(std::fs::create_dir_all).transpose().and_then(|_| {
            module.serialize().map_err(std::io::Error::other).and_then(|bytes| crate::profile::write_atomic(path, &bytes))
        });
        if let Err(e) = saved {
            tracing::warn!("could not cache the compiled {what}: {e}");
        }
    }
    Ok((module, true))
}

/// Whether `err` is the time limit stopping a pack.
pub(crate) fn interrupted(err: &wasmtime::Error) -> bool {
    err.downcast_ref::<Trap>() == Some(&Trap::Interrupt)
}

/// What the browser's tools can reach in a loaded page, without keeping it alive.
#[derive(Clone)]
pub struct PageRef {
    pub doc: Weak<RefCell<BaseDocument>>,
    /// The page's running pack; None when no script runs.
    pub pack: Option<Weak<RunningPack>>,
}

/// The same page: tools compare pages by identity.
impl PartialEq for PageRef {
    fn eq(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.doc, &other.doc)
    }
}

impl PageRef {
    pub fn doc(&self) -> Option<Rc<RefCell<BaseDocument>>> {
        self.doc.upgrade()
    }

    pub fn pack(&self) -> Option<Rc<RunningPack>> {
        self.pack.as_ref().and_then(Weak::upgrade)
    }
}

/// A page's running pack and what it shares with the host. The page document owns it; the
/// browser's tools hold it weakly (see `PageRef`).
pub struct RunningPack {
    /// "dart", "js" or "python".
    pub name: &'static str,
    /// The page's URL, for the trace.
    pub page: String,
    doc: Rc<RefCell<BaseDocument>>,
    objects: Rc<RefCell<Objects>>,
    runtime: RefCell<Box<dyn PackRuntime>>,
    /// Whether the console can evaluate in this pack, read once at start so it doesn't
    /// depend on the runtime being free.
    can_eval: bool,
    /// Set by an uncaught error while pause on exceptions is on. The pack gets no events or
    /// timers until resumed (timers due meanwhile run on resume).
    paused: Cell<bool>,
    /// Wakes the event loop when the next timer is due or a fetch finished (set from `poll`).
    waker: Arc<Mutex<Option<Waker>>>,
    timer_thread: RefCell<Option<Sender<Instant>>>,
    /// How many times each callback has been called, for the Inspector's Handles view.
    fires: RefCell<HashMap<usize, u64>>,
    /// The window size last seen, to notice a resize.
    window_size: Cell<(u32, u32)>,
    /// The field that has the focus and its value then, for `change` when it loses it.
    focused_value: RefCell<Option<(NodeId, Option<String>)>>,
}

/// What a page's pack holds, as the Inspector's Handles view shows it.
pub struct Handles {
    /// Handle and what it is (`Element button#go`, `Event click`, `Document`).
    pub objects: Vec<(u32, String)>,
    /// Node, event type, and each listener's callback id with how often it was called.
    pub listeners: Vec<(String, String, Vec<(usize, u64)>)>,
    /// Pending timers, with how often their callback was called.
    pub timers: Vec<(TimerInfo, u64)>,
    /// Distinct callbacks the host refers to (listeners and timers).
    pub callbacks: usize,
    /// Calls into the pack's callbacks since the page loaded.
    pub calls: u64,
}

impl RunningPack {
    /// What the pack holds right now: its handles, listeners per node, timers and callbacks.
    pub fn handles(&self) -> Handles {
        let objects = self.objects.borrow();
        let doc = self.doc.borrow();
        let fires = self.fires.borrow();
        let count = |id: &usize| fires.get(id).copied().unwrap_or(0);
        let held = objects
            .held()
            .into_iter()
            .map(|(h, obj)| {
                let what = match &obj {
                    Obj::Element(node) => format!("Element {}", debug::describe(&doc, *node)),
                    Obj::Event(e) => format!("Event {}", e.kind),
                    other => other.interface().to_string(),
                };
                (h, what)
            })
            .collect();
        let mut listeners: Vec<(String, String, Vec<(usize, u64)>)> = objects
            .listeners
            .iter()
            .filter(|(_, ids)| !ids.is_empty())
            .map(|((node, kind), ids)| (debug::describe(&doc, *node), kind.clone(), ids.iter().map(|id| (*id, count(id))).collect()))
            .collect();
        listeners.sort();
        let timers: Vec<(TimerInfo, u64)> = objects.pending_timers(Instant::now()).into_iter().map(|t| { let n = count(&t.callback); (t, n) }).collect();
        let mut ids: Vec<usize> = objects.listeners.values().flatten().copied().chain(timers.iter().map(|(t, _)| t.callback)).collect();
        ids.sort();
        ids.dedup();
        Handles { objects: held, listeners, timers, callbacks: ids.len(), calls: fires.values().sum() }
    }

    /// Evaluate console input `source` in the page's pack, on behalf of `by` (the console, or
    /// AI access). Like any call, it goes in the trace; DOM access inside it goes through the
    /// host as usual.
    pub fn eval(&self, source: &str, by: &'static str) -> Result<String> {
        let started = Instant::now();
        let result = match self.runtime.try_borrow_mut() {
            Ok(mut runtime) => {
                self.objects.borrow_mut().tape.step(|| replay::Step::Eval { source: source.to_string(), by });
                // Host calls the input makes are traced as the caller's, not the page's, so
                // the AI's redacted views can leave out what the user typed.
                self.objects.borrow().caller.set(Some(by));
                let result = runtime.eval(source);
                self.objects.borrow().caller.set(None);
                release_unused(&self.objects, runtime.as_mut());
                result
            }
            Err(_) => Err(anyhow!("the page's script is running")),
        };
        let (kind, shown) = match &result {
            Ok(text) => (TraceKind::Call, text.clone()),
            Err(e) => (TraceKind::Error, format!("ERROR {e:#}")),
        };
        host::record(TraceEntry { pack: by, kind, page: self.page.clone(), call: "eval".into(), args: source.to_string(), result: shown, micros: started.elapsed().as_micros() as u64 });
        // The input may have changed the page or started timers.
        self.doc.borrow().shell_provider.request_redraw();
        self.arm_timers();
        result
    }

    /// Whether the console can evaluate input in this page's pack.
    pub fn can_eval(&self) -> bool {
        self.can_eval
    }

    pub fn is_paused(&self) -> bool {
        self.paused.get()
    }

    /// Click the element `selector` matches, for AI control: the page's click listeners run
    /// as for `element.click()`, with no default action (a link isn't followed).
    pub fn click(&self, selector: &str) -> Result<String> {
        if self.paused.get() {
            bail!("the page is paused on an exception");
        }
        let found = self.doc.borrow().query_selector(selector).map_err(|e| anyhow!("bad selector {selector:?}: {e:?}"))?;
        let node = found.ok_or_else(|| anyhow!("nothing matches {selector:?}"))?;
        let chain = self.doc.borrow().node_chain(node);
        // Like element.click(), a click with no position.
        let mut event = DomEvent::new(node, click_at(0.0, 0.0, false));
        let mut doc = self.doc.clone();
        PackEventHandler { pack: self }.handle_event(&chain, &mut event, &mut doc, &mut EventState::default());
        self.doc.borrow().shell_provider.request_redraw();
        self.arm_timers();
        Ok(debug::describe(&self.doc.borrow(), node))
    }

    /// Let the pack run again after pausing on an exception.
    pub fn resume(&self) {
        self.objects.borrow_mut().tape.step(|| replay::Step::Resume);
        self.paused.set(false);
        self.doc.borrow().shell_provider.request_redraw();
        // Timers that came due while paused: wake the loop to run them.
        self.arm_timers();
    }

    /// Paused, or stopped for running too long: the pack gets no events or timers.
    fn idle(&self) -> bool {
        self.paused.get() || self.runtime.try_borrow().is_ok_and(|r| r.stopped())
    }

    /// Run every timer that is due. Returns whether any ran. Idle, none run. A batch that
    /// pauses partway still finishes: its one-shot timers are already out of the table, and
    /// dropping them would break the page on resume.
    fn run_due_timers(&self) -> bool {
        if self.idle() {
            return false;
        }
        let due = self.objects.borrow_mut().take_due_timers(Instant::now());
        self.run_timers(due)
    }

    /// Run a batch of timers (recorded as one step). Returns whether there were any.
    fn run_timers(&self, due: Vec<host::DueTimer>) -> bool {
        if due.is_empty() {
            return false;
        }
        let frames = due.iter().map(|t| (t.handle, if t.kind == "animation frame" { t.args.first().and_then(|a| a.as_num()) } else { None }));
        let frames = frames.collect();
        self.objects.borrow_mut().tape.step(|| replay::Step::Timers(frames));
        for timer in &due {
            if let Err(e) = self.call(timer.callback, &timer.args, timer.kind, String::new()) {
                self.uncaught(timer.kind, String::new(), &e);
            }
        }
        self.objects.borrow_mut().after_timers_ran(&due);
        release_unused(&self.objects, self.runtime.borrow_mut().as_mut());
        true
    }

    /// The tab moved to another entry of this page's document (back, forward, a fragment
    /// link): take its URL and state, then `popstate`, and `hashchange` if the fragment
    /// changed. Nothing happens if the page is already there (it made the entry itself).
    pub fn traverse(&self, url: &str, state: Option<String>) {
        let old = self.doc.borrow().url().to_string();
        let current_state = self.objects.borrow().history_state.clone();
        if old == url && current_state.as_ref().is_none_or(|s| *s == state) {
            return;
        }
        self.objects.borrow_mut().tape.step(|| replay::Step::Traverse { url: url.to_string(), state: state.clone() });
        self.move_to(url, state.clone());
        let root = [self.doc.borrow().root_node().id];
        self.dispatch(&root, "popstate", EventDetails { state, ..Default::default() });
        let fragment = |u: &str| u.split_once('#').map(|(_, f)| f.to_string());
        if fragment(&old) != fragment(url) {
            self.dispatch(&root, "hashchange", EventDetails { old_url: Some(old), new_url: Some(url.to_string()), ..Default::default() });
        }
        self.doc.borrow().shell_provider.request_redraw();
        self.arm_timers();
    }

    fn move_to(&self, url: &str, state: Option<String>) {
        if blitz_traits::net::Url::parse(url).is_ok() {
            self.doc.borrow_mut().set_base_url(url);
        }
        self.objects.borrow_mut().history_state = Some(state);
    }

    /// Show what the page drew on its canvases since last time: each one's pixels become
    /// its element's image, which Blitz paints at the canvas's place (`canvas.rs`).
    pub(crate) fn show_canvases(&self) {
        use blitz_dom::node::{ImageData, RasterImageData, SpecialElementData};
        let drawn: Vec<(NodeId, u32, u32, Vec<u8>)> = self
            .objects
            .borrow_mut()
            .canvases
            .iter_mut()
            .filter(|(_, c)| c.dirty)
            .map(|(id, c)| {
                c.dirty = false;
                (*id, c.width, c.height, c.render())
            })
            .collect();
        if drawn.is_empty() {
            return;
        }
        let mut doc = self.doc.borrow_mut();
        for (id, width, height, pixels) in drawn {
            if let Some(node) = doc.get_node_mut(id) {
                if let Some(el) = node.element_data_mut() {
                    el.special_data = match width as u64 * height as u64 {
                        0 => SpecialElementData::None,
                        _ => SpecialElementData::Image(Box::new(ImageData::Raster(RasterImageData::new(width, height, Arc::new(pixels))))),
                    };
                }
                node.mark_ancestors_dirty();
            }
        }
        doc.shell_provider.request_redraw();
    }

    /// Fire `resize` on `window` if the window changed size since last checked (Blitz has no
    /// resize callback for a document, so the event loop's poll checks).
    fn check_resize(&self) -> bool {
        let size = self.doc.borrow().viewport().window_size;
        if size == self.window_size.replace(size) {
            return false;
        }
        let root = [self.doc.borrow().root_node().id];
        self.dispatch(&root, "resize", EventDetails::default());
        true
    }

    /// Dispatch the events the page's own code queued (`hashchange` on `window` after it set
    /// `location.hash`, `focus` after `element.focus()`). Returns whether there were any.
    fn run_queued_events(&self) -> bool {
        if self.idle() {
            return false;
        }
        let queued = std::mem::take(&mut self.objects.borrow_mut().queued_events);
        for (node, kind, details) in &queued {
            // A node removed meanwhile gets nothing.
            if self.doc.borrow().get_node(*node).is_none() {
                continue;
            }
            let chain = self.doc.borrow().node_chain(*node);
            self.deliver(&chain, kind, details.clone(), None, false);
        }
        let objects = std::mem::take(&mut self.objects.borrow_mut().queued_object_events);
        for (target, kind) in &objects {
            // A transaction completes after its requests' events.
            if self.objects.borrow().transaction_waiting(*target, kind) {
                self.objects.borrow_mut().queue_object_event(*target, kind);
                continue;
            }
            self.objects.borrow_mut().tape.step(|| replay::Step::ObjectEvent { target: *target, kind: kind.clone() });
            self.object_event(*target, kind);
        }
        !queued.is_empty() || !objects.is_empty()
    }

    /// An event the page caused on an object that isn't a node (an IndexedDB request's
    /// `success`), with the bookkeeping around it.
    fn object_event(&self, target: u32, kind: &str) {
        self.objects.borrow_mut().idb_before(target, kind);
        self.dispatch_object(target, kind, EventDetails::default());
        self.objects.borrow_mut().idb_after(target);
        release_unused(&self.objects, self.runtime.borrow_mut().as_mut());
    }

    /// Dispatch an event and the form events that go with it (`form_events`; `change` goes
    /// before the `blur` that causes it, as in browsers). Returns whether Blitz's default
    /// action is cancelled.
    fn deliver(&self, chain: &[NodeId], kind: &str, details: EventDetails, key: Option<&str>, cancelled: bool) -> bool {
        if kind == "blur" {
            self.form_events(chain, kind, key, cancelled);
            return self.dispatch(chain, kind, details);
        }
        let prevented = self.dispatch(chain, kind, details);
        self.form_events(chain, kind, key, cancelled || prevented) || prevented
    }

    /// The events browsers fire around the ones Blitz reports, for forms: `change` when a
    /// text field loses focus with a new value, or a checkbox or radio is toggled; `submit`
    /// on the form before a submit button's click or Enter in a text field submits it.
    /// Returns whether the page cancelled the submission (`preventDefault` on `submit`), so
    /// the caller can cancel Blitz's default action. `cancelled`: that action was already
    /// cancelled. Each event is dispatched (and recorded) like any other, so a replay gets
    /// them as steps.
    fn form_events(&self, chain: &[NodeId], kind: &str, key: Option<&str>, cancelled: bool) -> bool {
        let Some(&target) = chain.first() else { return false };
        let field = |id: NodeId| self.doc.borrow().get_node(id).and_then(|n| n.element_data()).map(host::field_value);
        match kind {
            "focus" => {
                *self.focused_value.borrow_mut() = Some((target, field(target)));
                false
            }
            "blur" => {
                let focused = self.focused_value.borrow_mut().take();
                if let Some((node, value)) = focused.filter(|(node, _)| *node == target) {
                    let text_field = self.doc.borrow().get_node(node).and_then(|n| n.element_data()).is_some_and(|el| el.text_input_data().is_some());
                    if text_field && value != field(node) {
                        self.dispatch(chain, "change", EventDetails::default());
                    }
                }
                false
            }
            "input" => {
                let toggle = self.doc.borrow().get_node(target).and_then(|n| n.element_data()).is_some_and(|el| el.checkbox_input_checked().is_some());
                if toggle {
                    self.dispatch(chain, "change", EventDetails::default());
                }
                false
            }
            "click" | "keydown" if !cancelled => {
                let Some(form) = self.submitted_form(chain, kind, key) else { return false };
                let form_chain = self.doc.borrow().node_chain(form);
                self.dispatch(&form_chain, "submit", EventDetails::default())
            }
            _ => false,
        }
    }

    /// The form a click or key on `chain` would submit: a click on an enabled submit button,
    /// or Enter in a one-line text field, inside a form (or naming one with `form=`).
    fn submitted_form(&self, chain: &[NodeId], kind: &str, key: Option<&str>) -> Option<NodeId> {
        let doc = self.doc.borrow();
        let element = |id: &NodeId| doc.get_node(*id).and_then(|n| n.element_data());
        let control = match kind {
            "click" => chain.iter().copied().find(|id| element(id).is_some_and(|el| matches!(el.name.local.as_ref(), "button" | "input")))?,
            _ => *chain.first()?,
        };
        let el = element(&control)?;
        let submits = match kind {
            "click" => el.is_submit_button() || el.attr(blitz_dom::LocalName::from("type")) == Some("submit"),
            _ => key == Some("Enter") && el.text_input_data().is_some_and(|t| !t.is_multiline),
        };
        if !submits || el.attr(blitz_dom::LocalName::from("disabled")).is_some() {
            return None;
        }
        if let Some(form_id) = el.attr(blitz_dom::LocalName::from("form")) {
            return doc.get_element_by_id(form_id).filter(|f| element(f).is_some_and(|el| &*el.name.local == "form"));
        }
        doc.node_chain(control).into_iter().find(|id| element(id).is_some_and(|el| &*el.name.local == "form"))
    }

    /// Deliver what the page's WebSockets received (`websocket.rs`), each a recorded step.
    /// Paused or stopped, it waits (the connections stop reading once enough is waiting).
    fn deliver_sockets(&self) -> bool {
        if self.idle() {
            return false;
        }
        let events = self.objects.borrow().inbox.take();
        for (socket, event) in &events {
            self.objects.borrow_mut().tape.step(|| replay::Step::Socket { socket: *socket, event: event.clone() });
            self.socket_event(*socket, event);
        }
        !events.is_empty()
    }

    /// One thing that happened on socket `socket`: its state changes, then its event.
    fn socket_event(&self, socket: u32, event: &crate::websocket::SocketEvent) {
        let changed = self.objects.borrow_mut().socket_changed(socket, event);
        if let Some((kind, details)) = changed {
            self.dispatch_object(socket, kind, details);
        }
        if matches!(event, crate::websocket::SocketEvent::Close { .. }) {
            self.objects.borrow_mut().forget_target(socket);
        }
        release_unused(&self.objects, self.runtime.borrow_mut().as_mut());
    }

    /// Deliver an event to an object that isn't a node (a WebSocket): its `on<type>` handler,
    /// then its listeners. The caller records the step it comes from.
    fn dispatch_object(&self, target: u32, kind: &str, details: EventDetails) {
        let ids = self.objects.borrow().object_listeners(target, kind);
        if ids.is_empty() || self.idle() {
            return;
        }
        let root = self.doc.borrow().root_node().id;
        let handle = self.objects.borrow_mut().object_event(kind, target, root, details);
        let during = format!("{kind} listener");
        for id in ids {
            if let Err(e) = self.call(id, &[HostValue::Object(handle)], &during, String::new()) {
                self.uncaught(&during, String::new(), &e);
            }
            if self.objects.borrow_mut().event_mut(handle).is_some_and(|e| e.immediate_propagation_stopped) || self.paused.get() {
                break;
            }
        }
        self.objects.borrow_mut().release(handle);
    }

    /// Settle the promises of fetches that finished (each one a recorded step), so their
    /// callbacks run with the due timers. Returns whether any did.
    fn deliver_fetches(&self) -> bool {
        let done = self.objects.borrow_mut().take_fetched();
        for (promise, result) in &done {
            self.objects.borrow_mut().tape.step(|| replay::Step::Fetch { promise: *promise, result: result.clone() });
            self.fetch_finished(*promise, result);
        }
        !done.is_empty()
    }

    /// A finished fetch: an XMLHttpRequest's events, or its promise settled.
    fn fetch_finished(&self, promise: u32, result: &Result<crate::gate::FetchResponse, String>) {
        let xhr = self.objects.borrow_mut().xhr_finished(promise, result);
        match xhr {
            Some(None) => {}
            Some(Some((xhr, events))) => {
                for kind in events {
                    self.dispatch_object(xhr, kind, EventDetails::default());
                }
                self.objects.borrow_mut().xhr_delivered(xhr);
                release_unused(&self.objects, self.runtime.borrow_mut().as_mut());
            }
            None => self.objects.borrow_mut().deliver_fetch(promise, result.clone()),
        }
    }

    /// Deliver an event of type `kind` along `chain` (target first, then its ancestors) to
    /// the page's listeners, in bubbling order, recording it as a step. Returns whether a
    /// listener called `preventDefault`. Idle, or with no listener on the chain, nothing runs.
    fn dispatch(&self, chain: &[NodeId], kind: &str, details: EventDetails) -> bool {
        let Some(&target) = chain.first() else { return false };
        let listening = |node: &NodeId| self.objects.borrow().listeners.contains_key(&(*node, kind.to_string()));
        // Paused on an exception or stopped, the page's listeners don't run (Blitz's defaults still do).
        if self.idle() || !chain.iter().any(listening) {
            return false;
        }
        if let Some(path) = replay::path_of(&self.doc.borrow(), target) {
            let details = details.clone();
            self.objects.borrow_mut().tape.step(|| replay::Step::Event { kind: kind.to_string(), target: path, details });
        }
        // One event object for the whole dispatch, released when it ends.
        let handle = self.objects.borrow_mut().event(kind, target, details);
        let flag = |f: fn(&host::EventObj) -> bool| self.objects.borrow_mut().event_mut(handle).is_some_and(|e| f(e));
        // The chain runs from the target up to the root: bubbling order.
        'nodes: for node in chain {
            let ids = self.objects.borrow().listeners.get(&(*node, kind.to_string())).cloned().unwrap_or_default();
            if let Some(e) = self.objects.borrow_mut().event_mut(handle) {
                e.current_target = *node;
            }
            for id in ids {
                let on = debug::describe(&self.doc.borrow(), *node);
                let during = format!("{kind} listener");
                if let Err(e) = self.call(id, &[HostValue::Object(handle)], &during, on.clone()) {
                    self.uncaught(&during, on, &e);
                }
                if flag(|e| e.immediate_propagation_stopped) || self.paused.get() {
                    break 'nodes;
                }
            }
            if flag(|e| e.propagation_stopped) {
                break;
            }
        }
        let prevented = flag(|e| e.default_prevented);
        self.objects.borrow_mut().release(handle);
        release_unused(&self.objects, self.runtime.borrow_mut().as_mut());
        prevented
    }

    // ---- Recording and replay (`replay.rs`)

    /// Steps recorded so far, if this page is being recorded.
    pub fn recorded_steps(&self) -> Option<usize> {
        self.objects.borrow().tape.recording().map(|r| r.steps.len())
    }

    /// Stop recording and hand back the recording, with the page's DOM as it is now.
    pub fn stop_recording(&self) -> Option<replay::Recording> {
        let tape = std::mem::take(&mut self.objects.borrow_mut().tape);
        match tape {
            replay::Tape::Record(mut r) => {
                r.final_dom = replay::dom_text(&self.doc.borrow());
                Some(*r)
            }
            other => {
                self.objects.borrow_mut().tape = other;
                None
            }
        }
    }

    /// Apply one recorded step, as it happened.
    fn replay_step(&self, step: &replay::Step) -> Result<()> {
        match step {
            replay::Step::Event { kind, target, details } => {
                let node = replay::node_at(&self.doc.borrow(), target).ok_or_else(|| anyhow!("the {kind} target {target:?} isn't in the page"))?;
                let chain = self.doc.borrow().node_chain(node);
                self.dispatch(&chain, kind, details.clone());
            }
            replay::Step::Timers(fired) => {
                let due = self.objects.borrow_mut().take_timers(fired).ok_or_else(|| anyhow!("timers {fired:?} aren't all pending"))?;
                self.run_timers(due);
            }
            replay::Step::Eval { source, by } => {
                let _ = self.eval(source, by);
            }
            replay::Step::Resume => self.paused.set(false),
            replay::Step::Traverse { url, state } => self.move_to(url, state.clone()),
            replay::Step::Fetch { promise, result } => self.fetch_finished(*promise, result),
            replay::Step::Socket { socket, event } => self.socket_event(*socket, event),
            replay::Step::ObjectEvent { target, kind } => self.object_event(*target, kind),
        }
        Ok(())
    }

    /// Make sure the event loop wakes when the next timer is due. Not while idle: a due
    /// timer that can't run would wake it again at once, forever.
    fn arm_timers(&self) {
        if self.idle() {
            return;
        }
        let Some(deadline) = self.objects.borrow().next_deadline() else { return };
        let mut slot = self.timer_thread.borrow_mut();
        let sender = slot.get_or_insert_with(|| {
            let (tx, rx) = channel::<Instant>();
            let waker = Arc::clone(&self.waker);
            std::thread::Builder::new()
                .name("pack-timers".into())
                .spawn(move || timer_thread(rx, waker))
                .expect("failed to spawn the timer thread");
            tx
        });
        // A dead thread (disconnected channel) is replaced next time.
        if sender.send(deadline).is_err() {
            *slot = None;
        }
    }

    /// An error the page didn't catch: into the trace, and with pause on exceptions on, a
    /// snapshot of the page, which then pauses. `during` is what was running.
    fn uncaught(&self, during: &str, args: String, e: &anyhow::Error) {
        record_error(self.name, &self.page, during.to_string(), args.clone(), e);
        if snapshot_if_pausing(self.name, &self.page, &self.doc, &self.objects, during, args, e) {
            self.paused.set(true);
        }
    }

    fn fire(&self, id: usize, args: &[HostValue]) -> Result<()> {
        self.runtime.borrow_mut().fire_callback(id, args)
    }

    /// Call page function `id` for `during` (`click listener`, `timer`) on `on`, counted and
    /// timed: the trace gets one `Pack` entry with the time spent inside the pack. The host
    /// calls it made have entries of their own, so the profiler can tell the two apart.
    fn call(&self, id: usize, args: &[HostValue], during: &str, on: String) -> Result<()> {
        *self.fires.borrow_mut().entry(id).or_default() += 1;
        let started = Instant::now();
        let in_host = self.objects.borrow().host_micros.get();
        let result = self.fire(id, args);
        let in_host = self.objects.borrow().host_micros.get() - in_host;
        let micros = (started.elapsed().as_micros() as u64).saturating_sub(in_host);
        host::record(TraceEntry { pack: self.name, kind: TraceKind::Pack, page: self.page.clone(), call: during.to_string(), args: on, result: String::new(), micros });
        result
    }
}

pub struct PackDocument {
    pack: Rc<RunningPack>,
}

/// A page whose pack is starting: what its start-up code reports errors to, and what makes
/// the running page document once the page's code has run (`finish`).
pub struct Startup {
    name: &'static str,
    page: String,
    doc: Rc<RefCell<BaseDocument>>,
    objects: Rc<RefCell<Objects>>,
    /// An error while starting paused the page (pause on exceptions is on).
    paused: bool,
    /// When the pack started loading, for the profiler's `start` entry.
    started: Instant,
    /// Time in that spent waiting (fetching imports), which isn't the pack's.
    waiting: Duration,
}

impl Startup {
    /// An error the page's code didn't catch (a script, a module): into the trace, and with
    /// pause on exceptions on, a snapshot; the page pauses once it has started. Like a
    /// browser, the pack goes on to the page's next script.
    pub fn uncaught(&mut self, during: &str, args: String, e: &anyhow::Error) {
        record_error(self.name, &self.page, during.to_string(), args.clone(), e);
        self.paused |= snapshot_if_pausing(self.name, &self.page, &self.doc, &self.objects, during, args, e);
    }

    /// An error that isn't the page's code failing (an import that couldn't be fetched): into
    /// the trace only.
    pub fn report(&self, during: &str, args: String, e: &anyhow::Error) {
        record_error(self.name, &self.page, during.to_string(), args, e);
    }

    /// With pause on exceptions on, a snapshot only, for an error that stops the page from
    /// starting (the loader traces it and shows it in place of the page).
    pub fn snapshot(&self, during: &str, args: String, e: &anyhow::Error) {
        snapshot_if_pausing(self.name, &self.page, &self.doc, &self.objects, during, args, e);
    }

    /// Time spent not running the pack (fetching a module's imports), left out of its start.
    pub fn not_running(&mut self, waited: Duration) {
        self.waiting += waited;
    }

    /// The running page, with the pack that started on it.
    pub fn finish(self, runtime: Box<dyn PackRuntime>) -> PackDocument {
        // Loading the pack and running the page's code, less its host calls and waiting.
        let running = self.started.elapsed().saturating_sub(self.waiting);
        let micros = (running.as_micros() as u64).saturating_sub(self.objects.borrow().host_micros.get());
        host::record(TraceEntry { pack: self.name, kind: TraceKind::Pack, page: self.page.clone(), call: "start".into(), args: String::new(), result: String::new(), micros });
        let waker = self.objects.borrow().waker.clone();
        let window_size = self.doc.borrow().viewport().window_size;
        let pack = RunningPack {
            name: self.name,
            page: self.page,
            doc: self.doc,
            objects: self.objects,
            can_eval: runtime.can_eval(),
            runtime: RefCell::new(runtime),
            paused: Cell::new(self.paused),
            waker,
            timer_thread: Default::default(),
            fires: Default::default(),
            window_size: Cell::new(window_size),
            focused_value: Default::default(),
        };
        PackDocument { pack: Rc::new(pack) }
    }
}

impl PackDocument {
    /// With the page being recorded, keep what it loaded: its HTML, site CSS and program
    /// (the loader calls this once the pack has started).
    pub fn attach_recording(&self, html: &str, site_css: Option<&str>, program: replay::Program) {
        if let replay::Tape::Record(r) = &mut self.pack.objects.borrow_mut().tape {
            r.html = html.to_string();
            r.site_css = site_css.map(str::to_string);
            r.program = Some(program);
        }
    }

    pub(crate) fn replay_step(&self, step: &replay::Step) -> Result<()> {
        self.pack.replay_step(step)
    }

    /// End a replay: the page reads live values from now on. Returns why its reads stopped
    /// matching the tape, if they did.
    pub(crate) fn end_replay(&self) -> Option<String> {
        let diverged = match std::mem::take(&mut self.pack.objects.borrow_mut().tape) {
            replay::Tape::Replay { diverged, .. } => diverged,
            _ => None,
        };
        self.pack.objects.borrow_mut().settle_after_replay();
        diverged
    }

    #[cfg(test)]
    pub(crate) fn pack(&self) -> &RunningPack {
        &self.pack
    }

    /// The page and its pack, for the browser's tools.
    pub fn page_ref(&self) -> PageRef {
        PageRef { doc: Rc::downgrade(&self.pack.doc), pack: Some(Rc::downgrade(&self.pack)) }
    }

    /// Start pack `name` on the page `doc`: the host the pack talks to (load the pack with
    /// it), and the `Startup` to report to while the page's code runs. Each page gets its own
    /// host, objects and pack instance; nothing is shared between pages or packs.
    pub fn starting(doc: BaseDocument, env: PageEnv, name: &'static str) -> (PageHost, Startup) {
        let doc = Rc::new(RefCell::new(doc));
        let objects = Rc::new(RefCell::new(Objects::default()));
        let host = PageHost::new(name, doc.clone(), objects.clone(), env);
        let page = host.page.clone();
        // A recording or replay armed for this page starts with it, before the pack reads
        // anything.
        let mut tape = replay::take_armed(&page);
        if let replay::Tape::Record(r) = &mut tape {
            r.pack = name;
        }
        objects.borrow_mut().tape = tape;
        (host, Startup { name, page, doc, objects, paused: false, started: Instant::now(), waiting: Duration::ZERO })
    }
}


/// Wakes the event loop at the earliest deadline it has been sent.
fn timer_thread(rx: Receiver<Instant>, waker: Arc<Mutex<Option<Waker>>>) {
    let wake = || {
        if let Some(w) = waker.lock().unwrap().as_ref() {
            w.wake_by_ref();
        }
    };
    let mut deadline: Option<Instant> = None;
    loop {
        match deadline {
            None => match rx.recv() {
                Ok(d) => deadline = Some(d),
                Err(_) => return,
            },
            Some(current) => match rx.recv_timeout(current.saturating_duration_since(Instant::now())) {
                Ok(d) => deadline = Some(d.min(current)),
                Err(RecvTimeoutError::Timeout) => {
                    wake();
                    deadline = None;
                }
                Err(RecvTimeoutError::Disconnected) => return,
            },
        }
    }
}

fn release_unused(objects: &Rc<RefCell<Objects>>, runtime: &mut dyn PackRuntime) {
    let released = objects.borrow_mut().take_released_callbacks();
    for id in released {
        runtime.release_callback(id);
    }
}

fn record_error(pack: &'static str, page: &str, call: String, args: String, e: &anyhow::Error) {
    host::record(TraceEntry { pack, kind: TraceKind::Error, page: page.to_string(), call, args, result: format!("ERROR {e:#}"), micros: 0 });
}

/// With pause on exceptions on, keep a snapshot of the page after an uncaught error and say
/// it should pause. Whether it pauses is an input like any other (the setting can change at
/// any time), so it's on the tape: a replay pauses where the recording did, and a recorded
/// `Resume` step lets it go on.
fn snapshot_if_pausing(pack: &'static str, page: &str, doc: &RefCell<BaseDocument>, objects: &RefCell<Objects>, during: &str, args: String, e: &anyhow::Error) -> bool {
    let live = u64::from(debug::pause_on_exceptions());
    let pause = objects.borrow_mut().tape.read(replay::Read::Pause, live) != 0;
    // A replay doesn't keep snapshots again.
    if !pause || objects.borrow().tape.replaying() {
        return pause;
    }
    let what = if args.is_empty() { during.to_string() } else { format!("{during} on {args}") };
    let state = objects.borrow().summary();
    debug::snapshot_exception(pack, page, what, format!("{e:#}"), state, &doc.borrow());
    true
}

/// A primary-button click at (x, y), with Shift held if `shift`.
fn click_at(x: f32, y: f32, shift: bool) -> DomEventData {
    DomEventData::Click(BlitzPointerEvent {
        id: BlitzPointerId::Mouse,
        is_primary: true,
        coords: PointerCoords { page_x: x, page_y: y, screen_x: x, screen_y: y, client_x: x, client_y: y },
        button: MouseEventButton::Main,
        buttons: MouseEventButtons::empty(),
        mods: if shift { Modifiers::SHIFT } else { Modifiers::empty() },
        details: PointerDetails::default(),
        element: Default::default(),
        active_pointers: Default::default(),
    })
}

/// The mouse and keyboard details the DOM exposes for an event.
pub fn event_details(data: &DomEventData) -> EventDetails {
    use DomEventData::*;
    let mods = |m: Modifiers, d: &mut EventDetails| {
        d.alt_key = m.contains(Modifiers::ALT);
        d.ctrl_key = m.contains(Modifiers::CONTROL);
        d.shift_key = m.contains(Modifiers::SHIFT);
        d.meta_key = m.contains(Modifiers::META);
    };
    let pointer = |p: &BlitzPointerEvent| {
        let mut d = EventDetails {
            client_x: Some(p.coords.client_x as f64),
            client_y: Some(p.coords.client_y as f64),
            page_x: Some(p.coords.page_x as f64),
            page_y: Some(p.coords.page_y as f64),
            screen_x: Some(p.coords.screen_x as f64),
            screen_y: Some(p.coords.screen_y as f64),
            button: Some(p.button as u8 as f64),
            buttons: Some(p.buttons.bits() as f64),
            ..Default::default()
        };
        mods(p.mods, &mut d);
        d
    };
    let key = |k: &BlitzKeyEvent| {
        let mut d = EventDetails {
            key: Some(k.key.to_string()),
            code: Some(k.code.to_string()),
            repeat: Some(k.is_auto_repeating),
            ..Default::default()
        };
        mods(k.modifiers, &mut d);
        d
    };
    match data {
        PointerMove(p) | PointerDown(p) | PointerUp(p) | PointerCancel(p) | PointerEnter(p) | PointerLeave(p)
        | PointerOver(p) | PointerOut(p) | MouseMove(p) | MouseDown(p) | MouseUp(p) | MouseEnter(p)
        | MouseLeave(p) | MouseOver(p) | MouseOut(p) | TouchStart(p) | TouchMove(p) | TouchEnd(p)
        | TouchCancel(p) | Click(p) | ContextMenu(p) | DoubleClick(p) => pointer(p),
        KeyPress(k) | KeyDown(k) | KeyUp(k) => key(k),
        _ => EventDetails::default(),
    }
}

impl Document for PackDocument {
    fn inner(&self) -> DocGuard<'_> {
        DocGuard::RefCell(self.pack.doc.borrow())
    }

    fn inner_mut(&mut self) -> DocGuardMut<'_> {
        DocGuardMut::RefCell(self.pack.doc.borrow_mut())
    }

    fn handle_ui_event(&mut self, event: UiEvent) {
        let mut doc = self.pack.doc.clone();
        let mut driver = EventDriver::new(&mut doc, PackEventHandler { pack: &self.pack });
        driver.handle_ui_event(event);
        // The pack may have changed the DOM or started timers.
        self.pack.doc.borrow().shell_provider.request_redraw();
        self.pack.arm_timers();
    }

    fn poll(&mut self, task_context: Option<TaskContext>) -> bool {
        if let Some(cx) = &task_context {
            let mut waker = self.pack.waker.lock().unwrap();
            if !waker.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
                *waker = Some(cx.waker().clone());
            }
        }
        let resized = self.pack.check_resize();
        let delivered = self.pack.deliver_fetches() | self.pack.deliver_sockets();
        let queued = self.pack.run_queued_events();
        let ran = self.pack.run_due_timers();
        self.pack.show_canvases();
        self.pack.arm_timers();
        resized || delivered || queued || ran
    }
}

struct PackEventHandler<'a> {
    pack: &'a RunningPack,
}

impl EventHandler for PackEventHandler<'_> {
    fn handle_event(&mut self, chain: &[NodeId], event: &mut DomEvent, _doc: &mut dyn Document, state: &mut EventState) {
        let key = match &event.data {
            DomEventData::KeyDown(k) => Some(k.key.to_string()),
            _ => None,
        };
        if self.pack.deliver(chain, event.data.name(), event_details(&event.data), key.as_deref(), state.is_cancelled()) {
            state.prevent_default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interpreter::Script;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;

    fn text_of(doc: &BaseDocument, selector: &str) -> String {
        doc.get_node(doc.query_selector(selector).unwrap().unwrap()).unwrap().text_content()
    }

    /// Compile a Dart file with the dev-mode compiler (cached, so only the first run is slow).
    /// One at a time: the fixtures share a Dart project folder, which concurrent first-time
    /// compiles would race on.
    async fn compile(path: &str) -> Vec<u8> {
        static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _turn = ONE_AT_A_TIME.lock().await;
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path).canonicalize().unwrap();
        let url = blitz_traits::net::Url::from_file_path(&src).unwrap();
        let gate = std::sync::Arc::new(crate::gate::Gate::new(std::sync::Arc::new(blitz_net::Provider::new(None))));
        let signal = blitz_traits::net::AbortController::default().signal;
        crate::dev_compile::compile(&url, &url, &gate, &signal).await.unwrap()
    }

    fn listener(page: &PackDocument, selector: &str, kind: &str) -> (NodeId, usize) {
        let node = page.pack.doc.borrow().query_selector(selector).unwrap().unwrap();
        let ids = page.pack.objects.borrow().listeners[&(node, kind.to_string())].clone();
        assert_eq!(ids.len(), 1);
        (node, ids[0])
    }

    /// The real counter example, end to end: its HTML in Blitz, its compiled package:web Dart
    /// in the pack, the real host.
    #[tokio::test]
    async fn counter_example_runs_and_handles_clicks() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/counter");
        let html = std::fs::read_to_string(dir.join("index.html")).unwrap();
        let wasm = compile("../examples/counter/main.dart").await;
        let doc = HtmlDocument::from_html(&html, DocumentConfig::default()).into_inner();
        let page = crate::dart_pack::start(doc, Default::default(), &wasm).unwrap();
        assert_eq!(text_of(&page.pack.doc.borrow(), "#status"), "Dart is running. No JavaScript on this page.");

        // Deliver two clicks the way the event handler does: the button's click listeners,
        // each called with a fresh event object.
        let (button, id) = listener(&page, "#increment", "click");
        for _ in 0..2 {
            let event = HostValue::Object(page.pack.objects.borrow_mut().event("click", button, Default::default()));
            page.pack.fire(id, &[event]).unwrap();
        }

        let doc = page.pack.doc.borrow();
        assert_eq!(text_of(&doc, "#count"), "2");
        assert_eq!(text_of(&doc, "#log"), "Click 1, handled in Dart (click)Click 2, handled in Dart (click)");
        drop(doc);

        // Dart has no runtime compiler, so the console says so instead of evaluating.
        let err = page.pack.eval("1 + 1", "console").unwrap_err().to_string();
        assert!(err.starts_with("Dart pages can't evaluate code"), "{err}");
    }

    /// Errors and stack traces read in Dart source lines, via the embedded source map.
    #[tokio::test]
    async fn errors_and_stack_traces_show_dart_lines() {
        let wasm = compile("tests/fixtures/throws.dart").await;
        let doc = HtmlDocument::from_html("<button id=go>go</button>", DocumentConfig::default()).into_inner();
        let page = crate::dart_pack::start(doc, Default::default(), &wasm).unwrap();

        // main() printed StackTrace.current, from line 4.
        let printed = host::trace().into_iter().find(|e| e.call == "console" && e.args.starts_with("stack:")).unwrap();
        assert!(printed.args.contains("throws.dart:4:"), "{}", printed.args);

        // A click throws from explode(), line 9: the message and that line are reported.
        let (button, id) = listener(&page, "#go", "click");
        let event = HostValue::Object(page.pack.objects.borrow_mut().event("click", button, Default::default()));
        let err = format!("{:#}", page.pack.fire(id, &[event]).unwrap_err());
        // Dart's own toString: the error's type shows ("Bad state" is StateError's prefix).
        assert!(err.starts_with("Uncaught Dart error: Bad state: boom from a click"), "{err}");
        assert!(err.contains("at throws.dart:9:3"), "{err}");
        assert!(!err.contains("_internal"), "SDK capture frames are trimmed: {err}");
    }

    fn js_page(html: &str, scripts: &[&str]) -> PackDocument {
        let scripts: Vec<Script> = scripts
            .iter()
            .enumerate()
            .map(|(i, s)| Script { name: format!("test-{}.js", i + 1), source: s.to_string(), module: false })
            .collect();
        js_page_with(html, &scripts, &[])
    }

    /// A JS page whose module imports resolve from `files` (URL → source).
    fn js_page_with(html: &str, scripts: &[Script], files: &[(&str, &str)]) -> PackDocument {
        let doc = HtmlDocument::from_html(html, DocumentConfig::default()).into_inner();
        let files: std::collections::HashMap<String, String> = files.iter().map(|(u, s)| (u.to_string(), s.to_string())).collect();
        let fetch = |url: String| {
            let found = files.get(&url).cloned().ok_or_else(|| anyhow::anyhow!("404 {url}"));
            async move { found }
        };
        futures_lite_block_on(crate::js_pack::start(doc, Default::default(), scripts, fetch)).expect("JS engine missing? run packs/js/build.sh")
    }

    /// Drive a future that never actually waits (test fetches resolve immediately).
    fn futures_lite_block_on<T>(fut: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(fut)
    }

    /// The JS counter example, end to end: its inline script in the JS pack (Boa in wasm),
    /// the same host as Dart, the real Blitz DOM.
    #[test]
    fn js_counter_example_runs_and_handles_clicks() {
        let html = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/js-counter/index.html")).unwrap();
        let script = html.split("<script>").nth(1).unwrap().split("</script>").next().unwrap();
        let page = js_page(&html, &[script]);
        assert_eq!(text_of(&page.pack.doc.borrow(), "#status"), "JavaScript is running, as a pack in WebAssembly.");

        let (button, id) = listener(&page, "#increment", "click");
        for _ in 0..2 {
            let event = HostValue::Object(page.pack.objects.borrow_mut().event("click", button, Default::default()));
            page.pack.fire(id, &[event]).unwrap();
        }
        let doc = page.pack.doc.borrow();
        assert_eq!(text_of(&doc, "#count"), "2");
        assert_eq!(text_of(&doc, "#log"), "Click 1, handled in JS (click)Click 2, handled in JS (click)");
    }

    /// JS sees the web platform through the host and nothing else. (`fetch`, `XMLHttpRequest`
    /// and `WebSocket` exist, but go through the host and the page's gate: see their tests.)
    #[test]
    fn js_has_no_side_doors() {
        let page = js_page(
            "<p id=out></p>",
            &[r#"
                const reachable = ['require', 'process', 'Deno', 'Bun']
                    .filter((name) => typeof globalThis[name] !== 'undefined');
                const r = Math.random();
                document.querySelector('#out').textContent = reachable.length + ' ' + (r >= 0 && r < 1);
            "#],
        );
        assert_eq!(text_of(&page.pack.doc.borrow(), "#out"), "0 true");
        // Math.random went through the host, like Dart's.
        assert!(host::trace().iter().any(|e| e.pack == "js" && e.call == "Math.random"));
    }

    /// A failing script is reported and the next one still runs; host errors reach JS as
    /// exceptions it can catch.
    #[test]
    fn js_errors_are_reported_and_catchable() {
        let page = js_page(
            "<p id=out></p>",
            &[
                "throw new Error('first script broke')",
                r#"
                    let caught = '';
                    try { document.fooBar(); } catch (e) { caught = e.message; }
                    document.querySelector('#out').textContent = caught;
                "#,
            ],
        );
        // JS can't know fooBar is meant as a method, so it is read as a property first.
        assert_eq!(text_of(&page.pack.doc.borrow(), "#out"), "Document.fooBar is not supported yet");
        assert!(host::trace().iter().any(|e| e.pack == "js" && e.result.contains("first script broke")));
    }

    /// The trace says which page each entry came from and what kind it is, so the Trace panel
    /// (and AI access later) can show one tab's console and errors.
    #[test]
    fn trace_entries_carry_their_page_and_kind() {
        let page = js_page("<p></p>", &["console.log('hello from the trace test')", "throw new Error('trace test error')"]);
        let trace = host::trace();
        let logged = trace.iter().find(|e| e.args == "hello from the trace test").expect("console entry");
        assert_eq!(logged.kind, TraceKind::Console);
        assert_eq!(logged.page, page.pack.page);
        let error = trace.iter().find(|e| e.result.contains("trace test error")).expect("error entry");
        assert_eq!(error.kind, TraceKind::Error);
        assert_eq!(error.page, page.pack.page);
    }

    // ---- Cross-pack tests: the same page and behaviour in Dart, JS and Python, through the real
    // ---- event dispatch and timer code. Each must produce identical output.

    use blitz_traits::events::KeyState;
    use keyboard_types::{Code, Key, Location};

    const EVENTS_HTML: &str = include_str!("../tests/fixtures/events.html");

    async fn events_page(pack: &str) -> PackDocument {
        let doc = HtmlDocument::from_html(EVENTS_HTML, DocumentConfig::default()).into_inner();
        match pack {
            "dart" => crate::dart_pack::start(doc, Default::default(), &compile("tests/fixtures/events.dart").await).unwrap(),
            "python" => {
                let script = Script { name: "events.py".into(), source: include_str!("../tests/fixtures/events.py").into(), module: false };
                crate::python_pack::start(doc, Default::default(), &[script]).expect("Python pack missing? run packs/python/build.sh")
            }
            _ => {
                let script = Script { name: "events.js".into(), source: include_str!("../tests/fixtures/events.js").into(), module: false };
                crate::js_pack::start(doc, Default::default(), &[script], |_| async { anyhow::bail!("no imports") }).await.unwrap()
            }
        }
    }

    fn key_down(key: &str, code: Code, ctrl: bool) -> DomEventData {
        DomEventData::KeyDown(BlitzKeyEvent {
            key: Key::Character(key.into()),
            code,
            modifiers: if ctrl { Modifiers::CONTROL } else { Modifiers::empty() },
            location: Location::Standard,
            is_auto_repeating: false,
            is_composing: false,
            state: KeyState::Pressed,
            text: None,
        })
    }

    /// Dispatch `data` at the element matching `selector` the way the browser does; returns
    /// whether the page cancelled Blitz's default action.
    fn dispatch(page: &mut PackDocument, selector: &str, data: DomEventData) -> bool {
        let node = page.pack.doc.borrow().query_selector(selector).unwrap().unwrap();
        let chain = page.pack.doc.borrow().node_chain(node);
        let mut event = DomEvent::new(node, data);
        let mut state = EventState::default();
        let mut doc = page.pack.doc.clone();
        let mut handler = PackEventHandler { pack: &page.pack };
        handler.handle_event(&chain, &mut event, &mut doc, &mut state);
        state.is_cancelled()
    }

    fn output(page: &PackDocument) -> String {
        text_of(&page.pack.doc.borrow(), "#out")
    }

    /// Run timers until none are left (or a second passes), as the event loop would.
    fn settle_timers(page: &mut PackDocument) {
        let until = Instant::now() + std::time::Duration::from_secs(1);
        while page.pack.objects.borrow().next_deadline().is_some() && Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(2));
            page.pack.run_due_timers();
        }
    }

    async fn run_events_scenario(pack: &str) -> Vec<String> {
        let mut page = events_page(pack).await;
        let mut steps = Vec::new();
        assert!(!dispatch(&mut page, "#button", click_at(10.0, 20.0, true)));
        steps.push(output(&page));
        dispatch(&mut page, "#button", click_at(1.0, 2.0, false));
        steps.push(output(&page));
        dispatch(&mut page, "#stop", click_at(0.0, 0.0, false));
        steps.push(output(&page));
        assert!(dispatch(&mut page, "#field", key_down("a", Code::KeyA, true)), "{pack}: keydown preventDefault reaches Blitz");
        steps.push(output(&page));
        dispatch(&mut page, "#stream", click_at(0.0, 0.0, false));
        dispatch(&mut page, "#stream", click_at(0.0, 0.0, false));
        steps.push(output(&page));
        settle_timers(&mut page);
        steps.push(output(&page));

        // No leak: event objects are freed after each dispatch.
        let before = page.pack.objects.borrow().live_objects();
        for _ in 0..50 {
            dispatch(&mut page, "#button", click_at(0.0, 0.0, false));
        }
        assert_eq!(page.pack.objects.borrow().live_objects(), before, "{pack}: event objects leaked");
        steps
    }

    #[tokio::test]
    async fn events_and_timers_behave_the_same_in_dart_js_and_python() {
        let dart = run_events_scenario("dart").await;
        let js = run_events_scenario("js").await;
        let python = run_events_scenario("python").await;
        let expected_events = [
            "once,click:10,20,0,true",
            "once,click:10,20,0,true,click:1,2,0,false",
            "once,click:10,20,0,true,click:1,2,0,false,stop",
            "once,click:10,20,0,true,click:1,2,0,false,stop,key:a,KeyA,true",
            "once,click:10,20,0,true,click:1,2,0,false,stop,key:a,KeyA,true,stream",
        ];
        for (pack, steps) in [("Dart", &dart), ("JS", &js), ("Python", &python)] {
            assert_eq!(steps[..5], expected_events, "{pack}: events");
            // Independent timers may fire in either order (an overdue interval and a frame), so
            // compare the set; the cancelled timeout must never run.
            let after = steps[5].strip_prefix(expected_events[4]).unwrap_or(&steps[5]);
            let mut timers: Vec<&str> = after.split(',').filter(|s| !s.is_empty()).collect();
            timers.sort();
            assert_eq!(timers, ["frame", "interval:3", "timeout"], "{pack}: timers");
        }
    }

    // ---- Cross-pack: the web platform beyond the DOM (cookies, fetch, ...), the same page in
    // ---- each pack, against a local server (nothing leaves the machine).

    /// A small HTTP/1.1 server on 127.0.0.1 for the duration of the test process:
    /// `answer(method, path, headers, body)` gives (status, extra header lines, body).
    /// Returns its base URL.
    fn serve(answer: fn(&str, &str, &str, &str) -> (u16, String, String)) -> String {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut stream = stream;
                    loop {
                        let mut head = String::new();
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                                return;
                            }
                            if line == "\r\n" {
                                break;
                            }
                            head.push_str(&line.to_ascii_lowercase());
                        }
                        let mut words = head.split_whitespace();
                        let (method, path) = (words.next().unwrap_or_default().to_ascii_uppercase(), words.next().unwrap_or_default().to_string());
                        let length = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|n| n.trim().parse().ok()).unwrap_or(0);
                        let mut body = vec![0; length];
                        let _ = reader.read_exact(&mut body);
                        let (status, extra, text) = answer(&method, &path, &head, &String::from_utf8_lossy(&body));
                        let reply = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\n{extra}\r\n{text}", text.len());
                        if stream.write_all(reply.as_bytes()).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        base
    }

    /// The page's own server.
    fn same_origin_answer(method: &str, path: &str, headers: &str, body: &str) -> (u16, String, String) {
        let cookie = if headers.contains("fromserver=1") { "yes" } else { "no" };
        match (method, path) {
            ("GET", "/app/data.json") => (200, "Content-Type: application/json\r\nSet-Cookie: fromserver=1; Path=/\r\n".into(), r#"{"name":"web","n":3,"token":"S3CRET-BODY"}"#.into()),
            ("POST", "/app/echo") => {
                let x_a = headers.lines().find_map(|l| l.strip_prefix("x-a:")).unwrap_or_default().trim();
                (200, "Content-Type: text/plain\r\n".into(), format!("{method} {body} {x_a} cookie:{cookie}"))
            }
            ("GET", "/app/redirect") => (302, "Location: /app/data.json\r\n".into(), String::new()),
            _ => (404, String::new(), "nope".into()),
        }
    }

    /// Another origin (another port): one answer readable cross-origin, one not.
    fn cross_origin_answer(_method: &str, path: &str, headers: &str, _body: &str) -> (u16, String, String) {
        let cookie = if headers.contains("cookie:") { "sent" } else { "none" };
        match path {
            "/cors" => (200, "Access-Control-Allow-Origin: *\r\n".into(), format!("cookies {cookie}")),
            _ => (200, String::new(), "secret".into()),
        }
    }

    /// The tab's history as a page sees it, recording what the page asked of it.
    struct TestNav {
        entries: RefCell<Vec<(String, Option<String>)>>,
        asked: RefCell<Vec<String>>,
    }

    impl host::TabNav for TestNav {
        fn navigate(&self, url: blitz_traits::net::Url, replace: bool) {
            self.asked.borrow_mut().push(format!("navigate {url}{}", if replace { " (replace)" } else { "" }));
        }
        fn reload(&self) {
            self.asked.borrow_mut().push("reload".into());
        }
        fn set_entry(&self, url: blitz_traits::net::Url, state: Option<String>, replace: bool) {
            let mut entries = self.entries.borrow_mut();
            if replace {
                entries.pop();
            }
            entries.push((url.to_string(), state));
        }
        fn go(&self, delta: i64) {
            self.asked.borrow_mut().push(format!("go {delta}"));
        }
        fn state(&self) -> Option<String> {
            self.entries.borrow().last().and_then(|(_, s)| s.clone())
        }
        fn length(&self) -> usize {
            self.entries.borrow().len()
        }
    }

    /// What the web API page prints, in every pack.
    const WEB_API_OUTPUT: &[&str] = &[
        "cookie:a=1; b=2",
        "cookie:b=2",
        "storage:v,2,null,2,k",
        "storage:2,n,tab,1",
        "location:http:,/app/page.html,,,true",
        "push:/app/two,?x=1,2,2",
        "replace:/app/two?x=1#h,three,2",
        "window:400,300,2,0,60,true,en-US",
        // The test resizes the window after the scripts run; then the queued events go.
        "resize:500",
        "popstate:#next,null",
        "hashchange:true",
        "fetch:200,true,application/json,null",
        "json:web,3,11,12",
        "cookie:b=2; fromserver=1",
        "echo:POST hello b cookie:yes",
        "missing:404,false,nope",
        "redirect:200,true",
        "blocked:TypeError",
        "cors:cookies none",
        "nocors:TypeError",
    ];

    fn web_api_program(pack: &str, wasm: &[u8]) -> replay::Program {
        let script = |name: &str, source: &str| vec![Script { name: name.into(), source: source.into(), module: false }];
        match pack {
            "dart" => replay::Program::Dart(wasm.to_vec()),
            "python" => replay::Program::Scripts { pack: "python", scripts: script("web_api.py", include_str!("../tests/fixtures/web_api.py")), imports: Vec::new() },
            _ => replay::Program::Scripts { pack: "js", scripts: script("web_api.js", include_str!("../tests/fixtures/web_api.js")), imports: Vec::new() },
        }
    }

    /// Run the page's timers and deliver its fetches until both are done (or 5 s pass),
    /// letting the network's tasks run in between.
    async fn settle_network(page: &mut PackDocument) {
        let until = Instant::now() + std::time::Duration::from_secs(5);
        while Instant::now() < until {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            // As `poll` does.
            page.pack.deliver_fetches();
            page.pack.run_queued_events();
            page.pack.run_due_timers();
            let objects = page.pack.objects.borrow();
            if objects.fetching == 0 && objects.next_deadline().is_none() && objects.queued_events.is_empty() {
                break;
            }
        }
    }

    /// One page using cookies and fetch (`tests/fixtures/web_api.*`), the same in every pack.
    /// Recorded as it runs: a replay ends with the same DOM after the cookies have changed and
    /// with no network at all, because cookie reads and fetch responses are on the recording.
    /// The AI's redacted trace never shows a cookie value or a response, though the user's own
    /// trace does.
    #[tokio::test]
    async fn web_apis_behave_the_same_in_dart_js_and_python() {
        use blitz_net::CookieStore as _;
        crate::cookies::install();
        let base = serve(same_origin_answer);
        let cross = serve(cross_origin_answer);
        let html = include_str!("../tests/fixtures/web_api.html").replace("CROSS_ORIGIN", &cross);
        let url = blitz_traits::net::Url::parse(&format!("{base}app/page.html")).unwrap();
        let origin = crate::settings::origin_of(&url);
        let gate = std::sync::Arc::new(crate::gate::Gate::new(std::sync::Arc::new(blitz_net::Provider::new(None))));
        let wasm = compile("tests/fixtures/web_api.dart").await;
        let script = |name: &str, source: &str| vec![Script { name: name.into(), source: source.into(), module: false }];
        for pack in ["dart", "js", "python"] {
            // The packs share the server's site, and the profile's jar: start each afresh, with
            // a cookie the server set, HttpOnly, which page code must never see.
            crate::cookies::jar().clear_site("127.0.0.1");
            let server = blitz_net::HeaderValue::from_static("server=secret; HttpOnly; Path=/");
            crate::cookies::jar().set_cookies(&mut [server].iter(), &url);

            replay::arm(url.as_str());
            let viewport = blitz_traits::shell::Viewport::new(800, 600, 2.0, blitz_traits::shell::ColorScheme::Light);
            let doc = HtmlDocument::from_html(&html, DocumentConfig { base_url: Some(url.to_string()), viewport: Some(viewport), ..Default::default() }).into_inner();
            let nav = Rc::new(TestNav { entries: RefCell::new(vec![(url.to_string(), None)]), asked: Default::default() });
            let env = PageEnv { gate: Some(gate.clone()), nav: Some(nav.clone()), ..Default::default() };
            let mut page = match pack {
                "dart" => crate::dart_pack::start(doc, env, &wasm).unwrap(),
                "python" => crate::python_pack::start(doc, env, &script("web_api.py", include_str!("../tests/fixtures/web_api.py"))).unwrap(),
                _ => crate::js_pack::start(doc, env, &script("web_api.js", include_str!("../tests/fixtures/web_api.js")), |_| async { anyhow::bail!("no imports") }).await.unwrap(),
            };
            page.attach_recording(&html, None, web_api_program(pack, &wasm));
            // The window grows (before the network answers): the page gets `resize`.
            page.pack.doc.borrow_mut().set_viewport(blitz_traits::shell::Viewport::new(1000, 600, 2.0, blitz_traits::shell::ColorScheme::Light));
            page.pack.check_resize();
            settle_network(&mut page).await;
            assert_eq!(output(&page).split('|').collect::<Vec<_>>(), WEB_API_OUTPUT, "{pack}: {:?}", errors_on(url.as_str()));
            assert_eq!(*nav.asked.borrow(), ["go -1".to_string(), format!("{base}elsewhere")].map(|s| if s.starts_with("go") { s } else { format!("navigate {s}") }), "{pack}");
            assert!(nav.entries.borrow().last().unwrap().0.ends_with("/app/two?x=1#next"), "{pack}: {:?}", nav.entries.borrow());
            let recording = page.pack.stop_recording().unwrap();
            assert!(recording.tape.iter().any(|(k, _)| *k == replay::Read::Cookie), "{pack}: cookie reads are on the tape");
            assert!(recording.tape.iter().any(|(k, _)| *k == replay::Read::Storage), "{pack}: storage reads are on the tape");
            assert!(recording.steps.iter().any(|s| matches!(s, replay::Step::Fetch { .. })), "{pack}: responses are steps");

            // The jar changes afterwards; the replay still reads what the recording did, sends
            // nothing, and doesn't write to the real jar.
            crate::cookies::jar().set_document_cookie(&url, "b=changed; Path=/");
            crate::storage::local().write(&origin, |a| a.set("n", "changed")).unwrap();
            let config = DocumentConfig { base_url: Some(url.to_string()), ..Default::default() };
            let (mut replayed, outcome) = replay::replay(&recording, config, None).await.unwrap();
            settle_timers(&mut replayed);
            assert_eq!(outcome.diverged, None, "{pack}");
            assert_eq!(replay::dom_text(&replayed.pack().doc.borrow()), recording.final_dom, "{pack}");
            assert_eq!(crate::cookies::jar().document_cookie(&url), "b=changed; fromserver=1", "{pack}: the replay left the jar alone");
            assert_eq!(crate::storage::local().read(&origin, |a| a.get("n")).as_deref(), Some("changed"), "{pack}: and storage");

            let full = debug::trace(&origin, debug::Privacy::Full, usize::MAX);
            assert!(full.iter().any(|e| e.call == "Document.cookie" && e.result.contains("b=2")), "{pack}: the user's trace shows it");
            let redacted = debug::trace(&origin, debug::Privacy::Redacted, usize::MAX);
            assert!(redacted.iter().any(|e| e.call == "Document.cookie") && redacted.iter().any(|e| e.call == "Window.fetch"), "{pack}");
            assert!(full.iter().any(|e| e.call == "Storage.getItem" && e.result == "v"), "{pack}");
            // (What the page itself writes into its DOM is the page's output, and isn't hidden.)
            let secret = |e: &&TraceEntry| debug::holds_secrets(&e.call);
            assert!(redacted.iter().filter(secret).all(|e| e.args == debug::HIDDEN && e.result == debug::HIDDEN), "{pack}: a secret reached the AI's view");
            // Secrets the page reads but never shows: nowhere in the AI's trace or console.
            let console = debug::console(&origin, debug::Privacy::Redacted, usize::MAX);
            let texts = redacted.iter().flat_map(|e| [e.call.clone(), e.args.clone(), e.result.clone()]).chain(console.iter().flat_map(|c| [c.context.clone(), c.text.clone()]));
            for text in texts {
                for secret in ["S3CRET-STORE", "S3CRET-STATE", "S3CRET-BODY", "server=secret"] {
                    assert!(!text.contains(secret), "{pack}: {secret} reached the AI's view in {text:?}");
                }
            }
            assert!(full.iter().any(|e| e.result.contains("S3CRET-STORE") || e.args.contains("S3CRET-STORE")), "{pack}: the user's own trace keeps it");
        }
    }

    // ---- Cross-pack: DOM breadth and form events (`tests/fixtures/dom.*`).

    const DOM_HTML: &str = include_str!("../tests/fixtures/dom.html");

    /// Navigations a document asks for (a form submission), kept.
    #[derive(Default)]
    struct Navigations(Mutex<Vec<String>>);

    impl blitz_traits::navigation::NavigationProvider for Navigations {
        fn navigate_to(&self, options: blitz_traits::navigation::NavigationOptions) {
            self.0.lock().unwrap().push(options.url.to_string());
        }
    }

    /// Deliver `data` to the element `selector` matches the way the browser does: through
    /// Blitz's event driver, so its default actions (typing, toggling, submitting) happen.
    fn drive(page: &PackDocument, selector: &str, data: DomEventData) {
        let node = page.pack.doc.borrow().query_selector(selector).unwrap().unwrap();
        let mut doc = page.pack.doc.clone();
        let mut driver = EventDriver::new(&mut doc, PackEventHandler { pack: &page.pack });
        driver.handle_dom_event(DomEvent::new(node, data));
        page.pack.run_queued_events();
    }

    fn key(key: Key, text: Option<&str>) -> DomEventData {
        DomEventData::KeyDown(BlitzKeyEvent {
            key,
            code: Code::KeyX,
            modifiers: Modifiers::empty(),
            location: Location::Standard,
            is_auto_repeating: false,
            is_composing: false,
            state: KeyState::Pressed,
            text: text.map(Into::into),
        })
    }

    /// What the DOM page prints, in every pack.
    const DOM_OUTPUT: &[&str] = &[
        "query:2,p1,p2,null,one+two,null",
        "tree:3,SPAN,root,p2,p2,three,3,BODY",
        "mutate:p1+EM+p3,p2,0,one,true,false",
        "class:false,true,true,false,2,c d",
        "data:7,x",
        "style:color: red; background-color: blue; margin-top: 4px;,red,red,,10px,width: 10px;",
        r#"html:2,<b class="x">bold &amp; <i>it</i></b><script>document.getElementById("out").textContent = "RAN"</script>,<b class="x">bold &amp; <i>it</i></b>"#,
        "form:init,typed,init,true,true,false,false",
        "secret:12,true",
        "focus",
        "input:typedx",
        "submit",
        "toggle:false",
        "change:typedx",
        "blur",
        "submit",
        "rect:10,20,30,40,40,60",
    ];

    /// One page using the DOM's breadth (queries, the tree, classList, dataset, style,
    /// innerHTML, form fields) and form events (focus, input, change, blur, a cancelled
    /// submit), driven through Blitz's own event handling, the same in every pack. A replay
    /// ends with the same DOM; a password the page reads never reaches the AI's views.
    #[tokio::test]
    async fn the_dom_and_form_events_behave_the_same_in_dart_js_and_python() {
        let wasm = compile("tests/fixtures/dom.dart").await;
        for pack in ["dart", "js", "python"] {
            let url = format!("https://dom-{pack}.test/page.html");
            replay::arm(&url);
            let navigations = Arc::new(Navigations::default());
            let viewport = blitz_traits::shell::Viewport::new(800, 600, 1.0, blitz_traits::shell::ColorScheme::Light);
            let config = DocumentConfig { base_url: Some(url.clone()), viewport: Some(viewport), navigation_provider: Some(navigations.clone()), ..Default::default() };
            let doc = HtmlDocument::from_html(DOM_HTML, config).into_inner();
            let script = |name: &str, source: &str| vec![Script { name: name.into(), source: source.into(), module: false }];
            let (page, program) = match pack {
                "dart" => (crate::dart_pack::start(doc, Default::default(), &wasm).unwrap(), replay::Program::Dart(wasm.clone())),
                "python" => {
                    let scripts = script("dom.py", include_str!("../tests/fixtures/dom.py"));
                    (crate::python_pack::start(doc, Default::default(), &scripts).unwrap(), replay::Program::Scripts { pack: "python", scripts, imports: Vec::new() })
                }
                _ => {
                    let scripts = script("dom.js", include_str!("../tests/fixtures/dom.js"));
                    let page = crate::js_pack::start(doc, Default::default(), &scripts, |_| async { anyhow::bail!("no imports") }).await.unwrap();
                    (page, replay::Program::Scripts { pack: "js", scripts, imports: Vec::new() })
                }
            };
            page.attach_recording(DOM_HTML, None, program);
            // name.focus() queued `focus`; then the user types, presses Enter (submitting,
            // cancelled), ticks the checkbox (the field loses focus), clicks Send (cancelled
            // again), and clicks the box once it's laid out.
            page.pack.run_queued_events();
            drive(&page, "#name", key(Key::Character("x".into()), Some("x")));
            drive(&page, "#name", key(Key::Named(keyboard_types::NamedKey::Enter), None));
            drive(&page, "#agree", click_at(0.0, 0.0, false));
            drive(&page, "#send", click_at(0.0, 0.0, false));
            page.pack.doc.borrow_mut().resolve(0.0);
            drive(&page, "#box", click_at(0.0, 0.0, false));
            assert_eq!(output(&page).split('|').collect::<Vec<_>>(), DOM_OUTPUT, "{pack}: {:?}", errors_on(&url));
            assert!(navigations.0.lock().unwrap().is_empty(), "{pack}: the cancelled submits didn't navigate");

            let recording = page.pack.stop_recording().unwrap();
            assert!(recording.tape.iter().any(|(k, _)| *k == replay::Read::Field), "{pack}: field values are on the tape");
            let config = DocumentConfig { base_url: Some(url.clone()), ..Default::default() };
            let (replayed, outcome) = replay::replay(&recording, config, None).await.unwrap();
            assert_eq!(outcome.diverged, None, "{pack}");
            assert_eq!(replay::dom_text(&replayed.pack().doc.borrow()), recording.final_dom, "{pack}");

            let origin = format!("https://dom-{pack}.test");
            let redacted = debug::trace(&origin, debug::Privacy::Redacted, usize::MAX);
            let console = debug::console(&origin, debug::Privacy::Redacted, usize::MAX);
            assert!(redacted.iter().any(|e| e.call == "Element.value") && redacted.iter().any(|e| e.call == "Element.innerHTML"), "{pack}");
            let texts = redacted.iter().flat_map(|e| [e.call.clone(), e.args.clone(), e.result.clone()]).chain(console.iter().flat_map(|c| [c.context.clone(), c.text.clone()]));
            for text in texts {
                assert!(!text.contains("S3CRET-FIELD"), "{pack}: the password reached the AI's view in {text:?}");
            }
            let full = debug::trace(&origin, debug::Privacy::Full, usize::MAX);
            assert!(full.iter().any(|e| e.result.contains("S3CRET-FIELD")), "{pack}: the user's own trace keeps it");
        }
    }

    /// The DOM refuses to make the tree a loop; nodes dropped by `innerHTML` leave stale
    /// handles that fail (and their listeners go); a submit nobody cancels submits; and
    /// NodeLists the pack drops are freed.
    #[test]
    fn dom_edge_cases() {
        let navigations = Arc::new(Navigations::default());
        let config = DocumentConfig { base_url: Some("https://edge.test/".into()), navigation_provider: Some(navigations.clone()), ..Default::default() };
        let html = "<div id=root><p id=p>x</p></div><form action=done><button id=go>go</button></form><p id=out></p>";
        let doc = HtmlDocument::from_html(html, config).into_inner();
        let script = r#"
            const r = [];
            const root = document.getElementById('root');
            const p = document.getElementById('p');
            p.addEventListener('click', () => {});
            try { p.appendChild(root); } catch (e) { r.push(e.message.split(':')[0]); }
            try { root.removeChild(document.body); } catch (e) { r.push(e.message.split(':')[0]); }
            root.innerHTML = '<i>new</i>';
            try { p.id; } catch (e) { r.push(e.message); }
            for (let i = 0; i < 200; i++) document.querySelectorAll('p');
            document.getElementById('out').textContent = r.join('|');
        "#;
        let scripts = [Script { name: "edge.js".into(), source: script.into(), module: false }];
        let page = futures_lite_block_on(crate::js_pack::start(doc, Default::default(), &scripts, |_| async { anyhow::bail!("no imports") })).unwrap();
        assert_eq!(output(&page), "HierarchyRequestError|NotFoundError|this object is no longer available");
        assert!(page.pack.objects.borrow().listeners.values().all(|ids| ids.is_empty()), "the dropped node's listener went");
        drive(&page, "#go", click_at(0.0, 0.0, false));
        assert_eq!(*navigations.0.lock().unwrap(), ["https://edge.test/done?"], "an uncancelled submit submits");

        // Python frees each NodeList as its wrapper goes.
        let page = python_page("<p></p><p></p>", &["for i in range(200):\n    document.querySelectorAll('p').length"]);
        assert!(page.pack.objects.borrow().live_of("NodeList") <= 1);

        // Synchronous XMLHttpRequest would block the page: refused.
        let sync = js_page("<p id=out></p>", &["const x = new XMLHttpRequest(); let r = ''; try { x.open('GET', '/x', false); } catch (e) { r = e.message.split(':')[0]; } document.getElementById('out').textContent = r;"]);
        assert_eq!(output(&sync), "InvalidAccessError");
    }

    // ---- Cross-pack: WebSocket (`tests/fixtures/ws.*`), against a local server.

    /// A WebSocket server on 127.0.0.1: greets each connection with the cookie it sent,
    /// answers "ping" with a secret the page never shows, and closes with 4000 "done" on
    /// "bye". Returns its port and how many connections it has had.
    fn serve_sockets() -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = connections.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                std::thread::spawn(move || {
                    let mut cookie = String::from("none");
                    let callback = |request: &tungstenite::handshake::server::Request, response| {
                        if let Some(c) = request.headers().get("cookie") {
                            cookie = c.to_str().unwrap_or_default().to_string();
                        }
                        Ok(response)
                    };
                    let Ok(mut ws) = tungstenite::accept_hdr(stream, callback) else { return };
                    let _ = ws.send(tungstenite::Message::text(format!("hello cookie:{cookie}")));
                    while let Ok(message) = ws.read() {
                        match message.to_text().unwrap_or_default() {
                            "ping" => drop(ws.send(tungstenite::Message::text("S3CRET-WS-PONG"))),
                            "bye" => drop(ws.close(Some(tungstenite::protocol::CloseFrame { code: 4000.into(), reason: "done".into() }))),
                            _ => {}
                        }
                    }
                });
            }
        });
        (port, connections)
    }

    const WS_OUTPUT: &[&str] = &["state:0,true", "open:1,", "message:hello cookie:sock=1", "message:14", "close:4000,done,true,3", "blocked:error", "blocked:close:1006,false"];

    /// One page using a WebSocket (open, send, messages, a close from the server, a blocked
    /// third party), the same in every pack; the handshake carried the page's SameSite=Strict
    /// cookie. A replay ends with the same DOM and opens no connection; the message the page
    /// never shows doesn't reach the AI's views.
    #[tokio::test]
    async fn web_sockets_behave_the_same_in_dart_js_and_python() {
        let (port, connections) = serve_sockets();
        let url = format!("http://localhost:{port}/app/ws.html");
        let page_url = blitz_traits::net::Url::parse(&url).unwrap();
        crate::cookies::jar().set_document_cookie(&page_url, "sock=1; SameSite=Strict; Path=/");
        let html = include_str!("../tests/fixtures/ws.html").replace("WS_URL", &format!("ws://localhost:{port}/socket"));
        let gate = Arc::new(crate::gate::Gate::new(Arc::new(blitz_net::Provider::new(None))));
        let wasm = compile("tests/fixtures/ws.dart").await;
        let script = |name: &str, source: &str| vec![Script { name: name.into(), source: source.into(), module: false }];
        for pack in ["dart", "js", "python"] {
            replay::arm(&url);
            let doc = HtmlDocument::from_html(&html, DocumentConfig { base_url: Some(url.clone()), ..Default::default() }).into_inner();
            let env = PageEnv { gate: Some(gate.clone()), ..Default::default() };
            let (page, program) = match pack {
                "dart" => (crate::dart_pack::start(doc, env, &wasm).unwrap(), replay::Program::Dart(wasm.clone())),
                "python" => {
                    let scripts = script("ws.py", include_str!("../tests/fixtures/ws.py"));
                    (crate::python_pack::start(doc, env, &scripts).unwrap(), replay::Program::Scripts { pack: "python", scripts, imports: Vec::new() })
                }
                _ => {
                    let scripts = script("ws.js", include_str!("../tests/fixtures/ws.js"));
                    let page = crate::js_pack::start(doc, env, &scripts, |_| async { anyhow::bail!("no imports") }).await.unwrap();
                    (page, replay::Program::Scripts { pack: "js", scripts, imports: Vec::new() })
                }
            };
            page.attach_recording(&html, None, program);
            let until = Instant::now() + Duration::from_secs(5);
            while output(&page).split('|').count() < WS_OUTPUT.len() && Instant::now() < until {
                tokio::time::sleep(Duration::from_millis(5)).await;
                page.pack.deliver_sockets();
            }
            assert_eq!(output(&page).split('|').collect::<Vec<_>>(), WS_OUTPUT, "{pack}: {:?}", errors_on(&url));

            let recording = page.pack.stop_recording().unwrap();
            assert!(recording.steps.iter().any(|s| matches!(s, replay::Step::Socket { .. })), "{pack}: socket events are steps");
            let before = connections.load(std::sync::atomic::Ordering::Relaxed);
            let config = DocumentConfig { base_url: Some(url.clone()), ..Default::default() };
            let (replayed, outcome) = replay::replay(&recording, config, None).await.unwrap();
            assert_eq!(outcome.diverged, None, "{pack}");
            assert_eq!(replay::dom_text(&replayed.pack().doc.borrow()), recording.final_dom, "{pack}");
            assert_eq!(connections.load(std::sync::atomic::Ordering::Relaxed), before, "{pack}: the replay opened no socket");
            // Replayed to just after it opened, then live: the socket can't go on (there's no
            // network), so it closes rather than hanging open.
            let config = DocumentConfig { base_url: Some(url.clone()), ..Default::default() };
            let (partial, _) = replay::replay(&recording, config, Some(1)).await.unwrap();
            partial.pack().deliver_sockets();
            assert!(output(&partial).contains("close:1006,,false,3"), "{pack}: {}", output(&partial));

            let origin = format!("http://localhost:{port}");
            let redacted = debug::trace(&origin, debug::Privacy::Redacted, usize::MAX);
            let console = debug::console(&origin, debug::Privacy::Redacted, usize::MAX);
            assert!(redacted.iter().any(|e| e.call == "Event.data"), "{pack}");
            let texts = redacted.iter().flat_map(|e| [e.args.clone(), e.result.clone()]).chain(console.iter().flat_map(|c| [c.context.clone(), c.text.clone()]));
            for text in texts {
                assert!(!text.contains("S3CRET-WS"), "{pack}: a message reached the AI's view in {text:?}");
            }
            assert!(debug::trace(&origin, debug::Privacy::Full, usize::MAX).iter().any(|e| e.result.contains("S3CRET-WS")), "{pack}");
        }
    }

    /// A page's sockets close when it goes; a secure page can't open an insecure one.
    #[test]
    fn sockets_close_when_the_page_goes() {
        let (port, connections) = serve_sockets();
        let url = format!("http://localhost:{port}/");
        let gate = Arc::new(crate::gate::Gate::new(Arc::new(blitz_net::Provider::new(None))));
        let doc = HtmlDocument::from_html("<p id=out></p>", DocumentConfig { base_url: Some(url.clone()), ..Default::default() }).into_inner();
        let source = format!("const ws = new WebSocket('ws://localhost:{port}/'); ws.onopen = () => {{ document.getElementById('out').textContent = 'open'; }};");
        let scripts = [Script { name: "s.js".into(), source, module: false }];
        let page = futures_lite_block_on(crate::js_pack::start(doc, PageEnv { gate: Some(gate), ..Default::default() }, &scripts, |_| async { anyhow::bail!("no imports") })).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while output(&page) != "open" && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
            page.pack.deliver_sockets();
        }
        assert_eq!(output(&page), "open");
        let inbox = page.pack.objects.borrow().inbox.clone();
        drop(page);
        // The thread sees the page go and closes: nothing more arrives, and it lets go of the inbox.
        let until = Instant::now() + Duration::from_secs(5);
        while Arc::strong_count(&inbox.events) > 1 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(Arc::strong_count(&inbox.events), 1, "the connection's thread ended");
        assert_eq!(connections.load(std::sync::atomic::Ordering::Relaxed), 1);

        let doc = HtmlDocument::from_html("<p id=out></p>", DocumentConfig { base_url: Some("https://secure.test/".into()), ..Default::default() }).into_inner();
        let source = "let r = ''; try { new WebSocket('ws://secure.test/'); } catch (e) { r = e.message.split(':')[0]; } document.getElementById('out').textContent = r;";
        let scripts = [Script { name: "secure.js".into(), source: source.into(), module: false }];
        let secure = futures_lite_block_on(crate::js_pack::start(doc, PageEnv::default(), &scripts, |_| async { anyhow::bail!("no imports") })).unwrap();
        assert_eq!(output(&secure), "SecurityError");
    }

    // ---- Cross-pack: IndexedDB (`tests/fixtures/idb.*`).

    const IDB_OUTPUT: &[&str] = &[
        "upgrade:2,0",
        "open:shop,2,items+notes",
        "put:1",
        "add:ConstraintError",
        "complete",
        "store:id,true",
        "get:apple,2,fruit,10",
        "missing:true",
        "odd:1",
        "all:apple+pear",
        "count:2",
        "after-delete:1",
        "cleared:0",
        "done",
    ];

    /// Deliver what a page queued (IndexedDB events) and run its timers until nothing's left.
    fn settle_queued(page: &PackDocument) {
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until {
            let ran = page.pack.run_queued_events() | page.pack.run_due_timers();
            let objects = page.pack.objects.borrow();
            if !ran && objects.queued_object_events.is_empty() && objects.next_deadline().is_none() {
                break;
            }
        }
    }

    /// One page using IndexedDB (an upgrade creating stores, transactions, put/add/get/
    /// getAll/count/delete/clear, a constraint error), the same in every pack, kept in the
    /// profile's databases. A replay ends with the same DOM without touching them; the value
    /// the page never shows doesn't reach the AI's views.
    #[tokio::test]
    async fn indexed_db_behaves_the_same_in_dart_js_and_python() {
        let wasm = compile("tests/fixtures/idb.dart").await;
        let html = include_str!("../tests/fixtures/idb.html");
        for pack in ["dart", "js", "python"] {
            let url = format!("https://idb-{pack}.test/");
            let origin = format!("https://idb-{pack}.test");
            replay::arm(&url);
            let doc = HtmlDocument::from_html(html, DocumentConfig { base_url: Some(url.clone()), ..Default::default() }).into_inner();
            let script = |name: &str, source: &str| vec![Script { name: name.into(), source: source.into(), module: false }];
            let (page, program) = match pack {
                "dart" => (crate::dart_pack::start(doc, Default::default(), &wasm).unwrap(), replay::Program::Dart(wasm.clone())),
                "python" => {
                    let scripts = script("idb.py", include_str!("../tests/fixtures/idb.py"));
                    (crate::python_pack::start(doc, Default::default(), &scripts).unwrap(), replay::Program::Scripts { pack: "python", scripts, imports: Vec::new() })
                }
                _ => {
                    let scripts = script("idb.js", include_str!("../tests/fixtures/idb.js"));
                    let page = crate::js_pack::start(doc, Default::default(), &scripts, |_| async { anyhow::bail!("no imports") }).await.unwrap();
                    (page, replay::Program::Scripts { pack: "js", scripts, imports: Vec::new() })
                }
            };
            page.attach_recording(html, None, program);
            settle_queued(&page);
            assert_eq!(output(&page).split('|').collect::<Vec<_>>(), IDB_OUTPUT, "{pack}: {:?}", errors_on(&url));
            assert_eq!(crate::idb::local().run(&origin, false, |o| Ok(o.store_names("shop"))), Ok(vec!["items".to_string(), "notes".to_string()]), "{pack}: kept");
            // Python lets go of each request as its wrappers go: they're freed once done.
            if pack == "python" {
                assert!(page.pack.objects.borrow().live_of("IDBRequest") <= 2, "{pack}: requests piled up");
            }

            let recording = page.pack.stop_recording().unwrap();
            assert!(recording.tape.iter().any(|(k, _)| *k == replay::Read::Idb), "{pack}: outcomes are on the tape");
            // The databases change meanwhile; the replay reads what the recording did, and
            // changes nothing.
            crate::idb::local().run(&origin, true, |o| o.put("shop", "notes", serde_json::json!("changed"), Some(serde_json::json!("greeting")), false)).unwrap();
            let config = DocumentConfig { base_url: Some(url.clone()), ..Default::default() };
            let (replayed, outcome) = replay::replay(&recording, config, None).await.unwrap();
            assert_eq!(outcome.diverged, None, "{pack}");
            assert_eq!(replay::dom_text(&replayed.pack().doc.borrow()), recording.final_dom, "{pack}");
            assert_eq!(crate::idb::local().run(&origin, false, |o| o.get("shop", "notes", &serde_json::json!("greeting"))), Ok(Some(serde_json::json!("changed"))), "{pack}");

            let redacted = debug::trace(&origin, debug::Privacy::Redacted, usize::MAX);
            let console = debug::console(&origin, debug::Privacy::Redacted, usize::MAX);
            assert!(redacted.iter().any(|e| e.call == "IDBObjectStore.put"), "{pack}");
            let texts = redacted.iter().flat_map(|e| [e.args.clone(), e.result.clone()]).chain(console.iter().flat_map(|c| [c.context.clone(), c.text.clone()]));
            for text in texts {
                assert!(!text.contains("S3CRET-IDB"), "{pack}: a stored value reached the AI's view in {text:?}");
            }
        }
    }

    /// IndexedDB's rules: stores change only during an upgrade, a read-only transaction can't
    /// write, an older version can't be opened, and a page with no origin has none.
    #[test]
    fn indexed_db_refuses_what_it_should() {
        let script = r#"
            const r = [];
            const name = (e) => String(e.message || e).split(':')[0];
            const open = indexedDB.open('rules', 3);
            open.onupgradeneeded = () => open.result.createObjectStore('s');
            open.onsuccess = () => {
                const db = open.result;
                try { db.createObjectStore('t'); } catch (e) { r.push(name(e)); }
                try { db.transaction('s').objectStore('s').put(1, 1); } catch (e) { r.push(name(e)); }
                try { db.transaction('nope'); } catch (e) { r.push(name(e)); }
                const older = indexedDB.open('rules', 2);
                older.onerror = () => { r.push(older.error.name); document.getElementById('out').textContent = r.join('|'); };
            };
        "#;
        let doc = HtmlDocument::from_html("<p id=out></p>", DocumentConfig { base_url: Some("https://idb-rules.test/".into()), ..Default::default() }).into_inner();
        let scripts = [Script { name: "rules.js".into(), source: script.into(), module: false }];
        let page = futures_lite_block_on(crate::js_pack::start(doc, Default::default(), &scripts, |_| async { anyhow::bail!("no imports") })).unwrap();
        settle_queued(&page);
        assert_eq!(output(&page), "InvalidStateError|ReadOnlyError|NotFoundError|VersionError");

        let blank = js_page("<p id=out></p>", &["let r = ''; try { indexedDB.open('x'); } catch (e) { r = e.message.split(':')[0]; } document.getElementById('out').textContent = r;"]);
        assert_eq!(output(&blank), "SecurityError");
    }

    // ---- Cross-pack: Canvas 2D (`tests/fixtures/canvas.*`).

    const CANVAS_OUTPUT: &[&str] = &["size:60,40,null", "style:#000000,#008000,4,14px sans-serif", "url:true"];

    /// The canvas element's pixels, as Blitz will paint them.
    fn canvas_pixels(page: &PackDocument) -> (u32, Vec<u8>) {
        page.pack.show_canvases();
        let doc = page.pack.doc.borrow();
        let id = doc.query_selector("#c").unwrap().unwrap();
        let image = doc.get_node(id).unwrap().element_data().unwrap().raster_image_data().expect("the canvas shows its pixels");
        (image.width, image.data.data().to_vec())
    }

    /// One page drawing on a canvas (rectangles, a stroke, an arc, a closed path, a clear,
    /// text), the same in every pack, pixel for pixel; the pixels become the canvas element's
    /// image. A replay draws the same.
    #[tokio::test]
    async fn canvas_draws_the_same_in_dart_js_and_python() {
        let wasm = compile("tests/fixtures/canvas.dart").await;
        let html = include_str!("../tests/fixtures/canvas.html");
        let mut drawn = Vec::new();
        for pack in ["dart", "js", "python"] {
            let url = format!("https://canvas-{pack}.test/");
            replay::arm(&url);
            let doc = HtmlDocument::from_html(html, DocumentConfig { base_url: Some(url.clone()), ..Default::default() }).into_inner();
            let script = |name: &str, source: &str| vec![Script { name: name.into(), source: source.into(), module: false }];
            let (page, program) = match pack {
                "dart" => (crate::dart_pack::start(doc, Default::default(), &wasm).unwrap(), replay::Program::Dart(wasm.clone())),
                "python" => {
                    let scripts = script("canvas.py", include_str!("../tests/fixtures/canvas.py"));
                    (crate::python_pack::start(doc, Default::default(), &scripts).unwrap(), replay::Program::Scripts { pack: "python", scripts, imports: Vec::new() })
                }
                _ => {
                    let scripts = script("canvas.js", include_str!("../tests/fixtures/canvas.js"));
                    let page = crate::js_pack::start(doc, Default::default(), &scripts, |_| async { anyhow::bail!("no imports") }).await.unwrap();
                    (page, replay::Program::Scripts { pack: "js", scripts, imports: Vec::new() })
                }
            };
            page.attach_recording(html, None, program);
            assert_eq!(output(&page).split('|').collect::<Vec<_>>(), CANVAS_OUTPUT, "{pack}: {:?}", errors_on(&url));
            let (width, pixels) = canvas_pixels(&page);
            let at = |x: u32, y: u32| -> [u8; 4] { pixels[((y * width + x) * 4) as usize..][..4].try_into().unwrap() };
            assert_eq!(width, 60);
            assert_eq!(at(10, 5), [255, 0, 0, 255], "{pack}: fillRect");
            assert_eq!(at(1, 1), [0, 0, 0, 0], "{pack}: clearRect");
            assert_eq!(at(25, 7), [0, 128, 0, 255], "{pack}: strokeRect, 4 wide");
            assert_eq!(at(30, 7), [0, 0, 0, 0], "{pack}: inside the stroke");
            assert_eq!(at(10, 25), [0, 0, 255, 255], "{pack}: the arc");
            assert_eq!(at(36, 24), [255, 0, 255, 255], "{pack}: the closed path");
            assert_eq!(at(28, 31), [0, 0, 0, 0], "{pack}: outside it");
            let text = (42..60).flat_map(|x| (0..16).map(move |y| (x, y))).any(|(x, y)| at(x, y)[3] > 0);
            assert!(text, "{pack}: fillText drew something");
            drawn.push(pixels.clone());

            let recording = page.pack.stop_recording().unwrap();
            let config = DocumentConfig { base_url: Some(url.clone()), ..Default::default() };
            let (replayed, outcome) = replay::replay(&recording, config, None).await.unwrap();
            assert_eq!(outcome.diverged, None, "{pack}");
            assert_eq!(canvas_pixels(&replayed).1, pixels, "{pack}: the replay draws the same");
        }
        assert!(drawn.windows(2).all(|w| w[0] == w[1]), "the same pixels in every pack");
    }

    /// One XMLHttpRequest reused from its own `load` handler sends again, keeps its handlers,
    /// and holds one response's bytes at a time.
    #[tokio::test]
    async fn an_xhr_can_be_reused() {
        let base = serve(xhr_answer);
        let url = format!("{base}app/page.html");
        let gate = Arc::new(crate::gate::Gate::new(Arc::new(blitz_net::Provider::new(None))));
        let doc = HtmlDocument::from_html("<p id=out></p>", DocumentConfig { base_url: Some(url.clone()), ..Default::default() }).into_inner();
        let script = r#"
            const x = new XMLHttpRequest();
            let loads = 0, refused = '';
            x.onload = () => {
                loads++;
                document.getElementById('out').textContent = `${loads},${x.status},${refused}`;
                if (loads < 3) {
                    x.open('GET', 'data.json');
                    x.send();
                    try { x.open('GET', 'data.json'); } catch (e) { refused = e.message.split(':')[0]; }
                }
            };
            x.open('GET', 'data.json');
            x.send();
        "#;
        let scripts = [Script { name: "reuse.js".into(), source: script.into(), module: false }];
        let mut page = crate::js_pack::start(doc, PageEnv { gate: Some(gate), ..Default::default() }, &scripts, |_| async { anyhow::bail!("no imports") }).await.unwrap();
        settle_network(&mut page).await;
        assert_eq!(output(&page), "3,200,InvalidStateError");
        let body = r#"{"name":"web","n":3,"token":"S3CRET-BODY"}"#.len();
        assert_eq!(page.pack.objects.borrow().body_bytes, body, "one response held");
    }

    /// Keys typed into a form field (a password) never reach the AI's views, even when a
    /// listener reads them; keys on other elements still do.
    #[test]
    fn keys_typed_into_fields_stay_out_of_ai_views() {
        let url = "https://keys.test/";
        let html = "<input id=pw type=password><button id=b>b</button><p id=out></p>";
        let doc = HtmlDocument::from_html(html, DocumentConfig { base_url: Some(url.into()), ..Default::default() }).into_inner();
        let script = "let n = 0; const count = (e) => { e.key; e.code; document.getElementById('out').textContent = ++n; };
            document.getElementById('pw').addEventListener('keydown', count);
            document.getElementById('b').addEventListener('keydown', count);";
        let scripts = [Script { name: "keys.js".into(), source: script.into(), module: false }];
        let mut page = futures_lite_block_on(crate::js_pack::start(doc, Default::default(), &scripts, |_| async { anyhow::bail!("no imports") })).unwrap();
        dispatch(&mut page, "#pw", key_down("Q", Code::KeyQ, false));
        dispatch(&mut page, "#b", key_down("Z", Code::KeyZ, false));
        assert_eq!(output(&page), "2");
        let redacted = debug::trace("https://keys.test", debug::Privacy::Redacted, usize::MAX);
        assert!(redacted.iter().any(|e| e.call == "FieldKeyEvent.key" && e.result == debug::HIDDEN));
        assert!(!redacted.iter().any(|e| e.result == "Q" || e.result == "KeyQ"), "a password key reached the AI's view");
        assert!(redacted.iter().any(|e| e.call == "Event.key" && e.result == "Z"), "other keys still show");
    }

    /// Host errors don't echo the URLs and values pages pass them, so a secret in one can't
    /// reach the AI's views through an error message: uncaught, or a failed call's.
    #[tokio::test]
    async fn error_messages_dont_carry_secrets_to_ai() {
        let url = "https://err-secrets.test/";
        let doc = HtmlDocument::from_html("<p id=out></p>", DocumentConfig { base_url: Some(url.into()), ..Default::default() }).into_inner();
        let script = r#"
            const rethrow = (f) => { try { f(); } catch (e) { setTimeout(() => { throw e; }, 0); } };
            rethrow(() => new WebSocket('ws://x.test/?token=S3CRET-ERR'));
            rethrow(() => new WebSocket('http://[bad?token=S3CRET-ERR'));
            rethrow(() => history.pushState(null, '', 'https://other.test/?token=S3CRET-ERR'));
            rethrow(() => location.assign('http://[bad?token=S3CRET-ERR'));
            fetch('http://[bad?token=S3CRET-ERR').catch((e) => { throw e; });
            fetch('/missing?token=S3CRET-ERR').catch((e) => { throw e; });
            try { localStorage.setItem('k', 'S3CRET-ERR'.repeat(1 << 20)); } catch (e) { setTimeout(() => { throw e; }, 0); }
        "#;
        let scripts = [Script { name: "err.js".into(), source: script.into(), module: false }];
        let mut page = crate::js_pack::start(doc, PageEnv::default(), &scripts, |_| async { anyhow::bail!("no imports") }).await.unwrap();
        settle_network(&mut page).await;
        let origin = "https://err-secrets.test";
        let errors = debug::trace(origin, debug::Privacy::Full, usize::MAX).into_iter().filter(|e| e.kind == TraceKind::Error).count();
        assert!(errors >= 6, "the errors happened: {errors}");
        let redacted = debug::trace(origin, debug::Privacy::Redacted, usize::MAX);
        let console = debug::console(origin, debug::Privacy::Redacted, usize::MAX);
        let texts = redacted.iter().flat_map(|e| [e.call.clone(), e.args.clone(), e.result.clone()]).chain(console.iter().flat_map(|c| [c.context.clone(), c.text.clone()]));
        for text in texts {
            assert!(!text.contains("S3CRET-ERR"), "a secret reached the AI's view in {text:?}");
        }
    }

    /// A canvas's size from its HTML gets the same limit as one set from code, and a page's
    /// canvases are bounded in number and pixels.
    #[test]
    fn canvases_are_bounded() {
        let script = r#"
            const c = document.getElementById('c');
            c.getContext('2d').fillRect(0, 0, 10, 10);
            const r = [c.width, c.height, c.toDataURL().length > 0];
            let made = 0;
            try {
                for (let i = 0; i < 300; i++) {
                    const extra = document.createElement('canvas');
                    document.body.appendChild(extra);
                    extra.getContext('2d');
                    made++;
                }
            } catch (e) { r.push(e.message.split(':')[0]); }
            r.push(made);
            document.getElementById('out').textContent = r.join(',');
        "#;
        let page = js_page("<canvas id=c width=65535 height=65535></canvas><p id=out></p>", &[script]);
        assert_eq!(output(&page), "300,150,true,RangeError,255");
    }

    /// Blitz paints the canvas's pixels where the canvas is laid out, as the window would.
    #[test]
    fn blitz_paints_the_canvas() {
        let viewport = blitz_traits::shell::Viewport::new(100, 80, 1.0, blitz_traits::shell::ColorScheme::Light);
        let doc = HtmlDocument::from_html("<body style='margin: 0'><canvas id=c width=50 height=30></canvas>", DocumentConfig { viewport: Some(viewport), ..Default::default() }).into_inner();
        let script = "const c = document.getElementById('c').getContext('2d'); c.fillStyle = 'red'; c.fillRect(10, 10, 20, 10);";
        let scripts = [Script { name: "paint.js".into(), source: script.into(), module: false }];
        let page = futures_lite_block_on(crate::js_pack::start(doc, Default::default(), &scripts, |_| async { anyhow::bail!("no imports") })).unwrap();
        page.pack.show_canvases();
        let mut doc = page.pack.doc.borrow_mut();
        doc.resolve(0.0);
        let pixels = anyrender::render_to_buffer::<anyrender_vello_cpu::VelloCpuImageRenderer, _>(|scene| blitz_paint::paint_scene(scene, &mut doc, 1.0, 100, 80, 0, 0), 100, 80);
        let at = |x: usize, y: usize| pixels[(y * 100 + x) * 4..][..4].to_vec();
        assert_eq!(at(15, 15), [255, 0, 0, 255], "inside the drawn rectangle");
        assert_eq!(at(5, 5)[..3], [0, 0, 0], "the rest of the canvas is transparent");
    }

    // ---- Cross-pack: XMLHttpRequest (`tests/fixtures/xhr.*`), against a local server.

    fn xhr_answer(method: &str, path: &str, headers: &str, body: &str) -> (u16, String, String) {
        match (method, path) {
            ("GET", "/app/data.json") => (200, "Content-Type: application/json\r\n".into(), r#"{"name":"web","n":3,"token":"S3CRET-BODY"}"#.into()),
            ("POST", "/app/echo") => {
                let x_a = headers.lines().find_map(|l| l.strip_prefix("x-a:")).unwrap_or_default().trim();
                (200, "Content-Type: text/plain\r\n".into(), format!("{method} {body} {x_a}"))
            }
            _ => (404, String::new(), "nope".into()),
        }
    }

    /// One page using XMLHttpRequest (a GET, a POST with a header and body, a blocked third
    /// party), the same in every pack, on fetch's path; replayed without the network; the
    /// body the page never shows doesn't reach the AI's views.
    #[tokio::test]
    async fn xml_http_request_behaves_the_same_in_dart_js_and_python() {
        let base = serve(xhr_answer);
        let url = format!("{base}app/xhr.html");
        let origin = crate::settings::origin_of(&blitz_traits::net::Url::parse(&url).unwrap());
        let gate = Arc::new(crate::gate::Gate::new(Arc::new(blitz_net::Provider::new(None))));
        let wasm = compile("tests/fixtures/xhr.dart").await;
        let html = include_str!("../tests/fixtures/xhr.html");
        let script = |name: &str, source: &str| vec![Script { name: name.into(), source: source.into(), module: false }];
        for pack in ["dart", "js", "python"] {
            replay::arm(&url);
            let doc = HtmlDocument::from_html(html, DocumentConfig { base_url: Some(url.clone()), ..Default::default() }).into_inner();
            let env = PageEnv { gate: Some(gate.clone()), ..Default::default() };
            let (mut page, program) = match pack {
                "dart" => (crate::dart_pack::start(doc, env, &wasm).unwrap(), replay::Program::Dart(wasm.clone())),
                "python" => {
                    let scripts = script("xhr.py", include_str!("../tests/fixtures/xhr.py"));
                    (crate::python_pack::start(doc, env, &scripts).unwrap(), replay::Program::Scripts { pack: "python", scripts, imports: Vec::new() })
                }
                _ => {
                    let scripts = script("xhr.js", include_str!("../tests/fixtures/xhr.js"));
                    let page = crate::js_pack::start(doc, env, &scripts, |_| async { anyhow::bail!("no imports") }).await.unwrap();
                    (page, replay::Program::Scripts { pack: "js", scripts, imports: Vec::new() })
                }
            };
            page.attach_recording(html, None, program);
            settle_network(&mut page).await;
            assert_eq!(output(&page), "state:4|load:200,application/json,true|echo:POST hello b|blocked:0,4|end", "{pack}: {:?}", errors_on(&url));
            let recording = page.pack.stop_recording().unwrap();
            let config = DocumentConfig { base_url: Some(url.clone()), ..Default::default() };
            let (replayed, outcome) = replay::replay(&recording, config, None).await.unwrap();
            assert_eq!(outcome.diverged, None, "{pack}");
            assert_eq!(replay::dom_text(&replayed.pack().doc.borrow()), recording.final_dom, "{pack}");
            let redacted = debug::trace(&origin, debug::Privacy::Redacted, usize::MAX);
            assert!(redacted.iter().any(|e| e.call == "XMLHttpRequest.responseText"), "{pack}");
            assert!(redacted.iter().all(|e| !e.args.contains("S3CRET-BODY") && !e.result.contains("S3CRET-BODY")), "{pack}: the body reached the AI's view");
        }
    }

    /// Storage over the limit throws, as `QuotaExceededError`; a page with no origin has no
    /// storage; sessionStorage is the tab's (another tab's is separate).
    #[test]
    fn storage_quota_origins_and_tabs() {
        let script = r#"
            let quota = 'stored';
            try { localStorage.setItem('big', 'x'.repeat(6 * 1024 * 1024)); } catch (e) { quota = e.message.split(':')[0]; }
            sessionStorage.setItem('mine', '1');
            document.querySelector('#out').textContent = quota + ',' + localStorage.getItem('big');
        "#;
        let doc = HtmlDocument::from_html("<p id=out></p>", DocumentConfig { base_url: Some("https://quota.test/".into()), ..Default::default() }).into_inner();
        let scripts = [Script { name: "quota.js".into(), source: script.into(), module: false }];
        let tab: crate::storage::Session = Default::default();
        let page = futures_lite_block_on(crate::js_pack::start(doc, PageEnv { session: tab.clone(), ..Default::default() }, &scripts, |_| async { anyhow::bail!("no imports") })).unwrap();
        assert_eq!(output(&page), "QuotaExceededError,null");
        // The failed call's value doesn't reach the AI's console.
        let console = debug::console("https://quota.test", debug::Privacy::Redacted, usize::MAX);
        assert!(console.iter().any(|c| c.context.starts_with("Storage.setItem")), "{console:?}");
        assert!(!console.iter().any(|c| c.context.contains("xxxx") || c.text.contains("xxxx")));
        assert_eq!(tab.borrow()["https://quota.test"].get("mine").as_deref(), Some("1"), "the tab's session storage");

        let blank = js_page("<p id=out></p>", &["let r = ''; try { localStorage.getItem('x'); } catch (e) { r = e.message.split(':')[0]; } document.querySelector('#out').textContent = r;"]);
        assert_eq!(output(&blank), "SecurityError");
    }

    /// The tab going back to another entry of the page's document gives the page that URL and
    /// state, then `popstate` (and `hashchange` for a new fragment); a replay does the same.
    #[tokio::test]
    async fn moving_within_a_document_fires_popstate_and_replays() {
        let url = "https://traverse.test/a";
        replay::arm(url);
        let html = "<p id=out></p>";
        let doc = HtmlDocument::from_html(html, DocumentConfig { base_url: Some(url.into()), ..Default::default() }).into_inner();
        let scripts = vec![Script { name: "t.js".into(), source: "
            const out = document.querySelector('#out');
            window.addEventListener('popstate', (e) => { out.textContent += `pop:${location.pathname}${location.hash},${e.state && e.state.n};`; });
            window.addEventListener('hashchange', () => { out.textContent += 'hash;'; });".into(), module: false }];
        let page = crate::js_pack::start(doc, PageEnv::default(), &scripts, |_| async { anyhow::bail!("no imports") }).await.unwrap();
        page.attach_recording(html, None, replay::Program::Scripts { pack: "js", scripts, imports: Vec::new() });
        page.pack.traverse("https://traverse.test/b", Some(r#"{"n":1}"#.into()));
        page.pack.traverse("https://traverse.test/b#x", None);
        page.pack.traverse("https://traverse.test/b#x", None);
        assert_eq!(output(&page), "pop:/b,1;pop:/b#x,null;hash;", "the last one changed nothing");
        let recording = page.pack.stop_recording().unwrap();
        let config = DocumentConfig { base_url: Some(url.into()), ..Default::default() };
        let (replayed, outcome) = replay::replay(&recording, config, None).await.unwrap();
        assert_eq!(outcome.diverged, None);
        assert_eq!(replay::dom_text(&replayed.pack().doc.borrow()), recording.final_dom);
        assert_eq!(replayed.pack().doc.borrow().url().as_str(), "https://traverse.test/b#x");
    }

    /// Fetches that fail without reaching the network (a bad URL, no network) replay the same.
    #[tokio::test]
    async fn failed_fetches_replay_without_diverging() {
        let url = "https://fetch-fails.test/";
        replay::arm(url);
        let html = "<p id=out></p>";
        let doc = HtmlDocument::from_html(html, DocumentConfig { base_url: Some(url.into()), ..Default::default() }).into_inner();
        let scripts = vec![Script { name: "f.js".into(), source: "
            const out = document.querySelector('#out');
            fetch('http://[bad').catch((e) => { out.textContent += 'bad:' + e.name + ';'; });
            fetch('/x').catch((e) => { out.textContent += 'nonet:' + e.name + ';'; });".into(), module: false }];
        let mut page = crate::js_pack::start(doc, PageEnv::default(), &scripts, |_| async { anyhow::bail!("no imports") }).await.unwrap();
        page.attach_recording(html, None, replay::Program::Scripts { pack: "js", scripts, imports: Vec::new() });
        settle_network(&mut page).await;
        assert_eq!(output(&page), "bad:TypeError;nonet:TypeError;");
        let recording = page.pack.stop_recording().unwrap();
        let config = DocumentConfig { base_url: Some(url.into()), ..Default::default() };
        let (mut replayed, outcome) = replay::replay(&recording, config, None).await.unwrap();
        settle_timers(&mut replayed);
        assert_eq!(outcome.diverged, None);
        assert_eq!(replay::dom_text(&replayed.pack().doc.borrow()), recording.final_dom);
    }

    /// Many fetches don't pile up host objects: promises go once their callbacks are queued,
    /// and responses, headers and parsed data go when the pack drops them (Python's wrappers
    /// say so as they're freed; JS's when its garbage collector finalizes them, so only the
    /// promises are checked for JS).
    #[tokio::test]
    async fn many_fetches_dont_pile_up_host_objects() {
        let base = serve(same_origin_answer);
        let url = format!("{base}app/page.html");
        let gate = std::sync::Arc::new(crate::gate::Gate::new(std::sync::Arc::new(blitz_net::Provider::new(None))));
        let python = "
count = [0]
def once():
    def got(r):
        def text(t):
            def data(d):
                count[0] += 1
                document.querySelector('#out').textContent = str(count[0])
                if count[0] < 60:
                    once()
            r.json().then(data)
        r.headers.get('content-type')
        r.text().then(text)
    fetch('data.json').then(got)
once()
";
        let js = "
let count = 0;
(async () => {
  while (count < 60) {
    const r = await fetch('data.json');
    r.headers.get('content-type');
    await r.text();
    count++;
    document.querySelector('#out').textContent = String(count);
  }
})();";
        for (pack, source) in [("python", python), ("js", js)] {
            let doc = HtmlDocument::from_html("<p id=out></p>", DocumentConfig { base_url: Some(url.clone()), ..Default::default() }).into_inner();
            let env = PageEnv { gate: Some(gate.clone()), ..Default::default() };
            let scripts = [Script { name: format!("many.{pack}"), source: source.into(), module: false }];
            let mut page = match pack {
                "python" => crate::python_pack::start(doc, env, &scripts).unwrap(),
                _ => crate::js_pack::start(doc, env, &scripts, |_| async { anyhow::bail!("no imports") }).await.unwrap(),
            };
            settle_network(&mut page).await;
            assert_eq!(output(&page), "60", "{pack}: {:?}", errors_on(&url));
            let objects = page.pack.objects.borrow();
            assert_eq!(objects.live_of("Promise"), 0, "{pack}: promises piled up");
            if pack == "python" {
                for kind in ["Response", "Headers", "Object"] {
                    assert!(objects.live_of(kind) <= 1, "{pack}: {kind} objects piled up: {}", objects.live_of(kind));
                }
            }
        }
    }

    // ---- Time-travel replay: record a session, replay it in a fresh pack, same DOM.

    /// Record the events scenario on its own URL, plus clicks on `#noise` (random numbers and
    /// the clock, read now and from a timer) and, where the pack can eval, console input.
    async fn record_events_session(pack: &str) -> replay::Recording {
        // Its own URL: tests run in parallel, and a recording is armed by URL.
        static SESSIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SESSIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let url = format!("https://replay-{pack}-{n}.test/");
        let doc = HtmlDocument::from_html(EVENTS_HTML, DocumentConfig { base_url: Some(url.clone()), ..Default::default() }).into_inner();
        let wasm = if pack == "dart" { compile("tests/fixtures/events.dart").await } else { Vec::new() };
        replay::arm(&url);
        let (mut page, program) = match pack {
            "dart" => {
                (crate::dart_pack::start(doc, Default::default(), &wasm).unwrap(), replay::Program::Dart(wasm))
            }
            "python" => {
                let scripts = vec![Script { name: "events.py".into(), source: include_str!("../tests/fixtures/events.py").into(), module: false }];
                (crate::python_pack::start(doc, Default::default(), &scripts).unwrap(), replay::Program::Scripts { pack: "python", scripts, imports: Vec::new() })
            }
            _ => {
                let scripts = vec![Script { name: "events.js".into(), source: include_str!("../tests/fixtures/events.js").into(), module: false }];
                let page = crate::js_pack::start(doc, Default::default(), &scripts, |_| async { anyhow::bail!("no imports") }).await.unwrap();
                (page, replay::Program::Scripts { pack: "js", scripts, imports: Vec::new() })
            }
        };
        page.attach_recording(EVENTS_HTML, None, program);
        dispatch(&mut page, "#button", click_at(10.0, 20.0, true));
        dispatch(&mut page, "#noise", click_at(0.0, 0.0, false));
        dispatch(&mut page, "#stop", click_at(0.0, 0.0, false));
        dispatch(&mut page, "#field", key_down("a", Code::KeyA, true));
        settle_timers(&mut page);
        dispatch(&mut page, "#noise", click_at(0.0, 0.0, false));
        dispatch(&mut page, "#stream", click_at(0.0, 0.0, false));
        if page.pack.can_eval() {
            let add = if pack == "python" { "out.textContent = out.textContent + ',typed'" } else { "out.textContent += ',typed'" };
            page.pack.eval(add, debug::CONSOLE_PACK).unwrap();
        }
        settle_timers(&mut page);
        page.pack.stop_recording().expect("the page was being recorded")
    }

    /// In each pack: a recorded session (events, timers, random numbers, the clock, console
    /// input) replayed in a fresh pack ends with the same DOM, and replaying part of it stops
    /// at that step.
    #[tokio::test]
    async fn a_recorded_session_replays_to_the_same_dom_in_every_pack() {
        for pack in ["dart", "js", "python"] {
            let recording = record_events_session(pack).await;
            assert_eq!(recording.pack, pack);
            assert!(recording.tape.iter().any(|(k, _)| *k == replay::Read::Random), "{pack}: Math.random is on the tape");
            assert!(recording.tape.iter().any(|(k, _)| matches!(k, replay::Read::Now | replay::Read::Clock)), "{pack}: the clock is on the tape");
            assert!(recording.steps.iter().any(|s| matches!(s, replay::Step::Timers(_))), "{pack}: timers are steps");

            let config = || DocumentConfig { base_url: Some(recording.url.clone()), ..Default::default() };
            let (replayed, outcome) = replay::replay(&recording, config(), None).await.unwrap();
            assert_eq!(outcome, replay::Outcome { steps: recording.steps.len(), diverged: None }, "{pack}");
            assert_eq!(replay::dom_text(&replayed.pack().doc.borrow()), recording.final_dom, "{pack}: the replayed DOM differs");
            let noise = text_of(&replayed.pack().doc.borrow(), "#noise");
            assert_eq!(noise.matches(" t").count(), 2, "{pack}: two clicks' numbers and their timers' numbers: {noise} {:?}", errors_on(&recording.url));

            // Step to event 1: only the first click has happened.
            let (partial, outcome) = replay::replay(&recording, config(), Some(1)).await.unwrap();
            assert_eq!(outcome.steps, 1);
            assert_eq!(text_of(&partial.pack().doc.borrow(), "#out"), "once,click:10,20,0,true", "{pack}");
            // Afterwards the page is live: it gets events as usual.
            drop(partial);
        }
    }

    /// Without the tape, the same inputs give different numbers: the replay's match above is
    /// the tape's doing, not luck. A replay that can't apply a step says where it diverged.
    #[tokio::test]
    async fn replay_without_the_tape_differs_and_divergence_is_reported() {
        let mut recording = record_events_session("js").await;
        let mut untaped = recording.clone();
        untaped.tape.clear();
        let config = || DocumentConfig { base_url: Some(recording.url.clone()), ..Default::default() };
        let (page, outcome) = replay::replay(&untaped, config(), None).await.unwrap();
        assert!(outcome.diverged.is_some());
        assert_ne!(replay::dom_text(&page.pack().doc.borrow()), recording.final_dom);

        // A step whose target isn't there.
        recording.steps.insert(0, replay::Step::Event { kind: "click".into(), target: vec![9, 9, 9], details: Default::default() });
        let (_, outcome) = replay::replay(&recording, config(), None).await.unwrap();
        assert_eq!(outcome.steps, 0);
        assert!(outcome.diverged.unwrap().contains("isn't in the page"));
    }

    /// Pause on exceptions while recording: the first click's error pauses the page, so its
    /// second listener doesn't run; after Resume, with pausing off, both run. A replay pauses
    /// and resumes at the same points and ends with the same DOM.
    #[tokio::test]
    async fn a_pause_on_an_exception_replays_as_recorded() {
        let url = "https://replay-pause.test/";
        replay::arm(url);
        let doc = HtmlDocument::from_html("<button id=b>go</button><p id=out></p>", DocumentConfig { base_url: Some(url.into()), ..Default::default() }).into_inner();
        let scripts = vec![Script { name: "pause.js".into(), source: "
            const b = document.querySelector('#b'), out = document.querySelector('#out');
            b.addEventListener('click', () => { throw new Error('first listener fails'); });
            b.addEventListener('click', () => { out.textContent += 'second ran;'; });".into(), module: false }];
        let mut page = crate::js_pack::start(doc, Default::default(), &scripts, |_| async { anyhow::bail!("no imports") }).await.unwrap();
        page.attach_recording("<button id=b>go</button><p id=out></p>", None, replay::Program::Scripts { pack: "js", scripts, imports: Vec::new() });
        debug::set_pause_on_exceptions(true);
        dispatch(&mut page, "#b", click_at(0.0, 0.0, false));
        debug::set_pause_on_exceptions(false);
        assert!(page.pack.is_paused());
        assert_eq!(output(&page), "");
        page.pack.resume();
        dispatch(&mut page, "#b", click_at(0.0, 0.0, false));
        assert_eq!(output(&page), "second ran;");
        let recording = page.pack.stop_recording().unwrap();

        let config = DocumentConfig { base_url: Some(url.into()), ..Default::default() };
        let (replayed, outcome) = replay::replay(&recording, config, None).await.unwrap();
        assert_eq!(outcome.diverged, None);
        assert_eq!(replay::dom_text(&replayed.pack().doc.borrow()), recording.final_dom);
    }

    /// JS errors carry their type, message and a stack with function names and locations.
    #[test]
    fn js_errors_have_types_and_stacks() {
        let page = js_page("<p id=out></p>", &[
            "function inner() { null.foo; }\nfunction outer() { inner(); }\nouter();",
            "let e = new Error('x'); document.querySelector('#out').textContent = String(e.stack);",
        ]);
        // Other tests run scripts named test-1.js in parallel; find this one's error.
        let err = host::trace().into_iter().find(|e| e.pack == "js" && e.args == "test-1.js" && e.result.contains("at inner")).unwrap().result;
        assert!(err.contains("Uncaught TypeError"), "{err}");
        assert!(err.contains("at inner (test-1.js:1:"), "{err}");
        assert!(err.contains("at outer (test-1.js:2:"), "{err}");
        assert!(text_of(&page.pack.doc.borrow(), "#out").contains("test-2.js:1:"));
    }

    fn module(name: &str, source: &str) -> Script {
        Script { name: name.into(), source: source.into(), module: true }
    }

    fn classic(name: &str, source: &str) -> Script {
        Script { name: name.into(), source: source.into(), module: false }
    }

    /// Imports resolve relative to the importing module, every module in the graph is
    /// evaluated once in dependency order, and modules run after classic scripts.
    #[test]
    fn js_modules_load_imports_once_and_run_after_classic_scripts() {
        let add = "document.querySelector('#out').textContent +=";
        let page = js_page_with(
            "<p id=out></p>",
            &[
                // The module comes first in the page but runs after the classic script.
                module("https://site.test/app/main.js", &format!("import {{ count }} from './shared.js'; import './b.js'; import './c.js'; {add} ` main:${{count()}}`;")),
                classic("inline", "document.querySelector('#out').textContent = 'classic';"),
            ],
            &[
                ("https://site.test/app/shared.js", &format!("let n = 0; export const count = () => ++n; {add} ' shared';")),
                ("https://site.test/app/b.js", &format!("import {{ count }} from './shared.js'; count(); {add} ' b';")),
                ("https://site.test/app/c.js", &format!("import {{ count }} from '../app/shared.js'; count(); {add} ' c';")),
            ],
        );
        assert_eq!(text_of(&page.pack.doc.borrow(), "#out"), "classic shared b c main:3");
    }

    /// A missing import and a bare specifier are reported; the rest of the page still runs.
    #[test]
    fn js_module_errors_are_reported() {
        let page = js_page_with(
            "<p id=out>start</p>",
            &[
                module("https://site.test/missing.js", "import './nope.js';"),
                module("https://site.test/bare.js", "import 'lodash';"),
                module("https://site.test/ok.js", "document.querySelector('#out').textContent = 'ok';"),
            ],
            &[],
        );
        assert_eq!(text_of(&page.pack.doc.borrow(), "#out"), "ok");
        let trace = host::trace();
        assert!(trace.iter().any(|e| e.pack == "js" && e.args == "https://site.test/nope.js" && e.result.contains("404")));
        assert!(trace.iter().any(|e| e.pack == "js" && e.args == "https://site.test/bare.js" && e.result.contains("import map")));
    }

    /// Console input runs in the page's own JS context (seeing its globals), reaches the DOM
    /// only through the host, and is traced like any other call.
    #[test]
    fn console_eval_runs_in_the_page_through_the_host() {
        let page = js_page("<p id=out>start</p>", &["var counter = 41; function bump() { return ++counter; }"]);
        let eval = |source: &str| page.pack.eval(source, debug::CONSOLE_PACK);
        assert_eq!(eval("bump()").unwrap(), "42");
        assert_eq!(eval("document.querySelector('#out').textContent = 'set from the console'").unwrap(), "\"set from the console\"");
        assert_eq!(text_of(&page.pack.doc.borrow(), "#out"), "set from the console");

        // Results read like a browser console's.
        assert_eq!(eval("document.querySelector('#out')").unwrap(), "<p id=\"out\">");
        assert_eq!(eval("document").unwrap(), "Document");
        assert_eq!(eval("({a: 1, b: [1, 'x', null], c: {d: undefined}})").unwrap(), "{a: 1, b: [1, \"x\", null], c: {d: undefined}}");
        assert_eq!(eval("const o = {}; o.self = o; o").unwrap(), "{self: [circular]}");
        assert_eq!(eval("[bump, new Map([[1, 2]])]").unwrap(), "[ƒ bump(), Map(1) {1 => 2}]");
        let err = eval("missing.x").unwrap_err().to_string();
        assert!(err.contains("ReferenceError"), "{err}");

        // No side doors from the console either (XMLHttpRequest is the host's, through the gate).
        assert_eq!(eval("typeof require").unwrap(), "\"undefined\"");

        let trace = host::trace();
        assert!(trace.iter().any(|e| e.pack == "console" && e.call == "eval" && e.args == "bump()" && e.result == "42"));
        assert!(trace.iter().any(|e| e.pack == "console" && e.kind == TraceKind::Error && e.args == "missing.x"));
        // The DOM write went through the host, traced as the console's (not the page's).
        assert!(trace.iter().any(|e| e.pack == "console" && e.call == "Element.textContent =" && e.args.ends_with("set from the console")));
    }

    /// With pause on exceptions on, an uncaught error keeps a snapshot of the page and stops
    /// its listeners until resumed. (The setting is global, so it's on only around the click;
    /// other tests' pages that throw meanwhile would just pause, which none of them check.)
    #[test]
    fn pause_on_exceptions_snapshots_and_holds_events_until_resumed() {
        let mut page = js_page("<button id=b>go</button><p id=out></p>", &[r#"
            let clicks = 0;
            document.querySelector('#b').addEventListener('click', function onClick() {
                clicks++;
                document.querySelector('#out').textContent = 'clicks ' + clicks;
                if (clicks === 1) throw new Error('the first click fails');
            });
        "#]);
        debug::set_pause_on_exceptions(true);
        dispatch(&mut page, "#b", click_at(0.0, 0.0, false));
        debug::set_pause_on_exceptions(false);
        assert!(page.pack.is_paused());

        let origin = crate::settings::origin_of(&blitz_traits::net::Url::parse(&page.pack.page).unwrap());
        let snap = debug::exceptions(&origin).into_iter().find(|s| s.error.contains("the first click fails")).expect("a snapshot");
        assert_eq!(snap.during, "click listener on button#b");
        assert!(snap.error.contains("at onClick (test-1.js:6:"), "source location: {}", snap.error);
        assert!(snap.dom.contains("\"clicks 1\""), "the DOM as it was: {}", snap.dom);
        assert!(snap.recent_calls.iter().any(|c| c.starts_with("Element.textContent =")), "{:?}", snap.recent_calls);
        assert!(snap.state.contains("1 listeners"), "{}", snap.state);

        // Paused: the listener doesn't run, but the console still works.
        dispatch(&mut page, "#b", click_at(0.0, 0.0, false));
        assert_eq!(output(&page), "clicks 1");
        assert_eq!(page.pack.eval("clicks", debug::CONSOLE_PACK).unwrap(), "1");

        page.pack.resume();
        dispatch(&mut page, "#b", click_at(0.0, 0.0, false));
        assert_eq!(output(&page), "clicks 2");
    }

    /// AI control's click runs the page's listeners like element.click(); its eval is traced
    /// as the AI's.
    #[test]
    fn click_and_eval_for_ai_control() {
        let page = js_page("<button id=b>go</button><p id=out></p>", &[
            "document.querySelector('#b').addEventListener('click', () => { document.querySelector('#out').textContent = 'clicked by ai'; });",
        ]);
        assert_eq!(page.pack.click("#b").unwrap(), "button#b");
        assert_eq!(output(&page), "clicked by ai");
        assert!(page.pack.click("#missing").unwrap_err().to_string().contains("nothing matches"));

        assert_eq!(page.pack.eval("document.querySelector('#out').textContent", "ai").unwrap(), "\"clicked by ai\"");
        assert!(host::trace().iter().any(|e| e.pack == "ai" && e.call == "eval" && e.result == "\"clicked by ai\""));
    }

    /// What the user types into the console never reaches the AI's views, including the host
    /// calls and logs it makes (they're traced as the console's, not the page's).
    #[test]
    fn console_input_and_its_effects_stay_out_of_ai_views() {
        let page = js_page("<p id=out></p>", &[]);
        page.pack.eval("console.log('typed-log-qzx'); document.querySelector('#out').textContent = 'typed-text-qzx'", debug::CONSOLE_PACK).unwrap();
        let origin = crate::settings::origin_of(&blitz_traits::net::Url::parse(&page.pack.page).unwrap());
        // The user's own console still shows the log.
        assert!(debug::console(&origin, debug::Privacy::Full, usize::MAX).iter().any(|e| e.text == "typed-log-qzx"));
        // Match on text, not `{:?}`: a number marker could match an entry's timing.
        let console = debug::console(&origin, debug::Privacy::Redacted, usize::MAX);
        assert!(!console.iter().any(|e| e.text.contains("qzx") || e.input.as_deref().is_some_and(|i| i.contains("qzx"))), "console leaked");
        let trace = debug::trace(&origin, debug::Privacy::Redacted, usize::MAX);
        assert!(!trace.iter().any(|e| [&e.call, &e.args, &e.result].iter().any(|t| t.contains("qzx"))), "trace leaked");
    }

    /// A paused page doesn't arm the timer thread (a due timer would wake the loop forever);
    /// resuming arms it so timers that came due meanwhile run.
    #[test]
    fn paused_pages_dont_arm_timers_until_resumed() {
        let page = js_page("<p id=out>waiting</p>", &["setTimeout(() => { document.querySelector('#out').textContent = 'ran'; }, 0);"]);
        page.pack.paused.set(true);
        std::thread::sleep(std::time::Duration::from_millis(5));
        page.pack.arm_timers();
        assert!(page.pack.timer_thread.borrow().is_none());
        assert!(!page.pack.run_due_timers());
        assert_eq!(output(&page), "waiting");

        page.pack.resume();
        assert!(page.pack.timer_thread.borrow().is_some());
        assert!(page.pack.run_due_timers());
        assert_eq!(output(&page), "ran");
    }

    /// A JS listener that never returns is stopped by the time limit and reported; the pack
    /// is then stopped (its engine state is unusable) and gets no more events.
    #[test]
    fn runaway_js_is_stopped() {
        let mut page = js_page("<button id=b>go</button><p id=out></p>", &[
            "document.querySelector('#b').addEventListener('click', function spin() { while (true) {} });",
        ]);
        dispatch(&mut page, "#b", click_at(0.0, 0.0, false));
        assert!(host::trace().iter().any(|e| e.pack == "js" && e.result.contains("script ran too long and was stopped (JS)")));
        assert!(page.pack.idle());
        let err = page.pack.eval("1", debug::CONSOLE_PACK).unwrap_err().to_string();
        assert!(err.contains("was stopped (it ran too long or crashed)"), "{err}");
    }

    /// The same for Dart.
    #[tokio::test]
    async fn runaway_dart_is_stopped() {
        let wasm = compile("tests/fixtures/spins.dart").await;
        let doc = HtmlDocument::from_html("<button id=go>go</button>", DocumentConfig::default()).into_inner();
        let page = crate::dart_pack::start(doc, Default::default(), &wasm).unwrap();
        let (button, id) = listener(&page, "#go", "click");
        let event = HostValue::Object(page.pack.objects.borrow_mut().event("click", button, Default::default()));
        let err = page.pack.fire(id, &[event.clone()]).unwrap_err().to_string();
        assert!(err.contains("script ran too long and was stopped (Dart)"), "{err}");
        let err = page.pack.fire(id, &[event]).unwrap_err().to_string();
        assert!(err.contains("was stopped (it ran too long or crashed)"), "{err}");
    }

    /// The Handles view sees what the pack holds and how often each callback ran; each call
    /// into the pack is traced with its own time, apart from its host calls.
    #[test]
    fn handles_and_pack_calls_for_the_inspector() {
        let mut page = js_page("<button id=b>go</button><p id=out></p>", &[
            "const out = document.querySelector('#out');
             document.querySelector('#b').addEventListener('click', () => { out.textContent = 'clicked'; });
             setTimeout(() => {}, 60000);",
        ]);
        dispatch(&mut page, "#b", click_at(0.0, 0.0, false));
        dispatch(&mut page, "#b", click_at(0.0, 0.0, false));
        let h = page.pack.handles();
        assert_eq!(h.listeners.len(), 1);
        let (node, kind, ids) = &h.listeners[0];
        assert_eq!((node.as_str(), kind.as_str(), ids[0].1), ("button#b", "click", 2));
        assert_eq!((h.timers.len(), h.timers[0].0.kind), (1, "timeout"));
        assert!(h.timers[0].0.due_in_ms > 50_000.0);
        assert_eq!((h.callbacks, h.calls), (2, 2));
        assert!(h.objects.iter().any(|(_, what)| what == "Element p#out"), "{:?}", h.objects);
        assert!(h.objects.iter().all(|(_, what)| !what.starts_with("Event")), "event objects are released");
        // Each click is a Pack entry on this page, as is the start.
        let mine: Vec<_> = host::trace().into_iter().filter(|e| e.page == page.pack.page && e.kind == TraceKind::Pack).collect();
        assert!(mine.iter().any(|e| e.call == "start"));
        assert!(mine.iter().filter(|e| e.call == "click listener" && e.args == "button#b").count() >= 2);
    }

    // ---- The Python pack: the same contract, a third language.

    fn python_page(html: &str, scripts: &[&str]) -> PackDocument {
        let doc = HtmlDocument::from_html(html, DocumentConfig::default()).into_inner();
        let scripts: Vec<Script> = scripts
            .iter()
            .enumerate()
            .map(|(i, s)| Script { name: format!("test-{}.py", i + 1), source: s.to_string(), module: false })
            .collect();
        crate::python_pack::start(doc, Default::default(), &scripts).expect("Python pack missing? run packs/python/build.sh")
    }

    /// The Python counter example, end to end: its inline script in the Python pack
    /// (RustPython in wasm), the same host as Dart and JS, the real Blitz DOM.
    #[test]
    fn python_counter_example_runs_and_handles_clicks() {
        let html = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/python-counter/index.html")).unwrap();
        let script = html.split("<script type=\"text/python\">").nth(1).unwrap().split("</script>").next().unwrap();
        let page = python_page(&html, &[&crate::python_pack::dedent(script)]);
        assert_eq!(text_of(&page.pack.doc.borrow(), "#status"), "Python is running, as a pack in WebAssembly.");

        let (button, id) = listener(&page, "#increment", "click");
        for _ in 0..2 {
            let event = HostValue::Object(page.pack.objects.borrow_mut().event("click", button, Default::default()));
            page.pack.fire(id, &[event]).unwrap();
        }
        let doc = page.pack.doc.borrow();
        assert_eq!(text_of(&doc, "#count"), "2");
        assert_eq!(text_of(&doc, "#log"), "Click 1, handled in Python (click)Click 2, handled in Python (click)");
    }

    /// Uncaught Python errors read as Python tracebacks, with file, line and function; the
    /// bridge's own frames are left out. A failing script doesn't stop the next, and host
    /// errors reach Python as exceptions it can catch.
    #[test]
    fn python_errors_have_tracebacks() {
        let mut page = python_page("<button id=b>go</button><p id=out></p>", &[
            "def inner():\n    raise ValueError('python broke')\n\ndef outer():\n    inner()\n\nouter()",
            "try:\n    document.fooBar()\nexcept AttributeError as e:\n    document.querySelector('#out').textContent = str(e)",
            "def on_click(e):\n    {}['missing']\ndocument.querySelector('#b').addEventListener('click', on_click)",
            "def broken(:\n    pass",
            "def deeper(n):\n    return deeper(n + 1)\ndeeper(0)",
        ]);
        let trace = host::trace();
        let err = |script: &str| trace.iter().find(|e| e.pack == "python" && e.args == script).map(|e| e.result.clone()).unwrap_or_default();
        let first = err("test-1.py");
        assert!(first.starts_with("ERROR Uncaught ValueError: python broke"), "{first}");
        assert!(first.contains("File \"test-1.py\", line 7, in <module>"), "{first}");
        assert!(first.contains("File \"test-1.py\", line 5, in outer"), "{first}");
        assert!(first.contains("File \"test-1.py\", line 2, in inner"), "{first}");
        assert!(!first.contains("prelude"), "{first}");
        assert!(err("test-4.py").contains("SyntaxError"), "{}", err("test-4.py"));
        // Runaway recursion is Python's RecursionError (not a crash), its frames summarised.
        let deep = err("test-5.py");
        assert!(deep.starts_with("ERROR Uncaught RecursionError") && deep.contains("[Previous line repeated") && deep.len() < 1000, "{deep}");
        assert_eq!(output(&page), "Document.fooBar is not supported yet");

        // A listener's error, with pause on exceptions: a snapshot with the traceback.
        debug::set_pause_on_exceptions(true);
        dispatch(&mut page, "#b", click_at(0.0, 0.0, false));
        debug::set_pause_on_exceptions(false);
        assert!(page.pack.is_paused());
        let origin = crate::settings::origin_of(&blitz_traits::net::Url::parse(&page.pack.page).unwrap());
        let snap = debug::exceptions(&origin).into_iter().find(|s| s.error.contains("KeyError")).expect("a snapshot");
        assert_eq!(snap.during, "click listener on button#b");
        assert!(snap.error.contains("File \"test-3.py\", line 2, in on_click"), "{}", snap.error);
    }

    /// Console input runs in the page's own namespace, shows results with `repr`, and is
    /// traced as the console's.
    #[test]
    fn python_console_eval_runs_in_the_page() {
        let page = python_page("<p id=out>start</p>", &["counter = 41\ndef bump():\n    global counter\n    counter += 1\n    return counter"]);
        let eval = |source: &str| page.pack.eval(source, debug::CONSOLE_PACK);
        assert!(page.pack.can_eval());
        assert_eq!(eval("bump()").unwrap(), "42");
        assert_eq!(eval("x = [1, 'a', None]\nx").unwrap(), "[1, 'a', None]");
        assert_eq!(eval("document.querySelector('#out').textContent = 'set from the console'").unwrap(), "None");
        assert_eq!(output(&page), "set from the console");
        assert_eq!(eval("document.querySelector('#out')").unwrap(), "<p id=\"out\">");
        assert_eq!(eval("document").unwrap(), "Document");
        let err = eval("missing.x").unwrap_err().to_string();
        assert!(err.contains("NameError"), "{err}");
        let trace = host::trace();
        assert!(trace.iter().any(|e| e.pack == "console" && e.call == "eval" && e.args == "bump()" && e.result == "42"));
        assert!(trace.iter().any(|e| e.pack == "console" && e.call == "Element.textContent =" && e.args.ends_with("set from the console")));
    }

    /// A Python listener that never returns is stopped by the time limit, like JS and Dart.
    #[test]
    fn runaway_python_is_stopped() {
        let mut page = python_page("<button id=b>go</button>", &[
            "def spin(e):\n    while True:\n        pass\ndocument.querySelector('#b').addEventListener('click', spin)",
        ]);
        dispatch(&mut page, "#b", click_at(0.0, 0.0, false));
        assert!(host::trace().iter().any(|e| e.pack == "python" && e.result.contains("script ran too long and was stopped (Python)")));
        assert!(page.pack.idle());
        let err = page.pack.eval("1", debug::CONSOLE_PACK).unwrap_err().to_string();
        assert!(err.contains("was stopped (it ran too long or crashed)"), "{err}");
    }

    /// Python sees the web platform through the host and nothing else: no network, files,
    /// environment or processes, and no way to import them.
    #[test]
    fn python_has_no_side_doors() {
        let page = python_page("<p id=out></p>", &[r#"
reachable = []
for name in ['require', 'process']:
    if name in dir(__builtins__) or name in globals():
        reachable.append(name)
for module in ['os', 'socket', 'subprocess', 'urllib', 'http', 'js', 'pyodide', 'ctypes']:
    try:
        __import__(module)
        reachable.append(module)
    except ImportError:
        pass
try:
    open('/etc/passwd').read()
    reachable.append('open')
except Exception:
    pass
r = Math.random()
document.querySelector('#out').textContent = f'{len(reachable)} {0 <= r < 1} {reachable}'
"#]);
        assert_eq!(output(&page), "0 True []");
        assert!(host::trace().iter().any(|e| e.pack == "python" && e.call == "Math.random"));
    }

    // ---- Isolation and lifecycle, for every pack.

    /// Each page gets its own instance: a global one page sets isn't there in the next, in
    /// either language.
    #[test]
    fn each_page_gets_a_fresh_instance() {
        let first = python_page("<p id=out></p>", &["shared = 'leaked'"]);
        let second = python_page("<p id=out></p>", &["document.querySelector('#out').textContent = str('shared' in globals())"]);
        assert_eq!(output(&second), "False");
        drop(first);
        let first = js_page("<p id=out></p>", &["var shared = 'leaked';"]);
        let second = js_page("<p id=out></p>", &["document.querySelector('#out').textContent = typeof shared;"]);
        assert_eq!(output(&second), "undefined");
        drop(first);
    }

    /// Closing a tab or navigating drops the page document; the page's DOM, its pack instance
    /// and the host objects go with it (the tools only ever held them weakly).
    #[tokio::test]
    async fn a_dropped_page_releases_its_pack_and_dom() {
        for pack in ["dart", "js", "python"] {
            let mut page = events_page(pack).await;
            dispatch(&mut page, "#button", click_at(0.0, 0.0, false));
            // Arm the timer thread too, so it has to wind down.
            page.pack.arm_timers();
            let page_ref = page.page_ref();
            let objects = Rc::downgrade(&page.pack.objects);
            assert!(page_ref.doc().is_some() && page_ref.pack().is_some());
            drop(page);
            assert!(page_ref.doc().is_none(), "{pack}: the DOM outlived its page");
            assert!(page_ref.pack().is_none(), "{pack}: the pack outlived its page");
            assert!(objects.upgrade().is_none(), "{pack}: the host objects outlived the page");
        }
    }

    /// A page on its own URL, so its trace entries can be told apart from other tests'.
    fn python_page_at(url: &str, scripts: &[&str]) -> PackDocument {
        let config = DocumentConfig { base_url: Some(url.into()), ..Default::default() };
        let doc = HtmlDocument::from_html("<p id=out></p>", config).into_inner();
        let scripts: Vec<Script> = scripts.iter().map(|s| Script { name: "test.py".into(), source: s.to_string(), module: false }).collect();
        crate::python_pack::start(doc, Default::default(), &scripts).unwrap()
    }

    fn errors_on(page: &str) -> Vec<String> {
        host::trace().into_iter().filter(|e| e.page == page && e.kind == TraceKind::Error).map(|e| e.result).collect()
    }

    /// `time.sleep` would block the UI thread, and a raised recursion limit would overflow the
    /// Wasm stack: both are Python errors the page can see, and the pack keeps running.
    #[test]
    fn python_cant_block_or_overflow_the_page() {
        let page = python_page("<p id=out></p>", &[]);
        let err = page.pack.eval("import time\ntime.sleep(0.05)", debug::CONSOLE_PACK).unwrap_err().to_string();
        assert!(err.contains("time.sleep would block the page; use setTimeout"), "{err}");
        let err = page.pack.eval("import sys\nsys.setrecursionlimit(10**6)\ndef f(n):\n    return f(n + 1)\nf(0)", debug::CONSOLE_PACK).unwrap_err().to_string();
        assert!(err.starts_with("Uncaught RecursionError"), "{err}");
        assert_eq!(page.pack.eval("sys.getrecursionlimit()", debug::CONSOLE_PACK).unwrap(), "3000");
        assert!(!page.pack.idle());
    }

    /// If the interpreter crashes anyway (here, recursion past the cap through the prelude's
    /// own function), the pack is stopped with one clear error, not one per timer tick.
    #[test]
    fn a_crashed_python_pack_stops_with_one_error() {
        let url = "https://python-crash.test/";
        let page = python_page_at(url, &[r#"
import sys
def deep(n):
    return deep(n + 1)
def tick():
    sys.setrecursionlimit.__globals__['set_recursion_limit'](10**6)
    deep(0)
setInterval(tick, 1)
"#]);
        for _ in 0..5 {
            std::thread::sleep(std::time::Duration::from_millis(5));
            page.pack.run_due_timers();
        }
        let errors = errors_on(url);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("the Python pack crashed") && errors[0].contains("reload the page"), "{}", errors[0]);
        assert!(!errors[0].contains("wasm backtrace"), "{}", errors[0]);
        assert!(page.pack.idle());
    }

    /// An exception whose message spans lines keeps all of it in the headline.
    #[test]
    fn python_multi_line_messages_read_whole() {
        let page = python_page("<p id=out></p>", &[]);
        let err = page.pack.eval("def f():\n    raise ValueError('line one\\nline two')\nf()", debug::CONSOLE_PACK).unwrap_err().to_string();
        assert!(err.starts_with("Uncaught ValueError: line one\nline two\nTraceback (most recent call last):"), "{err}");
        assert!(err.contains("line 2, in f"), "{err}");
    }

    // ---- The PHP pack (spike): php-src in wasm, the same contract, a fourth language.

    fn php_page(html: &str, scripts: &[&str]) -> PackDocument {
        let doc = HtmlDocument::from_html(html, DocumentConfig::default()).into_inner();
        let scripts: Vec<Script> = scripts
            .iter()
            .enumerate()
            .map(|(i, s)| Script { name: format!("test-{}.php", i + 1), source: s.to_string(), module: false })
            .collect();
        crate::php_pack::start(doc, Default::default(), &scripts).expect("PHP pack missing? run packs/php/build.sh")
    }

    /// The PHP counter example, end to end: its `main.php` in the PHP pack (php-src in
    /// wasm), the same host as the other packs, the real Blitz DOM.
    #[test]
    fn php_counter_example_runs_and_handles_clicks() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/php-counter");
        let html = std::fs::read_to_string(format!("{dir}/index.html")).unwrap();
        assert!(html.contains(r#"<script type="text/php" src="main.php"></script>"#));
        let script = std::fs::read_to_string(format!("{dir}/main.php")).unwrap();
        let page = php_page(&html, &[&script]);
        let status = text_of(&page.pack.doc.borrow(), "#status");
        assert!(status.starts_with("PHP 8.4.") && status.ends_with("is running, as a pack in WebAssembly."), "{status}");

        let (button, id) = listener(&page, "#increment", "click");
        for _ in 0..2 {
            let event = HostValue::Object(page.pack.objects.borrow_mut().event("click", button, Default::default()));
            page.pack.fire(id, &[event]).unwrap();
        }
        let doc = page.pack.doc.borrow();
        assert_eq!(text_of(&doc, "#count"), "2");
        assert_eq!(text_of(&doc, "#log"), "Click 1, handled in PHP (click)Click 2, handled in PHP (click)");
    }

    /// PHP reaches the DOM through the host: properties, methods, a closure as a listener (the
    /// page's globals are superglobals, so it needs no `use`), echo to the console, and one
    /// global scope across scripts.
    #[test]
    fn php_runs_and_handles_clicks() {
        let page = php_page("<button id=increment>+</button><p id=count>0</p><ul id=log></ul><p id=status></p>", &[
            "<?php\n$clicks = 0;\n$document->querySelector('#status')->textContent = 'PHP ' . PHP_VERSION . ' is running';",
            r#"$document->querySelector('#increment')->addEventListener('click', function ($event) {
                global $clicks;
                $clicks++;
                $document->querySelector('#count')->textContent = (string) $clicks;
                $item = $document->createElement('li');
                $item->textContent = "Click $clicks ({$event->type})";
                $document->querySelector('#log')->appendChild($item);
                echo "clicked $clicks\n";
            });
            echo json_encode(['pcre' => preg_match('/^\d+$/', '42'), 'hash' => substr(hash('sha256', 'x'), 0, 8)]);"#,
        ]);
        let status = text_of(&page.pack.doc.borrow(), "#status");
        assert!(status.starts_with("PHP 8.4."), "{status:?}; trace: {:#?}", host::trace().into_iter().filter(|e| e.pack == "php").collect::<Vec<_>>());
        let (button, id) = listener(&page, "#increment", "click");
        for _ in 0..2 {
            let event = HostValue::Object(page.pack.objects.borrow_mut().event("click", button, Default::default()));
            page.pack.fire(id, &[event]).unwrap();
        }
        let doc = page.pack.doc.borrow();
        assert_eq!(text_of(&doc, "#count"), "2");
        assert_eq!(text_of(&doc, "#log"), "Click 1 (click)Click 2 (click)");
        let console: Vec<String> = host::trace().into_iter().filter(|e| e.pack == "php" && e.kind == TraceKind::Console).map(|e| e.args).collect();
        assert!(console.iter().any(|t| t == r#"{"pcre":1,"hash":"2d711642"}"#), "{console:?}");
        assert!(console.iter().any(|t| t == "clicked 2\n"), "{console:?}");
    }

    /// PHP's errors come back as messages with file and line; host errors are PHP exceptions
    /// the page can catch; a failing script doesn't stop the next.
    #[test]
    fn php_errors_are_reported() {
        let page = php_page("<p id=out></p>", &[
            "function inner() { throw new RuntimeException('php broke'); }\ninner();",
            "try { $document->fooBar(); } catch (Error $e) { $document->querySelector('#out')->textContent = $e->getMessage(); }",
            "function broken( {",
            "$f = new Fiber(fn() => 1); $f->start();",
            "echo $undefined_variable;",
        ]);
        let trace = host::trace();
        let err = |script: &str| trace.iter().find(|e| e.pack == "php" && e.args == script).map(|e| e.result.clone()).unwrap_or_default();
        let first = err("test-1.php");
        assert!(first.contains("Uncaught RuntimeException: php broke in test-1.php:1"), "{first}");
        assert_eq!(output(&page), "Document.fooBar() is not supported yet");
        assert!(err("test-3.php").contains("syntax error") && err("test-3.php").contains("test-3.php:1"), "{}", err("test-3.php"));
        assert!(err("test-4.php").contains("Fibers are not supported in this browser"), "{}", err("test-4.php"));
        // A warning isn't an error: it goes to the console and the script finishes.
        assert_eq!(err("test-5.php"), "");
        assert!(trace.iter().any(|e| e.pack == "php" && e.kind == TraceKind::Console && e.args.contains("Warning: Undefined variable $undefined_variable in test-5.php:1")));
    }

    /// Console input: an expression's value is shown; statements run in the page's scope.
    #[test]
    fn php_console_eval_runs_in_the_page() {
        let page = php_page("<p id=out>start</p>", &["$counter = 41;\nfunction bump() { global $counter; return ++$counter; }"]);
        let eval = |source: &str| page.pack.eval(source, debug::CONSOLE_PACK);
        // Randomness comes from the host (WASI random_get), as for every pack.
        assert!(matches!(eval("random_int(1, 6)").unwrap().as_str(), "1" | "2" | "3" | "4" | "5" | "6"));
        assert_eq!(eval("strlen(random_bytes(16))").unwrap(), "16");
        assert_eq!(eval("bump()").unwrap(), "42");
        assert_eq!(eval("[1, 'a', null];").unwrap(), "array (\n  0 => 1,\n  1 => 'a',\n  2 => NULL,\n)");
        assert_eq!(eval("$x = 5; $document->querySelector('#out')->textContent = 'from the console';").unwrap(), "");
        assert_eq!(output(&page), "from the console");
        assert_eq!(eval("$x * 2").unwrap(), "10");
        assert_eq!(eval("$document->querySelector('#out')").unwrap(), "<p id=\"out\">");
        let err = eval("missing()").unwrap_err().to_string();
        assert!(err.contains("Call to undefined function missing()"), "{err}");
    }

    /// A PHP listener that never returns is stopped by the time limit, like the other packs.
    #[test]
    fn runaway_php_is_stopped() {
        let mut page = php_page("<button id=b>go</button>", &[
            "$document->querySelector('#b')->addEventListener('click', function () { while (true) {} });",
        ]);
        dispatch(&mut page, "#b", click_at(0.0, 0.0, false));
        assert!(host::trace().iter().any(|e| e.pack == "php" && e.result.contains("script ran too long and was stopped (PHP)")));
        assert!(page.pack.idle());
    }

    /// PHP sees the web platform through the host and nothing else: no files, processes,
    /// sockets, DNS, environment or extensions, and nothing that blocks.
    #[test]
    fn php_has_no_side_doors() {
        let page = php_page("<p id=out></p>", &[r#"
            $reachable = [];
            if (@file_get_contents('/etc/passwd') !== false) $reachable[] = 'files';
            if (@file_put_contents('/tmp/php-pack-test', 'x') !== false) $reachable[] = 'write';
            if (@fsockopen('example.com', 80) !== false) $reachable[] = 'sockets';
            if (gethostbyname('example.com') !== 'example.com') $reachable[] = 'dns';
            if (@file_get_contents('http://example.com/') !== false) $reachable[] = 'http';
            if (getenv() !== []) $reachable[] = 'env';
            foreach (['exec', 'shell_exec', 'system', 'proc_open', 'popen', 'sleep', 'usleep'] as $f) {
                try { $f('true', 'r'); $reachable[] = $f; } catch (Error) {}
            }
            try { if (@dl('x.so')) $reachable[] = 'dl'; } catch (Error) {}
            $document->querySelector('#out')->textContent = $reachable ? implode(',', $reachable) : 'none';
        "#]);
        assert_eq!(output(&page), "none");
    }

    /// A page that allocates without end fails inside its pack (the memory cap), and the
    /// browser carries on.
    #[test]
    fn a_page_cant_take_all_the_memory() {
        let page = python_page("<p id=out></p>", &[]);
        let err = page.pack.eval("s = 'x' * (600 * 2**20)", debug::CONSOLE_PACK).unwrap_err().to_string();
        assert!(err.contains("MemoryError") || err.contains("Python pack crashed"), "{err}");
        let page = js_page("<p id=out></p>", &[]);
        let err = page.pack.eval("let s = 'x'.repeat(600 * 2 ** 20); s.length", debug::CONSOLE_PACK).unwrap_err().to_string();
        assert!(err.contains("RangeError") || err.contains("JS pack crashed"), "{err}");
    }
}
