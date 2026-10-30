// On Windows do NOT show a console window when opening the app
#![cfg_attr(all(not(test), target_os = "windows"), windows_subsystem = "windows")]
#![allow(clippy::collapsible_if)]

//! A web browser with UI powered by Dioxus Native and content rendering powered by Blitz

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::sync::Arc;

use blitz_traits::net::Url;
use dioxus_native::{NodeHandle, WindowAttributes, prelude::*, use_back_button, use_window_event};
use winit::event::WindowEvent;
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};

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
mod ai_access;
mod cookies;
mod dart_pack;
mod debug;
mod dev_compile;
mod gate;
mod host;
mod idb;
mod inspector;
mod interpreter;
mod js_pack;
mod js_permission;
mod page_document;
mod profile;
mod prompt;
mod php_pack;
mod python_pack;
mod rust_pack;
mod replay;
mod request_panel;
mod settings;
mod site_css;
mod site_panel;
mod storage;
mod source_map;
mod trace_panel;
mod browser_history;
mod command_palette;
mod canvas;
#[cfg(any(feature = "screenshot", feature = "capture"))]
mod capture;
mod document_loader;
mod favicon;
mod fps_overlay;
mod history;
mod hot_reload;
mod icons;
mod nav;
mod status_bar;
mod tab;
mod tab_manager;
mod tab_strip;
mod toolbar;
mod url_suggestions;
mod websocket;

use about_pages::AboutPage;
use browser_history::{BrowsingHistory, HistoryService, HistoryStore, MAX_HISTORY_ENTRIES};
use status_bar::StatusBar;
use tab::{Tab, TabId, TabStoreImplExt, TabWebView, active_tab, tab_display_title};
use tab_manager::TabManager;
use tab_strip::{TabStrip, VerticalTabs};
use toolbar::Toolbar;
use url_suggestions::provide_url_suggester;
#[cfg(target_os = "windows")]
use winit::platform::windows::WinIcon;

/// The side panel open next to the page; they share a spot, so at most one is open.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Panel {
    None,
    Requests,
    Trace,
    Site,
    Inspect,
}

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

    // Choose the profile before anything reads settings, history or the HTTP cache.
    let (profile_name, args) = profile::parse_args(std::env::args().skip(1));
    if let Err(e) = profile::init(&profile_name) {
        eprintln!("browser: {e}");
        std::process::exit(1);
    }
    #[cfg(feature = "cache")]
    if let Some(dir) = profile::dir() {
        blitz_net::set_cache_dir(dir.join("http-cache"));
    }
    // One cookie jar for requests and `document.cookie`, saved in the profile's folder.
    cookies::install();
    let cli_url = args.iter().find_map(|a| parse_cli_url(a));

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

    dioxus_native::launch_cfg(app, contexts, vec![Box::new(window_attributes)]);
    // Cookies save shortly after they change; write any still waiting.
    cookies::jar().flush();
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

    let history_store: HistoryStore = use_hook(|| HistoryStore::open(profile::dir()));

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

    // Tabs, pins and spaces, reopened from the last session.
    let mut manager = use_context_provider(|| {
        TabManager::restore(net_provider.clone(), cli_initial_url.clone(), home_url.clone())
    });
    tab_manager::use_tab_upkeep(manager);
    hot_reload::use_hot_reload(manager);
    let tabs: Store<Vec<Tab>> = manager.tabs;
    let active_tab_id: Signal<TabId> = manager.active_tab_id;

    let open_new_tab = use_callback(move |url: Url| {
        let new_id = manager.open_tab(url);
        manager.activate(new_id);
        if let Some(handle) = url_input_handle() {
            drop(handle.set_focus(true));
        }
    });

    let mut palette_open: Signal<bool> = use_signal(|| false);
    let open_panel: Signal<Panel> = use_signal(|| Panel::None);

    // Keyboard shortcuts (Cmd on macOS, Ctrl elsewhere): T new tab, W close tab,
    // Shift+T reopen the last closed tab, K or Shift+P the command palette.
    {
        let home_url = home_url.clone();
        let mut modifiers = ModifiersState::empty();
        use_window_event(move |event, _target| match event {
            WindowEvent::ModifiersChanged(new_modifiers) => {
                modifiers = new_modifiers.state();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let shortcut_modifier = if cfg!(target_os = "macos") {
                    modifiers.meta_key()
                } else {
                    modifiers.control_key()
                };
                if !shortcut_modifier || !event.state.is_pressed() {
                    return;
                }
                // Held keys repeat. Only new tab may; closing, reopening and toggling the
                // palette happen once per press.
                let new_tab = event.physical_key == PhysicalKey::Code(KeyCode::KeyT) && !modifiers.shift_key();
                if event.repeat && !new_tab {
                    return;
                }
                match event.physical_key {
                    PhysicalKey::Code(KeyCode::KeyT) if modifiers.shift_key() => manager.reopen_closed(None),
                    PhysicalKey::Code(KeyCode::KeyT) => open_new_tab(home_url.clone()),
                    // Keep at least one tab open, matching the close button.
                    PhysicalKey::Code(KeyCode::KeyW) => manager.close(active_tab_id()),
                    PhysicalKey::Code(KeyCode::KeyK) => palette_open.toggle(),
                    PhysicalKey::Code(KeyCode::KeyP) if modifiers.shift_key() => palette_open.toggle(),
                    _ => {}
                }
            }
            _ => {}
        });
    }

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

    let fps_overlay_el = rsx!(if show_fps() {
        fps_overlay::FpsOverlay {}
    });

    let mut window_title = tab_display_title(active_tab(tabs, active_tab_id()));
    // Say which profile a window is, so two profiles' windows can be told apart.
    if profile::name() != profile::DEFAULT {
        window_title = format!("{window_title} — {}", profile::name());
    }

    let vertical = (manager.vertical)();
    let frame_class = match (IS_MOBILE, vertical) {
        (true, _) => "mobile",
        (false, true) => "vertical",
        (false, false) => "",
    };

    rsx!(
        div {
            id: "frame",
            style: "--safe-area-top: {TOP_PAD}; padding-top: {TOP_PAD}; padding-bottom: {BOTTOM_PAD};",
            class: frame_class,
            title { "{window_title}" }
            document::Link { rel: "stylesheet", href: BROWSER_UI_STYLES }
            if vertical {
                VerticalTabs { home_url: home_url.clone(), open_new_tab }
            }
            div { class: "chrome-column",
                if !vertical {
                    TabStrip { home_url, open_new_tab }
                }
                Toolbar {
                    url_input_handle,
                    url_input_value,
                    tabs,
                    active_tab_id,
                    open_new_tab,
                    show_fps,
                    open_panel,
                }
                prompt::PromptBar {}
                js_permission::JsBar { tabs, active_tab_id }
                ai_access::AiAccess { tabs, active_tab_id }
                // One row, so split view's two tabs sit side by side.
                div { class: "tab-area",
                    for tab in tabs.iter() {
                        TabWebView {
                            key: "{tab.tab_id()}",
                            tabs,
                            tab_id: tab.tab_id(),
                            active_tab_id,
                        }
                    }
                }
                {fps_overlay_el}
                match open_panel() {
                    Panel::Requests => rsx!(request_panel::RequestPanel {}),
                    Panel::Trace => rsx!(trace_panel::TracePanel { tabs, active_tab_id }),
                    Panel::Site => rsx!(site_panel::SitePanel { tabs, active_tab_id }),
                    Panel::Inspect => rsx!(inspector::Inspector { tabs, active_tab_id, open_panel }),
                    Panel::None => rsx!(),
                }
                StatusBar { tabs, active_tab_id }
                if palette_open() {
                    command_palette::CommandPalette { open: palette_open, open_panel, open_new_tab }
                }
            }
        }
    )
}
