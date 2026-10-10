//! #6044 — le greffon « Réaffectation des canaux » : `/channel-remap`.
//!
//! - `GET /presets` : les préréglages (4.0 → stéréo, 4.0 → 5.1, 4.0 → 7.1,
//!   5.1 → stéréo ITU, échange gauche/droite, mono, identité), chacun sous sa
//!   forme complète — l'écran les charge dans la grille, il ne les recalcule
//!   pas — et les noms des canaux par défaut (FL FR BL BR…).
//! - `GET|PUT|DELETE /zones/{id}` : le réglage d'une zone
//!   (`zone_{id}_channel_remap`). `PUT` le valide (forme N × M, gains finis
//!   dans [−60, +12] dB ou `null` pour muet), l'enregistre et le fait entendre
//!   à chaud sur une sortie locale.
//! - `GET|PUT|DELETE /albums/{id}` : la règle d'un album
//!   (`album_{id}_channel_remap`, #5279), prioritaire sur celle de la zone.
//!
//! Chaque réponse porte `effective` : ce que vaut la matrice (gains linéaires
//! après normalisation, atténuation par sortie, recopie au bit près ou
//! mélange). L'écran l'affiche tel quel.
//!
//! Droits : GRATUIT (décision de Bertrand du 10/10/2026), mais FACULTATIF
//! comme l'égaliseur : le greffon `channel-remap` s'installe depuis le
//! catalogue (`POST /plugins/channel-remap/install`, droit `dsp_eq`). Lire est
//! libre ; écrire exige le greffon installé et activé (409
//! `plugin_unavailable` sinon), et l'hôte ne l'applique qu'installé.
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use tune_core::audio::reaffectation_canaux as rc;
use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/presets", get(lister_les_prereglages))
        .route(
            "/zones/{id}",
            get(lire_zone).put(ecrire_zone).delete(effacer_zone),
        )
        .route(
            "/albums/{id}",
            get(lire_album).put(ecrire_album).delete(effacer_album),
        )
}

/// Libellé français d'un préréglage (l'écran a ses propres traductions ; ce
/// libellé sert aux clients qui n'en ont pas).
fn libelle(id: &str) -> &'static str {
    match id {
        "quad_to_stereo" => "4.0 → stéréo (mixage)",
        "quad_to_5_1" => "4.0 → 5.1 (avant et arrière, centre et LFE muets)",
        "quad_to_7_1" => "4.0 → 7.1 (arrière sur BL/BR)",
        "5_1_to_stereo_itu" => "5.1 → stéréo (ITU-R BS.775)",
        "swap_lr" => "Échange gauche/droite",
        "mono" => "Mono (G + D) / 2",
        _ => "Identité",
    }
}

fn noms(n: u16) -> Value {
    rc::noms_des_canaux(n).map_or(Value::Null, |noms| json!(noms))
}

/// Ce que vaut un réglage : sa matrice effective, ou pourquoi il est refusé.
pub(crate) fn effective(reglage: &rc::ChannelRemapSettings) -> Value {
    match rc::Matrice::depuis_reglage(reglage) {
        Ok(m) => {
            let (n, s) = (usize::from(m.entrees()), usize::from(m.sorties()));
            let gains: Vec<Vec<f64>> = (0..s)
                .map(|o| (0..n).map(|i| m.coefficient(o, i)).collect())
                .collect();
            json!({
                "valid": true,
                "identity": m.est_identite(),
                "bit_exact_copy": m.est_recopie(),
                "linear_gains": gains,
                "normalization_db": m.attenuation_db(),
                // Une matrice change le contenu des voies : jamais bit-perfect,
                // sauf l'identité, qui ne s'applique pas.
                "bit_perfect": m.est_identite() || !reglage.enabled,
            })
        }
        Err(e) => json!({ "valid": false, "error": format!("{e:?}") }),
    }
}

fn forme(cle: &str, reglage: Option<rc::ChannelRemapSettings>) -> Value {
    let enregistre = reglage.is_some();
    let reglage = reglage.unwrap_or_default();
    json!({
        "key": cle,
        "saved": enregistre,
        "settings": reglage,
        "effective": effective(&reglage),
        "input_names": noms(reglage.inputs),
        "output_names": noms(reglage.outputs),
    })
}

async fn lister_les_prereglages() -> Json<Value> {
    let prereglages: Vec<Value> = rc::PREREGLAGES
        .iter()
        .filter_map(|id| rc::prereglage(id).map(|s| (id, s)))
        .map(|(id, s)| {
            json!({
                "id": id,
                "label": libelle(id),
                "input_names": noms(s.inputs),
                "output_names": noms(s.outputs),
                "effective": effective(&s),
                "settings": s,
            })
        })
        .collect();
    Json(json!({
        "presets": prereglages,
        "limits": {
            "max_channels": rc::CANAUX_MAX,
            "gain_min_db": rc::GAIN_MIN_DB,
            "gain_max_db": rc::GAIN_MAX_DB,
        },
    }))
}

fn refus(code: &str, detail: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": detail, "code": code })),
    )
        .into_response()
}

/// Valider un corps de `PUT` : le réglage complet, refusé s'il est mal formé.
fn valider(corps: Value) -> Result<rc::ChannelRemapSettings, Response> {
    let reglage: rc::ChannelRemapSettings = serde_json::from_value(corps)
        .map_err(|e| refus("channel_remap_invalid_body", e.to_string()))?;
    rc::Matrice::depuis_reglage(&reglage).map_err(|e| {
        let code = match e {
            rc::ErreurDeMatrice::Canaux => "channel_remap_invalid_channels",
            rc::ErreurDeMatrice::Forme => "channel_remap_invalid_shape",
            rc::ErreurDeMatrice::Gain => "channel_remap_invalid_gain",
        };
        refus(code, format!("{e:?}"))
    })?;
    Ok(reglage)
}

fn settings(state: &AppState) -> SettingsRepo {
    SettingsRepo::with_backend(state.backend.clone())
}

async fn lire_zone(State(state): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    let cle = rc::cle_de_zone(id);
    let mut v = forme(&cle, rc::lire(&settings(&state), &cle));
    v["zone_id"] = json!(id);
    Json(v)
}

async fn ecrire_zone(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(corps): Json<Value>,
) -> Response {
    // Greffon facultatif (gratuit) : écrire exige qu'il soit installé et
    // activé, comme l'égaliseur (`PUT /zones/{id}/dsp`).
    if let Err(r) = crate::premium_audio_plugins::require_installed(&state, rc::ID_GREFFON) {
        return r;
    }
    let reglage = match valider(corps) {
        Ok(r) => r,
        Err(r) => return r,
    };
    let cle = rc::cle_de_zone(id);
    if let Err(e) = settings(&state).set(&cle, &serde_json::to_string(&reglage).unwrap_or_default())
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response();
    }
    let a_chaud = state.orchestrator.refresh_zone_reaffectation(id).await;
    let mut v = forme(&cle, Some(reglage));
    v["zone_id"] = json!(id);
    v["applied_live"] = json!(a_chaud);
    Json(v).into_response()
}

async fn effacer_zone(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let cle = rc::cle_de_zone(id);
    let _ = settings(&state).delete(&cle);
    let a_chaud = state.orchestrator.refresh_zone_reaffectation(id).await;
    let mut v = forme(&cle, None);
    v["zone_id"] = json!(id);
    v["applied_live"] = json!(a_chaud);
    Json(v).into_response()
}

async fn lire_album(State(state): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    let cle = rc::cle_d_album(id);
    let mut v = forme(&cle, rc::lire(&settings(&state), &cle));
    v["album_id"] = json!(id);
    Json(v)
}

async fn ecrire_album(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(corps): Json<Value>,
) -> Response {
    // Greffon facultatif (gratuit) : écrire exige qu'il soit installé et
    // activé, comme l'égaliseur (`PUT /zones/{id}/dsp`).
    if let Err(r) = crate::premium_audio_plugins::require_installed(&state, rc::ID_GREFFON) {
        return r;
    }
    let reglage = match valider(corps) {
        Ok(r) => r,
        Err(r) => return r,
    };
    let cle = rc::cle_d_album(id);
    if let Err(e) = settings(&state).set(&cle, &serde_json::to_string(&reglage).unwrap_or_default())
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response();
    }
    let mut v = forme(&cle, Some(reglage));
    v["album_id"] = json!(id);
    Json(v).into_response()
}

async fn effacer_album(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let cle = rc::cle_d_album(id);
    let _ = settings(&state).delete(&cle);
    let mut v = forme(&cle, None);
    v["album_id"] = json!(id);
    Json(v).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_reglage_mal_forme_est_refuse_avec_son_code() {
        let r = valider(json!({"enabled": true, "inputs": 2, "outputs": 2, "gains_db": [[0.0]]}));
        assert_eq!(r.unwrap_err().status(), StatusCode::BAD_REQUEST);
        let r = valider(json!({"enabled": true, "inputs": 2, "outputs": 2,
            "gains_db": [[0.0, null], [null, 40.0]]}));
        assert!(r.is_err(), "+40 dB hors bornes");
        let ok = valider(serde_json::to_value(rc::prereglage("quad_to_5_1").unwrap()).unwrap());
        assert_eq!(ok.unwrap().outputs, 6);
    }

    #[test]
    fn l_effectif_d_un_prereglage_dit_recopie_ou_melange() {
        let e = effective(&rc::prereglage("quad_to_5_1").unwrap());
        assert_eq!(e["bit_exact_copy"], true);
        assert_eq!(e["bit_perfect"], false);
        let e = effective(&rc::prereglage("quad_to_stereo").unwrap());
        assert_eq!(e["bit_exact_copy"], false);
        assert!(e["normalization_db"][0].as_f64().unwrap() < -4.0);
    }
}
