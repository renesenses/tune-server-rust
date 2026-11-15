//! `GET /library/tracks/{id}/forme-onde` — la forme d'onde d'une piste pour
//! sa fiche technique (fil du forum 2793, rc12) : par canal et par colonne de
//! temps, les crêtes (min / max) et la RMS ; par canal et en tout, la crête
//! maximale, la RMS globale, le facteur de crête (crête − RMS, non pondéré ;
//! pas le PLR de l'EBU) et le nombre d'échantillons écrêtés (suites d'au
//! moins `ECRETAGE_SUITE_MIN` échantillons à la pleine échelle). JSON, niveaux linéaires normalisés à ±1 et
//! chiffres en dBFS.
//!
//! Même règle que le spectrogramme (#7857, `spectrogramme.rs`) :
//!
//! * calculé à la demande sur le PCM décodé par Tune (`tune_core::audio::forme_onde`),
//!   jamais ffmpeg, aucun son ; pistes locales sans perte et avec perte ;
//! * une piste QOBUZ ou TIDAL (go du 13/11) : l'audio est téléchargé pour le
//!   calcul puis effacé, seul le résultat reste en cache
//!   (`audio_de_service`) ; sa clé porte le service, l'identifiant chez lui
//!   et la qualité au lieu de l'empreinte du fichier ; la piste entière au
//!   nombre de colonnes par défaut seulement (422 `zoom_non_disponible`
//!   sinon). Une piste d'un autre service rend 422, comme avant ;
//! * le résultat de la piste entière, au nombre de colonnes par défaut (le
//!   seul que l'écran demande), est gardé dans
//!   `<cache des pochettes>/formes-onde/`, sous une clé qui porte la piste,
//!   l'empreinte du fichier (chemin, taille, date) et le nombre de colonnes :
//!   un autre nombre de colonnes est calculé sans cache ; un fichier remplacé ne sert jamais une forme périmée. Plafond
//!   [`PLAFOND_DEFAUT_OCTETS`] (ou `TUNE_FORME_ONDE_CACHE_MO`), les moins
//!   récemment servies partent d'abord ;
//! * un seul décodage lourd par piste à la fois, deux au plus en tout
//!   (verrous et sémaphore partagés avec le spectrogramme). La place du
//!   sémaphore vit DANS le travail bloquant : une requête abandonnée la garde
//!   jusqu'à ce que le décodage s'arrête, à la fenêtre suivante.
//!
//! Paramètres : `colonnes` (50..4000, 1000 par défaut) ; `debut_s` et `fin_s`
//! pour une PLAGE (le zoom de l'écran) : calculée sans cache, bornée à la
//! durée de la piste (422 si `debut_s` est au-delà, `fin_s` ramenée à la
//! fin). L'en-tête `X-Tune-Cache` dit `hit`, `miss`, `plage` ou `hors-cache`.

use std::path::{Path as CheminFs, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::audio::forme_onde::{self, COLONNES_DEFAUT, ECRETAGE_SUITE_MIN, FormeOnde};
use tune_core::db::models::Track;
use tune_core::db::track_repo::TrackRepo;
use tune_core::taches_de_fond::priorite;

use super::audio_de_service::{self as service, AudioTemporaire, Limites, PisteDeService, Refus};
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

/// Le nom en cache d'une piste de service : la clé du service, de
/// l'identifiant et de la qualité remplace l'empreinte du fichier.
pub(crate) fn nom_en_cache_service(id: i64, piste: &PisteDeService, colonnes: usize) -> String {
    format!("{id}-{}-{colonnes}.json", piste.cle())
}

pub(crate) async fn servir(
    state: &AppState,
    id: i64,
    p: Parametres,
    dossier: &CheminFs,
    plafond: u64,
) -> Response {
    servir_avec(
        state,
        id,
        p,
        dossier,
        plafond,
        Limites::de_l_environnement(),
    )
    .await
}

pub(crate) async fn servir_avec(
    state: &AppState,
    id: i64,
    p: Parametres,
    dossier: &CheminFs,
    plafond: u64,
    limites: Limites,
) -> Response {
    let colonnes = forme_onde::borner_colonnes(p.colonnes.unwrap_or(COLONNES_DEFAUT));
    let track = match TrackRepo::with_backend(state.backend.clone()).get(id) {
        Ok(Some(t)) => t,
        Ok(None) => return erreur(StatusCode::NOT_FOUND, "track not found"),
        Err(e) => return erreur(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    if track.file_path.as_deref().is_none_or(str::is_empty) {
        // Recette du 14/11 — une piste d'un serveur multimédia (UPnP) n'a
        // pas de fichier ici : un refus que l'écran sait dire.
        if track.source == "upnp" {
            return Refus::SourceReseau.reponse();
        }
        return match PisteDeService::de(&track) {
            Some(piste) => {
                servir_service(
                    state, &track, id, &piste, p, colonnes, dossier, plafond, limites,
                )
                .await
            }
            None => Refus::NonPrisEnCharge.reponse(),
        };
    }
    let Some((chemin, meta)) = fichier(&track) else {
        return erreur(StatusCode::UNPROCESSABLE_ENTITY, "track file not found");
    };
    let duree_piste_s = (track.duration_ms > 0).then(|| track.duration_ms as f64 / 1000.0);

    // Une plage (zoom) : calculée sans cache.
    if p.debut_s.is_some() || p.fin_s.is_some() {
        let (debut, duree) = match borner_la_plage(p.debut_s, p.fin_s, duree_piste_s) {
            Ok(plage) => plage,
            Err(r) => return r,
        };
        return match calculer(id, chemin, None, colonnes, debut, duree, duree_piste_s).await {
            Ok(v) => reponse(serde_json::to_vec(&v).unwrap_or_default(), "plage"),
            Err(r) => r,
        };
    }

    // Seul le nombre de colonnes par défaut entre au cache : un autre
    // nombre ne fait pas pousser le cache d'une variante par valeur.
    if colonnes != COLONNES_DEFAUT {
        return match calculer(id, chemin, None, colonnes, 0.0, None, duree_piste_s).await {
            Ok(v) => reponse(serde_json::to_vec(&v).unwrap_or_default(), "hors-cache"),
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
    let issue = match calculer(id, chemin, None, colonnes, 0.0, None, duree_piste_s).await {
        Ok(v) => mettre_en_cache(id, &v, &cible, dossier, plafond),
        Err(r) => r,
    };
    drop(garde);
    spectro::liberer(id, verrou);
    issue
}

fn mettre_en_cache(
    id: i64,
    v: &Value,
    cible: &CheminFs,
    dossier: &CheminFs,
    plafond: u64,
) -> Response {
    let octets = serde_json::to_vec(v).unwrap_or_default();
    if let Err(e) = ecrire(cible, &octets) {
        tracing::warn!(track_id = id, error = %e, "forme_onde_cache_non_ecrit");
    }
    spectro::evincer_selon(dossier, plafond, cible, "json");
    reponse(octets, "miss")
}

/// Une piste Qobuz ou TIDAL : la piste entière, au nombre de colonnes par
/// défaut, depuis le cache ou téléchargée, calculée, puis effacée.
#[allow(clippy::too_many_arguments)]
async fn servir_service(
    state: &AppState,
    track: &Track,
    id: i64,
    piste: &PisteDeService,
    p: Parametres,
    colonnes: usize,
    dossier: &CheminFs,
    plafond: u64,
    limites: Limites,
) -> Response {
    if p.debut_s.is_some() || p.fin_s.is_some() || colonnes != COLONNES_DEFAUT {
        return Refus::ZoomNonDisponible.reponse();
    }
    let cible = dossier.join(nom_en_cache_service(id, piste, colonnes));
    if let Some(octets) = spectro::lire_le_cache(&cible) {
        return reponse(octets, "hit");
    }
    // Un seul calcul par piste à la fois, téléchargement compris.
    let verrou = spectro::verrou_de(id);
    let garde = verrou.clone().lock_owned().await;
    if let Some(octets) = spectro::lire_le_cache(&cible) {
        drop(garde);
        spectro::liberer(id, verrou);
        return reponse(octets, "hit");
    }
    let duree_piste_s = (track.duration_ms > 0).then(|| track.duration_ms as f64 / 1000.0);
    let issue =
        match service::telecharger(state, piste, &service::dossier_temporaire(dossier), limites)
            .await
        {
            Ok(audio) => {
                let chemin = audio.chemin().to_string_lossy().into_owned();
                match calculer(id, chemin, Some(audio), colonnes, 0.0, None, duree_piste_s).await {
                    Ok(v) => mettre_en_cache(id, &v, &cible, dossier, plafond),
                    Err(r) => r,
                }
            }
            Err(refus) => refus.reponse(),
        };
    drop(garde);
    spectro::liberer(id, verrou);
    issue
}

/// La plage demandée, bornée à la piste : `(debut, duree)`. 400 pour des
/// bornes absurdes ; 422 quand `debut_s` tombe à la fin de la piste ou
/// au-delà ; `fin_s` au-delà de la fin est ramenée à la fin (sinon la durée
/// estimée gonfle les cases et la réponse perd des colonnes). Sans durée
/// connue, le décodage s'arrête de lui-même à la fin du fichier.
pub(crate) fn borner_la_plage(
    debut_s: Option<f64>,
    fin_s: Option<f64>,
    duree_piste_s: Option<f64>,
) -> Result<(f64, Option<f64>), Response> {
    let debut = debut_s.unwrap_or(0.0);
    if !debut.is_finite() || debut < 0.0 {
        return Err(erreur(StatusCode::BAD_REQUEST, "invalid debut_s"));
    }
    if let Some(fin) = fin_s {
        if !fin.is_finite() || fin <= debut {
            return Err(erreur(
                StatusCode::BAD_REQUEST,
                "fin_s must be after debut_s",
            ));
        }
    }
    let fin = match duree_piste_s {
        Some(piste) => {
            if debut >= piste {
                return Err(erreur(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "debut_s is past the end of the track",
                ));
            }
            Some(fin_s.map_or(piste, |f| f.min(piste)))
        }
        None => fin_s,
    };
    Ok((debut, fin.map(|f| f - debut)))
}

/// Levé quand le futur de la requête est lâché (le client a coupé, le zoom
/// suivant a annulé celui-ci) : le décodage le voit entre deux fenêtres et
/// s'arrête.
struct ArretAuDrop(Arc<AtomicBool>);

impl Drop for ArretAuDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// `audio` : le fichier téléchargé d'une piste de service, qui part (et
/// s'efface) avec le travail bloquant — jamais pendant qu'il se décode.
#[allow(clippy::too_many_arguments)]
async fn calculer(
    id: i64,
    chemin: String,
    audio: Option<AudioTemporaire>,
    colonnes: usize,
    debut_s: f64,
    duree_s: Option<f64>,
    duree_piste_s: Option<f64>,
) -> Result<Value, Response> {
    let arret = Arc::new(AtomicBool::new(false));
    let _arret_au_drop = ArretAuDrop(arret.clone());
    // `acquire_owned` : la place part avec le travail bloquant et n'est
    // rendue qu'à la fin du décodage, même si la requête est abandonnée.
    let place = Arc::clone(&spectro::GENERATIONS)
        .acquire_owned()
        .await
        .map_err(|e| erreur(StatusCode::INTERNAL_SERVER_ERROR, format!("semaphore: {e}")))?;
    let debut = std::time::Instant::now();
    let resultat = priorite::hors_du_fil_async_pour(ORIGINE, TACHE, move || {
        let _place = place;
        let _audio = audio;
        if arret.load(Ordering::Relaxed) {
            return Err(forme_onde::INTERROMPU.to_string());
        }
        forme_onde::calculer(&chemin, colonnes, debut_s, duree_s, duree_piste_s, &|| {
            priorite::ceder_pour_bloquant(ORIGINE, TACHE);
            !arret.load(Ordering::Relaxed)
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
                "colonnes_ecretees": v.colonnes_ecretees,
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
        "ecretage_suite_min": ECRETAGE_SUITE_MIN,
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
/// Une piste Qobuz ou TIDAL l'a aussi (calculée par téléchargement) : elle
/// porte alors `service`, et l'écran ne demande pas de plage. `None` pour une
/// piste sans fichier d'un autre service.
pub(crate) fn section(track: &Track, dossier: &CheminFs) -> Option<Value> {
    let id = track.id?;
    let url = format!("/api/v1/library/tracks/{id}/forme-onde?colonnes={COLONNES_DEFAUT}");
    if let Some((chemin, meta)) = fichier(track) {
        let presente = dossier
            .join(nom_en_cache(id, &chemin, &meta, COLONNES_DEFAUT))
            .is_file();
        return Some(json!({
            "etat": if presente { "a_jour" } else { "absent" },
            "url": url,
            "colonnes": COLONNES_DEFAUT,
        }));
    }
    if track.file_path.as_deref().is_some_and(|c| !c.is_empty()) {
        return None;
    }
    let piste = PisteDeService::de(track)?;
    let presente = dossier
        .join(nom_en_cache_service(id, &piste, COLONNES_DEFAUT))
        .is_file();
    Some(json!({
        "etat": if presente { "a_jour" } else { "absent" },
        "url": url,
        "colonnes": COLONNES_DEFAUT,
        "service": piste.service,
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
