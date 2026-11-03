//! #6285 — la détection des faux hi-res, côté routes.
//!
//! * `GET /library/albums/{id}/hires` : le verdict de l'album et celui de
//!   chacune de ses pistes mesurées (`verifie`, `conforme`, `douteux` + raison,
//!   `non_concluant`), avec la coupure et les bits effectifs. Une piste pas
//!   encore mesurée n'apparaît pas ; un album sans aucune mesure rend
//!   `non_analyse`.
//! * `GET /library/hires-check` : la passe est-elle armée, combien de pistes
//!   restent, combien sont rangées.
//! * `POST /library/hires-check` `{active, douteux_visible}` : arme ou
//!   désarme la passe (désactivée par défaut) ; montre ou masque le verdict
//!   « douteux » dans la liste des albums, son filtre et les règles
//!   intelligentes (masqué par défaut, deuxième étape). Les deux champs sont
//!   facultatifs : un champ absent garde sa valeur.
//!
//! Le verdict de la fiche suit le même réglage que la liste : tant que
//! « douteux » est masqué, une piste ou un album douteux se rend
//! `non_analyse`, sans raison (le badge de la fiche se lit ici). Les mesures
//! brutes (coupure, bits effectifs) restent rendues.

use axum::Json;
use axum::extract::{Path, State};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::audio::faux_hires_passe::{activer, compter_les_candidats, est_active};
use tune_core::audio::faux_hires_publie::{douteux_visible, montrer_douteux, publier_l_album};
use tune_core::db::backend::ToSqlValue;
use tune_core::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};

use crate::error::AppError;
use crate::state::AppState;

fn ph(engine: Engine, i: usize) -> String {
    match engine {
        Engine::Sqlite => SqliteDialect.placeholder(i),
        Engine::Postgres => PostgresDialect.placeholder(i),
    }
}

pub(super) async fn verdict_album(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let rows = state
        .backend
        .query_many(
            &format!(
                "SELECT h.track_id, h.verdict, h.raison, h.coupure_hz, h.bits_effectifs, \
                 h.sample_rate, h.bits_declares FROM hires_verifications h \
                 JOIN tracks t ON t.id = h.track_id WHERE t.album_id = {} ORDER BY h.track_id",
                ph(state.backend.engine(), 1)
            ),
            &[&id as &dyn ToSqlValue],
        )
        .map_err(AppError::internal)?;
    let visible = douteux_visible(&state.backend);
    let mut verdicts = Vec::with_capacity(rows.len());
    let pistes: Vec<Value> = rows
        .iter()
        .map(|r| {
            let verdict = r.get(1).and_then(|v| v.as_string()).unwrap_or_default();
            let raison = r.get(2).and_then(|v| v.as_string());
            verdicts.push((verdict.clone(), raison.clone()));
            // #6285 — une piste douteuse masquée se dit non analysée.
            let (verdict, raison) = if verdict == "douteux" && !visible {
                ("non_analyse".to_string(), None)
            } else {
                (verdict, raison)
            };
            json!({
                "track_id": r.first().and_then(|v| v.as_i64()),
                "verdict": verdict,
                "raison": raison,
                "coupure_hz": r.get(3).and_then(|v| v.as_f64()),
                "bits_effectifs": r.get(4).and_then(|v| v.as_i64()),
                "sample_rate": r.get(5).and_then(|v| v.as_i64()),
                "bits_declares": r.get(6).and_then(|v| v.as_i64()),
            })
        })
        .collect();
    let (verdict, raison) = publier_l_album(&verdicts, visible);
    Ok(Json(json!({
        "album_id": id,
        "verdict": verdict,
        "raison": raison,
        "pistes": pistes,
    })))
}

pub(super) async fn etat(State(state): State<AppState>) -> Json<Value> {
    let rangees = state
        .backend
        .query_one("SELECT COUNT(*) FROM hires_verifications", &[])
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0);
    Json(json!({
        "active": est_active(&state.backend),
        "douteux_visible": douteux_visible(&state.backend),
        "restantes": compter_les_candidats(&state.backend),
        "rangees": rangees,
    }))
}

#[derive(Deserialize)]
pub(super) struct Reglage {
    active: Option<bool>,
    douteux_visible: Option<bool>,
}

pub(super) async fn regler(
    State(state): State<AppState>,
    Json(body): Json<Reglage>,
) -> Result<Json<Value>, AppError> {
    if let Some(active) = body.active {
        activer(&state.backend, active).map_err(AppError::internal)?;
    }
    if let Some(visible) = body.douteux_visible {
        montrer_douteux(&state.backend, visible).map_err(AppError::internal)?;
    }
    Ok(Json(json!({
        "active": est_active(&state.backend),
        "douteux_visible": douteux_visible(&state.backend),
    })))
}

#[cfg(test)]
#[path = "faux_hires_tests_6285.rs"]
mod tests;
