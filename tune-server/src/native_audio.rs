//! Operator-controlled native audio packages. No default trust key and no
//! unsigned installation switch. Libraries activate only during startup.
use crate::{auth::RequireAdmin, state::AppState};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
pub fn root() -> PathBuf {
    std::env::var_os("TUNE_AUDIO_PLUGINS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("TUNE_PLUGINS_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("plugins"))
                .join("audio")
        })
}
pub fn trusted_keys() -> Result<Vec<String>, String> {
    if let Some(path) = std::env::var_os("TUNE_AUDIO_PLUGIN_TRUST") {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())
    } else {
        Ok(std::env::var("TUNE_AUDIO_PLUGIN_PUBLIC_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .into_iter()
            .collect())
    }
}
pub fn load_installed(settings: &tune_core::db::settings_repo::SettingsRepo) {
    let directory = root();
    let ids = match tune_plugin_native::package::installed_ids(&directory) {
        Ok(ids) => ids,
        Err(error) => {
            tracing::error!(%error,"native_audio_inventory_failed");
            for id in tune_core::audio::premium_plugins::IDS {
                tune_plugin_native::record_failure(id, error.clone());
            }
            return;
        }
    };
    let keys = trusted_keys();
    for id in ids {
        let tiers = !tune_core::audio::premium_plugins::contains(&id);
        if tiers && !tune_core::audio::natifs_tiers::identifiant_admissible(&id) {
            tracing::warn!(%id,"native_audio_unknown_slot");
            continue;
        }
        let demande = if tiers {
            tune_core::audio::natifs_tiers::demande(settings, &id)
        } else {
            tune_core::audio::premium_plugins::enabled(settings, &id)
        };
        if !demande {
            continue;
        }
        let loaded = keys
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|keys| tune_plugin_native::package::load(&directory, &id, keys))
            .and_then(|library| {
                // Un greffon natif tiers n'a de place que sur la chaîne DSP.
                if tiers && library.manifest.kind != tune_plugin_sdk::manifest::PluginKind::Dsp {
                    Err("third-party native plugins must be DSP plugins".to_string())
                } else {
                    Ok(library)
                }
            });
        match loaded {
            Ok(library) => {
                if let Err(error) = tune_plugin_native::register(library) {
                    tune_plugin_native::record_failure(&id, error.to_string());
                }
            }
            Err(error) => {
                tracing::error!(%id,%error,"native_audio_activation_refused");
                tune_plugin_native::record_failure(&id, error);
            }
        }
    }
}
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(status))
        .route("/{id}/install", post(install))
        .route(
            "/{id}/zones/{zone}",
            get(crate::routes::greffons_natifs_tiers::reglage_de_zone)
                .put(crate::routes::greffons_natifs_tiers::regler_la_zone),
        )
        .route(
            "/{id}/profiles",
            get(crate::routes::greffons_natifs_tiers::lister_les_profils)
                .post(crate::routes::greffons_natifs_tiers::enregistrer_un_profil),
        )
        .route(
            "/{id}/profiles/{profile}",
            axum::routing::delete(crate::routes::greffons_natifs_tiers::supprimer_un_profil),
        )
        .route("/{id}/assets/{*name}", get(asset))
        .route("/{id}/rollback", post(rollback))
        .route("/{id}/uninstall", post(uninstall))
        .layer(DefaultBodyLimit::max(
            tune_plugin_native::package::MAX_ARCHIVE as usize,
        ))
}
/// Les greffons natifs tiers présents sur le disque (au moins une version
/// retenue), triés. Un greffon désinstallé avec `remove_native` garde ses
/// versions : il reste listé, inactif.
pub fn third_party_ids() -> Vec<String> {
    let root = root();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|id| is_third_party(id))
        .collect();
    ids.sort();
    ids
}
/// Un greffon natif tiers présent sur le disque sous cet identifiant.
pub fn is_third_party(id: &str) -> bool {
    tune_core::audio::natifs_tiers::identifiant_admissible(id)
        && root().join(id).join("versions").is_dir()
}
/// Un emplacement que ces routes gèrent : l'un des quatre intégrés, ou un
/// greffon natif tiers présent sur le disque.
fn managed(id: &str) -> bool {
    tune_core::audio::premium_plugins::contains(id) || is_third_party(id)
}
async fn status(_admin: RequireAdmin) -> Response {
    let integres = tune_core::audio::premium_plugins::IDS
        .into_iter()
        .map(|id| (id.to_string(), false));
    let tiers = third_party_ids().into_iter().map(|id| (id, true));
    let plugins: Vec<_> = integres
        .chain(tiers)
        .map(|(id, third_party)| {
            json!({"id":id,"third_party":third_party,"native_loaded":tune_plugin_native::provider(&id).is_some(),"error":tune_plugin_native::failure(&id)})
        })
        .collect();
    Json(json!({"abi":1,"target":tune_plugin_native::package::host_target(),"trust_configured":trusted_keys().is_ok_and(|v|!v.is_empty()),"plugins":plugins})).into_response()
}
fn refusal(error: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error":"native_plugin_refused","detail":error})),
    )
        .into_response()
}
/// Binary ZIP body; the detached minisign signature is supplied as base64-free
/// UTF-8 in X-Tune-Plugin-Signature (newlines encoded as literal backslash-n).
async fn install(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let tiers = !tune_core::audio::premium_plugins::contains(&id);
    if tiers {
        if !tune_core::audio::natifs_tiers::identifiant_admissible(&id) {
            return refusal("invalid plugin id".into());
        }
        // L'identifiant d'un autre greffon (compilé, WASM) partagerait ses
        // drapeaux `plugin_{id}_*` : refusé.
        if name_taken_by_another_plugin(&state, &id).await {
            return refusal("plugin id already used by another plugin".into());
        }
    }
    if let Err(response) = crate::premium_guard::require_premium(&state.license, feature(&id)).await
    {
        return response;
    }
    let signature = match headers
        .get("x-tune-plugin-signature")
        .and_then(|v| v.to_str().ok())
    {
        Some(s) => s.replace("\\n", "\n"),
        None => return refusal("missing detached signature".into()),
    };
    let keys = match trusted_keys() {
        Ok(keys) => keys,
        Err(e) => return refusal(e),
    };
    let root = root();
    let expected = id.clone();
    let installed = tokio::task::spawn_blocking(move || {
        // Validate expected slot BEFORE activation, not after writing active.json.
        if tiers {
            let package = tune_plugin_native::package::inspect(&body, &signature, &keys)?;
            if package.manifest.kind != tune_plugin_sdk::manifest::PluginKind::Dsp {
                return Err("third-party native plugins must be DSP plugins".to_string());
            }
        }
        tune_plugin_native::package::install_for(&root, &body, &signature, &keys, &expected)
    })
    .await;
    match installed {
        Ok(Ok(package)) => {
            let settings =
                tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
            for suffix in ["installed", "enabled"] {
                if let Err(e) = settings.set(&format!("plugin_{id}_{suffix}"), "true") {
                    return refusal(e);
                }
            }
            Json(json!({"id":id,"installed":true,"restart_required":true,"target":package.target}))
                .into_response()
        }
        Ok(Err(e)) => refusal(e),
        Err(e) => refusal(e.to_string()),
    }
}
/// Le droit qui ouvre un emplacement. Le droit gratuit (`DspEq`) n'appartient
/// qu'à l'égaliseur, nommément ; tout autre identifiant, greffon natif tiers
/// compris, exige le Premium.
pub(crate) fn feature(id: &str) -> tune_core::license::Feature {
    match id {
        "equalizer" => tune_core::license::Feature::DspEq,
        "crossfeed" => tune_core::license::Feature::Crossfeed,
        "converter" => tune_core::license::Feature::BatchConverter,
        "declick" => tune_core::license::Feature::Declick,
        _ => THIRD_PARTY_FEATURE,
    }
}
/// Le droit Premium des greffons natifs tiers.
pub(crate) const THIRD_PARTY_FEATURE: tune_core::license::Feature =
    tune_core::license::Feature::PluginMarketplace;
/// Le nom est-il déjà celui d'un greffon d'une autre famille : compilé dans
/// ce serveur (jeu registré) ou WASM posé sur le disque ?
async fn name_taken_by_another_plugin(state: &AppState, id: &str) -> bool {
    if state
        .plugin_names
        .get()
        .is_some_and(|names| names.iter().any(|n| n == id))
    {
        return true;
    }
    let Some(dir) = crate::plugins::wasm_plugins_dir() else {
        return false;
    };
    tune_core::plugins::PluginManager::new(dir)
        .scan()
        .await
        .is_ok_and(|infos| infos.iter().any(|i| i.manifest.id == id))
}
async fn rollback(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    if !managed(&id) {
        return refusal("unknown plugin".into());
    }
    if let Err(response) = crate::premium_guard::require_premium(&state.license, feature(&id)).await
    {
        return response;
    }
    let keys = match trusted_keys() {
        Ok(keys) => keys,
        Err(e) => return refusal(e),
    };
    match tune_plugin_native::package::rollback(&root(), &id, &keys) {
        Ok(active) => {
            Json(json!({"id":id,"active":active,"restart_required":true})).into_response()
        }
        Err(e) => refusal(e),
    }
}
#[derive(Deserialize)]
struct UninstallOptions {
    #[serde(default)]
    remove_native: bool,
}
async fn uninstall(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(options): Json<UninstallOptions>,
) -> Response {
    if !managed(&id) {
        return refusal("unknown plugin".into());
    }
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    // Choix explicite : le retour du Premium ne doit pas l'annuler (#4861).
    if let Err(e) = tune_core::audio::premium_plugins::forget_withheld(&settings, &id) {
        return refusal(e);
    }
    for suffix in ["installed", "enabled"] {
        if let Err(e) = settings.set(&format!("plugin_{id}_{suffix}"), "false") {
            return refusal(e);
        }
    }
    if options.remove_native {
        if let Err(e) = tune_plugin_native::package::deactivate(&root(), &id) {
            return refusal(e);
        }
    }
    Json(json!({"id":id,"installed":false,"configuration_preserved":true,"restart_required":true}))
        .into_response()
}

async fn asset(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
) -> Response {
    if !managed(&id) {
        return refusal("unknown plugin".into());
    }
    if let Err(response) = crate::premium_guard::require_premium(&state.license, feature(&id)).await
    {
        return response;
    }
    if let Err(response) = crate::premium_audio_plugins::require_installed(&state, &id) {
        return response;
    }
    let keys = match trusted_keys() {
        Ok(keys) => keys,
        Err(e) => return refusal(e),
    };
    let content_type = if name.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if name.ends_with(".mjs") || name.ends_with(".js") {
        "text/javascript; charset=utf-8"
    } else if name.ends_with(".css") {
        "text/css; charset=utf-8"
    } else {
        return refusal("unsupported UI asset type".into());
    };
    match tokio::task::spawn_blocking(move||tune_plugin_native::package::read_asset(&root(),&id,&name,&keys)).await {
        Ok(Ok(bytes)) => ([("content-type",content_type),("x-content-type-options","nosniff"),("cache-control","no-store"),("content-security-policy","sandbox allow-scripts; default-src 'none'; script-src 'self' 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'none'; frame-ancestors 'self'"),("access-control-allow-origin","*")],bytes).into_response(),
        Ok(Err(e))=>refusal(e),Err(e)=>refusal(e.to_string())
    }
}
