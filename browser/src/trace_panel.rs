use std::time::Duration;

use dioxus_native::prelude::*;

use crate::host::{self, TraceEntry};

/// Rows rendered at once; the trace keeps more, but a long list makes every redraw slow.
const MAX_ROWS: usize = 300;

/// Side panel listing every call a language pack made through the host (the boundary tracer).
#[component]
pub fn TracePanel() -> Element {
    let mut entries: Signal<Vec<TraceEntry>> = use_signal(Vec::new);

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

    let all = entries.read();
    let errors = all.iter().filter(|e| e.result.starts_with("ERROR")).count();

    rsx!(
        div { class: "request-panel",
            div { class: "request-panel-header",
                span { class: "request-panel-title", "Trace" }
                span { class: "request-panel-counts", "{all.len()} calls, {errors} errors" }
            }
            div { class: "request-panel-controls",
                div { class: "request-clear", onclick: move |_| host::clear_trace(), "Clear" }
            }
            div { class: "request-list",
                for (i, entry) in all.iter().take(MAX_ROWS).enumerate() {
                    div {
                        key: "{i}",
                        class: if entry.result.starts_with("ERROR") { "request-row blocked" } else { "request-row" },
                        span { class: "request-status", "{entry.pack}" }
                        span { class: "request-url", "{entry.call}({entry.args}) → {entry.result}  {entry.micros}µs" }
                    }
                }
            }
        }
    )
}
