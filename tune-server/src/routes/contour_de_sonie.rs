//! #8554 — `GET` / `POST /zones/{id}/eq/loudness` : le contour de loudness
//! qui suit le volume (ISO 226:2003), réglé à côté de l'égaliseur de zone.
//!
//! Lire est libre ; écrire suit le droit de l'égaliseur (Premium, greffon
//! installé), comme `POST /zones/{id}/eq`. Le réglage est rangé en JSON sous
//! `zone_{id}_loudness` ; la réponse dit ce qu'il fait sur la zone
//! (`status` : `active`, `bypassed_pure`, `bypassed_strict_bitperfect`,
//! `level_unknown_fixed_volume`, `output_not_supported`, `disabled`) et le
//! contour au volume courant ; `pure` dit si PURE tient la zone, contour
//! allumé ou non.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::audio::contour_de_sonie as cs;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;

use crate::state::AppState;

/// Corps de `POST` : chaque champ absent garde sa valeur.
#[derive(Debug, Default, Deserialize)]
pub struct CorpsDuContour {
    pub enabled: Option<bool>,
    pub curve: Option<String>,
    pub bass_only: Option<bool>,
    pub intensity_pct: Option<f64>,
    pub reference_volume_db: Option<f64>,
    pub reference_phon: Option<f64>,
    pub level_source: Option<cs::SourceDuNiveau>,
    pub manual_level_db: Option<f64>,
}

fn reponse(state: &AppState, zone_id: i64, portee: Option<&str>) -> Value {
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten();
    let locale = zone
        .as_ref()
        .and_then(|z| z.output_device_id.as_deref())
        .is_some_and(|d| d.starts_with("local:"));
    let (reglage, etat) = cs::etat_de_la_zone(&state.backend, zone_id, locale);
    let volume_db = zone.as_ref().and_then(|z| cs::volume_db_du_gain(z.volume));
    let apercu = cs::apercu(&reglage, volume_db);
    let mut corps = json!({
        "zone_id": zone_id,
        "settings": reglage,
        "status": etat.code(),
        // PURE se dit même contour éteint : l'écran prévient avant l'allumage.
        "pure": tune_core::audio::audiophile::traitements_contournes(&state.backend, zone_id),
        "curves": [cs::COURBE_ISO_226_2003],
        "volume_db": volume_db.map(|v| (v * 10.0).round() / 10.0),
        "attenuation_db": apercu.as_ref().map(|(a, _)| (a * 10.0).round() / 10.0),
        "contour_db": apercu.map(|(_, points)| points
            .into_iter()
            .map(|(hz, db)| json!({"hz": hz, "db": db}))
            .collect::<Vec<_>>()),
    });
    if let Some(p) = portee {
        corps["applied"] = json!(p);
    }
    corps
}

pub async fn get_loudness(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    Json(reponse(&state, zone_id, None))
}

pub async fn set_loudness(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    headers: axum::http::HeaderMap,
    Json(corps): Json<CorpsDuContour>,
) -> axum::response::Response {
    if let Err(resp) = crate::premium_guard::require_premium_localise(
        &state.license,
        tune_core::license::Feature::DspEq,
        &headers,
    )
    .await
    {
        return resp;
    }
    if let Err(response) =
        crate::premium_audio_plugins::require_installed(&state, "equalizer", &headers)
    {
        return response;
    }
    let mut reglage = cs::lire(&state.backend, zone_id);
    if let Some(v) = corps.enabled {
        reglage.enabled = v;
    }
    if let Some(v) = corps.curve {
        reglage.curve = v;
    }
    if let Some(v) = corps.bass_only {
        reglage.bass_only = v;
    }
    if let Some(v) = corps.intensity_pct {
        reglage.intensity_pct = v;
    }
    if let Some(v) = corps.reference_volume_db {
        reglage.reference_volume_db = v;
    }
    if let Some(v) = corps.reference_phon {
        reglage.reference_phon = v;
    }
    if let Some(v) = corps.level_source {
        reglage.level_source = v;
    }
    if let Some(v) = corps.manual_level_db {
        reglage.manual_level_db = v;
    }
    if let Err(erreur) = reglage.valider() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": erreur }))).into_response();
    }
    let texte = serde_json::to_string(&reglage).unwrap_or_default();
    if let Err(erreur) =
        SettingsRepo::with_backend(state.backend.clone()).set(&cs::cle(zone_id), &texte)
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": erreur.to_string() })),
        )
            .into_response();
    }
    let portee = state
        .orchestrator
        .refresh_zone_contour_de_sonie(zone_id)
        .await;
    let code = match portee {
        tune_core::orchestrator::PorteeDuReglage::Immediate => "immediate",
        tune_core::orchestrator::PorteeDuReglage::RienNeJoue => "next_play",
        _ => "next_track",
    };
    Json(reponse(&state, zone_id, Some(code))).into_response()
}
