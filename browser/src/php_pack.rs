//! The PHP language pack: php-src's embed SAPI (built from `packs/php`) compiled to
//! WebAssembly, run as an `Interpreter` with the same `Host` as the other packs. Pages opt in
//! with `<script type="text/php">` (inline or `src`). Like Dart and Python it runs by default
//! and can be blocked per site.
//!
//! As with Python, nothing here is PHP-specific beyond where the interpreter is; the PHP side
//! of the bridge is `packs/php/src/prelude.php`.

use std::sync::OnceLock;

use anyhow::Result;
use blitz_dom::BaseDocument;

use crate::interpreter::{Interpreter, Language, Script};
use crate::page_document::PackDocument;

pub static PHP: Language = Language {
    label: "PHP",
    prefix: "php",
    path: concat!(env!("CARGO_MANIFEST_DIR"), "/../packs/php/target/php_pack.wasm"),
    build: "packs/php/build.sh",
    module: OnceLock::new(),
};

/// Run a page's PHP scripts in order, sharing one global scope. An error in one is reported
/// (in the trace) and the next still runs.
pub fn start(doc: BaseDocument, env: crate::host::PageEnv, scripts: &[Script]) -> Result<PackDocument> {
    let (host, mut startup) = PackDocument::starting(doc, env, "php");
    let mut runtime = Interpreter::load(&PHP, Box::new(host))?;
    runtime.run_scripts(scripts, &mut startup);
    Ok(startup.finish(Box::new(runtime)))
}
