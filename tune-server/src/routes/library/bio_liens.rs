//! #7910 — biographies cliquables : `bio_segments` ajoutés aux deux routes
//! `/bio` (artiste et album).
//!
//! Les routes d'origine (`artists::artist_bio`, `albums::album_bio`) gardent
//! tout leur choix de bio (stockée, proxy communautaire, repli) et rendent le
//! même corps qu'avant. Cette enveloppe lit leur réponse et lui ajoute
//! `bio_segments` quand une bio est présente ; `bio` reste le texte brut, pour
//! les clients qui ne connaissent pas les segments.
//!
//! Le découpage (`tune_core::metadata::bio_segments`) est mis en cache avec la
//! bio : la clé porte l'empreinte du texte et celle des liens d'origine, et le
//! cache entier est refait quand l'index partagé
//! (`bio_segments::index_partage`) est rebâti — bibliothèque changée, ou au
//! bout de dix minutes pour suivre un renommage.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::backend::DbBackend;
use tune_core::metadata::bio_segments::{self, BioSegment, GenreDeLien, IndexDesNoms, LienInterne};

use crate::state::AppState;

/// Nombre de bios découpées gardées par base ; au-delà, on repart de zéro.
const SEGMENTS_MAX: usize = 4096;

/// Taille maximale du corps relu (une bio fait quelques kilo-octets).
const CORPS_MAX: usize = 4 * 1024 * 1024;

pub(super) async fn artist_bio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    q: Query<super::artists::LangQuery>,
    headers: HeaderMap,
) -> Response {
    let reponse = super::artists::artist_bio(State(state.clone()), Path(id), q, headers)
        .await
        .into_response();
    let exclus = [LienInterne {
        genre: GenreDeLien::Artiste,
        id,
    }];
    avec_segments(&state.backend, GenreDeLien::Artiste, id, &exclus, reponse).await
}

pub(super) async fn album_bio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    q: Query<super::albums::LangQuery>,
    headers: HeaderMap,
) -> Response {
    let reponse = super::albums::album_bio(State(state.clone()), Path(id), q, headers)
        .await
        .into_response();
    // L'album de la page et son artiste : la fiche album mène déjà à
    // l'artiste, un lien de plus dans la bio ne ferait que répéter.
    let mut exclus = vec![LienInterne {
        genre: GenreDeLien::Album,
        id,
    }];
    if let Some(artiste) = AlbumRepo::with_backend(state.backend.clone())
        .get(id)
        .ok()
        .flatten()
        .and_then(|a| a.artist_id)
    {
        exclus.push(LienInterne {
            genre: GenreDeLien::Artiste,
            id: artiste,
        });
    }
    avec_segments(&state.backend, GenreDeLien::Album, id, &exclus, reponse).await
}

async fn avec_segments(
    db: &Arc<dyn DbBackend>,
    genre: GenreDeLien,
    id: i64,
    exclus: &[LienInterne],
    reponse: Response,
) -> Response {
    if reponse.status() != StatusCode::OK {
        return reponse;
    }
    let (parts, corps) = reponse.into_parts();
    let octets = match axum::body::to_bytes(corps, CORPS_MAX).await {
        Ok(o) => o,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let Ok(mut json) = serde_json::from_slice::<Value>(&octets) else {
        return Response::from_parts(parts, axum::body::Body::from(octets));
    };
    if let Some(bio) = json
        .get("bio")
        .and_then(Value::as_str)
        .filter(|b| !b.trim().is_empty())
    {
        let segments = segments_en_cache(db, genre, id, exclus, bio);
        json["bio_segments"] = serde_json::to_value(segments).unwrap_or(Value::Null);
    }
    let mut sortie = Json(json).into_response();
    for (nom, valeur) in parts.headers.iter() {
        if nom != header::CONTENT_LENGTH && nom != header::CONTENT_TYPE {
            sortie.headers_mut().append(nom.clone(), valeur.clone());
        }
    }
    sortie
}

/// Les bios déjà découpées d'une base, avec l'index qui les a produites
/// ([`bio_segments::index_partage`]) : un index rebâti vide le cache.
struct CacheDUneBase {
    /// Un `Weak` garde l'allocation : l'adresse ne peut pas être reprise par
    /// une autre base tant que l'entrée existe (bases de test successives).
    base: Weak<dyn DbBackend>,
    index: Arc<IndexDesNoms>,
    segments: HashMap<(GenreDeLien, i64, String), Vec<BioSegment>>,
}

fn caches() -> &'static Mutex<Vec<CacheDUneBase>> {
    static CACHES: OnceLock<Mutex<Vec<CacheDUneBase>>> = OnceLock::new();
    CACHES.get_or_init(|| Mutex::new(Vec::new()))
}

fn segments_en_cache(
    db: &Arc<dyn DbBackend>,
    genre: GenreDeLien,
    id: i64,
    exclus: &[LienInterne],
    bio: &str,
) -> Vec<BioSegment> {
    let externes = bio_segments::liens_externes_ranges(db, genre, id, bio);
    let empreinte_externes = serde_json::to_string(&externes).unwrap_or_default();
    let cle = (
        genre,
        id,
        format!(
            "{}:{}",
            bio_segments::empreinte(bio),
            bio_segments::empreinte(&empreinte_externes)
        ),
    );
    let index = bio_segments::index_partage(db);
    let ici = Arc::downgrade(db);
    {
        let mut caches = caches().lock().unwrap_or_else(|e| e.into_inner());
        caches.retain(|c| c.base.strong_count() > 0);
        if let Some(c) = caches
            .iter()
            .find(|c| Weak::ptr_eq(&c.base, &ici) && Arc::ptr_eq(&c.index, &index))
            && let Some(s) = c.segments.get(&cle)
        {
            return s.clone();
        }
    }

    let segments = bio_segments::segments_de_la_bio(bio, &index, exclus, &externes);
    let mut caches = caches().lock().unwrap_or_else(|e| e.into_inner());
    match caches.iter().position(|c| Weak::ptr_eq(&c.base, &ici)) {
        Some(p) => {
            let cache = &mut caches[p];
            // L'index vient d'être rebâti : les segments rangés avec l'ancien
            // ne valent plus.
            if !Arc::ptr_eq(&cache.index, &index) {
                cache.index = index;
                cache.segments.clear();
            }
            if cache.segments.len() >= SEGMENTS_MAX {
                cache.segments.clear();
            }
            cache.segments.insert(cle, segments.clone());
        }
        None => {
            let mut neuf = CacheDUneBase {
                base: ici,
                index,
                segments: HashMap::new(),
            };
            neuf.segments.insert(cle, segments.clone());
            caches.push(neuf);
        }
    }
    segments
}

#[cfg(test)]
#[path = "bio_liens_7910_tests.rs"]
mod bio_liens_7910_tests;
