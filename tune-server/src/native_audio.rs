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
        if !tune_core::audio::premium_plugins::contains(&id) {
            tracing::warn!(%id,"native_audio_unknown_slot");
            continue;
        }
        if !tune_core::audio::premium_plugins::enabled(settings, &id) {
            continue;
        }
        let loaded = keys
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|keys| tune_plugin_native::package::load(&directory, &id, keys));
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
        .route("/{id}/assets/{*name}", get(asset))
        .route("/{id}/rollback", post(rollback))
        .route("/{id}/uninstall", post(uninstall))
        .layer(DefaultBodyLimit::max(
            tune_plugin_native::package::MAX_ARCHIVE as usize,
        ))
}
async fn status(_admin: RequireAdmin) -> Response {
    let ids = tune_core::audio::premium_plugins::IDS;
    Json(json!({"abi":1,"target":tune_plugin_native::package::host_target(),"trust_configured":trusted_keys().is_ok_and(|v|!v.is_empty()),"plugins":ids.into_iter().map(|id|json!({"id":id,"native_loaded":tune_plugin_native::provider(id).is_some(),"error":tune_plugin_native::failure(id)})).collect::<Vec<_>>()})).into_response()
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
    if !tune_core::audio::premium_plugins::contains(&id) {
        return refusal("unknown premium feature slot".into());
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
/// Le droit de licence d'un greffon audio. `crossfeed-pro` (#5039) est
/// accordé par la licence Premium EXACTEMENT comme `crossfeed` : même droit.
/// Sans cette ligne il tomberait dans le bras par défaut, celui de
/// l'égaliseur, qui est GRATUIT. La politique commerciale propre à Crossfeed
/// Pro reste à décider ; elle passera par une variante de `Feature`.
pub(crate) fn feature(id: &str) -> tune_core::license::Feature {
    match id {
        "crossfeed" | "crossfeed-pro" => tune_core::license::Feature::Crossfeed,
        "converter" => tune_core::license::Feature::BatchConverter,
        "declick" => tune_core::license::Feature::Declick,
        _ => tune_core::license::Feature::DspEq,
    }
}
async fn rollback(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    if !tune_core::audio::premium_plugins::contains(&id) {
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
    if !tune_core::audio::premium_plugins::contains(&id) {
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
    if !tune_core::audio::premium_plugins::contains(&id) {
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

#[cfg(test)]
mod tests {
    use super::feature;
    use tune_core::license::Feature;

    /// #5039 : le manifeste de Crossfeed Pro réclame `crossfeed-pro`, et ce
    /// droit est celui de `crossfeed`, réservé au Premium — pas le bras par
    /// défaut (l'égaliseur, gratuit).
    #[test]
    fn crossfeed_pro_est_premium_comme_crossfeed_5039() {
        let manifest: serde_json::Value = serde_json::from_str(include_str!(
            "../../sdk/tune-plugin-crossfeed-pro/manifest.json"
        ))
        .unwrap();
        assert_eq!(manifest["entitlement"], "crossfeed-pro");
        assert_eq!(manifest["id"], "crossfeed-pro");
        let droit = feature("crossfeed-pro");
        assert_eq!(droit, feature("crossfeed"));
        assert_ne!(droit, Feature::DspEq, "tombé dans le bras gratuit");
        assert!(Feature::all_premium().contains(&droit));
    }
}
