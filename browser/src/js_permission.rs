//! Whether a page's JavaScript runs, and the non-blocking bar that offers to enable it.
//!
//! The rules (`decide`, tested) combine the global default with per-site choices: "Always
//! allow" and "Keep off" are saved; "Allow this visit" lasts until the browser quits. The bar
//! never holds up a page: it renders without JS, and the bar offers to reload with it. "Keep
//! off" and dismissing both stop the bar coming back, so browsing isn't interrupted.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dioxus_native::prelude::*;

use crate::history::HistoryNav;
use crate::settings::{self, JsDefault, Settings};
use crate::tab::{Tab, TabId, TabStoreImplExt, active_tab};

#[derive(Debug, PartialEq)]
pub enum JsDecision {
    Run,
    /// Don't run. `offer` says whether to show the bar.
    Skip { reason: String, offer: bool },
}

/// The rules, separate from any state so they can be tested.
pub fn decide(settings: &Settings, allowed_this_visit: bool, origin: &str) -> JsDecision {
    if settings.js_blocked.contains(origin) {
        return JsDecision::Skip { reason: format!("JS is kept off for {origin}"), offer: false };
    }
    if allowed_this_visit || settings.js_sites.contains(origin) {
        return JsDecision::Run;
    }
    match settings.js_default {
        JsDefault::Allow => JsDecision::Run,
        JsDefault::Off => JsDecision::Skip { reason: format!("JS is off by default ({origin})"), offer: false },
        JsDefault::Ask => JsDecision::Skip { reason: format!("JS is off for {origin} until allowed"), offer: true },
    }
}

/// A site's own JavaScript choice, as the Site panel shows and cycles it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SiteChoice {
    /// No choice saved: the global default applies (or "allow this visit").
    Default,
    AlwaysAllow,
    KeptOff,
}

impl SiteChoice {
    pub fn of(settings: &Settings, origin: &str) -> Self {
        if settings.js_blocked.contains(origin) {
            SiteChoice::KeptOff
        } else if settings.js_sites.contains(origin) {
            SiteChoice::AlwaysAllow
        } else {
            SiteChoice::Default
        }
    }

    /// The cycle: default → always allowed → kept off → default.
    pub fn next(self) -> Self {
        match self {
            SiteChoice::Default => SiteChoice::AlwaysAllow,
            SiteChoice::AlwaysAllow => SiteChoice::KeptOff,
            SiteChoice::KeptOff => SiteChoice::Default,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SiteChoice::Default => "default",
            SiteChoice::AlwaysAllow => "always allowed",
            SiteChoice::KeptOff => "kept off",
        }
    }

    fn apply(self, settings: &mut Settings, origin: &str) {
        settings.js_sites.remove(origin);
        settings.js_blocked.remove(origin);
        match self {
            SiteChoice::Default => {}
            SiteChoice::AlwaysAllow => {
                settings.js_sites.insert(origin.to_string());
            }
            SiteChoice::KeptOff => {
                settings.js_blocked.insert(origin.to_string());
            }
        }
    }
}

/// Move `origin` to its next choice and save it. Any "allow this visit" and pending offer
/// are dropped, since the user has now chosen. The Site panel and the command palette both
/// use this, so they always agree. Returns the new choice; it applies on reload.
pub fn cycle_site(origin: &str) -> SiteChoice {
    revoke_this_visit(origin);
    clear_offer(origin);
    let next = SiteChoice::of(&settings::get(), origin).next();
    settings::update(|s| next.apply(s, origin));
    next
}

/// Session state: never saved.
#[derive(Default)]
struct Session {
    /// "Allow this visit".
    allowed: BTreeSet<String>,
    /// Origins whose bar was dismissed this session.
    dismissed: BTreeSet<String>,
    /// Pages offering the bar: origin → how many scripts didn't run.
    offers: BTreeMap<String, usize>,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);
static VERSION: AtomicU64 = AtomicU64::new(0);

fn with_session<T>(f: impl FnOnce(&mut Session) -> T) -> T {
    let mut slot = SESSION.lock().unwrap();
    let out = f(slot.get_or_insert_with(Session::default));
    VERSION.fetch_add(1, Ordering::Relaxed);
    out
}

pub fn allowed_this_visit(origin: &str) -> bool {
    with_session(|s| s.allowed.contains(origin))
}

/// A page on `origin` skipped `scripts` scripts and may show the bar.
pub fn offer(origin: &str, scripts: usize) {
    with_session(|s| {
        if !s.dismissed.contains(origin) {
            s.offers.insert(origin.to_string(), scripts);
        }
    });
}

/// Forget any pending offer for `origin` (e.g. after a choice in the Site panel).
pub fn clear_offer(origin: &str) {
    with_session(|s| {
        s.offers.remove(origin);
    });
}

pub fn allow_this_visit(origin: &str) {
    with_session(|s| {
        s.allowed.insert(origin.to_string());
        s.offers.remove(origin);
    });
}

pub fn revoke_this_visit(origin: &str) {
    with_session(|s| {
        s.allowed.remove(origin);
    });
}

fn always_allow(origin: &str) {
    settings::update(|s| {
        s.js_blocked.remove(origin);
        s.js_sites.insert(origin.to_string());
    });
    clear_offer(origin);
}

fn keep_off(origin: &str) {
    settings::update(|s| {
        s.js_sites.remove(origin);
        s.js_blocked.insert(origin.to_string());
    });
    clear_offer(origin);
}

fn dismiss(origin: &str) {
    with_session(|s| {
        s.dismissed.insert(origin.to_string());
        s.offers.remove(origin);
    });
}

/// The bar for the active tab's site, when its scripts didn't run and it may offer JS.
#[component]
pub fn JsBar(tabs: Store<Vec<Tab>>, active_tab_id: Signal<TabId>) -> Element {
    let mut shown: Signal<Option<(String, usize)>> = use_signal(|| None);

    // The offers live outside Dioxus, and the tab can change; poll both (like the panels).
    use_hook(move || {
        spawn(async move {
            let mut last = (u64::MAX, String::new());
            loop {
                let page = active_tab(tabs, active_tab_id()).nav_history().current_url().read().url.clone();
                let origin = settings::origin_of(&page);
                let version = VERSION.load(Ordering::Relaxed);
                if (version, origin.clone()) != last {
                    last = (version, origin.clone());
                    let count = SESSION.lock().unwrap().as_ref().and_then(|s| s.offers.get(&origin).copied());
                    shown.set(count.map(|n| (origin, n)));
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        });
    });

    let Some((origin, scripts)) = shown() else { return rsx!() };
    let reload = move || active_tab(tabs, active_tab_id()).reload();
    let (o1, o2, o3, o4) = (origin.clone(), origin.clone(), origin.clone(), origin.clone());
    rsx!(
        div { class: "prompt-bar js-bar",
            span { class: "prompt-text",
                "JavaScript is off for " strong { "{origin}" } " ({scripts} script(s) didn't run)."
            }
            div { class: "prompt-button", onclick: move |_| { allow_this_visit(&o1); reload(); }, "Allow this visit" }
            div { class: "prompt-button primary", onclick: move |_| { always_allow(&o2); reload(); }, "Always allow" }
            div { class: "prompt-button", onclick: move |_| keep_off(&o3), "Keep off" }
            div { class: "prompt-button", title: "Dismiss", onclick: move |_| dismiss(&o4), "×" }
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SITE: &str = "https://news.example";

    #[test]
    fn default_ask_skips_and_offers() {
        assert_eq!(decide(&Settings::default(), false, SITE), JsDecision::Skip { reason: format!("JS is off for {SITE} until allowed"), offer: true });
    }

    #[test]
    fn default_off_skips_quietly_and_allow_runs() {
        let off = Settings { js_default: JsDefault::Off, ..Settings::default() };
        assert!(matches!(decide(&off, false, SITE), JsDecision::Skip { offer: false, .. }));
        let allow = Settings { js_default: JsDefault::Allow, ..Settings::default() };
        assert_eq!(decide(&allow, false, SITE), JsDecision::Run);
    }

    #[test]
    fn site_choices_override_the_default() {
        let mut s = Settings { js_default: JsDefault::Off, ..Settings::default() };
        s.js_sites.insert(SITE.into());
        assert_eq!(decide(&s, false, SITE), JsDecision::Run, "always allow beats default off");
        assert_eq!(decide(&Settings::default(), true, SITE), JsDecision::Run, "allowed this visit");

        let mut blocked = Settings { js_default: JsDefault::Allow, ..Settings::default() };
        blocked.js_blocked.insert(SITE.into());
        assert!(matches!(decide(&blocked, true, SITE), JsDecision::Skip { offer: false, .. }), "keep off beats everything, quietly");
    }

    #[test]
    fn dismissed_sites_are_not_offered_again() {
        let origin = "https://dismissed.example";
        dismiss(origin);
        offer(origin, 3);
        assert!(SESSION.lock().unwrap().as_ref().unwrap().offers.get(origin).is_none());
    }

    #[test]
    fn site_choices_cycle_and_replace_each_other() {
        let mut s = Settings::default();
        let o = "https://a.test";
        let mut seen = vec![];
        for _ in 0..3 {
            let next = SiteChoice::of(&s, o).next();
            next.apply(&mut s, o);
            seen.push((next, decide(&s, false, o) == JsDecision::Run));
            // Never both allowed and kept off.
            assert!(!(s.js_sites.contains(o) && s.js_blocked.contains(o)));
        }
        assert_eq!(seen, [(SiteChoice::AlwaysAllow, true), (SiteChoice::KeptOff, false), (SiteChoice::Default, false)]);
    }
}
