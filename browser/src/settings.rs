//! Browser settings that persist between runs: dev mode and per-site decisions.
//! Stored as JSON in the profile's folder (`profile.rs`), so each profile has its own
//! permissions. With no profile (tests) they stay in memory.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// What happens on a site with scripts when the user hasn't chosen for it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum JsDefault {
    /// Run without JS and offer to enable it (a non-blocking bar).
    #[default]
    Ask,
    /// Run without JS, silently. Enable per site in the Site panel.
    Off,
    /// Run JS (compatibility).
    Allow,
}

impl JsDefault {
    pub fn label(self) -> &'static str {
        match self {
            JsDefault::Ask => "Ask",
            JsDefault::Off => "Off",
            JsDefault::Allow => "Allow",
        }
    }

    pub fn next(self) -> Self {
        match self {
            JsDefault::Ask => JsDefault::Off,
            JsDefault::Off => JsDefault::Allow,
            JsDefault::Allow => JsDefault::Ask,
        }
    }
}

/// A per-site answer to "compile and run Dart source?".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Grant {
    Allow,
    Block,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Global dev switch. Off: source scripts are never compiled and never prompt.
    pub dev_mode: bool,
    /// Dev-mode compile grants, by origin (e.g. "https://myapp.test", "file://").
    pub dev_grants: BTreeMap<String, Grant>,
    /// What sites with scripts get when the user hasn't chosen for them.
    pub js_default: JsDefault,
    /// Origins where JS always runs ("Always allow").
    pub js_sites: BTreeSet<String>,
    /// Origins where JS never runs and isn't offered ("Keep off").
    pub js_blocked: BTreeSet<String>,
    /// Origins where the Dart pack may not run. Dart is on everywhere else.
    pub dart_blocked: BTreeSet<String>,
    /// Refuse requests a page makes to other sites, unless allowed for that page's origin.
    pub block_third_party: bool,
    /// Other sites (registrable domains) each origin may reach.
    pub allowed_third_party: BTreeMap<String, BTreeSet<String>>,
    /// Tabs in a sidebar on the left instead of a strip along the top.
    pub vertical_tabs: bool,
    /// Close unpinned tabs left alone this many hours; None (`null`) never closes them.
    pub close_idle_tabs_hours: Option<u32>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dev_mode: false,
            dev_grants: BTreeMap::new(),
            js_default: JsDefault::Ask,
            js_sites: BTreeSet::new(),
            js_blocked: BTreeSet::new(),
            dart_blocked: BTreeSet::new(),
            // Default-deny for third-party requests, as the design says.
            block_third_party: true,
            allowed_third_party: BTreeMap::new(),
            vertical_tabs: false,
            close_idle_tabs_hours: Some(12),
        }
    }
}

static SETTINGS: Mutex<Option<Settings>> = Mutex::new(None);

fn path() -> Option<PathBuf> {
    crate::profile::dir().map(|d| d.join("settings.json"))
}

/// A copy of the current settings.
pub fn get() -> Settings {
    let mut slot = SETTINGS.lock().unwrap();
    slot.get_or_insert_with(|| {
        path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    })
    .clone()
}

/// Change settings and save them.
pub fn update(change: impl FnOnce(&mut Settings)) {
    let mut settings = get();
    change(&mut settings);
    if let Some(p) = path() {
        let saved = p.parent().map(std::fs::create_dir_all).transpose().and_then(|_| {
            std::fs::write(&p, serde_json::to_string_pretty(&settings).unwrap_or_default())
        });
        if let Err(e) = saved {
            tracing::error!("could not save settings to {}: {e}", p.display());
        }
    }
    *SETTINGS.lock().unwrap() = Some(settings);
}

/// The origin a grant is stored under: scheme + host + port. All `file://` pages share one.
pub fn origin_of(url: &blitz_traits::net::Url) -> String {
    match url.scheme() {
        "file" => "file://".into(),
        _ => url.origin().ascii_serialization(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_saved_before_new_fields_keep_their_values_and_get_defaults() {
        // A settings.json from before tab settings existed.
        let old = r#"{"dev_mode":true,"js_sites":["https://a.test"],"block_third_party":false}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert!(s.dev_mode && !s.block_third_party);
        assert!(s.js_sites.contains("https://a.test"));
        assert!(!s.vertical_tabs);
        assert_eq!(s.close_idle_tabs_hours, Some(12));
        // `null` is how "never close idle tabs" is saved.
        let off: Settings = serde_json::from_str(r#"{"close_idle_tabs_hours":null}"#).unwrap();
        assert_eq!(off.close_idle_tabs_hours, None);
        let round: Settings = serde_json::from_str(&serde_json::to_string(&off).unwrap()).unwrap();
        assert_eq!(round.close_idle_tabs_hours, None);
    }
}
