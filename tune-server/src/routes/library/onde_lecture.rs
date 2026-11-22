//! `GET /library/tracks/{id}/waveform` — l'enveloppe RMS de la barre de
//! lecture d'une piste de bibliothèque (Discussion #8269), plus FINE à la
//! demande (fil du forum 2954, rc13, server#9173).
//!
//! * sans paramètre : 200 points, comme avant, depuis le cache en base
//!   (`tracks.waveform_json`) — un ancien client ne voit aucune différence ;
//! * `?points=N` : N borné à 100..2000 (le client web demande 1 000) ; 200
//!   passe par le cache en base, toute autre résolution par un cache disque
//!   `<cache des pochettes>/ondes-lecture/`, sous une clé qui porte la piste,
//!   l'empreinte du fichier (chemin, taille, date), la tranche CUE et la
//!   résolution — un fichier remplacé ne sert jamais une onde périmée.
//!   Plafond [`PLAFOND_DEFAUT_OCTETS`] (ou `TUNE_ONDE_LECTURE_CACHE_MO`),
//!   les moins récemment servies partent d'abord ;
//! * rien n'est calculé d'avance (ni au démarrage ni au scan) : une onde
//!   coûte une lecture complète de la piste, payée à la première demande
//!   seulement. Le décodage se fait PAR FENÊTRES de 10 s réduites en cases à
//!   mesure (`analyzer::onde_du_fichier_avec`, sur `forme_onde::calculer`) :
//!   la mémoire reste bornée quelle que soit la durée (relecture de
//!   server#9182 : décodée d'un bloc, une heure de CD pesait 1,3 Go) ;
//! * le calcul tourne HORS de l'exécuteur async (`spawn_blocking`), sous
//!   le sémaphore PARTAGÉ avec la fiche technique et le spectrogramme
//!   (`spectrogramme::GENERATIONS`, deux décodages lourds au plus en tout).
//!   Pas de file d'attente : sans place libre, ou quand la même onde est
//!   déjà en calcul, la route répond tout de suite `503 {"pending": true}`
//!   avec `Retry-After` — le client redemande et trouvera le cache. La place
//!   et la réservation partent avec le travail bloquant : une requête
//!   abandonnée les garde jusqu'à la fin du calcul ;
//! * les lectures et écritures du cache disque passent aussi hors de
//!   l'exécuteur ; un `.json.part` orphelin (arrêt brutal pendant une
//!   écriture) est effacé à l'éviction suivante ;
//! * une piste de feuille CUE (`file_path` nul, `cue_media_path` posé) : la
//!   TRANCHE de la piste dans son image, pas l'image entière (avant : « no
//!   file path »).
//!
//! Réponse : `{"track_id", "waveform": [...] | null, "points": N}` ;
//! `waveform: null` + `error` pour une piste sans fichier ou illisible
//! (inchangé).

use std::path::{Path as CheminFs, PathBuf};
use std::sync::{Arc, LazyLock};

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::audio::analyzer::{ONDE_POINTS_ANCIEN, borner_points_onde, onde_du_fichier_avec};
use tune_core::db::models::Track;
use tune_core::db::track_repo::TrackRepo;

use super::spectrogramme as spectro;
use crate::state::AppState;

/// Plafond du cache disque par défaut : 64 Mo (quelque 9 000 ondes de 1 000
/// points).
pub(crate) const PLAFOND_DEFAUT_OCTETS: u64 = 64 * 1024 * 1024;

/// Délai suggéré au client quand le calcul est en route ou sans place.
pub(crate) const REESSAYER_S: u64 = 3;

/// Âge au-delà duquel un `.json.part` est un orphelin.
const PROVISOIRE_ORPHELIN: std::time::Duration = std::time::Duration::from_secs(600);

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Parametres {
    /// Lu en texte : une valeur illisible rend un 400 qui le dit.
    pub(crate) points: Option<String>,
}

/// Les ondes en cours de calcul, par nom de cache : une seule tâche par clé.
static EN_COURS: LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    LazyLock::new(Default::default);

/// La réservation d'une clé ; rendue au drop (fin du travail bloquant).
pub(crate) struct Reservation(String);

impl Reservation {
    pub(crate) fn prendre(cle: &str) -> Option<Self> {
        let mut e = EN_COURS.lock().unwrap_or_else(|p| p.into_inner());
        e.insert(cle.to_string()).then(|| Self(cle.to_string()))
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        EN_COURS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.0);
    }
}

pub(crate) fn dossier_du_cache() -> PathBuf {
    super::artwork_cache_dir().join("ondes-lecture")
}

pub(crate) fn plafond() -> u64 {
    std::env::var("TUNE_ONDE_LECTURE_CACHE_MO")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|mo| mo * 1024 * 1024)
        .unwrap_or(PLAFOND_DEFAUT_OCTETS)
}

/// Ce qu'il faut décoder pour une piste : son fichier, ou la tranche d'une
/// image CUE (`debut_s`, `duree_s`, `0.0` = jusqu'au bout).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Source {
    pub(crate) chemin: String,
    pub(crate) debut_s: f64,
    pub(crate) duree_s: f64,
    /// Les bornes CUE en ms, pour la clé du cache.
    pub(crate) tranche: Option<(u64, Option<u64>)>,
}

/// La source d'une piste, sur la graphie du disque (#1865), ou `None` si
/// elle n'a pas de fichier (service, radio, UPnP).
pub(crate) fn source_de(track: &Track) -> Option<Source> {
    let resoudre = |c: &str| {
        tune_core::library::local_path::resolve_existing_local_path(c)
            .unwrap_or_else(|| c.to_string())
    };
    if let Some((media, debut_ms, fin_ms)) = track.bornes_cue() {
        let debut_s = debut_ms as f64 / 1000.0;
        let duree_s = fin_ms.map_or(0.0, |f| (f - debut_ms) as f64 / 1000.0);
        return Some(Source {
            chemin: resoudre(&media),
            debut_s,
            duree_s,
            tranche: Some((debut_ms, fin_ms)),
        });
    }
    let chemin = track.file_path.as_deref().filter(|c| !c.is_empty())?;
    Some(Source {
        chemin: resoudre(chemin),
        debut_s: 0.0,
        duree_s: 0.0,
        tranche: None,
    })
}

pub(crate) fn nom_en_cache(
    id: i64,
    source: &Source,
    meta: &std::fs::Metadata,
    points: usize,
) -> String {
    let empreinte = empreinte_stable(&source.chemin, meta);
    match source.tranche {
        Some((debut, fin)) => format!(
            "{id}-{empreinte:016x}-cue{debut}-{}-{points}.json",
            fin.map_or_else(|| "fin".to_string(), |f| f.to_string())
        ),
        None => format!("{id}-{empreinte:016x}-{points}.json"),
    }
}

/// L'empreinte d'un fichier (chemin, taille, date à la nanoseconde) par
/// FNV-1a 64 bits : stable d'une version de Rust à l'autre, contrairement à
/// `DefaultHasher` — une mise à jour du serveur ne rend pas le cache caduc.
pub(crate) fn empreinte_stable(chemin: &str, meta: &std::fs::Metadata) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut melanger = |octets: &[u8]| {
        for &o in octets {
            h ^= u64::from(o);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    melanger(chemin.as_bytes());
    melanger(&meta.len().to_le_bytes());
    let date = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    melanger(&date.to_le_bytes());
    h
}

fn reponse(id: i64, onde: Value) -> Response {
    let points = onde.as_array().map_or(0, Vec::len);
    Json(json!({ "track_id": id, "waveform": onde, "points": points })).into_response()
}

fn sans_onde(id: i64, erreur: &str) -> Response {
    Json(json!({ "track_id": id, "waveform": null, "error": erreur })).into_response()
}

/// « Pas encore » : calcul en route ou pas de place. Rien n'attend côté
/// serveur ; le client redemande (le web le fait déjà : un 503 est une
/// panne passagère, jamais retenue).
fn pas_encore(id: i64, raison: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(axum::http::header::RETRY_AFTER, REESSAYER_S.to_string())],
        Json(json!({ "track_id": id, "waveform": null, "pending": true, "raison": raison })),
    )
        .into_response()
}

/// Hors de l'exécuteur : relire une onde du cache disque.
async fn lire_cache(cible: PathBuf) -> Option<Vec<f32>> {
    tokio::task::spawn_blocking(move || {
        spectro::lire_le_cache(&cible)
            .and_then(|o| serde_json::from_slice::<Vec<f32>>(&o).ok())
            .filter(|o| !o.is_empty())
    })
    .await
    .ok()
    .flatten()
}

/// Efface les `.json.part` de plus de [`PROVISOIRE_ORPHELIN`].
pub(crate) fn purger_les_provisoires(dossier: &CheminFs) -> usize {
    let Ok(entrees) = std::fs::read_dir(dossier) else {
        return 0;
    };
    let maintenant = std::time::SystemTime::now();
    entrees
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".json.part"))
        .filter(|e| {
            e.metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| maintenant.duration_since(m).ok())
                .is_some_and(|age| age >= PROVISOIRE_ORPHELIN)
        })
        .filter(|e| std::fs::remove_file(e.path()).is_ok())
        .count()
}

pub(crate) async fn servir(
    state: &AppState,
    id: i64,
    p: Parametres,
    dossier: &CheminFs,
    plafond: u64,
) -> Response {
    servir_avec(state, id, p, dossier, plafond, &spectro::GENERATIONS).await
}

pub(crate) async fn servir_avec(
    state: &AppState,
    id: i64,
    p: Parametres,
    dossier: &CheminFs,
    plafond: u64,
    places: &Arc<tokio::sync::Semaphore>,
) -> Response {
    let points = match p.points.as_deref().map(str::trim) {
        None | Some("") => ONDE_POINTS_ANCIEN,
        Some(t) => match t.parse::<usize>() {
            Ok(n) => borner_points_onde(n),
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    "invalid points: expected an integer",
                )
                    .into_response();
            }
        },
    };
    let en_base = points == ONDE_POINTS_ANCIEN;
    let repo = TrackRepo::with_backend(state.backend.clone());

    if en_base && let Ok(Some(cached)) = repo.get_waveform(id) {
        return reponse(
            id,
            serde_json::from_str::<Value>(&cached).unwrap_or(Value::Null),
        );
    }

    let track = match repo.get(id) {
        Ok(Some(t)) => t,
        Ok(None) => return (StatusCode::NOT_FOUND, "track not found").into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let Some(source) = source_de(&track) else {
        return sans_onde(id, "no file path");
    };
    let chemin = source.chemin.clone();
    let Some(meta) = tokio::task::spawn_blocking(move || {
        std::fs::metadata(&chemin).ok().filter(|m| m.is_file())
    })
    .await
    .ok()
    .flatten() else {
        return sans_onde(id, "file unreadable or unsupported format");
    };
    let nom = nom_en_cache(id, &source, &meta, points);
    let cible = dossier.join(&nom);
    if !en_base && let Some(onde) = lire_cache(cible.clone()).await {
        return reponse(id, json!(onde));
    }

    // Une seule tâche par clé, et pas de file : sans place, « pas encore ».
    let Some(reservation) = Reservation::prendre(&nom) else {
        return pas_encore(id, "en_cours");
    };
    let Ok(place) = Arc::clone(places).try_acquire_owned() else {
        return pas_encore(id, "occupe");
    };
    let debut = std::time::Instant::now();
    let dossier_c = dossier.to_path_buf();
    let a_ecrire = (!en_base).then_some(cible);
    let onde = tokio::task::spawn_blocking(move || {
        let _place = place;
        let _reservation = reservation;
        let onde = onde_du_fichier_avec(
            &source.chemin,
            points,
            source.debut_s,
            source.duree_s,
            // Pas la durée en base : périmée (fichier remplacé), elle
            // tronquerait l'onde. Sans elle, les cases grossissent en route.
            None,
        );
        if let (Some(cible), false) = (&a_ecrire, onde.is_empty()) {
            let octets = serde_json::to_vec(&onde).unwrap_or_default();
            if let Err(e) = ecrire(cible, &octets) {
                tracing::warn!(track_id = id, error = %e, "onde_lecture_cache_non_ecrit");
            }
            purger_les_provisoires(&dossier_c);
            spectro::evincer_selon(&dossier_c, plafond, cible, "json");
        }
        onde
    })
    .await
    .unwrap_or_default();
    tracing::info!(
        track_id = id,
        points,
        duree_ms = debut.elapsed().as_millis() as u64,
        ok = !onde.is_empty(),
        "onde_lecture_calculee"
    );
    if onde.is_empty() {
        return sans_onde(id, "file unreadable or unsupported format");
    }
    if en_base {
        let json_str = serde_json::to_string(&onde).unwrap_or_default();
        repo.set_waveform(id, &json_str).ok();
    }
    reponse(id, json!(onde))
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

#[cfg(test)]
#[path = "onde_lecture_tests_2954.rs"]
mod tests;
