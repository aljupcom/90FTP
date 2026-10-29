#![windows_subsystem = "windows"]

mod app_state;
mod archive;
mod ftp_client;
mod local_fs;
mod logger;
mod models;
mod relay_engine;
mod remote;
mod remote_transfer;
mod sessions;
mod sftp_client;
mod sync;
mod transfer_control;
mod transfer_queue;
mod ui_prefs;
mod watcher;
mod webdav_client;

use std::sync::Arc;
use tao::{
    dpi::LogicalSize,
    event::{Event, StartCause, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::WindowBuilder,
};
#[cfg(target_os = "windows")]
use tao::platform::windows::IconExtWindows;
use wry::WebViewBuilder;

use crate::app_state::AppState;
use crate::models::IpcRequest;

#[derive(Debug, Clone, Copy)]
pub enum IpcTarget {
    Main,
    Terminal,
    Broadcast,
}

#[derive(Debug)]
enum AppEvent {
    ExecScript(IpcTarget, String),
    OpenNativeTerminal(serde_json::Value),
    /// Reveal the main window. Sent by the page once its first frame is ready,
    /// or by the safety-net timer if that signal never arrives.
    ShowMainWindow,
}

fn main() {
    logger::init_logger();
    // Warm the credential vault key (and perform the one-time DPAPI migration)
    // before the window opens, so the first IPC call is never blocked by it.
    sessions::init_vault();
    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();

    let saved_state = sessions::load_window_state();
    let mut builder = WindowBuilder::new()
        .with_title("90FTP - Modern Site Manager")
        .with_inner_size(LogicalSize::new(saved_state.width, saved_state.height))
        .with_min_inner_size(LogicalSize::new(800.0, 500.0))
        .with_visible(false);

    if saved_state.is_maximized {
        builder = builder.with_maximized(true);
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(icon) = tao::window::Icon::from_resource(1, None)
            .or_else(|_| tao::window::Icon::from_path("assets/app_icon.ico", None))
        {
            builder = builder.with_window_icon(Some(icon));
        }
    }

    let window = builder
        .build(&event_loop)
        .expect("Failed to create native desktop window");

    let proxy = event_loop.create_proxy();
    let proxy_cb = {
        let proxy = proxy.clone();
        Arc::new(move |script: String| {
            let _ = proxy.send_event(AppEvent::ExecScript(IpcTarget::Broadcast, script));
        })
    };
    let proxy_term = proxy.clone();
    let term_cb = {
        Arc::new(move |params: serde_json::Value| {
            let _ = proxy_term.send_event(AppEvent::OpenNativeTerminal(params));
        })
    };
    let app_state = Arc::new(AppState::new(Some(proxy_cb), Some(term_cb)));

    // The application script lives in `ui/app.js` and is injected here so the
    // page stays a single self-contained document: WebView2 loads it through
    // NavigateToString, where a relative <script src> would not resolve.
    // The interface language is injected before any page script runs, so the
    // correct direction and language are in place on the very first paint.
    // The value is whitelisted by `ui_prefs::normalize_language` and serialized
    // through serde_json, so a corrupted file cannot break out of the literal.
    let ui_prefs_boot = ui_prefs::load();
    let ui_prefs_script = format!(
        "window.__UI_PREFS__ = {};",
        serde_json::json!({ "language": ui_prefs_boot.language })
    );

    let html_content = include_str!("ui/index.html")
        .replace("// __APP_JS__", include_str!("ui/app.js"))
        .replace("/* __XTERM_CSS__ */", include_str!("ui/xterm.css"))
        .replace("// __XTERM_JS__", include_str!("ui/xterm.js"))
        .replace("// __XTERM_FIT_JS__", include_str!("ui/xterm-addon-fit.js"))
        .replace("// __I18N_EN__", include_str!("ui/i18n.en.js"))
        .replace("// __I18N_AR__", include_str!("ui/i18n.ar.js"))
        .replace("// __I18N_JS__", include_str!("ui/i18n.js"));

    let mut webview_dir = sessions::get_storage_dir();
    webview_dir.push("webview_data");
    let _ = std::fs::create_dir_all(&webview_dir);

    // Clean up stale lock files from abnormal process termination if any
    let lock_file = webview_dir.join("EBWebView").join("Default").join("LOCK");
    if lock_file.exists() {
        let _ = std::fs::remove_file(&lock_file);
    }

    let mut context = wry::WebContext::new(Some(webview_dir));

    let app_state_ipc = app_state.clone();
    let proxy_ipc = proxy.clone();
    let ipc_handler = move |req: wry::http::Request<String>| {
        let body = req.body().clone();
        let app_state = app_state_ipc.clone();
        let proxy = proxy_ipc.clone();

        std::thread::spawn(move || {
            match serde_json::from_str::<IpcRequest>(&body) {
                Ok(ipc_req) => {
                    // The page reports its first painted frame here. That is the
                    // signal to reveal the window, which is why the window is
                    // built hidden: WebView2 needs a few hundred milliseconds to
                    // come up, and showing the native frame before that paints
                    // the empty (black) client area first.
                    if ipc_req.action == "ui_ready" {
                        let _ = proxy.send_event(AppEvent::ShowMainWindow);
                    }
                    let resp = app_state.handle_request(ipc_req);
                    match serde_json::to_string(&resp) {
                        Ok(resp_json) => {
                            let script = format!("window.onRustResponse({});", resp_json);
                            let _ = proxy.send_event(AppEvent::ExecScript(IpcTarget::Main, script));
                        }
                        Err(e) => {
                            eprintln!("Failed to serialize IPC response: {:?}", e);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Failed to parse IPC request: {:?}. Body: {}", e, body);
                }
            }
        });
    };

    let webview = WebViewBuilder::new_with_web_context(&mut context)
        // DevTools stay available in debug builds only; shipping them enabled in a
        // release binary hands any script-injection bug a full execution console.
        .with_devtools(cfg!(debug_assertions))
        .with_background_color((17, 17, 19, 255))
        .with_initialization_script(&ui_prefs_script)
        .with_html(html_content)
        .with_ipc_handler(ipc_handler)
        .build(&window)
        .expect("Failed to initialize WebView");

    // The window stays hidden until the page says it has painted. `with_background_color`
    // already matches the dark shell, so even the shortest gap reads as the app
    // background instead of a black rectangle.
    let show_fallback = proxy.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(2500));
        let _ = show_fallback.send_event(AppEvent::ShowMainWindow);
    });

    let mut terminal_instance: Option<(tao::window::Window, wry::WebView)> = None;

    event_loop.run(move |event, event_loop_target, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::NewEvents(StartCause::Init) => {
                println!("90FTP Desktop Client Started.");
            }
            Event::UserEvent(AppEvent::ExecScript(target, script)) => {
                match target {
                    IpcTarget::Main => {
                        if let Err(e) = webview.evaluate_script(&script) {
                            logger::log_error(&format!("Main WebView evaluate_script error: {:?}", e));
                        }
                    }
                    IpcTarget::Terminal => {
                        if let Some((_, ref term_wv)) = terminal_instance {
                            if let Err(e) = term_wv.evaluate_script(&script) {
                                logger::log_error(&format!("Terminal WebView evaluate_script error: {:?}", e));
                            }
                        }
                    }
                    IpcTarget::Broadcast => {
                        if let Err(e) = webview.evaluate_script(&script) {
                            logger::log_error(&format!("Main WebView evaluate_script error: {:?}", e));
                        }
                        if let Some((_, ref term_wv)) = terminal_instance {
                            if let Err(e) = term_wv.evaluate_script(&script) {
                                logger::log_error(&format!("Terminal WebView evaluate_script error: {:?}", e));
                            }
                        }
                    }
                }
            }
            Event::UserEvent(AppEvent::ShowMainWindow) => {
                if !window.is_visible() {
                    window.set_visible(true);
                    window.set_focus();
                }
            }
            Event::UserEvent(AppEvent::OpenNativeTerminal(params)) => {
                let host = params.get("host").and_then(|v| v.as_str()).unwrap_or("LocalPC");
                let user = params.get("user").and_then(|v| v.as_str()).unwrap_or("root");
                let cwd = params.get("cwd").and_then(|v| v.as_str()).unwrap_or("~");
                let title = format!("{}@{}:{} - 90FTP Native Terminal", user, host, cwd);

                if let Some((ref term_win, ref term_wv)) = terminal_instance {
                    term_win.set_title(&title);
                    term_win.set_visible(true);
                    term_win.set_focus();
                    let init_run = format!("if(window.initTerminal)window.initTerminal({});", params);
                    let _ = term_wv.evaluate_script(&init_run);
                } else {
                    let mut term_builder = WindowBuilder::new()
                        .with_title(&title)
                        .with_inner_size(LogicalSize::new(840.0, 520.0))
                        .with_min_inner_size(LogicalSize::new(450.0, 300.0))
                        .with_resizable(true);

                    #[cfg(target_os = "windows")]
                    {
                        if let Ok(icon) = tao::window::Icon::from_resource(1, None)
                            .or_else(|_| tao::window::Icon::from_path("assets/app_icon.ico", None))
                        {
                            term_builder = term_builder.with_window_icon(Some(icon));
                        }
                    }

                    if let Ok(term_win) = term_builder.build(event_loop_target) {
                        let app_state_term = app_state.clone();
                        let proxy_term = proxy.clone();
                        let term_ipc_handler = move |req: wry::http::Request<String>| {
                            let body = req.body().clone();
                            let app_state = app_state_term.clone();
                            let proxy = proxy_term.clone();

                            std::thread::spawn(move || {
                                match serde_json::from_str::<IpcRequest>(&body) {
                                    Ok(ipc_req) => {
                                        let resp = app_state.handle_request(ipc_req);
                                        if let Ok(resp_json) = serde_json::to_string(&resp) {
                                            let script = format!("if(window.onRustResponse)window.onRustResponse({});", resp_json);
                                            let _ = proxy.send_event(AppEvent::ExecScript(IpcTarget::Terminal, script));
                                        }
                                    }
                                    Err(e) => {
                                        logger::log_error(&format!("Failed to parse terminal IPC request: {:?}. Body: {}", e, body));
                                    }
                                }
                            });
                        };

                        let term_html = include_str!("ui/terminal.html")
                            .replace("/* __XTERM_CSS__ */", include_str!("ui/xterm.css"))
                            .replace("// __XTERM_JS__", include_str!("ui/xterm.js"))
                            .replace("// __XTERM_FIT_JS__", include_str!("ui/xterm-addon-fit.js"));

                        let init_script = format!("window.__INIT_PARAMS__ = {};", params);

                        if let Ok(term_wv) = WebViewBuilder::new_with_web_context(&mut context)
                            .with_devtools(cfg!(debug_assertions))
                            .with_background_color((0, 0, 0, 255))
                            .with_initialization_script(&init_script)
                            .with_html(term_html)
                            .with_ipc_handler(term_ipc_handler)
                            .build(&term_win)
                        {
                            let init_run = format!("if(window.initTerminal)window.initTerminal({});", params);
                            let _ = term_wv.evaluate_script(&init_run);
                            terminal_instance = Some((term_win, term_wv));
                        }
                    }
                }
            }
            Event::WindowEvent {
                window_id,
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if window_id == window.id() {
                    let size = window.inner_size();
                    let is_maximized = window.is_maximized();
                    let scale = window.scale_factor();
                    let logical_size = size.to_logical::<f64>(scale);
                    sessions::save_window_state(&sessions::WindowState {
                        width: logical_size.width,
                        height: logical_size.height,
                        is_maximized,
                    });
                    *control_flow = ControlFlow::Exit;
                } else if let Some((ref term_win, _)) = terminal_instance {
                    if window_id == term_win.id() {
                        term_win.set_visible(false);
                    }
                }
            }
            _ => (),
        }
    });
}
