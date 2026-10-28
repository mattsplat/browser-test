//! A page document with a running Dart program. Clicks are routed to the Dart listeners
//! registered through the host (`domOnClick`), then Blitz's default handling runs.

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::Result;
use blitz_dom::{BaseDocument, DocGuard, DocGuardMut, Document, EventDriver, EventHandler, NodeId};
use blitz_traits::events::{DomEvent, DomEventData, EventState, UiEvent};

use crate::dart_pack::DartRuntime;
use crate::host::{self, Listeners, PageHost, TraceEntry};

pub struct DartDocument {
    inner: Rc<RefCell<BaseDocument>>,
    runtime: DartRuntime,
    listeners: Listeners,
}

impl DartDocument {
    /// Load the Dart program against `doc` and run its `main`.
    pub fn start(doc: BaseDocument, wasm: &[u8], mjs: &str) -> Result<Self> {
        let inner = Rc::new(RefCell::new(doc));
        let listeners = Listeners::default();
        let host = PageHost::new("dart", inner.clone(), listeners.clone());
        let mut runtime = DartRuntime::load(wasm, mjs, Box::new(host))?;
        runtime.run_main()?;
        Ok(Self { inner, runtime, listeners })
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
        let handler = DartEventHandler { runtime: &mut self.runtime, listeners: &self.listeners };
        let mut driver = EventDriver::new(&mut self.inner, handler);
        driver.handle_ui_event(event);
        // Dart may have changed the DOM.
        self.inner.borrow().shell_provider.request_redraw();
    }
}

struct DartEventHandler<'a> {
    runtime: &'a mut DartRuntime,
    listeners: &'a Listeners,
}

impl EventHandler for DartEventHandler<'_> {
    fn handle_event(&mut self, chain: &[NodeId], event: &mut DomEvent, _doc: &mut dyn Document, _state: &mut EventState) {
        if !matches!(event.data, DomEventData::Click(_)) {
            return;
        }
        // The chain runs from the target up to the root, so listeners fire in bubbling order.
        for node in chain {
            let ids = self.listeners.borrow().get(node).cloned().unwrap_or_default();
            for id in ids {
                if let Err(e) = self.runtime.fire_callback(id) {
                    host::record(TraceEntry {
                        pack: "dart",
                        call: "click handler".into(),
                        args: format!("node {node}"),
                        result: format!("ERROR {e:#}"),
                        micros: 0,
                    });
                }
            }
        }
    }
}
