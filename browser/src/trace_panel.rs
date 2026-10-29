use std::time::Duration;

use blitz_traits::net::Url;
use dioxus_native::prelude::*;

use crate::debug;
use crate::host::{self, TraceEntry, TraceKind};
use crate::history::HistoryNav;
use crate::settings;
use crate::tab::{Tab, TabId, TabStoreImplExt, active_tab};

/// Rows rendered at once; the trace keeps more, but a long list makes every redraw slow.
const MAX_ROWS: usize = 300;

/// Which kinds of entry the panel shows.
#[derive(Clone, Copy, PartialEq)]
enum Show {
    All,
    Console,
    Errors,
}

impl Show {
    fn includes(self, kind: TraceKind) -> bool {
        match self {
            Show::All => true,
            Show::Console => kind == TraceKind::Console,
            Show::Errors => kind == TraceKind::Error,
        }
    }
}

fn origin_of_page(page: &str) -> String {
    Url::parse(page).map(|u| settings::origin_of(&u)).unwrap_or_default()
}

/// Side panel listing every call a language pack made through the host (the boundary tracer),
/// with what pages logged and the errors they hit. Shows the active tab's site unless
/// "All tabs" is on.
#[component]
pub fn TracePanel(tabs: Store<Vec<Tab>>, active_tab_id: Signal<TabId>) -> Element {
    let mut entries: Signal<Vec<TraceEntry>> = use_signal(Vec::new);
    let mut show = use_signal(|| Show::All);
    let mut all_tabs = use_signal(|| false);

    // The trace lives in the host module, not a Dioxus signal, so poll it at ~4 fps
    // (same pattern as RequestPanel) and only copy it when its version changed.
    use_hook(move || {
        spawn(async move {
            let mut seen = u64::MAX;
            loop {
                let version = host::trace_version();
                if version != seen {
                    seen = version;
                    entries.set(host::trace());
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        });
    });

    let page = active_tab(tabs, active_tab_id()).nav_history().current_url().read().url.clone();
    let origin = settings::origin_of(&page);
    let all = entries.read();
    let in_scope: Vec<&TraceEntry> = all.iter().filter(|e| all_tabs() || debug::same_site(&e.page, &origin)).collect();
    let errors = in_scope.iter().filter(|e| e.kind == TraceKind::Error).count();
    let logged = in_scope.iter().filter(|e| e.kind == TraceKind::Console).count();
    let rows: Vec<&TraceEntry> = in_scope.iter().copied().filter(|e| show().includes(e.kind)).take(MAX_ROWS).collect();
    let choice = |s: Show| if show() == s { "request-toggle allowed" } else { "request-toggle" };

    rsx!(
        div { class: "request-panel",
            div { class: "request-panel-header",
                span { class: "request-panel-title", "Trace" }
                span { class: "request-panel-counts", "{in_scope.len()} entries, {logged} logged, {errors} errors" }
            }
            div { class: "request-panel-controls",
                div { class: choice(Show::All), onclick: move |_| show.set(Show::All), "All" }
                div { class: choice(Show::Console), onclick: move |_| show.set(Show::Console), "Console" }
                div { class: choice(Show::Errors), onclick: move |_| show.set(Show::Errors), "Errors" }
                div {
                    class: if all_tabs() { "request-toggle allowed" } else { "request-toggle" },
                    onclick: move |_| all_tabs.toggle(),
                    if all_tabs() { "All tabs" } else { "This site" }
                }
                div { class: "request-clear", onclick: move |_| host::clear_trace(), "Clear" }
            }
            div { class: "request-list",
                for (i, entry) in rows.iter().enumerate() {
                    div {
                        key: "{i}",
                        class: if entry.kind == TraceKind::Error { "request-row blocked" } else { "request-row" },
                        span { class: "request-status", "{entry.pack}" }
                        span { class: "request-url",
                            if all_tabs() { "[{origin_of_page(&entry.page)}] " }
                            match entry.kind {
                                // Logs at other levels say so: "[warn] …".
                                TraceKind::Console => match entry.call.strip_prefix("console.") {
                                    Some(level) => rsx!("[{level}] {entry.args}"),
                                    None => rsx!("{entry.args}"),
                                },
                                TraceKind::Load => rsx!("load {entry.args} → {entry.result}"),
                                _ => rsx!("{entry.call}({entry.args}) → {entry.result}  {entry.micros}µs"),
                            }
                        }
                    }
                }
            }
        }
    )
}
