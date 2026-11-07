//! #7922 — `GET` et `POST /zones/{id}/tune-tested` : proposer l'appareil
//! d'une zone pour Tune tested après une heure d'écoute sans erreur.
//!
//! Le compte et le contenu viennent de
//! [`tune_core::cloud::tune_tested_proposition`]. Le `GET` rend l'aperçu
//! exact du corps que le `POST` envoie, identifiant d'instance compris : le
//! client web montre ce qui part, sans rien recomposer. L'identité présentée
//! au site est celle du rapport de bogue (jeton de compte, sinon clé de
//! licence), par la même fonction du support.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use tune_core::cloud::tune_tested::RACINE_PAR_DEFAUT;
use tune_core::cloud::tune_tested_proposition::{self as proposition, EtatProposition};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;

use crate::state::AppState;

/// Chemin de la route du site qui reçoit les propositions : celle que le site
/// sert (`routes/api.php` de renesenses/site-mozaiklabs,
/// `TuneTestedPropositionController::store`). L'ancien chemin
/// `/api/v1/community/tune-tested/proposals` n'existait pas côté site : toute
/// proposition revenait « refusée par le site » (#8033, #8084).
pub(crate) const CHEMIN_DU_SITE: &str = "/api/v1/tune-tested/propositions";

/// La racine est celle du catalogue Tune tested (`RACINE_PAR_DEFAUT`, ou
/// `mozaik_base_url`), pour que les deux ne divergent pas.
fn url_du_site(settings: &SettingsRepo) -> String {
    let base = settings
        .get("mozaik_base_url")
        .ok()
        .flatten()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| RACINE_PAR_DEFAUT.to_string());
    format!("{base}{CHEMIN_DU_SITE}")
}

fn refus(statut: StatusCode, code: &str, extra: Value) -> Response {
    let mut corps = json!({ "error": code, "code": code });
    if let (Some(corps), Value::Object(extra)) = (corps.as_object_mut(), extra) {
        corps.extend(extra);
    }
    (statut, Json(corps)).into_response()
}

/// Le corps envoyé au site : la proposition et l'identifiant d'instance.
fn corps_envoye(settings: &SettingsRepo, apercu: &proposition::Proposition) -> Value {
    let mut corps = serde_json::to_value(apercu).unwrap_or_else(|_| json!({}));
    let instance_id = settings
        .get("instance_id")
        .ok()
        .flatten()
        .unwrap_or_default();
    if let Some(objet) = corps.as_object_mut() {
        objet.insert("instance_id".into(), json!(instance_id));
    }
    corps
}

/// L'état de la zone, ou `None` si elle n'existe pas.
fn etat_de_la_zone(state: &AppState, zone_id: i64) -> Option<(SettingsRepo, EtatProposition)> {
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten()?;
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let etat = proposition::etat(&settings, zone_id, zone.output_device_id.as_deref());
    Some((settings, etat))
}

fn vue(settings: &SettingsRepo, etat: &EtatProposition) -> Value {
    json!({
        "eligible": etat.eligible,
        "seconds_without_error": etat.seconds_without_error,
        "threshold_seconds": etat.threshold_seconds,
        "remaining_seconds": etat.remaining_seconds,
        "already_proposed": etat.already_proposed,
        "already_tune_tested": etat.already_tune_tested,
        "reason": etat.reason,
        "preview": etat.preview.as_ref().map(|p| corps_envoye(settings, p)),
    })
}

pub async fn etat_tune_tested(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Response {
    match etat_de_la_zone(&state, zone_id) {
        Some((settings, etat)) => Json(vue(&settings, &etat)).into_response(),
        None => refus(StatusCode::NOT_FOUND, "zone_not_found", json!({})),
    }
}

/// L'aperçu montré à l'utilisateur, si le client le renvoie avec sa
/// confirmation (`{"preview": …}`). Un corps vide ou illisible n'en porte pas.
fn apercu_confirme(corps: &[u8]) -> Option<Value> {
    serde_json::from_slice::<Value>(corps)
        .ok()?
        .get("preview")
        .cloned()
        .filter(|v| !v.is_null())
}

pub async fn proposer_tune_tested(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    confirmation: axum::body::Bytes,
) -> Response {
    let Some((settings, etat)) = etat_de_la_zone(&state, zone_id) else {
        return refus(StatusCode::NOT_FOUND, "zone_not_found", json!({}));
    };
    let apercu = match (etat.reason, etat.preview.as_ref()) {
        (None, Some(apercu)) => apercu,
        (Some("below_threshold"), _) => {
            return refus(
                StatusCode::CONFLICT,
                "tune_tested_below_threshold",
                json!({
                    "seconds_without_error": etat.seconds_without_error,
                    "threshold_seconds": etat.threshold_seconds,
                    "remaining_seconds": etat.remaining_seconds,
                }),
            );
        }
        (Some("no_device"), _) => {
            return refus(StatusCode::CONFLICT, "tune_tested_no_device", json!({}));
        }
        _ => {
            return refus(
                StatusCode::CONFLICT,
                "tune_tested_device_unidentified",
                json!({}),
            );
        }
    };
    let Some(identite) = crate::routes::support::identifiants_mozaiklabs(&settings)
        .into_iter()
        .next()
    else {
        return refus(
            StatusCode::PRECONDITION_FAILED,
            "tune_tested_not_connected",
            json!({}),
        );
    };
    let corps = corps_envoye(&settings, apercu);
    // L'aperçu est exact : si une piste s'est terminée depuis qu'il a été
    // montré, rien ne part et le client reçoit le nouveau à confirmer.
    if let Some(montre) = apercu_confirme(&confirmation)
        && montre != corps
    {
        return refus(
            StatusCode::CONFLICT,
            "tune_tested_preview_changed",
            json!({ "preview": corps }),
        );
    }
    let client = match tune_core::http::client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "tune_tested_client_http");
            return refus(
                StatusCode::INTERNAL_SERVER_ERROR,
                "tune_tested_send_failed",
                json!({}),
            );
        }
    };
    // `Accept: application/json`, comme le rapport de bogue : sans lui, un
    // refus de validation du site revient en redirection lue comme un succès.
    let requete = client
        .post(url_du_site(&settings))
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&corps);
    // Ni jeton ni clé ne sont journalisés.
    match identite.apply(requete).send().await {
        Ok(resp) if resp.status().is_success() => {
            if proposition::marquer_proposee(&settings, &apercu.device_key).is_err() {
                tracing::warn!("tune_tested_proposition_non_retenue");
            }
            let data: Value = resp.json().await.unwrap_or_else(|_| json!({}));
            tracing::info!(output_type = %apercu.output_type, "tune_tested_proposition_envoyee");
            Json(json!({
                "status": "ok",
                "already_proposed": true,
                "already_tune_tested": etat.already_tune_tested,
                // Le site rend `proposition` ({id, status}).
                "proposal": data
                    .get("proposition")
                    .or_else(|| data.get("proposal"))
                    .cloned()
                    .unwrap_or(Value::Null),
            }))
            .into_response()
        }
        Ok(resp) => {
            let statut = resp.status().as_u16();
            tracing::warn!(status = statut, "tune_tested_proposition_refusee");
            // Jamais 401 vers le client : il le lirait comme la fin de SA
            // session sur ce serveur. Le statut du site est rendu à part.
            let code = if statut == 429 {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::BAD_GATEWAY
            };
            refus(
                code,
                "tune_tested_site_refused",
                json!({ "status": statut }),
            )
        }
        Err(e) => {
            tracing::warn!(error = %e, "tune_tested_proposition_injoignable");
            refus(
                StatusCode::BAD_GATEWAY,
                "tune_tested_site_unreachable",
                json!({}),
            )
        }
    }
}

#[cfg(test)]
#[path = "tune_tested_7922_tests.rs"]
mod tests;
