//! The dev-mode permission prompt. A page load that wants to compile source calls `ask`
//! and waits; `PromptBar` (browser UI, never page content) shows the question and answers it.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dioxus_native::prelude::*;
use tokio::sync::oneshot;

use crate::settings::{self, Grant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    AllowSite,
    AllowOnce,
    Block,
}

struct Pending {
    origin: String,
    reply: oneshot::Sender<Answer>,
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
static VERSION: AtomicU64 = AtomicU64::new(0);

/// Ask whether `origin` may compile and run Dart source. A second ask while one is showing
/// replaces it; the first caller then gets `Block`.
pub async fn ask(origin: &str) -> Answer {
    let (reply, answer) = oneshot::channel();
    *PENDING.lock().unwrap() = Some(Pending { origin: origin.to_string(), reply });
    VERSION.fetch_add(1, Ordering::Relaxed);
    answer.await.unwrap_or(Answer::Block)
}

fn answer(a: Answer) {
    if let Some(p) = PENDING.lock().unwrap().take() {
        // Allow-for-site and Block are remembered; Allow-once is not.
        match a {
            Answer::AllowSite => settings::update(|s| {
                s.dev_grants.insert(p.origin.clone(), Grant::Allow);
            }),
            Answer::Block => settings::update(|s| {
                s.dev_grants.insert(p.origin.clone(), Grant::Block);
            }),
            Answer::AllowOnce => {}
        }
        let _ = p.reply.send(a);
    }
    VERSION.fetch_add(1, Ordering::Relaxed);
}

/// The bar under the toolbar that shows a pending dev-mode prompt.
#[component]
pub fn PromptBar() -> Element {
    let mut origin: Signal<Option<String>> = use_signal(|| None);

    // The prompt lives outside Dioxus, so poll it (same pattern as the side panels).
    use_hook(move || {
        spawn(async move {
            let mut seen = u64::MAX;
            loop {
                let version = VERSION.load(Ordering::Relaxed);
                if version != seen {
                    seen = version;
                    origin.set(PENDING.lock().unwrap().as_ref().map(|p| p.origin.clone()));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
    });

    let Some(site) = origin() else { return rsx!() };
    rsx!(
        div { class: "prompt-bar",
            span { class: "prompt-text", strong { "{site}" } " wants to compile and run Dart source." }
            div { class: "prompt-button primary", onclick: move |_| answer(Answer::AllowSite), "Allow for this site" }
            div { class: "prompt-button", onclick: move |_| answer(Answer::AllowOnce), "Allow once" }
            div { class: "prompt-button", onclick: move |_| answer(Answer::Block), "Block" }
        }
    )
}
