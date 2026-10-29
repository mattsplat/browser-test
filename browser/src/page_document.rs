//! A page document with a running language pack (Dart or JS). It delivers DOM events to the
//! listeners the pack registered, runs the pack's timers, and tells the pack when callbacks
//! are no longer needed. Both packs share the host, so this behaves identically for them.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Waker};
use std::time::Instant;

use anyhow::Result;
use blitz_dom::{BaseDocument, DocGuard, DocGuardMut, Document, EventDriver, EventHandler, NodeId};
use blitz_traits::events::{BlitzKeyEvent, BlitzPointerEvent, DomEvent, DomEventData, EventState, UiEvent};
use keyboard_types::Modifiers;

use crate::dart_pack::{DartRuntime, HostValue};
use crate::host::{self, EventDetails, Objects, PageHost, TraceEntry};
use crate::js_pack::JsRuntime;

/// What the document needs from a pack.
pub trait PackRuntime {
    /// Call a function the page handed over (a listener, a timer) with `args`.
    fn fire_callback(&mut self, id: usize, args: &[HostValue]) -> Result<()>;
    /// The host no longer refers to this callback; the pack may free it.
    fn release_callback(&mut self, id: usize);
}

impl PackRuntime for DartRuntime {
    fn fire_callback(&mut self, id: usize, args: &[HostValue]) -> Result<()> {
        DartRuntime::fire_callback(self, id, args)
    }
    fn release_callback(&mut self, id: usize) {
        DartRuntime::release_callback(self, id)
    }
}

/// A page script for the JS pack: its name (URL, or the page and a position for inline
/// scripts), its source, and whether it's a module.
#[derive(Clone, Debug)]
pub struct JsSource {
    pub name: String,
    pub source: String,
    pub module: bool,
}

pub struct PackDocument {
    inner: Rc<RefCell<BaseDocument>>,
    runtime: Box<dyn PackRuntime>,
    objects: Rc<RefCell<Objects>>,
    pack: &'static str,
    /// Wakes the event loop when the next timer is due (set from `poll`).
    waker: Arc<Mutex<Option<Waker>>>,
    timer_thread: Option<Sender<Instant>>,
}

impl PackDocument {
    fn new(inner: Rc<RefCell<BaseDocument>>, runtime: Box<dyn PackRuntime>, objects: Rc<RefCell<Objects>>, pack: &'static str) -> Self {
        Self { inner, runtime, objects, pack, waker: Default::default(), timer_thread: None }
    }

    fn host(doc: BaseDocument, pack: &'static str) -> (Rc<RefCell<BaseDocument>>, Rc<RefCell<Objects>>, PageHost) {
        let inner = Rc::new(RefCell::new(doc));
        let objects = Rc::new(RefCell::new(Objects::default()));
        let host = PageHost::new(pack, inner.clone(), objects.clone());
        (inner, objects, host)
    }

    /// Load a Dart program against `doc` and run its `main`.
    pub fn start_dart(doc: BaseDocument, wasm: &[u8]) -> Result<Self> {
        let (inner, objects, host) = Self::host(doc, "dart");
        let mut runtime = DartRuntime::load(wasm, Box::new(host))?;
        runtime.run_main()?;
        Ok(Self::new(inner, Box::new(runtime), objects, "dart"))
    }

    /// Run a page's scripts with the JS pack: classic scripts in order, then module scripts
    /// in order (as browsers run deferred modules). A module's imports are fetched with
    /// `fetch` (the gate, for real pages) until its graph is complete. Like a browser, an
    /// error in one script is reported (in the trace) and the next still runs.
    pub async fn start_js<F, Fut>(doc: BaseDocument, scripts: &[JsSource], mut fetch: F) -> Result<Self>
    where
        F: FnMut(String) -> Fut,
        Fut: std::future::Future<Output = Result<String>>,
    {
        let (inner, objects, host) = Self::host(doc, "js");
        let mut runtime = JsRuntime::load(Box::new(host))?;
        for script in scripts.iter().filter(|s| !s.module) {
            if let Err(e) = runtime.run(&script.name, &script.source) {
                record_error("js", "script".into(), script.name.clone(), &e);
            }
        }
        'modules: for module in scripts.iter().filter(|s| s.module) {
            runtime.add_module(&module.name, &module.source)?;
            // Each round fetches every import the engine found missing; a graph is rarely
            // more than a few levels deep, so this bound only stops a runaway.
            for _ in 0..64 {
                let missing = match runtime.discover(&module.name) {
                    Ok(missing) => missing,
                    Err(e) => {
                        record_error("js", "module".into(), module.name.clone(), &e);
                        continue 'modules;
                    }
                };
                if missing.is_empty() {
                    break;
                }
                for url in missing {
                    match fetch(url.clone()).await {
                        Ok(source) => runtime.add_module(&url, &source)?,
                        Err(e) => {
                            record_error("js", "import".into(), url, &e);
                            continue 'modules;
                        }
                    }
                }
            }
            if let Err(e) = runtime.run_module(&module.name) {
                record_error("js", "module".into(), module.name.clone(), &e);
            }
        }
        Ok(Self::new(inner, Box::new(runtime), objects, "js"))
    }

    /// Run every timer that is due. Returns whether any ran.
    fn run_due_timers(&mut self) -> bool {
        let due = self.objects.borrow_mut().take_due_timers(Instant::now());
        for timer in &due {
            if let Err(e) = self.runtime.fire_callback(timer.callback, &timer.args) {
                record_error(self.pack, "timer".into(), String::new(), &e);
            }
        }
        self.objects.borrow_mut().after_timers_ran(&due);
        release_unused(&self.objects, self.runtime.as_mut());
        !due.is_empty()
    }

    /// Make sure the event loop wakes when the next timer is due.
    fn arm_timer_thread(&mut self) {
        let Some(deadline) = self.objects.borrow().next_deadline() else { return };
        let sender = self.timer_thread.get_or_insert_with(|| {
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
            self.timer_thread = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn objects(&self) -> &Rc<RefCell<Objects>> {
        &self.objects
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

fn record_error(pack: &'static str, call: String, args: String, e: &anyhow::Error) {
    host::record(TraceEntry { pack, call, args, result: format!("ERROR {e:#}"), micros: 0 });
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
        DocGuard::RefCell(self.inner.borrow())
    }

    fn inner_mut(&mut self) -> DocGuardMut<'_> {
        DocGuardMut::RefCell(self.inner.borrow_mut())
    }

    fn handle_ui_event(&mut self, event: UiEvent) {
        let handler = PackEventHandler { runtime: self.runtime.as_mut(), objects: &self.objects, pack: self.pack };
        let mut driver = EventDriver::new(&mut self.inner, handler);
        driver.handle_ui_event(event);
        // The pack may have changed the DOM or started timers.
        self.inner.borrow().shell_provider.request_redraw();
        self.arm_timer_thread();
    }

    fn poll(&mut self, task_context: Option<TaskContext>) -> bool {
        if let Some(cx) = &task_context {
            let mut waker = self.waker.lock().unwrap();
            if !waker.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
                *waker = Some(cx.waker().clone());
            }
        }
        let ran = self.run_due_timers();
        self.arm_timer_thread();
        ran
    }
}

struct PackEventHandler<'a> {
    runtime: &'a mut dyn PackRuntime,
    objects: &'a Rc<RefCell<Objects>>,
    pack: &'static str,
}

impl EventHandler for PackEventHandler<'_> {
    fn handle_event(&mut self, chain: &[NodeId], event: &mut DomEvent, _doc: &mut dyn Document, state: &mut EventState) {
        let kind = event.data.name();
        let Some(&target) = chain.first() else { return };
        let listening = |node: &NodeId| self.objects.borrow().listeners.contains_key(&(*node, kind.to_string()));
        if !chain.iter().any(listening) {
            return;
        }
        // One event object for the whole dispatch, released when it ends.
        let handle = self.objects.borrow_mut().event(kind, target, event_details(&event.data));
        // The chain runs from the target up to the root: bubbling order.
        'nodes: for node in chain {
            let ids = self.objects.borrow().listeners.get(&(*node, kind.to_string())).cloned().unwrap_or_default();
            if let Some(e) = self.objects.borrow_mut().event_mut(handle) {
                e.current_target = *node;
            }
            for id in ids {
                if let Err(e) = self.runtime.fire_callback(id, &[HostValue::Object(handle)]) {
                    record_error(self.pack, format!("{kind} listener"), format!("node {node:?}"), &e);
                }
                if self.objects.borrow_mut().event_mut(handle).is_some_and(|e| e.immediate_propagation_stopped) {
                    break 'nodes;
                }
            }
            if self.objects.borrow_mut().event_mut(handle).is_some_and(|e| e.propagation_stopped) {
                break;
            }
        }
        if self.objects.borrow_mut().event_mut(handle).is_some_and(|e| e.default_prevented) {
            state.prevent_default();
        }
        self.objects.borrow_mut().release(handle);
        release_unused(self.objects, self.runtime);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let node = page.inner.borrow().query_selector(selector).unwrap().unwrap();
        let ids = page.objects.borrow().listeners[&(node, kind.to_string())].clone();
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
        let mut page = PackDocument::start_dart(doc, &wasm).unwrap();
        assert_eq!(text_of(&page.inner.borrow(), "#status"), "Dart is running. No JavaScript on this page.");

        // Deliver two clicks the way the event handler does: the button's click listeners,
        // each called with a fresh event object.
        let (button, id) = listener(&page, "#increment", "click");
        for _ in 0..2 {
            let event = HostValue::Object(page.objects.borrow_mut().event("click", button, Default::default()));
            page.runtime.fire_callback(id, &[event]).unwrap();
        }

        let doc = page.inner.borrow();
        assert_eq!(text_of(&doc, "#count"), "2");
        assert_eq!(text_of(&doc, "#log"), "Click 1, handled in Dart (click)Click 2, handled in Dart (click)");
    }

    /// Errors and stack traces read in Dart source lines, via the embedded source map.
    #[tokio::test]
    async fn errors_and_stack_traces_show_dart_lines() {
        let wasm = compile("tests/fixtures/throws.dart").await;
        let doc = HtmlDocument::from_html("<button id=go>go</button>", DocumentConfig::default()).into_inner();
        let mut page = PackDocument::start_dart(doc, &wasm).unwrap();

        // main() printed StackTrace.current, from line 4.
        let printed = host::trace().into_iter().find(|e| e.call == "console" && e.args.starts_with("stack:")).unwrap();
        assert!(printed.args.contains("throws.dart:4:"), "{}", printed.args);

        // A click throws from explode(), line 9: the message and that line are reported.
        let (button, id) = listener(&page, "#go", "click");
        let event = HostValue::Object(page.objects.borrow_mut().event("click", button, Default::default()));
        let err = format!("{:#}", page.runtime.fire_callback(id, &[event]).unwrap_err());
        // Dart's own toString: the error's type shows ("Bad state" is StateError's prefix).
        assert!(err.starts_with("Uncaught Dart error: Bad state: boom from a click"), "{err}");
        assert!(err.contains("at throws.dart:9:3"), "{err}");
        assert!(!err.contains("_internal"), "SDK capture frames are trimmed: {err}");
    }

    fn js_page(html: &str, scripts: &[&str]) -> PackDocument {
        let scripts: Vec<JsSource> = scripts
            .iter()
            .enumerate()
            .map(|(i, s)| JsSource { name: format!("test-{}.js", i + 1), source: s.to_string(), module: false })
            .collect();
        js_page_with(html, &scripts, &[])
    }

    /// A JS page whose module imports resolve from `files` (URL → source).
    fn js_page_with(html: &str, scripts: &[JsSource], files: &[(&str, &str)]) -> PackDocument {
        let doc = HtmlDocument::from_html(html, DocumentConfig::default()).into_inner();
        let files: std::collections::HashMap<String, String> = files.iter().map(|(u, s)| (u.to_string(), s.to_string())).collect();
        let fetch = |url: String| {
            let found = files.get(&url).cloned().ok_or_else(|| anyhow::anyhow!("404 {url}"));
            async move { found }
        };
        futures_lite_block_on(PackDocument::start_js(doc, scripts, fetch)).expect("JS engine missing? run packs/js/build.sh")
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
        let mut page = js_page(&html, &[script]);
        assert_eq!(text_of(&page.inner.borrow(), "#status"), "JavaScript is running, as a pack in WebAssembly.");

        let (button, id) = listener(&page, "#increment", "click");
        for _ in 0..2 {
            let event = HostValue::Object(page.objects.borrow_mut().event("click", button, Default::default()));
            page.runtime.fire_callback(id, &[event]).unwrap();
        }
        let doc = page.inner.borrow();
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
        assert_eq!(text_of(&page.inner.borrow(), "#out"), "0 true");
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
        assert_eq!(text_of(&page.inner.borrow(), "#out"), "Document.fooBar is not supported yet");
        assert!(host::trace().iter().any(|e| e.pack == "js" && e.result.contains("first script broke")));
    }

    // ---- Cross-pack tests: the same page and behaviour in Dart and JS, through the real
    // ---- event dispatch and timer code. Each must produce identical output.

    use blitz_traits::events::{BlitzKeyEvent, BlitzPointerEvent, BlitzPointerId, KeyState, MouseEventButton, MouseEventButtons, PointerCoords, PointerDetails};
    use keyboard_types::{Code, Key, Location, Modifiers};

    const EVENTS_HTML: &str = include_str!("../tests/fixtures/events.html");

    async fn events_page(pack: &str) -> PackDocument {
        let doc = HtmlDocument::from_html(EVENTS_HTML, DocumentConfig::default()).into_inner();
        match pack {
            "dart" => PackDocument::start_dart(doc, &compile("tests/fixtures/events.dart").await).unwrap(),
            _ => {
                let script = JsSource { name: "events.js".into(), source: include_str!("../tests/fixtures/events.js").into(), module: false };
                PackDocument::start_js(doc, &[script], |_| async { anyhow::bail!("no imports") }).await.unwrap()
            }
        }
    }

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
        let node = page.inner.borrow().query_selector(selector).unwrap().unwrap();
        let chain = page.inner.borrow().node_chain(node);
        let mut event = DomEvent::new(node, data);
        let mut state = EventState::default();
        let mut doc = page.inner.clone();
        let mut handler = PackEventHandler { runtime: page.runtime.as_mut(), objects: &page.objects, pack: page.pack };
        handler.handle_event(&chain, &mut event, &mut doc, &mut state);
        state.is_cancelled()
    }

    fn output(page: &PackDocument) -> String {
        text_of(&page.inner.borrow(), "#out")
    }

    /// Run timers until none are left (or a second passes), as the event loop would.
    fn settle_timers(page: &mut PackDocument) {
        let until = Instant::now() + std::time::Duration::from_secs(1);
        while page.objects.borrow().next_deadline().is_some() && Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(2));
            page.run_due_timers();
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
        let before = page.objects.borrow().live_objects();
        for _ in 0..50 {
            dispatch(&mut page, "#button", click_at(0.0, 0.0, false));
        }
        assert_eq!(page.objects.borrow().live_objects(), before, "{pack}: event objects leaked");
        steps
    }

    #[tokio::test]
    async fn events_and_timers_behave_the_same_in_dart_and_js() {
        let dart = run_events_scenario("dart").await;
        let js = run_events_scenario("js").await;
        let expected_events = [
            "once,click:10,20,0,true",
            "once,click:10,20,0,true,click:1,2,0,false",
            "once,click:10,20,0,true,click:1,2,0,false,stop",
            "once,click:10,20,0,true,click:1,2,0,false,stop,key:a,KeyA,true",
            "once,click:10,20,0,true,click:1,2,0,false,stop,key:a,KeyA,true,stream",
        ];
        for (pack, steps) in [("Dart", &dart), ("JS", &js)] {
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
        let err = host::trace().into_iter().find(|e| e.pack == "js" && e.args == "test-1.js").unwrap().result;
        assert!(err.contains("Uncaught TypeError"), "{err}");
        assert!(err.contains("at inner (test-1.js:1:"), "{err}");
        assert!(err.contains("at outer (test-1.js:2:"), "{err}");
        assert!(text_of(&page.inner.borrow(), "#out").contains("test-2.js:1:"));
    }

    fn module(name: &str, source: &str) -> JsSource {
        JsSource { name: name.into(), source: source.into(), module: true }
    }

    fn classic(name: &str, source: &str) -> JsSource {
        JsSource { name: name.into(), source: source.into(), module: false }
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
        assert_eq!(text_of(&page.inner.borrow(), "#out"), "classic shared b c main:3");
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
        assert_eq!(text_of(&page.inner.borrow(), "#out"), "ok");
        let trace = host::trace();
        assert!(trace.iter().any(|e| e.pack == "js" && e.args == "https://site.test/nope.js" && e.result.contains("404")));
        assert!(trace.iter().any(|e| e.pack == "js" && e.args == "https://site.test/bare.js" && e.result.contains("import map")));
    }
}
