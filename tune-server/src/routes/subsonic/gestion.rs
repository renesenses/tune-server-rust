//! Les routes de gestion de l'accès Subsonic, sous `/api/v1/subsonic` (#7855).
//!
//! - `GET  /reglage` : l'état du réglage et du droit Premium ;
//! - `PUT  /reglage` : l'allumer (administrateur et Premium) ou l'éteindre ;
//! - `GET  /cles` : les clés du profil, sans jamais la clé elle-même ;
//! - `POST /cles` : créer une clé, rendue UNE SEULE FOIS dans la réponse ;
//! - `DELETE /cles/{id}` : la révoquer.
//!
//! Éteindre le réglage et révoquer une clé restent possibles sans Premium :
//! fermer une porte ne doit jamais dépendre de la licence.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use tune_core::db::cles_clients::ClesClientsRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::license::Feature;

use super::{REGLAGE_ACCES, acces_active};
use crate::auth::RequireAdmin;
use crate::premium_guard::require_premium_localise;
use crate::routes::active_profile::ActiveProfile;
use crate::state::AppState;

/// Longueur maximale du nom d'une clé (nom du client ou de l'appareil).
const NOM_MAX: usize = 80;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/reglage", get(lire_reglage).put(ecrire_reglage))
        .route("/cles", get(lister_cles).post(creer_cle))
        .route("/cles/{id}", delete(revoquer_cle))
}

fn erreur_interne(e: String) -> Response {
    tracing::warn!(error = %e, "subsonic_gestion_failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "internal_error"})),
    )
        .into_response()
}

async fn lire_reglage(State(state): State<AppState>) -> Response {
    let premium = state.license.check_feature(Feature::Subsonic).await;
    Json(json!({"enabled": acces_active(&state), "premium": premium})).into_response()
}

#[derive(Deserialize)]
struct CorpsReglage {
    enabled: bool,
}

async fn ecrire_reglage(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    headers: HeaderMap,
    Json(corps): Json<CorpsReglage>,
) -> Response {
    if corps.enabled
        && let Err(refus) =
            require_premium_localise(&state.license, Feature::Subsonic, &headers).await
    {
        return refus;
    }
    let valeur = if corps.enabled { "true" } else { "false" };
    if let Err(e) = SettingsRepo::with_backend(state.backend.clone()).set(REGLAGE_ACCES, valeur) {
        return erreur_interne(e);
    }
    tracing::info!(enabled = corps.enabled, "subsonic_access_setting_changed");
    Json(json!({"enabled": corps.enabled})).into_response()
}

async fn lister_cles(State(state): State<AppState>, profil: ActiveProfile) -> Response {
    match ClesClientsRepo::with_backend(state.backend.clone()).lister(profil.id()) {
        Ok(cles) => Json(json!({"cles": cles})).into_response(),
        Err(e) => erreur_interne(e),
    }
}

#[derive(Deserialize)]
struct CorpsCle {
    nom: String,
}

async fn creer_cle(
    State(state): State<AppState>,
    profil: ActiveProfile,
    headers: HeaderMap,
    Json(corps): Json<CorpsCle>,
) -> Response {
    if let Err(refus) = require_premium_localise(&state.license, Feature::Subsonic, &headers).await
    {
        return refus;
    }
    let nom = corps.nom.trim();
    if nom.is_empty() || nom.chars().count() > NOM_MAX {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_name", "max_length": NOM_MAX})),
        )
            .into_response();
    }
    match ClesClientsRepo::with_backend(state.backend.clone()).creer(profil.id(), nom) {
        Ok(creee) => {
            tracing::info!(key_id = %creee.cle.id, profile_id = profil.id(), "subsonic_api_key_created");
            (
                StatusCode::CREATED,
                Json(json!({
                    "id": creee.cle.id,
                    "nom": creee.cle.nom,
                    "cree_le": creee.cle.cree_le,
                    // Montrée cette fois-ci seulement : seule son empreinte
                    // est conservée.
                    "cle": creee.secret,
                })),
            )
                .into_response()
        }
        Err(e) => erreur_interne(e),
    }
}

async fn revoquer_cle(
    State(state): State<AppState>,
    profil: ActiveProfile,
    Path(id): Path<String>,
) -> Response {
    match ClesClientsRepo::with_backend(state.backend.clone()).revoquer(profil.id(), &id) {
        Ok(true) => {
            tracing::info!(key_id = %id, profile_id = profil.id(), "subsonic_api_key_revoked");
            Json(json!({"id": id, "revoquee": true})).into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "key_not_found"})),
        )
            .into_response(),
        Err(e) => erreur_interne(e),
    }
}
