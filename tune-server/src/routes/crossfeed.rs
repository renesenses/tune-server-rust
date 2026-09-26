//! Préréglages NOMMÉS du crossfeed (#4684) — `/crossfeed/presets`.
//!
//! Même modèle que les préréglages d'égaliseur (`/eq/presets`, clé
//! `eq_presets`) : une liste JSON dans UNE ligne de réglages, `crossfeed_presets`,
//! globale au serveur. Un préréglage ne porte que ce qui fait le son —
//! `{ id, name, amount, delay_ms, created_at }` — jamais la case « activé » : il
//! se choisit pour une zone, il ne l'allume pas à sa place.
//!
//! #5081 — l'ombre de la tête fait aussi le son : un préréglage porte
//! `{ head_shadow_enabled, cutoff_hz, slope_db_per_octave }`. Un préréglage
//! enregistré avant, sans ces champs, se relit filtre ÉTEINT
//! (`GET /crossfeed/presets` les complète) — jamais avec le filtre de la zone.
//!
//! APPLIQUER n'a pas de route ici : le client envoie les valeurs du préréglage
//! à `PUT /zones/{id}/dsp`, le seul chemin qui persiste le réglage de la zone,
//! le fait entendre à chaud et publie `crossfeed_status`. Une seconde route
//! d'application aurait dû recopier tout cela.
//!
//! Sauvegarde de configuration : `crossfeed_presets` est une ligne de réglages
//! ordinaire, donc exportée et restaurée avec les autres
//! (`tune_core::config_backup`, témoin dans ses tests) — le passage du crossfeed
//! en greffon (#4363, préavis #4440) ne la perd pas.
//!
//! Droits : lister est libre (comme `/eq/presets`) ; enregistrer et supprimer
//! demandent le crossfeed Premium ET le greffon installé, comme
//! `PUT /zones/{id}/dsp` quand il porte un `crossfeed`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::error::AppError;
use crate::state::AppState;

/// La ligne de réglages qui porte la liste.
pub const CLE: &str = "crossfeed_presets";

/// Longueur maximale d'un nom, en caractères.
const NOM_MAX: usize = 64;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/presets", get(lister).post(enregistrer))
        .route("/presets/{id}", delete(supprimer))
}

fn charger(state: &AppState) -> Vec<Value> {
    SettingsRepo::with_backend(state.backend.clone())
        .get(CLE)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn ecrire(state: &AppState, presets: &[Value]) -> Result<(), AppError> {
    SettingsRepo::with_backend(state.backend.clone())
        .set(CLE, &serde_json::to_string(presets)?)
        .map_err(AppError::internal)?;
    Ok(())
}

/// Les deux gardes d'écriture, dans l'ordre de `PUT /zones/{id}/dsp`.
async fn autoriser(state: &AppState, headers: &axum::http::HeaderMap) -> Result<(), Response> {
    crate::premium_guard::require_premium_localise(
        &state.license,
        tune_core::license::Feature::Crossfeed,
        headers,
    )
    .await?;
    crate::premium_audio_plugins::require_installed(state, "crossfeed")
}

/// `GET /crossfeed/presets`
async fn lister(State(state): State<AppState>) -> Json<Value> {
    let presets: Vec<Value> = charger(&state).into_iter().map(completer).collect();
    Json(json!({ "presets": presets }))
}

/// #5081 — un préréglage d'avant l'ombre de la tête se relit filtre éteint.
fn completer(mut preset: Value) -> Value {
    let ombre = crate::routes::zones::ombre_normalisee(&preset, &Value::Null);
    if let (Some(p), Some(o)) = (preset.as_object_mut(), ombre.as_object()) {
        for (cle, valeur) in o {
            p.insert(cle.clone(), valeur.clone());
        }
    }
    preset
}

#[derive(Deserialize)]
struct CorpsPreset {
    name: String,
    amount: f64,
    delay_ms: f64,
    // #5081 — facultatifs : absents, le préréglage est enregistré filtre
    // éteint (700 Hz, 6 dB/oct).
    head_shadow_enabled: Option<bool>,
    cutoff_hz: Option<f64>,
    slope_db_per_octave: Option<f64>,
}

/// `POST /crossfeed/presets` — enregistre les valeurs sous ce nom.
///
/// Un nom déjà pris (casse et espaces ignorés) est MIS À JOUR, même `id` :
/// « enregistrer » deux fois « Salon » ne doit pas laisser deux « Salon »
/// indiscernables dans la liste. 201 à la création, 200 à la mise à jour.
/// Les valeurs sont bornées comme celles de la zone : un préréglage ne peut pas
/// promettre ce que `PUT /zones/{id}/dsp` rognerait ensuite.
async fn enregistrer(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(corps): Json<CorpsPreset>,
) -> Result<Response, AppError> {
    if let Err(refus) = autoriser(&state, &headers).await {
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
    let (amount, delay_ms) = tune_core::audio::crossfeed::borner(corps.amount, corps.delay_ms);
    let ombre = crate::routes::zones::ombre_normalisee(
        &json!({
            "head_shadow_enabled": corps.head_shadow_enabled,
            "cutoff_hz": corps.cutoff_hz,
            "slope_db_per_octave": corps.slope_db_per_octave,
        }),
        &Value::Null,
    );

    let mut presets = charger(&state);
    let meme_nom = |p: &Value| {
        p["name"]
            .as_str()
            .is_some_and(|n| n.trim().to_lowercase() == nom.to_lowercase())
    };
    let (statut, preset) = match presets.iter_mut().find(|p| meme_nom(p)) {
        Some(existant) => {
            existant["name"] = json!(nom);
            existant["amount"] = json!(amount);
            existant["delay_ms"] = json!(delay_ms);
            existant["head_shadow_enabled"] = ombre["head_shadow_enabled"].clone();
            existant["cutoff_hz"] = ombre["cutoff_hz"].clone();
            existant["slope_db_per_octave"] = ombre["slope_db_per_octave"].clone();
            (StatusCode::OK, existant.clone())
        }
        None => {
            let preset = json!({
                "id": uuid::Uuid::new_v4().to_string(),
                "name": nom,
                "amount": amount,
                "delay_ms": delay_ms,
                "head_shadow_enabled": ombre["head_shadow_enabled"],
                "cutoff_hz": ombre["cutoff_hz"],
                "slope_db_per_octave": ombre["slope_db_per_octave"],
                "created_at": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            });
            presets.push(preset.clone());
            (StatusCode::CREATED, preset)
        }
    };
    ecrire(&state, &presets)?;
    Ok((statut, Json(preset)).into_response())
}

/// `DELETE /crossfeed/presets/{id}`
async fn supprimer(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Response, AppError> {
    if let Err(refus) = autoriser(&state, &headers).await {
        return Ok(refus);
    }
    let mut presets = charger(&state);
    let avant = presets.len();
    presets.retain(|p| p["id"].as_str() != Some(id.as_str()));
    if presets.len() == avant {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "preset not found"})),
        )
            .into_response());
    }
    ecrire(&state, &presets)?;
    Ok(Json(json!({ "deleted": id })).into_response())
}
