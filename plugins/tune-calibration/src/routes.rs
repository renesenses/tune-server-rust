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
//! * `DELETE /mesures/{id}` — annuler ;
//! * `POST /mesures/{id}/correction` — proposer un égaliseur (C2) : cible,
//!   zone de correction, bandes ; rien n'est écrit ;
//! * `POST /mesures/{id}/correction/profil` — même calcul, enregistré comme
//!   profil de pièce de la zone ; il s'applique ensuite par
//!   `POST /api/v1/room-correction/profiles/{zone}/apply`.
//! * `POST /simulations` — simuler les modes de basse d'une pièce
//!   rectangulaire sans micro (C5) : dimensions, surfaces, positions ;
//!   modes, réponse estimée à la place d'écoute (201, `{ "id", … }`) ;
//! * `POST /simulations/{id}/correction` — proposition C2 sur la réponse
//!   estimée, plus prudente que sur mesure, marquée estimation ; avec
//!   `"expert": true` (#8332), maxima d'une mesure, défauts inchangés ;
//! * `POST /simulations/{id}/correction/profil` — la même, enregistrée comme
//!   profil de pièce de la zone (`zone_id` de la simulation ou du corps).
//! * `POST /zones/{zone}/sessions` — session à plusieurs positions (C4) :
//!   `{ "positions": 3..9, "centrale", "ponderation", "moyenne", "rejet" }`
//!   (201) ;
//! * `GET  /sessions` — les sessions connues ;
//! * `GET  /sessions/{id}` — positions, état de chaque mesure, verdicts de
//!   rejet et moyenne pondérée (dès 3 positions dont la centrale) ;
//! * `POST /sessions/{id}/positions` — mesurer une position : corps de
//!   `POST /zones/{zone}/mesures`, plus `index` (défaut : la première libre ;
//!   une position mesurée se refait en la nommant) et `etiquette` (202) ;
//! * `GET  /sessions/{id}/export` — le document versionné (v2) ;
//! * `POST /sessions/import` — relire un document v2, ou v1 (le résultat
//!   mono-position de `GET /mesures/{id}/resultat`) (201) ; une version plus
//!   récente donne 409 `unsupported_version` ;
//! * `POST /sessions/{id}/correction` et `/correction/profil` — C2 sur la
//!   moyenne (défauts d'une moyenne spatiale : 20–500 Hz, 10 bandes) ;
//! * `POST /sessions/{id}/fir` et `POST /mesures/{id}/fir` — FIR de
//!   correction à phase mixte (C3, T10b) : rapport (courbe, latence, bornes) ;
//! * `POST /sessions/{id}/fir.wav` et `POST /mesures/{id}/fir.wav` — le même
//!   FIR en RI `.wav` flottante, pour la convolution ;
//! * `POST /comparaisons` — A/B : `{ "avant", "apres" }` (ids de mesure ou de
//!   session), `f_min_hz`, `f_max_hz`, `cible` facultatifs.
//!
//! Aucune lecture ne part sans le `POST` explicite de l'utilisateur.
//!
//! **Tout le greffon est Premium** (décision de Bertrand du 04/11/2026) :
//! chaque route passe par [`garde_premium`], posée sur le routeur entier —
//! une route ajoutée plus tard ne peut pas l'oublier. Sans le droit
//! [`DROIT`], la réponse est un refus nommé `premium_required` (402), avant
//! toute lecture du micro et tout son joué.

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::license::{Feature, LicenseManager};

use crate::correction::{DemandeCorrection, Proposition, proposer};
use crate::correction_fir::{DemandeFir, RapportFir};
use crate::generateur::Voies;
use crate::mesure::{Controleur, Demande};
use crate::modes::{Entree, proposer_estimation, simuler};
use crate::session::{DemandeComparaison, DemandeSession, ErreurLecture};

#[derive(Clone)]
pub struct EtatRoutes {
    pub controleur: Arc<Controleur>,
    /// La licence de l'hôte (`PluginContext::license`). `None` : aucune
    /// licence fournie, ce qui ne vaut PAS Premium.
    pub license: Option<Arc<LicenseManager>>,
}

/// Le droit qui ouvre le greffon : celui de la correction de pièce.
pub const DROIT: Feature = Feature::RoomCorrection;

/// LE point de décision du payant, comme `tune_concerts::acces`.
///
/// ⚠️ Une licence ABSENTE ne vaut pas une autorisation : un hôte qui ne la
/// branche pas (essais, construction future) ouvrirait sinon le greffon à
/// tout le monde.
pub async fn premium(license: Option<&LicenseManager>) -> bool {
    match license {
        Some(l) => l.check_feature(DROIT).await,
        None => false,
    }
}

/// Le refus nommé, dans la forme de `premium_guard::corps_du_refus` du
/// serveur : `error` stable, `code` du droit, `message` en anglais (le client
/// traduit le `code`).
pub fn refus_premium() -> Response {
    (
        StatusCode::PAYMENT_REQUIRED,
        Json(json!({
            "error": "premium_required",
            "code": DROIT.code(),
            "feature": DROIT.display_name(),
            "message": format!("{} requires Tune Premium", DROIT.display_name()),
        })),
    )
        .into_response()
}

async fn garde_premium(State(etat): State<EtatRoutes>, requete: Request, suite: Next) -> Response {
    if premium(etat.license.as_deref()).await {
        suite.run(requete).await
    } else {
        tracing::info!(chemin = %requete.uri().path(), "calibration_refusee_sans_premium");
        refus_premium()
    }
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
        .route("/mesures/{id}/correction", post(correction))
        .route("/mesures/{id}/correction/profil", post(correction_profil))
        .route("/zones/{zone}/sessions", post(creer_session))
        .route("/sessions", get(sessions))
        .route(
            "/sessions/import",
            post(importer_session).layer(DefaultBodyLimit::max(crate::session::IMPORT_MAX_OCTETS)),
        )
        .route("/sessions/{id}", get(vue_session))
        .route("/sessions/{id}/positions", post(mesurer_position))
        .route("/sessions/{id}/export", get(exporter_session))
        .route("/sessions/{id}/correction", post(session_correction))
        .route(
            "/sessions/{id}/correction/profil",
            post(session_correction_profil),
        )
        .route("/mesures/{id}/fir", post(fir_rapport))
        .route("/mesures/{id}/fir.wav", post(fir_wav))
        .route("/sessions/{id}/fir", post(fir_rapport))
        .route("/sessions/{id}/fir.wav", post(fir_wav))
        .route("/comparaisons", post(comparer))
        .route("/simulations", post(simulation))
        .route("/simulations/{id}/correction", post(simulation_correction))
        .route(
            "/simulations/{id}/correction/profil",
            post(simulation_correction_profil),
        )
        .route_layer(from_fn_with_state(etat.clone(), garde_premium))
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

// Le refus est la réponse HTTP prête, comme `proposition_pour`.
#[allow(clippy::result_large_err)]
fn demande_de(zone: i64, d: DemandeMesure) -> Result<Demande, Response> {
    if d.entree.trim().is_empty() {
        return Err(refus(
            StatusCode::BAD_REQUEST,
            "entree_requise",
            "préciser l'entrée audio du micro".into(),
        ));
    }
    let voies = match d.voies.as_deref() {
        None => Voies::LesDeux,
        Some(v) => match Voies::depuis(v) {
            Some(v) => v,
            None => {
                return Err(refus(
                    StatusCode::BAD_REQUEST,
                    "voies_invalides",
                    "voies : gauche, droite ou deux".into(),
                ));
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
    Ok(demande)
}

async fn lancer(
    State(etat): State<EtatRoutes>,
    Path(zone): Path<i64>,
    Json(d): Json<DemandeMesure>,
) -> Response {
    let demande = match demande_de(zone, d) {
        Ok(d) => d,
        Err(r) => return r,
    };
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

/// La proposition C2 pour une mesure terminée, avec ce que la mesure dit de
/// la zone (égaliseur ou IR actifs pendant la mesure, PURE).
fn proposition_pour(
    etat: &EtatRoutes,
    id: &str,
    demande: &DemandeCorrection,
) -> Result<(i64, Proposition), Response> {
    let Some(m) = etat.controleur.mesure(id) else {
        return Err(refus(
            StatusCode::NOT_FOUND,
            "mesure_inconnue",
            format!("aucune mesure « {id} »"),
        ));
    };
    let Some(r) = m.resultat() else {
        return Err(refus(
            StatusCode::CONFLICT,
            "mesure_sans_resultat",
            "la mesure n'est pas terminée".into(),
        ));
    };
    let reponse = tune_calibration::analysis::LogResponse {
        freqs: r.reponse.freqs_hz.clone(),
        magnitude_db: r.reponse.db.clone(),
    };
    let mut p = proposer(&reponse, demande)
        .map_err(|e| refus(StatusCode::UNPROCESSABLE_ENTITY, "correction_refusee", e))?;
    let t = &r.traitements_zone;
    if t.egaliseur || t.correction_ir {
        p.avertissements.push(
            "l'égaliseur ou la correction IR de la zone était actif pendant la mesure : la proposition s'y ajoute, refaire la mesure sans eux"
                .into(),
        );
    }
    if t.pure {
        p.avertissements.push(
            "la zone est en PURE : aucune correction ne s'y entend tant que PURE est actif".into(),
        );
    }
    Ok((m.demande.zone_id, p))
}

async fn correction(
    State(etat): State<EtatRoutes>,
    Path(id): Path<String>,
    Json(d): Json<DemandeCorrection>,
) -> Response {
    match proposition_pour(&etat, &id, &d) {
        Ok((zone_id, p)) => {
            let mut v = serde_json::to_value(&p).unwrap_or(Value::Null);
            if let Some(o) = v.as_object_mut() {
                o.insert("id".into(), json!(id));
                o.insert("zone_id".into(), json!(zone_id));
            }
            Json(v).into_response()
        }
        Err(r) => r,
    }
}

#[derive(Deserialize)]
struct DemandeProfil {
    #[serde(flatten)]
    demande: DemandeCorrection,
    nom: Option<String>,
}

async fn correction_profil(
    State(etat): State<EtatRoutes>,
    Path(id): Path<String>,
    Json(d): Json<DemandeProfil>,
) -> Response {
    let (zone_id, p) = match proposition_pour(&etat, &id, &d.demande) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let nom = nom_du_profil(d.nom, || format!("Calibration {id}"));
    enregistrer_le_profil(&etat, &id, zone_id, &nom, p, false).await
}

fn nom_du_profil(nom: Option<String>, defaut: impl FnOnce() -> String) -> String {
    nom.map(|n| n.trim().chars().take(80).collect::<String>())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(defaut)
}

/// Enregistre la proposition comme profil de pièce de la zone (C2 et C5).
async fn enregistrer_le_profil(
    etat: &EtatRoutes,
    id: &str,
    zone_id: i64,
    nom: &str,
    p: Proposition,
    estimation: bool,
) -> Response {
    if p.bandes.is_empty() {
        return refus(
            StatusCode::UNPROCESSABLE_ENTITY,
            "correction_vide",
            "aucune bande à enregistrer : rien à couper dans la zone".into(),
        );
    }
    let profil = p.profil_de_zone(zone_id, nom, &chrono::Utc::now().to_rfc3339());
    let hote = etat.controleur.hote.clone();
    let a_ecrire = profil.clone();
    let ecrit = tokio::task::spawn_blocking(move || hote.enregistrer_profil_piece(&a_ecrire))
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
    if let Err(e) = ecrit {
        return refus(
            StatusCode::INTERNAL_SERVER_ERROR,
            "profil_non_enregistre",
            e,
        );
    }
    (
        StatusCode::CREATED,
        Json(json!({
            "id": id,
            "zone_id": zone_id,
            "profil": profil,
            "proposition": p,
            "estimation": estimation,
            // L'application est un second geste, par la route qui écrit
            // l'égaliseur de la zone et rafraîchit la sortie qui joue.
            "appliquer": format!("/api/v1/room-correction/profiles/{zone_id}/apply"),
        })),
    )
        .into_response()
}

/// Les entrées d'une simulation, avec un refus nommé quand elles ne
/// décrivent pas une pièce.
async fn simulation(State(etat): State<EtatRoutes>, Json(e): Json<Entree>) -> Response {
    let r = tokio::task::spawn_blocking(move || simuler(&e))
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
    match r {
        Ok(sim) => {
            let mut v = serde_json::to_value(&sim).unwrap_or(Value::Null);
            let id = etat.controleur.simulations.ranger(sim);
            tracing::info!(simulation = %id, "calibration_simulation_calculee");
            if let Some(o) = v.as_object_mut() {
                o.insert("id".into(), json!(id));
            }
            (StatusCode::CREATED, Json(v)).into_response()
        }
        Err(e) => refus(StatusCode::BAD_REQUEST, "simulation_invalide", e),
    }
}

// Même forme que `proposition_pour` : le refus est la réponse HTTP prête.
#[allow(clippy::result_large_err)]
fn proposition_estimee(
    etat: &EtatRoutes,
    id: &str,
    demande: &DemandeCorrection,
    expert: bool,
) -> Result<(Option<i64>, Proposition), Response> {
    let Some(sim) = etat.controleur.simulations.simulation(id) else {
        return Err(refus(
            StatusCode::NOT_FOUND,
            "simulation_inconnue",
            format!("aucune simulation « {id} »"),
        ));
    };
    let p = proposer_estimation(&sim, demande, expert)
        .map_err(|e| refus(StatusCode::UNPROCESSABLE_ENTITY, "correction_refusee", e))?;
    Ok((sim.entree.zone_id, p))
}

/// La demande de correction d'une estimation : celle de C2, plus le mode
/// expert (#8332), faux par défaut.
#[derive(Deserialize)]
struct DemandeEstimee {
    #[serde(flatten)]
    demande: DemandeCorrection,
    #[serde(default)]
    expert: bool,
}

async fn simulation_correction(
    State(etat): State<EtatRoutes>,
    Path(id): Path<String>,
    Json(d): Json<DemandeEstimee>,
) -> Response {
    match proposition_estimee(&etat, &id, &d.demande, d.expert) {
        Ok((zone_id, p)) => {
            let mut v = serde_json::to_value(&p).unwrap_or(Value::Null);
            if let Some(o) = v.as_object_mut() {
                o.insert("id".into(), json!(id));
                o.insert("zone_id".into(), json!(zone_id));
                o.insert("estimation".into(), json!(true));
                o.insert("expert".into(), json!(d.expert));
            }
            Json(v).into_response()
        }
        Err(r) => r,
    }
}

#[derive(Deserialize)]
struct DemandeProfilEstime {
    #[serde(flatten)]
    demande: DemandeCorrection,
    nom: Option<String>,
    /// Remplace la zone donnée à la simulation.
    zone_id: Option<i64>,
    /// Bornes relevées à celles d'une mesure (#8332).
    #[serde(default)]
    expert: bool,
}

async fn simulation_correction_profil(
    State(etat): State<EtatRoutes>,
    Path(id): Path<String>,
    Json(d): Json<DemandeProfilEstime>,
) -> Response {
    let (zone_sim, p) = match proposition_estimee(&etat, &id, &d.demande, d.expert) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let Some(zone_id) = d.zone_id.or(zone_sim) else {
        return refus(
            StatusCode::BAD_REQUEST,
            "zone_requise",
            "préciser la zone dont enregistrer le profil (zone_id)".into(),
        );
    };
    let nom = nom_du_profil(d.nom, || format!("Estimation {id}"));
    enregistrer_le_profil(&etat, &id, zone_id, &nom, p, true).await
}

// ── C4 : sessions à plusieurs positions ────────────────────────────────────

fn session_inconnue(id: &str) -> Response {
    refus(
        StatusCode::NOT_FOUND,
        "session_inconnue",
        format!("aucune session « {id} »"),
    )
}

async fn creer_session(
    State(etat): State<EtatRoutes>,
    Path(zone): Path<i64>,
    Json(d): Json<DemandeSession>,
) -> Response {
    match etat.controleur.creer_session(zone, d) {
        Ok(id) => match etat.controleur.vue_session(&id) {
            Some(v) => (StatusCode::CREATED, Json(v)).into_response(),
            None => session_inconnue(&id),
        },
        Err(e) => refus(StatusCode::BAD_REQUEST, "session_invalide", e),
    }
}

async fn sessions(State(etat): State<EtatRoutes>) -> Response {
    let c = &etat.controleur;
    let liste: Vec<Value> = c
        .sessions
        .ids()
        .iter()
        .filter_map(|id| c.vue_session(id))
        .map(|v| serde_json::to_value(v).unwrap_or(Value::Null))
        .collect();
    Json(json!({ "sessions": liste })).into_response()
}

async fn vue_session(State(etat): State<EtatRoutes>, Path(id): Path<String>) -> Response {
    match etat.controleur.vue_session(&id) {
        Some(v) => Json(v).into_response(),
        None => session_inconnue(&id),
    }
}

#[derive(Deserialize)]
struct DemandePosition {
    #[serde(flatten)]
    mesure: DemandeMesure,
    index: Option<usize>,
    etiquette: Option<String>,
}

async fn mesurer_position(
    State(etat): State<EtatRoutes>,
    Path(id): Path<String>,
    Json(d): Json<DemandePosition>,
) -> Response {
    let Some(s) = etat.controleur.sessions.session(&id) else {
        return session_inconnue(&id);
    };
    let Some(zone_id) = s.zone_id else {
        return refus(
            StatusCode::CONFLICT,
            "session_sans_zone",
            "session importée sans zone : on ne peut pas y mesurer".into(),
        );
    };
    let demande = match demande_de(zone_id, d.mesure) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let c = etat.controleur.clone();
    let (index, etiquette) = (d.index, d.etiquette);
    let session_id = id.clone();
    let r = tokio::task::spawn_blocking(move || {
        c.mesurer_position(&session_id, index, etiquette, demande)
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));
    match r {
        Ok((position, mesure)) => (
            StatusCode::ACCEPTED,
            Json(
                json!({ "session": id, "position": position, "id": mesure, "zone_id": s.zone_id }),
            ),
        )
            .into_response(),
        Err(e) if e.contains("déjà en cours") => {
            refus(StatusCode::CONFLICT, "mesure_en_cours", e)
        }
        Err(e) => refus(StatusCode::UNPROCESSABLE_ENTITY, "mesure_refusee", e),
    }
}

async fn exporter_session(State(etat): State<EtatRoutes>, Path(id): Path<String>) -> Response {
    match etat.controleur.session(&id) {
        Some(s) => Json(s).into_response(),
        None => session_inconnue(&id),
    }
}

async fn importer_session(State(etat): State<EtatRoutes>, Json(doc): Json<Value>) -> Response {
    match etat.controleur.importer_session(doc) {
        Ok((id, version)) => match etat.controleur.vue_session(&id) {
            Some(v) => {
                let mut v = serde_json::to_value(v).unwrap_or(Value::Null);
                if let Some(o) = v.as_object_mut() {
                    o.insert("version_lue".into(), json!(version));
                }
                (StatusCode::CREATED, Json(v)).into_response()
            }
            None => session_inconnue(&id),
        },
        Err(ErreurLecture::VersionNonPrise(v)) => refus(
            StatusCode::CONFLICT,
            "unsupported_version",
            format!(
                "session au format {v} : ce serveur lit jusqu'au format {}",
                crate::session::VERSION_SESSION
            ),
        ),
        Err(ErreurLecture::Invalide(e)) => refus(StatusCode::BAD_REQUEST, "session_invalide", e),
    }
}

#[allow(clippy::result_large_err)]
fn proposition_session(
    etat: &EtatRoutes,
    id: &str,
    demande: &DemandeCorrection,
) -> Result<(Option<i64>, Proposition), Response> {
    if etat.controleur.sessions.session(id).is_none() {
        return Err(session_inconnue(id));
    }
    etat.controleur
        .proposer_session(id, demande)
        .map_err(|e| refus(StatusCode::UNPROCESSABLE_ENTITY, "correction_refusee", e))
}

async fn session_correction(
    State(etat): State<EtatRoutes>,
    Path(id): Path<String>,
    Json(d): Json<DemandeCorrection>,
) -> Response {
    match proposition_session(&etat, &id, &d) {
        Ok((zone_id, p)) => {
            let mut v = serde_json::to_value(&p).unwrap_or(Value::Null);
            if let Some(o) = v.as_object_mut() {
                o.insert("id".into(), json!(id));
                o.insert("zone_id".into(), json!(zone_id));
            }
            Json(v).into_response()
        }
        Err(r) => r,
    }
}

async fn session_correction_profil(
    State(etat): State<EtatRoutes>,
    Path(id): Path<String>,
    Json(d): Json<DemandeProfil>,
) -> Response {
    let (zone_id, p) = match proposition_session(&etat, &id, &d.demande) {
        Ok(x) => x,
        Err(r) => return r,
    };
    // Un document v1 importé sans `zone_id` ne vise aucune zone : pas de
    // zone 0 par défaut.
    let Some(zone_id) = zone_id else {
        return refus(
            StatusCode::BAD_REQUEST,
            "zone_requise",
            "session sans zone (mesure importée sans zone_id) : aucun profil à enregistrer".into(),
        );
    };
    let nom = nom_du_profil(d.nom, || format!("Calibration {id}"));
    enregistrer_le_profil(&etat, &id, zone_id, &nom, p, false).await
}

/// La réponse (mesure ou moyenne de session) et le FIR conçu, hors du fil
/// asynchrone : 32 768 à 131 072 coefficients, quelques FFT longues.
async fn concevoir_fir(
    etat: &EtatRoutes,
    id: &str,
    d: DemandeFir,
) -> Result<(tune_calibration::fir::FirMesure, RapportFir), Response> {
    if etat.controleur.sessions.session(id).is_none() && etat.controleur.mesure(id).is_none() {
        return Err(refus(
            StatusCode::NOT_FOUND,
            "inconnue",
            format!("aucune mesure ni session « {id} »"),
        ));
    }
    let c = etat.controleur.clone();
    let id = id.to_string();
    tokio::task::spawn_blocking(move || {
        let (reponse, positions) = c.reponse_de(&id)?;
        crate::correction_fir::concevoir(&reponse, positions, &d)
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()))
    .map_err(|e| refus(StatusCode::UNPROCESSABLE_ENTITY, "fir_refuse", e))
}

async fn fir_rapport(
    State(etat): State<EtatRoutes>,
    Path(id): Path<String>,
    Json(d): Json<DemandeFir>,
) -> Response {
    match concevoir_fir(&etat, &id, d).await {
        Ok((_, rapport)) => {
            let mut v = serde_json::to_value(&rapport).unwrap_or(Value::Null);
            if let Some(o) = v.as_object_mut() {
                o.insert("id".into(), json!(id));
            }
            Json(v).into_response()
        }
        Err(r) => r,
    }
}

async fn fir_wav(
    State(etat): State<EtatRoutes>,
    Path(id): Path<String>,
    Json(d): Json<DemandeFir>,
) -> Response {
    let format = d.format;
    let (f, rapport) = match concevoir_fir(&etat, &id, d).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    match crate::correction_fir::wav(&f, format) {
        Ok(octets) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "audio/wav".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!(
                        "attachment; filename=\"correction-{id}-{}hz.wav\"",
                        rapport.frequence
                    ),
                ),
                (
                    header::HeaderName::from_static("x-tune-fir-latence"),
                    rapport.latence_echantillons.to_string(),
                ),
            ],
            octets,
        )
            .into_response(),
        Err(e) => refus(StatusCode::UNPROCESSABLE_ENTITY, "fir_refuse", e),
    }
}

async fn comparer(State(etat): State<EtatRoutes>, Json(d): Json<DemandeComparaison>) -> Response {
    for id in [&d.avant, &d.apres] {
        if etat.controleur.sessions.session(id).is_none() && etat.controleur.mesure(id).is_none() {
            return refus(
                StatusCode::NOT_FOUND,
                "inconnue",
                format!("aucune mesure ni session « {id} »"),
            );
        }
    }
    match etat.controleur.comparer(&d) {
        Ok(ab) => Json(ab).into_response(),
        Err(e) => refus(StatusCode::UNPROCESSABLE_ENTITY, "comparaison_refusee", e),
    }
}
