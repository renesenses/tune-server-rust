#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod server;

use serde::Serialize;
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager, State, WebviewUrl, WebviewWindowBuilder,
};
use tokio::sync::RwLock;

struct AppState {
    server_url: RwLock<String>,
    active_zone_id: RwLock<i64>,
    http: reqwest::Client,
}

#[derive(Debug, Clone, Serialize)]
struct WidgetData {
    zones: Vec<serde_json::Value>,
    now_playing: Option<serde_json::Value>,
    state: String,
    zone_id: i64,
    position_ms: i64,
    volume: f64,
    queue_length: i64,
    queue_position: i64,
}

#[tauri::command]
async fn get_status(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let url = state.server_url.read().await;
    let zone_id = *state.active_zone_id.read().await;
    let resp = state
        .http
        .get(format!("{url}/api/v1/widget/data?zone_id={zone_id}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json().await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn get_zones(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let url = state.server_url.read().await;
    let resp = state
        .http
        .get(format!("{url}/api/v1/zones"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json().await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn play_pause(state: State<'_, AppState>) -> Result<(), String> {
    let url = state.server_url.read().await;
    let zone_id = *state.active_zone_id.read().await;
    let status: serde_json::Value = state
        .http
        .get(format!("{url}/api/v1/widget/data?zone_id={zone_id}"))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let endpoint = if status["state"].as_str() == Some("playing") {
        "pause"
    } else {
        "resume"
    };
    state
        .http
        .post(format!("{url}/api/v1/zones/{zone_id}/{endpoint}"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn next_track(state: State<'_, AppState>) -> Result<(), String> {
    let url = state.server_url.read().await;
    let zone_id = *state.active_zone_id.read().await;
    state
        .http
        .post(format!("{url}/api/v1/zones/{zone_id}/next"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn prev_track(state: State<'_, AppState>) -> Result<(), String> {
    let url = state.server_url.read().await;
    let zone_id = *state.active_zone_id.read().await;
    state
        .http
        .post(format!("{url}/api/v1/zones/{zone_id}/previous"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn set_volume(state: State<'_, AppState>, volume: i32) -> Result<(), String> {
    let url = state.server_url.read().await;
    let zone_id = *state.active_zone_id.read().await;
    state
        .http
        .put(format!("{url}/api/v1/zones/{zone_id}/volume"))
        .json(&serde_json::json!({"volume": volume}))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn select_zone(state: State<'_, AppState>, zone_id: i64) -> Result<(), String> {
    *state.active_zone_id.write().await = zone_id;
    Ok(())
}

#[tauri::command]
async fn search(state: State<'_, AppState>, query: String) -> Result<serde_json::Value, String> {
    let url = state.server_url.read().await;
    let resp = state
        .http
        .get(format!(
            "{url}/api/v1/search?q={}&limit=8",
            urlencoding::encode(&query)
        ))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json().await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn set_server_url(state: State<'_, AppState>, url: String) -> Result<String, String> {
    let clean = if url.starts_with("http") {
        url.clone()
    } else {
        format!("http://{url}")
    };
    *state.server_url.write().await = clean.clone();
    // Persist to config file
    if let Some(dir) = dirs::config_dir() {
        let cfg_dir = dir.join("tune-widget");
        std::fs::create_dir_all(&cfg_dir).ok();
        let cfg_file = cfg_dir.join("config.json");
        let cfg = serde_json::json!({"server_url": &clean});
        std::fs::write(&cfg_file, cfg.to_string()).ok();
    }
    Ok(clean)
}

#[tauri::command]
async fn get_server_url(state: State<'_, AppState>) -> Result<String, String> {
    Ok(state.server_url.read().await.clone())
}

#[tauri::command]
async fn http_get(state: State<'_, AppState>, url: String) -> Result<serde_json::Value, String> {
    state.http.get(&url).send().await.map_err(|e| e.to_string())?
        .json().await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn http_post(state: State<'_, AppState>, url: String) -> Result<(), String> {
    state.http.post(&url).send().await.map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn http_put(state: State<'_, AppState>, url: String, body: String) -> Result<(), String> {
    state.http.put(&url).header("Content-Type", "application/json").body(body)
        .send().await.map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn play_item(
    state: State<'_, AppState>,
    item_type: String,
    item_id: i64,
) -> Result<(), String> {
    let url = state.server_url.read().await;
    let zone_id = *state.active_zone_id.read().await;
    let body = match item_type.as_str() {
        "album" => serde_json::json!({"album_id": item_id}),
        _ => serde_json::json!({"track_id": item_id}),
    };
    state
        .http
        .post(format!("{url}/api/v1/zones/{zone_id}/play"))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Global keyboard shortcuts — media control from any app.
//
// An earlier version registered these with per-shortcut `on_shortcut()`
// closures inside setup(); that crashed on macOS. This version uses the
// plugin's single global handler plus explicit `register()` calls whose
// failures are logged (never unwrapped), and does all work on the async
// runtime so the AppKit main thread is never blocked in the callback.
// ---------------------------------------------------------------------------
#[derive(Clone, Copy)]
enum ShortcutAction {
    PlayPause,
    Next,
    Prev,
    VolUp,
    VolDown,
}

const SHORTCUTS: &[(&str, ShortcutAction)] = &[
    ("CmdOrCtrl+Shift+Space", ShortcutAction::PlayPause),
    ("CmdOrCtrl+Shift+Right", ShortcutAction::Next),
    ("CmdOrCtrl+Shift+Left", ShortcutAction::Prev),
    ("CmdOrCtrl+Shift+Up", ShortcutAction::VolUp),
    ("CmdOrCtrl+Shift+Down", ShortcutAction::VolDown),
];

async fn fetch_widget_data(
    http: &reqwest::Client,
    url: &str,
    zone_id: i64,
) -> Option<serde_json::Value> {
    http.get(format!("{url}/api/v1/widget/data?zone_id={zone_id}"))
        .send()
        .await
        .ok()?
        .json::<serde_json::Value>()
        .await
        .ok()
}

async fn run_shortcut_action(app: tauri::AppHandle, action: ShortcutAction) {
    let state = app.state::<AppState>();
    let url = state.server_url.read().await.clone();
    let zone_id = *state.active_zone_id.read().await;
    let http = state.http.clone();
    match action {
        ShortcutAction::Next => {
            let _ = http
                .post(format!("{url}/api/v1/zones/{zone_id}/next"))
                .send()
                .await;
        }
        ShortcutAction::Prev => {
            let _ = http
                .post(format!("{url}/api/v1/zones/{zone_id}/previous"))
                .send()
                .await;
        }
        ShortcutAction::PlayPause => {
            let playing = fetch_widget_data(&http, &url, zone_id)
                .await
                .map(|d| d["state"].as_str() == Some("playing"))
                .unwrap_or(false);
            let endpoint = if playing { "pause" } else { "resume" };
            let _ = http
                .post(format!("{url}/api/v1/zones/{zone_id}/{endpoint}"))
                .send()
                .await;
        }
        ShortcutAction::VolUp | ShortcutAction::VolDown => {
            if let Some(data) = fetch_widget_data(&http, &url, zone_id).await {
                // widget/data reports volume as 0..1 or 0..100 depending on zone.
                let vol = data["volume"].as_f64().unwrap_or(0.5);
                let current = if vol > 1.0 { vol as i32 } else { (vol * 100.0) as i32 };
                let delta = if matches!(action, ShortcutAction::VolUp) { 5 } else { -5 };
                let next = (current + delta).clamp(0, 100);
                let _ = http
                    .put(format!("{url}/api/v1/zones/{zone_id}/volume"))
                    .json(&serde_json::json!({ "volume": next }))
                    .send()
                    .await;
            }
        }
    }
}

fn main() {
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

    tauri::Builder::default()
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    let action = SHORTCUTS.iter().find_map(|(spec, action)| {
                        spec.parse::<Shortcut>()
                            .ok()
                            .filter(|parsed| parsed == shortcut)
                            .map(|_| *action)
                    });
                    if let Some(action) = action {
                        let app = app.clone();
                        tauri::async_runtime::spawn(run_shortcut_action(app, action));
                    }
                })
                .build(),
        )
        .plugin(tauri_plugin_shell::init())
        .manage({
            let mut saved_url = "http://localhost:8888".to_string();
            if let Some(dir) = dirs::config_dir() {
                let cfg_file = dir.join("tune-widget").join("config.json");
                if let Ok(data) = std::fs::read_to_string(&cfg_file) {
                    if let Ok(cfg) = serde_json::from_str::<serde_json::Value>(&data) {
                        if let Some(url) = cfg["server_url"].as_str() {
                            saved_url = url.to_string();
                        }
                    }
                }
            }
            AppState {
                server_url: RwLock::new(saved_url),
                active_zone_id: RwLock::new(1),
                http: reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(10))
                    .build()
                    .unwrap(),
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_zones,
            play_pause,
            next_track,
            prev_track,
            set_volume,
            select_zone,
            search,
            play_item,
            set_server_url,
            get_server_url,
            http_get,
            http_post,
            http_put,
        ])
        .setup(|app| {
            // Hide from Dock on macOS (accessory app)
            #[cfg(target_os = "macos")]
            {
                app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            }

            // Tray menu
            let quit = MenuItem::with_id(app, "quit", "Quitter", true, None::<&str>)?;
            let open_web =
                MenuItem::with_id(app, "open_web", "Ouvrir Tune Web", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_web, &quit])?;

            let tray_icon_bytes = include_bytes!("../icons/tray-icon.png");
            let tray_img = tauri::image::Image::from_bytes(tray_icon_bytes).expect("tray icon");

            let _tray = TrayIconBuilder::new()
                .icon(tray_img)
                .icon_as_template(false)
                .tooltip("Tune Widget")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "quit" => app.exit(0),
                    "open_web" => {
                        let _ = tauri_plugin_shell::ShellExt::shell(app)
                            .open("http://localhost:8888", None);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(win) = app.get_webview_window("main") {
                            if win.is_visible().unwrap_or(false) {
                                let _ = win.hide();
                            } else {
                                let _ = win.show();
                                let _ = win.set_focus();
                            }
                        }
                    }
                })
                .build(app)?;

            // Register the global media-control shortcuts. Failures (e.g. a
            // combo already claimed by another app) are logged, never
            // unwrapped — an unhandled error here previously crashed macOS.
            for (spec, _) in SHORTCUTS {
                match spec.parse::<Shortcut>() {
                    Ok(sc) => {
                        if let Err(e) = app.global_shortcut().register(sc) {
                            tracing::warn!(shortcut = spec, error = %e, "global_shortcut_register_failed");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(shortcut = spec, error = %e, "global_shortcut_parse_failed");
                    }
                }
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error running tune-widget");
}
