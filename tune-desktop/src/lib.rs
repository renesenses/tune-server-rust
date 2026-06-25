//! Tune desktop controller (Tauri 2).
//!
//! This is a thin native shell: it hosts the existing web UI and talks to a
//! running `tune-server` over its HTTP/WS API (the server stays the single
//! source of truth). The Rust side exposes only what the browser sandbox
//! can't do — native dialogs, OS integration, and (later) media keys and
//! local audio-device access.
//!
//! Bootstrap model: the frontend connects to `GET /ws`, receives the
//! `type: "snapshot"` message for the full current state, then applies the
//! typed delta events — no polling.

use serde::Serialize;
use tauri::Manager;

#[derive(Serialize)]
pub struct AppInfo {
    /// Human-facing app name.
    pub name: String,
    /// Desktop app version (mirrors the workspace version).
    pub version: String,
    /// Base URL of the tune-server this client controls. Configurable via
    /// `TUNE_SERVER_URL`; defaults to the local server. The frontend uses
    /// this for its REST calls and to build the `ws(s)://…/ws` endpoint.
    pub server_url: String,
}

/// Expose app/build info and the configured server URL to the frontend.
/// Invoked from JS via `invoke("app_info")`.
#[tauri::command]
fn app_info() -> AppInfo {
    AppInfo {
        name: "Tune Desktop".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        server_url: std::env::var("TUNE_SERVER_URL")
            .unwrap_or_else(|_| "http://localhost:8888".into()),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Native file/folder dialogs (e.g. picking a library directory) —
        // one of the things the web sandbox can't do.
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![app_info])
        .setup(|app| {
            // The window loads the live server UI (http://localhost:8888 by
            // default, set in tauri.conf.json) so it's the exact current web UI
            // with same-origin API/WS. Override the host at runtime with
            // TUNE_SERVER_URL — handy for pointing at a remote server.
            if let Ok(server) = std::env::var("TUNE_SERVER_URL")
                && let Some(win) = app.get_webview_window("main")
                && let Ok(url) = server.parse::<tauri::Url>()
            {
                let _ = win.navigate(url);
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tune desktop application");
}
