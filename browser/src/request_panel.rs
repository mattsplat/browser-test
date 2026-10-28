use std::sync::Arc;
use std::time::Duration;

use dioxus_native::prelude::*;

use crate::gate::{Gate, LogEntry};

/// Rows rendered at once; the gate keeps more, but a long list makes every redraw slow.
const MAX_ROWS: usize = 300;

/// Side panel listing every request the gate saw, with the block-all switch.
#[component]
pub fn RequestPanel() -> Element {
    let gate = use_context::<Arc<Gate>>();
    let mut entries: Signal<Vec<LogEntry>> = use_signal(Vec::new);
    let mut block_all = use_signal(|| gate.block_all());

    // The log lives in the gate, not a Dioxus signal, so we poll it at ~4 fps
    // (same pattern as StatusBar) and only copy it when its version changed.
    {
        let gate = gate.clone();
        use_hook(move || {
            spawn(async move {
                let mut seen = u64::MAX;
                loop {
                    let version = gate.version();
                    if version != seen {
                        seen = version;
                        entries.set(gate.log());
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            });
        });
    }

    let toggle_block_all = {
        let gate = gate.clone();
        move |_| {
            let on = !block_all();
            gate.set_block_all(on);
            block_all.set(on);
        }
    };
    let clear = move |_| gate.clear_log();

    let all = entries.read();
    let blocked = all.iter().filter(|e| !e.allowed).count();

    rsx!(
        div { class: "request-panel",
            div { class: "request-panel-header",
                span { class: "request-panel-title", "Requests" }
                span { class: "request-panel-counts", "{all.len()} total, {blocked} blocked" }
            }
            div { class: "request-panel-controls",
                div {
                    class: if block_all() { "request-toggle on" } else { "request-toggle" },
                    onclick: toggle_block_all,
                    if block_all() { "Block all requests: ON" } else { "Block all requests: OFF" }
                }
                div { class: "request-clear", onclick: clear, "Clear" }
            }
            div { class: "request-list",
                for (i, entry) in all.iter().take(MAX_ROWS).enumerate() {
                    div {
                        key: "{i}",
                        class: if entry.allowed { "request-row" } else { "request-row blocked" },
                        span { class: "request-status", if entry.allowed { "ALLOWED" } else { "BLOCKED" } }
                        span { class: "request-url", "{entry.url}" }
                    }
                }
            }
        }
    )
}
