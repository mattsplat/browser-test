use std::cell::{Cell, RefCell};
use std::rc::Rc;
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
use crate::interpreter::Script;
use crate::page_document::{PackDocument, PageRef};
use crate::dev_compile;
use crate::favicon::favicon_candidate;
use crate::gate::PageGate;
use crate::host::{self, TraceEntry, TraceKind};
use crate::{dart_pack, js_pack, python_pack};
use crate::js_permission;
use crate::prompt;
use crate::replay;
use crate::settings::{self, Grant};
use crate::history::{BrowserNavProvider, History, SyncStore};

pub enum DocumentLoaderStatus {
    Loading,
    Idle,
}

#[derive(Clone)]
pub struct LoadedDocument {
    pub document: SubDocumentAttr,
    /// The page's DOM, for AI access to read; None for the browser's own error pages.
    pub page: Option<PageRef>,
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
    /// Record this tab's next page (Inspector → Recording). Taken by the next load, whether or
    /// not it runs a pack, so it never records a later visit.
    pub record_next: Cell<bool>,
    /// Scroll and form values a hot reload saved, with the URL they belong to; taken by the
    /// tab's next commit (`hot_reload::restore_for`).
    pub restore: RefCell<Option<(Url, crate::hot_reload::PageState)>>,
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
            record_next: Cell::new(false),
            restore: RefCell::new(None),
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

                let record = self.record_next.replace(false);
                let ((document, page), parsed_title, favicon_url) =
                    build_page_document(html, config, &net_provider, &signal, record).await;
                // Armed only for this load: drop it if no pack started to take it.
                if let (true, Ok(url)) = (record, Url::parse(&base_url)) {
                    replay::take_armed(url.as_str());
                }
                let favicon_candidate =
                    favicon_candidate(base_url.as_str(), favicon_url.as_deref());
                LoadedDocument {
                    document,
                    page,
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
                    page: None,
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
/// favicon URL (if any). A page runs one pack, the first of: Dart (`<script
/// type="application/dart" src="app.wasm">`), Python (`<script type="text/python">`), JS
/// (plain `<script>`, only where JS is allowed). Any other script is ignored.
async fn build_page_document(
    html: &str,
    config: DocumentConfig,
    net_provider: &Arc<StdNetProvider>,
    signal: &AbortSignal,
    recording: bool,
) -> ((SubDocumentAttr, Option<PageRef>), String, Option<String>) {
    let mut document = HtmlDocument::from_html(html, config).into_inner();
    // The site's own stylesheet, if the user gave it one: after the page's, before any script.
    let origin = settings::origin_of(document.url());
    let site_css = crate::site_css::for_origin(&settings::get(), &origin).map(str::to_string);
    crate::site_css::apply(&mut document, site_css.as_deref());
    // Being recorded (Inspector → Recording): armed just before the pack starts (which takes
    // it), and then given what the pack loaded, to replay it.
    let arm = |url: &Url| {
        if recording {
            replay::arm(url.as_str());
        }
    };
    let record = |page: &PackDocument, program: replay::Program| {
        if recording {
            page.attach_recording(html, site_css.as_deref(), program);
        }
    };
    let title = document
        .find_title_node()
        .map(|n| n.text_content())
        .unwrap_or_default();
    let favicon_url = document.favicon_url();

    let page_url = document.url().clone();
    // What the page's code reaches beyond its document: the network, through its gate.
    let env = crate::host::PageEnv { gate: Some(Arc::clone(net_provider)) };
    let Some(src) = dart_script_url(&document) else {
        // Python, like Dart, runs unless blocked for the site.
        let scripts = page_scripts(&document, "python");
        if !scripts.is_empty() {
            trace_skipped(&document, &page_url, "Python", &["js"]);
            if settings::get().python_blocked.contains(&origin) {
                trace_pack_load("python", &page_url, &page_url, format!("skipped {} script(s): Python is blocked for {origin}", scripts.len()));
                return (shown(document), title, favicon_url);
            }
            let sources = fetch_sources("python", &scripts, &page_url, net_provider, signal).await;
            arm(&page_url);
            return match python_pack::start(document, env, &sources) {
                Ok(python) => {
                    record(&python, replay::Program::Scripts { pack: "python", scripts: sources, imports: Vec::new() });
                    (shown_pack(python), title, favicon_url)
                }
                Err(e) => {
                    trace_pack_load("python", &page_url, &page_url, format!("ERROR {e:#}"));
                    ((error_document(&format!("Python pack failed: {e:#}")), None), title, favicon_url)
                }
            };
        }
        // No Dart or Python: the page's JS, if any, runs only where JS is allowed.
        let scripts = page_scripts(&document, "js");
        if scripts.is_empty() {
            return (shown(document), title, favicon_url);
        }
        match js_permission::decide(&settings::get(), js_permission::allowed_this_visit(&origin), &origin) {
            js_permission::JsDecision::Run => {}
            js_permission::JsDecision::Skip { reason, offer } => {
                trace_pack_load("js", &page_url, document.url(), format!("skipped {} script(s): {reason}", scripts.len()));
                if offer {
                    js_permission::offer(&origin, scripts.len());
                }
                return (shown(document), title, favicon_url);
            }
        }
        let sources = fetch_sources("js", &scripts, &page_url, net_provider, signal).await;
        // Module imports come through the gate too, as requests from this page.
        let imports: Rc<RefCell<Vec<(String, String)>>> = Default::default();
        let fetch_import = |url: String| {
            let (net, page, signal, imports) = (Arc::clone(net_provider), page_url.clone(), signal.clone(), imports.clone());
            async move {
                let parsed = Url::parse(&url)?;
                let (_, bytes) = net.fetch_for_page(Request::get(parsed).signal(signal), &page).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
                let source = String::from_utf8_lossy(&bytes).into_owned();
                if recording {
                    imports.borrow_mut().push((url, source.clone()));
                }
                Ok(source)
            }
        };
        arm(&page_url);
        return match js_pack::start(document, env, &sources, fetch_import).await {
            Ok(js) => {
                record(&js, replay::Program::Scripts { pack: "js", scripts: sources, imports: imports.take() });
                (shown_pack(js), title, favicon_url)
            }
            Err(e) => {
                trace_pack_load("js", &page_url, &page_url, format!("ERROR {e:#}"));
                ((error_document(&format!("JS pack failed: {e:#}")), None), title, favicon_url)
            }
        };
    };
    trace_skipped(&document, &page_url, "Dart", &["python", "js"]);
    if settings::get().dart_blocked.contains(&origin) {
        trace_load(&page_url, &src, format!("skipped: Dart is blocked for {origin}"));
        return (shown(document), title, favicon_url);
    }
    let wasm = if src.path().ends_with(".dart") {
        match dev_mode_compile(&src, &page_url, &origin, net_provider, signal).await {
            Ok(Some(wasm)) => wasm,
            Ok(None) => return (shown(document), title, favicon_url),
            // Compile errors are for the developer: show them on the page.
            Err(e) => return ((error_document(&format!("Compiling {src} failed:\n\n{e:#}")), None), title, favicon_url),
        }
    } else {
        match fetch_dart_program(&src, &page_url, net_provider, signal).await {
            Ok(wasm) => wasm,
            Err(e) => {
                trace_load(&page_url, &src, format!("ERROR {e:#}"));
                return (shown(document), title, favicon_url);
            }
        }
    };
    arm(&page_url);
    match dart_pack::start(document, env, &wasm) {
        Ok(dart) => {
            record(&dart, replay::Program::Dart(wasm));
            (shown_pack(dart), title, favicon_url)
        }
        // Loading failed partway; the document it was given is gone, so show the error.
        Err(e) => {
            trace_load(&page_url, &src, format!("ERROR {e:#}"));
            ((error_document(&format!("Dart pack failed: {e:#}")), None), title, favicon_url)
        }
    }
}

/// A page runs one pack: say in the trace which other packs' scripts it has that won't run.
fn trace_skipped(document: &BaseDocument, page: &Url, runs: &str, others: &[&'static str]) {
    for &pack in others {
        let count = page_scripts(document, pack).len();
        if count > 0 {
            let language = if pack == "js" { "JS" } else { "Python" };
            trace_pack_load(pack, page, page, format!("skipped {count} {language} script(s): this page runs {runs}"));
        }
    }
}

fn trace_load(page: &Url, src: &Url, result: String) {
    trace_pack_load("dart", page, src, result);
}

fn trace_pack_load(pack: &'static str, page: &Url, src: &Url, result: String) {
    let kind = if result.starts_with("ERROR") { TraceKind::Error } else { TraceKind::Load };
    host::record(TraceEntry { pack, kind, page: page.to_string(), call: "load".into(), args: src.to_string(), result, micros: 0 });
}

enum PageScript {
    Inline { source: String, module: bool },
    External { url: Url, module: bool },
}

/// The page's scripts for `pack` ("js" or "python"), in document order. For JS that's classic
/// and module scripts; for Python, `type="text/python"`.
fn page_scripts(document: &BaseDocument, pack: &str) -> Vec<PageScript> {
    let Ok(nodes) = document.query_selector_all("script") else { return Vec::new() };
    let mut scripts = Vec::new();
    for id in nodes {
        let Some(node) = document.get_node(id) else { continue };
        // Types compare ignoring ASCII case, as browsers do.
        let kind = node.attr(local_name!("type")).map(|t| t.trim().to_ascii_lowercase());
        let module = match (pack, kind.as_deref()) {
            ("js", None | Some("") | Some("text/javascript") | Some("application/javascript")) => false,
            ("js", Some("module")) => true,
            ("python", Some("text/python")) => false,
            _ => continue,
        };
        scripts.push(match node.attr(local_name!("src")) {
            Some(src) => match document.url().join(src) {
                Ok(url) => PageScript::External { url, module },
                Err(_) => continue,
            },
            None => PageScript::Inline { source: node.text_content(), module },
        });
    }
    scripts
}

/// Script sources in order. External scripts come through the gate; one that fails to load
/// is reported and skipped, as a browser would.
async fn fetch_sources(pack: &'static str, scripts: &[PageScript], page: &Url, net_provider: &Arc<StdNetProvider>, signal: &AbortSignal) -> Vec<Script> {
    let mut sources = Vec::new();
    let mut inline = 0;
    for script in scripts {
        match script {
            PageScript::Inline { source, module } => {
                inline += 1;
                // Inline modules need a URL to resolve relative imports against.
                let name = match module {
                    true => format!("{}#inline-module-{inline}", page.as_str().split('#').next().unwrap_or_default()),
                    false => format!("{page} (inline script {inline})"),
                };
                // Inline Python is indented to match the HTML around it.
                let source = if pack == "python" { python_pack::dedent(source) } else { source.clone() };
                sources.push(Script { name, source, module: *module });
            }
            PageScript::External { url, module } => match net_provider.fetch_for_page(Request::get(url.clone()).signal(signal.clone()), page).await {
                Ok((_, bytes)) => sources.push(Script { name: url.to_string(), source: String::from_utf8_lossy(&bytes).into_owned(), module: *module }),
                Err(e) => trace_pack_load(pack, page, url, format!("ERROR could not load: {e:?}")),
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
            trace_load(page, src, format!("skipped: {reason}"));
            return Ok(None);
        }
        DevDecision::Ask => {
            if prompt::ask(origin).await == prompt::Answer::Block {
                trace_load(page, src, format!("skipped: compiling source is blocked for {origin}"));
                return Ok(None);
            }
        }
        DevDecision::Compile => {}
    }
    let started = std::time::Instant::now();
    let wasm = dev_compile::compile(src, page, net_provider, signal).await?;
    trace_load(page, src, format!("compiled from source in {} ms", started.elapsed().as_millis()));
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

/// A page without a pack, shown as is.
fn shown(document: BaseDocument) -> (SubDocumentAttr, Option<PageRef>) {
    let page = Rc::new(RefCell::new(document));
    (SubDocumentAttr::new(page.clone()), Some(PageRef { doc: Rc::downgrade(&page), pack: None }))
}

fn shown_pack(document: PackDocument) -> (SubDocumentAttr, Option<PageRef>) {
    let page = document.page_ref();
    (SubDocumentAttr::new(document), Some(page))
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

    /// Each pack gets only its own scripts: `text/python` for Python; classic and module
    /// scripts for JS; other types for nobody.
    #[test]
    fn scripts_are_picked_by_type() {
        let html = r#"<script>js1</script><script type="Text/Python">py1</script><script type=" MODULE ">js2</script>
            <script type="text/python" src="app.py"></script><script type="text/x-template">neither</script>"#;
        let doc = HtmlDocument::from_html(html, DocumentConfig { base_url: Some("https://site.test/".into()), ..Default::default() }).into_inner();
        let names = |pack| page_scripts(&doc, pack).into_iter().map(|s| match s {
            PageScript::Inline { source, module } => format!("{source}{}", if module { " (module)" } else { "" }),
            PageScript::External { url, .. } => url.to_string(),
        }).collect::<Vec<_>>();
        assert_eq!(names("python"), ["py1", "https://site.test/app.py"]);
        assert_eq!(names("js"), ["js1", "js2 (module)"]);

        // The page runs Python, so its JS is skipped, and the trace says so.
        let page = Url::parse("https://skipped-js.test/").unwrap();
        trace_skipped(&doc, &page, "Python", &["js"]);
        assert!(host::trace().iter().any(|e| e.pack == "js" && e.page == page.as_str() && e.result == "skipped 2 JS script(s): this page runs Python"));
    }

    #[test]
    fn inline_python_is_dedented_and_keeps_its_lines() {
        let source = "\n    x = 1\n\n    def f():\n        return x\n";
        assert_eq!(python_pack::dedent(source), "\nx = 1\n\ndef f():\n    return x");
        // Nothing in common: unchanged.
        assert_eq!(python_pack::dedent("a = 1\n  # note"), "a = 1\n  # note");
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
