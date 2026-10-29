use std::time::{Duration, SystemTime};

use blitz_traits::net::{Request, Url};
use dioxus_native::prelude::*;

use crate::browser_history::{
    BrowsingHistoryStoreExt, HistoryEntry, HistoryService, format_elapsed,
};
use crate::nav::{is_enter_key, req_from_string};
use crate::tab::Favicon;

// How often the history page refreshes its "Just now / N min ago" labels.
const HISTORY_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

const NEWTAB_CSS: &str = local_asset!("about-newtab.css");
const STUB_CSS: &str = local_asset!("about-stub.css");
const HISTORY_CSS: &str = local_asset!("about-history.css");
const BLITZ_LOGO_WTIH_TEXT: &str = local_asset!("blitz-logo-with-text3.svg");

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AboutPage {
    NewTab,
    Settings,
    History,
    Bookmarks,
}

impl AboutPage {
    pub fn from_url(url: &Url) -> Option<Self> {
        if url.scheme() != "about" {
            return None;
        }
        match url.path() {
            "newtab" | "" => Some(Self::NewTab),
            "settings" => Some(Self::Settings),
            "history" => Some(Self::History),
            "bookmarks" => Some(Self::Bookmarks),
            _ => None,
        }
    }

    pub const fn title(self) -> &'static str {
        match self {
            Self::NewTab => "New Tab",
            Self::Settings => "Settings",
            Self::History => "History",
            Self::Bookmarks => "Bookmarks",
        }
    }

    pub const fn url_str(self) -> &'static str {
        match self {
            Self::NewTab => "about:newtab",
            Self::Settings => "about:settings",
            Self::History => "about:history",
            Self::Bookmarks => "about:bookmarks",
        }
    }

    #[allow(
        clippy::unwrap_used,
        reason = "URL strings are static literals — Url::parse cannot fail"
    )]
    pub fn parsed_url(self) -> Url {
        Url::parse(self.url_str()).unwrap()
    }
}

#[component]
pub fn AboutPageView(page: AboutPage, on_navigate: Callback<Request>) -> Element {
    match page {
        AboutPage::NewTab => rsx!(NewTabPage { on_navigate }),
        AboutPage::Settings => rsx!(SettingsPage {}),
        AboutPage::History => rsx!(HistoryPage { on_navigate }),
        AboutPage::Bookmarks => rsx!(StubPage { name: "Bookmarks" }),
    }
}

#[component]
fn NewTabPage(on_navigate: Callback<Request>) -> Element {
    let mut query = use_signal(String::new);
    rsx! {
        document::Link { rel: "stylesheet", href: NEWTAB_CSS }
        div { class: "about-newtab",
            div { class: "container",
                img { src: BLITZ_LOGO_WTIH_TEXT, alt: "Blitz" }
                input {
                    class: "search-input",
                    r#type: "text",
                    name: "q",
                    autofocus: true,
                    value: "{query}",
                    oninput: move |evt| query.set(evt.value()),
                    onkeydown: move |evt| {
                        if is_enter_key(&evt.key()) {
                            evt.prevent_default();
                            let q = query.read().clone();
                            if !q.is_empty() {
                                if let Some(req) = req_from_string(&q) {
                                    on_navigate(req);
                                }
                            }
                        }
                    },
                }
            }
        }
    }
}

#[component]
fn HistoryPage(on_navigate: Callback<Request>) -> Element {
    // `now` is bumped on a fixed cadence so each row's elapsed-time label
    // refreshes while the page is open. Computing the label here (rather than
    // per-row) keeps the wall-clock read in one place.
    let mut now = use_signal(SystemTime::now);
    use_future(move || async move {
        loop {
            tokio::time::sleep(HISTORY_REFRESH_INTERVAL).await;
            now.set(SystemTime::now());
        }
    });
    let now = now();

    let history = use_context::<HistoryService>();
    let browsing_history = history.browsing();

    let entries_lens = browsing_history.entries();
    let entries = entries_lens.read();
    rsx! {
        document::Link { rel: "stylesheet", href: HISTORY_CSS }
        div { class: "about-history",
            div { class: "toolbar",
                h1 { "History" }
                if !entries.is_empty() {
                    button {
                        class: "clear-btn",
                        onclick: move |_| history.clear(),
                        "Clear history"
                    }
                }
            }
            if entries.is_empty() {
                p { class: "empty", "No history yet." }
            } else {
                ul {
                    for entry in entries.iter() {
                        HistoryEntryRow {
                            key: "{entry.id}",
                            entry: entry.clone(),
                            elapsed: format_elapsed(entry.visited_at, now),
                            on_navigate,
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn HistoryEntryRow(
    entry: HistoryEntry,
    elapsed: String,
    on_navigate: Callback<Request>,
) -> Element {
    let url = entry.url.clone();
    rsx! {
        li {
            Favicon { url: entry.favicon_url.clone(), class: "favicon" }
            div { class: "entry-info",
                div { class: "entry-title",
                    a {
                        href: "#",
                        onclick: move |evt| {
                            evt.prevent_default();
                            on_navigate(Request::get(url.clone()));
                        },
                        "{entry.title}"
                    }
                }
                div { class: "entry-url", "{entry.url}" }
                div { class: "entry-time", "{elapsed}" }
            }
        }
    }
}

/// about:settings: browser-wide choices for this profile, and the profiles themselves.
#[component]
fn SettingsPage() -> Element {
    // Bumped after a change the page shows but nothing else signals (profile folders).
    let mut changes = use_signal(|| 0u64);
    let mut new_profile = use_signal(String::new);
    let mut message = use_signal(|| None::<String>);
    let _ = changes();

    let mut manager = use_context::<crate::tab_manager::TabManager>();
    let vertical = (manager.vertical)();
    let idle_hours = crate::settings::get().close_idle_tabs_hours;
    let idle_text = match idle_hours {
        None => "Never".to_string(),
        Some(h) if h % 24 == 0 => format!("After {} day{}", h / 24, if h == 24 { "" } else { "s" }),
        Some(h) => format!("After {h} hour{}", if h == 1 { "" } else { "s" }),
    };

    let current = crate::profile::name();
    let profiles = crate::profile::list();
    let open_profile = use_callback(move |name: String| {
        let result = crate::profile::create(&name).and_then(|_| crate::profile::open_in_new_process(&name));
        message.set(Some(match result {
            Ok(()) => format!("Opening \"{name}\" in a new window."),
            Err(e) => e,
        }));
        changes += 1;
    });
    rsx! {
        document::Link { rel: "stylesheet", href: HISTORY_CSS }
        div { class: "about-history about-settings",
            div { class: "toolbar", h1 { "Settings" } }
            h2 { "Tabs" }
            ul {
                li { class: "setting-row",
                    span { class: "setting-label", "Tab layout" }
                    button { class: "clear-btn", onclick: move |_| manager.set_vertical(!vertical),
                        if vertical { "Vertical (sidebar)" } else { "Horizontal (strip)" }
                    }
                }
                li { class: "setting-row",
                    span { class: "setting-label", "Close tabs left alone (pinned tabs stay)" }
                    button {
                        class: "clear-btn",
                        onclick: move |_| {
                            crate::settings::update(|s| s.close_idle_tabs_hours = next_idle_limit(s.close_idle_tabs_hours));
                            changes += 1;
                        },
                        "{idle_text}"
                    }
                }
            }
            h2 { "Profiles" }
            p { class: "setting-note",
                "Each profile has its own cookies, history, open tabs and site permissions, and opens in its own window. This window is "
                b { "{current}" }
                "."
            }
            ul {
                for name in profiles {
                    li { key: "{name}", class: "setting-row",
                        span { class: "setting-label", "{name}" }
                        if name == current {
                            span { class: "setting-value", "This window" }
                        } else if crate::profile::is_open(&name) {
                            span { class: "setting-value", "Open in another window" }
                        } else {
                            button { class: "clear-btn", onclick: move |_| open_profile(name.clone()), "Open" }
                        }
                    }
                }
                li { class: "setting-row",
                    input {
                        class: "setting-input",
                        r#type: "text",
                        placeholder: "New profile name",
                        value: "{new_profile}",
                        oninput: move |evt| new_profile.set(evt.value()),
                    }
                    button {
                        class: "clear-btn",
                        onclick: move |_| {
                            let name = new_profile.read().trim().to_string();
                            if crate::profile::list().contains(&name) {
                                message.set(Some(format!("\"{name}\" already exists.")));
                                return;
                            }
                            new_profile.set(String::new());
                            open_profile(name);
                        },
                        "Create and open"
                    }
                }
            }
            if let Some(m) = message() {
                p { class: "setting-note", "{m}" }
            }
        }
    }
}

/// The choices for closing idle tabs, in the order the button cycles through them.
const IDLE_LIMITS: [Option<u32>; 7] = [Some(1), Some(4), Some(12), Some(24), Some(72), Some(168), None];

fn next_idle_limit(current: Option<u32>) -> Option<u32> {
    let idx = IDLE_LIMITS.iter().position(|&l| l == current).map_or(0, |i| i + 1);
    IDLE_LIMITS[idx % IDLE_LIMITS.len()]
}

#[component]
fn StubPage(name: &'static str) -> Element {
    rsx! {
        document::Link { rel: "stylesheet", href: STUB_CSS }
        div { class: "about-stub",
            h1 { "{name}" }
            p { "Coming soon." }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_paths() {
        for (path, expected) in [
            ("about:newtab", AboutPage::NewTab),
            ("about:settings", AboutPage::Settings),
            ("about:history", AboutPage::History),
            ("about:bookmarks", AboutPage::Bookmarks),
        ] {
            let url = Url::parse(path).unwrap();
            assert_eq!(AboutPage::from_url(&url), Some(expected), "{path}");
        }
    }

    #[test]
    fn idle_limit_cycles_through_every_choice_and_off() {
        let mut seen = vec![];
        let mut limit = Some(12);
        for _ in 0..IDLE_LIMITS.len() {
            limit = next_idle_limit(limit);
            seen.push(limit);
        }
        assert_eq!(limit, Some(12), "back where it started");
        assert!(seen.contains(&None), "off is one of the choices");
        // A value edited by hand into settings.json restarts the cycle.
        assert_eq!(next_idle_limit(Some(5)), Some(1));
    }

    #[test]
    fn rejects_unknown() {
        assert_eq!(
            AboutPage::from_url(&Url::parse("about:nope").unwrap()),
            None
        );
        assert_eq!(
            AboutPage::from_url(&Url::parse("https://example.com").unwrap()),
            None
        );
    }

    #[test]
    fn url_roundtrip() {
        for p in [
            AboutPage::NewTab,
            AboutPage::Settings,
            AboutPage::History,
            AboutPage::Bookmarks,
        ] {
            assert_eq!(AboutPage::from_url(&p.parsed_url()), Some(p));
        }
    }
}
