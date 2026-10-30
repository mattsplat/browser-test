//! The Rust language pack. Rust is compiled ahead of time, like Dart: the page ships its
//! program (`<script type="application/rust" src="app.wasm">`), a crate built for
//! `wasm32-wasip1` on the `unscripted` crate (`packs/rust`). That program speaks the
//! interpreter packs' ABI (`host.request`, `rust_alloc`, `rust_fire`...), so it runs as an
//! `Interpreter` with the same `Host`; its entry point is `rust_main` instead of scripts.
//! Compiled code is kept per site and profile, as Dart's is. Runs by default; can be blocked
//! per site.

use std::sync::OnceLock;

use anyhow::Result;
use blitz_dom::BaseDocument;

use crate::interpreter::{Interpreter, Language};
use crate::page_document::PackDocument;

/// No engine to load: each page brings its own program, so `path` and `module` go unused.
pub static RUST: Language = Language {
    label: "Rust",
    prefix: "rust",
    path: "",
    build: "packs/rust/build.sh <your crate>",
    can_eval: false,
    module: OnceLock::new(),
};

/// Compile the page's program (or load it compiled), then run its `main!` entry point. A
/// panic or error there is reported, and the page keeps what it did before it.
pub fn start(doc: BaseDocument, env: crate::host::PageEnv, wasm: &[u8]) -> Result<PackDocument> {
    let origin = crate::settings::origin_of(doc.url());
    let module = crate::dart_pack::compiled(wasm, &origin, crate::profile::dir(), "rust")?;
    let (host, mut startup) = PackDocument::starting(doc, env, "rust");
    let mut runtime = Interpreter::load_module(&RUST, &module, Box::new(host))?;
    if let Err(e) = runtime.run_main() {
        startup.uncaught("main", String::new(), &e);
    }
    Ok(startup.finish(Box::new(runtime)))
}
