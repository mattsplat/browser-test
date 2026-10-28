//! A page document with a running Dart program. DOM events are delivered to the listeners
//! Dart registered with `addEventListener`, then Blitz's default handling runs.

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Result;
use blitz_dom::{BaseDocument, DocGuard, DocGuardMut, Document, EventDriver, EventHandler, NodeId};
use blitz_traits::events::{DomEvent, EventState, UiEvent};

use crate::dart_pack::DartRuntime;
use crate::host::{self, Objects, PageHost, TraceEntry};

pub struct DartDocument {
    inner: Rc<RefCell<BaseDocument>>,
    runtime: DartRuntime,
    objects: Rc<RefCell<Objects>>,
}

impl DartDocument {
    /// Load the Dart program against `doc` and run its `main`.
    pub fn start(doc: BaseDocument, wasm: &[u8]) -> Result<Self> {
        let inner = Rc::new(RefCell::new(doc));
        let objects = Rc::new(RefCell::new(Objects::default()));
        let host = PageHost::new("dart", inner.clone(), objects.clone());
        let mut runtime = DartRuntime::load(wasm, Box::new(host))?;
        runtime.run_main()?;
        Ok(Self { inner, runtime, objects })
    }
}

impl Document for DartDocument {
    fn inner(&self) -> DocGuard<'_> {
        DocGuard::RefCell(self.inner.borrow())
    }

    fn inner_mut(&mut self) -> DocGuardMut<'_> {
        DocGuardMut::RefCell(self.inner.borrow_mut())
    }

    fn handle_ui_event(&mut self, event: UiEvent) {
        let handler = DartEventHandler { runtime: &mut self.runtime, objects: &self.objects };
        let mut driver = EventDriver::new(&mut self.inner, handler);
        driver.handle_ui_event(event);
        // Dart may have changed the DOM.
        self.inner.borrow().shell_provider.request_redraw();
    }
}

struct DartEventHandler<'a> {
    runtime: &'a mut DartRuntime,
    objects: &'a Rc<RefCell<Objects>>,
}

impl EventHandler for DartEventHandler<'_> {
    fn handle_event(&mut self, chain: &[NodeId], event: &mut DomEvent, _doc: &mut dyn Document, _state: &mut EventState) {
        let kind = event.data.name();
        let Some(&target) = chain.first() else { return };
        // The chain runs from the target up to the root, so listeners fire in bubbling order.
        for node in chain {
            let ids = self.objects.borrow().listeners.get(&(*node, kind.to_string())).cloned().unwrap_or_default();
            for id in ids {
                let event_obj = self.objects.borrow_mut().event(kind, target);
                if let Err(e) = self.runtime.fire_callback(id, &[event_obj]) {
                    host::record(TraceEntry {
                        pack: "dart",
                        call: format!("{kind} listener"),
                        args: format!("node {node:?}"),
                        result: format!("ERROR {e:#}"),
                        micros: 0,
                    });
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

    /// The real counter example, end to end: its HTML in Blitz, its compiled package:web Dart
    /// in the pack, the real host. Build it first with packs/dart/build.sh.
    #[test]
    fn counter_example_runs_and_handles_clicks() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/counter");
        let html = std::fs::read_to_string(dir.join("index.html")).unwrap();
        let Ok(wasm) = std::fs::read(dir.join("main.wasm")) else {
            panic!("build the example first: packs/dart/build.sh examples/counter/main.dart");
        };
        let doc = HtmlDocument::from_html(&html, DocumentConfig::default()).into_inner();
        let mut page = DartDocument::start(doc, &wasm).unwrap();
        assert_eq!(text_of(&page.inner.borrow(), "#status"), "Dart is running. No JavaScript on this page.");

        // Deliver two clicks the way the event handler does: the button's click listeners,
        // each called with a fresh event object.
        let button = page.inner.borrow().query_selector("#increment").unwrap().unwrap();
        let ids = page.objects.borrow().listeners[&(button, "click".to_string())].clone();
        assert_eq!(ids.len(), 1);
        for _ in 0..2 {
            let event = page.objects.borrow_mut().event("click", button);
            page.runtime.fire_callback(ids[0], &[event]).unwrap();
        }

        let doc = page.inner.borrow();
        assert_eq!(text_of(&doc, "#count"), "2");
        assert_eq!(text_of(&doc, "#log"), "Click 1, handled in Dart (click)Click 2, handled in Dart (click)");
    }
}
