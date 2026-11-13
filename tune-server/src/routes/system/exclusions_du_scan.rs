//! #7478 — exclusions du scan PAR EMPLACEMENT de stockage.
//!
//! - `GET  /system/scan-exclusions` : les exclusions globales (inchangées,
//!   sous-chaînes), les règles par emplacement et les racines configurées.
//! - `POST /system/scan-exclusions/preview` `{"locations": […]}` : combien de
//!   pistes de la bibliothèque ces règles retireraient, avec des exemples.
//! - `PUT  /system/scan-exclusions` `{"locations": […], "confirm": N}` :
//!   enregistre. Si des pistes seraient retirées, `confirm` doit valoir
//!   EXACTEMENT ce nombre, sinon 409 et rien n'est écrit. Le retrait se fait
//!   au scan suivant, sous le plafond de la purge ; les fichiers ne sont
//!   jamais touchés.
//!
//! Décisions de Bertrand du 13/11 : aperçu puis retrait ; par emplacement ;
//! motifs glob ; insensible à la casse par défaut.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::scanner::exclusions::{
    CLE_EXCLUSIONS_PAR_EMPLACEMENT, Exclusions, ExclusionsDEmplacement, valider,
};

use crate::state::AppState;

/// Nombre d'exemples de chemins rendus par l'aperçu.
const EXEMPLES: usize = 20;

#[derive(Deserialize)]
pub(crate) struct CorpsExclusions {
    #[serde(default)]
    locations: Vec<ExclusionsDEmplacement>,
    /// Le nombre de pistes que l'utilisateur accepte de voir retirées.
    #[serde(default)]
    confirm: Option<u64>,
}

fn etat(state: &AppState) -> Value {
    json!({
        "global": crate::auto_scan::scan_exclude_patterns(&state.backend),
        "locations": crate::auto_scan::regles_par_emplacement(&state.backend),
        "roots": super::get_music_dirs_list(&state.backend),
    })
}

pub(crate) async fn lire(State(state): State<AppState>) -> Json<Value> {
    Json(etat(&state))
}

fn refus(statut: StatusCode, motif: impl Into<String>) -> Response {
    (statut, Json(json!({"error": motif.into()}))).into_response()
}

/// Valider les règles : motifs lisibles, et racines qui sont bien des
/// emplacements configurés.
fn regles_valides(
    state: &AppState,
    regles: &[ExclusionsDEmplacement],
) -> Result<Vec<ExclusionsDEmplacement>, Response> {
    valider(regles).map_err(|e| refus(StatusCode::BAD_REQUEST, e))?;
    let racines: Vec<String> = super::get_music_dirs_list(&state.backend)
        .iter()
        .map(|r| tune_core::scanner::walker::normalize_path(r))
        .collect();
    let mut propres = Vec::new();
    for r in regles {
        let racine = tune_core::scanner::walker::normalize_path(&r.root);
        if !racines.contains(&racine) {
            return Err(refus(
                StatusCode::BAD_REQUEST,
                format!("« {} » n'est pas un emplacement de la bibliothèque", r.root),
            ));
        }
        let patterns: Vec<String> = r.patterns.iter().map(|m| m.trim().to_string()).collect();
        if patterns.is_empty() {
            continue;
        }
        propres.push(ExclusionsDEmplacement {
            root: racine,
            patterns,
            case_sensitive: r.case_sensitive,
        });
    }
    Ok(propres)
}

/// Les pistes LOCALES de la bibliothèque que ces règles écarteraient.
pub(crate) fn pistes_retirees(
    state: &AppState,
    regles: &[ExclusionsDEmplacement],
) -> Result<Vec<String>, String> {
    let exclusions = Exclusions::new(&[], regles);
    if !exclusions.a_des_exclusions_par_emplacement() {
        return Ok(Vec::new());
    }
    let lignes = state
        .backend
        .query_many("SELECT file_path FROM tracks WHERE source = 'local'", &[])
        .map_err(|e| e.to_string())?;
    let mut chemins: Vec<String> = lignes
        .into_iter()
        .filter_map(|l| l.first().and_then(|v| v.as_string()))
        .filter(|p| exclusions.exclut_par_emplacement(p))
        .collect();
    chemins.sort();
    Ok(chemins)
}

pub(crate) async fn apercu(
    State(state): State<AppState>,
    Json(corps): Json<CorpsExclusions>,
) -> Response {
    let regles = match regles_valides(&state, &corps.locations) {
        Ok(r) => r,
        Err(e) => return e,
    };
    match pistes_retirees(&state, &regles) {
        Ok(chemins) => Json(json!({
            "tracks_to_remove": chemins.len(),
            "examples": chemins.iter().take(EXEMPLES).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => refus(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

pub(crate) async fn enregistrer(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Json(corps): Json<CorpsExclusions>,
) -> Response {
    let regles = match regles_valides(&state, &corps.locations) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let chemins = match pistes_retirees(&state, &regles) {
        Ok(c) => c,
        Err(e) => return refus(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let n = chemins.len() as u64;
    if n > 0 && corps.confirm != Some(n) {
        // Rien n'est écrit : l'écran doit d'abord dire combien de pistes
        // partiraient, et l'utilisateur le confirmer.
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "confirmation_required",
                "tracks_to_remove": n,
                "examples": chemins.iter().take(EXEMPLES).collect::<Vec<_>>(),
            })),
        )
            .into_response();
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let ecrit = if regles.is_empty() {
        settings.delete(CLE_EXCLUSIONS_PAR_EMPLACEMENT)
    } else {
        match serde_json::to_string(&regles) {
            Ok(v) => settings.set(CLE_EXCLUSIONS_PAR_EMPLACEMENT, &v),
            Err(e) => Err(e.to_string()),
        }
    };
    if let Err(e) = ecrit {
        return refus(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    crate::auto_scan::recharger_les_exclusions(&state.backend);
    tracing::info!(
        emplacements = regles.len(),
        pistes_a_retirer = n,
        "scan_exclusions_par_emplacement_enregistrees — retrait au prochain scan"
    );
    let mut rep = etat(&state);
    rep["tracks_to_remove"] = json!(n);
    Json(rep).into_response()
}
