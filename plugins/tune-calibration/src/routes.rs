//! Les routes du greffon, montées par l'hôte sous `/api/v1/ext/calibration`.
//!
//! * `GET  /microphones` — les entrées audio (micros) et les étalonnages déposés ;
//! * `POST /microphones/etalonnages` — déposer un fichier d'étalonnage
//!   (`{ "nom", "texte" }`, texte miniDSP ou REW `.cal`) ;
//! * `POST /zones/{zone}/mesures` — lancer une mesure (202, `{ "id" }`) ;
//! * `GET  /mesures` — toutes les mesures connues (avancement) ;
//! * `GET  /mesures/{id}` — l'avancement d'une mesure ;
//! * `GET  /mesures/{id}/resultat` — le résultat (409 tant qu'elle n'est pas
//!   terminée) ;
//! * `GET  /mesures/{id}/ri.wav` — la réponse impulsionnelle (WAV float32) ;
//! * `DELETE /mesures/{id}` — annuler.
//!
//! Aucune lecture ne part sans le `POST` explicite de l'utilisateur.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::generateur::Voies;
use crate::mesure::{Controleur, Demande};

#[derive(Clone)]
pub struct EtatRoutes {
    pub controleur: Arc<Controleur>,
}

pub fn router(etat: EtatRoutes) -> Router<()> {
    Router::new()
        .route("/microphones", get(microphones))
        .route("/microphones/etalonnages", post(deposer_etalonnage))
        .route("/zones/{zone}/mesures", post(lancer))
        .route("/mesures", get(mesures))
        .route("/mesures/{id}", get(avancement).delete(annuler))
        .route("/mesures/{id}/resultat", get(resultat))
        .route("/mesures/{id}/ri.wav", get(ri_wav))
        .with_state(etat)
}

fn refus(code: StatusCode, motif: &str, message: String) -> Response {
    (code, Json(json!({ "error": motif, "message": message }))).into_response()
}

async fn microphones(State(etat): State<EtatRoutes>) -> Response {
    let c = etat.controleur.clone();
    let (entrees, pile) =
        tokio::task::spawn_blocking(move || (c.peripheriques.lister(), c.peripheriques.pile()))
            .await
            .unwrap_or_else(|e| (Err(e.to_string()), "inconnue"));
    let entrees: Vec<Value> = match entrees {
        Ok(l) => l
            .into_iter()
            .map(|e| {
                json!({
                    "nom": e.nom,
                    "id": e.id,
                    "canaux": e.canaux,
                    "frequence_courante": e.frequence_courante,
                    "frequences": e.frequences,
                    "bits_servis": e.bits_servis,
                    "virtuelle": e.virtuelle,
                    "mesurable": e.frequence_courante == Some(crate::mesure::FREQUENCE_MESURE),
                })
            })
            .collect(),
        Err(e) => {
            return refus(StatusCode::SERVICE_UNAVAILABLE, "capture_indisponible", e);
        }
    };
    Json(json!({
        "pile": pile,
        "frequence_requise": crate::mesure::FREQUENCE_MESURE,
        "entrees": entrees,
        "etalonnages": etat.controleur.etalonnages(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct DepotEtalonnage {
    nom: String,
    texte: String,
}

async fn deposer_etalonnage(
    State(etat): State<EtatRoutes>,
    Json(d): Json<DepotEtalonnage>,
) -> Response {
    if d.texte.len() > 512 * 1024 {
        return refus(
            StatusCode::PAYLOAD_TOO_LARGE,
            "etalonnage_trop_grand",
            "512 Ko au plus".into(),
        );
    }
    match etat.controleur.deposer_etalonnage(&d.nom, &d.texte) {
        Ok(cal) => {
            let (lo, hi) = cal.range();
            (
                StatusCode::CREATED,
                Json(json!({
                    "nom": d.nom.trim(),
                    "points": cal.freqs.len(),
                    "plage_hz": [lo, hi],
                    "sens_factor_db": cal.sens_factor_db,
                    "serie": cal.serial,
                })),
            )
                .into_response()
        }
        Err(e) => refus(StatusCode::BAD_REQUEST, "etalonnage_invalide", e),
    }
}

#[derive(Deserialize, Default)]
struct DemandeMesure {
    entree: String,
    #[serde(default)]
    voie_micro: usize,
    etalonnage: Option<String>,
    duree_s: Option<f64>,
    volume: Option<f64>,
    /// `gauche`, `droite` ou `deux` (défaut).
    voies: Option<String>,
    /// Plafonds plus stricts seulement ; les valeurs par défaut sont des maxima.
    max_spl_db: Option<f64>,
    max_play_dbfs: Option<f64>,
}

async fn lancer(
    State(etat): State<EtatRoutes>,
    Path(zone): Path<i64>,
    Json(d): Json<DemandeMesure>,
) -> Response {
    if d.entree.trim().is_empty() {
        return refus(
            StatusCode::BAD_REQUEST,
            "entree_requise",
            "préciser l'entrée audio du micro".into(),
        );
    }
    let voies = match d.voies.as_deref() {
        None => Voies::LesDeux,
        Some(v) => match Voies::depuis(v) {
            Some(v) => v,
            None => {
                return refus(
                    StatusCode::BAD_REQUEST,
                    "voies_invalides",
                    "voies : gauche, droite ou deux".into(),
                );
            }
        },
    };
    let mut demande = Demande::defaut(zone, d.entree.trim());
    demande.voie_micro = d.voie_micro;
    demande.etalonnage = d.etalonnage.filter(|s| !s.trim().is_empty());
    if let Some(s) = d.duree_s {
        demande.duree_s = s;
    }
    if let Some(v) = d.volume {
        demande.volume_mesure_pct = v;
    }
    if let Some(s) = d.max_spl_db {
        demande.politique.max_spl_db = s;
    }
    if let Some(p) = d.max_play_dbfs {
        demande.politique.max_play_dbfs = p;
    }
    demande.voies = voies;
    let c = etat.controleur.clone();
    let r = tokio::task::spawn_blocking(move || c.lancer(demande))
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
    match r {
        Ok(id) => (
            StatusCode::ACCEPTED,
            Json(json!({ "id": id, "zone_id": zone })),
        )
            .into_response(),
        Err(e) if e.contains("déjà en cours") => {
            refus(StatusCode::CONFLICT, "mesure_en_cours", e)
        }
        Err(e) => refus(StatusCode::UNPROCESSABLE_ENTITY, "mesure_refusee", e),
    }
}

async fn mesures(State(etat): State<EtatRoutes>) -> Response {
    let liste: Vec<Value> = etat
        .controleur
        .mesures()
        .iter()
        .map(|m| serde_json::to_value(m.avancement()).unwrap_or(Value::Null))
        .collect();
    Json(json!({ "mesures": liste })).into_response()
}

async fn avancement(State(etat): State<EtatRoutes>, Path(id): Path<String>) -> Response {
    match etat.controleur.mesure(&id) {
        Some(m) => Json(m.avancement()).into_response(),
        None => refus(
            StatusCode::NOT_FOUND,
            "mesure_inconnue",
            format!("aucune mesure « {id} »"),
        ),
    }
}

async fn resultat(State(etat): State<EtatRoutes>, Path(id): Path<String>) -> Response {
    let Some(m) = etat.controleur.mesure(&id) else {
        return refus(
            StatusCode::NOT_FOUND,
            "mesure_inconnue",
            format!("aucune mesure « {id} »"),
        );
    };
    match m.resultat() {
        Some(r) => {
            let mut v = serde_json::to_value(&*r).unwrap_or(Value::Null);
            if let Some(o) = v.as_object_mut() {
                o.insert("id".into(), json!(m.id));
                o.insert("zone_id".into(), json!(m.demande.zone_id));
                o.insert("entree".into(), json!(m.demande.entree));
            }
            Json(v).into_response()
        }
        None => {
            let etat_m = m.etat();
            refus(
                StatusCode::CONFLICT,
                "mesure_sans_resultat",
                format!(
                    "mesure « {id} » : {}",
                    serde_json::to_string(&etat_m).unwrap_or_default()
                ),
            )
        }
    }
}

async fn ri_wav(State(etat): State<EtatRoutes>, Path(id): Path<String>) -> Response {
    let Some(m) = etat.controleur.mesure(&id) else {
        return refus(
            StatusCode::NOT_FOUND,
            "mesure_inconnue",
            format!("aucune mesure « {id} »"),
        );
    };
    match m.ri_wav() {
        Some(w) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "audio/wav".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"calibration-{id}-ri.wav\""),
                ),
            ],
            (*w).clone(),
        )
            .into_response(),
        None => refus(
            StatusCode::CONFLICT,
            "mesure_sans_resultat",
            "la mesure n'est pas terminée".into(),
        ),
    }
}

async fn annuler(State(etat): State<EtatRoutes>, Path(id): Path<String>) -> Response {
    match etat.controleur.mesure(&id) {
        None => refus(
            StatusCode::NOT_FOUND,
            "mesure_inconnue",
            format!("aucune mesure « {id} »"),
        ),
        Some(m) => {
            let annulee = etat.controleur.annuler(&id);
            Json(json!({ "id": id, "annulation_demandee": annulee, "etat": m.etat() }))
                .into_response()
        }
    }
}
