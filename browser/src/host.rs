//! The host dispatcher: the only way a language pack reaches the page. Packs see web
//! platform objects (`document`, elements, events, `Math`) as small integer handles; the real
//! nodes stay here. Every call is recorded in the trace (the boundary tracer), with its
//! arguments, result and timing.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use blitz_dom::{BaseDocument, LocalName, NodeId, QualName, ns};

use crate::dart_pack::{Host, HostValue};

/// A web platform object a pack holds a handle to.
#[derive(Clone, Debug)]
pub enum Obj {
    Document,
    Math,
    Element(NodeId),
    Event { kind: String, target: NodeId },
}

impl Obj {
    fn interface(&self) -> &'static str {
        match self {
            Obj::Document => "Document",
            Obj::Math => "Math",
            Obj::Element(_) => "Element",
            Obj::Event { .. } => "Event",
        }
    }
}

/// Objects handed to the pack, and the event listeners it registered. Shared between the
/// host (which the pack calls) and the document (which delivers events).
#[derive(Default)]
pub struct Objects {
    /// objects[h - 1] is the object behind handle h. Handle 0 is never issued.
    objects: Vec<Obj>,
    /// One handle per node, so the same element always compares equal.
    element_handles: HashMap<NodeId, u32>,
    singletons: HashMap<&'static str, u32>,
    /// Callback ids registered with addEventListener, by (node, event type).
    pub listeners: HashMap<(NodeId, String), Vec<usize>>,
}

impl Objects {
    fn add(&mut self, obj: Obj) -> HostValue {
        self.objects.push(obj);
        HostValue::Object(self.objects.len() as u32)
    }

    fn element(&mut self, node: NodeId) -> HostValue {
        if let Some(h) = self.element_handles.get(&node) {
            return HostValue::Object(*h);
        }
        let handle = self.add(Obj::Element(node));
        if let HostValue::Object(h) = handle {
            self.element_handles.insert(node, h);
        }
        handle
    }

    fn singleton(&mut self, name: &'static str, obj: Obj) -> HostValue {
        if let Some(h) = self.singletons.get(name) {
            return HostValue::Object(*h);
        }
        let handle = self.add(obj);
        if let HostValue::Object(h) = handle {
            self.singletons.insert(name, h);
        }
        handle
    }

    /// A new event object for a listener.
    pub fn event(&mut self, kind: &str, target: NodeId) -> HostValue {
        self.add(Obj::Event { kind: kind.to_string(), target })
    }

    fn get(&self, value: &HostValue) -> Result<Obj> {
        match value {
            HostValue::Object(h) => self.objects.get((*h as usize).wrapping_sub(1)).cloned().context("unknown object handle"),
            other => bail!("expected a web object, got {}", other.display()),
        }
    }
}

/// The page as a pack sees it.
pub struct PageHost {
    pub pack: &'static str,
    pub doc: Rc<RefCell<BaseDocument>>,
    pub objects: Rc<RefCell<Objects>>,
    rng: u64,
}

impl PageHost {
    pub fn new(pack: &'static str, doc: Rc<RefCell<BaseDocument>>, objects: Rc<RefCell<Objects>>) -> Self {
        // Randomness comes from the host so a recording can replay it (principle 4).
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
        Self { pack, doc, objects, rng: seed | 1 }
    }

    /// Run `f` and record it in the trace as `label(args)`.
    fn traced<T>(&mut self, label: String, args: String, f: impl FnOnce(&mut Self) -> Result<T>, show: impl Fn(&T) -> String) -> Result<T> {
        let started = Instant::now();
        let result = f(self);
        record(TraceEntry {
            pack: self.pack,
            call: label,
            args,
            result: match &result {
                Ok(v) => show(v),
                Err(e) => format!("ERROR {e:#}"),
            },
            micros: started.elapsed().as_micros() as u64,
        });
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

    /// The node a query or listener on `obj` is scoped to.
    fn scope(&self, obj: &Obj) -> Option<NodeId> {
        match obj {
            Obj::Document => Some(self.doc.borrow().root_node().id),
            Obj::Element(id) => Some(*id),
            _ => None,
        }
    }

    fn do_get(&mut self, target: &HostValue, prop: &str) -> Result<HostValue> {
        let obj = self.objects.borrow().get(target)?;
        Ok(match (&obj, prop) {
            (Obj::Document, "body") => match self.doc.borrow().query_selector("body").ok().flatten() {
                Some(id) => self.objects.borrow_mut().element(id),
                None => HostValue::Null,
            },
            (Obj::Element(id), "textContent") => HostValue::str(&self.doc.borrow().get_node(*id).context("removed")?.text_content()),
            (Obj::Element(id), "tagName") => {
                let doc = self.doc.borrow();
                let el = doc.get_node(*id).and_then(|n| n.element_data()).context("not an element")?;
                HostValue::str(&el.name.local.to_ascii_uppercase())
            }
            (Obj::Element(id), "id") => HostValue::str(self.doc.borrow().get_node(*id).and_then(|n| n.attr(LocalName::from("id"))).unwrap_or("")),
            (Obj::Event { kind, .. }, "type") => HostValue::str(kind),
            (Obj::Event { target, .. }, "target") => self.objects.borrow_mut().element(*target),
            _ => bail!("{}.{prop} is not supported yet", obj.interface()),
        })
    }

    fn do_set(&mut self, target: &HostValue, prop: &str, value: &HostValue) -> Result<()> {
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
            (Obj::Document | Obj::Element(_), "addEventListener") => {
                let node = self.scope(&obj).context("no scope")?;
                let HostValue::CallbackId(callback) = args.get(1).context("addEventListener: missing listener")? else {
                    bail!("addEventListener: listener is not a function");
                };
                self.objects.borrow_mut().listeners.entry((node, text(0)?)).or_default().push(*callback);
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
            (Obj::Math, "random") => {
                // xorshift64: fast, and good enough for a page's Math.random.
                self.rng ^= self.rng << 13;
                self.rng ^= self.rng >> 7;
                self.rng ^= self.rng << 17;
                HostValue::Num((self.rng >> 11) as f64 / (1u64 << 53) as f64)
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
        self.traced(
            "globalThis".into(),
            name.to_string(),
            |h| match name {
                "document" => Ok(h.objects.borrow_mut().singleton("document", Obj::Document)),
                "Math" => Ok(h.objects.borrow_mut().singleton("Math", Obj::Math)),
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
        record(TraceEntry { pack: self.pack, call: "console".into(), args: text.to_string(), result: String::new(), micros: 0 });
    }
}

/// How many trace entries are kept; older ones are dropped.
const TRACE_CAPACITY: usize = 2000;

#[derive(Clone, Debug)]
pub struct TraceEntry {
    pub pack: &'static str,
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
        PageHost::new("test", Rc::new(RefCell::new(doc)), Default::default())
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
        assert!(host.global("window").is_err());
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
