use std::collections::HashMap;

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::AppState;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::track_metadata_repo::TrackMetadataRepo;
use tune_core::db::track_repo::TrackRepo;

use super::SearchQuery;

/// Body for `POST /library/search/acoustic` — natural-language acoustic search.
/// Its fields are only read by the real handler; the no-feature stub ignores the
/// body, so silence dead-code there rather than emit a build warning.
#[derive(Deserialize)]
#[cfg_attr(not(feature = "audio-embedding"), allow(dead_code))]
pub(super) struct AcousticQuery {
    /// Free-text mood/timbre query, e.g. "warm analog jazz", "driving techno".
    pub query: String,
    /// Max tracks to return (clamped 1..=200; default 50).
    pub limit: Option<i64>,
}

/// Clé de dédoublonnage d'un enregistrement : titre normalisé (casse/espaces),
/// artiste normalisé, durée arrondie à la seconde. Le même enregistrement copié
/// dans deux dossiers produit la même clé ; deux versions d'une même chanson
/// (studio vs live) ont des durées différentes et gardent des clés distinctes —
/// c'est pourquoi on n'utilise PAS titre+artiste seuls.
#[cfg_attr(not(feature = "audio-embedding"), allow(dead_code))]
fn dedup_key(title: &str, artist: Option<&str>, duration_ms: i64) -> (String, String, i64) {
    fn norm(s: &str) -> String {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    }
    (
        norm(title),
        norm(artist.unwrap_or_default()),
        // Arrondi à la seconde la plus proche.
        (duration_ms + 500).div_euclid(1000),
    )
}

/// Dédoublonne une liste de pistes déjà classée par similarité décroissante :
/// garde la première occurrence de chaque clé (meilleure similarité), préserve
/// l'ordre. À appliquer AVANT la troncature à `limit`.
#[cfg_attr(not(feature = "audio-embedding"), allow(dead_code))]
fn dedup_ranked_tracks(
    tracks: Vec<tune_core::db::models::Track>,
) -> Vec<tune_core::db::models::Track> {
    let mut seen = std::collections::HashSet::new();
    tracks
        .into_iter()
        .filter(|t| seen.insert(dedup_key(&t.title, t.artist_name.as_deref(), t.duration_ms)))
        .collect()
}

pub(super) async fn search(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Json<Value> {
    let limit = q.limit.unwrap_or(20);
    let artists = ArtistRepo::with_backend(state.backend.clone())
        .search(&q.q, limit)
        .unwrap_or_default();
    let albums = AlbumRepo::with_backend(state.backend.clone())
        .search(&q.q, limit)
        .unwrap_or_default();
    let albums: Vec<Value> = albums.iter().map(|a| a.to_json()).collect();
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .search(&q.q, limit)
        .unwrap_or_default();

    // --- Extended metadata search (Approach B) ---
    // Search track_metadata for matches in searchable fields (composer,
    // conductor, lyricist, performer, remixer, producer, label, comment,
    // lyrics, isrc, catalog_number). Merge with FTS results.
    let meta_repo = TrackMetadataRepo::with_backend(state.backend.clone());
    let meta_matches = meta_repo.search_by_value(&q.q, limit).unwrap_or_default();

    // Collect track IDs already returned by FTS
    let fts_track_ids: std::collections::HashSet<i64> =
        tracks.iter().filter_map(|t| t.id).collect();

    // Build a map of track_id → matched metadata fields
    let mut matched_metadata: HashMap<i64, HashMap<String, String>> = HashMap::new();
    for (track_id, key, value) in &meta_matches {
        matched_metadata
            .entry(*track_id)
            .or_default()
            .insert(key.clone(), value.clone());
    }

    // Fetch tracks that matched via metadata but not via FTS
    let extra_ids: Vec<i64> = meta_matches
        .iter()
        .map(|(id, _, _)| *id)
        .filter(|id| !fts_track_ids.contains(id))
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let extra_tracks = if extra_ids.is_empty() {
        Vec::new()
    } else {
        TrackRepo::with_backend(state.backend.clone())
            .get_multiple(&extra_ids)
            .unwrap_or_default()
    };

    // Build track JSON: FTS tracks first, then metadata-only tracks.
    // Annotate with matched_metadata where applicable.
    let mut track_results: Vec<Value> = Vec::with_capacity(tracks.len() + extra_tracks.len());
    for t in tracks.iter().chain(extra_tracks.iter()) {
        let mut v = t.to_json();
        if let Some(id) = t.id {
            if let Some(meta) = matched_metadata.get(&id) {
                v.as_object_mut()
                    .unwrap()
                    .insert("matched_metadata".into(), json!(meta));
            }
        }
        track_results.push(v);
    }

    Json(json!({
        "artists": artists,
        "albums": albums,
        "tracks": track_results,
    }))
}

/// POST /library/search/acoustic — natural-language acoustic search (Phase 3).
///
/// Embeds the query with the CLAP text tower (same joint space as the library's
/// audio embeddings) and ranks tracks by cosine similarity — so "warm analog
/// jazz" surfaces acoustically matching tracks whatever their tags say. Premium.
/// Requires an `audio-embedding` build with the text model provisioned; returns
/// 503 when the model can't be loaded, and an empty list when no track has been
/// acoustically analysed yet (the sweep hasn't run / produced vectors).
#[cfg(feature = "audio-embedding")]
pub(super) async fn acoustic_search(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<AcousticQuery>,
) -> Result<Json<Value>, crate::error::AppError> {
    use tune_core::audio::{embedding_store, text_embedding};
    use tune_core::license::Feature;

    if !state
        .license
        .check_feature(Feature::AiRecommendations)
        .await
    {
        return Ok(Json(json!({
            "error": "premium_required",
            "message": "Acoustic search requires a Premium license",
            "tracks": [],
            "count": 0,
        })));
    }

    let query = body.query.trim();
    if query.is_empty() {
        return Err(crate::error::AppError::bad_request(
            "query must not be empty",
        ));
    }
    let limit = body.limit.unwrap_or(50).clamp(1, 200) as usize;

    // La tour texte du CLAP est entraînée en anglais : une requête libre en
    // français recale mal. Si l'utilisateur a configuré une clé API IA
    // (anthropic/openai/gemini), on traduit sa requête (cache local, une
    // seule traduction par requête) ; sans clé ou en cas d'échec, requête
    // brute — comportement historique, les presets anglais ne changent pas.
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let effective_query = tune_core::ai::translate::translate_query(&settings, query)
        .await
        .unwrap_or_else(|| query.to_string());
    if effective_query != query {
        tracing::info!(query = %query, translated = %effective_query, "acoustic_query_translated");
    }
    let query = effective_query.as_str();

    // Text-tower embedding (provisions runtime + model + tokenizer on first call).
    let qvec = text_embedding::embed_query(&state.backend, query)
        .await
        .map_err(|e| {
            crate::error::AppError::service_unavailable(format!(
                "acoustic search model unavailable: {e}"
            ))
        })?;

    // Sur-échantillonne (2× limit) : le dédoublonnage ci-dessous retire des
    // entrées, et il doit s'appliquer AVANT la troncature à `limit` pour ne pas
    // renvoyer moins de résultats que demandé.
    let ranked = embedding_store::rank_by_vector(&state.backend, &qvec, limit * 2, None);
    if ranked.is_empty() {
        // Une liste vide couvrait deux situations opposées : bibliothèque pas
        // encore analysée, ou requête sans correspondance. L'utilisateur ne
        // pouvait pas savoir s'il devait reformuler ou attendre (retour Fabien).
        // On le dit, plutôt que de renvoyer un silence ambigu.
        let analysed = embedding_store::analysed_count(&state.backend);
        if analysed == 0 {
            return Ok(Json(json!({
                "query": query,
                "tracks": [],
                "count": 0,
                "reason": "library_not_analysed",
                "analysed_tracks": 0,
            })));
        }
        return Ok(Json(json!({
            "query": query,
            "tracks": [],
            "count": 0,
            "reason": "no_match",
            "analysed_tracks": analysed,
        })));
    }

    let scores: HashMap<i64, f32> = ranked.iter().copied().collect();
    let ids: Vec<i64> = ranked.iter().map(|(id, _)| *id).collect();
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .get_multiple(&ids)
        .unwrap_or_default();

    // get_multiple preserves the ranked order; drop duplicate recordings (same
    // file copied in two folders → same title/artist/duration), keeping the
    // best-ranked occurrence, then truncate and annotate each with its cosine.
    let tracks = dedup_ranked_tracks(tracks);
    let items: Vec<Value> = tracks
        .iter()
        .take(limit)
        .map(|t| {
            let mut v = t.to_json();
            if let (Some(obj), Some(id)) = (v.as_object_mut(), t.id) {
                if let Some(s) = scores.get(&id) {
                    obj.insert("similarity".into(), json!(s));
                }
            }
            v
        })
        .collect();

    Ok(Json(json!({
        "query": query,
        "count": items.len(),
        "tracks": items,
    })))
}

/// Stub for builds without the `audio-embedding` feature: the text tower isn't
/// linked, so acoustic search is unavailable. Keeps the route present (503) so
/// the web client gets a clear signal rather than a 404.
#[cfg(not(feature = "audio-embedding"))]
pub(super) async fn acoustic_search(
    State(_state): State<AppState>,
    axum::Json(_body): axum::Json<AcousticQuery>,
) -> Result<Json<Value>, crate::error::AppError> {
    Err(crate::error::AppError::service_unavailable(
        "acoustic search is not available in this build",
    ))
}

#[cfg(test)]
mod tests {
    use super::{dedup_key, dedup_ranked_tracks};
    use tune_core::db::models::Track;

    fn track(id: i64, title: &str, artist: &str, duration_ms: i64) -> Track {
        let mut t = Track::new(title.to_string());
        t.id = Some(id);
        t.artist_name = Some(artist.to_string());
        t.duration_ms = duration_ms;
        t
    }

    #[test]
    fn dedup_key_normalise_casse_espaces_et_duree() {
        // Casse et espaces (bords + internes multiples) sont normalisés ;
        // la durée est arrondie à la seconde la plus proche.
        assert_eq!(
            dedup_key("  Road   Movie ", Some("Bernard  LAVILLIERS"), 249_733),
            dedup_key("road movie", Some("bernard lavilliers"), 250_499),
        );
        // Artiste absent == artiste vide.
        assert_eq!(
            dedup_key("Titre", None, 1_000),
            dedup_key("Titre", Some(""), 1_000)
        );
        // 249 733 ms → 250 s ; 249 400 ms → 249 s : clés différentes.
        assert_ne!(
            dedup_key("Road Movie", Some("Bernard Lavilliers"), 249_733),
            dedup_key("Road Movie", Some("Bernard Lavilliers"), 249_400),
        );
    }

    #[test]
    fn dedup_garde_premiere_occurrence_et_ordre() {
        // Cas réel .18 : même fichier dans deux dossiers (ids 53273 / 66258).
        let tracks = vec![
            track(53273, "Road Movie", "Bernard Lavilliers", 249_733),
            track(1, "Autre Piste", "Quelqu'un", 180_000),
            track(66258, "Road Movie", "Bernard Lavilliers", 249_733),
            track(2, "Encore Une", "Quelqu'un", 200_000),
        ];
        let out = dedup_ranked_tracks(tracks);
        let ids: Vec<i64> = out.iter().filter_map(|t| t.id).collect();
        // Le doublon exact disparaît, la première occurrence (meilleure
        // similarité) est conservée, l'ordre du classement est préservé.
        assert_eq!(ids, vec![53273, 1, 2]);
    }

    #[test]
    fn dedup_conserve_versions_studio_et_live() {
        // Même titre + même artiste mais durées différentes (studio vs live) :
        // les deux versions doivent rester.
        let tracks = vec![
            track(10, "Road Movie", "Bernard Lavilliers", 249_733),
            track(11, "Road Movie", "Bernard Lavilliers", 312_000),
        ];
        let out = dedup_ranked_tracks(tracks);
        assert_eq!(out.len(), 2);
    }
}

/// GET /library/search/acoustic/status — de quoi décider si l'écran Ambiance a
/// une chance de servir.
///
/// Trois informations distinctes, qu'il ne faut pas confondre :
/// - `available` : le binaire embarque la brique acoustique ;
/// - `enabled`   : l'analyse est activée sur ce serveur ;
/// - `analysed_tracks` : ce qui a déjà été analysé.
///
/// Le client masque l'entrée de navigation quand la fonction ne peut pas
/// marcher, et affiche l'avancement quand l'analyse tourne — plutôt que de
/// proposer une porte fermée (retour Fabien, arbitrage Bertrand).
pub(super) async fn acoustic_status(State(state): State<AppState>) -> Json<Value> {
    let available = cfg!(feature = "audio-embedding");
    let enabled = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .get("audio_embedding_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);

    #[cfg(feature = "audio-embedding")]
    let analysed = tune_core::audio::embedding_store::analysed_count(&state.backend);
    #[cfg(not(feature = "audio-embedding"))]
    let analysed = 0_i64;

    // Dénominateur de la progression : les pistes que la passe peut analyser,
    // pas la bibliothèque entière (le DSD et les pistes sans fichier local en
    // sont exclus). Sans lui, l'interface n'avait qu'un compteur qui montait
    // sans qu'on sache vers quoi.
    #[cfg(feature = "audio-embedding")]
    let eligible = tune_core::audio::embedding_store::eligible_count(&state.backend);
    #[cfg(not(feature = "audio-embedding"))]
    let eligible = 0_i64;

    // Numérateur de la progression : les pistes TRAITÉES pour le modèle
    // courant, pas celles dont on a tiré un embedding. Les deux diffèrent des
    // échecs, et confondre les deux figeait la jauge sous 100 % à jamais
    // (#1819). C'est `processed` qui doit piloter la barre : il atteint le
    // dénominateur quand il ne reste plus rien à faire.
    #[cfg(feature = "audio-embedding")]
    let processed = tune_core::audio::embedding_store::processed_count(&state.backend);
    #[cfg(not(feature = "audio-embedding"))]
    let processed = 0_i64;

    // Les pistes finies mais sans embedding. À dire franchement : « 51 pistes
    // n'ont pas pu être analysées » se comprend, une jauge coincée à 99,8 % non.
    #[cfg(feature = "audio-embedding")]
    let failed = (processed - analysed).max(0);
    #[cfg(not(feature = "audio-embedding"))]
    let failed = 0_i64;

    let throttle = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .get("audio_embedding_throttle")
        .ok()
        .flatten()
        .filter(|v| matches!(v.as_str(), "eco" | "equilibre" | "rapide"))
        .unwrap_or_else(|| "equilibre".to_string());

    // La passe peut-elle réellement travailler ? Sans cette information,
    // l'interface affichait « Analyse en cours — 0 % » aussi bien pour une
    // analyse qui démarre que pour une analyse incapable de démarrer, et
    // l'utilisateur concluait à un blocage (Fabien, v0.9.68).
    #[cfg(feature = "audio-embedding")]
    let model_ready = tune_core::audio::embedding::model_ready(
        &tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone()),
    );
    #[cfg(not(feature = "audio-embedding"))]
    let model_ready = false;

    // `model_ready` seul confond « jamais tenté », « en cours de
    // téléchargement » et « en échec ». Les trois donnaient le même message à
    // l'utilisateur, qui allait alors chercher la panne du côté de sa connexion
    // (#1658) ou concluait à une jauge bloquée (#1512).
    #[cfg(feature = "audio-embedding")]
    let model_fetch = {
        let f = tune_core::audio::embedding::fetch_state("audio_model");
        json!({
            "in_progress": f.in_progress,
            "downloaded_bytes": f.downloaded,
            "total_bytes": f.total,
            "attempts": f.attempts,
            "last_error": f.last_error,
        })
    };
    #[cfg(not(feature = "audio-embedding"))]
    let model_fetch = json!(null);

    // Pourquoi la passe ne travaille pas, quand elle ne travaille pas.
    //
    // Une passe en pause et une passe cassée donnaient exactement le même
    // écran — jauge immobile, rien qui bouge. Bilou a ouvert un fil sur une
    // analyse « qui ne démarre pas » (#1457) alors qu'elle cédait le passage à
    // sa musique, comme prévu. `null` quand rien ne l'empêche de tourner.
    #[cfg(feature = "audio-embedding")]
    let paused_reason = tune_core::audio::embedding::pause_acoustique().nom();
    #[cfg(not(feature = "audio-embedding"))]
    let paused_reason: Option<&str> = None;

    Json(json!({
        "available": available,
        "enabled": enabled,
        "model_ready": model_ready,
        "model_fetch": model_fetch,
        // « playback » | « thermal » | « low_memory » | « not_premium » | null
        "paused_reason": paused_reason,
        // Embeddings réellement écrits pour le modèle courant.
        "analysed_tracks": analysed,
        // Pistes traitées (embedding écrit OU échec constaté) : le numérateur
        // de la barre, celui qui atteint le dénominateur quand c'est fini.
        "processed_tracks": processed,
        // Traitées sans embedding. L'interface doit les nommer, pas les taire.
        "failed_tracks": failed,
        "eligible_tracks": eligible,
        // Ce qui RESTE à faire, mesuré sur les pistes traitées et non sur les
        // embeddings : sinon les échecs restaient éternellement « en attente »
        // et la jauge ne finissait jamais (#1819).
        "pending_tracks": (eligible - processed).max(0),
        "throttle": throttle,
    }))
}
