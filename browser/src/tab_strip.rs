//! The tab list, as a strip along the top or a sidebar on the left (Settings → vertical
//! tabs). Both show the current space's tabs with pinned tabs first and compact, the space
//! switcher (when there's more than one space) and a new tab button. Tab operations are in
//! `tab_manager.rs`.

use blitz_traits::net::Url;
use dioxus_native::prelude::*;

use crate::tab::{Favicon, TabId, TabStoreExt, TabStoreImplExt, tab_display_title};
use crate::tab_manager::TabManager;

#[cfg(target_os = "macos")]
const TABSTRIP_CLASS: &str = "tabstrip merged-titlebar";
#[cfg(not(target_os = "macos"))]
const TABSTRIP_CLASS: &str = "tabstrip";

#[cfg(target_os = "macos")]
const VTABS_CLASS: &str = "vtabs merged-titlebar";
#[cfg(not(target_os = "macos"))]
const VTABS_CLASS: &str = "vtabs";

#[component]
pub fn TabStrip(home_url: Url, open_new_tab: Callback<Url>) -> Element {
    let manager = use_context::<TabManager>();
    rsx!(
        div { class: TABSTRIP_CLASS,
            SpaceSwitcher {}
            for id in manager.visible_ids() {
                TabItem { key: "{id}", id }
            }
            div {
                class: "tab-new",
                onclick: move |_| open_new_tab(home_url.clone()),
                "+"
            }
        }
    )
}

#[component]
pub fn VerticalTabs(home_url: Url, open_new_tab: Callback<Url>) -> Element {
    let manager = use_context::<TabManager>();
    let ids = manager.visible_ids();
    let pinned: Vec<TabId> = ids.iter().copied().filter(|&id| is_pinned(manager, id)).collect();
    let others: Vec<TabId> = ids.iter().copied().filter(|&id| !is_pinned(manager, id)).collect();
    rsx!(
        div { class: VTABS_CLASS,
            if !pinned.is_empty() {
                div { class: "vtabs__pinned",
                    for id in pinned {
                        TabItem { key: "{id}", id }
                    }
                }
            }
            div { class: "vtabs__list",
                for id in others {
                    TabItem { key: "{id}", id }
                }
                div {
                    class: "tab-new",
                    onclick: move |_| open_new_tab(home_url.clone()),
                    "+ New tab"
                }
            }
            SpaceSwitcher {}
        }
    )
}

fn is_pinned(manager: TabManager, id: TabId) -> bool {
    manager.tabs.iter().any(|t| t.tab_id() == id && t.pinned().cloned())
}

/// One tab: favicon, title and close button; pinned tabs show only the favicon.
#[component]
fn TabItem(id: TabId) -> Element {
    let mut manager = use_context::<TabManager>();
    let Some(tab) = manager.tabs.iter().find(|t| t.tab_id() == id) else { return rsx!() };
    let is_active = id == (manager.active_tab_id)();
    let pinned = tab.pinned().cloned();
    let title = tab_display_title(tab);
    let favicon_url = tab.favicon_url().cloned();
    let can_close = !pinned && manager.tabs.len() > 1;
    let class = match (is_active, pinned) {
        (true, true) => "tab tab--active tab--pinned",
        (true, false) => "tab tab--active",
        (false, true) => "tab tab--pinned",
        (false, false) => "tab",
    };
    rsx!(
        div {
            class,
            title: "{title}",
            onclick: move |_| manager.activate(id),
            Favicon { url: favicon_url, class: "tab__favicon" }
            if pinned {
                // Pages without a favicon still need something to click.
                if tab.favicon_url().read().is_none() {
                    span { class: "tab__initial", "{initial(&title)}" }
                }
            } else {
                span { class: "tab__title", "{title}" }
            }
            span { class: "tab__tooltip", "{title}" }
            if can_close {
                div {
                    class: "tab__close",
                    onclick: move |evt| { evt.stop_propagation(); manager.close(id); },
                    "×"
                }
            }
        }
    )
}

fn initial(title: &str) -> String {
    title.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "•".into())
}

/// The spaces, as chips; click one to show its tabs. Hidden while there's only one.
#[component]
fn SpaceSwitcher() -> Element {
    let mut manager = use_context::<TabManager>();
    let spaces = manager.spaces.read().clone();
    if spaces.len() < 2 {
        return rsx!();
    }
    let current = manager.current_space.read().clone();
    rsx!(
        div { class: "spaces",
            for name in spaces {
                div {
                    key: "{name}",
                    class: if name == current { "space space--active" } else { "space" },
                    onclick: move |_| manager.switch_space(&name),
                    "{name}"
                }
            }
        }
    )
}
