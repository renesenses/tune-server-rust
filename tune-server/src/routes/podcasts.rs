use crate::error::AppError;
use crate::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};
use tune_core::playback::NowPlaying;
use tune_core::streaming::podcasts::PodcastService;
use tune_core::streaming::radiofrance::{RadioFranceApi, RfStation};
use tune_core::streaming::vignette_podcast;
#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default = "default_country")]
    country: String,
    language: Option<String>,
}
fn default_limit() -> usize {
    20
}
fn default_country() -> String {
    "us".into()
}
#[derive(Deserialize)]
struct TopQuery {
    genre: Option<u32>,
    #[serde(default = "default_country")]
    country: String,
}
/// `GET /discover` (#3395). Même paramètre et même valeur par défaut que
/// [`TopQuery`] : les deux routes servent le même palmarès, elles ne peuvent
/// pas diverger sur le pays.
#[derive(Deserialize)]
struct DiscoverQuery {
    #[serde(default = "default_country")]
    country: String,
}
#[derive(Deserialize)]
struct EpisodesQuery {
    feed_url: Option<String>,
    /// Apple top-chart id ("apple-{trackId}"). Top-chart podcasts carry no feed
    /// URL, so this lets episodes be previewed by resolving the feed URL from the
    /// id — without subscribing first (Bilou, #1000).
    source_id: Option<String>,
    #[serde(default = "default_episode_limit")]
    limit: usize,
}
fn default_episode_limit() -> usize {
    50
}
#[derive(Deserialize)]
struct Subscribe {
    #[serde(default)]
    feed_url: String,
    title: String,
    author: Option<String>,
    image_url: Option<String>,
    description: Option<String>,
    source_id: Option<String>,
}
#[derive(Deserialize)]
struct PlayEpisodeRequest {
    audio_url: String,
    title: Option<String>,
    podcast_name: Option<String>,
    cover_url: Option<String>,
    duration_ms: Option<u64>,
}
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/search", get(search_podcasts))
        .route("/subscriptions", get(list_subscriptions).post(subscribe))
        .route("/subscriptions/{id}", axum::routing::delete(unsubscribe))
        .route("/radiofrance", get(radiofrance_podcasts))
        .route("/radiofrance/shows", get(rf_shows))
        .route("/radiofrance/shows/search", get(rf_search_shows))
        .route("/radiofrance/episodes", get(rf_episodes))
        .route("/discover", get(discover_podcasts))
        .route("/top", get(top_podcasts))
        .route("/genres", get(list_genres))
        .route("/episodes/{podcast_id}", get(podcast_episodes))
        .route("/episodes", get(episodes_by_feed_url))
        .route("/play/{zone_id}", post(play_episode))
}
async fn search_podcasts(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Value>, AppError> {
    let svc = PodcastService::with_client(state.http_client.clone());
    match svc
        .search(&q.q, q.limit, &q.country, q.language.as_deref())
        .await
    {
        Ok(results) => Ok(Json(
            json!({"query": q.q, "count": results.len(), "items": results}),
        )),
        Err(e) => {
            warn!(query = %q.q, error = %e, "podcast_search_failed");
            Err(AppError::internal(e))
        }
    }
}
async fn list_subscriptions(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let rows = state.backend.query_many("SELECT id, feed_url, title, author, image_url, description, source_id FROM podcast_subscriptions ORDER BY title", &[]).map_err(AppError::internal)?;
    let cache = crate::routes::library::artwork_cache_dir();
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            let image_url = r.get(4).and_then(|v| v.as_string());
            // #5214 — la vignette mise en cache, servie par la route des
            // pochettes locales. Le client lit `cover_url` AVANT `image_url` :
            // absente (pas encore en cache, téléchargement échoué), il retombe
            // sur l'URL distante, comme avant.
            let cover_url = image_url
                .as_deref()
                .and_then(|u| vignette_podcast::en_cache(&cache, u));
            json!({
                "id": r.first().and_then(|v| v.as_i64()),
                "feed_url": r.get(1).and_then(|v| v.as_string()),
                "title": r.get(2).and_then(|v| v.as_string()),
                "author": r.get(3).and_then(|v| v.as_string()),
                "image_url": image_url,
                "cover_url": cover_url,
                "description": r.get(5).and_then(|v| v.as_string()),
                "source_id": r.get(6).and_then(|v| v.as_string()),
            })
        })
        .collect();
    Ok(Json(json!(items)))
}
async fn subscribe(
    State(state): State<AppState>,
    Json(body): Json<Subscribe>,
) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;

    let feed_url = if body.feed_url.is_empty() {
        // Top chart podcasts have no feed URL — resolve via iTunes lookup.
        let apple_id = body
            .source_id
            .as_deref()
            .and_then(|s| s.strip_prefix("apple-"))
            .unwrap_or("");
        if apple_id.is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "No feed URL and no Apple ID to resolve"})),
            )
                .into_response();
        }
        let svc = PodcastService::with_client(state.http_client.clone());
        match svc.resolve_feed_url(apple_id).await {
            Some(url) => url,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "Could not resolve feed URL from Apple ID"})),
                )
                    .into_response();
            }
        }
    } else {
        body.feed_url.clone()
    };

    let sql = if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
        "INSERT INTO podcast_subscriptions (feed_url, title, author, image_url, description, source_id) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (feed_url) DO NOTHING"
    } else {
        "INSERT OR IGNORE INTO podcast_subscriptions (feed_url, title, author, image_url, description, source_id) VALUES (?, ?, ?, ?, ?, ?)"
    };
    match state.backend.execute(
        sql,
        &[
            &feed_url as &dyn ToSqlValue,
            &body.title as &dyn ToSqlValue,
            &body.author as &dyn ToSqlValue,
            &body.image_url as &dyn ToSqlValue,
            &body.description as &dyn ToSqlValue,
            &body.source_id as &dyn ToSqlValue,
        ],
    ) {
        // `execute` rend le nombre de lignes RÉELLEMENT écrites, sur les deux
        // moteurs : `INSERT OR IGNORE` sur SQLite et `ON CONFLICT (feed_url)
        // DO NOTHING` sur PostgreSQL rendent tous deux 0 quand la ligne était
        // déjà là (`rows_affected()` côté PG, cf. #3248). C'est le seul signal
        // qui sépare les deux cas, et il était jeté ici (#3542).
        Ok(lignes_ecrites) => {
            let creation = lignes_ecrites > 0;
            // L'identifiant est relu par le flux, jamais par `last_insert_rowid`
            // : sur un abonnement DÉJÀ existant aucune ligne n'a été écrite, et
            // sur PostgreSQL un `ON CONFLICT` ne renseigne rien. Une seule
            // requête couvre donc honnêtement les deux cas. Le `?` est traduit
            // en `$1` par le dos PostgreSQL, comme partout ailleurs sur ce
            // chemin (cf. `unsubscribe`).
            let id = state
                .backend
                .query_one(
                    "SELECT id FROM podcast_subscriptions WHERE feed_url = ?",
                    &[&feed_url as &dyn ToSqlValue],
                )
                .ok()
                .flatten()
                .and_then(|r| r.first().and_then(|v| v.as_i64()));
            info!(
                title = %body.title,
                feed_url = %feed_url,
                creation,
                id,
                "podcast_subscribed"
            );
            // #5214 — la vignette est mise en cache MAINTENANT, bornée dans le
            // temps : un hébergeur lent ne retient pas l'abonnement. Un échec
            // ne fait pas échouer l'abonnement ; le rattrapage du démarrage et
            // le prochain rafraîchissement du flux réessaient.
            let cover_url = match body.image_url.as_deref().filter(|u| !u.trim().is_empty()) {
                Some(image) => tokio::time::timeout(
                    DELAI_VIGNETTE_ABONNEMENT,
                    mettre_vignette_en_cache(&state, image),
                )
                .await
                .ok()
                .flatten(),
                None => None,
            };
            // 201 pour une création, 200 pour un abonnement déjà présent : un
            // client qui reçoit 201 sait qu'il vient d'ajouter quelque chose et
            // peut le dire, là où le 201 systématique d'avant lui faisait
            // annoncer un ajout à chaque nouveau clic sur le même podcast.
            let code = if creation {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            (
                code,
                Json(json!({
                    "id": id,
                    "created": creation,
                    "title": body.title,
                    "feed_url": feed_url,
                    "cover_url": cover_url,
                })),
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}
async fn unsubscribe(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;
    state
        .backend
        .execute(
            "DELETE FROM podcast_subscriptions WHERE id = ?",
            &[&id as &dyn ToSqlValue],
        )
        .ok();
    StatusCode::NO_CONTENT
}
async fn radiofrance_podcasts() -> Json<Value> {
    Json(json!(PodcastService::curated_french_podcasts()))
}
/// `GET /discover?country={cc}` — la sélection éditoriale et le palmarès
/// Apple du pays demandé.
///
/// #3395 — le handler ne prenait AUCUN `Query` : `?country=` n'était pas mal
/// utilisé, il n'était jamais lu. Le palmarès partait sur « us » écrit en dur
/// et la sélection était la liste française quel que soit le pays, ce qui
/// rendait la réponse octet pour octet identique entre `fr` et `us` — le
/// sélecteur de pays de l'écran Podcasts paraissait mort.
///
/// Les deux sections ne se comportent pas pareil, et c'est assumé :
///
/// - `top` vient d'un vrai fournisseur (Apple), qui filtre par pays lui-même
///   — le pays est un segment de SON chemin, le serveur ne fait que le lui
///   transmettre ;
/// - `curated` est une liste écrite en dur, et il n'en existe QUE pour la
///   France. Elle n'est donc rendue que pour `fr`, et la réponse annonce
///   `curated_country` pour que le client sache à quoi s'en tenir. Aucun
///   filtre n'est simulé côté serveur : hors de France, il n'y a rien à
///   filtrer.
async fn discover_podcasts(
    State(state): State<AppState>,
    Query(q): Query<DiscoverQuery>,
) -> Result<Json<Value>, AppError> {
    let curated = PodcastService::curated_for_country(&q.country);
    let svc = PodcastService::with_client(state.http_client.clone());
    let top = svc.top_podcasts(None, &q.country).await.unwrap_or_default();
    Ok(Json(json!({
        "country": q.country,
        "curated": curated,
        // Le pays — le seul — pour lequel une sélection éditoriale existe. Un
        // client hors de ce pays sait ainsi que `curated` est vide par
        // construction, et non parce que la requête a échoué.
        "curated_country": PodcastService::CURATED_COUNTRY,
        "top": top,
        "genres": PodcastService::available_genres(),
    })))
}
/// `GET /top?genre={genreId}&country={cc}` — le Top 50 Apple du pays demandé,
/// éventuellement filtré par genre.
///
/// Le pays est paramétré depuis #3207 ; le commentaire, lui, annonçait encore
/// « in France » (#3395).
async fn top_podcasts(
    State(state): State<AppState>,
    Query(q): Query<TopQuery>,
) -> Result<Json<Value>, AppError> {
    let svc = PodcastService::with_client(state.http_client.clone());
    match svc.top_podcasts(q.genre, &q.country).await {
        Ok(podcasts) => Ok(Json(json!({
            "genre": q.genre,
            "count": podcasts.len(),
            "items": podcasts,
        }))),
        Err(e) => {
            warn!(genre = ?q.genre, error = %e, "top_podcasts_failed");
            Err(AppError::internal(e))
        }
    }
}
/// GET /genres — list all available genre filters for the top endpoint.
async fn list_genres() -> Json<Value> {
    Json(json!(PodcastService::available_genres()))
}
async fn episodes_by_feed_url(
    State(state): State<AppState>,
    Query(q): Query<EpisodesQuery>,
) -> Result<Json<Value>, AppError> {
    let svc = PodcastService::with_client(state.http_client.clone());
    // Use the feed URL directly, or resolve it from an Apple top-chart id
    // ("apple-{trackId}") — top-chart podcasts have no feed URL, so this lets
    // their episodes be previewed without subscribing first (Bilou, #1000).
    let feed_url = match q.feed_url.filter(|u| !u.is_empty()) {
        Some(u) => u,
        None => {
            let apple_id = q
                .source_id
                .as_deref()
                .and_then(|s| s.strip_prefix("apple-"))
                .unwrap_or("");
            if apple_id.is_empty() {
                return Err(AppError::bad_request(
                    "feed_url or source_id (apple-…) query parameter is required",
                ));
            }
            match svc.resolve_feed_url(apple_id).await {
                Some(u) => u,
                None => {
                    return Err(AppError::bad_request(
                        "could not resolve feed URL from Apple id",
                    ));
                }
            }
        }
    };
    match svc.get_feed(&feed_url, q.limit).await {
        Ok(flux) => {
            suivre_image_du_flux(&state, &feed_url, &flux.image_url);
            let episodes = flux.episodes;
            Ok(Json(
                json!({"feed_url": feed_url, "count": episodes.len(), "episodes": episodes}),
            ))
        }
        Err(e) => {
            warn!(feed_url = %feed_url, error = %e, "podcast_episodes_fetch_failed");
            Err(AppError::internal(e))
        }
    }
}
async fn podcast_episodes(
    State(state): State<AppState>,
    Path(podcast_id): Path<String>,
    Query(q): Query<EpisodesQuery>,
) -> Result<impl IntoResponse, AppError> {
    if let Some(ref feed_url) = q.feed_url {
        let svc = PodcastService::with_client(state.http_client.clone());
        return match svc.get_feed(feed_url, q.limit).await {
            Ok(flux) => {
                suivre_image_du_flux(&state, feed_url, &flux.image_url);
                let episodes = flux.episodes;
                Ok(Json(json!({"podcast_id": podcast_id, "feed_url": feed_url, "count": episodes.len(), "episodes": episodes})).into_response())
            }
            Err(e) => Ok(Json(json!({"podcast_id": podcast_id, "error": e})).into_response()),
        };
    }
    let feed_url = {
        use tune_core::db::backend::ToSqlValue;
        if let Ok(id) = podcast_id.parse::<i64>() {
            state
                .backend
                .query_one(
                    "SELECT feed_url FROM podcast_subscriptions WHERE id = ?",
                    &[&id as &dyn ToSqlValue],
                )
                .ok()
                .flatten()
                .and_then(|r| r.first().and_then(|v| v.as_string()))
        } else {
            let like = format!("%{}%", podcast_id.replace('-', " "));
            state
                .backend
                .query_one(
                    "SELECT feed_url FROM podcast_subscriptions WHERE title LIKE ?",
                    &[&like as &dyn ToSqlValue],
                )
                .ok()
                .flatten()
                .and_then(|r| r.first().and_then(|v| v.as_string()))
        }
    };
    let Some(feed_url) = feed_url else {
        return Ok(Json(json!({"podcast_id": podcast_id, "episodes": [], "error": "podcast not found in subscriptions"})).into_response());
    };
    let svc = PodcastService::with_client(state.http_client.clone());
    match svc.get_feed(&feed_url, q.limit).await {
        Ok(flux) => {
            suivre_image_du_flux(&state, &feed_url, &flux.image_url);
            let episodes = flux.episodes;
            let count = episodes.len();
            Ok(Json(json!({"podcast_id": podcast_id, "feed_url": feed_url, "count": count, "episodes": episodes})).into_response())
        }
        Err(e) => Ok(
            Json(json!({"podcast_id": podcast_id, "feed_url": feed_url, "error": e}))
                .into_response(),
        ),
    }
}
async fn play_episode(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<PlayEpisodeRequest>,
) -> impl IntoResponse {
    let title = body.title.as_deref().unwrap_or("Podcast Episode");
    let podcast_name = body.podcast_name.as_deref().unwrap_or("Podcast");
    let device_id = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten()
        .and_then(|z| z.output_device_id);
    let mime_type = guess_audio_mime(&body.audio_url);
    // #5214 — la pochette « en cours », que Lecture en cours et l'Historique
    // du client reprennent : la vignette en cache quand elle existe. La sortie
    // (`PlayMedia` plus bas) garde l'URL distante, qu'un appareil de rendu va
    // chercher lui-même.
    let pochette = pochette_de_lecture(
        &state,
        body.cover_url.as_deref(),
        body.podcast_name.as_deref(),
    );
    let np = NowPlaying {
        track_id: None,
        title: title.to_string(),
        artist_name: Some(podcast_name.to_string()),
        album_title: Some(podcast_name.to_string()),
        cover_path: pochette,
        duration_ms: body.duration_ms.unwrap_or(0) as i64,
        source: "podcast".into(),
        source_id: Some(body.audio_url.clone()),
        stream_id: None,
        ..Default::default()
    };
    state.playback.play(zone_id, np).await;
    let (output_sent, output_error) = if let Some(ref did) = device_id {
        let output_arc = {
            let outputs = state.outputs.lock().await;
            outputs.get(did)
        };
        if let Some(output_arc) = output_arc {
            let output = output_arc.lock().await;
            let media = tune_core::outputs::PlayMedia {
                url: &body.audio_url,
                mime_type,
                title: Some(title),
                artist: Some(podcast_name),
                album: Some(podcast_name),
                cover_url: body.cover_url.as_deref(),
                duration_ms: body.duration_ms,
                ..Default::default()
            };
            match output.play_media(&media).await {
                Ok(()) => (true, None),
                Err(e) => (false, Some(format!("Output device error: {e}"))),
            }
        } else {
            (
                false,
                Some("Device not yet discovered. Please retry in a few seconds.".into()),
            )
        }
    } else {
        (false, None)
    };
    info!(
        zone_id,
        title,
        podcast = podcast_name,
        output_sent,
        "podcast_episode_play"
    );
    let zone_state = state.playback.get_state(zone_id).await;
    Json(json!({"zone_id": zone_id, "title": title, "podcast": podcast_name, "audio_url": body.audio_url, "mime_type": mime_type, "output_sent": output_sent, "error": output_error, "state": zone_state})).into_response()
}
// ─── Vignette mise en cache (#5214) ─────────────────────────────────

/// Délai accordé à la mise en cache de la vignette pendant l'abonnement.
const DELAI_VIGNETTE_ABONNEMENT: std::time::Duration = std::time::Duration::from_secs(8);

/// Met en cache la vignette `url` (voir `tune_core::streaming::vignette_podcast`)
/// et rend son adresse locale, ou `None` en le journalisant.
pub(crate) async fn mettre_vignette_en_cache(state: &AppState, url: &str) -> Option<String> {
    let cache = crate::routes::library::artwork_cache_dir();
    match vignette_podcast::mettre_en_cache(
        &state.relais_pochettes,
        &cache,
        url,
        vignette_podcast::TAILLE_MAX,
    )
    .await
    {
        Ok(adresse) => {
            info!(url, adresse = %adresse, "podcast_vignette_en_cache");
            Some(adresse)
        }
        Err(e) => {
            warn!(url, erreur = %e, "podcast_vignette_non_mise_en_cache");
            None
        }
    }
}

/// La pochette à poser sur l'épisode en lecture.
///
/// 1. l'image envoyée par le client a sa copie en cache → la copie ;
/// 2. elle est déjà locale (condensat, chemin `/api/…`) → telle quelle ;
/// 3. elle est distante sur un hôte que le relais admet → telle quelle : elle
///    s'affiche déjà, et c'est peut-être l'image propre de l'épisode ;
/// 4. sinon, la vignette en cache de l'abonnement de ce podcast (même titre) ;
/// 5. sinon, l'image envoyée, comme avant.
fn pochette_de_lecture(
    state: &AppState,
    cover_url: Option<&str>,
    podcast_name: Option<&str>,
) -> Option<String> {
    use tune_core::db::backend::ToSqlValue;
    use tune_core::library::artwork_proxy::{hote_autorise, hotes_supplementaires};
    let cache = crate::routes::library::artwork_cache_dir();
    let envoyee = cover_url.map(str::trim).filter(|c| !c.is_empty());
    if let Some(c) = envoyee {
        if let Some(adresse) = vignette_podcast::en_cache(&cache, c) {
            return Some(adresse);
        }
        if !(c.starts_with("http://") || c.starts_with("https://")) {
            return Some(c.to_string());
        }
        let admise = reqwest::Url::parse(c)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .is_some_and(|h| hote_autorise(&h, &hotes_supplementaires(&state.backend)));
        if admise {
            return Some(c.to_string());
        }
    }
    let de_l_abonnement = podcast_name.and_then(|nom| {
        let nom = nom.to_string();
        state
            .backend
            .query_many(
                "SELECT image_url FROM podcast_subscriptions WHERE title = ?",
                &[&nom as &dyn ToSqlValue],
            )
            .ok()?
            .into_iter()
            .filter_map(|r| r.first().and_then(|v| v.as_string()))
            .find_map(|image| vignette_podcast::en_cache(&cache, &image))
    });
    de_l_abonnement.or_else(|| envoyee.map(str::to_string))
}

/// Après lecture d'un flux : si c'est celui d'un abonnement, retient l'image
/// que le flux déclare maintenant et la met en cache si elle n'y est pas —
/// c'est le rattrapage « au prochain rafraîchissement » des abonnements
/// antérieurs à #5214, et le suivi d'un flux qui change d'image. Le
/// téléchargement part en tâche de fond : la liste des épisodes n'attend pas.
fn suivre_image_du_flux(state: &AppState, feed_url: &str, image: &str) {
    use tune_core::db::backend::ToSqlValue;
    let image = image.trim();
    if image.is_empty() {
        return;
    }
    let feed = feed_url.to_string();
    let Some(ligne) = state
        .backend
        .query_one(
            "SELECT image_url FROM podcast_subscriptions WHERE feed_url = ?",
            &[&feed as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
    else {
        return;
    };
    let stockee = ligne.first().and_then(|v| v.as_string());
    if stockee.as_deref() != Some(image) {
        let neuve = image.to_string();
        match state.backend.execute(
            "UPDATE podcast_subscriptions SET image_url = ? WHERE feed_url = ?",
            &[&neuve as &dyn ToSqlValue, &feed as &dyn ToSqlValue],
        ) {
            Ok(_) => info!(feed_url, ancienne = ?stockee, image, "podcast_image_du_flux_changee"),
            Err(e) => warn!(feed_url, erreur = %e, "podcast_image_du_flux_non_enregistree"),
        }
    }
    let cache = crate::routes::library::artwork_cache_dir();
    if vignette_podcast::en_cache(&cache, image).is_none() {
        let state = state.clone();
        let image = image.to_string();
        tokio::spawn(async move {
            mettre_vignette_en_cache(&state, &image).await;
        });
    }
}

/// Le rattrapage du démarrage : met en cache la vignette de chaque abonnement
/// qui n'en a pas encore. Rend (mises en cache, échecs).
pub async fn rattraper_vignettes(state: &AppState) -> (usize, usize) {
    let lignes = state
        .backend
        .query_many(
            "SELECT image_url FROM podcast_subscriptions WHERE image_url IS NOT NULL",
            &[],
        )
        .unwrap_or_default();
    let cache = crate::routes::library::artwork_cache_dir();
    let (mut faites, mut echecs) = (0, 0);
    for image in lignes
        .into_iter()
        .filter_map(|r| r.first().and_then(|v| v.as_string()))
    {
        let image = image.trim().to_string();
        if !(image.starts_with("http://") || image.starts_with("https://"))
            || vignette_podcast::en_cache(&cache, &image).is_some()
        {
            continue;
        }
        match mettre_vignette_en_cache(state, &image).await {
            Some(_) => faites += 1,
            None => echecs += 1,
        }
    }
    (faites, echecs)
}

// ─── Radio France GraphQL API ───────────────────────────────────────

#[derive(Deserialize)]
struct RfShowsQuery {
    station: Option<String>,
}

#[derive(Deserialize)]
struct RfSearchQuery {
    q: String,
}

#[derive(Deserialize)]
struct RfEpisodesQuery {
    show_url: String,
    #[serde(default = "default_episode_limit")]
    limit: usize,
}

fn get_rf_api_key(state: &AppState) -> Option<String> {
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .get("radiofrance_api_key")
        .ok()
        .flatten()
        .filter(|k| !k.is_empty())
}

/// L'absence de clé n'est pas une requête fautive : c'est un état de
/// configuration du serveur. Répondre `400 bad_request` faisait passer
/// l'ouverture de l'écran Podcasts pour une erreur (#1026) et ne laissait au
/// client qu'un message technique anglais citant un nom de variable.
///
/// Ici : `412 Precondition Failed` (même statut que Discogs et setlist.fm
/// pour leurs clés absentes), un code stable pour qui programme contre
/// l'API, le nom exact du réglage qui active la source, et un message dans
/// la langue de l'interface. Les clients déployés, qui ne testent que
/// `res.ok`, retombent comme avant sur la liste éditoriale sans clé.
fn rf_cle_absente(headers: &HeaderMap) -> axum::response::Response {
    let lang = crate::i18n::lang_from_header(headers);
    (
        StatusCode::PRECONDITION_FAILED,
        Json(json!({
            "error": "radiofrance_cle_absente",
            "message": crate::i18n::t(&lang, "podcasts.radiofrance.cleAbsente"),
            "setting": "radiofrance_api_key",
        })),
    )
        .into_response()
}

async fn rf_shows(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<RfShowsQuery>,
) -> Result<axum::response::Response, AppError> {
    let Some(api_key) = get_rf_api_key(&state) else {
        return Ok(rf_cle_absente(&headers));
    };
    let api = RadioFranceApi::with_client(state.http_client.clone(), api_key);
    let code = q.station.as_deref().unwrap_or("FRANCEINTER");
    let station = RfStation::from_code(code)
        .ok_or_else(|| AppError::bad_request(format!("unknown station: {code}")))?;
    match api.list_shows(station).await {
        Ok(shows) => Ok(Json(
            json!({"station": station.label(), "count": shows.len(), "shows": shows}),
        )
        .into_response()),
        Err(e) => {
            warn!(station = code, error = %e, "rf_shows_failed");
            Err(AppError::internal(e))
        }
    }
}

async fn rf_search_shows(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<RfSearchQuery>,
) -> Result<axum::response::Response, AppError> {
    let Some(api_key) = get_rf_api_key(&state) else {
        return Ok(rf_cle_absente(&headers));
    };
    let api = RadioFranceApi::with_client(state.http_client.clone(), api_key);
    match api.search_shows(&q.q).await {
        Ok(shows) => {
            Ok(Json(json!({"query": q.q, "count": shows.len(), "shows": shows})).into_response())
        }
        Err(e) => {
            warn!(query = %q.q, error = %e, "rf_search_failed");
            Err(AppError::internal(e))
        }
    }
}

async fn rf_episodes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<RfEpisodesQuery>,
) -> Result<axum::response::Response, AppError> {
    let Some(api_key) = get_rf_api_key(&state) else {
        return Ok(rf_cle_absente(&headers));
    };
    let api = RadioFranceApi::with_client(state.http_client.clone(), api_key);
    match api.get_episodes(&q.show_url, q.limit as u32).await {
        Ok(episodes) => Ok(Json(
            json!({"show_url": q.show_url, "count": episodes.len(), "episodes": episodes}),
        )
        .into_response()),
        Err(e) => {
            warn!(show = %q.show_url, error = %e, "rf_episodes_failed");
            Err(AppError::internal(e))
        }
    }
}

fn guess_audio_mime(url: &str) -> &'static str {
    let lower = url.to_lowercase();
    let path = lower.split('?').next().unwrap_or(&lower);
    if path.ends_with(".mp3") {
        "audio/mpeg"
    } else if path.ends_with(".m4a") || path.ends_with(".aac") || path.ends_with(".mp4") {
        "audio/mp4"
    } else if path.ends_with(".ogg") || path.ends_with(".opus") {
        "audio/ogg"
    } else if path.ends_with(".flac") {
        "audio/flac"
    } else if path.ends_with(".wav") {
        "audio/wav"
    } else {
        "audio/mpeg"
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_guess_audio_mime() {
        assert_eq!(guess_audio_mime("https://x.com/ep.mp3"), "audio/mpeg");
        assert_eq!(guess_audio_mime("https://x.com/ep.m4a?t=1"), "audio/mp4");
        assert_eq!(guess_audio_mime("https://x.com/stream"), "audio/mpeg");
    }

    /// #3395 — `/discover` LIT le pays, et le lit comme `/top`.
    ///
    /// Le défaut n'était pas une mauvaise lecture du paramètre : le handler
    /// n'en prenait aucun. Le témoin épingle la lecture elle-même (le
    /// `Query`), pays absent comme pays fourni, et exige la même valeur par
    /// défaut que `/top` — c'est leur divergence qui rendrait à nouveau une
    /// section sourde au sélecteur.
    #[test]
    fn discover_lit_le_pays_comme_top_3395() {
        // L'extracteur réellement employé par la route, sur une vraie chaîne
        // de requête.
        let lu = |q: &str| -> DiscoverQuery {
            let uri: axum::http::Uri = format!("/podcasts/discover?{q}").parse().unwrap();
            Query::<DiscoverQuery>::try_from_uri(&uri)
                .expect("la chaîne de requête doit se désérialiser")
                .0
        };
        assert_eq!(lu("").country, default_country(), "défaut de /discover");
        assert_eq!(lu("country=fr").country, "fr");
        assert_eq!(lu("country=us").country, "us");
        let uri: axum::http::Uri = "/podcasts/top".parse().unwrap();
        let top = Query::<TopQuery>::try_from_uri(&uri).unwrap().0;
        assert_eq!(
            lu("").country,
            top.country,
            "/discover et /top doivent partager la valeur par défaut"
        );
    }

    /// #3395 — la sélection éditoriale n'est servie que sous son drapeau.
    ///
    /// La route n'invente aucun filtre : elle rend ce que le fournisseur —
    /// ici une liste écrite en dur, française — sait couvrir, et rien
    /// ailleurs.
    #[test]
    fn discover_ne_sert_la_selection_que_pour_son_pays_3395() {
        assert!(!PodcastService::curated_for_country("fr").is_empty());
        assert!(PodcastService::curated_for_country("us").is_empty());
        assert_eq!(PodcastService::CURATED_COUNTRY, "fr");
    }
}
