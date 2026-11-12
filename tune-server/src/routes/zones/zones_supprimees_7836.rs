//! #7836 (fil 2642, décision de Bertrand du 10/11) — Réglages › Zones :
//! « Zones supprimées (n) » liste les zones supprimées, chacune avec un
//! bouton Restaurer.
//!
//! Supprimer une zone ne fait que la MASQUER (`is_hidden`, motif et date) :
//! la découverte mDNS ne la recrée pas d'elle-même (`mdns_zone_hidden_skipping`).
//! Le Mac d'un testeur, supprimé le 30/10, restait ainsi introuvable : seule la
//! liste des appareils de « Nouvelle zone » permettait de le restaurer.
//!
//! | route | rôle |
//! |---|---|
//! | `GET /zones/supprimees` | les zones supprimées par l'utilisateur |
//! | `POST /zones/{id}/restaurer` | la rendre visible, réglages conservés |
//!
//! Seuls les masquages de l'utilisateur sont listés et restaurables
//! (`suppression_utilisateur`, `suppression_totale`, et un motif inconnu
//! d'avant la migration 112). Un appareil ignoré se rétablit par
//! « Ne plus ignorer » ; un doublon, un reflet ou une zone fusionnée ne
//! reviennent pas.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use tracing::{info, warn};
use tune_core::db::zone_repo::{MotifMasquage, ZoneMasquee, ZoneRepo};

use crate::state::AppState;

/// Le masquage vient-il d'une suppression par l'utilisateur ?
pub(crate) fn supprimee_par_l_utilisateur(motif: Option<&str>) -> bool {
    match MotifMasquage::depuis_stocke(motif) {
        None => true,
        Some(MotifMasquage::SuppressionUtilisateur | MotifMasquage::SuppressionTotale) => true,
        Some(_) => false,
    }
}

fn corps_de_la_zone(z: &ZoneMasquee, detecte: bool) -> Value {
    json!({
        "id": z.id,
        "name": z.name,
        "output_device_id": (!z.device_id.is_empty()).then_some(&z.device_id),
        "output_type": (!z.protocole.is_empty()).then_some(&z.protocole),
        "motif": z.motif,
        "masquee_le": z.masquee_le,
        // L'appareil est-il annoncé en ce moment ? Une zone restaurée dont
        // l'appareil est absent reste hors ligne jusqu'à son retour.
        "appareil_detecte": detecte,
    })
}

/// `GET /zones/supprimees`
pub(crate) async fn lister(State(state): State<AppState>) -> Response {
    let zones = match ZoneRepo::with_backend(state.backend.clone()).zones_masquees() {
        Ok(z) => z,
        Err(e) => {
            warn!(error = %e, "zones_supprimees_lecture_echouee");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "zones_supprimees_illisibles"})),
            )
                .into_response();
        }
    };
    let sorties = state.outputs.lock().await;
    let items: Vec<Value> = zones
        .iter()
        .filter(|z| supprimee_par_l_utilisateur(z.motif.as_deref()))
        .map(|z| {
            corps_de_la_zone(
                z,
                !z.device_id.is_empty() && sorties.get(&z.device_id).is_some(),
            )
        })
        .collect();
    drop(sorties);
    Json(json!({ "count": items.len(), "items": items })).into_response()
}

/// `POST /zones/{id}/restaurer` — rend la zone visible, avec ses réglages
/// (volume, DSP…), hors de tout ancien groupe, comme `POST /zones` sur le
/// même appareil.
pub(crate) async fn restaurer(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let etat = match repo.etat_de_masquage(id) {
        Ok(Some(e)) => e,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "zone_introuvable"})),
            )
                .into_response();
        }
        Err(e) => {
            warn!(zone_id = id, error = %e, "zone_restauration_lecture_echouee");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "zone_illisible"})),
            )
                .into_response();
        }
    };
    if !etat.masquee {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "zone_non_supprimee"})),
        )
            .into_response();
    }
    if !supprimee_par_l_utilisateur(etat.motif.as_deref()) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": "zone_non_restaurable", "motif": etat.motif})),
        )
            .into_response();
    }
    if let Err(e) = repo.unhide(id) {
        warn!(zone_id = id, error = %e, "zone_restauration_echouee");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "zone_restauration_echouee"})),
        )
            .into_response();
    }
    super::ecriture::sortir_la_zone_recreee_des_groupes(&state, id);
    info!(zone_id = id, motif = ?etat.motif, masquee_le = ?etat.masquee_le, "zone_restauree");
    state
        .event_bus
        .emit("zone.updated", json!({ "zone_id": id }));
    let v = crate::routes::playback::build_zone_json(&state, id).await;
    (StatusCode::OK, Json(v)).into_response()
}

#[cfg(test)]
#[path = "zones_supprimees_7836_tests.rs"]
mod tests;
