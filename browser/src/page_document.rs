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
use crate::host::{self, EventDetails, HostValue, Objects, PageHost, TraceEntry, TraceKind};

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
    /// Wakes the event loop when the next timer is due (set from `poll`).
    waker: Arc<Mutex<Option<Waker>>>,
    timer_thread: RefCell<Option<Sender<Instant>>>,
}

impl RunningPack {
    /// Evaluate console input `source` in the page's pack, on behalf of `by` (the console, or
    /// AI access). Like any call, it goes in the trace; DOM access inside it goes through the
    /// host as usual.
    pub fn eval(&self, source: &str, by: &'static str) -> Result<String> {
        let started = Instant::now();
        let result = match self.runtime.try_borrow_mut() {
            Ok(mut runtime) => {
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
        for timer in &due {
            if let Err(e) = self.fire(timer.callback, &timer.args) {
                self.uncaught("timer", String::new(), &e);
            }
        }
        self.objects.borrow_mut().after_timers_ran(&due);
        release_unused(&self.objects, self.runtime.borrow_mut().as_mut());
        !due.is_empty()
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

    /// The running page, with the pack that started on it.
    pub fn finish(self, runtime: Box<dyn PackRuntime>) -> PackDocument {
        let pack = RunningPack {
            name: self.name,
            page: self.page,
            doc: self.doc,
            objects: self.objects,
            can_eval: runtime.can_eval(),
            runtime: RefCell::new(runtime),
            paused: Cell::new(self.paused),
            waker: Default::default(),
            timer_thread: Default::default(),
        };
        PackDocument { pack: Rc::new(pack) }
    }
}

impl PackDocument {
    /// The page and its pack, for the browser's tools.
    pub fn page_ref(&self) -> PageRef {
        PageRef { doc: Rc::downgrade(&self.pack.doc), pack: Some(Rc::downgrade(&self.pack)) }
    }

    /// Start pack `name` on the page `doc`: the host the pack talks to (load the pack with
    /// it), and the `Startup` to report to while the page's code runs. Each page gets its own
    /// host, objects and pack instance; nothing is shared between pages or packs.
    pub fn starting(doc: BaseDocument, name: &'static str) -> (PageHost, Startup) {
        let doc = Rc::new(RefCell::new(doc));
        let objects = Rc::new(RefCell::new(Objects::default()));
        let host = PageHost::new(name, doc.clone(), objects.clone());
        let page = host.page.clone();
        (host, Startup { name, page, doc, objects, paused: false })
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
/// it should pause.
fn snapshot_if_pausing(pack: &'static str, page: &str, doc: &RefCell<BaseDocument>, objects: &RefCell<Objects>, during: &str, args: String, e: &anyhow::Error) -> bool {
    if !debug::pause_on_exceptions() {
        return false;
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
        let ran = self.pack.run_due_timers();
        self.pack.arm_timers();
        ran
    }
}

struct PackEventHandler<'a> {
    pack: &'a RunningPack,
}

impl EventHandler for PackEventHandler<'_> {
    fn handle_event(&mut self, chain: &[NodeId], event: &mut DomEvent, _doc: &mut dyn Document, state: &mut EventState) {
        let pack = self.pack;
        let kind = event.data.name();
        let Some(&target) = chain.first() else { return };
        let listening = |node: &NodeId| pack.objects.borrow().listeners.contains_key(&(*node, kind.to_string()));
        // Paused on an exception or stopped, the page's listeners don't run (Blitz's defaults still do).
        if pack.idle() || !chain.iter().any(listening) {
            return;
        }
        // One event object for the whole dispatch, released when it ends.
        let handle = pack.objects.borrow_mut().event(kind, target, event_details(&event.data));
        let flag = |f: fn(&host::EventObj) -> bool| pack.objects.borrow_mut().event_mut(handle).is_some_and(|e| f(e));
        // The chain runs from the target up to the root: bubbling order.
        'nodes: for node in chain {
            let ids = pack.objects.borrow().listeners.get(&(*node, kind.to_string())).cloned().unwrap_or_default();
            if let Some(e) = pack.objects.borrow_mut().event_mut(handle) {
                e.current_target = *node;
            }
            for id in ids {
                if let Err(e) = pack.fire(id, &[HostValue::Object(handle)]) {
                    let on = debug::describe(&pack.doc.borrow(), *node);
                    pack.uncaught(&format!("{kind} listener"), on, &e);
                }
                if flag(|e| e.immediate_propagation_stopped) || pack.paused.get() {
                    break 'nodes;
                }
            }
            if flag(|e| e.propagation_stopped) {
                break;
            }
        }
        if flag(|e| e.default_prevented) {
            state.prevent_default();
        }
        pack.objects.borrow_mut().release(handle);
        release_unused(&pack.objects, pack.runtime.borrow_mut().as_mut());
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
    async fn compile(path: &str) -> Vec<u8> {
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
        let page = crate::dart_pack::start(doc, &wasm).unwrap();
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
        let page = crate::dart_pack::start(doc, &wasm).unwrap();

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
        futures_lite_block_on(crate::js_pack::start(doc, scripts, fetch)).expect("JS engine missing? run packs/js/build.sh")
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

    /// JS sees the web platform through the host and nothing else.
    #[test]
    fn js_has_no_side_doors() {
        let page = js_page(
            "<p id=out></p>",
            &[r#"
                const reachable = ['fetch', 'XMLHttpRequest', 'require', 'process', 'WebSocket', 'Deno', 'Bun']
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
            "dart" => crate::dart_pack::start(doc, &compile("tests/fixtures/events.dart").await).unwrap(),
            "python" => {
                let script = Script { name: "events.py".into(), source: include_str!("../tests/fixtures/events.py").into(), module: false };
                crate::python_pack::start(doc, &[script]).expect("Python pack missing? run packs/python/build.sh")
            }
            _ => {
                let script = Script { name: "events.js".into(), source: include_str!("../tests/fixtures/events.js").into(), module: false };
                crate::js_pack::start(doc, &[script], |_| async { anyhow::bail!("no imports") }).await.unwrap()
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

        // No side doors from the console either.
        assert_eq!(eval("typeof fetch").unwrap(), "\"undefined\"");

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
        let page = crate::dart_pack::start(doc, &wasm).unwrap();
        let (button, id) = listener(&page, "#go", "click");
        let event = HostValue::Object(page.pack.objects.borrow_mut().event("click", button, Default::default()));
        let err = page.pack.fire(id, &[event.clone()]).unwrap_err().to_string();
        assert!(err.contains("script ran too long and was stopped (Dart)"), "{err}");
        let err = page.pack.fire(id, &[event]).unwrap_err().to_string();
        assert!(err.contains("was stopped (it ran too long or crashed)"), "{err}");
    }

    // ---- The Python pack: the same contract, a third language.

    fn python_page(html: &str, scripts: &[&str]) -> PackDocument {
        let doc = HtmlDocument::from_html(html, DocumentConfig::default()).into_inner();
        let scripts: Vec<Script> = scripts
            .iter()
            .enumerate()
            .map(|(i, s)| Script { name: format!("test-{}.py", i + 1), source: s.to_string(), module: false })
            .collect();
        crate::python_pack::start(doc, &scripts).expect("Python pack missing? run packs/python/build.sh")
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
for name in ['fetch', 'XMLHttpRequest', 'require']:
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
        crate::python_pack::start(doc, &scripts).unwrap()
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
