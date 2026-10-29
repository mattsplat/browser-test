use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use dioxus_native::prelude::*;

use crate::gate::Gate;
use crate::js_permission;
use crate::settings::{self, Grant, SiteCss};
use crate::site_css;
use crate::tab::{Tab, TabId, TabStoreImplExt, active_tab};
use crate::history::HistoryNav;

/// A third-party site the page tried to reach.
#[derive(Clone, PartialEq)]
struct SeenSite {
    requests: usize,
    blocked: usize,
}

/// Per-site policy for the page in the active tab: which packs run, the dev-mode grant, and
/// which other sites it may reach. Changes apply on reload.
#[component]
pub fn SitePanel(tabs: Store<Vec<Tab>>, active_tab_id: Signal<TabId>) -> Element {
    let gate = use_context::<Arc<Gate>>();
    // Bumped after every settings change so the panel re-reads them.
    let changes = use_signal(|| 0u64);
    let mut seen: Signal<BTreeMap<String, SeenSite>> = use_signal(BTreeMap::new);

    let tab = active_tab(tabs, active_tab_id());
    let page = tab.nav_history().current_url().read().url.clone();
    let origin = settings::origin_of(&page);

    // Third-party sites the active tab's origin asked for, from the gate log (polled like
    // the other panels). The tab can navigate while the panel is open, so the origin is
    // re-read on every pass.
    {
        let gate = gate.clone();
        use_hook(move || {
            spawn(async move {
                let mut last = (u64::MAX, String::new());
                loop {
                    let page = active_tab(tabs, active_tab_id()).nav_history().current_url().read().url.clone();
                    let origin = settings::origin_of(&page);
                    if (gate.version(), origin.clone()) != last {
                        last = (gate.version(), origin.clone());
                        let mut sites: BTreeMap<String, SeenSite> = BTreeMap::new();
                        for entry in gate.log() {
                            if entry.page.as_deref() != Some(origin.as_str()) {
                                continue;
                            }
                            if let Some(site) = entry.third_party {
                                let s = sites.entry(site).or_insert(SeenSite { requests: 0, blocked: 0 });
                                s.requests += 1;
                                s.blocked += usize::from(!entry.allowed);
                            }
                        }
                        seen.set(sites);
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            });
        });
    }

    let _ = changes();
    let current = settings::get();
    let dart_on = !current.dart_blocked.contains(&origin);
    let python_on = !current.python_blocked.contains(&origin);
    // This site's JavaScript state: its own choice, or the global default.
    let this_visit = js_permission::allowed_this_visit(&origin);
    let (js_text, js_class) = if current.js_blocked.contains(&origin) {
        ("Kept off".to_string(), "request-toggle on")
    } else if current.js_sites.contains(&origin) {
        ("Always allowed".to_string(), "request-toggle allowed")
    } else if this_visit {
        ("Allowed this visit".to_string(), "request-toggle allowed")
    } else {
        (format!("Default ({})", current.js_default.label()), "request-toggle")
    };
    let js_default = current.js_default;
    let grant = current.dev_grants.get(&origin).copied();
    let allowed_sites = current.allowed_third_party.get(&origin).cloned().unwrap_or_default();

    let o = origin.clone();
    let toggle_dart = move |_| {
        let o = o.clone();
        apply(changes, Box::new(move |s| {
            if !s.dart_blocked.remove(&o) {
                s.dart_blocked.insert(o);
            }
        }))
    };
    let o = origin.clone();
    let toggle_python = move |_| {
        let o = o.clone();
        apply(changes, Box::new(move |s| {
            if !s.python_blocked.remove(&o) {
                s.python_blocked.insert(o);
            }
        }))
    };
    let o = origin.clone();
    // Cycles: default → always allowed → kept off → default.
    let cycle_js = move |_| {
        js_permission::cycle_site(&o);
        let mut changes = changes;
        changes += 1;
    };
    let cycle_default = move |_| apply(changes, Box::new(|s| s.js_default = s.js_default.next()));
    let o = origin.clone();
    let reset_grant = move |_| {
        let o = o.clone();
        apply(changes, Box::new(move |s| {
            s.dev_grants.remove(&o);
        }))
    };
    // Cookies are kept per site (registrable domain), like the third-party rule.
    let site = crate::gate::site_of(&page);
    let cookie_count = site.as_deref().map(|s| crate::cookies::jar().count(s)).unwrap_or(0);
    let clear_cookies = {
        let site = site.clone();
        move |_| {
            if let Some(site) = &site {
                crate::cookies::jar().clear_site(site);
            }
            let mut changes = changes;
            changes += 1;
        }
    };
    let toggle_third_party = move |_| apply(changes, Box::new(|s| s.block_third_party = !s.block_third_party));
    let reload = move |_| active_tab(tabs, active_tab_id()).reload();

    let grant_text = match grant {
        Some(Grant::Allow) => "allowed",
        Some(Grant::Block) => "blocked",
        None => "not asked yet",
    };
    let sites = seen.read();

    rsx!(
        div { class: "request-panel",
            div { class: "request-panel-header",
                span { class: "request-panel-title", "Site" }
                span { class: "request-panel-counts", "{origin}" }
            }
            div { class: "site-section",
                div { class: "site-heading", "Languages" }
                div { class: "site-row",
                    span { "Dart" }
                    div { class: if dart_on { "request-toggle" } else { "request-toggle on" }, onclick: toggle_dart,
                        if dart_on { "Allowed" } else { "Blocked" }
                    }
                }
                div { class: "site-row",
                    span { "Python" }
                    div { class: if python_on { "request-toggle" } else { "request-toggle on" }, onclick: toggle_python,
                        if python_on { "Allowed" } else { "Blocked" }
                    }
                }
                div { class: "site-row",
                    span { "JavaScript" }
                    div { class: js_class, onclick: cycle_js, "{js_text}" }
                }
                div { class: "site-row",
                    span { "JavaScript default (all sites)" }
                    div { class: "request-toggle", onclick: cycle_default, "{js_default.label()}" }
                }
                div { class: "site-row",
                    span { "Dev mode: compile Dart source ({grant_text})" }
                    if grant.is_some() {
                        div { class: "request-clear", onclick: reset_grant, "Ask again" }
                    }
                }
            }
            div { class: "site-section",
                div { class: "site-heading", "Third-party sites" }
                div { class: "site-row",
                    span { "Block third-party requests (all sites)" }
                    div { class: if current.block_third_party { "request-toggle on" } else { "request-toggle" }, onclick: toggle_third_party,
                        if current.block_third_party { "ON" } else { "OFF" }
                    }
                }
                if sites.is_empty() {
                    div { class: "site-note", "This page hasn't requested anything from another site." }
                }
                for (site, info) in sites.iter() {
                    ThirdPartyRow {
                        key: "{site}",
                        origin: origin.clone(),
                        site: site.clone(),
                        requests: info.requests,
                        blocked: info.blocked,
                        allowed: allowed_sites.contains(site),
                        changes,
                    }
                }
            }
            div { class: "site-section",
                div { class: "site-heading", "Site data" }
                div { class: "site-row",
                    span { "Cookies for {site.as_deref().unwrap_or(\"this page\")}: {cookie_count}" }
                    if cookie_count > 0 {
                        div { class: "request-clear", onclick: clear_cookies, "Clear cookies" }
                    }
                }
            }
            SiteCssSection { key: "{origin}", origin: origin.clone(), tabs }
            div { class: "request-panel-controls site-footer",
                div { class: "request-clear", onclick: reload, "Reload to apply" }
            }
        }
    )
}

#[component]
fn ThirdPartyRow(origin: String, site: String, requests: usize, blocked: usize, allowed: bool, mut changes: Signal<u64>) -> Element {
    let label = format!("{site}  ({requests} requests, {blocked} blocked)");
    let flip = move |_| {
        let (origin, site) = (origin.clone(), site.clone());
        settings::update(move |s| {
            let list = s.allowed_third_party.entry(origin).or_default();
            if !list.remove(&site) {
                list.insert(site);
            }
        });
        *changes.write() += 1;
    };
    rsx!(
        div { class: if blocked > 0 { "site-row request-row blocked" } else { "site-row request-row" },
            span { class: "request-url", "{label}" }
            div { class: if allowed { "request-toggle allowed" } else { "request-toggle" }, onclick: flip,
                if allowed { "Allowed" } else { "Allow" }
            }
        }
    )
}

/// The site's user stylesheet: edit, apply (to its open pages at once, no reload) and switch
/// on or off. Keyed by origin in the panel, so the draft starts over on another site.
#[component]
fn SiteCssSection(origin: String, tabs: Store<Vec<Tab>>) -> Element {
    let saved = settings::get().site_css.get(&origin).cloned().unwrap_or_default();
    let mut draft = use_signal(|| saved.css.clone());
    let mut enabled = use_signal(|| saved.enabled);

    let save = {
        let origin = origin.clone();
        move |css: String, on: bool| {
            let o = origin.clone();
            let entry = SiteCss { css, enabled: on };
            settings::update(move |s| {
                if entry.css.trim().is_empty() {
                    s.site_css.remove(&o);
                } else {
                    s.site_css.insert(o, entry);
                }
            });
            // Every open page of this site, not just the active tab's.
            let current = settings::get();
            let css = site_css::for_origin(&current, &origin);
            for tab in crate::debug::tabs(tabs, 0) {
                if tab.origin() == origin {
                    if let Some(doc) = tab.page.and_then(|p| p.doc()) {
                        if let Ok(mut doc) = doc.try_borrow_mut() {
                            site_css::apply(&mut doc, css);
                        }
                    }
                }
            }
        }
    };
    let apply = {
        let save = save.clone();
        move |_| {
            enabled.set(true);
            save(draft(), true)
        }
    };
    let toggle = move |_| {
        let on = !enabled();
        enabled.set(on);
        save(draft(), on)
    };

    rsx!(
        div { class: "site-section",
            div { class: "site-heading", "Custom CSS for this site" }
            textarea {
                class: "site-css-input",
                placeholder: "body {{ font-size: 18px; }}",
                value: draft(),
                oninput: move |evt| draft.set(evt.value()),
            }
            div { class: "site-row",
                div { class: if enabled() { "request-toggle allowed" } else { "request-toggle" }, onclick: toggle,
                    if enabled() { "On" } else { "Off" }
                }
                div { class: "request-clear", onclick: apply, "Apply" }
            }
            div { class: "site-note", "Applied after the page's own styles, on this site only. No script runs." }
        }
    )
}

/// Save a settings change and re-render the panel.
fn apply(mut changes: Signal<u64>, change: Box<dyn FnOnce(&mut settings::Settings)>) {
    settings::update(change);
    *changes.write() += 1;
}
