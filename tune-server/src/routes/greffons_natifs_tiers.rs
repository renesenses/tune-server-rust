//! Greffons natifs TIERS — réglage de zone et profils nommés, sous
//! `/audio-plugins/{id}` (voir `tune_core::audio::natifs_tiers`).
//!
//! - `GET|PUT /audio-plugins/{id}/zones/{zone}` : le réglage du greffon pour
//!   une zone. Le corps du `PUT` est le JSON de réglages du greffon, tel quel ;
//!   l'étage n'est construit que s'il porte `"enabled": true`. Le réglage est
//!   validé par la fabrique du greffon lui-même avant d'être enregistré, puis
//!   poussé à chaud sur une sortie locale qui joue.
//! - `GET|POST /audio-plugins/{id}/profiles`, `DELETE
//!   /audio-plugins/{id}/profiles/{profile}` : profils nommés, sur le modèle
//!   des préréglages du crossfeed (`routes::crossfeed`) — une liste JSON dans
//!   UNE ligne de réglages par greffon, `{ id, name, settings, created_at }`,
//!   sans migration. Un profil se choisit, il ne s'applique pas ici : le
//!   client envoie ses `settings` au `PUT` de la zone.
//!
//! Droits : lire est libre ; écrire demande le Premium (un greffon natif tiers
//! n'a jamais de droit gratuit) ET le greffon installé, activé et chargé.
//! Les quatre emplacements intégrés gardent leurs propres routes.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, http::HeaderMap};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::audio::natifs_tiers;
use tune_core::db::settings_repo::SettingsRepo;

use crate::error::AppError;
use crate::state::AppState;

/// Longueur maximale d'un nom de profil, en caractères.
const NOM_MAX: usize = 64;

/// Un greffon natif tiers que ce serveur connaît : présent sur le disque ou
/// chargé. Les quatre emplacements intégrés n'en sont jamais.
fn connu(id: &str) -> bool {
    natifs_tiers::identifiant_admissible(id)
        && (crate::native_audio::is_third_party(id) || tune_plugin_native::provider(id).is_some())
}

fn inconnu(id: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": "plugin_inconnu", "plugin": id})),
    )
        .into_response()
}

fn refus_du_greffon(detail: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": "invalid_plugin_settings", "detail": detail})),
    )
        .into_response()
}

/// Les deux gardes d'écriture : Premium, puis greffon actif.
async fn autoriser(state: &AppState, id: &str, headers: &HeaderMap) -> Result<(), Response> {
    crate::premium_guard::require_premium_localise(
        &state.license,
        crate::native_audio::THIRD_PARTY_FEATURE,
        headers,
    )
    .await?;
    crate::premium_audio_plugins::require_installed(state, id)
}

fn settings(state: &AppState) -> SettingsRepo {
    SettingsRepo::with_backend(state.backend.clone())
}

/// `GET /audio-plugins/{id}/zones/{zone}`
pub async fn reglage_de_zone(
    State(state): State<AppState>,
    Path((id, zone)): Path<(String, i64)>,
) -> Response {
    if !connu(&id) {
        return inconnu(&id);
    }
    let s = settings(&state);
    Json(json!({
        "plugin": id,
        "zone_id": zone,
        "settings": natifs_tiers::reglage_de_zone(&s, zone, &id),
        "active": natifs_tiers::actif(&s, &id),
    }))
    .into_response()
}

/// `PUT /audio-plugins/{id}/zones/{zone}` — corps : le JSON de réglages du
/// greffon. Rend `applied_live: true` quand une sortie locale qui joue vient
/// de recevoir l'étage ; `false` ne signale pas un échec (rien ne joue, zone
/// non locale, mode PURE) : le réglage vaudra à la lecture suivante.
pub async fn regler_la_zone(
    State(state): State<AppState>,
    Path((id, zone)): Path<(String, i64)>,
    headers: HeaderMap,
    Json(reglage): Json<Value>,
) -> Result<Response, AppError> {
    if !connu(&id) {
        return Ok(inconnu(&id));
    }
    if let Err(refus) = autoriser(&state, &id, &headers).await {
        return Ok(refus);
    }
    if tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .get(zone)
        .map_err(AppError::internal)?
        .is_none()
    {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "zone not found", "zone_id": zone})),
        )
            .into_response());
    }
    if let Err(detail) = natifs_tiers::valider_reglage(&id, &reglage) {
        return Ok(refus_du_greffon(detail));
    }
    settings(&state)
        .set(
            &natifs_tiers::cle_de_zone(zone, &id),
            &serde_json::to_string(&reglage)?,
        )
        .map_err(AppError::internal)?;
    // Persister ne suffit pas : l'étage casque d'une sortie locale qui joue
    // est rebâti tout de suite, comme pour le crossfeed (#1786).
    let applied_live = state.orchestrator.refresh_zone_crossfeed(zone).await;
    Ok(Json(json!({
        "plugin": id,
        "zone_id": zone,
        "settings": reglage,
        "applied_live": applied_live,
    }))
    .into_response())
}

fn charger(state: &AppState, id: &str) -> Vec<Value> {
    settings(state)
        .get(&natifs_tiers::cle_des_profils(id))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn ecrire(state: &AppState, id: &str, profils: &[Value]) -> Result<(), AppError> {
    settings(state)
        .set(
            &natifs_tiers::cle_des_profils(id),
            &serde_json::to_string(profils)?,
        )
        .map_err(AppError::internal)?;
    Ok(())
}

/// `GET /audio-plugins/{id}/profiles`
pub async fn lister_les_profils(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if !connu(&id) {
        return inconnu(&id);
    }
    Json(json!({ "profiles": charger(&state, &id) })).into_response()
}

#[derive(Deserialize)]
pub struct CorpsProfil {
    name: String,
    settings: Value,
}

/// `POST /audio-plugins/{id}/profiles` — enregistre ces réglages sous ce nom.
///
/// Un nom déjà pris (casse et espaces ignorés) est MIS À JOUR, même `id`.
/// 201 à la création, 200 à la mise à jour. Les réglages passent par la même
/// validation que le réglage d'une zone : un profil ne peut pas promettre ce
/// que le `PUT` de la zone refuserait ensuite.
pub async fn enregistrer_un_profil(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(corps): Json<CorpsProfil>,
) -> Result<Response, AppError> {
    if !connu(&id) {
        return Ok(inconnu(&id));
    }
    if let Err(refus) = autoriser(&state, &id, &headers).await {
        return Ok(refus);
    }
    let nom = corps.name.trim();
    if nom.is_empty() {
        return Err(AppError::bad_request("name est vide"));
    }
    if nom.chars().count() > NOM_MAX {
        return Err(AppError::bad_request(format!(
            "name dépasse {NOM_MAX} caractères"
        )));
    }
    if let Err(detail) = natifs_tiers::valider_reglage(&id, &corps.settings) {
        return Ok(refus_du_greffon(detail));
    }
    let mut profils = charger(&state, &id);
    let meme_nom = |p: &Value| {
        p["name"]
            .as_str()
            .is_some_and(|n| n.trim().to_lowercase() == nom.to_lowercase())
    };
    let (statut, profil) = match profils.iter_mut().find(|p| meme_nom(p)) {
        Some(existant) => {
            existant["name"] = json!(nom);
            existant["settings"] = corps.settings.clone();
            (StatusCode::OK, existant.clone())
        }
        None => {
            let profil = json!({
                "id": uuid::Uuid::new_v4().to_string(),
                "name": nom,
                "settings": corps.settings,
                "created_at": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            });
            profils.push(profil.clone());
            (StatusCode::CREATED, profil)
        }
    };
    ecrire(&state, &id, &profils)?;
    Ok((statut, Json(profil)).into_response())
}

/// `DELETE /audio-plugins/{id}/profiles/{profile}`
pub async fn supprimer_un_profil(
    State(state): State<AppState>,
    Path((id, profil)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if !connu(&id) {
        return Ok(inconnu(&id));
    }
    if let Err(refus) = autoriser(&state, &id, &headers).await {
        return Ok(refus);
    }
    let mut profils = charger(&state, &id);
    let avant = profils.len();
    profils.retain(|p| p["id"].as_str() != Some(profil.as_str()));
    if profils.len() == avant {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "profile not found"})),
        )
            .into_response());
    }
    ecrire(&state, &id, &profils)?;
    Ok(Json(json!({ "deleted": profil })).into_response())
}
