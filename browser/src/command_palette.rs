//! The command palette (Cmd+K or Cmd+Shift+P; Ctrl elsewhere): one filterable list of open
//! tabs, browser commands, recently closed tabs and history. Arrows move, Enter runs, Esc
//! closes. Matching is fuzzy (nucleo, as in the URL bar's suggestions) and ranked by
//! `rank`, a pure function so ordering is tested without the UI.
//!
//! Commands only call what the menus and panels already call (settings, AI access, the
//! tab manager), so the palette can't do anything they can't.

use blitz_traits::net::{Request, Url};
use dioxus_native::prelude::*;
use dioxus_native::NodeHandle;
use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32Str};

use crate::Panel;
use crate::about_pages::AboutPage;
use crate::browser_history::{BrowsingHistoryStoreExt, HistoryService};
use crate::history::HistoryNav;
use crate::js_permission::{self, SiteChoice};
use crate::nav::is_enter_key;
use crate::settings;
use crate::tab::{Favicon, TabId, TabStoreExt, TabStoreImplExt, active_tab, tab_display_title};
use crate::tab_manager::{SavedTab, TabManager};

/// Rows shown at most.
const MAX_ROWS: usize = 40;
/// History rows shown at most, so they don't crowd out tabs and commands.
const MAX_HISTORY: usize = 8;

#[cfg(target_os = "macos")]
const MOD: &str = "Cmd";
#[cfg(not(target_os = "macos"))]
const MOD: &str = "Ctrl";

/// What a row is; on equal scores, earlier groups rank first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    Tab,
    Command,
    Closed,
    History,
}

impl Group {
    fn label(self) -> &'static str {
        match self {
            Group::Tab => "Tab",
            Group::Command => "Command",
            Group::Closed => "Closed tab",
            Group::History => "History",
        }
    }
}

#[derive(Clone, PartialEq)]
pub enum Action {
    SwitchTab(TabId),
    NewTab,
    CloseTab,
    CloseOtherTabs,
    Reload,
    TogglePin,
    ReopenLastClosed,
    ReopenClosed(SavedTab),
    OpenPanel(Panel),
    ToggleDevMode,
    ToggleAiAccess,
    ToggleJsForSite,
    ShowAbout(AboutPage),
    OpenUrl(Url),
    SwitchSpace(String),
    NewSpace(String),
    MoveToSpace(String),
    RemoveSpace(String),
    SetVertical(bool),
    SplitWith(TabId),
    CloseSplit,
    OpenProfile(String),
    NewProfile(String),
}

#[derive(Clone, PartialEq)]
pub struct Item {
    pub action: Action,
    pub title: String,
    /// Shown dimmed after the title, and matched too (a tab's URL, a shortcut).
    pub detail: String,
    pub favicon: Option<Url>,
    pub group: Group,
}

impl Item {
    fn command(action: Action, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Item { action, title: title.into(), detail: detail.into(), favicon: None, group: Group::Command }
    }
}

/// The rows to show for `query`, as indexes into `items`. An empty query lists everything
/// but history, in the order given. Otherwise items that fuzzy-match title or detail, best
/// first; ties go to tabs, then commands, then closed tabs, then history, then list order.
pub fn rank(query: &str, items: &[Item]) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        return (0..items.len()).filter(|&i| items[i].group != Group::History).take(MAX_ROWS).collect();
    }
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            let haystack = format!("{} {}", item.title, item.detail);
            pattern.score(Utf32Str::new(&haystack, &mut buf), &mut matcher).map(|score| (score, i))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(items[a.1].group.cmp(&items[b.1].group)).then(a.1.cmp(&b.1)));
    let mut history = 0;
    scored
        .into_iter()
        .map(|(_, i)| i)
        .filter(|&i| {
            if items[i].group == Group::History {
                history += 1;
                return history <= MAX_HISTORY;
            }
            true
        })
        .take(MAX_ROWS)
        .collect()
}

/// Every row the palette can show right now, before filtering. `profiles` is each profile
/// and whether it's open, read once when the palette opens.
fn all_items(manager: TabManager, history: &HistoryService, profiles: &[(String, bool)]) -> Vec<Item> {
    let mut items = Vec::new();
    let active = (manager.active_tab_id)();
    let current_space = manager.current_space.read().clone();

    // Open tabs, every space's, except the one on screen.
    let mut open_urls = Vec::new();
    for tab in manager.tabs.iter() {
        let url = tab.nav_history().current_url().read().url.clone();
        open_urls.push(url.clone());
        if tab.tab_id() == active {
            continue;
        }
        let space = tab.space().cloned();
        let detail = if tab.pinned().cloned() || space == current_space { url.to_string() } else { format!("{space} · {url}") };
        items.push(Item {
            action: Action::SwitchTab(tab.tab_id()),
            title: tab_display_title(tab),
            detail,
            favicon: tab.favicon_url().cloned(),
            group: Group::Tab,
        });
    }

    let tab = active_tab(manager.tabs, active);
    let page = tab.nav_history().current_url().read().url.clone();
    let pinned = tab.pinned().cloned();
    let s = settings::get();
    items.push(Item::command(Action::NewTab, "New tab", format!("{MOD}+T")));
    items.push(Item::command(Action::CloseTab, "Close tab", format!("{MOD}+W")));
    items.push(Item::command(Action::Reload, "Reload", ""));
    items.push(Item::command(Action::TogglePin, if pinned { "Unpin tab" } else { "Pin tab" }, ""));
    items.push(Item::command(Action::CloseOtherTabs, "Close other tabs", "keeps pinned tabs"));
    items.push(Item::command(Action::OpenPanel(Panel::Inspect), "Open Inspect panel", "elements, console"));
    items.push(Item::command(Action::OpenPanel(Panel::Trace), "Open Trace panel", ""));
    items.push(Item::command(Action::OpenPanel(Panel::Requests), "Open Requests panel", "network"));
    items.push(Item::command(Action::OpenPanel(Panel::Site), "Open Site panel", "permissions"));
    items.push(Item::command(Action::ToggleDevMode, if s.dev_mode { "Turn dev mode off" } else { "Turn dev mode on" }, "dev mode"));
    let ai = crate::ai_access::is_on();
    items.push(Item::command(Action::ToggleAiAccess, if ai { "Turn AI access off" } else { "Turn AI access on" }, "AI access (MCP)"));
    if matches!(page.scheme(), "http" | "https" | "file") {
        // The Site panel's JavaScript cycle, one step.
        let origin = settings::origin_of(&page);
        let choice = SiteChoice::of(&s, &origin);
        let title = format!("JavaScript for this site: {} → {}", choice.label(), choice.next().label());
        items.push(Item::command(Action::ToggleJsForSite, title, format!("{origin}, reloads")));
    }
    items.push(Item::command(Action::ShowAbout(AboutPage::History), "Open history", ""));
    items.push(Item::command(Action::ShowAbout(AboutPage::Bookmarks), "Open bookmarks", ""));
    items.push(Item::command(Action::ShowAbout(AboutPage::Settings), "Open settings", "profiles, tabs"));
    let vertical = (manager.vertical)();
    items.push(Item::command(Action::SetVertical(!vertical), if vertical { "Use horizontal tabs" } else { "Use vertical tabs" }, "tab layout"));
    // Split view: the active tab beside another tab on screen in this space.
    if manager.split_shown() {
        items.push(Item::command(Action::CloseSplit, "Close split view", "split"));
    } else {
        for id in manager.visible_ids().into_iter().filter(|&id| id != active) {
            if let Some(other) = manager.tabs.iter().find(|t| t.tab_id() == id) {
                items.push(Item::command(Action::SplitWith(id), format!("Split with tab: {}", tab_display_title(other)), "split view"));
            }
        }
    }
    if !manager.closed.read().is_empty() {
        items.push(Item::command(Action::ReopenLastClosed, "Reopen closed tab", format!("{MOD}+Shift+T")));
    }

    // Spaces.
    let spaces = manager.spaces.read().clone();
    for space in spaces.iter().filter(|s| **s != current_space) {
        items.push(Item::command(Action::SwitchSpace(space.clone()), format!("Switch to space: {space}"), ""));
        if !pinned {
            items.push(Item::command(Action::MoveToSpace(space.clone()), format!("Move tab to space: {space}"), ""));
        }
    }
    if spaces.len() > 1 {
        items.push(Item::command(Action::RemoveSpace(current_space.clone()), format!("Remove space: {current_space}"), "closes its tabs"));
    }

    // Profiles open in their own window.
    for (name, open) in profiles {
        if !open {
            items.push(Item::command(Action::OpenProfile(name.clone()), format!("Open profile: {name}"), "new window"));
        }
    }

    // Recently closed, newest first.
    for closed in manager.closed.read().iter().rev() {
        items.push(Item {
            action: Action::ReopenClosed(closed.clone()),
            title: closed.title.clone(),
            detail: closed.url.to_string(),
            favicon: None,
            group: Group::Closed,
        });
    }

    // History, newest first, once per URL, leaving out pages already open.
    let entries = history.browsing().entries();
    for entry in entries.read().iter() {
        if open_urls.contains(&entry.url) {
            continue;
        }
        open_urls.push(entry.url.clone());
        let title = if entry.title.is_empty() { entry.url.to_string() } else { entry.title.clone() };
        items.push(Item {
            action: Action::OpenUrl(entry.url.clone()),
            title,
            detail: entry.url.to_string(),
            favicon: entry.favicon_url.clone(),
            group: Group::History,
        });
    }
    items
}

/// Rows that make something named after the query (a space, a profile).
fn create_items(query: &str, manager: TabManager, profiles: &[(String, bool)]) -> Vec<Item> {
    let name = query.trim();
    let mut items = Vec::new();
    if name.is_empty() {
        return items;
    }
    if !manager.spaces.read().iter().any(|s| s.to_lowercase() == name.to_lowercase()) {
        items.push(Item::command(Action::NewSpace(name.into()), format!("New space “{name}”"), ""));
    }
    if crate::profile::valid_name(name) && !crate::profile::exists(profiles, name) {
        items.push(Item::command(Action::NewProfile(name.into()), format!("New profile “{name}”"), "opens in a new window"));
    }
    items
}

fn perform(action: Action, mut manager: TabManager, mut open_panel: Signal<Panel>, open_new_tab: Callback<Url>) {
    let active = *manager.active_tab_id.peek();
    let tab = active_tab(manager.tabs, active);
    match action {
        Action::SwitchTab(id) => manager.activate(id),
        Action::NewTab => open_new_tab(AboutPage::NewTab.parsed_url()),
        Action::CloseTab => manager.close(active),
        Action::CloseOtherTabs => manager.close_others(),
        Action::Reload => tab.reload(),
        Action::TogglePin => manager.toggle_pin(active),
        Action::ReopenLastClosed => manager.reopen_closed(None),
        Action::ReopenClosed(tab) => manager.reopen_closed(Some(&tab)),
        Action::OpenPanel(panel) => open_panel.set(panel),
        Action::ToggleDevMode => settings::update(|s| s.dev_mode = !s.dev_mode),
        Action::ToggleAiAccess => {
            if crate::ai_access::is_on() {
                crate::ai_access::turn_off()
            } else {
                crate::ai_access::turn_on()
            }
        }
        Action::ToggleJsForSite => {
            js_permission::cycle_site(&settings::origin_of(&tab.nav_history().current_url().read().url));
            tab.reload();
        }
        Action::ShowAbout(page) => manager.show_about(page),
        Action::OpenUrl(url) => tab.navigate(Request::get(url)),
        Action::SwitchSpace(name) => manager.switch_space(&name),
        Action::NewSpace(name) => {
            manager.create_space(&name);
        }
        Action::MoveToSpace(name) => manager.move_to_space(active, &name),
        Action::RemoveSpace(name) => manager.remove_space(&name),
        Action::SetVertical(on) => manager.set_vertical(on),
        Action::SplitWith(id) => manager.split_with(id),
        Action::CloseSplit => manager.close_split(),
        Action::OpenProfile(name) | Action::NewProfile(name) => {
            if let Err(e) = crate::profile::open(&name) {
                tracing::error!("{e}");
            }
        }
    }
}

#[component]
pub fn CommandPalette(mut open: Signal<bool>, open_panel: Signal<Panel>, open_new_tab: Callback<Url>) -> Element {
    let manager = use_context::<TabManager>();
    let history = use_context::<HistoryService>();
    let mut query = use_signal(String::new);
    let mut selected = use_signal(|| 0usize);

    // Checking which profiles are open takes their locks, so do it once per opening.
    let profiles = use_hook(crate::profile::list_with_state);

    let items = all_items(manager, &history, &profiles);
    let q = query.read().clone();
    let mut rows: Vec<Item> = rank(&q, &items).into_iter().map(|i| items[i].clone()).collect();
    rows.extend(create_items(&q, manager, &profiles));
    let count = rows.len();
    let current = selected().min(count.saturating_sub(1));

    let run = use_callback(move |action: Action| {
        open.set(false);
        perform(action, manager, open_panel, open_new_tab);
    });
    let enter_rows = rows.clone();
    // Keyed by content, not position, so a favicon never stays on the wrong row as the list
    // changes (see url_suggestions.rs). Two tabs can look the same, so repeats are numbered.
    let mut seen = std::collections::HashMap::<String, usize>::new();
    let keys: Vec<String> = rows
        .iter()
        .map(|row| {
            let key = format!("{:?}|{}|{}", row.group, row.title, row.detail);
            let n = seen.entry(key.clone()).or_default();
            *n += 1;
            format!("{key}|{n}")
        })
        .collect();

    rsx!(
        div { class: "palette-backdrop", onpointerdown: move |_| open.set(false) }
        div { class: "palette",
            div { class: "urlbar-input-wrapper",
                input {
                    class: "urlbar-input palette-input",
                    r#type: "text",
                    placeholder: "Search tabs, commands and history",
                    value: "{q}",
                    onmounted: move |evt: Event<MountedData>| {
                        if let Some(handle) = evt.downcast::<NodeHandle>() {
                            drop(handle.set_focus(true));
                        }
                    },
                    oninput: move |evt| {
                        query.set(evt.value());
                        selected.set(0);
                    },
                    onkeydown: move |evt| {
                        match evt.key() {
                            Key::Named(NamedKey::ArrowDown) if count > 0 => {
                                evt.prevent_default();
                                selected.set((current + 1) % count);
                            }
                            Key::Named(NamedKey::ArrowUp) if count > 0 => {
                                evt.prevent_default();
                                selected.set((current + count - 1) % count);
                            }
                            Key::Named(NamedKey::Escape) => {
                                evt.prevent_default();
                                open.set(false);
                            }
                            k if is_enter_key(&k) => {
                                evt.prevent_default();
                                if let Some(row) = enter_rows.get(current) {
                                    run(row.action.clone());
                                }
                            }
                            _ => {}
                        }
                    },
                }
                if count > 0 {
                    div { class: "urlbar-suggestions palette-list",
                        for (idx, (row, key)) in rows.into_iter().zip(keys).enumerate() {
                            {
                                let action = row.action.clone();
                                rsx!(
                                    div {
                                        key: "{key}",
                                        class: if idx == current { "suggestion-row selected" } else { "suggestion-row" },
                                        onpointerdown: move |evt| {
                                            evt.prevent_default();
                                            run(action.clone());
                                        },
                                        Favicon { url: row.favicon.clone(), class: "suggestion-favicon" }
                                        span { class: "suggestion-title", "{row.title}" }
                                        span { class: "suggestion-url", "{row.detail}" }
                                        span { class: "palette-group", "{row.group.label()}" }
                                    }
                                )
                            }
                        }
                    }
                }
            }
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(group: Group, title: &str, detail: &str) -> Item {
        let action = Action::OpenUrl(Url::parse("https://x.test/").unwrap());
        Item { action, title: title.into(), detail: detail.into(), favicon: None, group }
    }

    fn titles(items: &[Item], ranked: &[usize]) -> Vec<String> {
        ranked.iter().map(|&i| items[i].title.clone()).collect()
    }

    #[test]
    fn empty_query_lists_everything_but_history_in_order() {
        let items = [
            item(Group::Tab, "Rust", "https://rust-lang.org/"),
            item(Group::Command, "New tab", ""),
            item(Group::History, "Old page", "https://old.test/"),
            item(Group::Closed, "Closed", "https://closed.test/"),
        ];
        assert_eq!(titles(&items, &rank("", &items)), ["Rust", "New tab", "Closed"]);
        assert_eq!(rank("   ", &items), rank("", &items), "blank is empty");
    }

    #[test]
    fn filters_fuzzily_on_title_and_detail() {
        let items = [
            item(Group::Command, "Open Inspect panel", "elements, console"),
            item(Group::Command, "Open Trace panel", ""),
            item(Group::Tab, "Docs", "https://docs.rs/"),
        ];
        assert_eq!(titles(&items, &rank("insp", &items)), ["Open Inspect panel"]);
        assert_eq!(titles(&items, &rank("console", &items)), ["Open Inspect panel"], "detail matches");
        assert_eq!(titles(&items, &rank("TRACE", &items)), ["Open Trace panel"], "case-insensitive");
        assert_eq!(titles(&items, &rank("opntrc", &items)), ["Open Trace panel"], "subsequence");
        assert_eq!(titles(&items, &rank("docs.rs", &items)), ["Docs"]);
        assert!(rank("zzz", &items).is_empty());
    }

    #[test]
    fn better_matches_rank_first_and_ties_go_to_tabs_then_commands() {
        let items = [
            item(Group::History, "reload docs", ""),
            item(Group::Command, "Reload", ""),
            item(Group::Command, "Turn dev mode on", "r e l o a d"),
        ];
        let ranked = titles(&items, &rank("reload", &items));
        assert_eq!(ranked[0], "Reload", "a whole-word command beats a scattered match");

        let tie = [item(Group::History, "Mail", ""), item(Group::Command, "Mail", ""), item(Group::Tab, "Mail", "")];
        let groups: Vec<Group> = rank("mail", &tie).iter().map(|&i| tie[i].group).collect();
        assert_eq!(groups, [Group::Tab, Group::Command, Group::History]);
    }

    #[test]
    fn history_is_capped_so_it_cant_crowd_out_the_rest() {
        let mut items: Vec<Item> = (0..20).map(|i| item(Group::History, &format!("page {i}"), "")).collect();
        items.push(item(Group::Command, "Open history", ""));
        let ranked = rank("p", &items);
        let history = ranked.iter().filter(|&&i| items[i].group == Group::History).count();
        assert_eq!(history, MAX_HISTORY);
        assert!(ranked.iter().any(|&i| items[i].group == Group::Command));
    }
}
