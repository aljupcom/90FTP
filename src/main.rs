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
}

fn main() {
    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();

    let saved_state = sessions::load_window_state();
    let mut builder = WindowBuilder::new()
        .with_title("90FTP - Modern Site Manager")
        .with_inner_size(LogicalSize::new(saved_state.width, saved_state.height))
        .with_min_inner_size(LogicalSize::new(800.0, 500.0));

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
    let app_state = Arc::new(AppState::new());

    let html_content = include_str!("ui/index.html");

    let mut webview_dir = sessions::get_storage_dir();
    webview_dir.push("webview_data");
    let _ = std::fs::create_dir_all(&webview_dir);

    // Clean up stale lock files from abnormal process termination if any
    let lock_file = webview_dir.join("EBWebView").join("Default").join("LOCK");
    if lock_file.exists() {
        let _ = std::fs::remove_file(&lock_file);
    }

    let mut context = wry::WebContext::new(Some(webview_dir));

    let webview = WebViewBuilder::new_with_web_context(&mut context)
        .with_devtools(true)
        .with_html(html_content)
        .with_ipc_handler({
            let app_state = app_state.clone();
            let proxy = proxy.clone();
            move |req| {
                let body = req.body().clone();
                let app_state = app_state.clone();
                let proxy = proxy.clone();

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
            }
        })
        .build(&window)
        .expect("Failed to initialize WebView");

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::NewEvents(StartCause::Init) => {
                println!("90FTP Desktop Client Started.");
            }
            Event::UserEvent(AppEvent::ExecScript(script)) => {
                if let Err(e) = webview.evaluate_script(&script) {
                    eprintln!("WebView evaluate_script error: {:?}", e);
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
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
            }
            _ => (),
        }
    });
}
