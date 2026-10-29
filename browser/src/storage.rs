//! Web storage: `localStorage` (per profile and origin, saved in the profile's folder) and
//! `sessionStorage` (per tab and origin, in memory). Both are the same `Area` of string keys
//! and values; the host (`Obj::Storage`) gives pages `getItem`, `setItem`, `removeItem`,
//! `clear`, `key` and `length`. All `file://` pages share one origin, as for every other
//! per-origin setting.
//!
//! **Saving:** each origin's local storage is one file, `<profile>/local-storage/<sha256 of the
//! origin>.json`, replaced whole (`profile::write_atomic`) on every change. With no profile
//! folder (tests) it stays in memory.
//!
//! **Privacy:** stored values never reach AI access (`debug::holds_secrets` hides `Storage.*`
//! calls in redacted views), and the Site panel shows only a size.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

/// How much one origin may store, in UTF-16 code units of keys plus values (about 5 MB, the
/// usual browser limit, counted as browsers count it: string length).
pub const LIMIT: usize = 5 * 1024 * 1024;

/// One origin's storage. Keys are kept sorted, so `key(n)` is stable.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Area {
    items: BTreeMap<String, String>,
}

fn units(s: &str) -> usize {
    s.encode_utf16().count()
}

impl Area {
    pub fn get(&self, key: &str) -> Option<String> {
        self.items.get(key).cloned()
    }

    /// Store `value` under `key`, unless that takes the origin over `LIMIT`.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        let replaced = self.items.get(key).map(|old| units(key) + units(old)).unwrap_or(0);
        if self.size() - replaced + units(key) + units(value) > LIMIT {
            return Err(format!("QuotaExceededError: this would take the site's storage over {} MB", LIMIT >> 20));
        }
        self.items.insert(key.to_string(), value.to_string());
        Ok(())
    }

    pub fn remove(&mut self, key: &str) {
        self.items.remove(key);
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    pub fn key(&self, index: usize) -> Option<String> {
        self.items.keys().nth(index).cloned()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// UTF-16 code units of keys and values, what `LIMIT` counts.
    pub fn size(&self) -> usize {
        self.items.iter().map(|(k, v)| units(k) + units(v)).sum()
    }
}

/// A tab's session storage, by origin. The tab's loader owns it; its pages share it.
pub type Session = Rc<RefCell<HashMap<String, Area>>>;

/// The profile's local storage: every origin's area, loaded when first used.
pub struct Local {
    areas: Mutex<HashMap<String, Area>>,
    /// The `local-storage` folder; None keeps everything in memory.
    dir: Option<PathBuf>,
}

/// The profile's local storage.
pub fn local() -> &'static Local {
    static LOCAL: OnceLock<Local> = OnceLock::new();
    LOCAL.get_or_init(|| Local::open(crate::profile::dir().map(|d| d.join("local-storage"))))
}

#[derive(Serialize, Deserialize)]
struct Saved {
    origin: String,
    items: BTreeMap<String, String>,
}

impl Local {
    pub fn open(dir: Option<PathBuf>) -> Local {
        Local { areas: Mutex::new(HashMap::new()), dir }
    }

    fn file(&self, origin: &str) -> Option<PathBuf> {
        use sha2::{Digest, Sha256};
        let name: String = Sha256::digest(origin.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
        self.dir.as_ref().map(|d| d.join(format!("{name}.json")))
    }

    fn load(&self, origin: &str) -> Area {
        let Some(path) = self.file(origin) else { return Area::default() };
        let Ok(bytes) = std::fs::read(&path) else { return Area::default() };
        match serde_json::from_slice::<Saved>(&bytes) {
            Ok(saved) if saved.origin == origin => Area { items: saved.items },
            Ok(_) => Area::default(),
            Err(e) => {
                crate::profile::keep_bad(&path, &e.to_string());
                Area::default()
            }
        }
    }

    /// Read origin `origin`'s area.
    pub fn read<T>(&self, origin: &str, f: impl FnOnce(&Area) -> T) -> T {
        let mut areas = self.areas.lock().unwrap();
        let area = areas.entry(origin.to_string()).or_insert_with(|| self.load(origin));
        f(area)
    }

    /// Change origin `origin`'s area, and save it if `f` succeeds.
    pub fn write<T>(&self, origin: &str, f: impl FnOnce(&mut Area) -> Result<T, String>) -> Result<T, String> {
        let mut areas = self.areas.lock().unwrap();
        let area = areas.entry(origin.to_string()).or_insert_with(|| self.load(origin));
        let result = f(area)?;
        if let Some(path) = self.file(origin) {
            let saved = Saved { origin: origin.to_string(), items: area.items.clone() };
            let written = path.parent().map(std::fs::create_dir_all).transpose().and_then(|_| {
                crate::profile::write_atomic(&path, serde_json::to_string(&saved).unwrap_or_default().as_bytes())
            });
            if let Err(e) = written {
                tracing::error!("could not save local storage to {}: {e}", path.display());
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_area_keeps_strings_in_key_order_within_the_limit() {
        let mut area = Area::default();
        area.set("b", "2").unwrap();
        area.set("a", "1").unwrap();
        assert_eq!((area.len(), area.key(0).as_deref(), area.key(5)), (2, Some("a"), None));
        area.set("a", "one").unwrap();
        assert_eq!(area.get("a").as_deref(), Some("one"));
        assert_eq!(area.size(), 1 + 3 + 1 + 1);
        let big = "x".repeat(LIMIT);
        assert!(area.set("big", &big).unwrap_err().starts_with("QuotaExceededError"));
        assert_eq!(area.len(), 2, "a refused value isn't stored");
        area.remove("a");
        area.clear();
        assert_eq!(area.len(), 0);
    }

    /// Local storage is saved per origin and read back by a new process; one origin never
    /// sees another's.
    #[test]
    fn local_storage_is_saved_per_origin() {
        let dir = std::env::temp_dir().join(format!("local-storage-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let local = Local::open(Some(dir.clone()));
        local.write("https://a.test", |a| a.set("k", "v")).unwrap();
        local.write("https://b.test", |a| a.set("k", "other")).unwrap();
        let reopened = Local::open(Some(dir.clone()));
        assert_eq!(reopened.read("https://a.test", |a| a.get("k")).as_deref(), Some("v"));
        assert_eq!(reopened.read("https://b.test", |a| a.get("k")).as_deref(), Some("other"));
        assert_eq!(reopened.read("https://c.test", |a| a.len()), 0);
        reopened.write("https://a.test", |a| Ok(a.clear())).unwrap();
        assert_eq!(Local::open(Some(dir.clone())).read("https://a.test", |a| a.len()), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
