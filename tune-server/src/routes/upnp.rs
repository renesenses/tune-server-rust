use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(upnp_status))
        .route("/config", get(upnp_config).post(set_upnp_config))
}

/// GET /api/v1/upnp/status — report whether the UPnP MediaServer is active.
async fn upnp_status(State(state): State<AppState>) -> Json<serde_json::Value> {
    let upnp = state.upnp.as_ref();
    // #9079 — monté ne veut pas dire en service : l'interrupteur décide.
    let enabled = upnp.is_some_and(|u| u.annonce.est_actif());
    let uuid = upnp.map(|u| u.uuid.clone()).unwrap_or_default();
    let friendly_name = upnp
        .map(|u| u.friendly_name.clone())
        .unwrap_or_else(|| "Tune Server".into());

    Json(json!({
        "enabled": enabled,
        "uuid": uuid,
        "friendly_name": friendly_name,
        "port": state.port,
    }))
}

/// GET /api/v1/upnp/config — return the current UPnP configuration.
async fn upnp_config(State(state): State<AppState>) -> Json<serde_json::Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    // #8374 — mêmes règles que l'annonceur (`background::spawn_upnp_advertiser`)
    // et que `UpnpState::new` : clé en base, sinon variable d'environnement,
    // sinon `Tune (<nom>)` (#8189). L'écran reflète ainsi ce que le serveur fait.
    let enabled = tune_core::upnp_server::annonce_voulue(&settings);
    let friendly_name = tune_core::upnp_server::nom_publie(&settings);

    Json(json!({
        "enabled": enabled,
        "friendly_name": friendly_name,
    }))
}

/// POST /api/v1/upnp/config — update UPnP configuration.
async fn set_upnp_config(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());

    if let Some(enabled) = body.get("enabled").and_then(|v| v.as_bool()) {
        settings
            .set("upnp_enabled", if enabled { "true" } else { "false" })
            .ok();
        // #9079 — la bascule prend effet tout de suite, et non plus au
        // redémarrage : routes /upnp en 404, plus de réponse aux M-SEARCH et
        // `ssdp:byebye` des trois identités quand on coupe ; l'annonceur
        // reprend ses NOTIFY quand on rallume.
        if let Some(ref upnp) = state.upnp
            && upnp.annonce.basculer(enabled)
            && !enabled
        {
            tune_core::upnp_server::retirer_du_reseau().await;
        }
    }
    if let Some(name) = body.get("friendly_name").and_then(|v| v.as_str()) {
        settings.set("upnp_friendly_name", name).ok();
    }

    Json(json!({"ok": true}))
}
