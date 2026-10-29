//! The browser's internal debugging protocol: what its tools can learn about a tab. The
//! Inspector, the Trace panel and AI access all read tabs, the DOM, console and errors, the
//! trace and requests through here, so they agree, and redaction for AI lives in one place.
//!
//! Everything here reads; nothing changes the page. The trace and the request log are
//! global (all tabs), so "a tab's" entries are those from pages on the tab's site (origin).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use blitz_dom::{BaseDocument, NodeId, local_name};
use blitz_traits::net::Url;
use dioxus_native::prelude::*;

use crate::page_document::PageRef;
use crate::gate::{Gate, LogEntry};
use crate::history::HistoryNav;
use crate::host::{self, TraceEntry, TraceKind};
use crate::settings;
use crate::tab::{Tab, TabId, TabStoreExt, TabStoreImplExt};

// ---- Tabs

/// A tab as the tools see it.
pub struct TabInfo {
    pub id: TabId,
    pub url: Url,
    pub title: String,
    pub active: bool,
    /// The loaded page, if it's still alive (about pages and error pages have none).
    pub page: Option<PageRef>,
}

impl TabInfo {
    /// The site the tab's trace, console and requests are filtered by.
    pub fn origin(&self) -> String {
        settings::origin_of(&self.url)
    }
}

pub fn tabs(tabs: Store<Vec<Tab>>, active: TabId) -> Vec<TabInfo> {
    tabs.iter()
        .map(|tab| TabInfo {
            id: tab.tab_id(),
            url: tab.nav_history().current_url().read().url.clone(),
            title: tab.title().read().clone(),
            active: tab.tab_id() == active,
            page: tab.page().read().clone(),
        })
        .collect()
}

/// Tab `wanted`, or the active tab.
pub fn tab(all: Store<Vec<Tab>>, active: TabId, wanted: Option<TabId>) -> Option<TabInfo> {
    let wanted = wanted.unwrap_or(active);
    tabs(all, active).into_iter().find(|t| t.id == wanted)
}

/// Whether trace entry page `page` (a URL) is on site `origin`.
pub fn same_site(page: &str, origin: &str) -> bool {
    Url::parse(page).map(|u| settings::origin_of(&u) == origin).unwrap_or(false)
}

// ---- The DOM

/// How much of the page a tool may see. The browser's own tools see the DOM as it is;
/// AI access never sees form values, scripts, hidden inputs or URL secrets.
#[derive(Clone, Copy, PartialEq)]
pub enum Privacy {
    Full,
    Redacted,
}

/// One DOM node as the tools show it.
pub struct NodeView {
    pub kind: NodeKind,
    pub children: Vec<NodeId>,
}

pub enum NodeKind {
    Element {
        tag: String,
        attrs: Vec<(String, String)>,
        /// A form field whose value was withheld (redacted views only).
        value_hidden: bool,
    },
    /// Text, with runs of whitespace collapsed; whitespace-only text is left out.
    Text(String),
    /// The document itself.
    Document,
}

/// Node `id`, or None if it doesn't exist, isn't shown (comments, whitespace) or is withheld.
pub fn node(doc: &BaseDocument, id: NodeId, privacy: Privacy) -> Option<NodeView> {
    let node = doc.get_node(id)?;
    let redacted = privacy == Privacy::Redacted;
    let kind = if let Some(el) = node.element_data() {
        let tag = el.name.local.to_string();
        if redacted {
            if matches!(tag.as_str(), "script" | "style" | "template" | "link" | "meta") {
                return None;
            }
            if tag == "input" && el.attr(local_name!("type")).is_some_and(|t| t.eq_ignore_ascii_case("hidden")) {
                return None;
            }
        }
        let attrs = el
            .attrs()
            .iter()
            .filter_map(|attr| {
                let name = attr.name.local.to_string();
                if !redacted {
                    return Some((name, attr.value.to_string()));
                }
                let value = match name.as_str() {
                    "href" | "src" | "action" => redact_relative(doc, &attr.value),
                    "id" | "class" | "alt" | "title" | "role" | "aria-label" | "type" | "name" | "placeholder" | "for" | "method" | "disabled" | "checked" => {
                        attr.value.to_string()
                    }
                    _ => return None,
                };
                Some((name, value))
            })
            .collect();
        let value_hidden = redacted && matches!(tag.as_str(), "input" | "textarea" | "select");
        // A textarea's text is its value.
        let children = if redacted && tag == "textarea" { Vec::new() } else { node.children.to_vec() };
        return Some(NodeView { kind: NodeKind::Element { tag, attrs, value_hidden }, children });
    } else if let Some(text) = node.text_data() {
        let text = text.content.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            return None;
        }
        NodeKind::Text(text)
    } else if id == doc.root_node().id {
        NodeKind::Document
    } else {
        return None;
    };
    Some(NodeView { kind, children: node.children.to_vec() })
}

/// `tag#id.class`, the short name tools use for an element.
pub fn describe(doc: &BaseDocument, id: NodeId) -> String {
    let Some(el) = doc.get_node(id).and_then(|n| n.element_data()) else { return format!("node {}", id.as_u64()) };
    let mut out = el.name.local.to_string();
    if let Some(id) = el.attr(local_name!("id")) {
        out.push_str(&format!("#{id}"));
    }
    if let Some(class) = el.attr(local_name!("class")) {
        for c in class.split_whitespace().take(3) {
            out.push_str(&format!(".{c}"));
        }
    }
    out
}

/// An element's box, in page coordinates (CSS pixels).
pub struct BoxModel {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Top, right, bottom, left.
    pub margin: [f32; 4],
    pub border: [f32; 4],
    pub padding: [f32; 4],
}

/// Computed styles worth showing for a selected element.
const KEY_STYLES: &[&str] = &[
    "display", "position", "box-sizing", "color", "background-color", "font-family", "font-size", "font-weight",
    "line-height", "text-align", "visibility", "opacity", "overflow-x", "overflow-y", "z-index",
];

/// Where element `id` was laid out, from Blitz's last layout.
pub fn box_model(doc: &BaseDocument, id: NodeId) -> Option<BoxModel> {
    let node = doc.get_node(id)?;
    node.element_data()?;
    let layout = node.final_layout();
    let at = node.absolute_position(0.0, 0.0);
    Some(BoxModel {
        x: at.x,
        y: at.y,
        width: layout.size.width,
        height: layout.size.height,
        margin: [layout.margin.top, layout.margin.right, layout.margin.bottom, layout.margin.left],
        border: [layout.border.top, layout.border.right, layout.border.bottom, layout.border.left],
        padding: [layout.padding.top, layout.padding.right, layout.padding.bottom, layout.padding.left],
    })
}

/// Key computed styles of element `id` (empty values left out).
pub fn computed_styles(doc: &BaseDocument, id: NodeId) -> Vec<(&'static str, String)> {
    KEY_STYLES.iter().map(|&name| (name, doc.resolved_style_value(id, name))).filter(|(_, v)| !v.is_empty()).collect()
}

/// The most a page outline returns, in characters.
const MAX_OUTLINE: usize = 60_000;

/// The page as indented lines: `tag#id.class attr="…" "text"`, as AI access returns it
/// (redacted: no scripts, styles, hidden inputs or form field values).
pub fn outline(doc: &BaseDocument) -> String {
    let mut out = String::new();
    write_outline(doc, doc.root_node().id, 0, &mut out);
    if out.len() > MAX_OUTLINE {
        let mut cut = MAX_OUTLINE;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push_str("\n… (truncated)");
    }
    out
}

fn write_outline(doc: &BaseDocument, id: NodeId, depth: usize, out: &mut String) {
    if out.len() > MAX_OUTLINE {
        return;
    }
    let Some(view) = node(doc, id, Privacy::Redacted) else { return };
    let depth = match &view.kind {
        NodeKind::Document => depth,
        NodeKind::Text(text) => {
            let short: String = text.chars().take(300).collect();
            push_line(out, depth, &format!("{short:?}"));
            return;
        }
        NodeKind::Element { attrs, value_hidden, .. } => {
            let mut line = describe(doc, id);
            for (name, value) in attrs {
                if name != "id" && name != "class" {
                    line.push_str(&format!(" {name}={value:?}"));
                }
            }
            if *value_hidden {
                line.push_str(" [value hidden]");
            }
            push_line(out, depth, &line);
            depth + 1
        }
    };
    for &child in &view.children {
        write_outline(doc, child, depth, out);
    }
}

fn push_line(out: &mut String, depth: usize, line: &str) {
    for _ in 0..depth {
        out.push_str("  ");
    }
    out.push_str(line);
    out.push('\n');
}

/// A URL without the parts that commonly carry secrets: credentials, query values and the
/// fragment (OAuth tokens travel in both).
pub fn redact(url: &str) -> String {
    let Ok(mut u) = Url::parse(url) else { return "[unreadable URL]".into() };
    let _ = u.set_password(None);
    let _ = u.set_username("");
    if u.query().is_some() {
        let keys = u.query_pairs().map(|(k, _)| format!("{k}=REDACTED")).collect::<Vec<_>>().join("&");
        u.set_query(if keys.is_empty() { None } else { Some(&keys) });
    }
    if u.fragment().is_some() {
        u.set_fragment(Some("REDACTED"));
    }
    u.to_string()
}

/// Attribute URLs are often relative; resolve them against the page so they can be redacted.
fn redact_relative(doc: &BaseDocument, value: &str) -> String {
    match doc.url().join(value) {
        Ok(u) => redact(u.as_str()),
        Err(_) => "[unreadable URL]".into(),
    }
}

// ---- Console, trace and requests

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Level {
    Debug,
    Log,
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Log => "log",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

/// A console line: something the page logged, an error it hit, or console input and its result.
#[derive(Clone, Debug, PartialEq)]
pub struct ConsoleEntry {
    pub level: Level,
    pub pack: &'static str,
    /// For errors, what was running (e.g. `click listener(node 12)`); empty otherwise.
    pub context: String,
    /// The message, error (with its source-mapped stack) or result.
    pub text: String,
    /// For console input: what was typed. `text` is then its result.
    pub input: Option<String>,
}

/// The pack name console input is traced under (AI input is traced as "ai").
pub const CONSOLE_PACK: &str = "console";

/// A trace entry as a console line, if it is one.
pub fn console_entry(e: &TraceEntry) -> Option<ConsoleEntry> {
    let error_text = || e.result.trim_start_matches("ERROR ").to_string();
    if e.call == "eval" && (e.pack == CONSOLE_PACK || e.pack == "ai") {
        let failed = e.kind == TraceKind::Error;
        let text = if failed { error_text() } else { e.result.clone() };
        return Some(ConsoleEntry { level: if failed { Level::Error } else { Level::Log }, pack: e.pack, context: String::new(), text, input: Some(e.args.clone()) });
    }
    match e.kind {
        TraceKind::Console => {
            let level = match e.call.strip_prefix("console.") {
                Some("debug") => Level::Debug,
                Some("info") => Level::Info,
                Some("warn") => Level::Warn,
                Some("error") => Level::Error,
                _ => Level::Log,
            };
            Some(ConsoleEntry { level, pack: e.pack, context: String::new(), text: e.args.clone(), input: None })
        }
        TraceKind::Error => Some(ConsoleEntry { level: Level::Error, pack: e.pack, context: format!("{}({})", e.call, e.args), text: error_text(), input: None }),
        _ => None,
    }
}

/// Site `origin`'s console, newest first. Redacted views leave out what the user typed into
/// the console (it may hold anything); the host calls it made stay, as for any page code.
pub fn console(origin: &str, privacy: Privacy, limit: usize) -> Vec<ConsoleEntry> {
    host::trace()
        .iter()
        .filter(|e| same_site(&e.page, origin) && !(privacy == Privacy::Redacted && e.pack == CONSOLE_PACK))
        .filter_map(|e| {
            let mut entry = console_entry(e)?;
            // A failed call that carries secrets (a quota error from `Storage.setItem(k, v)`).
            if privacy == Privacy::Redacted && e.kind == TraceKind::Error && holds_secrets(&e.call) {
                entry.context = format!("{}({HIDDEN})", e.call);
            }
            Some(entry)
        })
        .take(limit)
        .collect()
}

/// Site `origin`'s trace, newest first, without AI access's own calls (and, redacted, without
/// console input).
pub fn trace(origin: &str, privacy: Privacy, limit: usize) -> Vec<TraceEntry> {
    host::trace()
        .into_iter()
        .filter(|e| same_site(&e.page, origin) && e.pack != "ai" && !(privacy == Privacy::Redacted && e.pack == CONSOLE_PACK))
        .take(limit)
        .map(|mut e| {
            if privacy == Privacy::Redacted && holds_secrets(&e.call) {
                e.args = HIDDEN.into();
                e.result = HIDDEN.into();
            }
            e
        })
        .collect()
}

/// What a redacted view shows in place of a secret.
pub const HIDDEN: &str = "(hidden)";

/// Host calls whose arguments or results carry what AI access must never see: cookie values,
/// stored values, request and response bodies and headers, the page's full URLs and history
/// state (AI sees URLs only redacted), WebSocket URLs and messages, IndexedDB values, and form fields' values: `value` and `checked`, and
/// HTML read or written with `innerHTML`/`outerHTML` (which can hold a field's value, so
/// they're hidden on every element rather than only on forms).
pub fn holds_secrets(call: &str) -> bool {
    let member = call.trim_end_matches(" =");
    // Plain objects and arrays hold parsed responses, history states and fetch inits.
    const INTERFACES: [&str; 14] = [
        "Storage.", "Response.", "Headers.", "Promise.", "Location.", "History.", "Object.", "Array.", "WebSocket.",
        "IDBFactory.", "IDBRequest.", "IDBDatabase.", "IDBTransaction.", "IDBObjectStore.",
    ];
    INTERFACES.iter().any(|p| member.starts_with(p))
        || [
            "Document.cookie",
            "Window.fetch",
            "Window.location",
            "Document.location",
            "Event.state",
            "Event.newURL",
            "Event.oldURL",
            "Element.value",
            "Element.checked",
            "Element.innerHTML",
            "Element.outerHTML",
            "Window.WebSocket",
            "Event.data",
            "Event.reason",
        ]
        .contains(&member)
}

/// Requests pages on site `origin` made, newest first.
pub fn requests(gate: &Gate, origin: &str, limit: usize) -> Vec<LogEntry> {
    gate.log().into_iter().filter(|e| e.page.as_deref() == Some(origin)).take(limit).collect()
}

// ---- Profiler

/// One kind of call: how often it ran and the time it took, in microseconds.
#[derive(Clone, Debug, PartialEq)]
pub struct ProfileRow {
    pub label: String,
    pub count: u64,
    pub micros: u64,
}

/// Where a site's script time went: inside its packs (listeners, timers, start-up), and
/// crossing into the host (each host operation), each slowest first.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Profile {
    pub pack: Vec<ProfileRow>,
    pub host: Vec<ProfileRow>,
    pub pack_micros: u64,
    pub host_micros: u64,
}

/// Aggregate trace entries (one site's, see `trace`) into a profile. Pack entries already
/// leave out their host calls, so the two totals don't overlap. Console and AI input aren't
/// the page's work and are left out.
pub fn profile(entries: &[TraceEntry]) -> Profile {
    let mut pack: Vec<ProfileRow> = Vec::new();
    let mut host: Vec<ProfileRow> = Vec::new();
    let add = |rows: &mut Vec<ProfileRow>, label: String, micros: u64| match rows.iter_mut().find(|r| r.label == label) {
        Some(row) => {
            row.count += 1;
            row.micros += micros;
        }
        None => rows.push(ProfileRow { label, count: 1, micros }),
    };
    for e in entries.iter().filter(|e| e.pack != CONSOLE_PACK && e.pack != "ai") {
        match e.kind {
            TraceKind::Pack => add(&mut pack, format!("{}: {}", e.pack, e.call), e.micros),
            // Failed host calls took time too; uncaught errors (no time) aren't host calls.
            TraceKind::Call | TraceKind::Error if e.micros > 0 && e.call != "eval" => add(&mut host, e.call.clone(), e.micros),
            _ => {}
        }
    }
    pack.sort_by(|a, b| b.micros.cmp(&a.micros).then(a.label.cmp(&b.label)));
    host.sort_by(|a, b| b.micros.cmp(&a.micros).then(a.label.cmp(&b.label)));
    Profile { pack_micros: pack.iter().map(|r| r.micros).sum(), host_micros: host.iter().map(|r| r.micros).sum(), pack, host }
}

// ---- Exceptions

/// What was true when a pack hit an uncaught error, kept for the Inspector.
#[derive(Clone, Debug)]
pub struct ExceptionSnapshot {
    /// Distinguishes snapshots in the UI.
    pub id: u64,
    pub pack: &'static str,
    pub page: String,
    /// What was running: `click listener on button#go`, `timer`, `script app.js`.
    pub during: String,
    /// The error, with its source-mapped stack.
    pub error: String,
    /// The page's last host calls before the error, oldest first.
    pub recent_calls: Vec<String>,
    /// What the pack held (handles, listeners, timers).
    pub state: String,
    /// The page's DOM at that moment, as an outline.
    pub dom: String,
}

/// Snapshots kept; older ones are dropped.
const SNAPSHOT_CAPACITY: usize = 20;
/// Host calls a snapshot keeps from before the error.
const RECENT_CALLS: usize = 12;

static PAUSE_ON_EXCEPTIONS: AtomicBool = AtomicBool::new(false);
static SNAPSHOTS: Mutex<Vec<ExceptionSnapshot>> = Mutex::new(Vec::new());
static NEXT_SNAPSHOT: AtomicU64 = AtomicU64::new(1);

/// Off at every launch; the Inspector turns it on.
pub fn pause_on_exceptions() -> bool {
    PAUSE_ON_EXCEPTIONS.load(Ordering::Relaxed)
}

pub fn set_pause_on_exceptions(on: bool) {
    PAUSE_ON_EXCEPTIONS.store(on, Ordering::Relaxed);
}

/// Record a snapshot of `doc` after an uncaught error (call only with pause on exceptions on).
pub fn snapshot_exception(pack: &'static str, page: &str, during: String, error: String, state: String, doc: &BaseDocument) {
    let recent_calls = host::trace()
        .iter()
        .filter(|e| e.page == page && e.kind == TraceKind::Call)
        .take(RECENT_CALLS)
        .map(|e| format!("{}({}) → {}", e.call, e.args, e.result))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let id = NEXT_SNAPSHOT.fetch_add(1, Ordering::Relaxed);
    let snapshot = ExceptionSnapshot { id, pack, page: page.to_string(), during, error, recent_calls, state, dom: outline(doc) };
    let mut all = SNAPSHOTS.lock().unwrap();
    if all.len() == SNAPSHOT_CAPACITY {
        all.remove(0);
    }
    all.push(snapshot);
}

/// Site `origin`'s exception snapshots, newest first.
pub fn exceptions(origin: &str) -> Vec<ExceptionSnapshot> {
    SNAPSHOTS.lock().unwrap().iter().rev().filter(|s| same_site(&s.page, origin)).cloned().collect()
}

pub fn clear_exceptions(origin: &str) {
    SNAPSHOTS.lock().unwrap().retain(|s| !same_site(&s.page, origin));
}

#[cfg(test)]
mod tests {
    use super::*;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;

    #[test]
    fn urls_lose_credentials_query_values_and_fragments() {
        assert_eq!(redact("https://me:pw@example.com/a?token=abc&x=1#access_token=zzz"), "https://example.com/a?token=REDACTED&x=REDACTED#REDACTED");
        assert_eq!(redact("https://example.com/plain"), "https://example.com/plain");
    }

    const SIGN_IN: &str = r#"<html><head><title>Sign in</title><script>var secret = 1</script></head><body>
        <h1 id="t" class="big title">Welcome</h1>
        <a href="/next?session=abc">Next</a>
        <form><input type="password" name="pw" value="hunter2"><input type="hidden" name="csrf" value="tok">
        <textarea name="note">private note</textarea></form></body></html>"#;

    fn sign_in_page() -> BaseDocument {
        use blitz_traits::shell::{ColorScheme, Viewport};
        let viewport = Some(Viewport::new(800, 600, 1.0, ColorScheme::Light));
        let config = DocumentConfig { base_url: Some("https://example.com/login".into()), viewport, ..Default::default() };
        HtmlDocument::from_html(SIGN_IN, config).into_inner()
    }

    #[test]
    fn outline_hides_field_values_and_scripts() {
        let text = outline(&sign_in_page());
        assert!(text.contains("h1#t.big.title"), "{text}");
        assert!(text.contains("\"Welcome\""), "{text}");
        assert!(text.contains(r#"href="https://example.com/next?session=REDACTED""#), "{text}");
        assert!(text.contains(r#"input type="password" name="pw" [value hidden]"#), "{text}");
        for secret in ["hunter2", "csrf", "tok", "private note", "var secret", "abc"] {
            assert!(!text.contains(secret), "{secret} leaked:\n{text}");
        }
    }

    /// Walk every node the given view shows, collecting all text and attribute values.
    fn everything_visible(doc: &BaseDocument, privacy: Privacy) -> String {
        let mut out = String::new();
        let mut stack = vec![doc.root_node().id];
        while let Some(id) = stack.pop() {
            let Some(view) = node(doc, id, privacy) else { continue };
            match &view.kind {
                NodeKind::Element { attrs, .. } => attrs.iter().for_each(|(k, v)| out.push_str(&format!("{k}={v} "))),
                NodeKind::Text(t) => out.push_str(t),
                NodeKind::Document => {}
            }
            stack.extend(view.children.iter().copied());
        }
        out
    }

    /// The Inspector sees the DOM as it is; the redacted view (AI) never sees secrets.
    #[test]
    fn node_views_show_everything_or_redact() {
        let doc = sign_in_page();
        let full = everything_visible(&doc, Privacy::Full);
        for shown in ["hunter2", "csrf", "private note", "var secret", "session=abc"] {
            assert!(full.contains(shown), "{shown} missing from the full view: {full}");
        }
        let redacted = everything_visible(&doc, Privacy::Redacted);
        for secret in ["hunter2", "csrf", "tok", "private note", "var secret", "abc"] {
            assert!(!redacted.contains(secret), "{secret} leaked: {redacted}");
        }
    }

    #[test]
    fn describe_and_box_model() {
        let mut doc = sign_in_page();
        doc.resolve(0.0);
        let h1 = doc.query_selector("h1").unwrap().unwrap();
        assert_eq!(describe(&doc, h1), "h1#t.big.title");
        let b = box_model(&doc, h1).unwrap();
        assert!(b.width > 0.0 && b.height > 0.0, "laid out: {}x{}", b.width, b.height);
        // The UA stylesheet gives h1 a bold font and vertical margins.
        assert!(b.margin[0] > 0.0);
        let styles = computed_styles(&doc, h1);
        assert!(styles.iter().any(|(k, v)| *k == "display" && v == "block"), "{styles:?}");
    }

    #[test]
    fn console_lines_from_the_trace() {
        let entry = |kind, call: &str, args: &str, result: &str| TraceEntry { pack: "js", kind, page: "https://a.test/".into(), call: call.into(), args: args.into(), result: result.into(), micros: 0 };
        let warn = console_entry(&entry(TraceKind::Console, "console.warn", "careful", "")).unwrap();
        assert_eq!((warn.level, warn.text.as_str()), (Level::Warn, "careful"));
        let print = console_entry(&entry(TraceKind::Console, "console", "hi", "")).unwrap();
        assert_eq!(print.level, Level::Log);
        let error = console_entry(&entry(TraceKind::Error, "click listener", "node 3", "ERROR Uncaught TypeError: x\n    at f (a.js:1:2)")).unwrap();
        assert_eq!((error.level, error.context.as_str()), (Level::Error, "click listener(node 3)"));
        assert!(error.text.starts_with("Uncaught TypeError"));
        let mut eval = entry(TraceKind::Call, "eval", "1 + 1", "2");
        eval.pack = CONSOLE_PACK;
        let eval = console_entry(&eval).unwrap();
        assert_eq!((eval.input.as_deref(), eval.text.as_str()), (Some("1 + 1"), "2"));
        assert!(console_entry(&entry(TraceKind::Call, "Document.querySelector", "#a", "#1")).is_none());
    }

    #[test]
    fn profile_separates_pack_time_from_host_time() {
        let entry = |pack: &'static str, kind, call: &str, micros| TraceEntry { pack, kind, page: "https://p.test/".into(), call: call.into(), args: String::new(), result: String::new(), micros };
        let entries = [
            entry("js", TraceKind::Pack, "click listener", 300),
            entry("js", TraceKind::Pack, "click listener", 100),
            entry("js", TraceKind::Pack, "timer", 50),
            entry("js", TraceKind::Call, "Element.textContent =", 40),
            entry("js", TraceKind::Call, "Document.querySelector", 70),
            entry("js", TraceKind::Call, "Element.textContent =", 40),
            entry("js", TraceKind::Error, "click listener", 0),
            entry(CONSOLE_PACK, TraceKind::Call, "Element.textContent =", 900),
            entry("js", TraceKind::Console, "console", 0),
        ];
        let p = profile(&entries);
        let rows = |r: &[ProfileRow]| r.iter().map(|r| (r.label.clone(), r.count, r.micros)).collect::<Vec<_>>();
        assert_eq!(rows(&p.pack), [("js: click listener".to_string(), 2, 400), ("js: timer".to_string(), 1, 50)]);
        assert_eq!(rows(&p.host), [("Element.textContent =".to_string(), 2, 80), ("Document.querySelector".to_string(), 1, 70)]);
        assert_eq!((p.pack_micros, p.host_micros), (450, 150));
    }

    /// What the user types into the console never reaches the redacted (AI) view.
    #[test]
    fn redacted_console_and_trace_leave_out_console_input() {
        let page = "https://console-privacy.test/";
        let typed = TraceEntry { pack: CONSOLE_PACK, kind: TraceKind::Call, page: page.into(), call: "eval".into(), args: "secret typed".into(), result: "1".into(), micros: 0 };
        host::record(typed);
        let origin = "https://console-privacy.test";
        assert!(console(origin, Privacy::Full, 10).iter().any(|e| e.input.as_deref() == Some("secret typed")));
        assert!(console(origin, Privacy::Redacted, 10).is_empty());
        assert!(trace(origin, Privacy::Redacted, 10).is_empty());
        assert_eq!(trace(origin, Privacy::Full, 10).len(), 1);
    }
}
