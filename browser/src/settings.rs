//! Browser settings that persist between runs: dev mode and per-site decisions.
//! Stored as JSON in the app's config folder.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

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
    /// Origins where the JS pack may run. JS is off everywhere else (principle 1).
    pub js_sites: BTreeSet<String>,
    /// Origins where the Dart pack may not run. Dart is on everywhere else.
    pub dart_blocked: BTreeSet<String>,
    /// Refuse requests a page makes to other sites, unless allowed for that page's origin.
    pub block_third_party: bool,
    /// Other sites (registrable domains) each origin may reach.
    pub allowed_third_party: BTreeMap<String, BTreeSet<String>>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dev_mode: false,
            dev_grants: BTreeMap::new(),
            js_sites: BTreeSet::new(),
            dart_blocked: BTreeSet::new(),
            // Default-deny for third-party requests, as the design says.
            block_third_party: true,
            allowed_third_party: BTreeMap::new(),
        }
    }
}

static SETTINGS: Mutex<Option<Settings>> = Mutex::new(None);

fn path() -> Option<PathBuf> {
    directories::ProjectDirs::from("dev", "mattsplat", "browser-test").map(|d| d.config_dir().join("settings.json"))
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
