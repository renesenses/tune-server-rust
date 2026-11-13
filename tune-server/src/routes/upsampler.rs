//! #7614 — le greffon de suréchantillonnage : `/upsampler`.
//!
//! - `GET /` : le catalogue des filtres offerts (réserve de niveau par
//!   rapport, en dB), les rapports proposés (§ 7 de l'étude : ×2, ×4, ×8,
//!   ×16), le verdict du test de la machine et l'état du greffon.
//! - `POST /machine-test` (administrateur) : mesure la machine (étape I5 de
//!   l'étude, quelques secondes, hors de tout fil audio) et garde le verdict.
//! - `GET|PUT /zones/{id}` : le réglage d'une zone (`zone_{id}_upsampler`).
//!
//! Droits : PAYANT, comme le crossfeed (décision de Bertrand du 07/11/2026),
//! et FACULTATIF : le greffon `upsampler` s'installe depuis le catalogue
//! (`POST /plugins/upsampler/install`, droit `upsampling`). Lire est libre ;
//! activer exige le Premium (402) et le greffon installé (409). Désactiver
//! reste toujours possible.
//!
//! Le réglage ne change jamais la piste en cours : l'étage d'une piste garde
//! le choix de son ouverture, la sortie compare à la frontière suivante et
//! rouvre. La réponse le dit (`applies_at: "next_track"`).
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use tune_core::audio::surechantillonnage as su;
use tune_core::audio::surechantillonnage::greffon;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::license::Feature;

use crate::{auth::RequireAdmin, state::AppState};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(catalogue))
        .route("/machine-test", post(tester_la_machine))
        .route("/zones/{id}", get(lire_zone).put(ecrire_zone))
}

fn settings(state: &AppState) -> SettingsRepo {
    SettingsRepo::with_backend(state.backend.clone())
}

/// Les filtres offerts et leur réserve de niveau, par rapport entier.
pub(crate) fn filtres() -> Value {
    let filtres: Vec<Value> = greffon::OFFERED_FILTERS
        .iter()
        .map(|id| {
            let reserves: serde_json::Map<String, Value> = greffon::OFFERED_RATIOS
                .iter()
                .filter_map(|&r| {
                    greffon::headroom_db(id, r)
                        .map(|db| (format!("x{r}"), json!((db * 100.0).round() / 100.0)))
                })
                .collect();
            json!({
                "id": id,
                // #7614 — réserve fixe + limiteur du mode Réaliste de l'égaliseur.
                "limiter": "realistic",
                "minimum_phase": id.ends_with("-pm"),
                "default": *id == greffon::DEFAULT_FILTER,
                "headroom_db": reserves,
            })
        })
        .collect();
    json!(filtres)
}

async fn etat(state: &AppState) -> Value {
    let s = settings(state);
    json!({
        "premium": state.license.check_feature(Feature::Upsampling).await,
        "installed": tune_core::audio::premium_plugins::enabled(&s, su::ID),
        "machine_test": su::verdict_enregistre(&s),
    })
}

async fn catalogue(State(state): State<AppState>) -> Json<Value> {
    let mut v = etat(&state).await;
    v["filters"] = filtres();
    v["ratios"] = json!(greffon::OFFERED_RATIOS);
    v["default_filter"] = json!(greffon::DEFAULT_FILTER);
    Json(v)
}

/// Mesure la machine hors du fil asynchrone, et garde le verdict.
pub(crate) async fn mesurer_et_enregistrer(state: &AppState) -> Result<su::VerdictMachine, String> {
    let verdict = tokio::task::spawn_blocking(|| su::mesurer_la_machine(su::DUREE_DU_TEST_S))
        .await
        .map_err(|e| e.to_string())?;
    su::enregistrer_le_verdict(&settings(state), &verdict)?;
    tracing::info!(
        simd = %verdict.simd_path,
        admitted = ?verdict.filtres_admis(),
        "upsampler_machine_test_done"
    );
    Ok(verdict)
}

async fn tester_la_machine(_admin: RequireAdmin, State(state): State<AppState>) -> Response {
    match mesurer_et_enregistrer(&state).await {
        Ok(verdict) => Json(json!({ "machine_test": verdict })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

/// Pourquoi le réglage de la zone ne s'appliquerait pas, à cet instant.
///
/// #8149 — au-delà du choix de la zone (licence, greffon, PURE, test de la
/// machine), deux refus que l'écran taisait : une zone qui ne sort pas sur
/// la sortie locale ni sur une zone DLNA / OpenHome (#8519)
/// (`output_not_supported`, même code que le chemin du
/// signal) et le « bit-perfect strict » de la zone (`strict_bitperfect`,
/// même code que `RefusDeCadenceDuGreffon::BitPerfectStrict`), qui refuse
/// toute conversion de cadence (GgB, fil 2742).
fn contrainte(state: &AppState, premium: bool, zone_id: i64) -> Option<&'static str> {
    let pure = tune_core::audio::audiophile::zone_enabled(&state.backend, zone_id);
    if let Err(ecarte) = su::choix_de_la_zone(&settings(state), premium, pure, zone_id) {
        return Some(ecarte.code());
    }
    let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten();
    // Discussion #8519 — DLNA et OpenHome le prennent en charge aussi : Tune
    // leur sert un flux suréchantillonné.
    let reseau_pris_en_charge = zone
        .as_ref()
        .and_then(|z| z.output_type.as_deref())
        .is_some_and(tune_core::audio::surechantillonnage_reseau::sortie_prise_en_charge);
    let peripherique = zone
        .and_then(|z| z.output_device_id)
        .filter(|d| !d.is_empty());
    if !reseau_pris_en_charge && peripherique.is_some_and(|d| !d.starts_with("local:")) {
        return Some("output_not_supported");
    }
    if tune_core::audio::bitperfect_strict::zone_enabled(&state.backend, zone_id) {
        return Some("strict_bitperfect");
    }
    None
}

async fn forme_de_zone(state: &AppState, zone_id: i64) -> Value {
    let s = settings(state);
    let premium = state.license.check_feature(Feature::Upsampling).await;
    let reglages = su::reglages_de_la_zone(&s, zone_id);
    json!({
        "zone_id": zone_id,
        "settings": reglages,
        "constraint": contrainte(state, premium, zone_id),
        "admitted_filters": su::verdict_enregistre(&s).map(|v| {
            v.filtres_admis().into_iter().map(str::to_string).collect::<Vec<_>>()
        }),
        "applies_at": "next_track",
    })
}

async fn lire_zone(State(state): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    Json(forme_de_zone(&state, id).await)
}

async fn ecrire_zone(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(corps): Json<Value>,
) -> Response {
    let reglages = match greffon::settings(&corps) {
        Ok(r) => r,
        Err(_) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": "invalid_upsampler_settings" })),
            )
                .into_response();
        }
    };
    // Désactiver est toujours permis ; activer exige le droit et le greffon.
    if reglages.enabled {
        if let Err(r) = crate::premium_guard::require_premium_localise(
            &state.license,
            Feature::Upsampling,
            &headers,
        )
        .await
        {
            return r;
        }
        if let Err(r) = crate::premium_audio_plugins::require_installed(&state, su::ID, &headers) {
            return r;
        }
        // Étape I5 : au premier usage, la machine est mesurée avant que le
        // réglage ne serve.
        if su::verdict_enregistre(&settings(&state)).is_none()
            && let Err(e) = mesurer_et_enregistrer(&state).await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response();
        }
    }
    if let Err(e) = su::enregistrer_les_reglages(&settings(&state), id, &corps) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response();
    }
    // La sortie vivante reçoit le nouveau choix ; la piste en cours n'en
    // est pas affectée (frontière suivante).
    state.orchestrator.refresh_zone_upsampler(id).await;
    Json(forme_de_zone(&state, id).await).into_response()
}

#[cfg(test)]
#[path = "upsampler_tests.rs"]
mod tests;
