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

/// Les fiches héritées de la clef de réglages `plugins`, débarrassées de
/// celles que ce serveur ne peut pas honorer.
///
/// Cette clef est le **second** catalogue, et le seul que le tri de
/// `MarketplacePlugin::is_installable` n'atteint pas : elle n'est écrite nulle
/// part dans ce dépôt (`git grep 'set("plugins"' → 0 écriture, 3 lectures) et
/// n'est rendue que par ici et par `/system/plugins`. Ce qu'elle contient vient
/// donc de la table `settings` d'AVANT — celle du Tune écrit en Python, que la
/// migration conserve —, et ces lignes ressortent **telles quelles** : ni
/// `type`, ni `platforms`, aucun signal qui distingue une fiche vivante d'une
/// fiche morte. C'est ce que décrit #2132 : « rien ne les filtre ».
///
/// Le signal retenu n'est pas un champ de la fiche — il n'y en a aucun de
/// fiable dans un objet JSON libre — mais **le nom, confronté à ce que ce
/// binaire peut réellement charger** ([`noms_chargeables`]) : exactement
/// l'autorité que `install`/`update` interrogent déjà. Proposer et installer
/// répondent ainsi de la même vérité ; sans quoi le gestionnaire offrirait une
/// fiche que le bouton refuse ensuite par un 404.
///
/// Une fiche gardée est rendue **inchangée**, identifiant compris : un
/// identifiant qui bouge casse les installations existantes.
pub(crate) async fn fiches_locales_honorables(state: &AppState) -> Vec<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let heritees: Vec<Value> = settings
        .get("plugins")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if heritees.is_empty() {
        return heritees;
    }

    let chargeables = noms_chargeables(state).await;
    let mut ecartees: Vec<String> = Vec::new();
    let gardees: Vec<Value> = heritees
        .into_iter()
        .filter(|fiche| {
            // Une fiche sans nom exploitable ne désigne rien : toutes les
            // routes d'action (`/{name}/install`, `/enable`, `/{name}`) sont
            // clavetées sur `name`.
            let nom = fiche.get("name").and_then(Value::as_str).unwrap_or("");
            if !nom.is_empty() && chargeables.contains(nom) {
                return true;
            }
            ecartees.push(if nom.is_empty() {
                "<sans nom>".to_string()
            } else {
                nom.to_string()
            });
            false
        })
        .collect();

    if !ecartees.is_empty() {
        // Même journal que le tri du catalogue distant
        // (`marketplace_catalog_uninstallable_rows_dropped`) : les deux
        // sources se lisent de la même façon dans un export.
        tracing::info!(
            kept = gardees.len(),
            dropped = ecartees.len(),
            noms = ?ecartees,
            "plugins_local_catalog_unloadable_rows_dropped"
        );
    }
    gardees
}

async fn list_plugins(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut plugins: Vec<Value> = fiches_locales_honorables(&state).await;

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

    // Le sort d'une fiche que l'utilisateur croit avoir installée.
    //
    // Avant le garde-fou d'`install_plugin`, un clic sur « Synchronized
    // Lyrics » posait `plugin_lyrics_installed=true` et `_enabled=true` et
    // répondait « installé, redémarrage requis ». Ces deux réglages sont
    // toujours dans la base des serveurs ≤ v0.9.124, et cette route les
    // relisait telle quelle : elle répondait encore « installed » pour un
    // greffon qui n'a jamais existé ici, et le répéterait à vie.
    //
    // On ne détruit rien — `DELETE /plugins/{name}` reste la sortie, et si le
    // greffon arrive un jour le réglage reprend son sens tout seul. On cesse
    // seulement de confirmer une installation qui n'a rien chargé, et on le
    // DIT : `unavailable`, avec la raison (#2132).
    if (installed || enabled) && !peut_etre_installe(&state, &name).await {
        tracing::info!(plugin_name = %name, "plugin_installed_flag_names_nothing");
        return Json(json!({
            "name": name,
            "installed": false,
            "enabled": false,
            "status": "unavailable",
            "detail": "no plugin by that name is compiled into this server or installed on disk — nothing was ever loaded",
        }));
    }

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
    noms_chargeables(state).await.contains(name)
}

/// L'ensemble des identifiants que ce serveur peut réellement charger.
///
/// Une seule autorité, interrogée par tout ce qui promet quelque chose :
/// `install`, `update`, le détail d'une fiche, et le tri des fiches héritées
/// ([`fiches_locales_honorables`]). Deux sources, et seulement deux :
///
/// * le jeu **registré** ([`AppState::plugin_names`]) — dormant et hors
///   catalogue compris (#2090) ;
/// * les greffons **wasm déjà posés sur le disque**, que `load_wasm_plugins`
///   ramasse au démarrage suivant.
///
/// Le balayage du disque a lieu à chaque appel : c'est un `readdir` sur un
/// dossier de quelques entrées, et la seule alternative — mémoriser le jeu au
/// démarrage — rendrait invisible un greffon installé depuis le dernier boot,
/// que la liste des greffons wasm plus bas montre pourtant déjà.
async fn noms_chargeables(state: &AppState) -> std::collections::HashSet<String> {
    let mut noms: std::collections::HashSet<String> = state
        .plugin_names
        .get()
        .map(|v| v.iter().cloned().collect())
        .unwrap_or_default();

    let Some(dir) = crate::plugins::wasm_plugins_dir() else {
        return noms;
    };
    let manager = tune_core::plugins::PluginManager::new(dir);
    if let Ok(infos) = manager.scan().await {
        noms.extend(infos.into_iter().map(|i| i.manifest.id));
    }
    noms
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
