//! `GET /library/tracks/{id}/spectrogramme.png` — le spectrogramme d'une
//! piste, généré à la demande (#7857).
//!
//! * Le calcul est dans `tune_core::audio::spectrogramme` (PCM décodé par
//!   Tune, jamais ffmpeg).
//! * Les PNG sont gardés dans `<cache des pochettes>/spectrogrammes/`. La clé
//!   porte la piste, le chemin, la taille et la date du fichier, et la taille
//!   de l'image : un fichier remplacé ne sert jamais une image périmée.
//! * Le cache a un plafond ([`PLAFOND_DEFAUT_OCTETS`], ou
//!   `TUNE_SPECTROGRAMME_CACHE_MO`) : au-delà, les images servies le moins
//!   récemment partent d'abord.
//! * Une seule génération à la fois par piste : un second appel attend la
//!   première, puis lit le cache. Deux générations au plus en tout.
//! * Priorité basse : la génération tourne sur un fil marqué « de fond » et
//!   cède à la lecture entre deux fenêtres décodées, comme les passes de fond.
//!
//! L'en-tête `X-Tune-Cache` dit si l'image vient du cache (`hit`) ou vient
//! d'être générée (`miss`).
//!
//! #8160 — l'image calculée est CONSERVÉE pour la fiche technique de la piste :
//! chaque génération écrit sa trace datée dans `spectrogrammes` (nom du PNG,
//! taille, date et durée du calcul, signature du fichier audio,
//! `tune_core::db::spectrogrammes`). L'image précédente d'une piste part du
//! cache quand une nouvelle la remplace ; une image évincée emporte sa ligne.
//! `POST /library/tracks/{id}/spectrogramme` recalcule sans lire le cache et
//! rend la section `spectrogramme` de la fiche.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path as CheminFs, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::SystemTime;

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tune_core::audio::spectrogramme::{self, HAUTEUR_DEFAUT, LARGEUR_DEFAUT};
use tune_core::db::spectrogrammes;
use tune_core::db::track_repo::TrackRepo;
use tune_core::taches_de_fond::priorite;

use crate::state::AppState;

/// Plafond du cache par défaut : 256 Mo (environ 2 000 images 1200 × 400).
pub(crate) const PLAFOND_DEFAUT_OCTETS: u64 = 256 * 1024 * 1024;

const TACHE: &str = "spectrogramme";
/// Toujours demandé à la main (menu d'une piste) : jamais freiné (#8086).
const ORIGINE: priorite::Origine = priorite::Origine::Demandee;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Taille {
    #[serde(alias = "width")]
    largeur: Option<u32>,
    #[serde(alias = "height")]
    hauteur: Option<u32>,
}

/// Le dossier du cache, sous celui des pochettes.
pub(crate) fn dossier_du_cache() -> PathBuf {
    super::artwork_cache_dir().join("spectrogrammes")
}

fn plafond() -> u64 {
    std::env::var("TUNE_SPECTROGRAMME_CACHE_MO")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|mo| mo * 1024 * 1024)
        .unwrap_or(PLAFOND_DEFAUT_OCTETS)
}

pub(crate) async fn png(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(taille): Query<Taille>,
) -> Response {
    servir(&state, id, taille, &dossier_du_cache(), plafond()).await
}

/// Une génération à la fois par piste (spectrogramme et forme d'onde : un
/// seul décodage lourd par piste à la fois).
static VERROUS: LazyLock<Mutex<HashMap<i64, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Deux générations au plus en même temps, toutes pistes confondues (la
/// forme d'onde, fil 2793, partage cette limite).
pub(super) static GENERATIONS: LazyLock<tokio::sync::Semaphore> =
    LazyLock::new(|| tokio::sync::Semaphore::new(2));

pub(super) fn verrou_de(id: i64) -> Arc<tokio::sync::Mutex<()>> {
    let mut verrous = VERROUS.lock().unwrap_or_else(|e| e.into_inner());
    verrous.entry(id).or_default().clone()
}

pub(super) fn liberer(id: i64, verrou: Arc<tokio::sync::Mutex<()>>) {
    let mut verrous = VERROUS.lock().unwrap_or_else(|e| e.into_inner());
    // La table et nous : personne d'autre n'attend, l'entrée peut partir.
    if Arc::strong_count(&verrou) <= 2 {
        verrous.remove(&id);
    }
}

pub(crate) async fn servir(
    state: &AppState,
    id: i64,
    taille: Taille,
    dossier: &CheminFs,
    plafond: u64,
) -> Response {
    match obtenir(state, id, taille, dossier, plafond, false).await {
        Ok((octets, cache)) => reponse(octets, cache),
        Err(r) => r,
    }
}

/// `POST /library/tracks/{id}/spectrogramme` (#8160) : recalcule l'image sans
/// lire le cache, puis rend la section `spectrogramme` de la fiche technique.
pub(crate) async fn recalculer(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(taille): Query<Taille>,
) -> Response {
    recalculer_dans(&state, id, taille, &dossier_du_cache(), plafond()).await
}

pub(crate) async fn recalculer_dans(
    state: &AppState,
    id: i64,
    taille: Taille,
    dossier: &CheminFs,
    plafond: u64,
) -> Response {
    if let Err(r) = obtenir(state, id, taille, dossier, plafond, true).await {
        return r;
    }
    let section = TrackRepo::with_backend(state.backend.clone())
        .get(id)
        .ok()
        .flatten()
        .and_then(|t| super::fiche_technique::fichier_de_la_piste(&t))
        .and_then(|(_, meta)| section(state.backend.as_ref(), id, &meta, dossier));
    axum::Json(section.unwrap_or(serde_json::Value::Null)).into_response()
}

/// L'image de la piste : depuis le cache, ou générée (`forcer` : toujours
/// générée). Rend les octets et `hit` / `miss`, ou la réponse d'erreur.
async fn obtenir(
    state: &AppState,
    id: i64,
    taille: Taille,
    dossier: &CheminFs,
    plafond: u64,
    forcer: bool,
) -> Result<(Vec<u8>, &'static str), Response> {
    let (largeur, hauteur) = spectrogramme::borner(
        taille.largeur.unwrap_or(LARGEUR_DEFAUT),
        taille.hauteur.unwrap_or(HAUTEUR_DEFAUT),
    );
    let track = match TrackRepo::with_backend(state.backend.clone()).get(id) {
        Ok(Some(t)) => t,
        Ok(None) => return Err((StatusCode::NOT_FOUND, "track not found").into_response()),
        Err(e) => {
            return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response());
        }
    };
    let Some(chemin) = track.file_path.as_deref() else {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, "track has no local file").into_response());
    };
    // La graphie du disque, pas celle de la base (#1865).
    let chemin = tune_core::library::local_path::resolve_existing_local_path(chemin)
        .unwrap_or_else(|| chemin.to_string());
    let Ok(meta) = std::fs::metadata(&chemin) else {
        return Err((StatusCode::UNPROCESSABLE_ENTITY, "track file not found").into_response());
    };
    let nom = nom_en_cache(id, &chemin, &meta, largeur, hauteur);
    let cible = dossier.join(&nom);

    if !forcer && let Some(octets) = lire_le_cache(&cible) {
        return Ok((octets, "hit"));
    }

    let verrou = verrou_de(id);
    let garde = verrou.clone().lock_owned().await;
    // Pendant notre attente, un autre appel a pu la générer.
    if !forcer && let Some(octets) = lire_le_cache(&cible) {
        drop(garde);
        liberer(id, verrou);
        return Ok((octets, "hit"));
    }
    let debut = std::time::Instant::now();
    let resultat = generer(chemin, largeur, hauteur, track.duration_ms).await;
    let duree_ms = debut.elapsed().as_millis() as i64;
    let issue = match resultat {
        Ok(octets) => {
            if let Err(e) = ecrire(&cible, &octets) {
                tracing::warn!(track_id = id, error = %e, "spectrogramme_cache_non_ecrit");
            } else {
                conserver(
                    state, id, &nom, &octets, &meta, largeur, hauteur, duree_ms, dossier,
                );
            }
            let evincees = evincer(dossier, plafond, &cible);
            if !evincees.is_empty()
                && let Err(e) =
                    spectrogrammes::oublier_les_fichiers(state.backend.as_ref(), &evincees)
            {
                tracing::warn!(error = %e, "spectrogramme_oubli_des_evincees");
            }
            Ok((octets, "miss"))
        }
        Err(e) => {
            tracing::info!(track_id = id, error = %e, "spectrogramme_impossible");
            Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("spectrogram failed: {e}"),
            )
                .into_response())
        }
    };
    drop(garde);
    liberer(id, verrou);
    issue
}

/// #8160 — la trace datée de l'image qui vient d'être écrite. L'image
/// précédente de la piste, si elle porte un autre nom (fichier changé, autre
/// taille), quitte le cache : elle ne sert plus. Un échec n'empêche pas de
/// servir l'image : il se journalise.
#[allow(clippy::too_many_arguments)]
fn conserver(
    state: &AppState,
    id: i64,
    nom: &str,
    octets: &[u8],
    meta: &std::fs::Metadata,
    largeur: u32,
    hauteur: u32,
    duree_ms: i64,
    dossier: &CheminFs,
) {
    let trace = spectrogrammes::Spectrogramme {
        track_id: id,
        fichier: nom.to_string(),
        largeur: i64::from(largeur),
        hauteur: i64::from(hauteur),
        octets: octets.len() as i64,
        signature: spectrogrammes::signature(meta),
        taille_fichier: Some(meta.len() as i64),
        mtime_fichier: spectrogrammes::mtime_secondes(meta),
        duree_calcul_ms: Some(duree_ms),
        calcule_le: spectrogrammes::maintenant(),
    };
    match spectrogrammes::enregistrer(state.backend.as_ref(), &trace) {
        Ok(Some(ancienne)) => {
            let _ = std::fs::remove_file(dossier.join(ancienne));
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(track_id = id, error = %e, "spectrogramme_trace_non_ecrite"),
    }
}

/// #8160 — la section `spectrogramme` de la fiche technique d'une piste qui a
/// un fichier (`meta` : son état d'aujourd'hui).
///
/// * `etat` : `a_jour` (l'image vaut pour le fichier d'aujourd'hui), `perime`
///   (le fichier a changé depuis le calcul), `absent` (jamais calculée, ou
///   évincée du cache : sa ligne s'efface alors ici) ;
/// * `url` : l'image à demander, à la taille de la dernière image calculée
///   (servie depuis le cache quand elle est à jour) ;
/// * la date et la durée du calcul, la taille de l'image.
pub(crate) fn section(
    db: &dyn tune_core::db::backend::DbBackend,
    id: i64,
    meta: &std::fs::Metadata,
    dossier: &CheminFs,
) -> Option<serde_json::Value> {
    let trace = match spectrogrammes::lire(db, id) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(track_id = id, error = %e, "spectrogramme_trace_illisible");
            None
        }
    };
    let trace = trace.filter(|t| {
        let presente = dossier.join(&t.fichier).is_file();
        if !presente {
            // Évincée du cache, ou cache effacé : la ligne ne dit plus rien.
            let _ = spectrogrammes::oublier(db, id);
        }
        presente
    });
    let (largeur, hauteur) = trace
        .as_ref()
        .map(|t| (t.largeur, t.hauteur))
        .unwrap_or((i64::from(LARGEUR_DEFAUT), i64::from(HAUTEUR_DEFAUT)));
    let url = format!(
        "/api/v1/library/tracks/{id}/spectrogramme.png?largeur={largeur}&hauteur={hauteur}"
    );
    Some(match trace {
        None => serde_json::json!({
            "etat": "absent",
            "url": url,
            "largeur": largeur,
            "hauteur": hauteur,
        }),
        Some(t) => {
            let etat = if t.signature == spectrogrammes::signature(meta) {
                "a_jour"
            } else {
                "perime"
            };
            serde_json::json!({
                "etat": etat,
                "url": url,
                "largeur": t.largeur,
                "hauteur": t.hauteur,
                "octets": t.octets,
                "calcule_le": t.calcule_le,
                "duree_calcul_ms": t.duree_calcul_ms,
            })
        }
    })
}

async fn generer(
    chemin: String,
    largeur: u32,
    hauteur: u32,
    duree_ms: i64,
) -> Result<Vec<u8>, String> {
    let _place = GENERATIONS
        .acquire()
        .await
        .map_err(|e| format!("semaphore: {e}"))?;
    let duree_s = (duree_ms > 0).then(|| duree_ms as f64 / 1000.0);
    let debut = std::time::Instant::now();
    // Un geste de l'utilisateur, pas une analyse de fond : aucune cession à
    // la lecture entre deux fenêtres (#8086), une seule génération à la fois
    // par piste et deux au plus au total, un nice modéré.
    let resultat = priorite::hors_du_fil_async_pour(ORIGINE, TACHE, move || {
        spectrogramme::generer_png(&chemin, largeur, hauteur, duree_s, &|| {
            priorite::ceder_pour_bloquant(ORIGINE, TACHE);
        })
    })
    .await
    .unwrap_or_else(|| Err("generation interrupted".to_string()));
    tracing::info!(
        duree_ms = debut.elapsed().as_millis() as u64,
        ok = resultat.is_ok(),
        "spectrogramme_genere"
    );
    resultat
}

/// Le nom du fichier en cache : la piste, une empreinte du chemin, de la
/// taille et de la date du fichier, puis la taille de l'image.
pub(crate) fn nom_en_cache(
    id: i64,
    chemin: &str,
    meta: &std::fs::Metadata,
    largeur: u32,
    hauteur: u32,
) -> String {
    format!(
        "{id}-{:016x}-{largeur}x{hauteur}.png",
        empreinte_du_fichier(chemin, meta)
    )
}

/// Une empreinte du chemin, de la taille et de la date du fichier : elle
/// change dès que le fichier est remplacé (forme d'onde comprise, fil 2793).
pub(super) fn empreinte_du_fichier(chemin: &str, meta: &std::fs::Metadata) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    chemin.hash(&mut h);
    meta.len().hash(&mut h);
    meta.modified()
        .ok()
        .and_then(|m| m.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .hash(&mut h);
    h.finish()
}

/// Lit une image en cache et la marque servie (sa date), pour l'éviction.
pub(super) fn lire_le_cache(cible: &CheminFs) -> Option<Vec<u8>> {
    let octets = std::fs::read(cible).ok()?;
    if let Ok(f) = std::fs::File::options().write(true).open(cible) {
        let _ = f.set_modified(SystemTime::now());
    }
    Some(octets)
}

/// Écriture atomique : un lecteur ne voit jamais une image à moitié écrite.
fn ecrire(cible: &CheminFs, octets: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = cible.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let provisoire = cible.with_extension("png.part");
    std::fs::write(&provisoire, octets)?;
    std::fs::rename(&provisoire, cible)
}

/// Ramène le cache sous `plafond` octets, les images servies le moins
/// récemment d'abord. `garder` (l'image qui vient d'être écrite) ne part pas.
/// Rend les noms des images retirées (#8160 : leurs lignes s'effacent).
pub(crate) fn evincer(dossier: &CheminFs, plafond: u64, garder: &CheminFs) -> Vec<String> {
    evincer_selon(dossier, plafond, garder, "png")
}

/// [`evincer`] pour les fichiers d'extension `extension` (la forme d'onde,
/// fil 2793, garde du JSON).
pub(super) fn evincer_selon(
    dossier: &CheminFs,
    plafond: u64,
    garder: &CheminFs,
    extension: &str,
) -> Vec<String> {
    let mut retirees = Vec::new();
    let Ok(entrees) = std::fs::read_dir(dossier) else {
        return retirees;
    };
    let mut fichiers: Vec<(PathBuf, u64, SystemTime)> = entrees
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == extension))
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            m.is_file().then(|| {
                (
                    e.path(),
                    m.len(),
                    m.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                )
            })
        })
        .collect();
    let mut total: u64 = fichiers.iter().map(|(_, l, _)| l).sum();
    if total <= plafond {
        return retirees;
    }
    fichiers.sort_by_key(|(_, _, quand)| *quand);
    for (chemin, longueur, _) in fichiers {
        if total <= plafond {
            break;
        }
        if chemin == garder {
            continue;
        }
        if std::fs::remove_file(&chemin).is_ok() {
            total -= longueur;
            if let Some(nom) = chemin.file_name() {
                retirees.push(nom.to_string_lossy().into_owned());
            }
        }
    }
    retirees
}

fn reponse(octets: Vec<u8>, cache: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "private, max-age=3600"),
            (header::HeaderName::from_static("x-tune-cache"), cache),
        ],
        octets,
    )
        .into_response()
}

#[cfg(test)]
#[path = "spectrogramme_tests_7857.rs"]
mod tests;

#[cfg(test)]
#[path = "spectrogramme_conserve_tests_8160.rs"]
mod tests_8160;
