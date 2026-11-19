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
//!   coûte un décodage complet de la piste (le fichier est lu en entier, une
//!   fois ; sur un partage réseau il est d'abord copié en local par le
//!   décodeur), payé à la première demande seulement ;
//! * le décodage et le calcul tournent HORS de l'exécuteur async
//!   (`spawn_blocking`), [`CALCULS_MAX`] au plus en même temps ; la place
//!   part avec le travail bloquant (une requête abandonnée la garde jusqu'à
//!   la fin du décodage, rien ne s'empile au-delà) ;
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
use tune_core::audio::analyzer::{ONDE_POINTS_ANCIEN, borner_points_onde, onde_du_fichier};
use tune_core::db::models::Track;
use tune_core::db::track_repo::TrackRepo;

use super::spectrogramme as spectro;
use crate::state::AppState;

/// Plafond du cache disque par défaut : 64 Mo (quelque 9 000 ondes de 1 000
/// points).
pub(crate) const PLAFOND_DEFAUT_OCTETS: u64 = 64 * 1024 * 1024;

/// Décodages simultanés au plus pour la barre de lecture.
pub(crate) const CALCULS_MAX: usize = 2;

static CALCULS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(CALCULS_MAX)));

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Parametres {
    pub(crate) points: Option<usize>,
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
    let empreinte = spectro::empreinte_du_fichier(&source.chemin, meta);
    match source.tranche {
        Some((debut, fin)) => format!(
            "{id}-{empreinte:016x}-cue{debut}-{}-{points}.json",
            fin.map_or_else(|| "fin".to_string(), |f| f.to_string())
        ),
        None => format!("{id}-{empreinte:016x}-{points}.json"),
    }
}

fn reponse(id: i64, onde: Value) -> Response {
    let points = onde.as_array().map_or(0, Vec::len);
    Json(json!({ "track_id": id, "waveform": onde, "points": points })).into_response()
}

fn sans_onde(id: i64, erreur: &str) -> Response {
    Json(json!({ "track_id": id, "waveform": null, "error": erreur })).into_response()
}

/// Décode et calcule hors de l'exécuteur, une place du sémaphore tenue.
async fn calculer(source: Source, points: usize) -> Vec<f32> {
    let Ok(place) = Arc::clone(&CALCULS).acquire_owned().await else {
        return Vec::new();
    };
    let debut = std::time::Instant::now();
    let chemin = source.chemin.clone();
    let onde = tokio::task::spawn_blocking(move || {
        let _place = place;
        onde_du_fichier(&source.chemin, points, source.debut_s, source.duree_s)
    })
    .await
    .unwrap_or_default();
    tracing::info!(
        file = %chemin,
        points,
        duree_ms = debut.elapsed().as_millis() as u64,
        ok = !onde.is_empty(),
        "onde_lecture_calculee"
    );
    onde
}

pub(crate) async fn servir(
    state: &AppState,
    id: i64,
    p: Parametres,
    dossier: &CheminFs,
    plafond: u64,
) -> Response {
    let points = p.points.map_or(ONDE_POINTS_ANCIEN, borner_points_onde);
    let en_base = points == ONDE_POINTS_ANCIEN;
    let repo = TrackRepo::with_backend(state.backend.clone());

    if en_base {
        if let Ok(Some(cached)) = repo.get_waveform(id) {
            return reponse(
                id,
                serde_json::from_str::<Value>(&cached).unwrap_or(Value::Null),
            );
        }
    }

    let track = match repo.get(id) {
        Ok(Some(t)) => t,
        Ok(None) => return (StatusCode::NOT_FOUND, "track not found").into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let Some(source) = source_de(&track) else {
        return sans_onde(id, "no file path");
    };

    let cible = if en_base {
        None
    } else {
        let Some(meta) = std::fs::metadata(&source.chemin)
            .ok()
            .filter(|m| m.is_file())
        else {
            return sans_onde(id, "file unreadable or unsupported format");
        };
        let cible = dossier.join(nom_en_cache(id, &source, &meta, points));
        if let Some(onde) = spectro::lire_le_cache(&cible)
            .and_then(|o| serde_json::from_slice::<Vec<f32>>(&o).ok())
            .filter(|o| !o.is_empty())
        {
            return reponse(id, json!(onde));
        }
        Some(cible)
    };

    let onde = calculer(source, points).await;
    if onde.is_empty() {
        return sans_onde(id, "file unreadable or unsupported format");
    }
    let octets = serde_json::to_vec(&onde).unwrap_or_default();
    match cible {
        None => {
            repo.set_waveform(id, &String::from_utf8_lossy(&octets))
                .ok();
        }
        Some(cible) => {
            if let Err(e) = ecrire(&cible, &octets) {
                tracing::warn!(track_id = id, error = %e, "onde_lecture_cache_non_ecrit");
            }
            spectro::evincer_selon(dossier, plafond, &cible, "json");
        }
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
