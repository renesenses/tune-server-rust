//! TESS T17 — routes des préréglages de chaîne et du mode d'application
//! (maquette A2), sur le modèle de l'API TESS (T08) : corps stricts, refus
//! `{status, code, detail, plugin_id}` (contrat § 7).
//!
//! | Route | Rôle |
//! |---|---|
//! | `GET /tess/presets` | liste globale et `apply_mode` |
//! | `POST /tess/presets` `{name, zone_id}` | « Save » : enregistre la chaîne courante de la zone |
//! | `DELETE /tess/presets/{id}` | la croix (Pure compris) |
//! | `PUT /tess/presets/apply_mode` `{apply_mode}` | « Immediately » / « At next playback » |
//! | `GET /zones/{id}/tess/presets` | la liste, avec `active_preset` de la zone |
//! | `POST /zones/{id}/tess/presets/{preset}/load` | « Load » : valide (402/409), écrit (T02), applique selon le mode |
//!
//! `active_preset` n'est publié que si la chaîne de la zone est encore
//! exactement celle du préréglage (`prereglage_actif`).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::tess::migration::lire_zone;
use tune_core::tess::prereglages_stockage::{
    ErreurStockage, charger, definir_le_mode, enregistrer_la_chaine, lire_prereglages,
    mode_effectif, prereglage_actif, supprimer,
};
use tune_core::tess::{ModeApplication, RefusTess, catalogue_de_reference};

use crate::state::AppState;

fn refus(status: u16, code: &str, detail: String, plugin_id: Option<&str>) -> Response {
    let r = RefusTess {
        status,
        code: code.to_owned(),
        detail,
        plugin_id: plugin_id.map(str::to_owned),
    };
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(r),
    )
        .into_response()
}

fn refus_stockage(e: &ErreurStockage) -> Response {
    refus(e.statut_http(), e.code(), e.to_string(), e.plugin_id())
}

/// Corps strict : JSON illisible, champ inconnu, valeur inconnue = 400.
fn corps<T: serde::de::DeserializeOwned>(brut: &str) -> Result<T, Response> {
    serde_json::from_str(brut).map_err(|e| refus(400, "invalid_body", e.to_string(), None))
}

fn zone_existe(state: &AppState, zone_id: i64) -> bool {
    ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten()
        .is_some()
}

fn zone_inconnue(zone_id: i64) -> Response {
    refus(404, "unknown_zone", format!("unknown zone {zone_id}"), None)
}

async fn liste(State(state): State<AppState>) -> Response {
    match lire_prereglages(&SettingsRepo::with_backend(state.backend.clone())) {
        Ok(l) => Json(l).into_response(),
        Err(e) => refus_stockage(&e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Sauver {
    name: String,
    zone_id: i64,
}

async fn sauver(State(state): State<AppState>, brut: String) -> Response {
    let b: Sauver = match corps(&brut) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if !zone_existe(&state, b.zone_id) {
        return zone_inconnue(b.zone_id);
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    // La chaîne ENREGISTRÉE (lecture Premium : aucune conversion P2 n'est
    // figée dans un préréglage).
    let chaine = match lire_zone(&settings, b.zone_id, true) {
        Ok(l) => l.chaine,
        Err(e) => return refus(409, "chain_unreadable", e.to_string(), None),
    };
    match enregistrer_la_chaine(&settings, &b.name, &chaine) {
        Ok(p) => {
            // La zone porte exactement cette chaîne : elle devient active.
            let _ = settings.set(
                &tune_core::tess::prereglages_stockage::cle_prereglage_actif(b.zone_id),
                &p.id,
            );
            (StatusCode::CREATED, Json(p)).into_response()
        }
        Err(e) => refus_stockage(&e),
    }
}

async fn effacer(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match supprimer(&SettingsRepo::with_backend(state.backend.clone()), &id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => refus_stockage(&e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Mode {
    apply_mode: ModeApplication,
}

async fn regler_le_mode(State(state): State<AppState>, brut: String) -> Response {
    let b: Mode = match corps(&brut) {
        Ok(b) => b,
        Err(r) => return r,
    };
    if b.apply_mode == ModeApplication::Unknown {
        return refus(
            400,
            "invalid_body",
            "apply_mode: immediately or at_next_playback".into(),
            None,
        );
    }
    match definir_le_mode(
        &SettingsRepo::with_backend(state.backend.clone()),
        b.apply_mode,
    ) {
        Ok(l) => Json(l).into_response(),
        Err(e) => refus_stockage(&e),
    }
}

async fn liste_de_la_zone(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Response {
    if !zone_existe(&state, zone_id) {
        return zone_inconnue(zone_id);
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let l = match lire_prereglages(&settings) {
        Ok(l) => l,
        Err(e) => return refus_stockage(&e),
    };
    let premium = state.license.is_premium().await;
    let actif = lire_zone(&settings, zone_id, premium)
        .ok()
        .and_then(|z| prereglage_actif(&settings, zone_id, &z.chaine));
    let mut v = serde_json::to_value(&l).unwrap_or(Value::Null);
    v["zone_id"] = json!(zone_id);
    v["active_preset"] = json!(actif);
    Json(v).into_response()
}

async fn charger_sur_la_zone(
    State(state): State<AppState>,
    Path((zone_id, preset)): Path<(i64, String)>,
) -> Response {
    if !zone_existe(&state, zone_id) {
        return zone_inconnue(zone_id);
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let premium = state.license.is_premium().await;
    let mode = match lire_prereglages(&settings) {
        Ok(l) => mode_effectif(l.apply_mode),
        Err(e) => return refus_stockage(&e),
    };
    let chaine = match charger(
        &settings,
        zone_id,
        &preset,
        &catalogue_de_reference(),
        premium,
    ) {
        Ok(c) => c,
        Err(e) => return refus_stockage(&e),
    };
    let geste = state
        .orchestrator
        .appliquer_la_chaine_tess(zone_id, mode)
        .await;
    state.event_bus.emit_typed(
        tune_core::event_types::EventType::ZoneUpdated,
        json!({ "id": zone_id }),
    );
    Json(json!({
        "zone_id": zone_id,
        "active_preset": preset,
        "chain": chaine,
        "apply_mode": mode,
        "application": geste.code(),
    }))
    .into_response()
}

/// `/tess/…` (préréglages globaux).
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/presets", get(liste).post(sauver))
        .route("/presets/apply_mode", put(regler_le_mode))
        .route("/presets/{id}", delete(effacer))
}

/// `/zones/{id}/tess/presets…` (fusionné dans le routeur des zones).
pub fn router_zones() -> Router<AppState> {
    Router::new()
        .route("/{id}/tess/presets", get(liste_de_la_zone))
        .route(
            "/{id}/tess/presets/{preset}/load",
            post(charger_sur_la_zone),
        )
}
