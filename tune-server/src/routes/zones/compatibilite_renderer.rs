//! La compatibilité de commande apprise pour le renderer d'une zone.
//!
//! Quand un renderer refuse `SetAVTransportURI` (501, 714, 716), Tune essaie
//! une commande plus conservatrice — DIDL réduite, orthographe du MIME que le
//! renderer publie, métadonnées vides — et RETIENT celle qui passe, par
//! appareil (UDN) et par MIME source. Cette mémoire est rangée en base et
//! survit au redémarrage (`tune_core::outputs::dlna_repli_set_uri`).
//!
//! - `GET /zones/{id}/compatibilite-renderer` l'expose au diagnostic ;
//! - `DELETE /zones/{id}/compatibilite-renderer` la fait oublier à la main
//!   (« réinitialiser la compatibilité ») : la piste suivante repart de la
//!   commande complète.

use super::*;
use tune_core::outputs::dlna_repli_set_uri as compat;

/// L'appareil DLNA de la zone, ou la réponse d'erreur à rendre.
fn appareil_de_la_zone(state: &AppState, id: i64) -> Result<String, axum::response::Response> {
    let zone = match ZoneRepo::with_backend(state.backend.clone()).get(id) {
        Ok(Some(z)) => z,
        _ => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "zone_not_found" })),
            )
                .into_response());
        }
    };
    if zone.output_type.as_deref() != Some("dlna") {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "not_a_dlna_renderer",
                "message": "La compatibilité de commande ne concerne que les zones DLNA.",
            })),
        )
            .into_response());
    }
    zone.output_device_id.ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "no_output_device" })),
        )
            .into_response()
    })
}

/// `GET /zones/{id}/compatibilite-renderer`.
pub(super) async fn compatibilite_renderer(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> axum::response::Response {
    let device_id = match appareil_de_la_zone(&state, id) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let etat = tokio::task::spawn_blocking(move || compat::compatibilite_de(&device_id)).await;
    match etat {
        Ok(etat) => Json(json!({
            "zone_id": id,
            "device_id": etat.device_id,
            "firmware": etat.firmware,
            "mode_conservateur": !etat.profils.is_empty(),
            "profils": etat.profils,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// `DELETE /zones/{id}/compatibilite-renderer` : « réinitialiser la
/// compatibilité ».
pub(super) async fn reinitialiser_compatibilite_renderer(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> axum::response::Response {
    let device_id = match appareil_de_la_zone(&state, id) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let resultat =
        tokio::task::spawn_blocking(move || compat::reinitialiser_compatibilite(&device_id))
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r);
    match resultat {
        Ok(oublies) => Json(json!({ "zone_id": id, "profils_oublies": oublies })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}
