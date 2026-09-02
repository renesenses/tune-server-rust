use axum::Json;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
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
    headers: HeaderMap,
) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let artist = repo.get(id).ok().flatten();
    let Some(artist) = artist else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let lang = langue_demandee(q.lang.as_deref(), &headers);
    let lang = lang.as_str();

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

/// Quelle langue la requête demande-t-elle, pour les deux routes « bio » ?
///
/// Précédence, dans cet ordre :
/// 1. le paramètre `?lang=` explicite — un appel qui nomme sa langue gagne ;
/// 2. l'en-tête `Accept-Language`, via [`crate::i18n::lang_from_header`], la
///    MÊME lecture que `browse.rs` et `system/enrich.rs` ;
/// 3. `fr`, repli déjà porté par `lang_from_header`.
///
/// Sans l'étape 2, les deux routes repliaient droit sur `"fr"` (#1849) : le
/// client web n'envoie `?lang=` sur aucune des deux, il transmet sa locale par
/// l'en-tête sur CHAQUE requête (`tune-web-client/src/lib/api.ts`). Un
/// utilisateur en interface anglaise obtenait donc `lang = "fr"`,
/// `langue_convient(Some("fr"), "fr")` était vrai, et la bio française lui
/// était resservie — le garde-fou de langue livré au-dessus n'ayant jamais
/// l'occasion de jouer.
///
/// Un `?lang=` vide (`?lang=`) est traité comme absent : il ne nomme aucune
/// langue, et le laisser passer donnerait `lang = ""`, qui ne convient à
/// aucune bio estampillée et déclencherait un appel réseau pour rien.
pub(super) fn langue_demandee(param: Option<&str>, headers: &HeaderMap) -> String {
    param
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| crate::i18n::lang_from_header(headers))
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

/// Taille maximale d'une image d'artiste deposee a la main.
///
/// Le `DefaultBodyLimit` global (50 Mo) laisse passer n'importe quelle photo
/// d'appareil. Elle serait ecrite dans le cache, puis relue entierement a
/// chaque vignette. Au-dela de cette borne la route le DIT, au lieu de
/// repondre 200 (#3102).
pub(super) const IMAGE_ARTISTE_MAX_OCTETS: usize = 10 * 1024 * 1024;

/// Le coeur de [`artist_image_upload`], repertoire de cache donne.
///
/// Separe du gestionnaire pour etre eprouve sans variable d'environnement :
/// `artwork_cache_dir()` lit `TUNE_ARTWORK_DIR`, commun a tout le processus et
/// donc inutilisable depuis des essais paralleles. Meme decoupage que
/// `serve_artwork_from`, juste en face.
///
/// La reponse porte l'ARTISTE RELU EN BASE — `image_path` compris. Elle ne
/// portait que `artist_id`, `hash` et `size` : le client la lit pourtant comme
/// un artiste complet (`api.ts:1390` la type `Promise<Artist>`,
/// `ArtistEditModal.svelte:49` fait `updated.image_path ?? null`), donc
/// `image_path` valait `undefined`, l'apercu retombait sur le gabarit vide, et
/// le testeur voyait « enregistre » avec une vignette vide (#3102, Fuccaro).
pub(super) fn enregistrer_image_artiste(
    artist_repo: &ArtistRepo,
    id: i64,
    data: &[u8],
    cache_dir: &std::path::Path,
) -> Response {
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

    if data.len() > IMAGE_ARTISTE_MAX_OCTETS {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({
                "error": "image too large",
                "size": data.len(),
                "max_size": IMAGE_ARTISTE_MAX_OCTETS,
            })),
        )
            .into_response();
    }

    // Le format vient des OCTETS, jamais du `content-type` annonce par le
    // client : celui-ci n'etait consulte que pour y chercher « png », et tout
    // le reste — WebP de Discogs compris — etait ecrit sous `.jpg` puis servi
    // `image/jpeg`. Un format que la lecture ne sait pas resservir est refuse
    // EN LE DISANT (#3102).
    let Some(ext) = tune_core::library::artwork::sniff_image_ext(data) else {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(json!({
                "error": "unsupported image format",
                "supported": tune_core::library::artwork::FORMATS_IMAGE_SERVABLES,
            })),
        )
            .into_response();
    };

    // Condensat de CONTENU, plus d'identite figee (#1444) : sous
    // `artwork_hash("artist-{id}")`, remplacer l'image gardait la meme URL
    // servie `immutable` — l'ancienne image restait affichee. Voir le
    // commentaire complet dans `artwork.rs::upload_album_artwork`.
    let hash = tune_core::library::artwork::content_hash(data);
    if tune_core::library::artwork::save_to_cache(data, cache_dir, &hash, ext).is_none() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to save image"})),
        )
            .into_response();
    }

    artist.image_path = Some(hash.clone());
    artist.image_source = Some("upload".into());
    // L'ecriture etait jetee par un `.ok()` : une base en echec repondait
    // quand meme 200 avec le meme corps, et le prochain chargement perdait
    // l'image sans que rien ne l'ait dit.
    if let Err(e) = artist_repo.update(&artist) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("update failed: {e}")})),
        )
            .into_response();
    }

    // On rend l'artiste RELU, pas celui qu'on vient de construire : la reponse
    // dit alors ce que le prochain chargement lira, et non ce qu'on esperait
    // avoir ecrit.
    let enregistre = match artist_repo.get(id) {
        Ok(Some(a)) if a.image_path.as_deref() == Some(hash.as_str()) => a,
        _ => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "image saved but not indexed"})),
            )
                .into_response();
        }
    };

    let mut corps = json!(enregistre);
    if let Some(obj) = corps.as_object_mut() {
        // Les trois cles historiques restent : d'autres clients les lisent.
        obj.insert("artist_id".into(), json!(id));
        obj.insert("hash".into(), json!(hash));
        obj.insert("size".into(), json!(data.len()));
    }
    Json(corps).into_response()
}

pub(super) async fn artist_image_upload(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let mut image_data: Option<Vec<u8>> = None;
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "image" || name == "file" {
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

    let artist_repo = ArtistRepo::with_backend(state.backend.clone());
    enregistrer_image_artiste(&artist_repo, id, &data, &artwork_cache_dir())
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

// ---------------------------------------------------------------------------
// #3102 — l'image d'artiste posee a la main.
// ---------------------------------------------------------------------------
//
// Fuccaro, fil forum 1317, 01/09/2026, PC sous Windows 11 : « J'essaye de les
// ajouter manuellement via le bouton "editer l'artiste", en telechargeant une
// image sur Discogs. Le message est : "enregistre" mais je n'ai pas l'image
// qui s'affiche. »
//
// Aucun essai ici ne se contente d'un code HTTP — le symptome est justement
// que le 200 mentait. Chacun va chercher les OCTETS que la route de service
// rend a l'adresse que la reponse annonce.
#[cfg(test)]
mod tests_image_artiste_televersee {
    use super::*;
    use std::sync::Arc;
    use tune_core::db::backend::DbBackend;
    use tune_core::db::models::Artist;

    fn base_memoire() -> Arc<dyn DbBackend> {
        let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    fn artiste(repo: &ArtistRepo, nom: &str) -> i64 {
        repo.create(&Artist::new(nom.into()))
            .expect("insert artist")
    }

    /// Un PNG 1x1 VALIDE, fabrique ici octet par octet. Aucune image de
    /// testeur n'entre dans ce depot.
    fn png_1x1() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ]
    }

    /// Le format que Discogs sert de plus en plus, et que l'ancien
    /// gestionnaire rangeait sous `.jpg`.
    fn webp_minimal() -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&[0x14, 0, 0, 0]);
        v.extend_from_slice(b"WEBPVP8 ");
        v.extend_from_slice(b"octets-webp");
        v
    }

    async fn json_de(reponse: Response) -> (StatusCode, Value) {
        let statut = reponse.status();
        let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            statut,
            serde_json::from_slice(&octets).unwrap_or(Value::Null),
        )
    }

    /// Ce que le navigateur obtient reellement a l'adresse annoncee : le code,
    /// le type MIME, et les octets. C'est la route de service de production
    /// (`serve_artwork_from`), pas une relecture de fichier a la main.
    async fn servi(cache: &std::path::Path, hash: &str) -> (StatusCode, String, Vec<u8>) {
        let reponse = super::super::artwork::serve_artwork_from(cache, hash, None).await;
        let statut = reponse.status();
        let mime = reponse
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (statut, mime, octets)
    }

    /// L'EPREUVE QUI TRANCHE.
    ///
    /// Apres televersement, l'adresse rendue par la reponse sert reellement
    /// l'image televersee — et un second chargement, qui repart de la base
    /// comme le fait l'ecran apres un F5, la rend encore.
    ///
    /// Avant correctif la reponse ne portait que `artist_id`, `hash` et
    /// `size` : `body["image_path"]` etait absent, l'assertion tombe des la
    /// premiere ligne. Remettre ces trois cles seules dans
    /// `enregistrer_image_artiste` la fait retomber.
    #[tokio::test]
    async fn l_adresse_rendue_sert_l_image_televersee_et_la_rend_encore() {
        let cache = tune_core::test_scratch::scratch_dir("tune-3102-adresse");
        let repo = ArtistRepo::with_backend(base_memoire());
        let id = artiste(&repo, "Taj Mahal");
        let octets = png_1x1();

        let (statut, corps) =
            json_de(enregistrer_image_artiste(&repo, id, &octets, cache.path())).await;
        assert_eq!(statut, StatusCode::OK, "corps: {corps}");

        let adresse = corps["image_path"]
            .as_str()
            .unwrap_or_else(|| panic!("la reponse ne porte pas l'adresse de l'image : {corps}"))
            .to_string();

        // Premier chargement : ce que l'apercu du modal demande sur-le-champ.
        let (code, mime, servis) = servi(cache.path(), &adresse).await;
        assert_eq!(code, StatusCode::OK, "l'adresse annoncee n'est pas servie");
        assert_eq!(mime, "image/png");
        assert_eq!(servis, octets, "ce ne sont pas les octets televerses");

        // Second chargement : la fiche relue en base, comme apres un F5.
        let relu = repo.get(id).unwrap().expect("artiste present");
        assert_eq!(
            relu.image_path.as_deref(),
            Some(adresse.as_str()),
            "la base ne connait pas l'adresse que la reponse a annoncee"
        );
        assert_eq!(relu.image_source.as_deref(), Some("upload"));
        let (code2, _, servis2) = servi(cache.path(), relu.image_path.as_deref().unwrap()).await;
        assert_eq!(code2, StatusCode::OK);
        assert_eq!(servis2, octets, "le second chargement perd l'image");
    }

    /// Le format se lit dans les octets : un WebP est servi `image/webp`, pas
    /// range sous `.jpg` et annonce `image/jpeg`.
    #[tokio::test]
    async fn un_webp_est_servi_comme_un_webp() {
        let cache = tune_core::test_scratch::scratch_dir("tune-3102-webp");
        let repo = ArtistRepo::with_backend(base_memoire());
        let id = artiste(&repo, "Stretch");
        let octets = webp_minimal();

        let (statut, corps) =
            json_de(enregistrer_image_artiste(&repo, id, &octets, cache.path())).await;
        assert_eq!(statut, StatusCode::OK, "corps: {corps}");
        let adresse = corps["image_path"].as_str().expect("adresse").to_string();

        let (code, mime, servis) = servi(cache.path(), &adresse).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(mime, "image/webp", "l'extension ecrite ment sur le contenu");
        assert_eq!(servis, octets);

        // TEMOIN de l'enrichissement automatique : c'est le predicat exact sur
        // lequel `batch_enrich_artist_artwork_inner` decide de remettre un
        // artiste dans sa file — donc d'ecraser l'image posee a la main.
        assert!(
            tune_core::library::artwork::cached_artwork_exists(cache.path(), &adresse),
            "la passe automatique croirait le cache vide et ecraserait l'image"
        );
    }

    /// TEMOIN : un artiste sans image n'en a toujours aucune, proprement.
    /// Aucune entree fantome, aucun condensat annonce dans le vide.
    #[tokio::test]
    async fn un_artiste_sans_image_n_en_annonce_aucune() {
        let cache = tune_core::test_scratch::scratch_dir("tune-3102-vide");
        let repo = ArtistRepo::with_backend(base_memoire());
        let id = artiste(&repo, "Charades");

        let relu = repo.get(id).unwrap().expect("artiste present");
        assert_eq!(relu.image_path, None);
        assert!(!tune_core::library::artwork::cached_artwork_exists(
            cache.path(),
            "ffffffffffffffffffffffffffffffff"
        ));
        let (code, _, _) = servi(cache.path(), "ffffffffffffffffffffffffffffffff").await;
        assert_eq!(code, StatusCode::NOT_FOUND);
    }

    /// TEMOIN : un fichier refuse echoue EN LE DISANT. Pas de 200, et surtout
    /// pas d'adresse annoncee en base pour un fichier que la lecture ne
    /// saurait pas resservir.
    #[tokio::test]
    async fn un_fichier_qui_n_est_pas_une_image_est_refuse_en_le_disant() {
        let cache = tune_core::test_scratch::scratch_dir("tune-3102-format");
        let repo = ArtistRepo::with_backend(base_memoire());
        let id = artiste(&repo, "Sam Cooke");

        let (statut, corps) = json_de(enregistrer_image_artiste(
            &repo,
            id,
            b"<!doctype html><title>page Discogs</title>",
            cache.path(),
        ))
        .await;
        assert_eq!(statut, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(corps["error"], "unsupported image format");
        assert_eq!(
            repo.get(id).unwrap().unwrap().image_path,
            None,
            "un refus ne doit rien indexer"
        );
    }

    /// TEMOIN : trop gros echoue en le disant, avec la borne dans le corps.
    #[tokio::test]
    async fn une_image_trop_grosse_est_refusee_en_le_disant() {
        let cache = tune_core::test_scratch::scratch_dir("tune-3102-taille");
        let repo = ArtistRepo::with_backend(base_memoire());
        let id = artiste(&repo, "Quarterflash");

        let mut enorme = png_1x1();
        enorme.resize(IMAGE_ARTISTE_MAX_OCTETS + 1, 0);
        let (statut, corps) =
            json_de(enregistrer_image_artiste(&repo, id, &enorme, cache.path())).await;
        assert_eq!(statut, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(corps["max_size"], IMAGE_ARTISTE_MAX_OCTETS);
        assert_eq!(repo.get(id).unwrap().unwrap().image_path, None);
        assert_eq!(
            std::fs::read_dir(cache.path())
                .into_iter()
                .flatten()
                .count(),
            0,
            "rien ne doit etre ecrit dans le cache"
        );
    }

    /// TEMOIN : un artiste qui n'existe pas reste un 404, et le cache reste
    /// vide — le refus precede toute ecriture.
    #[tokio::test]
    async fn un_artiste_inconnu_reste_un_404() {
        let cache = tune_core::test_scratch::scratch_dir("tune-3102-inconnu");
        let repo = ArtistRepo::with_backend(base_memoire());
        let (statut, corps) = json_de(enregistrer_image_artiste(
            &repo,
            4242,
            &png_1x1(),
            cache.path(),
        ))
        .await;
        assert_eq!(statut, StatusCode::NOT_FOUND);
        assert_eq!(corps["error"], "artist not found");
        assert_eq!(
            std::fs::read_dir(cache.path())
                .into_iter()
                .flatten()
                .count(),
            0
        );
    }

    /// Le contrat de compatibilite : les trois cles que la reponse portait
    /// avant #3102 sont toujours la, a cote de l'artiste.
    #[tokio::test]
    async fn les_trois_cles_historiques_restent_dans_la_reponse() {
        let cache = tune_core::test_scratch::scratch_dir("tune-3102-cles");
        let repo = ArtistRepo::with_backend(base_memoire());
        let id = artiste(&repo, "Jermaine Jackson");
        let octets = png_1x1();

        let (_, corps) = json_de(enregistrer_image_artiste(&repo, id, &octets, cache.path())).await;
        assert_eq!(corps["artist_id"], id);
        assert_eq!(corps["size"], octets.len());
        assert_eq!(corps["hash"], corps["image_path"]);
        assert_eq!(corps["name"], "Jermaine Jackson");
    }
}
