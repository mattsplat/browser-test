//! Hot reload (dev mode): when a local page's source changes on disk, reload it and put back
//! what can be put back (scroll position, form field values; the URL and its hash stay, since
//! it's a reload of the same URL).
//!
//! dart2wasm has no hot module replacement, so for every pack this is a restart plus state
//! restoration: a fresh pack instance runs the new code (Dart recompiles through
//! `dev_compile`, whose cache sees the newer source). The page's own in-memory state is lost.
//!
//! Only for `file://` pages, only with dev mode on and a dev-mode Allow grant for the page's
//! origin; otherwise nothing is watched. Files are watched by polling their modification
//! times (no watcher dependency): the page's folder, not its subfolders, for page files, and
//! a Dart project's whole tree (as `dev_compile` does) when the folder has a `pubspec.yaml`.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use blitz_dom::{BaseDocument, LocalName, NodeId, QualName, local_name, ns};
use blitz_traits::net::Url;
use dioxus_native::prelude::*;

use crate::host::{self, TraceEntry, TraceKind};
use crate::settings::{self, Grant, Settings};
use crate::tab::{TabId, TabStoreExt, TabStoreImplExt};
use crate::tab_manager::TabManager;
use crate::history::HistoryNav;

/// How often watched files are checked.
const POLL: Duration = Duration::from_millis(500);
/// Files that, when changed, reload the page.
const WATCHED: &[&str] = &["html", "htm", "js", "mjs", "py", "dart", "css", "wasm"];

/// Whether `url` is hot reloaded: a local file, dev mode on, and the site granted.
pub fn watching(settings: &Settings, url: &Url) -> bool {
    url.scheme() == "file" && settings.dev_mode && settings.dev_grants.get(&settings::origin_of(url)) == Some(&Grant::Allow)
}

/// The newest modification time among the files that make up the page at `path`.
pub fn newest_change(path: &Path) -> Option<SystemTime> {
    let folder = path.parent()?;
    let mut newest = None;
    for entry in std::fs::read_dir(folder).ok()?.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()).is_some_and(|e| WATCHED.contains(&e)) {
            newest = newest.max(entry.metadata().and_then(|m| m.modified()).ok());
        }
    }
    // A Dart project: its source can live in subfolders (lib/).
    if folder.join("pubspec.yaml").exists() {
        newest = newest.max(crate::dev_compile::newest_source(folder));
    }
    newest
}

/// What a reload keeps: the scroll position and form field values.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageState {
    pub scroll: (f64, f64),
    /// (field key, value), in document order. Text as typed; checkboxes as "true"/"false".
    pub fields: Vec<(String, String)>,
}

/// The form fields worth keeping: text inputs, textareas and checkboxes, never passwords or
/// hidden inputs.
fn fields(doc: &BaseDocument) -> Vec<(NodeId, String)> {
    let Ok(ids) = doc.query_selector_all("input, textarea") else { return Vec::new() };
    ids.into_iter()
        .filter_map(|id| {
            let el = doc.get_node(id)?.element_data()?;
            let kind = el.attr(local_name!("type")).unwrap_or("text").to_ascii_lowercase();
            if kind == "password" || kind == "hidden" {
                return None;
            }
            // Fields are matched by tag, id and name; repeats by their order.
            let key = format!("{}#{}[{}]", el.name.local, el.attr(local_name!("id")).unwrap_or(""), el.attr(local_name!("name")).unwrap_or(""));
            Some((id, key))
        })
        .collect()
}

impl PageState {
    pub fn capture(doc: &BaseDocument) -> Self {
        let scroll = doc.viewport_scroll();
        let fields = fields(doc)
            .into_iter()
            .filter_map(|(id, key)| {
                let el = doc.get_node(id)?.element_data()?;
                let value = match el.checkbox_input_checked() {
                    Some(checked) => checked.to_string(),
                    None => el.text_input_data()?.editor.text().to_string(),
                };
                Some((key, value))
            })
            .collect();
        PageState { scroll: (scroll.x, scroll.y), fields }
    }

    /// Put the state back on a freshly loaded page. A field is restored if the new page has a
    /// field with the same key at the same position among those keys.
    pub fn restore(&self, doc: &mut BaseDocument) {
        let mut saved: HashMap<&str, Vec<&str>> = HashMap::new();
        for (key, value) in &self.fields {
            saved.entry(key).or_default().push(value);
        }
        let mut seen: HashMap<String, usize> = HashMap::new();
        for (id, key) in fields(doc) {
            let n = seen.entry(key.clone()).or_default();
            let Some(value) = saved.get(key.as_str()).and_then(|v| v.get(*n)) else { continue };
            *n += 1;
            let checkbox = doc.get_node(id).and_then(|n| n.element_data()).and_then(|el| el.checkbox_input_checked()).is_some();
            if checkbox {
                if let Some(el) = doc.get_node_mut(id).and_then(|n| n.element_data_mut()) {
                    el.set_checkbox_input_checked(*value == "true");
                }
            } else {
                // Setting `value` is how Blitz fills a text field's editor.
                doc.mutate().set_attribute(id, QualName::new(None, ns!(), LocalName::from("value")), value);
            }
        }
        doc.set_viewport_scroll(blitz_dom::Point { x: self.scroll.0, y: self.scroll.1 });
        doc.shell_provider.request_redraw();
    }
}

/// The state to restore on a page that was just committed: what a hot reload saved, but only
/// if the page is the one it was saved from. The saved state is taken on every commit, so a
/// navigation elsewhere (or a slow recompile overtaken by one) drops it rather than passing
/// it on to another page.
pub fn restore_for(saved: Option<(Url, PageState)>, committed: &Url) -> Option<PageState> {
    saved.filter(|(url, _)| url == committed).map(|(_, state)| state)
}

/// Watch the pages on screen (the active tab, and its split partner) and hot reload those
/// whose files change. Tabs out of sight are checked again when they're shown.
pub fn use_hot_reload(manager: TabManager) {
    let tabs = manager.tabs;
    use_future(move || async move {
        // Per tab: the URL watched and the newest change seen for it.
        let mut seen: HashMap<TabId, (Url, SystemTime)> = HashMap::new();
        loop {
            tokio::time::sleep(POLL).await;
            let current = settings::get();
            let mut live = Vec::new();
            for tab in tabs.iter() {
                let id = tab.tab_id();
                let url = tab.nav_history().current_url().read().url.clone();
                if !manager.is_shown(id) || !watching(&current, &url) {
                    continue;
                }
                let Some(changed) = url.to_file_path().ok().and_then(|p| newest_change(&p)) else { continue };
                live.push(id);
                match seen.get(&id) {
                    // Same page, and something is newer than last time: reload.
                    Some((watched, at)) if *watched == url && changed > *at => {
                        let page = tab.page().read().clone();
                        // Replaces anything an earlier reload saved for this tab.
                        let state = page.and_then(|p| p.doc()).map(|doc| (url.clone(), PageState::capture(&doc.borrow())));
                        *tab.loader_rc().restore.borrow_mut() = state;
                        host::record(TraceEntry { pack: "dev", kind: TraceKind::Load, page: url.to_string(), call: "hot reload".into(), args: url.to_string(), result: "source changed; reloading".into(), micros: 0 });
                        tab.reload();
                    }
                    _ => {}
                }
                seen.insert(id, (url, changed));
            }
            seen.retain(|id, _| live.contains(id));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use blitz_dom::DocumentConfig;
    use blitz_html::HtmlDocument;

    #[test]
    fn only_granted_local_pages_in_dev_mode_are_watched() {
        let url = Url::parse("file:///tmp/app/index.html").unwrap();
        let mut s = Settings::default();
        s.dev_grants.insert("file://".into(), Grant::Allow);
        assert!(!watching(&s, &url), "dev mode off");
        s.dev_mode = true;
        assert!(watching(&s, &url));
        s.dev_grants.insert("file://".into(), Grant::Block);
        assert!(!watching(&s, &url), "blocked");
        s.dev_grants.remove("file://");
        assert!(!watching(&s, &url), "not asked yet");
        s.dev_grants.insert("https://app.test".into(), Grant::Allow);
        assert!(!watching(&s, &Url::parse("https://app.test/").unwrap()), "no local files to watch");
    }

    /// Saved state goes back only into the page it came from; any other page gets nothing.
    #[test]
    fn saved_state_is_restored_only_on_the_same_page() {
        let url = |s: &str| Url::parse(s).unwrap();
        let state = PageState { scroll: (0.0, 10.0), fields: vec![("input#a[]".into(), "typed".into())] };
        let saved = || Some((url("file:///app/index.html"), state.clone()));
        assert_eq!(restore_for(saved(), &url("file:///app/index.html")), Some(state.clone()));
        assert_eq!(restore_for(saved(), &url("https://other.test/")), None, "navigated elsewhere meanwhile");
        assert_eq!(restore_for(None, &url("file:///app/index.html")), None);
    }

    #[test]
    fn a_change_to_a_page_file_or_dart_source_is_seen() {
        let dir = std::env::temp_dir().join(format!("hot-reload-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        let set = |name: &str, secs: u64| {
            let path = dir.join(name);
            std::fs::write(&path, "x").unwrap();
            let t = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
            std::fs::File::options().write(true).open(&path).unwrap().set_modified(t).unwrap();
            t
        };
        let page = dir.join("index.html");
        set("index.html", 100);
        let js = set("app.js", 200);
        set("notes.txt", 900);
        assert_eq!(newest_change(&page), Some(js), "unwatched files don't count");
        // Subfolders count only in a Dart project.
        set("lib/main.dart", 300);
        assert_eq!(newest_change(&page), Some(js));
        set("pubspec.yaml", 50);
        assert_eq!(newest_change(&page), Some(SystemTime::UNIX_EPOCH + Duration::from_secs(300)));
        // Hidden folders and dependencies aren't walked.
        for skipped in [".git", "node_modules"] {
            std::fs::create_dir_all(dir.join(skipped)).unwrap();
            set(&format!("{skipped}/x.dart"), 800);
        }
        assert_eq!(newest_change(&page), Some(SystemTime::UNIX_EPOCH + Duration::from_secs(300)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn doc(html: &str) -> BaseDocument {
        HtmlDocument::from_html(html, DocumentConfig::default()).into_inner()
    }

    /// Scroll and form values survive into the reloaded page; passwords never do.
    #[test]
    fn scroll_and_form_values_are_restored() {
        let html = "<input id=a><input name=n><input name=n><textarea id=t></textarea><input type=checkbox id=c><input type=password id=p>";
        let mut before = doc(html);
        for (sel, text) in [("#a", "alpha"), ("input[name=n]", "first"), ("#t", "notes"), ("#p", "secret")] {
            let id = before.query_selector(sel).unwrap().unwrap();
            before.mutate().set_attribute(id, QualName::new(None, ns!(), LocalName::from("value")), text);
        }
        let second = before.query_selector_all("input[name=n]").unwrap()[1];
        before.mutate().set_attribute(second, QualName::new(None, ns!(), LocalName::from("value")), "second");
        let c = before.query_selector("#c").unwrap().unwrap();
        before.get_node_mut(c).unwrap().element_data_mut().unwrap().set_checkbox_input_checked(true);
        before.set_viewport_scroll(blitz_dom::Point { x: 0.0, y: 340.0 });

        let state = PageState::capture(&before);
        assert!(!state.fields.iter().any(|(_, v)| v == "secret"), "{state:?}");

        let mut after = doc(html);
        state.restore(&mut after);
        assert_eq!(PageState::capture(&after), state);
        assert_eq!(after.viewport_scroll().y, 340.0);
    }
}
