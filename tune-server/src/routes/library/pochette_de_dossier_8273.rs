//! Discussion #8273, point 1 — la pochette de chaque dossier de la vue
//! « Répertoires ».
//!
//! `GET /library/browse/cover?path=…&size=80` rend une VIGNETTE, jamais
//! l'image d'origine en grand : le client la demande en chargement paresseux,
//! une par ligne visible, et une racine de 11 891 dossiers d'artistes (#3857)
//! ne doit pas faire transiter 11 891 fichiers `folder.jpg` de 3 Mo.
//!
//! L'ordre de recherche :
//! 1. une image de pochette posée DANS le dossier (`folder.jpg`, `cover.jpg`,
//!    `front.jpg`… — la règle du scanner, [`image_de_pochette_dans`]) ;
//! 2. sinon, la pochette d'un album dont les pistes sont rangées sous ce
//!    dossier (sous-dossiers compris) : un dossier d'artiste prend celle de
//!    son premier album, un coffret rangé par disque celle du coffret ;
//! 3. sinon `404` : le client garde son icône de dossier.
//!
//! Le disque n'est lu qu'à travers [`super::browse::chemin_dans_la_bibliotheque`],
//! la porte de la vue elle-même : un chemin que la liste refuserait d'ouvrir,
//! la vignette ne le lit pas davantage. Partage réseau compris : le travail
//! part hors de l'exécuteur (#5677) et le nombre de lectures simultanées est
//! borné, pour qu'un défilement rapide sur un NAS lent n'occupe pas tous les
//! fils bloquants.
//!
//! [`image_de_pochette_dans`]: tune_core::library::artwork::image_de_pochette_dans

use std::path::{Path, PathBuf};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use unicode_normalization::UnicodeNormalization;

use super::artwork::{is_hex_hash, serve_artwork_from};
use super::artwork_cache_dir;
use super::facets::hors_executeur;
use crate::state::AppState;
use tune_core::library::artwork as pochettes;

#[derive(Deserialize)]
pub(super) struct PochetteDossierQuery {
    path: String,
    /// `String` et non `u32`, comme `/artwork/{hash}` : une taille illisible
    /// retombe sur la case par défaut au lieu d'un 400.
    size: Option<String>,
}

/// La case rendue sans `?size=` : celle des grilles.
const CASE_PAR_DEFAUT: u32 = 200;

/// Au-delà, on ne lit même pas l'image : ce n'est pas une pochette, et la lire
/// entière depuis un partage réseau pour une vignette de 80 px coûterait plus
/// que tout le reste de l'écran.
const PLAFOND_IMAGE_LUE: u64 = 32 * 1024 * 1024;

/// Une image que l'on ne sait pas réduire (WebP, BMP : `image` n'est compilé
/// qu'avec JPEG et PNG) n'est servie telle quelle qu'en dessous de ce poids.
/// Au-dessus, le client garde l'icône : « pas de transfert d'image entière ».
const PLAFOND_ORIGINAL_SERVI: usize = 256 * 1024;

/// Lectures de pochettes de dossier menées en même temps, au plus.
const LECTURES_SIMULTANEES: usize = 4;
static LECTURES: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(LECTURES_SIMULTANEES));

/// La vignette se rafraîchit au plus toutes les cinq minutes côté navigateur ;
/// au-delà, l'ETag rend la revalidation gratuite (304).
const CACHE_CONTROL: &str = "private, max-age=300";

/// D'où vient la pochette d'un dossier.
#[derive(Debug, PartialEq)]
pub(super) enum SourcePochette {
    /// Une image posée dans le dossier.
    Image(PathBuf),
    /// Le condensat de la pochette d'un album rangé sous le dossier.
    Album(String),
    Aucune,
}

/// La case de vignette pour une taille demandée — toujours une case : sans
/// taille, la case par défaut ; au-delà de la plus grande, la plus grande.
pub(super) fn case_demandee(size: Option<&str>) -> u32 {
    let plus_grande = *pochettes::THUMB_SIZES.last().unwrap_or(&CASE_PAR_DEFAUT);
    size.and_then(|s| s.trim().parse::<u32>().ok())
        .map(|t| pochettes::thumb_bucket(t).unwrap_or(plus_grande))
        .unwrap_or(CASE_PAR_DEFAUT)
}

/// Cherche la pochette du dossier `brut` (bloquant : disque et base).
pub(super) fn source_de_la_pochette(
    state: &AppState,
    brut: &str,
) -> Result<SourcePochette, crate::error::AppError> {
    let (chemin, _racine) = super::browse::chemin_dans_la_bibliotheque(state, brut)?;
    if let Some(image) = pochettes::image_de_pochette_dans(Path::new(&chemin)) {
        return Ok(SourcePochette::Image(image));
    }
    Ok(pochette_d_un_album_du_dossier(state, &chemin)
        .map(SourcePochette::Album)
        .unwrap_or(SourcePochette::Aucune))
}

/// La pochette (en cache local) du premier album rangé sous `dossier`, dans
/// l'ordre des chemins. Même plage indexée et même `LIKE` que la liste des
/// pistes du dossier (#5998) : la requête ne relit pas toute la table. Une
/// pochette DISTANTE (`http…`) est écartée : la servir obligerait à la
/// télécharger en entier pour une vignette.
fn pochette_d_un_album_du_dossier(state: &AppState, dossier: &str) -> Option<String> {
    use tune_core::db::backend::ToSqlValue;
    use tune_core::db::engine::Engine;
    use tune_core::db::track_repo as pistes;

    let dossier_nfc: String = dossier.nfc().collect();
    let motif = pistes::folder_like_pattern(&dossier_nfc);
    let (bas, haut) = pistes::bornes_du_dossier(&dossier_nfc);
    let engine = state.backend.engine();
    let (p1, p2, p3) = match engine {
        Engine::Postgres => ("$1", "$2", "$3"),
        Engine::Sqlite => ("?1", "?2", "?3"),
    };
    let chemin = "COALESCE(t.file_path, t.cue_media_path)";
    let plage = pistes::plage_du_dossier(engine, chemin, p2, p3);
    let sql = format!(
        "SELECT al.cover_path FROM tracks t JOIN albums al ON al.id = t.album_id \
         WHERE {plage} AND {chemin} LIKE {p1}{esc} \
         AND al.cover_path IS NOT NULL AND al.cover_path <> '' \
         AND al.cover_path NOT LIKE 'http%' \
         ORDER BY {chemin} LIMIT 1",
        esc = pistes::like_escape_clause()
    );
    let ligne = state
        .backend
        .query_one(
            &sql,
            &[
                &motif as &dyn ToSqlValue,
                &bas as &dyn ToSqlValue,
                &haut as &dyn ToSqlValue,
            ],
        )
        .map_err(|e| tracing::warn!(dossier, error = %e, "pochette_dossier_requete_echouee"))
        .ok()
        .flatten()?;
    let cover = ligne.first().and_then(|v| v.as_string())?;
    Some(if is_hex_hash(&cover) {
        cover
    } else {
        pochettes::artwork_hash(&cover)
    })
}

/// Clé de cache d'une image de dossier : son chemin, son poids et sa date de
/// modification. Remplacer `folder.jpg` change la clé, donc la vignette.
fn cle_d_image(chemin: &Path, meta: &std::fs::Metadata) -> String {
    use sha2::{Digest, Sha256};
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = Sha256::new();
    h.update(b"pochette-de-dossier\0");
    h.update(chemin.to_string_lossy().as_bytes());
    h.update(format!("\0{}\0{mtime}", meta.len()).as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

pub(super) enum Vignette {
    Corps {
        octets: Vec<u8>,
        mime: &'static str,
        etag: String,
    },
    NonModifiee(String),
    Aucune,
}

/// La vignette d'une image de dossier, depuis le cache ou fabriquée (bloquant).
pub(super) fn vignette_d_image(
    cache_dir: &Path,
    chemin: &Path,
    case: u32,
    si_aucune_de: Option<&str>,
) -> Vignette {
    let Ok(meta) = std::fs::metadata(chemin) else {
        return Vignette::Aucune;
    };
    let cle = cle_d_image(chemin, &meta);
    let etag = format!("{cle}-w{case}");
    if si_aucune_de.is_some_and(|e| e.trim_matches('"') == etag) {
        return Vignette::NonModifiee(etag);
    }
    let en_cache = pochettes::thumb_path(cache_dir, case, &cle);
    if let Ok(octets) = std::fs::read(&en_cache) {
        return Vignette::Corps {
            octets,
            mime: "image/jpeg",
            etag,
        };
    }
    if meta.len() > PLAFOND_IMAGE_LUE {
        return Vignette::Aucune;
    }
    let Ok(original) = pochettes::lire_l_image(chemin) else {
        return Vignette::Aucune;
    };
    if let Some(octets) = pochettes::make_thumbnail(&original, case) {
        pochettes::store_thumbnail(cache_dir, case, &cle, &octets);
        return Vignette::Corps {
            octets,
            mime: "image/jpeg",
            etag,
        };
    }
    // Déjà plus petite que la case, ou format non réductible : servie telle
    // quelle si elle est légère, sinon rien.
    match pochettes::sniff_image_ext(&original) {
        Some(ext) if original.len() <= PLAFOND_ORIGINAL_SERVI => Vignette::Corps {
            octets: original,
            mime: pochettes::cache_mime(ext),
            etag,
        },
        _ => Vignette::Aucune,
    }
}

fn reponse(statut: StatusCode, corps: Vec<u8>, mime: Option<&str>, etag: Option<&str>) -> Response {
    let mut h = HeaderMap::new();
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL),
    );
    if let Some(mime) = mime.and_then(|m| HeaderValue::from_str(m).ok()) {
        h.insert(header::CONTENT_TYPE, mime);
    }
    if let Some(etag) = etag.and_then(|e| HeaderValue::from_str(&format!("\"{e}\"")).ok()) {
        h.insert(header::ETAG, etag);
    }
    (statut, h, corps).into_response()
}

/// `GET /library/browse/cover?path=…&size=…`
pub(super) async fn pochette_de_dossier(
    headers: HeaderMap,
    State(state): State<AppState>,
    Query(q): Query<PochetteDossierQuery>,
) -> Response {
    servir(
        &state,
        &artwork_cache_dir(),
        &headers,
        q.path,
        q.size.as_deref(),
    )
    .await
}

/// Le corps de la route, répertoire du cache donné (les tests n'en partagent
/// pas : `TUNE_ARTWORK_DIR` est commun au processus).
pub(super) async fn servir(
    state: &AppState,
    cache_dir: &Path,
    headers: &HeaderMap,
    chemin: String,
    size: Option<&str>,
) -> Response {
    let case = case_demandee(size);
    let Ok(_jeton) = LECTURES.acquire().await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let etat = state.clone();
    let source = match hors_executeur("pochette_de_dossier", move || {
        source_de_la_pochette(&etat, &chemin)
    })
    .await
    {
        Ok(Ok(source)) => source,
        Ok(Err(e)) | Err(e) => return e.into_response(),
    };
    match source {
        SourcePochette::Image(image) => {
            let si_aucune_de = headers
                .get(header::IF_NONE_MATCH)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let dir = cache_dir.to_path_buf();
            let vignette = tokio::task::spawn_blocking(move || {
                vignette_d_image(&dir, &image, case, si_aucune_de.as_deref())
            })
            .await
            .unwrap_or(Vignette::Aucune);
            match vignette {
                Vignette::Corps { octets, mime, etag } => {
                    reponse(StatusCode::OK, octets, Some(mime), Some(&etag))
                }
                Vignette::NonModifiee(etag) => {
                    reponse(StatusCode::NOT_MODIFIED, Vec::new(), None, Some(&etag))
                }
                Vignette::Aucune => reponse(StatusCode::NOT_FOUND, Vec::new(), None, None),
            }
        }
        SourcePochette::Album(hash) => {
            let mut r = serve_artwork_from(cache_dir, &hash, Some(case)).await;
            // La pochette d'un album est immuable sous SON adresse ; celle du
            // dossier, non — un album ajouté ou un `folder.jpg` posé la change.
            r.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static(CACHE_CONTROL),
            );
            r
        }
        SourcePochette::Aucune => reponse(StatusCode::NOT_FOUND, Vec::new(), None, None),
    }
}

#[cfg(test)]
#[path = "pochette_de_dossier_8273_tests.rs"]
mod tests;
