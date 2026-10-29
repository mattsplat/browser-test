//! Profiles: separate people or roles, each with its own data folder. A profile's folder holds
//! its settings (per-site permissions included), history, open tabs and HTTP cache, and its
//! cookies live in its own process's memory, so nothing a site stores is shared.
//!
//! **One profile per process.** The browser runs as `browser --profile <name>` (no flag:
//! "Default"), and "open profile" starts another process. Settings, the gate, the trace and
//! AI access are process-wide globals, so a process *is* the boundary: no per-profile
//! plumbing is needed, and nothing in one profile can reach another's state. A lock file
//! keeps two processes off the same profile.
//!
//! Layout, under the app's data folder (`~/Library/Application Support/dev.mattsplat.browser-test`
//! on macOS): `profiles/<name>/` with `settings.json`, `history.sqlite3`, `session.json`,
//! `http-cache/` and `lock`. Data from before profiles existed moves into `profiles/Default`
//! on the first run (`migrate_legacy`).

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const DEFAULT: &str = "Default";

/// The running profile's name and folder. Unset in tests, so nothing there touches the user's
/// files: settings stay in memory and history opens in memory.
static CURRENT: OnceLock<(String, PathBuf)> = OnceLock::new();
/// Held open for the life of the process; the OS releases the lock when it exits.
static LOCK: OnceLock<File> = OnceLock::new();

/// Replace `path` with `bytes` all at once: write a temp file beside it (named for this
/// process, so two processes never share one), then rename it over. Readers see the old file
/// or the new one, never half of one, and a process that has the old file memory-mapped
/// keeps its copy instead of seeing it change underneath it.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!(".{name}.{}.{n}.tmp", std::process::id()));
    std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, path)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// A saved file couldn't be read: keep it as `<name>.bad` (replacing an older one) so the
/// next save doesn't destroy what's in it, and say so in the log.
pub fn keep_bad(path: &Path, error: &str) {
    let bad = path.with_extension(format!("{}.bad", path.extension().and_then(|e| e.to_str()).unwrap_or("")));
    tracing::error!("{} couldn't be read ({error}); keeping it as {} and starting fresh", path.display(), bad.display());
    if let Err(e) = std::fs::rename(path, &bad) {
        tracing::error!("couldn't move {} aside: {e}", path.display());
    }
}

/// Names become folder names, so only plain characters: no dots, slashes or leading spaces.
pub fn valid_name(name: &str) -> bool {
    let len = name.chars().count();
    (1..=32).contains(&len)
        && name.trim() == name
        && name.chars().all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
}

fn app_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("dev", "mattsplat", "browser-test")
}

/// Where every profile's folder lives.
fn profiles_root() -> Option<PathBuf> {
    app_dirs().map(|d| d.data_dir().join("profiles"))
}

/// Move the files kept before profiles existed into the Default profile's folder. Only
/// moves a file when the profile doesn't already have one, so it never overwrites, and an
/// interrupted run finishes on the next start. The history database's journal files move
/// only with their database: next to another database they'd corrupt it.
pub fn migrate_legacy(config_dir: &Path, data_dir: &Path, default_dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(default_dir)?;
    let move_if_absent = |from: &Path, name: &str| -> io::Result<()> {
        let (src, dst) = (from.join(name), default_dir.join(name));
        if src.is_file() && !dst.exists() {
            std::fs::rename(&src, &dst)?;
        }
        Ok(())
    };
    move_if_absent(config_dir, "settings.json")?;
    move_if_absent(data_dir, "history.sqlite3")?;
    // The database is in the profile and not left behind: moved now or by an earlier run.
    let db_moved = default_dir.join("history.sqlite3").is_file() && !data_dir.join("history.sqlite3").exists();
    if db_moved {
        for name in ["history.sqlite3-wal", "history.sqlite3-shm", "history.sqlite3-journal"] {
            move_if_absent(data_dir, name)?;
        }
    }
    Ok(())
}

/// Lock a profile's folder for this process: `Ok(None)` if another process holds it.
fn lock(dir: &Path) -> io::Result<Option<File>> {
    let file = File::options().create(true).truncate(false).write(true).open(dir.join("lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

/// Folder names ignore case on macOS, so "work" is the existing "Work": this returns the
/// spelling already in `root`, or `name` if there's none.
fn spelling_in(root: &Path, name: &str) -> String {
    let lower = name.to_lowercase();
    list_in(root).into_iter().find(|n| n.to_lowercase() == lower).unwrap_or_else(|| name.to_string())
}

/// Make `name` this process's profile: create its folder, lock it, and (for Default) move
/// old data into it. Call once, at startup, before anything reads settings or history.
pub fn init(name: &str) -> Result<(), String> {
    if !valid_name(name) {
        return Err(format!("\"{name}\" isn't a valid profile name (letters, digits, spaces, - and _)"));
    }
    // No app folder (some mobile targets): run with everything in memory, as before.
    let Some(dirs) = app_dirs() else { return Ok(()) };
    let root = dirs.data_dir().join("profiles");
    let name = spelling_in(&root, name);
    let dir = root.join(&name);
    std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    // A window of this profile that's just closing may hold the lock a moment longer.
    let mut attempts = 0;
    let file = loop {
        match lock(&dir) {
            Ok(Some(file)) => break file,
            Ok(None) if attempts < 5 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            Ok(None) => return Err(format!("profile \"{name}\" is already open in another window")),
            Err(e) => return Err(format!("profile \"{name}\": couldn't lock {}: {e}", dir.display())),
        }
    };
    if name == DEFAULT {
        migrate_legacy(dirs.config_dir(), dirs.data_dir(), &dir).map_err(|e| format!("couldn't move old data into {}: {e}", dir.display()))?;
    }
    let _ = LOCK.set(file);
    let _ = CURRENT.set((name, dir));
    Ok(())
}

/// The running profile's folder; None in tests.
pub fn dir() -> Option<&'static Path> {
    CURRENT.get().map(|(_, dir)| dir.as_path())
}

pub fn name() -> &'static str {
    CURRENT.get().map_or(DEFAULT, |(name, _)| name.as_str())
}

/// Profile names found in `root`, sorted, always including Default.
fn list_in(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| valid_name(n))
        .collect();
    if !names.iter().any(|n| n == DEFAULT) {
        names.push(DEFAULT.to_string());
    }
    names.sort();
    names
}

/// Every profile, and whether some process (this one included) has it open. Checking
/// takes each profile's lock for a moment, so call it once per page, not per render.
pub fn list_with_state() -> Vec<(String, bool)> {
    let Some(root) = profiles_root() else { return vec![(DEFAULT.to_string(), true)] };
    list_in(&root)
        .into_iter()
        .map(|name| {
            let open = name == self::name() || matches!(lock(&root.join(&name)), Ok(None));
            (name, open)
        })
        .collect()
}

/// Whether a profile by this name exists, ignoring case.
pub fn exists(profiles: &[(String, bool)], name: &str) -> bool {
    profiles.iter().any(|(n, _)| n.to_lowercase() == name.to_lowercase())
}

/// Create a profile's folder (or find the existing one, ignoring case), so it's listed
/// before it's first opened. Returns the name as the folder spells it.
pub fn create(name: &str) -> Result<String, String> {
    if !valid_name(name) {
        return Err(format!("\"{name}\" isn't a valid profile name"));
    }
    let root = profiles_root().ok_or("no app data folder")?;
    let name = spelling_in(&root, name);
    let dir = root.join(&name);
    std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    Ok(name)
}

/// Create the profile if needed and start it in a new window. Returns its name as spelled.
pub fn open(name: &str) -> Result<String, String> {
    let name = create(name)?;
    open_in_new_process(&name)?;
    Ok(name)
}

/// Start the browser again in another profile, as its own process.
fn open_in_new_process(name: &str) -> Result<(), String> {
    if !valid_name(name) {
        return Err(format!("\"{name}\" isn't a valid profile name"));
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut child = std::process::Command::new(exe)
        .arg("--profile")
        .arg(name)
        .spawn()
        .map_err(|e| format!("couldn't start profile \"{name}\": {e}"))?;
    // Reap it when it exits, so it doesn't linger as a zombie while this one runs.
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Split the command line into a profile name (`--profile <name>` or `--profile=<name>`) and
/// the remaining arguments.
pub fn parse_args(args: impl IntoIterator<Item = String>) -> (String, Vec<String>) {
    let mut profile = DEFAULT.to_string();
    let mut rest = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--profile" {
            if let Some(name) = args.next() {
                profile = name;
            }
        } else if let Some(name) = arg.strip_prefix("--profile=") {
            profile = name.to_string();
        } else {
            rest.push(arg);
        }
    }
    (profile, rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("browser-profile-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn names_are_plain_folder_names() {
        for good in ["Default", "Work", "side project", "a-b_c", "Ünï"] {
            assert!(valid_name(good), "{good}");
        }
        for bad in ["", "..", "a/b", "a\\b", ".hidden", " lead", "trail ", "a.b", &"x".repeat(33)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn legacy_files_move_into_default_without_overwriting() {
        let root = temp_dir("migrate");
        let (config, data) = (root.join("config"), root.join("data"));
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(config.join("settings.json"), r#"{"dev_mode":true}"#).unwrap();
        std::fs::write(data.join("history.sqlite3"), "db").unwrap();
        std::fs::write(data.join("history.sqlite3-wal"), "wal").unwrap();
        let default = data.join("profiles").join(DEFAULT);

        migrate_legacy(&config, &data, &default).unwrap();
        assert_eq!(std::fs::read_to_string(default.join("settings.json")).unwrap(), r#"{"dev_mode":true}"#);
        assert_eq!(std::fs::read_to_string(default.join("history.sqlite3")).unwrap(), "db");
        assert_eq!(std::fs::read_to_string(default.join("history.sqlite3-wal")).unwrap(), "wal");
        assert!(!config.join("settings.json").exists());
        assert!(!data.join("history.sqlite3").exists());

        // A stray old file never replaces the profile's own; running again is harmless.
        std::fs::write(config.join("settings.json"), "stale").unwrap();
        migrate_legacy(&config, &data, &default).unwrap();
        assert_eq!(std::fs::read_to_string(default.join("settings.json")).unwrap(), r#"{"dev_mode":true}"#);
        assert_eq!(std::fs::read_to_string(config.join("settings.json")).unwrap(), "stale");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn atomic_writes_replace_the_whole_file_and_leave_no_temp_files() {
        let root = temp_dir("atomic");
        let path = root.join("settings.json");
        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1, "no temp files left");
        keep_bad(&path, "test");
        assert!(!path.exists());
        assert_eq!(std::fs::read_to_string(root.join("settings.json.bad")).unwrap(), "second");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn journal_files_move_only_with_their_database() {
        let root = temp_dir("journal");
        let (config, data, default) = (root.join("config"), root.join("data"), root.join("Default"));
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&default).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        // The profile already has its own database, so the old one and its WAL both stay.
        std::fs::write(default.join("history.sqlite3"), "new").unwrap();
        std::fs::write(data.join("history.sqlite3"), "old").unwrap();
        std::fs::write(data.join("history.sqlite3-wal"), "old wal").unwrap();
        migrate_legacy(&config, &data, &default).unwrap();
        assert!(!default.join("history.sqlite3-wal").exists());
        assert!(data.join("history.sqlite3-wal").exists());

        // A run interrupted after moving the database moves its WAL next time.
        std::fs::remove_file(data.join("history.sqlite3")).unwrap();
        migrate_legacy(&config, &data, &default).unwrap();
        assert_eq!(std::fs::read_to_string(default.join("history.sqlite3-wal")).unwrap(), "old wal");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn each_profile_gets_its_own_folder_and_lock() {
        let root = temp_dir("dirs");
        for name in ["Work", "Home"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        std::fs::create_dir_all(root.join("..bad")).unwrap();
        assert_eq!(list_in(&root), ["Default", "Home", "Work"]);

        let work = lock(&root.join("Work")).unwrap();
        assert!(work.is_some());
        assert!(lock(&root.join("Work")).unwrap().is_none(), "a second process can't open the same profile");
        assert!(lock(&root.join("Home")).unwrap().is_some(), "other profiles are unaffected");
        drop(work);
        assert!(lock(&root.join("Work")).unwrap().is_some(), "the lock goes with the process");
        // Other failures are errors, not "already open".
        assert!(lock(&root.join("Missing")).is_err());

        // Names that differ only in case are the same profile.
        assert_eq!(spelling_in(&root, "work"), "Work");
        assert_eq!(spelling_in(&root, "DEFAULT"), "Default");
        assert_eq!(spelling_in(&root, "New"), "New");
        let profiles = vec![("Work".to_string(), false)];
        assert!(exists(&profiles, "WORK") && !exists(&profiles, "Home"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn profile_flag_is_taken_out_of_the_arguments() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_args(args(&["example.com"])), (DEFAULT.into(), args(&["example.com"])));
        assert_eq!(parse_args(args(&["--profile", "Work", "example.com"])), ("Work".into(), args(&["example.com"])));
        assert_eq!(parse_args(args(&["--profile=Home"])), ("Home".into(), vec![]));
    }
}
