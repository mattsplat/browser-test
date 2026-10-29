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
/// interrupted run finishes on the next start.
pub fn migrate_legacy(config_dir: &Path, data_dir: &Path, default_dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(default_dir)?;
    let files = [
        (config_dir, "settings.json"),
        (data_dir, "history.sqlite3"),
        (data_dir, "history.sqlite3-wal"),
        (data_dir, "history.sqlite3-shm"),
        (data_dir, "history.sqlite3-journal"),
    ];
    for (from, name) in files {
        let (src, dst) = (from.join(name), default_dir.join(name));
        if src.is_file() && !dst.exists() {
            std::fs::rename(&src, &dst)?;
        }
    }
    Ok(())
}

/// Lock a profile's folder for this process. Fails if another process holds it.
fn lock(dir: &Path) -> io::Result<File> {
    let file = File::options().create(true).truncate(false).write(true).open(dir.join("lock"))?;
    file.try_lock().map_err(|_| io::Error::other("it's already open in another window"))?;
    Ok(file)
}

/// Make `name` this process's profile: create its folder (migrating old data into Default)
/// and lock it. Call once, at startup, before anything reads settings or history.
pub fn init(name: &str) -> Result<(), String> {
    if !valid_name(name) {
        return Err(format!("\"{name}\" isn't a valid profile name (letters, digits, spaces, - and _)"));
    }
    // No app folder (some mobile targets): run with everything in memory, as before.
    let Some(dirs) = app_dirs() else { return Ok(()) };
    let dir = dirs.data_dir().join("profiles").join(name);
    let made = if name == DEFAULT {
        migrate_legacy(dirs.config_dir(), dirs.data_dir(), &dir)
    } else {
        std::fs::create_dir_all(&dir)
    };
    made.map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
    let file = lock(&dir).map_err(|e| format!("profile \"{name}\": {e}"))?;
    let _ = LOCK.set(file);
    let _ = CURRENT.set((name.to_string(), dir));
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

pub fn list() -> Vec<String> {
    profiles_root().map(|root| list_in(&root)).unwrap_or_else(|| vec![DEFAULT.to_string()])
}

/// Whether some process (this one included) has the profile open.
pub fn is_open(name: &str) -> bool {
    if name == self::name() && dir().is_some() {
        return true;
    }
    let Some(dir) = profiles_root().map(|r| r.join(name)) else { return false };
    dir.is_dir() && lock(&dir).is_err()
}

/// Create a profile's folder, so it's listed before it's first opened.
pub fn create(name: &str) -> Result<(), String> {
    if !valid_name(name) {
        return Err(format!("\"{name}\" isn't a valid profile name"));
    }
    let dir = profiles_root().ok_or("no app data folder")?.join(name);
    std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))
}

/// Start the browser again in another profile, as its own process.
pub fn open_in_new_process(name: &str) -> Result<(), String> {
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
    fn each_profile_gets_its_own_folder_and_lock() {
        let root = temp_dir("dirs");
        for name in ["Work", "Home"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        std::fs::create_dir_all(root.join("..bad")).unwrap();
        assert_eq!(list_in(&root), ["Default", "Home", "Work"]);

        let work = lock(&root.join("Work")).unwrap();
        assert!(lock(&root.join("Work")).is_err(), "a second process can't open the same profile");
        assert!(lock(&root.join("Home")).is_ok(), "other profiles are unaffected");
        drop(work);
        assert!(lock(&root.join("Work")).is_ok(), "the lock goes with the process");
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
