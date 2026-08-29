use axum::Json;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::AppState;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::track_repo::{TrackRepo, dedup_display_tracks};

use super::{Pagination, api_cache_get, api_cache_set, artwork_cache_dir, now_iso_utc};

#[derive(Deserialize)]
pub(super) struct LangQuery {
    lang: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ImageReportBody {
    reason: Option<String>,
}

pub(super) async fn list_artists(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);
    let total = repo.count().unwrap_or(0);
    let items = match repo.list(limit, offset) {
        Ok(artists) => artists,
        Err(e) => {
            tracing::error!(
                error = %e,
                limit,
                offset,
                total,
                "list_artists_query_failed — stats show {total} artists but query returned error"
            );
            Vec::new()
        }
    };
    Json(json!({"items": items, "total": total, "limit": limit, "offset": offset}))
}

pub(super) async fn get_artist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    match repo.get(id) {
        Ok(Some(artist)) => {
            let mut j = json!(artist);
            if let (Some(obj), Ok(Some(prov))) = (j.as_object_mut(), repo.bio_provenance(id)) {
                obj.insert("bio_provenance".into(), prov);
            }
            Json(j).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub(super) async fn artist_bio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<LangQuery>,
) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let artist = repo.get(id).ok().flatten();
    let Some(artist) = artist else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let lang = q.lang.as_deref().unwrap_or("fr");

    // Prefer a locally-enriched bio (with provenance/attribution) over the
    // community proxy — this surfaces the sourced Wikipedia/Last.fm/Qobuz/
    // TheAudioDB bio and its licence to the client.
    //
    // MAIS seulement si elle est dans la bonne langue (#1849, Dimitri).
    //
    // Cette branche rendait la bio stockée sans jamais regarder ni sa langue ni
    // celle qu'on demande, et le `lang` ci-dessus n'était donc JAMAIS atteint
    // pour un artiste qui en possédait une. Or l'enrichissement récupère les
    // bios dans la langue de CELUI QUI LE LANCE (`enrich.rs:139`,
    // `lang_from_header`) : une bibliothèque enrichie depuis une interface en
    // français stocke tout en français, et le sert ensuite à tout le monde.
    // C'est exactement ce que voit Dimitri.
    let prov = repo.bio_provenance(id).ok().flatten();
    let bio_lang = prov
        .as_ref()
        .and_then(|p| p.get("lang").and_then(|v| v.as_str()));
    let stored_ok = langue_convient(bio_lang, lang);

    if let Some(ref bio) = artist.bio {
        if !bio.is_empty() && stored_ok {
            return Json(json!({
                "artist": artist.name,
                "bio": bio,
                "bio_provenance": prov,
            }))
            .into_response();
        }
    }
    // Community artist-bio API is keyed by NAME (AI-generated on demand) — NO
    // MusicBrainz id required, so it works for the whole library, most of which
    // has no MBID. Mirrors the album path.
    let cache_key = format!("cache:artistbio:{}:{lang}", artist.name);
    if let Some(cached) = api_cache_get(&state.backend, &cache_key) {
        return Json(cached).into_response();
    }
    match state
        .http_client
        .get("https://mozaiklabs.fr/api/v1/artists/bio")
        // `lang` est transmis au site, qui ne le recevait pas : il ne servait
        // qu'à nommer l'entrée de cache. ⚠️ L'effet dépend du site
        // (`site-mozaiklabs`) : s'il ignore le paramètre, il rendra la même
        // langue qu'avant — sans régression, mais sans gain non plus. À
        // vérifier là-bas avant d'annoncer #1849 comme réglé.
        .query(&[("name", artist.name.as_str()), ("lang", lang)])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let data: Value = resp.json().await.unwrap_or(json!({}));
            let out = json!({
                "artist": artist.name,
                "bio": data.get("bio").cloned().unwrap_or(Value::Null),
                "source": data.get("source").cloned().unwrap_or(Value::Null),
            });
            if out.get("bio").map(|b| !b.is_null()).unwrap_or(false) {
                api_cache_set(&state.backend, &cache_key, &out);
            }
            Json(out).into_response()
        }
        _ => repli_sur_la_bio_stockee(&artist.name, artist.bio.as_deref(), prov),
    }
}

/// La langue d'une bio stockée convient-elle à celle qu'on demande ?
///
/// Comparaison sur la base seule : `fr` et `fr-FR` désignent la même chose, et
/// une bio Wikipédia française n'est pas moins française parce qu'un client
/// annonce `fr-CA`.
///
/// ⚠️ Une langue INCONNUE — colonne vide sur une ligne ancienne — est acceptée.
/// C'est délibéré : refuser déclencherait un appel réseau pour chaque artiste
/// dont la provenance n'a jamais été renseignée, à la première ouverture de
/// chaque fiche. On ne dispose d'aucune preuve que ces bios soient dans la
/// mauvaise langue ; un ré-enrichissement renseignera `bio_lang` et la
/// comparaison redeviendra exacte.
pub(super) fn langue_convient(stockee: Option<&str>, demandee: &str) -> bool {
    let Some(stockee) = stockee.map(str::trim).filter(|s| !s.is_empty()) else {
        return true;
    };
    let base = |s: &str| {
        s.split(['-', '_'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
    };
    base(stockee) == base(demandee)
}

/// Ressert la bio stockée quand la langue demandée n'a rien donné.
///
/// « Une biographie en français vaut mieux qu'un panneau vide » — le ticket le
/// dit, et c'est juste : refuser une bio dans la mauvaise langue serait
/// remplacer un défaut par un pire. La provenance part avec elle, donc le
/// client sait dans quelle langue elle est et peut le signaler.
fn repli_sur_la_bio_stockee(
    nom: &str,
    bio: Option<&str>,
    prov: Option<Value>,
) -> axum::response::Response {
    match bio.filter(|b| !b.is_empty()) {
        Some(b) => Json(json!({
            "artist": nom,
            "bio": b,
            "bio_provenance": prov,
        }))
        .into_response(),
        None => Json(json!({"artist": nom, "bio": null})).into_response(),
    }
}

pub(super) async fn artist_similar(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let artist = repo.get(id).ok().flatten();
    let Some(artist) = artist else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(ref mbid) = artist.musicbrainz_id else {
        return Json(json!({"artist": artist.name, "artists": []})).into_response();
    };
    let cache_key = format!("cache:similar:{mbid}");
    if let Some(cached) = api_cache_get(&state.backend, &cache_key) {
        return Json(cached).into_response();
    }
    match state
        .http_client
        .get(format!("https://mozaiklabs.fr/api/{mbid}/similar"))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let data: Value = resp.json().await.unwrap_or(json!({}));
            api_cache_set(&state.backend, &cache_key, &data);
            Json(data).into_response()
        }
        _ => Json(json!({"mbid": mbid, "artists": []})).into_response(),
    }
}

pub(super) async fn artist_metadata(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let artist = repo.get(id).ok().flatten();
    let Some(artist) = artist else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(ref mbid) = artist.musicbrainz_id else {
        return Json(json!(artist)).into_response();
    };
    let cache_key = format!("cache:meta:{mbid}");
    if let Some(cached) = api_cache_get(&state.backend, &cache_key) {
        return Json(cached).into_response();
    }
    match state
        .http_client
        .get(format!("https://mozaiklabs.fr/api/{mbid}"))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let data: Value = resp.json().await.unwrap_or(json!({}));
            api_cache_set(&state.backend, &cache_key, &data);
            Json(data).into_response()
        }
        _ => Json(json!(artist)).into_response(),
    }
}

pub(super) async fn artist_albums(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Json<Value> {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let items = repo.list_by_artist(id).unwrap_or_default();
    let items: Vec<Value> = items.iter().map(|a| a.to_json()).collect();
    Json(json!(items))
}

pub(super) async fn artist_tracks(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Json<Value> {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let items = dedup_display_tracks(repo.list_by_artist(id).unwrap_or_default());
    Json(json!(items))
}

pub(super) async fn artist_timeline(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let artist_repo = ArtistRepo::with_backend(state.backend.clone());
    let artist = match artist_repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let mut albums = repo.list_by_artist(id).unwrap_or_default();
    albums.sort_by(|a, b| a.year.unwrap_or(0).cmp(&b.year.unwrap_or(0)));

    let years: Vec<i32> = albums.iter().filter_map(|a| a.year).collect();
    let mut gaps = Vec::new();
    for w in years.windows(2) {
        if w[1] - w[0] > 1 {
            gaps.push(json!({"from": w[0], "to": w[1], "years": w[1] - w[0]}));
        }
    }

    let items: Vec<Value> = albums.iter().map(|a| a.to_json()).collect();
    Json(json!({
        "artist": artist.name,
        "artist_id": id,
        "albums": items,
        "gaps": gaps,
        "career_span": if years.len() >= 2 { Some(json!({"first": years[0], "last": years[years.len()-1], "years": years[years.len()-1] - years[0]})) } else { None },
    }))
    .into_response()
}

pub(super) async fn artist_image(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let artist = match repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let Some(ref image_path) = artist.image_path else {
        return StatusCode::NOT_FOUND.into_response();
    };

    if image_path.starts_with("http") {
        return axum::response::Redirect::temporary(image_path).into_response();
    }

    // If it's already a hex hash, use it directly; otherwise hash it
    let hash = if super::artwork_is_hex_hash(image_path) {
        image_path.to_string()
    } else {
        tune_core::library::artwork::artwork_hash(image_path)
    };

    axum::response::Redirect::temporary(&format!("/api/v1/library/artwork/{hash}")).into_response()
}

pub(super) async fn artist_image_upload(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let artist_repo = ArtistRepo::with_backend(state.backend.clone());
    let mut artist = match artist_repo.get(id) {
        Ok(Some(a)) => a,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "artist not found"})),
            )
                .into_response();
        }
    };
    let mut image_data: Option<Vec<u8>> = None;
    let mut ext = "jpg".to_string();
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "image" || name == "file" {
            if let Some(ct) = field.content_type() {
                if ct.contains("png") {
                    ext = "png".to_string();
                }
            }
            image_data = field.bytes().await.ok().map(|b| b.to_vec());
        }
    }
    let Some(data) = image_data else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no image provided"})),
        )
            .into_response();
    };
    let cache_dir = artwork_cache_dir();
    // Condensat de CONTENU, plus d'identité figée (#1444) : sous
    // `artwork_hash("artist-{id}")`, remplacer l'image gardait la même URL
    // servie `immutable` — l'ancienne image restait affichée. Voir le
    // commentaire complet dans `artwork.rs::upload_album_artwork`.
    let hash = tune_core::library::artwork::content_hash(&data);
    if tune_core::library::artwork::save_to_cache(&data, &cache_dir, &hash, &ext).is_none() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to save image"})),
        )
            .into_response();
    }
    artist.image_path = Some(hash.clone());
    artist.image_source = Some("upload".into());
    artist_repo.update(&artist).ok();
    Json(json!({
        "artist_id": id,
        "hash": hash,
        "size": data.len(),
    }))
    .into_response()
}

pub(super) async fn artist_image_report(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    // Optional body: the web flag button POSTs with no JSON payload, so a
    // required `Json<ImageReportBody>` extractor rejected it with 422 and the
    // UI showed "erreur lors du signalement" (Jean Valjean #1096). Accept a
    // missing/empty body; `reason` stays None.
    body: Option<Json<ImageReportBody>>,
) -> impl IntoResponse {
    let reason = body.and_then(|Json(b)| b.reason);
    // Recorded in metadata_reports like every other report. It used to be
    // stashed as a settings key (reported_artist_image_{id}), which no code
    // could list or aggregate and which never reached the community backend.
    tune_core::db::metadata_report_repo::MetadataReportRepo::with_backend(state.backend.clone())
        .insert(
            "artist_image",
            Some(id),
            None,
            None,
            None,
            reason.as_deref().unwrap_or("incorrect_image"),
            None,
            &now_iso_utc(),
        )
        .ok();
    // Also remove the wrong image locally so the artist shows a placeholder and
    // a fresh image is re-fetched on the next enrichment (Jean Valjean #1096:
    // "supprimer les images incorrectes en appuyant sur le drapeau").
    let cleared = ArtistRepo::with_backend(state.backend.clone())
        .clear_image(id)
        .is_ok();
    Json(json!({"reported": true, "artist_id": id, "image_cleared": cleared}))
}

// ---------------------------------------------------------------------------
// PUT /artists/{id} — update artist metadata (name, sort_name, bio)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(super) struct ArtistUpdate {
    name: Option<String>,
    sort_name: Option<String>,
    bio: Option<String>,
    musicbrainz_id: Option<String>,
}

pub(super) async fn update_artist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<ArtistUpdate>,
) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let mut artist = match repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    if let Some(ref v) = body.name {
        let trimmed = v.trim();
        if trimmed.is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "artist name cannot be empty"})),
            )
                .into_response();
        }
        artist.name = trimmed.to_string();
    }
    if let Some(ref v) = body.sort_name {
        artist.sort_name = Some(v.clone());
    }
    if let Some(ref v) = body.bio {
        artist.bio = if v.is_empty() { None } else { Some(v.clone()) };
    }
    if let Some(ref v) = body.musicbrainz_id {
        artist.musicbrainz_id = if v.is_empty() { None } else { Some(v.clone()) };
    }

    if let Err(e) = repo.update(&artist) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("update failed: {e}")})),
        )
            .into_response();
    }

    if body.name.is_some() {
        let db = state.backend.clone();
        let new_name = artist.name.clone();
        use tune_core::db::backend::ToSqlValue;
        use tune_core::db::engine::Engine;
        let p1 = if db.engine() == Engine::Postgres {
            "$1"
        } else {
            "?1"
        };
        let p2 = if db.engine() == Engine::Postgres {
            "$2"
        } else {
            "?2"
        };
        let _ = db.execute(
            &format!("UPDATE tracks SET artist_name = {p1} WHERE artist_id = {p2}"),
            &[&new_name as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        );
        let _ = db.execute(
            &format!("UPDATE albums SET artist_name = {p1} WHERE artist_id = {p2}"),
            &[&new_name as &dyn ToSqlValue, &id as &dyn ToSqlValue],
        );
    }

    Json(json!(artist)).into_response()
}

#[cfg(test)]
mod tests_langue_bio {
    use super::langue_convient;

    /// Le cas de Dimitri (#1849) : bibliothèque enrichie en français, lecteur
    /// anglophone. La bio stockée ne doit PAS être resservie telle quelle.
    #[test]
    fn une_bio_francaise_ne_convient_pas_a_une_demande_anglaise() {
        assert!(!langue_convient(Some("fr"), "en"));
    }

    #[test]
    fn la_meme_langue_convient() {
        assert!(langue_convient(Some("fr"), "fr"));
        assert!(langue_convient(Some("en"), "en"));
    }

    /// `fr` et `fr-FR` désignent la même chose : une bio Wikipédia française
    /// n'est pas moins française parce qu'un client annonce `fr-CA`.
    #[test]
    fn seule_la_base_de_la_langue_compte() {
        assert!(langue_convient(Some("fr"), "fr-FR"));
        assert!(langue_convient(Some("fr-FR"), "fr"));
        assert!(langue_convient(Some("fr_CA"), "fr-BE"));
        assert!(langue_convient(Some("EN"), "en-GB"));
        assert!(!langue_convient(Some("fr-FR"), "en-GB"));
    }

    /// Une langue inconnue est ACCEPTÉE, délibérément. Refuser déclencherait un
    /// appel réseau pour chaque artiste dont la provenance n'a jamais été
    /// renseignée, à la première ouverture de chaque fiche — et rien ne dit que
    /// ces bios soient dans la mauvaise langue.
    #[test]
    fn une_langue_inconnue_est_acceptee() {
        assert!(langue_convient(None, "en"));
        assert!(langue_convient(Some(""), "en"));
        assert!(langue_convient(Some("   "), "en"));
    }
}
