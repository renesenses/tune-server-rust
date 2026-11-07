#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod server;
mod volume;
mod vue_mini;

use serde::Serialize;
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager, State,
};
use tokio::sync::RwLock;

struct AppState {
    server_url: RwLock<String>,
    active_zone_id: RwLock<i64>,
    http: reqwest::Client,
}

/// Shape of the `/api/v1/widget/data` payload. Kept as the written record of
/// what the endpoint returns, even though the code currently forwards the raw
/// JSON: deleting it would lose the only place that documents the contract.
#[allow(dead_code)]
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
        // Une fraction, jamais un pourcentage : voir `volume.rs`.
        .json(&volume::corps_lineaire(volume))
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
            "{url}/api/v1/search?q={}&limit=8&sources=local",
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
        // Fusionner, pas écraser : `full_mode` (remember_mode) vit dans le
        // même fichier.
        let mut cfg: serde_json::Value = std::fs::read_to_string(&cfg_file)
            .ok()
            .and_then(|d| serde_json::from_str(&d).ok())
            .filter(|v: &serde_json::Value| v.is_object())
            .unwrap_or_else(|| serde_json::json!({}));
        cfg["server_url"] = serde_json::json!(&clean);
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
    state
        .http
        .get(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn http_post(state: State<'_, AppState>, url: String) -> Result<(), String> {
    state
        .http
        .post(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn http_put(state: State<'_, AppState>, url: String, body: String) -> Result<(), String> {
    state
        .http
        .put(&url)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
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
    /// Bascule mini ↔ complet. Un raccourci global, parce que la fenêtre
    /// complète affiche l'interface du serveur : on ne peut pas y greffer de
    /// bouton depuis le widget, et le menu de la barre système suppose qu'on
    /// sache qu'il existe.
    ToggleMode,
}

const SHORTCUTS: &[(&str, ShortcutAction)] = &[
    ("CmdOrCtrl+Shift+Space", ShortcutAction::PlayPause),
    ("CmdOrCtrl+Shift+Right", ShortcutAction::Next),
    ("CmdOrCtrl+Shift+Left", ShortcutAction::Prev),
    ("CmdOrCtrl+Shift+Up", ShortcutAction::VolUp),
    ("CmdOrCtrl+Shift+Down", ShortcutAction::VolDown),
    ("CmdOrCtrl+Shift+M", ShortcutAction::ToggleMode),
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
        // Seule action qui ne parle pas au serveur : elle doit fonctionner même
        // quand il ne répond pas.
        ShortcutAction::ToggleMode => {
            let full = !full_mode_active(&app);
            set_full_mode(&app, full).await;
        }
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
            // Le gain et la courbe de la zone sont dans `GET /zones`, pas à la
            // racine de `/widget/data` (voir `volume::zone_dans`). Sans eux,
            // on ne touche à rien plutôt que de repartir d'une valeur inventée.
            let zones = match http.get(format!("{url}/api/v1/zones")).send().await {
                Ok(r) => r.json::<serde_json::Value>().await.ok(),
                Err(_) => None,
            };
            let Some((gain, courbe)) = zones.as_ref().and_then(|z| volume::zone_dans(z, zone_id))
            else {
                tracing::info!(zone_id, "shortcut_volume_zone_unknown");
                return;
            };
            let delta = if matches!(action, ShortcutAction::VolUp) {
                5
            } else {
                -5
            };
            let next = volume::apres_un_pas(gain, courbe, delta);
            let _ = http
                .put(format!("{url}/api/v1/zones/{zone_id}/volume"))
                .json(&serde_json::json!({ "volume": next }))
                .send()
                .await;
        }
    }
}

/// True if an HTTP server answers at `url` (any response, even an error status,
/// means the port is up).
async fn server_reachable(http: &reqwest::Client, url: &str) -> bool {
    http.get(url)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        .is_ok()
}

/// Ensure a tune-server is running. If nothing answers on the configured URL,
/// spawn the bundled `tune-server` sidecar (Windows installer ships it) and wait
/// up to ~30 s for it to come up. On a macOS dev build with no bundled sidecar,
/// the spawn fails gracefully and the widget just uses a separately-started
/// server.
async fn ensure_server_running(app: tauri::AppHandle) {
    use tauri_plugin_shell::ShellExt;
    let http = app.state::<AppState>().http.clone();
    let url = app.state::<AppState>().server_url.read().await.clone();
    if server_reachable(&http, &url).await {
        return;
    }
    match app.shell().sidecar("tune-server").and_then(|c| c.spawn()) {
        Ok(_) => tracing::info!("tune_server_sidecar_spawned"),
        Err(e) => {
            tracing::warn!(error = %e, "tune_server_sidecar_unavailable");
            return;
        }
    }
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if server_reachable(&http, &url).await {
            tracing::info!("tune_server_ready");
            return;
        }
    }
    tracing::warn!("tune_server_start_timeout");
}

/// Ouvre un journal sur disque, à côté de la configuration.
///
/// Sans lui, `tracing` était déclaré mais aucun abonné n'était installé : tous
/// les messages du programme partaient à la poubelle. Un échec au démarrage —
/// typiquement le WebView2 absent sous Windows, qui laisse le processus vivant
/// sans jamais ouvrir de fenêtre — ne laissait donc aucune trace, et le testeur
/// n'avait rien à envoyer (retour Sandro).
///
/// Renvoie le chemin du journal et la garde du writer, qui doit vivre aussi
/// longtemps que le programme sous peine de perdre les derniers messages.
fn init_logging() -> Option<(
    std::path::PathBuf,
    tracing_appender::non_blocking::WorkerGuard,
)> {
    let dir = dirs::config_dir()?.join("tune-widget");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("tune-widget.log");

    // Repart de zéro au-delà de 1 Mo : un widget qui tourne des semaines ne doit
    // pas remplir le disque, et seul le dernier démarrage sert au diagnostic.
    if std::fs::metadata(&path)
        .map(|m| m.len() > 1_000_000)
        .unwrap_or(false)
    {
        std::fs::remove_file(&path).ok();
    }

    let (writer, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::never(&dir, "tune-widget.log"));
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(writer)
        .with_max_level(tracing::Level::INFO)
        .init();

    Some((path, guard))
}

/// Dossiers de cache web à jeter quand la version du widget change.
///
/// L'interface du mini-lecteur est une page servie au moteur de rendu du
/// système ; celui-ci en garde une copie sur disque, que ni la désinstallation
/// ni la réinstallation n'emportaient. Une correction d'interface pouvait donc
/// rester invisible pour toujours — c'est ce qu'a vécu Sandro,
/// qui voyait le bouton ⤢ dans un bac à sable Windows vierge et pas sur sa
/// machine, même après désinstallation puis réinstallation (#1704).
///
/// On ne rend QUE le cache HTTP. Le reste du profil (`Local Storage` en
/// particulier) contient l'adresse du serveur choisie par l'utilisateur, que
/// `app.js` y range sous la clé `tune-server` : la purger le renverrait sur
/// l'adresse en dur du code, c'est-à-dire le réseau de quelqu'un d'autre.
fn webview_http_cache_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs_to_clear = Vec::new();

    // Windows. Aucun `dataDirectory` n'est déclaré dans tauri.conf.json et wry
    // ne pose rien à la place : WebView2 retombe donc sur son emplacement par
    // défaut, documenté par Microsoft comme « le chemin de l'exécutable suivi
    // de .WebView2 » — soit, ici, à l'intérieur même du dossier d'installation.
    // C'est aussi pourquoi le désinstalleur le laisse : il termine par un
    // `RMDir "$INSTDIR"` sans /r, qui échoue en silence sur un dossier non vide.
    // On le déduit de l'exécutable en cours plutôt que de recomposer un chemin
    // à partir du nom du produit : c'est la seule façon de rester juste si
    // l'installation a été déplacée ou renommée.
    #[cfg(target_os = "windows")]
    if let Ok(exe) = std::env::current_exe() {
        let mut udf = exe.into_os_string();
        udf.push(".WebView2");
        let ebwebview = std::path::PathBuf::from(udf).join("EBWebView");
        // Un profil par sous-dossier (« Default » sauf configuration
        // contraire) : on les balaie tous plutôt que de parier sur le nom.
        if let Ok(entries) = std::fs::read_dir(&ebwebview) {
            for entry in entries.flatten() {
                let profile = entry.path();
                if profile.is_dir() {
                    dirs_to_clear.push(profile.join("Cache"));
                    dirs_to_clear.push(profile.join("Code Cache"));
                }
            }
        }
    }

    // macOS. WKWebView range son cache disque sous ~/Library/Caches/<identifiant
    // du paquet>, à côté — et non à l'intérieur — des données de site, qui
    // vivent dans ~/Library/WebKit. Le piège y est donc le même que sous
    // Windows (rien n'est effacé en déplaçant l'application à la corbeille), et
    // le dossier ne contient que du cache : le vider ne coûte rien.
    #[cfg(target_os = "macos")]
    if let Some(caches) = dirs::cache_dir() {
        dirs_to_clear.push(caches.join("fr.mozaiklabs.tune-widget"));
    }

    dirs_to_clear
}

/// Le cache doit-il être jeté, au vu du témoin laissé par le dernier démarrage ?
///
/// Sans témoin, oui : c'est le cas de l'utilisateur qui installe cette version
/// par-dessus une ancienne, donc exactement celui qu'on cherche à rattraper.
fn cache_is_stale(seen: Option<&str>, current: &str) -> bool {
    seen.map(str::trim) != Some(current)
}

/// Jette le cache web dès que la version diffère de celle du dernier démarrage.
///
/// À appeler AVANT que Tauri ne construise la fenêtre : une fois le moteur de
/// rendu démarré, les fichiers du cache sont ouverts et ne peuvent plus être
/// supprimés.
///
/// Le témoin de version est rangé à côté de la configuration et du journal,
/// dans un dossier que ni la mise à jour ni la purge ne touchent — sinon la
/// purge se redéclencherait à chaque démarrage.
fn purge_webview_cache_on_version_change() {
    let Some(dir) = dirs::config_dir().map(|d| d.join("tune-widget")) else {
        return;
    };
    let stamp = dir.join("webview-cache-version");
    let current = env!("CARGO_PKG_VERSION");

    if !cache_is_stale(std::fs::read_to_string(&stamp).ok().as_deref(), current) {
        return;
    }

    for path in webview_http_cache_dirs() {
        match std::fs::remove_dir_all(&path) {
            Ok(()) => tracing::info!(cache = %path.display(), "webview_cache_purged"),
            // Absent = rien à faire ; c'est le cas courant, pas une anomalie.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(cache = %path.display(), error = %e, "webview_cache_purge_failed")
            }
        }
    }

    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(&stamp, current);
    }
}

/// Bascule mini ↔ complet depuis l'interface elle-même.
///
/// Le menu de la barre système la proposait déjà, mais il faut savoir qu'il
/// existe : le seul endroit où l'utilisateur regarde, c'est la fenêtre qu'il a
/// sous les yeux (Sandro).
#[tauri::command]
async fn set_display_mode(app: tauri::AppHandle, full: bool) -> Result<(), String> {
    set_full_mode(&app, full).await;
    Ok(())
}

/// Le client web servi par ce serveur affiche-t-il la vue mini ?
///
/// La réponse choisit entre une fenêtre et deux. C'est une vraie question posée
/// au serveur en marche, pas une supposition : Sandro pointe son widget vers un
/// GentooPlayer sur une autre machine, mis à jour à son rythme.
///
/// On ne sonde plus la VERSION du serveur. Le seuil 0.9.64 désignait
/// `MiniPlayer.svelte`, parti avec l'ancienne interface le 19/09/2026 : tout
/// serveur de 0.9.64 à la rc9 répondait « oui » et la fenêtre de 380 px
/// recevait l'interface complète. Le client web annonce désormais lui-même la
/// vue mini par `vue-mini.json` (voir `vue_mini::annonce_vue_mini`).
///
/// Tout ce qui n'est pas clair — injoignable, 404, `index.html` — répond
/// `false` et garde l'interface embarquée : le repli doit être ce qui marche.
async fn server_has_mini_layout(http: &reqwest::Client, url: &str) -> bool {
    let sonde = format!("{}/vue-mini.json", url.trim_end_matches('/'));
    let corps = match http
        .get(&sonde)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
        Ok(r) => {
            tracing::info!(url = %sonde, status = %r.status(), "mini_layout_probe_absent_keeping_two_windows");
            return false;
        }
        Err(e) => {
            tracing::info!(url = %sonde, error = %e, "mini_layout_probe_failed_keeping_two_windows");
            return false;
        }
    };
    let supported = vue_mini::annonce_vue_mini(&corps);
    tracing::info!(url = %sonde, supported, "mini_layout_probe");
    supported
}

/// Point the single window at one layout or the other.
///
/// Returns `false` when the unified path is not available, so the caller can
/// fall back to the two-window behaviour.
async fn navigate_unified(app: &tauri::AppHandle, full: bool) -> bool {
    let (url, http) = {
        let state = app.state::<AppState>();
        let url = state.server_url.read().await.clone();
        (url, state.http.clone())
    };
    if !server_has_mini_layout(&http, &url).await {
        return false;
    }
    let target = if full {
        url.clone()
    } else {
        vue_mini::adresse_mini(&url)
    };
    let parsed = match target.parse() {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(url = %target, error = %e, "unified_bad_url");
            return false;
        }
    };
    // Reuse the "full" window as THE window: it already exists with the right
    // close-to-hide behaviour, and it is the one that talks to the server.
    open_full_window(app).await;
    let Some(win) = app.get_webview_window("full") else {
        return false;
    };
    if let Err(e) = win.navigate(parsed) {
        tracing::warn!(url = %target, error = %e, "unified_navigate_failed");
        return false;
    }
    // The two layouts want very different windows. Mini stays on top and out of
    // the taskbar, like the widget it replaces; full behaves like an app.
    if full {
        // Meme borne que a l'ouverture : sans elle, repasser en grand rendait
        // a la fenetre les 800 points de haut qui ne tiennent pas sur un
        // bureau de 720 (#1598).
        let (work_area, scale) = active_work_area(app);
        let (w, h) = fit_to_work_area(work_area, scale, FULL_WINDOW_SIZE, FULL_WINDOW_MIN_SIZE);
        let _ = win.set_max_size(None::<tauri::Size>);
        let _ = win.set_min_size(Some(tauri::LogicalSize::new(
            FULL_WINDOW_MIN_SIZE.0,
            FULL_WINDOW_MIN_SIZE.1,
        )));
        let _ = win.set_size(tauri::LogicalSize::new(w, h));
        let _ = win.set_always_on_top(false);
    } else {
        // 🔴 La borne MINIMALE d'abord : la fenêtre est construite avec celle
        // de la fenêtre complète (900 × 600), et `set_size` ne descend pas
        // en dessous. La fenêtre « mini » faisait donc 900 × 600.
        let _ = win.set_min_size(Some(tauri::LogicalSize::new(
            vue_mini::MINI_MIN.0,
            vue_mini::MINI_MIN.1,
        )));
        let _ = win.set_max_size(Some(tauri::LogicalSize::new(
            vue_mini::MINI_LARGEUR_MAX,
            vue_mini::MINI_HAUTEUR_MAX,
        )));
        let (w, h) = taille_mini(lire_cfg_bool("mini_replie", false));
        let _ = win.set_size(tauri::LogicalSize::new(w, h));
        let _ = win.set_always_on_top(lire_cfg_bool("mini_epingle", true));
    }
    let _ = win.set_resizable(true);
    let _ = win.set_skip_taskbar(!full);
    let _ = win.set_decorations(true);
    let _ = win.show();
    let _ = win.set_focus();
    // The bundled mini window has no reason to exist in this mode.
    if let Some(mini) = app.get_webview_window("main") {
        let _ = mini.hide();
    }
    tracing::info!(full, "unified_window_mode");
    true
}

/// Switch between the two modes without ever reloading either one.
///
/// Both windows stay alive: showing one hides the other. That is the whole
/// point of keeping two windows at this stage — morphing a single one would
/// mean navigating between the bundled mini interface and the server's URL,
/// and every toggle would cost a reload and lose scroll, search and view.
/// (Once the mini player becomes a layout of the web client, one window that
/// resizes becomes the better design — that is step 3, not this one.)
async fn set_full_mode(app: &tauri::AppHandle, full: bool) {
    // Step 4: one window that switches layout, when the server can serve both.
    // Falls back to the two-window design below on an older server.
    if navigate_unified(app, full).await {
        remember_mode(full);
        return;
    }
    if full {
        open_full_window(app).await;
        if let Some(mini) = app.get_webview_window("main") {
            let _ = mini.hide();
        }
    } else {
        if let Some(win) = app.get_webview_window("full") {
            let _ = win.hide();
        }
        if let Some(mini) = app.get_webview_window("main") {
            let _ = mini.show();
            let _ = mini.set_focus();
        }
    }
    remember_mode(full);
}

/// Which window a tray left-click should act on: the one the user last chose.
fn full_mode_active(app: &tauri::AppHandle) -> bool {
    app.get_webview_window("full")
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false)
}

fn config_file() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("tune-widget").join("config.json"))
}

/// Lit un booléen de `config.json` (`mini_epingle`, `mini_replie`…).
fn lire_cfg_bool(cle: &str, defaut: bool) -> bool {
    config_file()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|d| serde_json::from_str::<serde_json::Value>(&d).ok())
        .and_then(|c| c[cle].as_bool())
        .unwrap_or(defaut)
}

/// Écrit un booléen dans `config.json` en FUSIONNANT : `server_url` et
/// `full_mode` vivent dans le même fichier.
fn ecrire_cfg_bool(cle: &str, valeur: bool) {
    let Some(file) = config_file() else {
        return;
    };
    let mut cfg: serde_json::Value = std::fs::read_to_string(&file)
        .ok()
        .and_then(|d| serde_json::from_str(&d).ok())
        .filter(|v: &serde_json::Value| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    cfg[cle] = serde_json::json!(valeur);
    if let Some(dir) = file.parent() {
        if std::fs::create_dir_all(dir).is_ok() {
            let _ = std::fs::write(&file, cfg.to_string());
        }
    }
}

/// Taille de la fenêtre mini selon qu'elle est repliée (maquette C) ou non.
fn taille_mini(replie: bool) -> (f64, f64) {
    if replie {
        vue_mini::MINI_REPLIE
    } else {
        vue_mini::MINI_DEPLIE
    }
}

/// Exécute un geste natif demandé par la page (`/__widget/<action>`).
///
/// La page ne parle à la fenêtre QUE par ces quatre adresses, que
/// `vue_mini::decider_navigation` n'accepte que depuis le serveur configuré.
/// Aucune commande Tauri n'est ouverte à la page distante.
async fn executer_action(app: tauri::AppHandle, action: vue_mini::ActionWidget) {
    use vue_mini::ActionWidget;
    let Some(win) = app.get_webview_window("full") else {
        return;
    };
    match action {
        ActionWidget::Epingle => {
            let epingle = !lire_cfg_bool("mini_epingle", true);
            let _ = win.set_always_on_top(epingle);
            ecrire_cfg_bool("mini_epingle", epingle);
            tracing::info!(epingle, "mini_epingle");
        }
        ActionWidget::Replier | ActionWidget::Deplier => {
            let replie = matches!(action, ActionWidget::Replier);
            let (w, h) = taille_mini(replie);
            let _ = win.set_size(tauri::LogicalSize::new(w, h));
            ecrire_cfg_bool("mini_replie", replie);
            tracing::info!(replie, "mini_replie");
        }
        ActionWidget::Complet => set_full_mode(&app, true).await,
    }
}

/// Décide d'une navigation de la fenêtre qui charge le serveur, et exécute ce
/// qui doit l'être. `true` = le moteur charge l'adresse.
fn garder_navigation(app: &tauri::AppHandle, cible: &tauri::Url) -> bool {
    // Lecture non bloquante : on est sur le fil de l'interface. Le verrou n'est
    // tenu qu'un instant, le temps d'écrire une adresse ; s'il l'est, on
    // refuse plutôt que de charger sans savoir ce qu'est le serveur.
    let serveur = match app.state::<AppState>().server_url.try_read() {
        Ok(s) => s.clone(),
        Err(_) => return false,
    };
    match vue_mini::decider_navigation(&serveur, cible) {
        vue_mini::Decision::Autoriser => true,
        vue_mini::Decision::Action(action) => {
            let app = app.clone();
            tauri::async_runtime::spawn(executer_action(app, action));
            false
        }
        vue_mini::Decision::Externe => {
            #[allow(deprecated)]
            {
                use tauri_plugin_shell::ShellExt;
                if let Err(e) = app.shell().open(cible.as_str(), None) {
                    tracing::warn!(url = %cible, error = %e, "lien_externe_non_ouvert");
                }
            }
            false
        }
        vue_mini::Decision::Refuser => {
            tracing::warn!(url = %cible, "navigation_refusee");
            false
        }
    }
}

/// Persist the chosen mode next to the server URL, so a restart comes back the
/// way it was left rather than always in mini.
fn remember_mode(full: bool) {
    let Some(dir) = dirs::config_dir() else {
        return;
    };
    let dir = dir.join("tune-widget");
    let file = dir.join("config.json");
    let mut cfg: serde_json::Value = std::fs::read_to_string(&file)
        .ok()
        .and_then(|d| serde_json::from_str(&d).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    cfg["full_mode"] = serde_json::json!(full);
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(&file, cfg.to_string());
    }
}

fn saved_full_mode() -> bool {
    dirs::config_dir()
        .map(|d| d.join("tune-widget").join("config.json"))
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|d| serde_json::from_str::<serde_json::Value>(&d).ok())
        .and_then(|c| c["full_mode"].as_bool())
        .unwrap_or(false)
}

/// Taille souhaitee de la grande fenetre, en points logiques.
const FULL_WINDOW_SIZE: (f64, f64) = (1200.0, 800.0);
/// Taille en deca de laquelle la grande fenetre ne descend pas.
const FULL_WINDOW_MIN_SIZE: (f64, f64) = (900.0, 600.0);
/// Marge laissee autour de la fenetre dans la zone de travail de l'ecran.
const WORK_AREA_MARGIN: f64 = 48.0;

/// Ramener une taille voulue a ce que l'ecran peut reellement afficher.
///
/// La grande fenetre demandait 1200x800 points quoi qu'il arrive. Sur un
/// 1920x1080 regle a 150 % — la mise a l'echelle Windows la plus repandue —
/// le bureau n'offre que 1280x720 points : les 800 de hauteur debordent et le
/// bas de l'interface passe sous le bord de l'ecran. C'est la seconde moitie
/// de #1598, celle que le zoom seul ne reglait pas.
///
/// `work_area` arrive en pixels physiques alors que `inner_size` et `set_size`
/// parlent en points logiques : d'ou la division par le facteur d'echelle.
///
/// La taille minimale de la fenetre l'emporte sur la contrainte d'ecran : sur
/// un ecran plus petit qu'elle, mieux vaut une fenetre deplacable qu'une
/// fenetre ecrasee.
fn fit_to_work_area(
    work_area: Option<(u32, u32)>,
    scale: f64,
    want: (f64, f64),
    min: (f64, f64),
) -> (f64, f64) {
    let Some((width, height)) = work_area else {
        return want;
    };
    if !scale.is_finite() || scale <= 0.0 {
        return want;
    }
    let available_w = f64::from(width) / scale - WORK_AREA_MARGIN;
    let available_h = f64::from(height) / scale - WORK_AREA_MARGIN;
    (
        want.0.min(available_w.max(min.0)),
        want.1.min(available_h.max(min.1)),
    )
}

/// Zone de travail de l'ecran ou la fenetre va s'ouvrir, a defaut l'ecran
/// principal. `None` quand aucun ecran n'est identifiable : on garde alors la
/// taille voulue plutot que d'inventer une contrainte.
fn active_work_area(app: &tauri::AppHandle) -> (Option<(u32, u32)>, f64) {
    let monitor = app
        .get_webview_window("full")
        .or_else(|| app.get_webview_window("main"))
        .and_then(|win| win.current_monitor().ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten());
    match monitor {
        Some(monitor) => {
            let area = monitor.work_area();
            (
                Some((area.size.width, area.size.height)),
                monitor.scale_factor(),
            )
        }
        None => (None, 1.0),
    }
}

/// Open (or focus) the full Tune interface in a real application window.
///
/// The interface is NOT bundled: the window loads the server's own web UI over
/// http. That is deliberate — bundling would mean republishing the app on every
/// web change and, worse, shipping a UI that can drift from the API of the
/// server it drives. Sandro points his widget at a GentooPlayer on another
/// machine, so that drift is a certainty, not a risk.
///
/// It also replaces what this menu entry used to do: hand the URL to the
/// system browser, hardcoded to `localhost:8888` — the wrong machine entirely
/// for anyone whose server is not the desktop.
async fn open_full_window(app: &tauri::AppHandle) {
    use tauri::{WebviewUrl, WebviewWindowBuilder};

    if let Some(win) = app.get_webview_window("full") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
        return;
    }

    let url = {
        let state = app.state::<AppState>();
        let guard = state.server_url.read().await;
        guard.clone()
    };
    let parsed = match url.parse() {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(url = %url, error = %e, "full_window_bad_server_url");
            return;
        }
    };

    let (work_area, scale) = active_work_area(app);
    let (width, height) =
        fit_to_work_area(work_area, scale, FULL_WINDOW_SIZE, FULL_WINDOW_MIN_SIZE);

    match WebviewWindowBuilder::new(app, "full", WebviewUrl::External(parsed))
        .title("Tune")
        .inner_size(width, height)
        .min_inner_size(FULL_WINDOW_MIN_SIZE.0, FULL_WINDOW_MIN_SIZE.1)
        .resizable(true)
        .decorations(true)
        // Ctrl/Cmd + molette et Ctrl/Cmd + « - = + ». Sans cet appel, wry passe
        // `false` a `ICoreWebView2Settings::IsZoomControlEnabled` : sous Windows
        // la molette est morte, exactement ce que decrit #1598. Sur macOS et
        // Linux, c'est ce meme drapeau qui fait injecter par Tauri le script
        // equivalent (`plugin:webview|set_webview_zoom`), d'ou la capacite
        // `full-window` qui autorise cette commande sur cette fenetre.
        .zoom_hotkeys_enabled(true)
        // La fenêtre charge le serveur configuré, et RIEN d'autre : les liens
        // externes partent dans le navigateur, les gestes `/__widget/…` sont
        // exécutés sans être chargés (voir `vue_mini::decider_navigation`).
        .on_navigation({
            let app = app.clone();
            move |cible| garder_navigation(&app, cible)
        })
        .build()
    {
        Ok(win) => {
            // Closing hides instead of destroying: the web interface stays
            // loaded, so coming back is instant and nothing is lost. Quitting
            // is the tray menu's job, as it already was.
            let handle = win.clone();
            win.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = handle.hide();
                }
            });
            tracing::info!(url = %url, "full_window_opened");
        }
        Err(e) => tracing::error!(url = %url, error = %e, "full_window_open_failed"),
    }
}

fn main() {
    use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

    // En tout premier : ce qui échoue ensuite doit laisser une trace.
    // La garde est conservée jusqu'à la fin de main, sinon les derniers
    // messages seraient perdus à l'extinction.
    let _log_guard = init_logging().map(|(path, guard)| {
        tracing::info!(
            version = env!("CARGO_PKG_VERSION"),
            journal = %path.display(),
            "tune_widget_starting"
        );
        guard
    });

    // Avant toute construction de fenêtre : le moteur de rendu verrouille les
    // fichiers de son cache dès qu'il démarre.
    purge_webview_cache_on_version_change();

    let run = tauri::Builder::default()
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
            // L'URL retenue explique à elle seule le cas « le widget a lancé un
            // serveur local » : ensure_server_running ne démarre le sidecar que
            // si cette adresse ne répond pas.
            tracing::info!(server_url = %saved_url, "server_url_resolved");
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
            set_display_mode,
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
                MenuItem::with_id(app, "open_web", "Fenêtre complète", true, None::<&str>)?;
            let open_mini =
                MenuItem::with_id(app, "open_mini", "Mini-lecteur", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_web, &open_mini, &quit])?;

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
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            ensure_server_running(app.clone()).await;
                            set_full_mode(&app, true).await;
                        });
                    }
                    "open_mini" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            set_full_mode(&app, false).await;
                        });
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    // `button_state` matters: a tray Click fires TWICE per
                    // physical click — once on press, once on release. Matching
                    // both ran this handler twice, so the window was shown and
                    // hidden again within the same click and nothing appeared.
                    // Clicking repeatedly "worked" only when the timing left it
                    // on the visible half (Sandro, 9 Aug 2026: "je dois cliquer
                    // de façon répétée et frénétique"). Acting on release only
                    // gives exactly one toggle per click.
                    if let tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } = event
                    {
                        // Act on the window of the CURRENT mode: someone who
                        // chose the full window expects the tray icon to bring
                        // that back, not the mini player they left behind.
                        let app = tray.app_handle();
                        let label = if full_mode_active(app) { "full" } else { "main" };
                        let visible = app
                            .get_webview_window(label)
                            .and_then(|w| w.is_visible().ok())
                            .unwrap_or(false);
                        if visible {
                            if let Some(win) = app.get_webview_window(label) {
                                let _ = win.hide();
                            }
                        } else {
                            // Showing goes through set_full_mode rather than
                            // straight to the window: that is where the server
                            // is asked whether it can serve both layouts. A
                            // widget started before its server was up therefore
                            // still lands on the unified window at the first
                            // click, instead of being stuck on the bundled mini
                            // until the next toggle.
                            let h = app.clone();
                            let full = saved_full_mode();
                            tauri::async_runtime::spawn(async move {
                                set_full_mode(&h, full).await;
                            });
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

            // Come back in the mode the user left, rather than always in mini.
            if saved_full_mode() {
                let h = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    ensure_server_running(h.clone()).await;
                    set_full_mode(&h, true).await;
                });
            }

            // Auto-start the server on launch so opening the widget = Tune runs
            // (single-installer launcher UX on Windows). No-op if already up.
            tauri::async_runtime::spawn(ensure_server_running(app.handle().clone()));

            Ok(())
        })
        .run(tauri::generate_context!());

    // Un échec ici est le cas qu'on cherchait justement à instrumenter : sous
    // Windows, une fenêtre qui ne s'ouvre pas vient presque toujours du runtime
    // WebView2 absent. `expect` paniquait vers une sortie d'erreur qu'une
    // application graphique n'affiche à personne.
    if let Err(e) = run {
        tracing::error!(
            error = %e,
            "widget_run_failed : impossible de démarrer l'interface. Sous Windows, \
             vérifier que « Microsoft Edge WebView2 Runtime » est installé."
        );
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        cache_is_stale, fit_to_work_area, taille_mini, FULL_WINDOW_MIN_SIZE, FULL_WINDOW_SIZE,
    };

    #[test]
    fn la_grande_fenetre_ne_depasse_plus_le_bureau() {
        // 1920x1080 a 100 % : il y a la place, on ne rabote rien.
        assert_eq!(
            fit_to_work_area(
                Some((1920, 1040)),
                1.0,
                FULL_WINDOW_SIZE,
                FULL_WINDOW_MIN_SIZE
            ),
            (1200.0, 800.0)
        );
        // Le cas de Sandro : 1920x1080 a 150 %, soit 1280x720 points une fois
        // la barre des taches deduite. Les 800 points de haut ne tiennent pas.
        let (w, h) = fit_to_work_area(
            Some((1920, 1040)),
            1.5,
            FULL_WINDOW_SIZE,
            FULL_WINDOW_MIN_SIZE,
        );
        assert!(
            h < 1040.0 / 1.5,
            "la hauteur doit tenir dans les {} points du bureau, obtenu {h}",
            1040.0 / 1.5
        );
        assert!((w - 1200.0).abs() < 0.001, "largeur inattendue : {w}");
        assert!(
            (h - (1040.0 / 1.5 - 48.0)).abs() < 0.001,
            "hauteur inattendue : {h}"
        );
        // Ecran plus petit que la taille minimale de la fenetre : la minimale
        // gagne, on ne fabrique pas une fenetre ecrasee.
        assert_eq!(
            fit_to_work_area(
                Some((1024, 640)),
                1.0,
                FULL_WINDOW_SIZE,
                FULL_WINDOW_MIN_SIZE
            ),
            (976.0, 600.0)
        );
        // Aucun ecran identifiable, ou facteur d'echelle aberrant : on garde la
        // taille voulue plutot que d'inventer une contrainte.
        assert_eq!(
            fit_to_work_area(None, 1.0, FULL_WINDOW_SIZE, FULL_WINDOW_MIN_SIZE),
            FULL_WINDOW_SIZE
        );
        assert_eq!(
            fit_to_work_area(
                Some((1920, 1040)),
                0.0,
                FULL_WINDOW_SIZE,
                FULL_WINDOW_MIN_SIZE
            ),
            FULL_WINDOW_SIZE
        );
    }

    #[test]
    fn the_cache_is_kept_only_for_the_version_that_wrote_it() {
        // Même version : on ne touche à rien, sinon la purge se redéclencherait
        // à chaque démarrage.
        assert!(!cache_is_stale(Some("0.1.4"), "0.1.4"));
        // Le témoin est écrit sans retour à la ligne, mais un éditeur ou une
        // copie manuelle peut en ajouter un : ce n'est pas un changement de
        // version.
        assert!(!cache_is_stale(Some("0.1.4\n"), "0.1.4"));
        // Version différente : c'est le cas d'une mise à jour.
        assert!(cache_is_stale(Some("0.1.3"), "0.1.4"));
        // Aucun témoin : première exécution après l'arrivée de ce correctif,
        // donc très probablement au-dessus d'un cache périmé (#1704).
        assert!(cache_is_stale(None, "0.1.4"));
        // Témoin illisible ou tronqué : on purge plutôt que de parier.
        assert!(cache_is_stale(Some(""), "0.1.4"));
    }

    #[test]
    fn la_fenetre_mini_se_replie_en_bandeau() {
        assert_eq!(taille_mini(false), (360.0, 560.0));
        assert_eq!(taille_mini(true), (360.0, 140.0));
    }
}
