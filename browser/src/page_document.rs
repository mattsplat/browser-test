//! A page document with a running language pack (Dart or JS). DOM events are delivered to
//! the listeners the pack registered with `addEventListener`, then Blitz's default handling
//! runs. Both packs share the host, so event delivery is identical for them.

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Result;
use blitz_dom::{BaseDocument, DocGuard, DocGuardMut, Document, EventDriver, EventHandler, NodeId};
use blitz_traits::events::{DomEvent, EventState, UiEvent};

use crate::dart_pack::{DartRuntime, HostValue};
use crate::host::{self, Objects, PageHost, TraceEntry};
use crate::js_pack::JsRuntime;

/// What the document needs from a pack: a way to call the listeners it registered.
pub trait PackRuntime {
    fn fire_callback(&mut self, id: usize, args: &[HostValue]) -> Result<()>;
}

impl PackRuntime for DartRuntime {
    fn fire_callback(&mut self, id: usize, args: &[HostValue]) -> Result<()> {
        DartRuntime::fire_callback(self, id, args)
    }
}

pub struct PackDocument {
    inner: Rc<RefCell<BaseDocument>>,
    runtime: Box<dyn PackRuntime>,
    objects: Rc<RefCell<Objects>>,
    pack: &'static str,
}

impl PackDocument {
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
        Ok(Self { inner, runtime: Box::new(runtime), objects, pack: "dart" })
    }

    /// Run a page's scripts in order with the JS pack. Like a browser, an error in one
    /// script is reported (in the trace) and the next still runs.
    pub fn start_js(doc: BaseDocument, scripts: &[String]) -> Result<Self> {
        let (inner, objects, host) = Self::host(doc, "js");
        let mut runtime = JsRuntime::load(Box::new(host))?;
        for (i, source) in scripts.iter().enumerate() {
            if let Err(e) = runtime.run(source) {
                record_error("js", format!("script {}", i + 1), String::new(), &e);
            }
        }
        Ok(Self { inner, runtime: Box::new(runtime), objects, pack: "js" })
    }
}

fn record_error(pack: &'static str, call: String, args: String, e: &anyhow::Error) {
    host::record(TraceEntry { pack, call, args, result: format!("ERROR {e:#}"), micros: 0 });
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
        // The pack may have changed the DOM.
        self.inner.borrow().shell_provider.request_redraw();
    }
}

struct PackEventHandler<'a> {
    runtime: &'a mut dyn PackRuntime,
    objects: &'a Rc<RefCell<Objects>>,
    pack: &'static str,
}

impl EventHandler for PackEventHandler<'_> {
    fn handle_event(&mut self, chain: &[NodeId], event: &mut DomEvent, _doc: &mut dyn Document, _state: &mut EventState) {
        let kind = event.data.name();
        let Some(&target) = chain.first() else { return };
        // The chain runs from the target up to the root, so listeners fire in bubbling order.
        for node in chain {
            let ids = self.objects.borrow().listeners.get(&(*node, kind.to_string())).cloned().unwrap_or_default();
            for id in ids {
                let event_obj = self.objects.borrow_mut().event(kind, target);
                if let Err(e) = self.runtime.fire_callback(id, &[event_obj]) {
                    record_error(self.pack, format!("{kind} listener"), format!("node {node:?}"), &e);
                }
            }
        }
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
            let event = page.objects.borrow_mut().event("click", button);
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
        let event = page.objects.borrow_mut().event("click", button);
        let err = format!("{:#}", page.runtime.fire_callback(id, &[event]).unwrap_err());
        assert!(err.starts_with("Uncaught Dart error: boom from a click"), "{err}");
        assert!(err.contains("at throws.dart:9:3"), "{err}");
        assert!(!err.contains("_internal"), "SDK capture frames are trimmed: {err}");
    }

    fn js_page(html: &str, scripts: &[&str]) -> PackDocument {
        let doc = HtmlDocument::from_html(html, DocumentConfig::default()).into_inner();
        let scripts: Vec<String> = scripts.iter().map(|s| s.to_string()).collect();
        PackDocument::start_js(doc, &scripts).expect("JS engine missing? run packs/js/build.sh")
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
            let event = page.objects.borrow_mut().event("click", button);
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
}
