//! The JS language pack: a JS engine (Boa, built from `packs/js`) compiled to WebAssembly,
//! run as an `Interpreter` with the same `Host` as the Dart and Python packs. JS gets nothing
//! the other packs don't (principle 1), and is off unless a site is allowed.
//!
//! What's JS-only is here: ES modules, whose imports the host fetches through the gate before
//! the graph runs. The bridge, WASI and the compiled-engine cache are in `interpreter.rs`.

use std::sync::OnceLock;

use anyhow::{Result, bail};
use blitz_dom::BaseDocument;
use serde_json::Value;

use crate::interpreter::{Interpreter, Language, Script};
use crate::page_document::PackDocument;

pub static JS: Language = Language {
    label: "JS",
    prefix: "js",
    path: concat!(env!("CARGO_MANIFEST_DIR"), "/../packs/js/target/wasm32-wasip1/release/js_pack.wasm"),
    build: "packs/js/build.sh",
    module: OnceLock::new(),
};

/// Run a page's scripts with the JS pack: classic scripts in order, then module scripts in
/// order (as browsers run deferred modules). A module's imports are fetched with `fetch` (the
/// gate, for real pages) until its graph is complete. Like a browser, an error in one script
/// is reported (in the trace) and the next still runs; with pause on exceptions on, the page
/// pauses once its scripts have run.
pub async fn start<F, Fut>(doc: BaseDocument, scripts: &[Script], mut fetch: F) -> Result<PackDocument>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    let (host, mut startup) = PackDocument::starting(doc, "js");
    let mut runtime = Interpreter::load(&JS, Box::new(host))?;
    runtime.run_scripts(scripts, &mut startup);
    'modules: for module in scripts.iter().filter(|s| s.module) {
        add_module(&mut runtime, &module.name, &module.source)?;
        // Each round fetches every import the engine found missing; a graph is rarely more
        // than a few levels deep, so this bound only stops a runaway.
        for _ in 0..64 {
            let missing = match discover(&mut runtime, &module.name) {
                Ok(missing) => missing,
                Err(e) => {
                    startup.uncaught("module", module.name.clone(), &e);
                    continue 'modules;
                }
            };
            if missing.is_empty() {
                break;
            }
            for url in missing {
                // Network time isn't the pack's: keep it out of the profiler's `start`.
                let fetching = std::time::Instant::now();
                let fetched = fetch(url.clone()).await;
                startup.not_running(fetching.elapsed());
                match fetched {
                    Ok(source) => add_module(&mut runtime, &url, &source)?,
                    Err(e) => {
                        // A failed fetch isn't the page's script failing, so no snapshot.
                        startup.report("import", url, &e);
                        continue 'modules;
                    }
                }
            }
        }
        if let Err(e) = run_module(&mut runtime, &module.name) {
            startup.uncaught("module", module.name.clone(), &e);
        }
    }
    Ok(startup.finish(Box::new(runtime)))
}

/// Give the engine module `url`'s source.
fn add_module(js: &mut Interpreter, url: &str, source: &str) -> Result<()> {
    js.enter()?;
    let (u, ul) = js.write_str(url)?;
    let (s, sl) = js.write_str(source)?;
    js.call::<(u32, u32, u32, u32), ()>("add_module", (u, ul, s, sl))
}

/// The imports of module graph `url` that aren't loaded yet (empty when complete).
fn discover(js: &mut Interpreter, url: &str) -> Result<Vec<String>> {
    js.enter()?;
    let (u, ul) = js.write_str(url)?;
    let status = js.call::<(u32, u32), u64>("discover", (u, ul))?;
    if status == 0 {
        return Ok(Vec::new());
    }
    let reply: Value = serde_json::from_str(&js.take_str(status)?)?;
    if let Some(error) = reply.get("error").and_then(|e| e.as_str()) {
        bail!("{error}");
    }
    Ok(reply["missing"].as_array().into_iter().flatten().filter_map(|m| m.as_str().map(String::from)).collect())
}

/// Link and evaluate module graph `url` (once `discover` reports it complete).
fn run_module(js: &mut Interpreter, url: &str) -> Result<()> {
    js.enter()?;
    let (u, ul) = js.write_str(url)?;
    let status = js.call::<(u32, u32), u64>("run_module", (u, ul))?;
    js.check(status)
}
