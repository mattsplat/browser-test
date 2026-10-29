//! The Inspector: the browser's dev tools for the active tab, in one side panel.
//!
//! - **Elements:** the page's DOM as a tree. Selecting an element shows its attributes, its
//!   box and key computed styles, and highlights it on the page.
//! - **Console:** what the page logged and the errors it hit, filterable by level, with an
//!   input line that evaluates in the page's JS, Python or PHP pack (Dart can't; see `PackRuntime`).
//! - **Exceptions:** pause on exceptions, and the snapshots it kept.
//! - **Handles:** what the page's pack holds: host objects by handle, listeners per node,
//!   pending timers, and how often each callback ran.
//! - **Profiler:** time inside the pack per kind of call, and time in each host operation,
//!   aggregated from the trace (so it covers the trace's last 2000 entries).
//! - **Recording:** record what the page's pack receives (reloading it first), then replay it
//!   in a fresh pack in this tab, all of it or up to step N (`replay.rs`).
//! - **Network** is the Requests panel.
//!
//! Everything is read through `debug`, the same protocol AI access uses. Console input is the
//! user's own action, so it needs no AI permission; it still runs through the host.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::{Rc, Weak};
use std::time::Duration;

use blitz_dom::{BaseDocument, NodeId};
use dioxus_native::prelude::*;

use crate::Panel;
use crate::debug::{self, ConsoleEntry, Level, NodeKind, Privacy};
use crate::host;
use crate::nav::is_enter_key;
use crate::document_loader::make_doc_config;
use crate::history::HistoryNav;
use crate::page_document::PageRef;
use crate::replay;
use crate::tab::{Tab, TabId, TabStoreExt, TabStoreImplExt, active_tab};
use dioxus_native::SubDocumentAttr;

/// Tree rows and console lines rendered at once; more make every redraw slow.
const MAX_ROWS: usize = 1500;
const MAX_CONSOLE: usize = 300;

#[derive(Clone, Copy, PartialEq)]
enum View {
    Elements,
    Console,
    Exceptions,
    Handles,
    Profiler,
    Recording,
}

/// Which console lines show.
#[derive(Clone, Copy, PartialEq)]
enum Show {
    All,
    Errors,
    Warnings,
    Logs,
}

impl Show {
    fn includes(self, level: Level) -> bool {
        match self {
            Show::All => true,
            Show::Errors => level == Level::Error,
            Show::Warnings => level == Level::Warn,
            Show::Logs => matches!(level, Level::Log | Level::Info | Level::Debug),
        }
    }
}

/// Tree state belongs to one document; a new page (or a reload) starts fresh. Documents are
/// told apart by address, which is enough while the panel holds no reference to them.
type DocKey = usize;

fn doc_key(doc: &Rc<RefCell<BaseDocument>>) -> DocKey {
    Rc::as_ptr(doc) as usize
}

#[component]
pub fn Inspector(tabs: Store<Vec<Tab>>, active_tab_id: Signal<TabId>, open_panel: Signal<Panel>) -> Element {
    let mut view = use_signal(|| View::Elements);
    // Bumped to re-read the DOM and pause state, which aren't signals.
    let mut tick = use_signal(|| 0u64);
    let mut console: Signal<Vec<ConsoleEntry>> = use_signal(Vec::new);
    let mut expanded: Signal<(DocKey, HashSet<NodeId>)> = use_signal(|| (0, HashSet::new()));
    let mut selected: Signal<Option<(DocKey, NodeId)>> = use_signal(|| None);
    let highlighted: Rc<RefCell<Option<Weak<RefCell<BaseDocument>>>>> = use_hook(Default::default);
    // The snapshot whose DOM outline is shown.
    let show_dom: Signal<Option<u64>> = use_signal(|| None);

    let tab = debug::tab(tabs, active_tab_id(), None);
    let origin = tab.as_ref().map(|t| t.origin()).unwrap_or_default();
    let page: Option<PageRef> = tab.and_then(|t| t.page);
    let doc = page.as_ref().and_then(|p| p.doc());
    let key = doc.as_ref().map(doc_key).unwrap_or(0);

    // Poll like the other panels: the console when the trace changes, the rest on a timer.
    use_hook(move || {
        spawn(async move {
            let mut seen = (u64::MAX, String::new());
            let mut polls = 0u64;
            loop {
                let origin = debug::tab(tabs, active_tab_id(), None).map(|t| t.origin()).unwrap_or_default();
                if (host::trace_version(), origin.clone()) != seen {
                    seen = (host::trace_version(), origin.clone());
                    console.set(debug::console(&origin, Privacy::Full, MAX_CONSOLE));
                }
                // An element clicked with the DevTools picker (toolbar menu) is selected here,
                // with its ancestors opened so its row shows.
                if let Some(doc) = debug::tab(tabs, active_tab_id(), None).and_then(|t| t.page).and_then(|p| p.doc()) {
                    let picked = doc.try_borrow_mut().ok().and_then(|mut d| d.devtools_mut().picked_node.take());
                    if let Some(node) = picked {
                        let key = doc_key(&doc);
                        let d = doc.borrow();
                        let mut open = if expanded.peek().0 == key { expanded.peek().1.clone() } else { default_expanded(&d) };
                        let mut parent = d.get_node(node).and_then(|n| n.parent);
                        while let Some(id) = parent {
                            open.insert(id);
                            parent = d.get_node(id).and_then(|n| n.parent);
                        }
                        drop(d);
                        expanded.set((key, open));
                        selected.set(Some((key, node)));
                        view.set(View::Elements);
                    }
                }
                polls += 1;
                if polls % 2 == 0 && view() != View::Console {
                    tick += 1;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        });
    });

    // Highlight the selected element on its page, and only there.
    {
        let highlighted = highlighted.clone();
        use_effect(move || {
            let doc = debug::tab(tabs, active_tab_id(), None).and_then(|t| t.page).and_then(|p| p.doc());
            let node = selected().filter(|(k, _)| doc.as_ref().is_some_and(|d| doc_key(d) == *k)).map(|(_, n)| n);
            set_highlight(&highlighted, doc.as_ref(), node);
        });
    }
    {
        let highlighted = highlighted.clone();
        use_drop(move || set_highlight(&highlighted, None, None));
    }

    let _ = tick();
    let tab_class = |v: View| if view() == v { "request-toggle allowed" } else { "request-toggle" };

    let body = match view() {
        View::Elements => match &doc {
            None => rsx!(div { class: "inspector-note", "No page is loaded in this tab." }),
            Some(doc) => {
                let open = if expanded().0 == key { expanded().1 } else { default_expanded(&doc.borrow()) };
                let chosen = selected().filter(|(k, _)| *k == key).map(|(_, n)| n);
                let rows = tree_rows(&doc.borrow(), &open);
                let details = chosen.map(|id| node_details(&doc.borrow(), id));
                rsx!(
                    div { class: "request-list inspector-tree",
                        for row in rows {
                            div {
                                key: "{row.id.as_u64()}",
                                class: if chosen == Some(row.id) { "dom-row selected" } else { "dom-row" },
                                style: "padding-left: {8 + row.depth * 14}px",
                                span {
                                    class: "dom-arrow",
                                    onclick: {
                                        let open = open.clone();
                                        move |_| {
                                            let mut set = open.clone();
                                            if !set.remove(&row.id) {
                                                set.insert(row.id);
                                            }
                                            expanded.set((key, set));
                                        }
                                    },
                                    if !row.has_children { "" } else if row.open { "▾" } else { "▸" }
                                }
                                span {
                                    class: if row.is_text { "dom-label dom-text" } else { "dom-label" },
                                    onclick: move |_| selected.set(Some((key, row.id))),
                                    "{row.label}"
                                }
                            }
                        }
                    }
                    if let Some(details) = details {
                        {details}
                    }
                )
            }
        },
        View::Console => rsx!(ConsoleView { page: page.clone(), entries: console, origin: origin.clone() }),
        // A plain function, not a component: it must re-render on every tick.
        View::Exceptions => exceptions_view(page.clone(), origin.clone(), tick, show_dom),
        View::Handles => handles_view(page.clone()),
        View::Profiler => profiler_view(&origin),
        View::Recording => rsx!(RecordingView { tabs, active_tab_id, origin: origin.clone(), tick: tick() }),
    };

    rsx!(
        div { class: "request-panel inspector",
            div { class: "request-panel-header",
                span { class: "request-panel-title", "Inspect" }
                span { class: "request-panel-counts", "{origin}" }
            }
            div { class: "request-panel-controls",
                div { class: tab_class(View::Elements), onclick: move |_| view.set(View::Elements), "Elements" }
                div { class: tab_class(View::Console), onclick: move |_| view.set(View::Console), "Console" }
                div { class: tab_class(View::Exceptions), onclick: move |_| view.set(View::Exceptions), "Exceptions" }
                div { class: tab_class(View::Handles), onclick: move |_| view.set(View::Handles), "Handles" }
                div { class: tab_class(View::Profiler), onclick: move |_| view.set(View::Profiler), "Profiler" }
                div { class: tab_class(View::Recording), onclick: move |_| view.set(View::Recording), "Recording" }
                div { class: "request-toggle", onclick: move |_| open_panel.set(Panel::Requests), "Network ↗" }
            }
            {body}
        }
    )
}

/// Point Blitz's highlight overlay at `node` on `doc`, clearing it wherever it was before.
fn set_highlight(slot: &RefCell<Option<Weak<RefCell<BaseDocument>>>>, doc: Option<&Rc<RefCell<BaseDocument>>>, node: Option<NodeId>) {
    let previous = slot.borrow_mut().take().and_then(|w| w.upgrade());
    if let Some(old) = previous {
        if let Ok(mut old) = old.try_borrow_mut() {
            old.devtools_mut().highlight_node = None;
            old.shell_provider.request_redraw();
        }
    }
    if let (Some(doc), Some(node)) = (doc, node) {
        if let Ok(mut d) = doc.try_borrow_mut() {
            d.devtools_mut().highlight_node = Some(node);
            d.shell_provider.request_redraw();
            *slot.borrow_mut() = Some(Rc::downgrade(doc));
        }
    }
}

// ---- Elements

#[derive(Clone, PartialEq)]
struct TreeRow {
    id: NodeId,
    depth: usize,
    label: String,
    is_text: bool,
    has_children: bool,
    open: bool,
}

/// `<html>` and `<body>` start open, as in other browsers' inspectors.
fn default_expanded(doc: &BaseDocument) -> HashSet<NodeId> {
    ["html", "body"].iter().filter_map(|s| doc.query_selector(s).ok().flatten()).collect()
}

/// The visible rows of the tree: every node under an open one.
fn tree_rows(doc: &BaseDocument, open: &HashSet<NodeId>) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    let root = doc.root_node().id;
    let Some(view) = debug::node(doc, root, Privacy::Full) else { return rows };
    // Depth-first, children in order: push them reversed.
    let mut stack: Vec<(NodeId, usize)> = view.children.iter().rev().map(|&c| (c, 0)).collect();
    while let Some((id, depth)) = stack.pop() {
        if rows.len() >= MAX_ROWS {
            break;
        }
        let Some(view) = debug::node(doc, id, Privacy::Full) else { continue };
        let has_children = view.children.iter().any(|&c| debug::node(doc, c, Privacy::Full).is_some());
        let is_open = has_children && open.contains(&id);
        let (label, is_text) = match &view.kind {
            NodeKind::Element { tag, attrs, .. } => (element_label(tag, attrs), false),
            NodeKind::Text(text) => (format!("\"{}\"", shorten(text, 80)), true),
            NodeKind::Document => continue,
        };
        rows.push(TreeRow { id, depth, label, is_text, has_children, open: is_open });
        if is_open {
            stack.extend(view.children.iter().rev().map(|&c| (c, depth + 1)));
        }
    }
    rows
}

fn element_label(tag: &str, attrs: &[(String, String)]) -> String {
    let mut out = format!("<{tag}");
    for (name, value) in attrs {
        out.push_str(&format!(" {name}=\"{}\"", shorten(value, 40)));
    }
    out.push('>');
    out
}

fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut short: String = text.chars().take(max).collect();
    short.push('…');
    short
}

/// The selected element: attributes, box and computed styles.
fn node_details(doc: &BaseDocument, id: NodeId) -> Element {
    let Some(view) = debug::node(doc, id, Privacy::Full) else {
        return rsx!(div { class: "inspector-details", "The selected node is no longer in the page." });
    };
    let attrs = match &view.kind {
        NodeKind::Element { attrs, .. } => attrs.clone(),
        NodeKind::Text(text) => vec![("text".to_string(), text.clone())],
        NodeKind::Document => Vec::new(),
    };
    let sides = |s: [f32; 4]| format!("{} {} {} {}", s[0], s[1], s[2], s[3]);
    let layout = debug::box_model(doc, id).map(|b| {
        vec![
            ("position".to_string(), format!("{:.0}, {:.0}", b.x, b.y)),
            ("size".to_string(), format!("{:.0} × {:.0}", b.width, b.height)),
            ("margin".to_string(), sides(b.margin)),
            ("border".to_string(), sides(b.border)),
            ("padding".to_string(), sides(b.padding)),
        ]
    });
    let styles = debug::computed_styles(doc, id);
    let name = debug::describe(doc, id);
    rsx!(
        div { class: "inspector-details",
            div { class: "site-heading", "{name}" }
            for (k, v) in attrs {
                div { class: "detail-row", span { class: "detail-key", "{k}" } span { class: "detail-value", "{v}" } }
            }
            if let Some(layout) = layout {
                div { class: "detail-section", "Box (CSS px; margin, border, padding: top right bottom left)" }
                for (k, v) in layout {
                    div { class: "detail-row", span { class: "detail-key", "{k}" } span { class: "detail-value", "{v}" } }
                }
            }
            if !styles.is_empty() {
                div { class: "detail-section", "Computed style" }
                for (k, v) in styles {
                    div { class: "detail-row", span { class: "detail-key", "{k}" } span { class: "detail-value", "{v}" } }
                }
            }
        }
    )
}

// ---- Console

/// A pack's language, as the console's input line names it.
fn language(pack: &str) -> &str {
    match pack {
        "js" => "JavaScript",
        "python" => "Python",
        "php" => "PHP",
        other => other,
    }
}

#[component]
fn ConsoleView(page: Option<PageRef>, entries: Signal<Vec<ConsoleEntry>>, origin: String) -> Element {
    let mut input = use_signal(String::new);
    let mut show = use_signal(|| Show::All);

    let mut run = {
        let page = page.clone();
        move || {
            let source = input().trim().to_string();
            if source.is_empty() {
                return;
            }
            // The result (or error) reaches the list through the trace.
            if let Some(pack) = page.as_ref().and_then(|p| p.pack()) {
                let _ = pack.eval(&source, debug::CONSOLE_PACK);
            }
            input.set(String::new());
        }
    };

    let all = entries.read();
    let count = |level: Level| all.iter().filter(|e| e.level == level).count();
    let (errors, warnings) = (count(Level::Error), count(Level::Warn));
    let choice = |s: Show| if show() == s { "request-toggle allowed" } else { "request-toggle" };
    let rows: Vec<ConsoleEntry> = all.iter().filter(|e| show().includes(e.level)).cloned().collect();

    rsx!(
        div { class: "request-panel-controls",
            div { class: choice(Show::All), onclick: move |_| show.set(Show::All), "All" }
            div { class: choice(Show::Errors), onclick: move |_| show.set(Show::Errors), "Errors ({errors})" }
            div { class: choice(Show::Warnings), onclick: move |_| show.set(Show::Warnings), "Warnings ({warnings})" }
            div { class: choice(Show::Logs), onclick: move |_| show.set(Show::Logs), "Logs" }
            div { class: "request-clear", onclick: move |_| host::clear_trace(), "Clear" }
        }
        match page.as_ref().and_then(|p| p.pack()) {
            Some(pack) if pack.can_eval() => rsx!(
                input {
                    class: "console-input",
                    "type": "text",
                    placeholder: "Evaluate {language(pack.name)} in this page (Enter)",
                    value: input(),
                    oninput: move |evt| input.set(evt.value()),
                    onkeydown: move |evt| {
                        if is_enter_key(&evt.key()) {
                            evt.prevent_default();
                            run();
                        }
                    },
                }
            ),
            Some(_) => rsx!(div { class: "inspector-note",
                "Dart pages can't evaluate code: dart2wasm compiles ahead of time and the page ships no compiler. Use Elements to inspect the page, or print from the Dart source."
            }),
            None => rsx!(div { class: "inspector-note", "No script runs on this page, so there's nothing to evaluate in." }),
        }
        div { class: "request-list",
            if rows.is_empty() {
                div { class: "inspector-note", "Nothing logged for {origin}." }
            }
            for (i, entry) in rows.iter().enumerate() {
                div {
                    key: "{i}",
                    class: match entry.level {
                        Level::Error => "console-row error",
                        Level::Warn => "console-row warn",
                        _ => "console-row",
                    },
                    if let Some(typed) = &entry.input {
                        div { class: "console-input-echo", "› {typed}" }
                        div { class: "console-text", "‹ {entry.text}" }
                    } else {
                        if !entry.context.is_empty() {
                            div { class: "console-context", "{entry.pack}: in {entry.context}" }
                        }
                        div { class: "console-text",
                            if entry.level == Level::Info || entry.level == Level::Debug { "[{entry.level.name()}] " }
                            "{entry.text}"
                        }
                    }
                }
            }
        }
    )
}

// ---- Exceptions

/// Its actions bump `tick` so the Inspector re-reads what they changed.
fn exceptions_view(page: Option<PageRef>, origin: String, mut tick: Signal<u64>, mut show_dom: Signal<Option<u64>>) -> Element {
    let pack = page.as_ref().and_then(|p| p.pack());
    let paused = pack.as_ref().is_some_and(|p| p.is_paused());
    let snapshots = debug::exceptions(&origin);
    let on = debug::pause_on_exceptions();
    rsx!(
        div { class: "site-section",
            div { class: "site-row",
                span { "Pause on exceptions" }
                div {
                    class: if on { "request-toggle on" } else { "request-toggle" },
                    onclick: move |_| {
                        debug::set_pause_on_exceptions(!on);
                        tick += 1;
                    },
                    if on { "ON" } else { "OFF" }
                }
            }
            div { class: "site-note",
                "An uncaught error keeps a snapshot here, and the page's script gets no more events or timers until you resume. Off at every launch."
            }
            if paused {
                div { class: "site-row paused-bar",
                    span { "This page is paused on an exception." }
                    div {
                        class: "prompt-button primary",
                        onclick: move |_| {
                            if let Some(p) = &pack {
                                p.resume();
                            }
                            tick += 1;
                        },
                        "Resume"
                    }
                }
            }
        }
        div { class: "request-panel-controls",
            span { class: "request-panel-counts", "{snapshots.len()} snapshots for {origin}" }
            div {
                class: "request-clear",
                onclick: move |_| {
                    debug::clear_exceptions(&origin);
                    tick += 1;
                },
                "Clear"
            }
        }
        div { class: "request-list",
            for snap in snapshots {
                div { key: "{snap.id}", class: "snapshot",
                    div { class: "site-heading", "{snap.pack}: {snap.during}" }
                    div { class: "console-text error-text", "{snap.error}" }
                    div { class: "detail-section", "Page state" }
                    div { class: "console-text", "{snap.state}" }
                    div { class: "detail-section", "Last host calls before the error" }
                    for (i, call) in snap.recent_calls.iter().enumerate() {
                        div { key: "{i}", class: "console-text", "{call}" }
                    }
                    div {
                        class: "detail-section detail-toggle",
                        onclick: move |_| show_dom.set(if show_dom() == Some(snap.id) { None } else { Some(snap.id) }),
                        if show_dom() == Some(snap.id) { "▾ DOM at the time" } else { "▸ DOM at the time" }
                    }
                    if show_dom() == Some(snap.id) {
                        div { class: "console-text", "{snap.dom}" }
                    }
                }
            }
        }
    )
}

// ---- Handles

/// What the page's pack holds, re-read on every tick.
fn handles_view(page: Option<PageRef>) -> Element {
    let Some(pack) = page.as_ref().and_then(|p| p.pack()) else {
        return rsx!(div { class: "inspector-note", "No script runs on this page, so it holds nothing." });
    };
    let h = pack.handles();
    let listener_count: usize = h.listeners.iter().map(|(_, _, ids)| ids.len()).sum();
    rsx!(
        div { class: "request-panel-controls",
            span { class: "request-panel-counts",
                "{h.objects.len()} handles, {listener_count} listeners, {h.timers.len()} timers, {h.callbacks} callbacks, {h.calls} calls"
            }
        }
        div { class: "request-list",
            div { class: "detail-section", "Listeners (node, event: callback × calls)" }
            for (i, (node, kind, ids)) in h.listeners.iter().enumerate() {
                div { key: "l{i}", class: "detail-row",
                    span { class: "detail-key", "{node}" }
                    span { class: "detail-value",
                        "{kind}: "
                        {ids.iter().map(|(id, n)| format!("#{id} × {n}")).collect::<Vec<_>>().join(", ")}
                    }
                }
            }
            div { class: "detail-section", "Timers" }
            if h.timers.is_empty() {
                div { class: "inspector-note", "No timers pending." }
            }
            for (t, n) in h.timers.iter() {
                div { key: "t{t.handle}", class: "detail-row",
                    span { class: "detail-key", "{t.kind} {t.handle}" }
                    span { class: "detail-value", "in {t.due_in_ms:.0} ms → callback #{t.callback} (ran {n}×)" }
                }
            }
            div { class: "detail-section", "Handles (host objects the pack can reach)" }
            for (handle, what) in h.objects.iter() {
                div { key: "h{handle}", class: "detail-row",
                    span { class: "detail-key", "#{handle}" }
                    span { class: "detail-value", "{what}" }
                }
            }
        }
    )
}

// ---- Profiler

fn profiler_view(origin: &str) -> Element {
    let p = debug::profile(&debug::trace(origin, Privacy::Full, usize::MAX));
    let total = (p.pack_micros + p.host_micros).max(1);
    let pct = |micros: u64| micros * 100 / total;
    let ms = |micros: u64| format!("{:.1} ms", micros as f64 / 1000.0);
    let rows = |rows: Vec<debug::ProfileRow>, class: &'static str| {
        rsx!(
            for row in rows {
                div { key: "{row.label}", class: "profile-row",
                    div { class: "profile-label", "{row.label}" }
                    div { class: "profile-bar-track",
                        div { class: "profile-bar {class}", style: "width: {pct(row.micros).max(1)}%" }
                    }
                    div { class: "profile-numbers", "{ms(row.micros)} · {row.count}×" }
                }
            }
        )
    };
    rsx!(
        div { class: "request-panel-controls",
            span { class: "request-panel-counts",
                "In the pack {ms(p.pack_micros)} ({pct(p.pack_micros)}%) · crossing into the host {ms(p.host_micros)} ({pct(p.host_micros)}%)"
            }
            div { class: "request-clear", onclick: move |_| host::clear_trace(), "Clear" }
        }
        div { class: "request-list",
            div { class: "detail-section", "Inside the pack (excluding its host calls), slowest first" }
            {rows(p.pack, "pack")}
            div { class: "detail-section", "Host operations, slowest first" }
            {rows(p.host, "host")}
        }
    )
}

// ---- Recording

/// Record the page, and replay recordings into this tab. `tick` re-renders it with the
/// Inspector's poll, so the step count stays current.
#[component]
fn RecordingView(tabs: Store<Vec<Tab>>, active_tab_id: Signal<TabId>, origin: String, tick: u64) -> Element {
    let _ = tick;
    let mut status = use_signal(String::new);
    // The recording picked, and the step to replay to.
    let mut chosen: Signal<Option<(u64, usize)>> = use_signal(|| None);
    let mut changes = use_signal(|| 0u64);
    let _ = changes();

    let info = debug::tab(tabs, active_tab_id(), None);
    let pack = info.and_then(|t| t.page).and_then(|p| p.pack());
    let recording_steps = pack.as_ref().and_then(|p| p.recorded_steps());
    let recordings = replay::recordings(&origin);

    let record = move |_| {
        let tab = active_tab(tabs, active_tab_id());
        tab.loader_rc().record_next.set(true);
        tab.reload();
        status.set("Reloading to record from the start…".into());
    };
    let stop = {
        let pack = pack.clone();
        move |_| {
            if let Some(r) = pack.as_ref().and_then(|p| p.stop_recording()) {
                let steps = r.steps.len();
                let id = replay::keep(r);
                chosen.set(Some((id, steps)));
                status.set(format!("Recorded {steps} steps."));
            }
            changes += 1;
        }
    };

    let selected = chosen().and_then(|(id, step)| recordings.iter().find(|r| r.id == id).map(|r| (r.clone(), step)));
    rsx!(
        div { class: "site-section",
            match (recording_steps, &pack) {
                (Some(n), _) => rsx!(
                    div { class: "site-row paused-bar",
                        span { "Recording this page: {n} steps" }
                        div { class: "prompt-button primary", onclick: stop, "Stop" }
                    }
                ),
                (None, Some(_)) => rsx!(
                    div { class: "site-row",
                        span { "Record what this page's script receives" }
                        div { class: "prompt-button", onclick: record, "Record" }
                    }
                ),
                (None, None) => rsx!(div { class: "inspector-note", "No script runs on this page, so there's nothing to record." }),
            }
            div { class: "site-note",
                "Recording reloads the page, then keeps every event, timer, clock reading and random number its script gets, in memory only. Replaying runs a fresh copy of the page in this tab and feeds them back."
            }
            if !status().is_empty() {
                div { class: "site-note", "{status}" }
            }
        }
        div { class: "request-list",
            if recordings.is_empty() {
                div { class: "inspector-note", "No recordings for {origin}." }
            }
            for r in recordings.iter() {
                {
                    let (id, total) = (r.id, r.steps.len());
                    let full = if r.full { " (stopped at the size limit)" } else { "" };
                    let open = selected.as_ref().is_some_and(|(s, _)| s.id == id);
                    rsx!(
                        div { key: "{id}", class: "snapshot",
                            div {
                                class: "site-heading detail-toggle",
                                onclick: move |_| chosen.set(if open { None } else { Some((id, total)) }),
                                if open { "▾ " } else { "▸ " }
                                "#{id} {r.pack}: {total} steps, {r.tape.len()} values{full}"
                            }
                            div { class: "console-text", "{r.url}" }
                        }
                    )
                }
            }
            if let Some((rec, step)) = selected {
                {replay_controls(tabs, active_tab_id, rec, step, chosen, status, changes)}
            }
        }
    )
}

/// Step through a recording: pick step N and replay up to it in the tab; verify it replays
/// to the same DOM; forget it.
fn replay_controls(
    tabs: Store<Vec<Tab>>,
    active_tab_id: Signal<TabId>,
    rec: replay::Recording,
    step: usize,
    mut chosen: Signal<Option<(u64, usize)>>,
    mut status: Signal<String>,
    mut changes: Signal<u64>,
) -> Element {
    let (id, total) = (rec.id, rec.steps.len());
    let mut to = move |n: usize| chosen.set(Some((id, n.min(total))));
    let replay_here = {
        let rec = rec.clone();
        move |_| {
            let tab = active_tab(tabs, active_tab_id());
            if tab.nav_history().current_url().read().url.as_str() != rec.url {
                status.set(format!("Open {} in this tab to replay it here.", rec.url));
                return;
            }
            let loader = tab.loader_rc();
            let config = make_doc_config(Some(rec.url.clone()), loader.net_provider.clone(), loader.history, loader.font_ctx.clone(), None);
            let rec = rec.clone();
            status.set(format!("Replaying to step {step}…"));
            spawn(async move {
                match replay::replay(&rec, config, Some(step)).await {
                    Ok((page, outcome)) => {
                        let page_ref = page.page_ref();
                        *tab.document().write() = Some(SubDocumentAttr::new(page));
                        *tab.page().write() = Some(page_ref);
                        status.set(match outcome.diverged {
                            None => format!("Replayed {} of {total} steps; the page is live from here.", outcome.steps),
                            Some(why) => format!("Replayed {} steps, then diverged: {why}", outcome.steps),
                        });
                    }
                    Err(e) => status.set(format!("Replay failed: {e:#}")),
                }
            });
        }
    };
    let verify = {
        let rec = rec.clone();
        move |_| {
            let rec = rec.clone();
            spawn(async move {
                let config = blitz_dom::DocumentConfig { base_url: Some(rec.url.clone()), ..Default::default() };
                status.set(match replay::replay(&rec, config, None).await {
                    Ok((page, outcome)) => {
                        let same = page.page_ref().doc().is_some_and(|d| replay::dom_text(&d.borrow()) == rec.final_dom);
                        match (outcome.diverged, same) {
                            (None, true) => format!("Verified: all {total} steps replay to the same DOM."),
                            (None, false) => "Replayed every step, but the DOM differs from the recording's.".into(),
                            (Some(why), _) => format!("Diverged: {why}"),
                        }
                    }
                    Err(e) => format!("Replay failed: {e:#}"),
                });
            });
        }
    };
    let forget = move |_| {
        replay::forget(id);
        chosen.set(None);
        changes += 1;
    };
    rsx!(
        div { class: "request-panel-controls",
            div { class: "request-toggle", onclick: move |_| to(0), "⏮" }
            div { class: "request-toggle", onclick: move |_| to(step.saturating_sub(1)), "◀" }
            span { class: "request-panel-counts", "step {step} of {total}" }
            div { class: "request-toggle", onclick: move |_| to(step + 1), "▶" }
            div { class: "request-toggle", onclick: move |_| to(total), "⏭" }
        }
        div { class: "request-panel-controls",
            div { class: "prompt-button primary", onclick: replay_here, "Replay to step {step} in this tab" }
            div { class: "prompt-button", onclick: verify, "Verify" }
            div { class: "request-clear", onclick: forget, "Forget" }
        }
        for (i, s) in rec.steps.iter().enumerate().take(MAX_CONSOLE) {
            div {
                key: "{i}",
                class: if i < step { "console-row" } else { "console-row dimmed" },
                onclick: move |_| to(i + 1),
                "{i + 1}. {s.describe()}"
            }
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;

    /// The tree starts with `<html>` and `<body>` open, shows attributes, skips whitespace,
    /// and opening a node shows its children one level deeper.
    #[test]
    fn tree_rows_follow_what_is_open() {
        let html = "<html><head><title>T</title></head><body>\n  <div id=a class=x><p>hi</p></div>\n</body></html>";
        let doc = HtmlDocument::from_html(html, DocumentConfig::default()).into_inner();
        let mut open = default_expanded(&doc);
        let labels = |open: &HashSet<NodeId>| tree_rows(&doc, open).into_iter().map(|r| (r.depth, r.label)).collect::<Vec<_>>();
        let s = |d: usize, l: &str| (d, l.to_string());
        assert_eq!(labels(&open), [s(0, "<html>"), s(1, "<head>"), s(1, "<body>"), s(2, "<div id=\"a\" class=\"x\">")]);

        open.insert(doc.query_selector("#a").unwrap().unwrap());
        open.insert(doc.query_selector("p").unwrap().unwrap());
        assert_eq!(labels(&open)[4..], [s(3, "<p>"), s(4, "\"hi\"")]);
    }
}
