use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    let router = Router::new()
        .route("/", get(list_plugins))
        .route("/docs", get(plugin_docs))
        .route("/{name}", get(get_plugin))
        .route("/{name}", axum::routing::delete(delete_plugin))
        .route("/{name}/enable", post(enable_plugin))
        .route("/{name}/disable", post(disable_plugin))
        .route("/{name}/install", post(install_plugin))
        .route("/{name}/update", post(update_plugin));

    // P2 of the plugin ABI (RFC §3.5): a single catch-all that dispatches
    // `/api/v1/plugins/{id}/{*path}` into the loaded wasm plugin. Gated behind
    // `plugins-wasm`, so the default server never mounts it. The param is named
    // `name` to match the routes above (matchit rejects two differently-named
    // params at the same position); the static `/{name}/enable` etc. still win
    // over this catch-all for their exact paths.
    #[cfg(feature = "plugins-wasm")]
    let router = router.route("/{name}/{*path}", get(wasm_dispatch).post(wasm_dispatch));

    router
}

/// Dispatch an HTTP request to a loaded wasm plugin (P2, RFC §3.5).
///
/// Packages the request as `{method, path, query, body}` JSON, runs the plugin
/// off the async runtime (its wasmtime `Store` is not `Sync`, and any
/// host-function it triggers must block on the runtime from a non-worker
/// thread), and returns the plugin's `{status, headers?, body}` envelope. A
/// premium-marked plugin is gated with the existing `premium_guard` first.
#[cfg(feature = "plugins-wasm")]
async fn wasm_dispatch(
    State(state): State<AppState>,
    method: axum::http::Method,
    Path((id, subpath)): Path<(String, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let Some(registry) = state.wasm_plugins.get() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "wasm plugins not loaded" })),
        )
            .into_response();
    };

    let Some(loaded) = registry.get(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "plugin not found", "id": id })),
        )
            .into_response();
    };

    // Premium gate BEFORE dispatch: the host owns licensing, not the plugin
    // (RFC §3.5). Reuse the same guard the native premium routes use.
    if loaded.manifest.premium {
        if let Err(resp) = crate::premium_guard::require_premium(
            &state.license,
            tune_core::license::Feature::PluginMarketplace,
        )
        .await
        {
            return resp;
        }
    }

    let body_json: Value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap_or(Value::Null)
    };
    let req = json!({
        "method": method.as_str(),
        "path": format!("/{subpath}"),
        "query": query.unwrap_or_default(),
        "body": body_json,
    });
    let req_str = req.to_string();

    // The wasm call (and every host-function it triggers) runs on a blocking
    // thread: the Store isn't Sync — serialise per plugin via its Mutex — and
    // the host's async capabilities `block_on` the runtime, which is only sound
    // off a runtime worker.
    let wasm_plugins = state.wasm_plugins.clone();
    let plugin_id = id.clone();
    let call = tokio::task::spawn_blocking(move || {
        let registry = wasm_plugins.get().expect("registry present");
        let loaded = registry.get(&plugin_id).expect("plugin present");
        let mut plugin = loaded.plugin.blocking_lock();
        plugin.handle_route(&req_str)
    })
    .await;

    let resp_str = match call {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "plugin_error", "message": e })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "plugin_task_failed", "message": e.to_string() })),
            )
                .into_response();
        }
    };

    let parsed: Value = match serde_json::from_str(&resp_str) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "plugin_bad_response", "message": e.to_string() })),
            )
                .into_response();
        }
    };

    let status = parsed
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|c| u16::try_from(c).ok())
        .and_then(|c| StatusCode::from_u16(c).ok())
        .unwrap_or(StatusCode::OK);
    let out_body = parsed.get("body").cloned().unwrap_or(Value::Null);
    (status, Json(out_body)).into_response()
}

async fn list_plugins(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut plugins: Vec<Value> = settings
        .get("plugins")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    // Built-in plugins
    let xtune_dir = std::env::var("TUNE_XTUNE_DIR").unwrap_or_else(|_| "xtune-web".into());
    let xtune_installed = std::path::Path::new(&xtune_dir).exists();
    plugins.push(serde_json::json!({
        "name": "xtune",
        "display_name": "xTune",
        "description": "Vinyl turntable player — interface platine vinyle immersive",
        "version": "1.0.0",
        "author": "MozAIk Labs",
        "type": "built-in",
        "installed": xtune_installed,
        "enabled": xtune_installed,
        "url": "/xtune/",
        "icon": "vinyl",
    }));

    // Plugins actually loaded through the SDK. These are the only entries
    // backed by running code — everything above is settings bookkeeping.
    //
    // Read from the snapshot `plugins::init` published, never from the loader:
    // event dispatch holds the loader's lock across every plugin's `on_event`,
    // so reaching for it here would let one slow plugin hang this endpoint.
    for info in plugin_snapshot(&state) {
        plugins.push(serde_json::json!({
            "name": info.name,
            "display_name": info.name,
            "description": info.description,
            "version": info.version,
            "type": "sdk",
            "installed": true,
            "enabled": info.enabled,
            "url": format!("/api/v1/ext/{}", info.name),
            "config_schema": info.config_schema,
        }));
    }

    // Compiled-in plugins that did not load: opt-in ones the user has not
    // installed (DJ/Karaoke), or default-on ones they disabled. Listed from
    // the snapshot `plugins::init` published so they stay visible in the
    // manager — an opt-in one as `installed:false` (→ "Install"), a disabled
    // one as `installed:true, enabled:false` (→ "Enable").
    for info in plugin_available_snapshot(&state) {
        let installed = if info.opt_in {
            settings
                .get(&format!("plugin_{}_installed", info.name))
                .ok()
                .flatten()
                .as_deref()
                == Some("true")
        } else {
            true
        };
        plugins.push(serde_json::json!({
            "name": info.name,
            "display_name": info.name,
            "description": info.description,
            "version": info.version,
            "type": "sdk",
            "installed": installed,
            "enabled": false,
            "loaded": false,
            "url": format!("/api/v1/ext/{}", info.name),
            "config_schema": info.config_schema,
        }));
    }

    // Wasm plugins installed on disk (marketplace installs or bundled).
    // Scanned from the plugins dir rather than the loaded registry so a
    // disabled plugin — or one installed since the last restart — still shows
    // up and can be toggled. `loaded` distinguishes running from
    // pending-restart.
    if let Some(dir) = crate::plugins::wasm_plugins_dir() {
        let manager = tune_core::plugins::PluginManager::new(dir);
        if let Ok(infos) = manager.scan().await {
            for info in infos {
                let id = info.manifest.id.clone();
                let enabled = settings
                    .get(&format!("plugin_{id}_enabled"))
                    .ok()
                    .flatten()
                    .map(|v| v != "false")
                    .unwrap_or(true);
                #[cfg(feature = "plugins-wasm")]
                let loaded = state
                    .wasm_plugins
                    .get()
                    .is_some_and(|reg| reg.get(&id).is_some());
                #[cfg(not(feature = "plugins-wasm"))]
                let loaded = false;
                plugins.push(serde_json::json!({
                    "name": id,
                    "display_name": info.manifest.name,
                    "description": info.manifest.description,
                    "version": info.manifest.version,
                    "author": info.manifest.author,
                    "type": "wasm",
                    "installed": true,
                    "enabled": enabled,
                    "loaded": loaded,
                    "restart_required": enabled && !loaded,
                    "url": format!("/api/v1/plugins/{id}/"),
                }));
            }
        }
    }

    Json(json!(plugins))
}

/// The plugins `plugins::init` loaded, or an empty slice before it has run.
fn plugin_snapshot(state: &AppState) -> &[tune_core::plugin_sdk::PluginInfo] {
    state
        .plugin_info
        .get()
        .map(|v| v.as_slice())
        .unwrap_or_default()
}

/// Compiled-in plugins `plugins::init` skipped (opt-in-not-installed or
/// disabled), or an empty slice before it has run.
fn plugin_available_snapshot(state: &AppState) -> &[tune_core::plugin_sdk::AvailablePluginInfo] {
    state
        .plugin_available
        .get()
        .map(|v| v.as_slice())
        .unwrap_or_default()
}

async fn get_plugin(Path(name): Path<String>, State(state): State<AppState>) -> Json<Value> {
    // An SDK plugin is authoritative about itself: it is loaded or it is not,
    // regardless of what the settings table happens to say.
    if let Some(info) = plugin_snapshot(&state).iter().find(|p| p.name == name) {
        return Json(json!({
            "name": info.name,
            "description": info.description,
            "version": info.version,
            "type": "sdk",
            "installed": true,
            "enabled": info.enabled,
            "status": "loaded",
            "config_schema": info.config_schema,
        }));
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("plugin_{name}_installed");
    let installed = settings
        .get(&key)
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);

    let enabled_key = format!("plugin_{name}_enabled");
    let enabled = settings
        .get(&enabled_key)
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);

    Json(json!({
        "name": name,
        "installed": installed,
        "enabled": enabled,
        "status": if installed { "installed" } else { "not_installed" },
    }))
}

async fn enable_plugin(Path(name): Path<String>, State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("plugin_{name}_enabled");
    settings.set(&key, "true").ok();
    Json(json!({ "name": name, "enabled": true }))
}

async fn disable_plugin(Path(name): Path<String>, State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("plugin_{name}_enabled");
    settings.set(&key, "false").ok();
    Json(json!({ "name": name, "enabled": false }))
}

#[derive(Deserialize)]
struct InstallRequest {
    #[allow(dead_code)]
    version: Option<String>,
}

/// Can flipping `plugin_{name}_installed` ever make something run here?
///
/// Only two kinds of name can: a plugin compiled into this binary — which is
/// the whole registered set, dormant and uncatalogued included, see
/// [`AppState::plugin_names`] — and a wasm plugin already unpacked in the
/// plugins directory, which `load_wasm_plugins` picks up at the next boot.
///
/// Anything else names nothing: the two settings get written, the startup gate
/// finds no such plugin, and nothing ever loads. Le catalogue distant sert
/// encore 24 fiches de l'ère Python (`platforms: "python"`, `pip install …`) ;
/// `MarketplacePlugin::is_installable` les retire de ce que le serveur PROPOSE,
/// mais le nom d'une de ces fiches — ou le nom hérité d'une bibliothèque
/// migrée, qui ressort de la clé `plugins` de la table des réglages — arrive
/// encore ici par la ligne locale du gestionnaire, et repartait avec
/// « installé » et « redémarrage requis » (#2132).
async fn peut_etre_installe(state: &AppState, name: &str) -> bool {
    if state
        .plugin_names
        .get()
        .is_some_and(|noms| noms.iter().any(|n| n == name))
    {
        return true;
    }

    // Un greffon wasm déjà posé sur le disque : ses réglages le gouvernent
    // vraiment, donc (ré)installer par son identifiant a un sens.
    let Some(dir) = crate::plugins::wasm_plugins_dir() else {
        return false;
    };
    let manager = tune_core::plugins::PluginManager::new(dir);
    match manager.scan().await {
        Ok(infos) => infos.iter().any(|i| i.manifest.id == name),
        Err(_) => false,
    }
}

/// 404 pour un nom que ce serveur ne porte pas — corps identique pour
/// `install` et `update`, qui écrivaient tous les deux le même réglage.
fn greffon_inconnu(name: &str) -> axum::response::Response {
    tracing::info!(plugin_name = %name, "plugin_install_refused_unknown_name");
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": "plugin_inconnu",
            "name": name,
            "detail": "no plugin by that name is compiled into this server or installed on disk — nothing would load",
        })),
    )
        .into_response()
}

async fn install_plugin(
    Path(name): Path<String>,
    State(state): State<AppState>,
    Json(_body): Json<InstallRequest>,
) -> axum::response::Response {
    // No download for compiled-in plugins (Bandcamp today) — installing just
    // flips the settings the startup gate reads. Wasm marketplace installs go
    // through a separate route. `restart_required` because the gate only runs
    // at startup, so the plugin loads on the next boot, not this request.
    //
    // Volontairement non filtré par `catalogued()` : un greffon hors catalogue
    // (dj, karaoke — voir #2090) n'est plus PROPOSÉ, mais reste installable par
    // qui le demande nommément. Le retrait du catalogue est une fin de
    // promesse, pas une condamnation. C'est pourquoi le garde-fou ci-dessous
    // porte sur le jeu REGISTRÉ et non sur le catalogue.
    if !peut_etre_installe(&state, &name).await {
        return greffon_inconnu(&name);
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("plugin_{name}_installed");
    settings.set(&key, "true").ok();
    let enabled_key = format!("plugin_{name}_enabled");
    settings.set(&enabled_key, "true").ok();
    Json(json!({ "name": name, "status": "installed", "restart_required": true })).into_response()
}

async fn update_plugin(
    Path(name): Path<String>,
    State(state): State<AppState>,
) -> axum::response::Response {
    // Stub: Rust server doesn't use pip. Track state in settings — but only
    // for a name that means something here: this route writes the very same
    // `plugin_{name}_installed` key as `install_plugin`, so leaving it open
    // would leave the hole open through the "Update" button.
    if !peut_etre_installe(&state, &name).await {
        return greffon_inconnu(&name);
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("plugin_{name}_installed");
    settings.set(&key, "true").ok();
    Json(json!({ "name": name, "status": "updated" })).into_response()
}

async fn delete_plugin(
    Path(name): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    // A wasm plugin lives on disk: uninstalling means removing its directory
    // (the flags alone would leave it re-loaded at every startup).
    let removed_dir = match crate::plugins::remove_wasm_dir(&name) {
        Ok(removed) => removed,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "uninstall_failed", "detail": e })),
            )
                .into_response();
        }
    };
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("plugin_{name}_installed");
    // A compiled-in plugin (DJ/Karaoke) has no dir to remove, but if it was
    // installed it is loaded and running now — the startup gate only drops it
    // next boot, so a restart is still required.
    let was_installed = settings.get(&key).ok().flatten().as_deref() == Some("true");
    settings.delete(&key).ok();
    let enabled_key = format!("plugin_{name}_enabled");
    settings.delete(&enabled_key).ok();
    // 200 + JSON rather than 204: the web client's fetchJSON treats an empty
    // body as an error, and it needs restart_required for its banner.
    Json(json!({
        "status": "uninstalled",
        "name": name,
        "restart_required": removed_dir || was_installed,
    }))
    .into_response()
}

async fn plugin_docs() -> Json<Value> {
    // `/docs/plugins` n'existe pas sur le site (404) : la doc plugins vit dans
    // le guide utilisateur, section « Plugins » (#1282, Jean Valjean).
    Json(json!({ "url": "https://mozaiklabs.fr/guide#plugins" }))
}
