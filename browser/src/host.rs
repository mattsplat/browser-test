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
use blitz_dom::{BaseDocument, LocalName, NodeId, QualName, ns};

use crate::dart_pack::{Host, HostValue};

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
}

impl Obj {
    fn interface(&self) -> &'static str {
        match self {
            Obj::Window => "Window",
            Obj::Document => "Document",
            Obj::Math => "Math",
            Obj::Console => "Console",
            Obj::Element(_) => "Element",
            Obj::Event(_) => "Event",
        }
    }
}

struct Timer {
    due: Instant,
    interval: Option<Duration>,
    callback: usize,
    /// requestAnimationFrame: called with a timestamp.
    frame: bool,
}

/// A timer callback that is due, as `take_due_timers` hands it out.
pub struct DueTimer {
    pub callback: usize,
    pub args: Vec<HostValue>,
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

    /// A callback can be released once no listener or timer refers to it.
    fn release_if_unused(&mut self, callback: usize) {
        let in_listeners = self.listeners.values().any(|ids| ids.contains(&callback));
        let in_timers = self.timers.values().any(|t| t.callback == callback);
        if !in_listeners && !in_timers {
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
        self.timers.insert(handle, Timer { due: Instant::now() + delay, interval, callback, frame });
        HostValue::Num(handle as f64)
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
                callback: timer.callback,
                args: if timer.frame { vec![HostValue::Num(elapsed_ms)] } else { Vec::new() },
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
    rng: u64,
}

impl PageHost {
    pub fn new(pack: &'static str, doc: Rc<RefCell<BaseDocument>>, objects: Rc<RefCell<Objects>>) -> Self {
        // Randomness comes from the host so a recording can replay it (principle 4).
        let seed = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
        let page = doc.borrow().url().to_string();
        Self { pack, page, doc, objects, rng: seed | 1 }
    }

    /// Run `f` and record it in the trace as `label(args)`.
    fn traced<T>(&mut self, label: String, args: String, f: impl FnOnce(&mut Self) -> Result<T>, show: impl Fn(&T) -> String) -> Result<T> {
        let started = Instant::now();
        let result = f(self);
        let (kind, shown) = match &result {
            Ok(v) => (TraceKind::Call, show(v)),
            Err(e) => (TraceKind::Error, format!("ERROR {e:#}")),
        };
        record(TraceEntry { pack: self.pack, kind, page: self.page.clone(), call: label, args, result: shown, micros: started.elapsed().as_micros() as u64 });
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
            _ => return None,
        })
    }

    fn do_get(&mut self, target: &HostValue, prop: &str) -> Result<HostValue> {
        let obj = self.objects.borrow().get(target)?;
        Ok(match (&obj, prop) {
            (Obj::Window, "document") => self.objects.borrow_mut().singleton("document", Obj::Document),
            (Obj::Window, "console") => self.objects.borrow_mut().singleton("console", Obj::Console),
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
            (Obj::Event(e), _) => match self.event_prop(e, prop) {
                Some(v) => v,
                None => bail!("Event.{prop} is not supported yet"),
            },
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
                record(TraceEntry { pack: self.pack, kind: TraceKind::Console, page: self.page.clone(), call, args: line, result: String::new(), micros: 0 });
                HostValue::Undefined
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
        record(TraceEntry { pack: self.pack, kind: TraceKind::Console, page: self.page.clone(), call: "console".into(), args: text.to_string(), result: String::new(), micros: 0 });
    }

    fn now(&mut self) -> f64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0)
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
