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
use crate::dart_document::DartDocument;
use crate::favicon::favicon_candidate;
use crate::host::{self, TraceEntry};
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
    DocumentConfig {
        viewport: None,
        base_url,
        net_provider: Some(net_provider as _),
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

    let Some(wasm_url) = dart_script_url(&document) else {
        return (SubDocumentAttr::new(document), title, favicon_url);
    };
    match fetch_dart_program(&wasm_url, net_provider, signal).await {
        Ok(wasm) => match DartDocument::start(document, &wasm) {
            Ok(dart) => (SubDocumentAttr::new(dart), title, favicon_url),
            // Loading failed partway; the document it was given is gone, so show the error.
            Err(e) => (error_document(&format!("Dart pack failed: {e:#}")), title, favicon_url),
        },
        Err(e) => {
            host::record(TraceEntry {
                pack: "dart",
                call: "load".into(),
                args: wasm_url.to_string(),
                result: format!("ERROR {e:#}"),
                micros: 0,
            });
            (SubDocumentAttr::new(document), title, favicon_url)
        }
    }
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
    net_provider: &Arc<StdNetProvider>,
    signal: &AbortSignal,
) -> anyhow::Result<Vec<u8>> {
    let request = Request::get(wasm_url.clone()).signal(signal.clone());
    let (_, wasm) = net_provider.fetch_async(request).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(wasm.to_vec())
}

fn error_document(message: &str) -> SubDocumentAttr {
    let html = format!(
        "<html><body style=\"font-family: sans-serif; padding: 24px\"><h2>Page script failed</h2><pre style=\"white-space: pre-wrap\">{}</pre></body></html>",
        message.replace('&', "&amp;").replace('<', "&lt;")
    );
    SubDocumentAttr::new(HtmlDocument::from_html(&html, DocumentConfig::default()).into_inner())
}
