use std::sync::{Arc, Mutex};

use blitz_dom::{DocumentConfig, FontContext, StyleThreading};
use blitz_html::{HtmlDocument, HtmlProvider};
use blitz_traits::{
    net::{AbortController, AbortSignal, Request, Url},
    shell::ShellProvider,
};
use dioxus_native::{SubDocumentAttr, prelude::*};
use linebender_resource_handle::Blob;

use blitz_dom::{BaseDocument, local_name};

use crate::StdNetProvider;
use crate::page_document::PackDocument;
use crate::dev_compile;
use crate::favicon::favicon_candidate;
use crate::gate::PageGate;
use crate::host::{self, TraceEntry};
use crate::prompt;
use crate::settings::{self, Grant};
use crate::history::{BrowserNavProvider, History, SyncStore};

pub enum DocumentLoaderStatus {
    Loading,
    Idle,
}

#[derive(Clone)]
pub struct LoadedDocument {
    pub document: SubDocumentAttr,
    pub html_source: String,
    pub title: String,
    // The favicon URL we'd probe for this page, computed without I/O. The
    // actual fetch+decode runs in the background after the tab applies the
    // load, so it doesn't block document swap-in. None means we couldn't even
    // form a candidate (e.g. an error page with no base URL).
    pub favicon_candidate: Option<Url>,
    // True for synthesized error/404 pages. Callers use this to gate side
    // effects that should only fire on real loads (e.g. recording history).
    pub is_error: bool,
}

pub struct DocumentLoader {
    pub font_ctx: FontContext,
    pub net_provider: Arc<StdNetProvider>,
    pub status: Signal<DocumentLoaderStatus>,
    pub history: SyncStore<History>,
    pub reload_generation: Signal<u64>,
    current_abort: Mutex<Option<AbortController>>,
}

pub fn make_doc_config(
    base_url: Option<String>,
    net_provider: Arc<StdNetProvider>,
    history: SyncStore<History>,
    font_ctx: FontContext,
    abort_signal: Option<AbortSignal>,
) -> DocumentConfig {
    // The page's own view of the gate, so its requests are checked as coming from it.
    let page = base_url.as_deref().and_then(|u| Url::parse(u).ok());
    DocumentConfig {
        viewport: None,
        base_url,
        net_provider: Some(Arc::new(PageGate { gate: net_provider, page }) as _),
        navigation_provider: Some(Arc::new(BrowserNavProvider { history })),
        shell_provider: Some(consume_context::<Arc<dyn ShellProvider>>()),
        html_parser_provider: Some(Arc::new(HtmlProvider)),
        font_ctx: Some(font_ctx),
        media_type: None,
        abort_signal,
        style_threading: StyleThreading::Parallel,
        ..Default::default()
    }
}

impl DocumentLoader {
    pub fn new(net_provider: Arc<StdNetProvider>, history: SyncStore<History>) -> Self {
        let mut font_ctx = FontContext::default();
        font_ctx
            .collection
            .register_fonts(Blob::new(Arc::new(blitz_dom::BULLET_FONT) as _), None);

        Self {
            font_ctx,
            net_provider,
            status: Signal::new(DocumentLoaderStatus::Idle),
            history,
            reload_generation: Signal::new(0),
            current_abort: Mutex::new(None),
        }
    }

    pub fn reload(&self) {
        self.abort_current();
        let mut reload_generation = self.reload_generation;
        *reload_generation.write() += 1;
    }

    pub fn reload_generation(&self) -> u64 {
        *self.reload_generation.read()
    }

    pub fn abort_current(&self) {
        if let Some(controller) = self.current_abort.lock().unwrap().take() {
            controller.abort();
        }
    }

    pub async fn load_document(&self, req: Request) -> LoadedDocument {
        let net_provider = Arc::clone(&self.net_provider);
        let font_ctx = self.font_ctx.clone();
        let history = self.history;

        let controller = AbortController::default();
        let signal = controller.signal.clone();
        {
            let mut slot = self.current_abort.lock().unwrap();
            if let Some(prev) = slot.take() {
                prev.abort();
            }
            *slot = Some(controller);
        }

        let req = req.signal(signal.clone());

        let response = net_provider.fetch_async(req).await;

        match response {
            Ok((resolved_url, bytes)) => {
                tracing::info!("Loaded {}", resolved_url);
                let base_url = resolved_url.clone();
                let config = make_doc_config(
                    Some(resolved_url),
                    Arc::clone(&net_provider),
                    history,
                    font_ctx,
                    Some(signal.clone()),
                );

                let body_text;
                let (html, is_error) = if bytes.is_empty() {
                    (include_str!("../assets/404.html"), true)
                } else {
                    body_text = String::from_utf8_lossy(&bytes);
                    (&*body_text, false)
                };

                let (document, parsed_title, favicon_url) =
                    build_page_document(html, config, &net_provider, &signal).await;
                let favicon_candidate =
                    favicon_candidate(base_url.as_str(), favicon_url.as_deref());
                LoadedDocument {
                    document,
                    html_source: html.to_string(),
                    title: parsed_title,
                    favicon_candidate,
                    is_error,
                }
            }
            Err(err) => {
                tracing::error!("Error loading document: {:?}", err);

                let error_msg = format!("{err:?}");
                let config =
                    make_doc_config(None, net_provider, history, font_ctx, Some(signal.clone()));

                let error_html = include_str!("../assets/error.html");
                let mut document = HtmlDocument::from_html(error_html, config).into_inner();
                if let Some(text_node) = document
                    .get_element_by_id("error")
                    .and_then(|el| document.get_node(el))
                    .and_then(|node| node.children.first().copied())
                {
                    document.mutate().set_node_text(text_node, &error_msg);
                }
                let parsed_title = document
                    .find_title_node()
                    .map(|n| n.text_content())
                    .unwrap_or_default();
                LoadedDocument {
                    document: SubDocumentAttr::new(document),
                    html_source: error_html.to_string(),
                    title: parsed_title,
                    favicon_candidate: None,
                    is_error: true,
                }
            }
        }
    }
}

impl Drop for DocumentLoader {
    fn drop(&mut self) {
        self.abort_current();
    }
}

/// Build the document for a loaded page, returning the document, its title, and its
/// favicon URL (if any). A `<script type="application/dart" src="app.wasm">` starts the Dart
/// pack; any other script is ignored (JS will be a pack too, off by default).
async fn build_page_document(
    html: &str,
    config: DocumentConfig,
    net_provider: &Arc<StdNetProvider>,
    signal: &AbortSignal,
) -> (SubDocumentAttr, String, Option<String>) {
    let document = HtmlDocument::from_html(html, config).into_inner();
    let title = document
        .find_title_node()
        .map(|n| n.text_content())
        .unwrap_or_default();
    let favicon_url = document.favicon_url();

    let origin = settings::origin_of(document.url());
    let page_url = document.url().clone();
    let Some(src) = dart_script_url(&document) else {
        // No Dart: the page's JS, if any, runs only where JS is allowed.
        let scripts = js_scripts(&document);
        if scripts.is_empty() {
            return (SubDocumentAttr::new(document), title, favicon_url);
        }
        if !settings::get().js_sites.contains(&origin) {
            trace_pack_load("js", document.url(), format!("skipped {} script(s): JS is off for {origin}", scripts.len()));
            return (SubDocumentAttr::new(document), title, favicon_url);
        }
        let sources = fetch_js_sources(&scripts, &page_url, net_provider, signal).await;
        return match PackDocument::start_js(document, &sources) {
            Ok(js) => (SubDocumentAttr::new(js), title, favicon_url),
            Err(e) => (error_document(&format!("JS pack failed: {e:#}")), title, favicon_url),
        };
    };
    if settings::get().dart_blocked.contains(&origin) {
        trace_load(&src, format!("skipped: Dart is blocked for {origin}"));
        return (SubDocumentAttr::new(document), title, favicon_url);
    }
    let wasm = if src.path().ends_with(".dart") {
        match dev_mode_compile(&src, &page_url, &origin, net_provider, signal).await {
            Ok(Some(wasm)) => wasm,
            Ok(None) => return (SubDocumentAttr::new(document), title, favicon_url),
            // Compile errors are for the developer: show them on the page.
            Err(e) => return (error_document(&format!("Compiling {src} failed:\n\n{e:#}")), title, favicon_url),
        }
    } else {
        match fetch_dart_program(&src, &page_url, net_provider, signal).await {
            Ok(wasm) => wasm,
            Err(e) => {
                trace_load(&src, format!("ERROR {e:#}"));
                return (SubDocumentAttr::new(document), title, favicon_url);
            }
        }
    };
    match PackDocument::start_dart(document, &wasm) {
        Ok(dart) => (SubDocumentAttr::new(dart), title, favicon_url),
        // Loading failed partway; the document it was given is gone, so show the error.
        Err(e) => (error_document(&format!("Dart pack failed: {e:#}")), title, favicon_url),
    }
}

fn trace_load(src: &Url, result: String) {
    trace_pack_load("dart", src, result);
}

fn trace_pack_load(pack: &'static str, src: &Url, result: String) {
    host::record(TraceEntry { pack, call: "load".into(), args: src.to_string(), result, micros: 0 });
}

enum JsScript {
    Inline(String),
    External(Url),
}

/// The page's classic scripts, in document order. Module scripts aren't supported yet.
fn js_scripts(document: &BaseDocument) -> Vec<JsScript> {
    let Ok(nodes) = document.query_selector_all("script") else { return Vec::new() };
    let mut scripts = Vec::new();
    for id in nodes {
        let Some(node) = document.get_node(id) else { continue };
        match node.attr(local_name!("type")).map(str::trim) {
            None | Some("") | Some("text/javascript") | Some("application/javascript") => {}
            Some("module") => {
                trace_pack_load("js", document.url(), "skipped a module script: not supported yet".into());
                continue;
            }
            _ => continue,
        }
        scripts.push(match node.attr(local_name!("src")) {
            Some(src) => match document.url().join(src) {
                Ok(url) => JsScript::External(url),
                Err(_) => continue,
            },
            None => JsScript::Inline(node.text_content()),
        });
    }
    scripts
}

/// Script sources in order. External scripts come through the gate; one that fails to load
/// is reported and skipped, as a browser would.
async fn fetch_js_sources(scripts: &[JsScript], page: &Url, net_provider: &Arc<StdNetProvider>, signal: &AbortSignal) -> Vec<(String, String)> {
    let mut sources = Vec::new();
    let mut inline = 0;
    for script in scripts {
        match script {
            JsScript::Inline(text) => {
                inline += 1;
                sources.push((format!("{page} (inline script {inline})"), text.clone()));
            }
            JsScript::External(url) => match net_provider.fetch_for_page(Request::get(url.clone()).signal(signal.clone()), page).await {
                Ok((_, bytes)) => sources.push((url.to_string(), String::from_utf8_lossy(&bytes).into_owned())),
                Err(e) => trace_pack_load("js", url, format!("ERROR could not load: {e:?}")),
            },
        }
    }
    sources
}

#[derive(Debug, PartialEq)]
enum DevDecision {
    Skip(String),
    Ask,
    Compile,
}

/// Dev mode's rules, separate from any I/O so they can be tested.
fn dev_mode_decision(settings: &settings::Settings, origin: &str) -> DevDecision {
    if !settings.dev_mode {
        return DevDecision::Skip("Dart source needs dev mode, which is off".into());
    }
    match settings.dev_grants.get(origin) {
        Some(Grant::Allow) => DevDecision::Compile,
        Some(Grant::Block) => DevDecision::Skip(format!("compiling source is blocked for {origin}")),
        None => DevDecision::Ask,
    }
}

/// Dev mode's rules for a page whose Dart script is source (`.dart`): never compile when dev
/// mode is off, respect a remembered Block, and otherwise ask once per site before compiling.
/// `Ok(None)` means the page renders without its script.
async fn dev_mode_compile(
    src: &Url,
    page: &Url,
    origin: &str,
    net_provider: &Arc<StdNetProvider>,
    signal: &AbortSignal,
) -> anyhow::Result<Option<Vec<u8>>> {
    match dev_mode_decision(&settings::get(), origin) {
        DevDecision::Skip(reason) => {
            trace_load(src, format!("skipped: {reason}"));
            return Ok(None);
        }
        DevDecision::Ask => {
            if prompt::ask(origin).await == prompt::Answer::Block {
                trace_load(src, format!("skipped: compiling source is blocked for {origin}"));
                return Ok(None);
            }
        }
        DevDecision::Compile => {}
    }
    let started = std::time::Instant::now();
    let wasm = dev_compile::compile(src, page, net_provider, signal).await?;
    trace_load(src, format!("compiled from source in {} ms", started.elapsed().as_millis()));
    Ok(Some(wasm))
}

/// The `src` of the page's Dart script, resolved against the page URL.
fn dart_script_url(document: &BaseDocument) -> Option<Url> {
    let node = document.query_selector(r#"script[type="application/dart"]"#).ok()??;
    let src = document.get_node(node)?.attr(local_name!("src"))?;
    document.url().join(src).ok()
}

/// Fetch a Dart program: one `.wasm`, with its import table embedded by
/// `packs/dart/build.sh`. It goes through the gate like any other request.
async fn fetch_dart_program(
    wasm_url: &Url,
    page: &Url,
    net_provider: &Arc<StdNetProvider>,
    signal: &AbortSignal,
) -> anyhow::Result<Vec<u8>> {
    let request = Request::get(wasm_url.clone()).signal(signal.clone());
    let (_, wasm) = net_provider.fetch_for_page(request, page).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(wasm.to_vec())
}

fn error_document(message: &str) -> SubDocumentAttr {
    let html = format!(
        "<html><body style=\"font-family: sans-serif; padding: 24px\"><h2>Page script failed</h2><pre style=\"white-space: pre-wrap\">{}</pre></body></html>",
        message.replace('&', "&amp;").replace('<', "&lt;")
    );
    SubDocumentAttr::new(HtmlDocument::from_html(&html, DocumentConfig::default()).into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use settings::Settings;

    #[test]
    fn dev_mode_off_never_compiles_or_asks() {
        let mut s = Settings::default();
        s.dev_grants.insert("file://".into(), Grant::Allow);
        assert!(matches!(dev_mode_decision(&s, "file://"), DevDecision::Skip(_)));
    }

    #[test]
    fn dev_mode_on_asks_for_unknown_sites_and_respects_grants() {
        let mut s = Settings { dev_mode: true, ..Default::default() };
        s.dev_grants.insert("https://ok.test".into(), Grant::Allow);
        s.dev_grants.insert("https://no.test".into(), Grant::Block);
        assert_eq!(dev_mode_decision(&s, "https://new.test"), DevDecision::Ask);
        assert_eq!(dev_mode_decision(&s, "https://ok.test"), DevDecision::Compile);
        assert!(matches!(dev_mode_decision(&s, "https://no.test"), DevDecision::Skip(_)));
    }

    #[test]
    fn grants_are_per_origin() {
        let url = |s: &str| Url::parse(s).unwrap();
        assert_eq!(settings::origin_of(&url("https://myapp.test/a/b.html")), "https://myapp.test");
        assert_ne!(settings::origin_of(&url("https://api.myapp.test/")), "https://myapp.test");
        assert_eq!(settings::origin_of(&url("http://localhost:8080/x")), "http://localhost:8080");
        assert_eq!(settings::origin_of(&url("file:///Users/me/page.html")), "file://");
    }
}
