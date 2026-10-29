//! The Python language pack: a Python interpreter (RustPython, built from `packs/python`)
//! compiled to WebAssembly, run as an `Interpreter` with the same `Host` as the Dart and JS
//! packs. Pages opt in with `<script type="text/python">` (inline or `src`). Like Dart it runs
//! by default and can be blocked per site.
//!
//! Nothing here is Python-specific beyond where the interpreter is: the bridge, WASI, the
//! time limit and the compiled-module cache are shared with JS (`interpreter.rs`), and the
//! Python side of the bridge is `packs/python/src/prelude.py`.

use std::sync::OnceLock;

use anyhow::Result;
use blitz_dom::BaseDocument;

use crate::interpreter::{Interpreter, Language, Script};
use crate::page_document::PackDocument;

pub static PYTHON: Language = Language {
    label: "Python",
    prefix: "py",
    path: concat!(env!("CARGO_MANIFEST_DIR"), "/../packs/python/target/wasm32-wasip1/release/python_pack.wasm"),
    build: "packs/python/build.sh",
    module: &MODULE,
};
static MODULE: OnceLock<wasmtime::Module> = OnceLock::new();

/// Run a page's Python scripts in order, in one shared namespace (like a browser's global
/// scope). An error in one is reported (in the trace) and the next still runs.
pub fn start(doc: BaseDocument, scripts: &[Script]) -> Result<PackDocument> {
    let (host, mut startup) = PackDocument::starting(doc, "python");
    let mut runtime = Interpreter::load(&PYTHON, Box::new(host))?;
    runtime.run_scripts(scripts, &mut startup);
    Ok(startup.finish(Box::new(runtime)))
}
