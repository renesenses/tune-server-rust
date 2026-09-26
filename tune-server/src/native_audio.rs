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
/// #5149 — clé publique minisign des greffons natifs de Mozaiklabs (id
/// `24A5D1CD444CB780`). Paire DÉDIÉE aux greffons, distincte de la clé des
/// mises à jour du serveur (`UPDATE_PUBLIC_KEY`) : un incident sur l'une ne
/// doit pas contraindre l'autre. Sans elle, aucun greffon natif signé par
/// Mozaiklabs ne s'installait chez un utilisateur qui n'avait pas réglé
/// `TUNE_AUDIO_PLUGIN_TRUST` lui-même.
pub(crate) const MOZAIKLABS_PLUGIN_PUBLIC_KEY: &str =
    "RWSAt0xEzdGlJGlttTKkGF4Q3M/tI3jyTY5kLh+PNWziX4QJ3FRQlrJT";

/// Les clés de confiance : celle de Mozaiklabs toujours, puis celles que
/// l'opérateur AJOUTE par `TUNE_AUDIO_PLUGIN_TRUST` (fichier JSON, liste de
/// clés) ou `TUNE_AUDIO_PLUGIN_PUBLIC_KEY`. Un fichier illisible reste une
/// erreur : une confiance mal réglée ne doit pas passer en silence.
pub fn trusted_keys() -> Result<Vec<String>, String> {
    let ajoutees: Vec<String> = if let Some(path) = std::env::var_os("TUNE_AUDIO_PLUGIN_TRUST") {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())?
    } else {
        std::env::var("TUNE_AUDIO_PLUGIN_PUBLIC_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .into_iter()
            .collect()
    };
    Ok(avec_la_cle_de_mozaiklabs(ajoutees))
}

fn avec_la_cle_de_mozaiklabs(ajoutees: Vec<String>) -> Vec<String> {
    let mut cles = vec![MOZAIKLABS_PLUGIN_PUBLIC_KEY.to_string()];
    for cle in ajoutees {
        let cle = cle.trim().to_string();
        if !cle.is_empty() && !cles.contains(&cle) {
            cles.push(cle);
        }
    }
    cles
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
pub(crate) fn feature(id: &str) -> tune_core::license::Feature {
    match id {
        "crossfeed" => tune_core::license::Feature::Crossfeed,
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
mod cle_de_mozaiklabs_5149 {
    use super::{MOZAIKLABS_PLUGIN_PUBLIC_KEY, avec_la_cle_de_mozaiklabs};

    #[test]
    fn la_cle_integree_est_une_cle_publique_minisign_valide() {
        assert!(
            minisign_verify::PublicKey::from_base64(MOZAIKLABS_PLUGIN_PUBLIC_KEY).is_ok(),
            "#5149 : la clé publique des greffons intégrée au serveur n'est pas lisible par minisign-verify"
        );
    }

    #[test]
    fn sans_reglage_la_cle_de_mozaiklabs_est_de_confiance() {
        assert_eq!(
            avec_la_cle_de_mozaiklabs(Vec::new()),
            vec![MOZAIKLABS_PLUGIN_PUBLIC_KEY.to_string()],
            "#5149 : sans réglage de l'opérateur, aucun greffon signé par Mozaiklabs ne s'installerait"
        );
    }

    #[test]
    fn les_cles_de_l_operateur_s_ajoutent_sans_doublon() {
        let cles = avec_la_cle_de_mozaiklabs(vec![
            "  CLE-OPERATEUR  ".to_string(),
            MOZAIKLABS_PLUGIN_PUBLIC_KEY.to_string(),
            String::new(),
            "CLE-OPERATEUR".to_string(),
        ]);
        assert_eq!(
            cles,
            vec![
                MOZAIKLABS_PLUGIN_PUBLIC_KEY.to_string(),
                "CLE-OPERATEUR".to_string()
            ],
            "#5149 : une clé de l'opérateur s'ajoute à celle de Mozaiklabs, sans la remplacer ni se dédoubler"
        );
    }
}
