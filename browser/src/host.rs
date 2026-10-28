//! The host dispatcher: the only way a language pack reaches the page. Every call is
//! recorded in the trace (the boundary tracer), with its arguments, result and timing.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use blitz_dom::{BaseDocument, LocalName, NodeId, QualName, ns};

use crate::dart_pack::{Host, HostValue};

/// Click listeners by node: callback ids registered with `domOnClick`.
pub type Listeners = Rc<RefCell<HashMap<NodeId, Vec<usize>>>>;

/// The page as a pack sees it. Packs hold small integer handles, never Blitz node ids.
pub struct PageHost {
    pub pack: &'static str,
    pub doc: Rc<RefCell<BaseDocument>>,
    pub listeners: Listeners,
    /// handles[h - 1] is the node behind handle h. Handle 0 is never issued.
    handles: Vec<NodeId>,
    handle_of: HashMap<NodeId, usize>,
}

impl Host for PageHost {
    fn call(&mut self, name: &str, args: &[HostValue]) -> Result<HostValue> {
        let started = Instant::now();
        let result = self.dispatch(name, args);
        record(TraceEntry {
            pack: self.pack,
            call: name.to_string(),
            args: args.iter().map(|a| a.display()).collect::<Vec<_>>().join(", "),
            result: match &result {
                Ok(v) => v.display(),
                Err(e) => format!("ERROR {e:#}"),
            },
            micros: started.elapsed().as_micros() as u64,
        });
        result
    }

    fn console(&mut self, text: &str) {
        record(TraceEntry {
            pack: self.pack,
            call: "console".into(),
            args: text.to_string(),
            result: String::new(),
            micros: 0,
        });
    }
}

impl PageHost {
    pub fn new(pack: &'static str, doc: Rc<RefCell<BaseDocument>>, listeners: Listeners) -> Self {
        Self { pack, doc, listeners, handles: Vec::new(), handle_of: HashMap::new() }
    }

    fn handle(&mut self, node: NodeId) -> HostValue {
        let handle = *self.handle_of.entry(node).or_insert_with(|| {
            self.handles.push(node);
            self.handles.len()
        });
        HostValue::Num(handle as f64)
    }

    fn dispatch(&mut self, name: &str, args: &[HostValue]) -> Result<HostValue> {
        let arg = |i: usize| args.get(i).with_context(|| format!("{name}: missing argument {i}"));
        let text = |i: usize| arg(i).map(|v| v.display());
        let node = |i: usize| -> Result<NodeId> {
            let handle = arg(i)?.as_num().with_context(|| format!("{name}: argument {i} is not a node handle"))?;
            let id = *self
                .handles
                .get((handle as usize).wrapping_sub(1))
                .with_context(|| format!("{name}: no node handle {handle}"))?;
            if self.doc.borrow().get_node(id).is_none() {
                bail!("{name}: node handle {handle} was removed");
            }
            Ok(id)
        };

        Ok(match name {
            // Returns the first matching node, or -1.
            "domQuery" => {
                let selector = text(0)?;
                let found = self.doc.borrow().query_selector(&selector).map_err(|e| anyhow::anyhow!("bad selector {selector:?}: {e:?}"))?;
                match found {
                    Some(id) => self.handle(id),
                    None => HostValue::Num(-1.0),
                }
            }
            "domCreate" => {
                let tag = text(0)?;
                let name = QualName::new(None, ns!(html), LocalName::from(tag.as_str()));
                let id = self.doc.borrow_mut().mutate().create_element(name, Vec::new());
                self.handle(id)
            }
            "domAppend" => {
                let (parent, child) = (node(0)?, node(1)?);
                self.doc.borrow_mut().mutate().append_children(parent, &[child]);
                HostValue::Undefined
            }
            // Replaces the node's children with one text node (like `textContent = ...`).
            "domSetText" => {
                let (target, value) = (node(0)?, text(1)?);
                let mut doc = self.doc.borrow_mut();
                let mut m = doc.mutate();
                m.remove_and_drop_all_children(target);
                let text_node = m.create_text_node(&value);
                m.append_children(target, &[text_node]);
                HostValue::Undefined
            }
            "domOnClick" => {
                let target = node(0)?;
                let callback = arg(1)?.as_num().context("domOnClick: callback id")? as usize;
                self.listeners.borrow_mut().entry(target).or_default().push(callback);
                HostValue::Undefined
            }
            other => bail!("unknown host call {other}"),
        })
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
        PageHost::new("test", Rc::new(RefCell::new(doc)), Listeners::default())
    }

    fn text_of(host: &PageHost, selector: &str) -> String {
        let doc = host.doc.borrow();
        let id = doc.query_selector(selector).unwrap().unwrap();
        doc.get_node(id).unwrap().text_content()
    }

    fn s(v: &str) -> HostValue {
        HostValue::str(v)
    }

    #[test]
    fn query_set_text_create_and_append_change_the_dom() {
        let mut host = page("<p id=a>old</p><ul id=list></ul>");
        let a = host.call("domQuery", &[s("#a")]).unwrap();
        host.call("domSetText", &[a, s("new")]).unwrap();
        assert_eq!(text_of(&host, "#a"), "new");

        let list = host.call("domQuery", &[s("#list")]).unwrap();
        let item = host.call("domCreate", &[s("li")]).unwrap();
        host.call("domSetText", &[item.clone(), s("one")]).unwrap();
        host.call("domAppend", &[list, item]).unwrap();
        assert_eq!(text_of(&host, "#list li"), "one");
    }

    #[test]
    fn missing_element_is_minus_one_and_bad_handles_error() {
        let mut host = page("<p>x</p>");
        assert_eq!(host.call("domQuery", &[s("#nope")]).unwrap().as_num(), Some(-1.0));
        assert!(host.call("domSetText", &[HostValue::Num(99.0), s("x")]).is_err());
        assert!(host.call("domSetText", &[HostValue::Num(0.0), s("x")]).is_err());
        assert!(host.call("noSuchCall", &[]).is_err());
    }

    #[test]
    fn handles_are_small_and_stable() {
        let mut host = page("<p id=a>x</p>");
        let first = host.call("domQuery", &[s("#a")]).unwrap().as_num();
        let again = host.call("domQuery", &[s("#a")]).unwrap().as_num();
        assert_eq!(first, Some(1.0));
        assert_eq!(again, first);
    }

    #[test]
    fn on_click_registers_listener_for_the_node() {
        let mut host = page("<button id=b>x</button>");
        let b = host.call("domQuery", &[s("#b")]).unwrap();
        host.call("domOnClick", &[b, HostValue::Num(3.0)]).unwrap();
        let listeners = host.listeners.borrow();
        assert_eq!(listeners.values().next().unwrap(), &vec![3]);
    }

    #[test]
    fn every_call_is_traced() {
        let mut host = page("<p id=a>x</p>");
        host.call("domQuery", &[s("#a")]).unwrap();
        let _ = host.call("noSuchCall", &[]);
        let trace = trace();
        assert!(trace.iter().any(|e| e.call == "domQuery" && e.args == "#a"));
        assert!(trace.iter().any(|e| e.call == "noSuchCall" && e.result.starts_with("ERROR")));
    }
}
