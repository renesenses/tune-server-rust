//! `GET /library/tracks/{id}/forme-onde` — la forme d'onde d'une piste pour
//! sa fiche technique (fil du forum 2793, rc12) : par canal et par colonne de
//! temps, les crêtes (min / max) et la RMS ; par canal et en tout, la crête
//! maximale, la RMS globale, le facteur de crête (PLR) et le nombre
//! d'échantillons écrêtés. JSON, niveaux linéaires normalisés à ±1 et
//! chiffres en dBFS.
//!
//! Même règle que le spectrogramme (#7857, `spectrogramme.rs`) :
//!
//! * calculé à la demande sur le PCM décodé par Tune (`tune_core::audio::forme_onde`),
//!   jamais ffmpeg, aucun son ; pistes locales sans perte et avec perte ;
//!   une piste de service (sans fichier) rend 422, comme le spectrogramme ;
//! * le résultat de la piste entière est gardé dans
//!   `<cache des pochettes>/formes-onde/`, sous une clé qui porte la piste,
//!   l'empreinte du fichier (chemin, taille, date) et le nombre de colonnes :
//!   un fichier remplacé ne sert jamais une forme périmée. Plafond
//!   [`PLAFOND_DEFAUT_OCTETS`] (ou `TUNE_FORME_ONDE_CACHE_MO`), les moins
//!   récemment servies partent d'abord ;
//! * un seul décodage lourd par piste à la fois, deux au plus en tout
//!   (verrous et sémaphore partagés avec le spectrogramme).
//!
//! Paramètres : `colonnes` (50..4000, 1000 par défaut) ; `debut_s` et `fin_s`
//! pour une PLAGE (le zoom de l'écran) : calculée sans cache, bornée au
//! fichier. L'en-tête `X-Tune-Cache` dit `hit`, `miss` ou `plage`.

use std::path::{Path as CheminFs, PathBuf};

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::audio::forme_onde::{self, COLONNES_DEFAUT, FormeOnde, SEUIL_ECRETAGE_DBFS};
use tune_core::db::models::Track;
use tune_core::db::track_repo::TrackRepo;
use tune_core::taches_de_fond::priorite;

use super::spectrogramme as spectro;
use crate::state::AppState;

/// Plafond du cache par défaut : 64 Mo (quelque 600 formes stéréo de 1 000
/// colonnes).
pub(crate) const PLAFOND_DEFAUT_OCTETS: u64 = 64 * 1024 * 1024;

const TACHE: &str = "forme_onde";
/// Toujours demandée à la main (fiche technique) : jamais freinée (#8086).
const ORIGINE: priorite::Origine = priorite::Origine::Demandee;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Parametres {
    #[serde(alias = "columns")]
    pub(crate) colonnes: Option<usize>,
    #[serde(alias = "start_s")]
    pub(crate) debut_s: Option<f64>,
    #[serde(alias = "end_s")]
    pub(crate) fin_s: Option<f64>,
}

pub(crate) fn dossier_du_cache() -> PathBuf {
    super::artwork_cache_dir().join("formes-onde")
}

fn plafond() -> u64 {
    std::env::var("TUNE_FORME_ONDE_CACHE_MO")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|mo| mo * 1024 * 1024)
        .unwrap_or(PLAFOND_DEFAUT_OCTETS)
}

pub(crate) async fn json(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(p): Query<Parametres>,
) -> Response {
    servir(&state, id, p, &dossier_du_cache(), plafond()).await
}

fn erreur(statut: StatusCode, message: impl Into<String>) -> Response {
    (statut, message.into()).into_response()
}

/// Le fichier d'une piste tel que le spectrogramme le lit : `file_path`, sur
/// sa graphie du disque (#1865), avec son état d'aujourd'hui.
fn fichier(track: &Track) -> Option<(String, std::fs::Metadata)> {
    let chemin = track.file_path.as_deref().filter(|c| !c.is_empty())?;
    let chemin = tune_core::library::local_path::resolve_existing_local_path(chemin)
        .unwrap_or_else(|| chemin.to_string());
    let meta = std::fs::metadata(&chemin).ok().filter(|m| m.is_file())?;
    Some((chemin, meta))
}

pub(crate) fn nom_en_cache(
    id: i64,
    chemin: &str,
    meta: &std::fs::Metadata,
    colonnes: usize,
) -> String {
    format!(
        "{id}-{:016x}-{colonnes}.json",
        spectro::empreinte_du_fichier(chemin, meta)
    )
}

pub(crate) async fn servir(
    state: &AppState,
    id: i64,
    p: Parametres,
    dossier: &CheminFs,
    plafond: u64,
) -> Response {
    let colonnes = forme_onde::borner_colonnes(p.colonnes.unwrap_or(COLONNES_DEFAUT));
    let track = match TrackRepo::with_backend(state.backend.clone()).get(id) {
        Ok(Some(t)) => t,
        Ok(None) => return erreur(StatusCode::NOT_FOUND, "track not found"),
        Err(e) => return erreur(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    if track.file_path.as_deref().is_none_or(str::is_empty) {
        return erreur(StatusCode::UNPROCESSABLE_ENTITY, "track has no local file");
    }
    let Some((chemin, meta)) = fichier(&track) else {
        return erreur(StatusCode::UNPROCESSABLE_ENTITY, "track file not found");
    };
    let duree_piste_s = (track.duration_ms > 0).then(|| track.duration_ms as f64 / 1000.0);

    // Une plage (zoom) : calculée sans cache.
    if p.debut_s.is_some() || p.fin_s.is_some() {
        let debut = p.debut_s.unwrap_or(0.0);
        if !debut.is_finite() || debut < 0.0 {
            return erreur(StatusCode::BAD_REQUEST, "invalid debut_s");
        }
        let duree = match p.fin_s {
            Some(fin) if !fin.is_finite() || fin <= debut => {
                return erreur(StatusCode::BAD_REQUEST, "fin_s must be after debut_s");
            }
            Some(fin) => Some(fin - debut),
            None => None,
        };
        return match calculer(id, chemin, colonnes, debut, duree, duree_piste_s).await {
            Ok(v) => reponse(serde_json::to_vec(&v).unwrap_or_default(), "plage"),
            Err(r) => r,
        };
    }

    let nom = nom_en_cache(id, &chemin, &meta, colonnes);
    let cible = dossier.join(&nom);
    if let Some(octets) = spectro::lire_le_cache(&cible) {
        return reponse(octets, "hit");
    }
    let verrou = spectro::verrou_de(id);
    let garde = verrou.clone().lock_owned().await;
    // Pendant notre attente, un autre appel a pu la calculer.
    if let Some(octets) = spectro::lire_le_cache(&cible) {
        drop(garde);
        spectro::liberer(id, verrou);
        return reponse(octets, "hit");
    }
    let issue = match calculer(id, chemin, colonnes, 0.0, None, duree_piste_s).await {
        Ok(v) => {
            let octets = serde_json::to_vec(&v).unwrap_or_default();
            if let Err(e) = ecrire(&cible, &octets) {
                tracing::warn!(track_id = id, error = %e, "forme_onde_cache_non_ecrit");
            }
            spectro::evincer_selon(dossier, plafond, &cible, "json");
            reponse(octets, "miss")
        }
        Err(r) => r,
    };
    drop(garde);
    spectro::liberer(id, verrou);
    issue
}

async fn calculer(
    id: i64,
    chemin: String,
    colonnes: usize,
    debut_s: f64,
    duree_s: Option<f64>,
    duree_piste_s: Option<f64>,
) -> Result<Value, Response> {
    let _place = spectro::GENERATIONS
        .acquire()
        .await
        .map_err(|e| erreur(StatusCode::INTERNAL_SERVER_ERROR, format!("semaphore: {e}")))?;
    let debut = std::time::Instant::now();
    let resultat = priorite::hors_du_fil_async_pour(ORIGINE, TACHE, move || {
        forme_onde::calculer(&chemin, colonnes, debut_s, duree_s, duree_piste_s, &|| {
            priorite::ceder_pour_bloquant(ORIGINE, TACHE);
        })
    })
    .await
    .unwrap_or_else(|| Err("computation interrupted".to_string()));
    let duree_ms = debut.elapsed().as_millis() as u64;
    tracing::info!(
        track_id = id,
        duree_ms,
        ok = resultat.is_ok(),
        "forme_onde_calculee"
    );
    match resultat {
        Ok(f) => Ok(en_json(id, &f, debut_s, duree_ms)),
        Err(e) => Err(erreur(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("waveform failed: {e}"),
        )),
    }
}

fn arrondi(x: f64, decimales: i32) -> f64 {
    let m = 10f64.powi(decimales);
    (x * m).round() / m
}

fn colonne(v: &[f32]) -> Vec<f64> {
    v.iter().map(|&x| arrondi(f64::from(x), 4)).collect()
}

fn db(v: Option<f64>) -> Value {
    v.map(|x| json!(arrondi(x, 2))).unwrap_or(Value::Null)
}

/// Le contrat JSON (version 1). Les colonnes sont linéaires (−1..1 pour
/// `min` / `max`, 0..1 pour `rms`) ; les chiffres sont en dBFS (`null` pour
/// un silence).
pub(crate) fn en_json(id: i64, f: &FormeOnde, debut_s: f64, duree_calcul_ms: u64) -> Value {
    let voies: Vec<Value> = f
        .voies
        .iter()
        .enumerate()
        .map(|(canal, v)| {
            json!({
                "canal": canal,
                "crete_dbfs": db(v.crete_dbfs()),
                "rms_dbfs": db(v.rms_dbfs()),
                "facteur_crete_db": db(v.facteur_de_crete_db()),
                "ecretes": v.ecretes,
                "min": colonne(&v.min),
                "max": colonne(&v.max),
                "rms": colonne(&v.rms),
            })
        })
        .collect();
    json!({
        "version": 1,
        "track_id": id,
        "colonnes": f.colonnes,
        "canaux": f.voies.len(),
        "sample_rate": f.sample_rate,
        "debut_s": arrondi(debut_s, 3),
        "duree_s": arrondi(f.duree_s, 3),
        "seuil_ecretage_dbfs": SEUIL_ECRETAGE_DBFS,
        "global": {
            "crete_dbfs": db(f.crete_dbfs()),
            "rms_dbfs": db(f.rms_dbfs()),
            "facteur_crete_db": db(f.facteur_de_crete_db()),
            "ecretes": f.ecretes,
        },
        "voies": voies,
        "calcule_le": tune_core::db::spectrogrammes::maintenant(),
        "duree_calcul_ms": duree_calcul_ms,
    })
}

/// La section `forme_d_onde` de la fiche technique : `etat` (`a_jour` si la
/// forme de la piste entière, au nombre de colonnes par défaut, est en cache
/// pour le fichier d'aujourd'hui ; `absent` sinon) et l'URL à demander.
/// `None` pour une piste sans fichier : même règle que le spectrogramme.
pub(crate) fn section(track: &Track, dossier: &CheminFs) -> Option<Value> {
    let id = track.id?;
    let (chemin, meta) = fichier(track)?;
    let presente = dossier
        .join(nom_en_cache(id, &chemin, &meta, COLONNES_DEFAUT))
        .is_file();
    Some(json!({
        "etat": if presente { "a_jour" } else { "absent" },
        "url": format!("/api/v1/library/tracks/{id}/forme-onde?colonnes={COLONNES_DEFAUT}"),
        "colonnes": COLONNES_DEFAUT,
    }))
}

/// Écriture atomique : un lecteur ne voit jamais un JSON à moitié écrit.
fn ecrire(cible: &CheminFs, octets: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = cible.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let provisoire = cible.with_extension("json.part");
    std::fs::write(&provisoire, octets)?;
    std::fs::rename(&provisoire, cible)
}

fn reponse(octets: Vec<u8>, cache: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "private, max-age=3600"),
            (header::HeaderName::from_static("x-tune-cache"), cache),
        ],
        octets,
    )
        .into_response()
}

#[cfg(test)]
#[path = "forme_onde_tests_2793.rs"]
mod tests;
