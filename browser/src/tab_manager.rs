//! Tab management over the tab store: activating and closing tabs, pinned tabs, spaces,
//! recently closed tabs, closing idle tabs, and saving the session so tabs come back after a
//! restart.
//!
//! - **Pinned** tabs come first (compact), show in every space, survive "close other tabs"
//!   and are never closed automatically.
//! - **Spaces** only organise: each tab belongs to one named space and the browser shows the
//!   current space's tabs (plus pinned ones). Everything else (cookies, settings, history)
//!   is shared; profiles (`profile.rs`) are what separate those.
//! - **Idle tabs**: an unpinned tab nobody has looked at for `close_idle_tabs_hours` (default
//!   12, `None` for never) is closed, and can be reopened from recently closed tabs.
//! - **Session**: open tabs, their spaces and pins are saved to `session.json` in the profile's
//!   folder whenever they change, and reopened at startup. Each tab comes back at its current
//!   page; its back/forward list doesn't.
//!
//! - **Split view**: two tabs side by side, a pair (`split`). It shows while either of the
//!   two is the active tab; the active one has focus and the toolbar. Not saved with the
//!   session.
//!
//! The rules are pure functions (`display_order`, `next_active_after_close`, `idle_tabs`) so
//! they're tested without a UI.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use blitz_traits::net::Url;
use dioxus_native::prelude::*;
use serde::{Deserialize, Serialize};

use crate::StdNetProvider;
use crate::about_pages::AboutPage;
use crate::history::HistoryNav;
use crate::tab::{Tab, TabId, TabStoreExt, TabStoreImplExt, open_tab, tab_display_title};

/// The space every profile starts with.
pub const FIRST_SPACE: &str = "Main";
/// How many closed tabs can be reopened (they're saved with the session, so tabs closed while
/// you were away can be found again).
const MAX_CLOSED: usize = 50;
/// How often idle tabs are looked for.
pub const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// What the rules need to know about a tab.
#[derive(Clone, Debug, PartialEq)]
pub struct TabMeta {
    pub id: TabId,
    pub pinned: bool,
    pub space: String,
    pub last_active: SystemTime,
}

/// The tabs shown for `space`, in order: pinned tabs (from every space) first, then the
/// space's own tabs, each group in the order they were opened.
pub fn display_order(tabs: &[TabMeta], space: &str) -> Vec<TabId> {
    let pinned = tabs.iter().filter(|t| t.pinned);
    let own = tabs.iter().filter(|t| !t.pinned && t.space == space);
    pinned.chain(own).map(|t| t.id).collect()
}

/// Which tab to show after closing `closing` from `order` (the tabs on screen): the one
/// that slides into its place, or the one before it if it was last. None if it was alone.
pub fn next_active_after_close(order: &[TabId], closing: TabId) -> Option<TabId> {
    let idx = order.iter().position(|&id| id == closing)?;
    let rest: Vec<TabId> = order.iter().copied().filter(|&id| id != closing).collect();
    rest.get(idx).or(rest.last()).copied()
}

/// Which tabs are on screen: the active one, and its split partner if it's in the pair.
pub fn shown_tabs(active: TabId, split: Option<(TabId, TabId)>) -> Vec<TabId> {
    match split {
        Some((a, b)) if a == active || b == active => vec![a, b],
        _ => vec![active],
    }
}

/// Tabs to close because nobody has looked at them for `limit`: unpinned, not the one on
/// screen, and last left at least `limit` ago. `limit` None turns this off.
pub fn idle_tabs(tabs: &[TabMeta], active: TabId, now: SystemTime, limit: Option<Duration>) -> Vec<TabId> {
    let Some(limit) = limit else { return Vec::new() };
    tabs.iter()
        .filter(|t| !t.pinned && t.id != active)
        .filter(|t| now.duration_since(t.last_active).is_ok_and(|idle| idle >= limit))
        .map(|t| t.id)
        .collect()
}

/// One tab in `session.json`, open or recently closed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedTab {
    pub url: String,
    pub title: String,
    pub pinned: bool,
    pub space: String,
    /// Seconds since the Unix epoch.
    pub last_active: u64,
}

/// The open tabs, as saved between runs.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub tabs: Vec<SavedTab>,
    /// Index into `tabs` of the tab on screen.
    pub active: Option<usize>,
    pub spaces: Vec<String>,
    pub current_space: String,
    /// Recently closed tabs, most recent last.
    pub closed: Vec<SavedTab>,
}

/// Read `session.json`. A file that isn't a session is kept as `session.json.bad` (so the
/// next save can't destroy it) and the browser starts empty; a tab that can't be read is
/// skipped on its own.
pub fn load_session(dir: Option<&Path>) -> Session {
    let Some(path) = dir.map(|d| d.join("session.json")) else { return Session::default() };
    let Ok(text) = std::fs::read_to_string(&path) else { return Session::default() };
    parse_session(&text).unwrap_or_else(|e| {
        crate::profile::keep_bad(&path, &e.to_string());
        Session::default()
    })
}

fn parse_session(text: &str) -> Result<Session, serde_json::Error> {
    let mut value: serde_json::Value = serde_json::from_str(text)?;
    let mut list = |key: &str| value.as_object_mut().and_then(|o| o.remove(key));
    let (tabs, closed) = (list("tabs"), list("closed"));
    let mut session: Session = serde_json::from_value(value)?;
    let each = |list: Option<serde_json::Value>| -> Vec<Option<SavedTab>> {
        match list {
            Some(serde_json::Value::Array(items)) => items.into_iter().map(|v| serde_json::from_value(v).ok()).collect(),
            _ => Vec::new(),
        }
    };
    let tabs = each(tabs);
    let skipped = tabs.iter().filter(|t| t.is_none()).count();
    if skipped > 0 {
        tracing::warn!("session.json: skipped {skipped} tab(s) that couldn't be read");
    }
    // The active index counts only the tabs that were read.
    session.active = session.active.filter(|&a| tabs.get(a).is_some_and(Option::is_some)).map(|a| tabs[..a].iter().flatten().count());
    session.tabs = tabs.into_iter().flatten().collect();
    session.closed = each(closed).into_iter().flatten().collect();
    Ok(session)
}

fn save_session(dir: &Path, json: &str) {
    let path = dir.join("session.json");
    if let Err(e) = crate::profile::write_atomic(&path, json.as_bytes()) {
        tracing::error!("could not save session to {}: {e}", path.display());
    }
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A saved time, trusted only if it's a real moment no later than `now` (a hand-edited or
/// corrupt value, or a clock that went backwards, reads as `now`).
fn from_unix_secs(secs: u64, now: SystemTime) -> SystemTime {
    UNIX_EPOCH.checked_add(Duration::from_secs(secs)).filter(|t| *t <= now).unwrap_or(now)
}

/// Tab state and the operations on it, shared through context. Everything that activates,
/// opens or closes a tab goes through here, so pins, spaces and idle times stay right.
#[derive(Clone, Copy)]
pub struct TabManager {
    pub tabs: Store<Vec<Tab>>,
    pub active_tab_id: Signal<TabId>,
    pub spaces: Signal<Vec<String>>,
    pub current_space: Signal<String>,
    /// Most recently closed last.
    pub closed: Signal<Vec<SavedTab>>,
    /// The tab layout (Settings → vertical tabs), kept here so the UI re-renders on change.
    pub vertical: Signal<bool>,
    /// Two tabs shown side by side (split view), in no particular order.
    pub split: Signal<Option<(TabId, TabId)>>,
    net_provider: CopyValue<Arc<StdNetProvider>>,
}

impl TabManager {
    /// Set up tab state, reopening the saved session. Tabs that went idle past the limit
    /// while the browser was closed go straight to recently closed, without loading.
    /// `initial_url` (from the command line) is shown in a new tab, or in an open tab already
    /// showing it.
    pub fn restore(net_provider: Arc<StdNetProvider>, initial_url: Option<Url>, home_url: Url) -> Self {
        Self::from_session(load_session(crate::profile::dir()), net_provider, initial_url, home_url)
    }

    fn from_session(session: Session, net_provider: Arc<StdNetProvider>, initial_url: Option<Url>, home_url: Url) -> Self {
        let mut spaces = session.spaces.clone();
        if spaces.is_empty() {
            spaces.push(FIRST_SPACE.to_string());
        }
        let current_space = if spaces.contains(&session.current_space) { session.current_space.clone() } else { spaces[0].clone() };
        let mut manager = TabManager {
            tabs: Store::new(Vec::new()),
            active_tab_id: Signal::new(0),
            spaces: Signal::new(spaces),
            current_space: Signal::new(current_space),
            closed: Signal::new(session.closed.clone()),
            vertical: Signal::new(crate::settings::get().vertical_tabs),
            split: Signal::new(None),
            net_provider: CopyValue::new(net_provider),
        };

        let now = SystemTime::now();
        let metas: Vec<TabMeta> = session
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| TabMeta { id: i as TabId, pinned: t.pinned, space: t.space.clone(), last_active: from_unix_secs(t.last_active, now) })
            .collect();
        // The tab that was on screen at quit counts as looked at, as it would while running.
        let stale = idle_tabs(&metas, session.active.map_or(TabId::MAX, |a| a as TabId), now, idle_limit());

        let mut active = None;
        for (i, saved) in session.tabs.iter().enumerate() {
            if stale.contains(&(i as TabId)) {
                manager.remember_closed(saved.clone());
                continue;
            }
            let Ok(url) = Url::parse(&saved.url) else { continue };
            let space = if manager.spaces.peek().contains(&saved.space) { saved.space.clone() } else { manager.current_space.peek().clone() };
            let tab = open_tab(manager.tabs, url, manager.net_provider.cloned(), space);
            *tab.title().write() = saved.title.clone();
            *tab.pinned().write() = saved.pinned;
            *tab.last_active().write() = metas[i].last_active;
            if session.active == Some(i) {
                active = Some(tab.tab_id());
            }
        }

        let already_open = initial_url.as_ref().and_then(|url| {
            manager.tabs.iter().find(|t| t.nav_history().current_url().read().url == *url).map(|t| t.tab_id())
        });
        let active = match (already_open, initial_url) {
            (Some(id), _) => Some(id),
            (None, Some(url)) => Some(manager.open_tab(url)),
            (None, None) => active.filter(|&id| manager.is_visible(id)).or_else(|| manager.visible_ids().first().copied()),
        };
        let active = active.unwrap_or_else(|| manager.open_tab(home_url));
        manager.active_tab_id.set(active);
        manager
    }

    pub fn metas(&self) -> Vec<TabMeta> {
        self.tabs
            .iter()
            .map(|t| TabMeta {
                id: t.tab_id(),
                pinned: t.pinned().cloned(),
                space: t.space().cloned(),
                last_active: t.last_active().cloned(),
            })
            .collect()
    }

    fn find(&self, id: TabId) -> Option<Store<Tab>> {
        self.tabs.iter().find(|t| t.tab_id() == id).map(Into::into)
    }

    /// The tabs on screen, in the order they're shown.
    pub fn visible_ids(&self) -> Vec<TabId> {
        display_order(&self.metas(), &self.current_space.read())
    }

    fn is_visible(&self, id: TabId) -> bool {
        self.visible_ids().contains(&id)
    }

    /// Open a tab in the current space, without showing it.
    pub fn open_tab(&self, url: Url) -> TabId {
        open_tab(self.tabs, url, self.net_provider.cloned(), self.current_space.peek().clone()).tab_id()
    }

    /// Show a tab, switching to its space if it's in another one.
    pub fn activate(&mut self, id: TabId) {
        let Some(tab) = self.find(id) else { return };
        let previous = *self.active_tab_id.peek();
        if previous != id {
            // The tab being left starts its idle time now.
            if let Some(prev) = self.find(previous) {
                *prev.last_active().write() = SystemTime::now();
            }
        }
        *tab.last_active().write() = SystemTime::now();
        if !tab.pinned().cloned() {
            let space = tab.space().cloned();
            if *self.current_space.peek() != space {
                self.current_space.set(space);
            }
        }
        self.active_tab_id.set(id);
    }

    /// Show an about page: NewTab always gets a fresh tab; other pages focus a tab already
    /// showing them if there is one, or open in a new tab.
    pub fn show_about(&mut self, page: AboutPage) {
        let existing = (page != AboutPage::NewTab)
            .then(|| self.tabs.iter().find(|t| AboutPage::from_url(&t.nav_history().current_url().read().url) == Some(page)))
            .flatten()
            .map(|t| t.tab_id());
        let id = existing.unwrap_or_else(|| self.open_tab(page.parsed_url()));
        self.activate(id);
    }

    /// Close a tab, remembering it so it can be reopened. The last tab never closes; closing
    /// the last one in a space leaves a new tab page there.
    pub fn close(&mut self, id: TabId) {
        if self.tabs.len() <= 1 {
            return;
        }
        let Some(tab) = self.find(id) else { return };
        // A split loses its pair when either side closes.
        if self.split.peek().is_some_and(|(a, b)| a == id || b == id) {
            self.split.set(None);
        }
        let closed = SavedTab {
            url: tab.nav_history().current_url().read().url.to_string(),
            title: tab_display_title(tab),
            pinned: tab.pinned().cloned(),
            space: tab.space().cloned(),
            last_active: unix_secs(SystemTime::now()),
        };
        let order = self.visible_ids();
        let next = next_active_after_close(&order, id);
        let was_active = *self.active_tab_id.peek() == id;
        if was_active {
            match next {
                Some(next) => self.activate(next),
                None => {
                    let fresh = self.open_tab(AboutPage::NewTab.parsed_url());
                    self.activate(fresh);
                }
            }
        }
        let idx = self.tabs.iter().position(|t| t.tab_id() == id);
        if let Some(idx) = idx {
            self.tabs.remove(idx);
        }
        self.remember_closed(closed);
    }

    fn remember_closed(&mut self, tab: SavedTab) {
        let mut list = self.closed.write();
        list.push(tab);
        if list.len() > MAX_CLOSED {
            list.remove(0);
        }
    }

    /// Close every unpinned tab in this space except the one on screen.
    pub fn close_others(&mut self) {
        let active = *self.active_tab_id.peek();
        let space = self.current_space.peek().clone();
        let others: Vec<TabId> =
            self.metas().into_iter().filter(|t| !t.pinned && t.space == space && t.id != active).map(|t| t.id).collect();
        for id in others {
            self.close(id);
        }
    }

    /// Close tabs left alone longer than the setting allows.
    pub fn close_idle(&mut self) {
        let limit = idle_limit();
        let active = *self.active_tab_id.peek();
        for id in idle_tabs(&self.metas(), active, SystemTime::now(), limit) {
            self.close(id);
        }
    }

    /// Reopen a closed tab (`which`, or the most recent if None) and show it, pinned again
    /// if it was pinned.
    pub fn reopen_closed(&mut self, which: Option<&SavedTab>) {
        let tab = {
            let mut closed = self.closed.write();
            let index = match which {
                Some(which) => closed.iter().rposition(|t| t == which),
                None => closed.len().checked_sub(1),
            };
            let Some(index) = index else { return };
            closed.remove(index)
        };
        let Ok(url) = Url::parse(&tab.url) else { return };
        if self.spaces.peek().contains(&tab.space) {
            self.current_space.set(tab.space);
        }
        let id = self.open_tab(url);
        if let Some(new) = self.find(id) {
            *new.pinned().write() = tab.pinned;
        }
        self.activate(id);
    }

    pub fn toggle_pin(&mut self, id: TabId) {
        if let Some(tab) = self.find(id) {
            let pinned = !tab.pinned().cloned();
            *tab.pinned().write() = pinned;
            // An unpinned tab joins the space the user is looking at.
            if !pinned {
                *tab.space().write() = self.current_space.peek().clone();
            }
        }
    }

    /// Show a space: its most recently used tab, or a new tab page if it has none.
    pub fn switch_space(&mut self, name: &str) {
        if !self.spaces.peek().iter().any(|s| s == name) {
            return;
        }
        self.current_space.set(name.to_string());
        let metas = self.metas();
        let own = metas.iter().filter(|t| !t.pinned && t.space == name);
        match own.max_by_key(|t| t.last_active).map(|t| t.id) {
            Some(id) => self.activate(id),
            None => {
                let id = self.open_tab(AboutPage::NewTab.parsed_url());
                self.activate(id);
            }
        }
    }

    /// Add a space and show it. Returns false if the name is empty or taken (ignoring case).
    pub fn create_space(&mut self, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() || self.spaces.peek().iter().any(|s| s.to_lowercase() == name.to_lowercase()) {
            return false;
        }
        self.spaces.write().push(name.to_string());
        self.switch_space(name);
        true
    }

    /// Move a tab to another space and follow it there.
    pub fn move_to_space(&mut self, id: TabId, space: &str) {
        let Some(tab) = self.find(id) else { return };
        *tab.space().write() = space.to_string();
        *tab.pinned().write() = false;
        self.activate(id);
    }

    /// Remove a space, closing its tabs. The last space can't be removed.
    pub fn remove_space(&mut self, name: &str) {
        if self.spaces.peek().len() <= 1 || !self.spaces.peek().iter().any(|s| s == name) {
            return;
        }
        let next = self.spaces.peek().iter().find(|s| *s != name).cloned().unwrap_or_default();
        if *self.current_space.peek() == name {
            self.switch_space(&next);
        }
        let doomed: Vec<TabId> = self.metas().into_iter().filter(|t| !t.pinned && t.space == name).map(|t| t.id).collect();
        for id in doomed {
            self.close(id);
        }
        self.spaces.write().retain(|s| s != name);
    }

    /// Show `other` beside the active tab (split view). The active tab keeps focus.
    pub fn split_with(&mut self, other: TabId) {
        let active = *self.active_tab_id.peek();
        if other != active && self.find(other).is_some() {
            self.split.set(Some((active, other)));
        }
    }

    /// Back to one tab: the active one.
    pub fn close_split(&mut self) {
        self.split.set(None);
    }

    /// Whether a split is on screen: the active tab is one of the pair.
    pub fn split_shown(&self) -> bool {
        shown_tabs(*self.active_tab_id.read(), *self.split.read()).len() > 1
    }

    /// Whether tab `id` is on screen (the active tab, or its split partner).
    pub fn is_shown(&self, id: TabId) -> bool {
        shown_tabs(*self.active_tab_id.read(), *self.split.read()).contains(&id)
    }

    pub fn set_vertical(&mut self, on: bool) {
        crate::settings::update(|s| s.vertical_tabs = on);
        self.vertical.set(on);
    }

    /// The session as it would be saved now. Reading it subscribes the caller to every tab.
    /// The tab on screen is saved as just looked at, so it isn't counted idle after a
    /// restart that shows another tab.
    pub fn session(&self) -> Session {
        let active = *self.active_tab_id.read();
        let now = unix_secs(SystemTime::now());
        let tabs: Vec<SavedTab> = self
            .tabs
            .iter()
            .map(|t| SavedTab {
                url: t.nav_history().current_url().read().url.to_string(),
                title: t.title().cloned(),
                pinned: t.pinned().cloned(),
                space: t.space().cloned(),
                last_active: if t.tab_id() == active { now } else { unix_secs(t.last_active().cloned()) },
            })
            .collect();
        Session {
            active: self.tabs.iter().position(|t| t.tab_id() == active),
            tabs,
            spaces: self.spaces.read().clone(),
            current_space: self.current_space.read().clone(),
            closed: self.closed.read().clone(),
        }
    }
}

/// The idle limit from settings; None when idle tabs are never closed.
fn idle_limit() -> Option<Duration> {
    crate::settings::get().close_idle_tabs_hours.map(|h| Duration::from_secs(u64::from(h) * 3600))
}

/// Keep `session.json` up to date, and close idle tabs once a minute.
pub fn use_tab_upkeep(manager: TabManager) {
    let mut last_saved = use_hook(|| CopyValue::new(String::new()));
    use_effect(move || {
        let Some(dir) = crate::profile::dir() else { return };
        let json = serde_json::to_string_pretty(&manager.session()).unwrap_or_default();
        if *last_saved.peek() != json {
            save_session(dir, &json);
            last_saved.set(json);
        }
    });
    use_future(move || async move {
        let mut manager = manager;
        loop {
            manager.close_idle();
            tokio::time::sleep(IDLE_CHECK_INTERVAL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: Duration = Duration::from_secs(3600);

    fn meta(id: TabId, pinned: bool, space: &str, idle_hours: u64, now: SystemTime) -> TabMeta {
        TabMeta { id, pinned, space: space.into(), last_active: now - HOUR * idle_hours as u32 }
    }

    #[test]
    fn pinned_tabs_come_first_and_show_in_every_space() {
        let now = SystemTime::now();
        let tabs = [meta(1, false, "Main", 0, now), meta(2, true, "Work", 0, now), meta(3, false, "Work", 0, now), meta(4, false, "Main", 0, now)];
        assert_eq!(display_order(&tabs, "Main"), [2, 1, 4]);
        assert_eq!(display_order(&tabs, "Work"), [2, 3]);
        assert_eq!(display_order(&tabs, "Empty"), [2]);
    }

    #[test]
    fn closing_shows_the_neighbour() {
        assert_eq!(next_active_after_close(&[1, 2, 3], 2), Some(3));
        assert_eq!(next_active_after_close(&[1, 2, 3], 3), Some(2));
        assert_eq!(next_active_after_close(&[1, 2, 3], 1), Some(2));
        assert_eq!(next_active_after_close(&[7], 7), None);
        assert_eq!(next_active_after_close(&[1, 2], 9), None, "a tab not on screen changes nothing");
    }

    #[test]
    fn split_view_shows_the_pair_only_while_one_of_it_is_active() {
        assert_eq!(shown_tabs(1, None), [1]);
        assert_eq!(shown_tabs(1, Some((1, 2))), [1, 2]);
        assert_eq!(shown_tabs(2, Some((1, 2))), [1, 2], "focusing the other side keeps the split");
        assert_eq!(shown_tabs(3, Some((1, 2))), [3], "another tab shows alone");
    }

    #[test]
    fn idle_tabs_close_after_the_limit_except_pinned_and_active() {
        let now = SystemTime::now();
        let tabs = [
            meta(1, false, "Main", 13, now), // idle long enough
            meta(2, false, "Main", 11, now), // not yet
            meta(3, true, "Main", 100, now), // pinned: never
            meta(4, false, "Main", 50, now), // on screen: never
            meta(5, false, "Work", 12, now), // exactly the limit, in another space
        ];
        assert_eq!(idle_tabs(&tabs, 4, now, Some(HOUR * 12)), [1, 5]);
        assert_eq!(idle_tabs(&tabs, 4, now, Some(HOUR)), [1, 2, 5]);
        assert!(idle_tabs(&tabs, 4, now, None).is_empty(), "off closes nothing");
        // A clock that went backwards (last_active in the future) isn't idle.
        let future = [TabMeta { last_active: now + HOUR, ..meta(6, false, "Main", 0, now) }];
        assert!(idle_tabs(&future, 4, now, Some(Duration::ZERO)).is_empty());
    }

    #[test]
    fn pins_and_spaces_survive_a_restart() {
        let dir = std::env::temp_dir().join(format!("browser-session-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let session = Session {
            tabs: vec![
                SavedTab { url: "https://pinned.test/".into(), title: "Pinned".into(), pinned: true, space: "Main".into(), last_active: 10 },
                SavedTab { url: "https://work.test/".into(), title: "Work".into(), pinned: false, space: "Work".into(), last_active: 20 },
            ],
            active: Some(1),
            spaces: vec!["Main".into(), "Work".into()],
            current_space: "Work".into(),
            closed: vec![SavedTab { url: "https://closed.test/".into(), ..Default::default() }],
        };
        save_session(&dir, &serde_json::to_string(&session).unwrap());
        assert_eq!(load_session(Some(&dir)), session);
        // No folder (tests) or no file: an empty session, never an error.
        assert_eq!(load_session(None), Session::default());
        std::fs::remove_file(dir.join("session.json")).unwrap();
        assert_eq!(load_session(Some(&dir)), Session::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_tab_costs_only_that_tab_and_a_bad_file_is_kept() {
        let text = r#"{"tabs":[{"url":"https://a.test/","pinned":true},{"url":7},{"url":"https://c.test/"}],
                       "active":2,"spaces":["Main"],"closed":[{"title":"x"},"junk"]}"#;
        let s = parse_session(text).unwrap();
        assert_eq!(s.tabs.len(), 2, "the second tab is skipped");
        assert!(s.tabs[0].pinned && s.tabs[0].title.is_empty(), "missing fields get defaults");
        assert_eq!(s.active, Some(1), "the active index follows the skip");
        assert_eq!(s.closed.len(), 1);
        assert_eq!(parse_session(r#"{"tabs":[{"url":7}],"active":0}"#).unwrap().active, None);

        let dir = std::env::temp_dir().join(format!("browser-session-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("session.json"), "{not json").unwrap();
        assert_eq!(load_session(Some(&dir)), Session::default());
        assert!(!dir.join("session.json").exists(), "moved aside before anything saves over it");
        assert_eq!(std::fs::read_to_string(dir.join("session.json.bad")).unwrap(), "{not json");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saved_times_from_the_future_or_out_of_range_read_as_now() {
        let now = SystemTime::now();
        assert_eq!(from_unix_secs(u64::MAX, now), now, "no panic on overflow");
        assert_eq!(from_unix_secs(unix_secs(now) + 3600, now), now);
        assert_eq!(from_unix_secs(10, now), UNIX_EPOCH + Duration::from_secs(10));
    }

    #[test]
    fn closing_reopening_pinning_and_spaces_on_the_tab_store() {
        // Dioxus swallows panics during render, so the scenario reports how it went.
        thread_local!(static PASSED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) });
        fn app() -> Element {
            let passed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(scenario)).is_ok();
            PASSED.set(Some(passed));
            VNode::empty()
        }
        fn scenario() {
            let gate = Arc::new(crate::gate::Gate::new(Arc::new(blitz_net::Provider::new(None))));
            let home = AboutPage::NewTab.parsed_url();
            let mut m = TabManager::restore(gate, None, home);
            let url = |s: &str| Url::parse(s).unwrap();
            let first = *m.active_tab_id.peek();
            assert_eq!(m.visible_ids(), [first], "no saved session: one new tab");

            let a = m.open_tab(url("https://a.test/"));
            let b = m.open_tab(url("https://b.test/"));
            m.activate(b);
            m.close(b);
            assert_eq!(*m.active_tab_id.peek(), a, "the neighbour before it takes over");
            assert_eq!(m.closed.peek().len(), 1);
            m.reopen_closed(None);
            let reopened = *m.active_tab_id.peek();
            assert_eq!(m.find(reopened).unwrap().nav_history().current_url().read().url, url("https://b.test/"));
            assert!(m.closed.peek().is_empty());

            // Split view: the pair shows while either is active; closing one side ends it.
            m.split_with(reopened);
            assert_eq!(*m.split.peek(), None, "a tab can't split with itself");
            m.split_with(a);
            assert!(m.is_shown(a) && m.is_shown(reopened));
            m.activate(a);
            assert!(m.is_shown(reopened), "focus moved to the other side");
            m.close(reopened);
            assert_eq!(*m.split.peek(), None);
            m.reopen_closed(None);

            // Pinned tabs come first and survive "close other tabs".
            m.toggle_pin(a);
            m.activate(first);
            m.close_others();
            assert_eq!(m.visible_ids(), [a, first]);

            // A new space starts with a new tab page; pinned tabs show there too.
            assert!(m.create_space("Work"));
            assert!(!m.create_space("Work"), "names are unique");
            assert_eq!(*m.current_space.peek(), "Work");
            let work_tab = *m.active_tab_id.peek();
            assert_eq!(m.visible_ids(), [a, work_tab]);
            m.switch_space(FIRST_SPACE);
            assert_eq!(*m.active_tab_id.peek(), first, "back to the space's last tab");
            // Switching to a tab in another space shows that space.
            m.activate(work_tab);
            assert_eq!(*m.current_space.peek(), "Work");

            // Removing a space closes its tabs; pinned tabs stay.
            m.remove_space("Work");
            assert_eq!(*m.spaces.peek(), [FIRST_SPACE]);
            assert_eq!(m.visible_ids(), [a, first]);
            m.remove_space(FIRST_SPACE);
            assert_eq!(*m.spaces.peek(), [FIRST_SPACE], "the last space stays");

            // The session lists every tab with its pin and space.
            let session = m.session();
            assert_eq!(session.tabs.len(), 2);
            assert!(session.tabs.iter().any(|t| t.pinned && t.url == "https://a.test/"));

            // A closed pinned tab comes back pinned; picking a closed tab by value.
            m.close(a);
            let saved = m.closed.peek().last().cloned().unwrap();
            assert!(saved.pinned);
            m.reopen_closed(Some(&saved));
            assert!(m.find(*m.active_tab_id.peek()).unwrap().pinned().cloned());

            // At startup, tabs idle past the limit (12 hours by default) go straight to
            // recently closed; pinned and the tab on screen at quit stay.
            let now = unix_secs(SystemTime::now());
            let saved = |url: &str, pinned: bool, hours: u64| SavedTab {
                url: url.into(),
                pinned,
                space: FIRST_SPACE.into(),
                last_active: now - hours * 3600,
                ..Default::default()
            };
            let session = Session {
                tabs: vec![saved("https://old.test/", false, 13), saved("https://pin.test/", true, 99), saved("https://was-on-screen.test/", false, 50), saved("https://fresh.test/", false, 1)],
                active: Some(2),
                closed: vec![saved("https://earlier.test/", false, 0)],
                ..Default::default()
            };
            let gate = Arc::new(crate::gate::Gate::new(Arc::new(blitz_net::Provider::new(None))));
            let r = TabManager::from_session(session, gate, None, AboutPage::NewTab.parsed_url());
            let urls: Vec<String> = r.session().tabs.into_iter().map(|t| t.url).collect();
            assert_eq!(urls, ["https://pin.test/", "https://was-on-screen.test/", "https://fresh.test/"]);
            let closed: Vec<String> = r.closed.peek().iter().map(|t| t.url.clone()).collect();
            assert_eq!(closed, ["https://earlier.test/", "https://old.test/"]);
        }
        VirtualDom::new(app).rebuild_in_place();
        assert_eq!(PASSED.get(), Some(true), "the scenario's assertions failed (see the panic above)");
    }
}
