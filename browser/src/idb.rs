//! IndexedDB's storage, a useful subset: per profile and origin, databases with versions,
//! object stores (in-line keys with a `keyPath`, or out-of-line keys, either with
//! `autoIncrement`) and their records in key order. The host (`Obj::Idb*`) turns pages'
//! requests into these operations and their results into events.
//!
//! **Saving:** each origin's databases are one file, `<profile>/indexeddb/<sha256 of the
//! origin>.json`, replaced whole (`profile::write_atomic`) on every change, as local storage
//! is. With no profile folder (tests) it stays in memory. An origin may store `LIMIT` bytes
//! (its JSON); a change that would go over fails with `QuotaExceededError` and changes
//! nothing.
//!
//! **Values** are JSON data: strings, numbers, booleans, null, arrays and plain objects (not
//! the whole structured-clone algorithm: no `Date`, `Map`, `Blob`, typed arrays or cycles).
//! **Keys** are numbers or strings, numbers first, as IndexedDB orders them (no array, date
//! or binary keys).

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How much one origin may keep, in bytes of its saved JSON: twice local storage's limit.
pub const LIMIT: usize = 10 << 20;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Origin {
    databases: BTreeMap<String, Database>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Database {
    version: u64,
    stores: BTreeMap<String, Store>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Store {
    key_path: Option<String>,
    auto_increment: bool,
    next_key: f64,
    /// (key, value), in key order.
    records: Vec<(Value, Value)>,
}

/// IndexedDB's key order: numbers, then strings.
fn compare(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64().unwrap_or(0.0).total_cmp(&y.as_f64().unwrap_or(0.0)),
        (Value::Number(_), _) => Ordering::Less,
        (_, Value::Number(_)) => Ordering::Greater,
        _ => a.as_str().unwrap_or_default().encode_utf16().cmp(b.as_str().unwrap_or_default().encode_utf16()),
    }
}

/// `key`, if it's a valid key here (a finite number or a string).
fn valid_key(key: &Value) -> Result<Value, String> {
    match key {
        Value::Number(n) if n.as_f64().is_some_and(f64::is_finite) => Ok(key.clone()),
        Value::String(_) => Ok(key.clone()),
        _ => Err("DataError: that isn't a valid key (keys are numbers or strings here)".into()),
    }
}

/// The value at `path` (`a.b.c`) in `value`.
fn at_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(value, |v, part| v.get(part))
}

impl Origin {
    fn database(&self, db: &str) -> Result<&Database, String> {
        self.databases.get(db).ok_or_else(|| format!("NotFoundError: there's no database {db:?}"))
    }

    fn store(&self, db: &str, store: &str) -> Result<&Store, String> {
        self.database(db)?.stores.get(store).ok_or_else(|| format!("NotFoundError: there's no object store {store:?}"))
    }

    fn store_mut(&mut self, db: &str, store: &str) -> Result<&mut Store, String> {
        let database = self.databases.get_mut(db).ok_or_else(|| format!("NotFoundError: there's no database {db:?}"))?;
        database.stores.get_mut(store).ok_or_else(|| format!("NotFoundError: there's no object store {store:?}"))
    }

    /// Open database `name` at `version` (None: its current one, or 1 for a new one),
    /// creating it. Returns (its old version, 0 if new; its version now).
    pub fn open(&mut self, name: &str, version: Option<f64>) -> Result<(u64, u64), String> {
        let old = self.databases.get(name).map_or(0, |d| d.version);
        let new = match version {
            None => old.max(1),
            Some(v) if v >= 1.0 && v.fract() == 0.0 && v < 9.0e15 => v as u64,
            Some(v) => return Err(format!("TypeError: {v} isn't a valid version")),
        };
        if new < old {
            return Err(format!("VersionError: the database is at version {old}, newer than {new}"));
        }
        self.databases.entry(name.to_string()).or_default().version = new;
        Ok((old, new))
    }

    pub fn delete_database(&mut self, name: &str) {
        self.databases.remove(name);
    }

    pub fn version(&self, db: &str) -> u64 {
        self.databases.get(db).map_or(0, |d| d.version)
    }

    pub fn store_names(&self, db: &str) -> Vec<String> {
        self.databases.get(db).map(|d| d.stores.keys().cloned().collect()).unwrap_or_default()
    }

    /// A store's `keyPath` and `autoIncrement`.
    pub fn store_info(&self, db: &str, store: &str) -> Result<(Option<String>, bool), String> {
        let s = self.store(db, store)?;
        Ok((s.key_path.clone(), s.auto_increment))
    }

    pub fn create_store(&mut self, db: &str, name: &str, key_path: Option<String>, auto_increment: bool) -> Result<(), String> {
        let database = self.databases.get_mut(db).ok_or_else(|| format!("NotFoundError: there's no database {db:?}"))?;
        if database.stores.contains_key(name) {
            return Err(format!("ConstraintError: there's already an object store {name:?}"));
        }
        if key_path.as_deref() == Some("") && auto_increment {
            return Err("InvalidAccessError: an autoIncrement store can't have an empty keyPath".into());
        }
        database.stores.insert(name.to_string(), Store { key_path, auto_increment, next_key: 1.0, records: Vec::new() });
        Ok(())
    }

    pub fn delete_store(&mut self, db: &str, name: &str) -> Result<(), String> {
        self.store(db, name)?;
        self.databases.get_mut(db).map(|d| d.stores.remove(name));
        Ok(())
    }

    /// `put` (or `add`, with `no_overwrite`): store `value` under its key, the one given or
    /// the one at the store's key path, or a generated one. Returns the key.
    pub fn put(&mut self, db: &str, store: &str, mut value: Value, key: Option<Value>, no_overwrite: bool) -> Result<Value, String> {
        let s = self.store_mut(db, store)?;
        let key = match (&s.key_path, key) {
            (Some(_), Some(_)) => return Err("DataError: this store's keys are in its values (keyPath); don't pass a key".into()),
            (Some(path), None) => match at_path(&value, path) {
                Some(k) => valid_key(k)?,
                None if s.auto_increment && value.is_object() && !path.contains('.') => {
                    let k = Value::from(s.next_key);
                    value[path.as_str()] = k.clone();
                    k
                }
                None => return Err(format!("DataError: the value has no key at {path:?}")),
            },
            (None, Some(k)) => valid_key(&k)?,
            (None, None) if s.auto_increment => Value::from(s.next_key),
            (None, None) => return Err("DataError: this store needs a key (it has no keyPath or autoIncrement)".into()),
        };
        if let (true, Some(n)) = (s.auto_increment, key.as_f64()) {
            if n >= s.next_key {
                s.next_key = n.floor() + 1.0;
            }
        }
        match s.records.binary_search_by(|(k, _)| compare(k, &key)) {
            Ok(_) if no_overwrite => return Err("ConstraintError: there's already a record with that key".into()),
            Ok(i) => s.records[i].1 = value,
            Err(i) => s.records.insert(i, (key.clone(), value)),
        }
        Ok(key)
    }

    /// The value under `key`, or None.
    pub fn get(&self, db: &str, store: &str, key: &Value) -> Result<Option<Value>, String> {
        let key = valid_key(key)?;
        let s = self.store(db, store)?;
        Ok(s.records.binary_search_by(|(k, _)| compare(k, &key)).ok().map(|i| s.records[i].1.clone()))
    }

    pub fn delete(&mut self, db: &str, store: &str, key: &Value) -> Result<(), String> {
        let key = valid_key(key)?;
        let s = self.store_mut(db, store)?;
        if let Ok(i) = s.records.binary_search_by(|(k, _)| compare(k, &key)) {
            s.records.remove(i);
        }
        Ok(())
    }

    /// Every value, in key order.
    pub fn get_all(&self, db: &str, store: &str) -> Result<Vec<Value>, String> {
        Ok(self.store(db, store)?.records.iter().map(|(_, v)| v.clone()).collect())
    }

    pub fn count(&self, db: &str, store: &str) -> Result<usize, String> {
        Ok(self.store(db, store)?.records.len())
    }

    pub fn clear(&mut self, db: &str, store: &str) -> Result<(), String> {
        self.store_mut(db, store)?.records.clear();
        Ok(())
    }
}

/// The profile's IndexedDB: every origin's databases, loaded when first used.
pub struct Databases {
    origins: Mutex<HashMap<String, Origin>>,
    /// The `indexeddb` folder; None keeps everything in memory.
    dir: Option<PathBuf>,
}

pub fn local() -> &'static Databases {
    static LOCAL: OnceLock<Databases> = OnceLock::new();
    LOCAL.get_or_init(|| Databases::open(crate::profile::dir().map(|d| d.join("indexeddb"))))
}

#[derive(Deserialize)]
struct Saved {
    origin: String,
    #[serde(flatten)]
    data: Origin,
}

#[derive(Serialize)]
struct Saving<'a> {
    origin: &'a str,
    #[serde(flatten)]
    data: &'a Origin,
}

impl Databases {
    pub fn open(dir: Option<PathBuf>) -> Databases {
        Databases { origins: Mutex::new(HashMap::new()), dir }
    }

    fn file(&self, origin: &str) -> Option<PathBuf> {
        use sha2::{Digest, Sha256};
        let name: String = Sha256::digest(origin.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
        self.dir.as_ref().map(|d| d.join(format!("{name}.json")))
    }

    fn load(&self, origin: &str) -> Origin {
        let Some(path) = self.file(origin) else { return Origin::default() };
        let Ok(bytes) = std::fs::read(&path) else { return Origin::default() };
        match serde_json::from_slice::<Saved>(&bytes) {
            Ok(saved) if saved.origin == origin => saved.data,
            Ok(_) => Origin::default(),
            Err(e) => {
                crate::profile::keep_bad(&path, &e.to_string());
                Origin::default()
            }
        }
    }

    /// Run `f` on origin `origin`'s databases: a change (`write`, saved as `write` does) or
    /// a read (`f` changes nothing).
    pub fn run<T>(&self, origin: &str, write: bool, f: impl FnOnce(&mut Origin) -> Result<T, String>) -> Result<T, String> {
        if write {
            return self.write(origin, f);
        }
        let mut origins = self.origins.lock().unwrap();
        f(origins.entry(origin.to_string()).or_insert_with(|| self.load(origin)))
    }

    /// Change origin `origin`'s databases with `f`, and save them. If `f` fails, or the
    /// result is over `LIMIT`, nothing changes.
    pub fn write<T>(&self, origin: &str, f: impl FnOnce(&mut Origin) -> Result<T, String>) -> Result<T, String> {
        let mut origins = self.origins.lock().unwrap();
        let data = origins.entry(origin.to_string()).or_insert_with(|| self.load(origin));
        let mut changed = data.clone();
        let result = f(&mut changed)?;
        let saved = serde_json::to_string(&Saving { origin, data: &changed }).map_err(|e| e.to_string())?;
        if saved.len() > LIMIT {
            return Err(format!("QuotaExceededError: this would take the site's IndexedDB over {} MB", LIMIT >> 20));
        }
        *data = changed;
        if let Some(path) = self.file(origin) {
            let written = path.parent().map(std::fs::create_dir_all).transpose().and_then(|_| crate::profile::write_atomic(&path, saved.as_bytes()));
            if let Err(e) = written {
                tracing::error!("could not save IndexedDB to {}: {e}", path.display());
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stores_keep_records_in_key_order_with_their_key_rules() {
        let mut o = Origin::default();
        assert_eq!(o.open("db", Some(2.0)).unwrap(), (0, 2));
        assert!(o.open("db", Some(1.0)).unwrap_err().starts_with("VersionError"));
        assert_eq!(o.open("db", None).unwrap(), (2, 2));
        o.create_store("db", "people", Some("id".into()), true).unwrap();
        o.create_store("db", "notes", None, false).unwrap();
        assert!(o.create_store("db", "notes", None, false).unwrap_err().starts_with("ConstraintError"));

        assert_eq!(o.put("db", "people", json!({"name": "a"}), None, false).unwrap(), json!(1.0));
        assert_eq!(o.put("db", "people", json!({"id": 10, "name": "b"}), None, false).unwrap(), json!(10));
        assert_eq!(o.put("db", "people", json!({"name": "c"}), None, false).unwrap(), json!(11.0), "after the highest key");
        assert!(o.put("db", "people", json!({"id": 10}), None, true).unwrap_err().starts_with("ConstraintError"));
        assert!(o.put("db", "people", json!({"id": 1}), Some(json!(1)), false).unwrap_err().starts_with("DataError"));
        assert_eq!(o.get("db", "people", &json!(1)).unwrap(), Some(json!({"name": "a", "id": 1.0})));

        o.put("db", "notes", json!("x"), Some(json!("b")), false).unwrap();
        o.put("db", "notes", json!("y"), Some(json!(5)), false).unwrap();
        o.put("db", "notes", json!("z"), Some(json!("a")), false).unwrap();
        assert_eq!(o.get_all("db", "notes").unwrap(), [json!("y"), json!("z"), json!("x")], "numbers first, then strings");
        assert!(o.put("db", "notes", json!(1), None, false).unwrap_err().starts_with("DataError"));
        assert!(o.put("db", "notes", json!(1), Some(json!({"no": 1})), false).unwrap_err().starts_with("DataError"));
        o.delete("db", "notes", &json!(5)).unwrap();
        assert_eq!(o.count("db", "notes").unwrap(), 2);
        o.clear("db", "notes").unwrap();
        assert_eq!(o.count("db", "notes").unwrap(), 0);
        assert_eq!(o.store_names("db"), ["notes", "people"]);
    }

    /// Saved per origin, read back by a new process; over the limit, nothing changes.
    #[test]
    fn databases_are_saved_per_origin_within_the_limit() {
        let dir = std::env::temp_dir().join(format!("idb-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let dbs = Databases::open(Some(dir.clone()));
        dbs.write("https://a.test", |o| {
            o.open("db", Some(1.0))?;
            o.create_store("db", "s", None, true)?;
            o.put("db", "s", json!({"v": 1}), None, false)
        })
        .unwrap();
        let big = "x".repeat(LIMIT);
        assert!(dbs.write("https://a.test", |o| o.put("db", "s", json!(big), None, false)).unwrap_err().starts_with("QuotaExceededError"));
        let reopened = Databases::open(Some(dir.clone()));
        assert_eq!(reopened.run("https://a.test", false, |o| o.count("db", "s")), Ok(1));
        assert_eq!(reopened.run("https://b.test", false, |o| Ok(o.version("db"))), Ok(0));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
