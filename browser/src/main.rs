// On Windows do NOT show a console window when opening the app
#![cfg_attr(all(not(test), target_os = "windows"), windows_subsystem = "windows")]
#![allow(clippy::collapsible_if)]

//! A web browser with UI powered by Dioxus Native and content rendering powered by Blitz

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::sync::Arc;

use blitz_traits::net::Url;
use dioxus_native::{NodeHandle, WindowAttributes, prelude::*, use_back_button};

#[cfg(target_os = "macos")]
use winit::platform::macos::WindowAttributesMacOS;

// Every network request goes through the gate; see gate.rs.
pub(crate) type StdNetProvider = gate::Gate;

/// A `file://` URL for a file in `assets/`. Dioxus's `asset!` only resolves when built with
/// the Dioxus CLI, so we point at the source files directly. The binary therefore finds its
/// UI files only while the repo stays where it was built.
macro_rules! local_asset {
    ($path:literal) => {
        concat!("file://", env!("CARGO_MANIFEST_DIR"), "/assets/", $path)
    };
}

mod about_pages;
mod dart_document;
mod dart_pack;
mod dev_compile;
mod gate;
mod host;
mod prompt;
mod request_panel;
mod settings;
mod source_map;
mod trace_panel;
mod browser_history;
#[cfg(any(feature = "screenshot", feature = "capture"))]
mod capture;
mod document_loader;
mod favicon;
mod fps_overlay;
mod history;
mod icons;
mod nav;
mod status_bar;
mod tab;
mod tab_strip;
mod toolbar;
mod url_suggestions;

use about_pages::AboutPage;
use browser_history::{BrowsingHistory, HistoryService, HistoryStore, MAX_HISTORY_ENTRIES};
use status_bar::StatusBar;
use tab::{Tab, TabId, TabStoreImplExt, TabWebView, active_tab, open_tab, tab_display_title};
use tab_strip::TabStrip;
use toolbar::Toolbar;
use url_suggestions::provide_url_suggester;
#[cfg(target_os = "windows")]
use winit::platform::windows::WinIcon;

static BROWSER_UI_STYLES: &str = local_asset!("browser.css");
pub(crate) const IS_MOBILE: bool = cfg!(any(target_os = "android", target_os = "ios"));

#[unsafe(no_mangle)]
#[cfg(target_os = "android")]
pub fn android_main(android_app: dioxus_native::AndroidApp) {
    dioxus_native::set_android_app(android_app);
    main()
}

#[derive(Clone)]
pub(crate) struct CliInitialUrl(pub Option<Url>);

fn parse_cli_url(s: &str) -> Option<Url> {
    if let Ok(url) = Url::parse(s) {
        return Some(url);
    }
    if s.contains('.') && !s.contains(' ') {
        if let Ok(url) = Url::parse(&format!("https://{s}")) {
            return Some(url);
        }
    }
    None
}

fn main() {
    #[cfg(feature = "tracing")]
    tracing_subscriber::fmt::init();

    let cli_url = std::env::args().skip(1).find_map(|a| parse_cli_url(&a));

    let window_attributes = WindowAttributes::default();
    #[cfg(target_os = "windows")]
    let window_attributes = window_attributes
        .with_window_icon(WinIcon::from_resource(32512, None).map(Into::into).ok());
    #[cfg(target_os = "macos")]
    let window_attributes = window_attributes.with_platform_attributes(Box::new(
        WindowAttributesMacOS::default()
            .with_titlebar_transparent(true)
            .with_fullsize_content_view(true)
            .with_title_hidden(true)
            .with_unified_titlebar(true),
    ));

    let initial_url_ctx = CliInitialUrl(cli_url);
    let contexts: Vec<Box<dyn Fn() -> Box<dyn std::any::Any> + Send + Sync>> =
        vec![Box::new(move || {
            Box::new(initial_url_ctx.clone()) as Box<dyn std::any::Any>
        })];

    dioxus_native::launch_cfg(app, contexts, vec![Box::new(window_attributes)])
}

fn app() -> Element {
    let home_url = use_hook(|| AboutPage::NewTab.parsed_url());
    let cli_initial_url = use_hook(|| try_consume_context::<CliInitialUrl>().and_then(|c| c.0));
    // dioxus-native creates the real provider; wrap it so nothing fetches around the gate.
    let net_provider = use_context_provider(|| {
        Arc::new(StdNetProvider::new(consume_context::<Arc<blitz_net::Provider>>()))
    });

    let url_input_handle: Signal<Option<NodeHandle>> = use_signal(|| None);
    let url_input_value = use_signal(|| home_url.to_string());

    let history_store: HistoryStore = use_hook(HistoryStore::open);

    // Synchronous on purpose: the toolbar's URL suggestions read from this
    // store on first render, so the entries need to be present before the
    // tree mounts. The read is a single sqlite query capped at
    // MAX_HISTORY_ENTRIES rows and runs once per process.
    let browsing_history: Store<BrowsingHistory> = {
        let history_store = history_store.clone();
        use_store(move || {
            BrowsingHistory::from_entries(history_store.load_recent(MAX_HISTORY_ENTRIES))
        })
    };

    let history_service = HistoryService::new(browsing_history, history_store);
    use_context_provider(|| history_service.clone());
    provide_url_suggester(browsing_history);

    let tabs: Store<Vec<Tab>> = use_store(Vec::new);
    let mut active_tab_id: Signal<TabId> = use_hook(|| {
        let first_tab_url = cli_initial_url.clone().unwrap_or_else(|| home_url.clone());
        let tab = open_tab(tabs, first_tab_url, net_provider.clone());
        Signal::new(tab.tab_id())
    });

    let open_new_tab = use_callback(move |url: Url| {
        let new_id = open_tab(tabs, url, net_provider.clone());
        active_tab_id.set(new_id.tab_id());
        if let Some(handle) = url_input_handle() {
            drop(handle.set_focus(true));
        }
    });

    // Navigate back in the active tab's history when the (Android) hardware
    // back button is pressed. `go_back` is a no-op when there is no history to
    // go back to.
    use_back_button(move || {
        active_tab(tabs, active_tab_id()).go_back();
    });

    // HACK: Winit doesn't support "safe area" on Android yet.
    // So we just hardcode a fallback safe area.
    const TOP_PAD: &str = if cfg!(target_os = "android") {
        "30px"
    } else {
        "0px"
    };
    const BOTTOM_PAD: &str = if cfg!(target_os = "android") {
        "44px"
    } else {
        "0px"
    };

    let show_fps: Signal<bool> = use_signal(|| false);
    let show_requests: Signal<bool> = use_signal(|| false);
    let show_trace: Signal<bool> = use_signal(|| false);

    let fps_overlay_el = rsx!(if show_fps() {
        fps_overlay::FpsOverlay {}
    });

    let window_title = tab_display_title(active_tab(tabs, active_tab_id()));

    rsx!(
        div {
            id: "frame",
            style: "--safe-area-top: {TOP_PAD}; padding-top: {TOP_PAD}; padding-bottom: {BOTTOM_PAD};",
            class: if IS_MOBILE { "mobile" } else { "" },
            title { "{window_title}" }
            document::Link { rel: "stylesheet", href: BROWSER_UI_STYLES }
            TabStrip {
                tabs,
                active_tab_id,
                home_url,
                open_new_tab,
            }
            Toolbar {
                url_input_handle,
                url_input_value,
                tabs,
                active_tab_id,
                open_new_tab,
                show_fps,
                show_requests,
                show_trace,
            }
            prompt::PromptBar {}
            for tab in tabs.iter() {
                TabWebView {
                    key: "{tab.tab_id()}",
                    tabs,
                    tab_id: tab.tab_id(),
                    active_tab_id,
                }
            }
            {fps_overlay_el}
            if show_requests() {
                request_panel::RequestPanel {}
            }
            if show_trace() {
                trace_panel::TracePanel {}
            }
            StatusBar { tabs, active_tab_id }
        }
    )
}
