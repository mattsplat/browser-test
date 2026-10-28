//! Dev mode: compile a page's Dart source on load with the locally installed Dart SDK.
//!
//! The compile runs `packs/dart/build.sh` inside macOS `sandbox-exec` with outbound network
//! denied. Output is cached, and rebuilt only when the source is newer than the cache.
//! - `file://` pages compile in their own folder, so multi-file apps and their pubspec work.
//! - `http(s)` pages: the source is fetched through the gate and compiled as a single file in
//!   a cached workspace that depends on package:web.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use blitz_traits::net::{AbortSignal, Request, Url};

use crate::StdNetProvider;

/// Outbound network is denied; local IPC the compiler uses internally still works.
const SANDBOX_PROFILE: &str = "(version 1)(allow default)(deny network-outbound (remote ip))";

const WORKSPACE_PUBSPEC: &str = "name: dev_page\npublish_to: none\nenvironment:\n  sdk: ^3.6.0\ndependencies:\n  web: ^1.1.0\n";

fn cache_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("dev", "mattsplat", "browser-test").context("no cache folder")?;
    let dir = dirs.cache_dir().join("dart-dev");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn hash_of(value: impl Hash) -> String {
    let mut h = DefaultHasher::new();
    value.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Compile the Dart source at `src` and return the wasm.
pub async fn compile(src: &Url, net: &Arc<StdNetProvider>, signal: &AbortSignal) -> Result<Vec<u8>> {
    let (source, out) = match src.scheme() {
        "file" => {
            let source = src.to_file_path().ok().context("bad file URL")?;
            let out = cache_dir()?.join(format!("{}.wasm", hash_of(&source)));
            (source, out)
        }
        "http" | "https" => {
            let request = Request::get(src.clone()).signal(signal.clone());
            let (_, bytes) = net.fetch_async(request).await.map_err(|e| anyhow::anyhow!("fetching {src}: {e:?}"))?;
            let dir = cache_dir()?.join("http").join(hash_of(&bytes[..]));
            std::fs::create_dir_all(&dir)?;
            let source = dir.join("main.dart");
            std::fs::write(&source, &bytes)?;
            std::fs::write(dir.join("pubspec.yaml"), WORKSPACE_PUBSPEC)?;
            (source, dir.join("main.wasm"))
        }
        other => bail!("cannot compile Dart from a {other}: URL"),
    };

    if !is_fresh(&source, &out) {
        tokio::task::spawn_blocking(move || build(&source, &out).map(|_| out))
            .await
            .context("compile task failed")?
            .and_then(|out| std::fs::read(out).context("reading compiled wasm"))
    } else {
        std::fs::read(&out).context("reading cached wasm")
    }
}

/// The cache is fresh if it is newer than every .dart file and pubspec in the source folder.
fn is_fresh(source: &Path, out: &Path) -> bool {
    let Ok(built) = std::fs::metadata(out).and_then(|m| m.modified()) else { return false };
    let folder = source.parent().unwrap_or(Path::new("."));
    newest_source(folder).is_some_and(|newest| newest <= built)
}

fn newest_source(dir: &Path) -> Option<SystemTime> {
    let mut newest = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let modified = if path.is_dir() {
            // Generated folders never hold the user's source.
            if name == ".dart_tool" || name == "build" { continue }
            newest_source(&path)
        } else if path.extension().is_some_and(|e| e == "dart") || name == "pubspec.yaml" {
            entry.metadata().and_then(|m| m.modified()).ok()
        } else {
            None
        };
        newest = newest.max(modified);
    }
    newest
}

fn build(source: &Path, out: &Path) -> Result<()> {
    let folder = source.parent().context("source has no folder")?;
    // Resolve package:web offline: pages must already have their packages, and the compile
    // itself never touches the network.
    if !folder.join(".dart_tool").exists() {
        run(Command::new("dart").args(["pub", "get", "--offline"]).current_dir(folder))
            .context("`dart pub get --offline` failed; run `dart pub get` once with network")?;
    }
    let build_sh = Path::new(env!("CARGO_MANIFEST_DIR")).join("../packs/dart/build.sh");
    run(Command::new("sandbox-exec").args(["-p", SANDBOX_PROFILE, "sh"]).arg(build_sh).arg(source).arg(out))
}

fn run(command: &mut Command) -> Result<()> {
    let output = command.output().context("could not start the Dart SDK (is `dart` on PATH?)")?;
    if !output.status.success() {
        // Compile errors land here; show them as-is.
        let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        bail!("{}", text.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dev-compile-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path, offset_secs: u64) {
        std::fs::write(path, "x").unwrap();
        let time = SystemTime::now() + Duration::from_secs(offset_secs);
        std::fs::File::options().write(true).open(path).unwrap().set_modified(time).unwrap();
    }

    #[test]
    fn cache_is_stale_until_built_and_again_after_an_edit() {
        let dir = temp_dir("fresh");
        let (src, out) = (dir.join("main.dart"), dir.join("out.wasm"));
        touch(&src, 0);
        assert!(!is_fresh(&src, &out), "no output yet");
        touch(&out, 10);
        assert!(is_fresh(&src, &out));
        touch(&dir.join("helper.dart"), 20);
        assert!(!is_fresh(&src, &out), "a newer .dart file in the folder");
        // Generated folders don't count.
        std::fs::create_dir_all(dir.join(".dart_tool")).unwrap();
        touch(&out, 30);
        touch(&dir.join(".dart_tool/gen.dart"), 40);
        assert!(is_fresh(&src, &out));
    }

    /// Compiles the counter example's source for real, inside the sandbox. Needs `dart`.
    #[tokio::test]
    async fn compiles_the_counter_from_source() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/counter/main.dart").canonicalize().unwrap();
        let url = Url::from_file_path(&src).unwrap();
        let gate = Arc::new(StdNetProvider::new(Arc::new(blitz_net::Provider::new(None))));
        let signal = blitz_traits::net::AbortController::default().signal;
        let wasm = compile(&url, &gate, &signal).await.unwrap();
        assert_eq!(&wasm[..4], b"\0asm");
        // The import table is embedded, so the Dart pack can load it as-is.
        assert!(wasm.windows(17).any(|w| w == b"dart2wasm-imports"));
    }
}
