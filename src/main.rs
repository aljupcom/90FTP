#![windows_subsystem = "windows"]

mod app_state;
mod ftp_client;
mod local_fs;
mod models;
mod remote;
mod sessions;
mod sftp_client;

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

#[derive(Debug)]
enum AppEvent {
    ExecScript(String),
    OpenNativeTerminal(serde_json::Value),
}

fn main() {
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
            let _ = proxy.send_event(AppEvent::ExecScript(script));
        })
    };
    let proxy_term = proxy.clone();
    let term_cb = {
        Arc::new(move |params: serde_json::Value| {
            let _ = proxy_term.send_event(AppEvent::OpenNativeTerminal(params));
        })
    };
    let app_state = Arc::new(AppState::new(Some(proxy_cb), Some(term_cb)));

    let html_content = include_str!("ui/index.html")
        .replace("/* __XTERM_CSS__ */", include_str!("ui/xterm.css"))
        .replace("// __XTERM_JS__", include_str!("ui/xterm.js"))
        .replace("// __XTERM_FIT_JS__", include_str!("ui/xterm-addon-fit.js"));

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
                    let resp = app_state.handle_request(ipc_req);
                    match serde_json::to_string(&resp) {
                        Ok(resp_json) => {
                            let script = format!("window.onRustResponse({});", resp_json);
                            let _ = proxy.send_event(AppEvent::ExecScript(script));
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
        .with_devtools(true)
        .with_background_color((17, 17, 19, 255))
        .with_html(html_content)
        .with_ipc_handler(ipc_handler)
        .build(&window)
        .expect("Failed to initialize WebView");

    // Show window smoothly now that WebView2 is initialized with dark theme background
    window.set_visible(true);

    let mut terminal_instance: Option<(tao::window::Window, wry::WebView)> = None;

    event_loop.run(move |event, event_loop_target, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::NewEvents(StartCause::Init) => {
                println!("90FTP Desktop Client Started.");
            }
            Event::UserEvent(AppEvent::ExecScript(script)) => {
                if let Err(e) = webview.evaluate_script(&script) {
                    eprintln!("WebView evaluate_script error: {:?}", e);
                }
                if let Some((_, ref term_wv)) = terminal_instance {
                    let _ = term_wv.evaluate_script(&script);
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
                                if let Ok(ipc_req) = serde_json::from_str::<IpcRequest>(&body) {
                                    let resp = app_state.handle_request(ipc_req);
                                    if let Ok(resp_json) = serde_json::to_string(&resp) {
                                        let script = format!("if(window.onRustResponse)window.onRustResponse({});", resp_json);
                                        let _ = proxy.send_event(AppEvent::ExecScript(script));
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
                            .with_devtools(true)
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
                        terminal_instance = None;
                    }
                }
            }
            _ => (),
        }
    });
}
