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
use tune_core::streaming::podcast_annuaires::RelaisPodcastIndex;
use tune_core::streaming::podcasts::{EchecResolution, PodcastService};
use tune_core::streaming::radiofrance::{RadioFranceApi, RfStation};
use tune_core::streaming::vignette_podcast;
#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    #[serde(default = "default_limit")]
    limit: usize,
    /// #7152 — plus de défaut `us` : sans pays, celui de l'utilisateur ou du
    /// serveur ([`pays_de_recherche`]).
    country: Option<String>,
    language: Option<String>,
}

/// #7152 — réglage serveur du pays de la recherche de podcasts
/// (`PATCH /system/config`). Vide ou absent : automatique.
pub const CLE_PAYS_PODCASTS: &str = "podcasts_country";

/// D'où vient le pays de la recherche (#7152), rendu dans la réponse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginePaysPodcasts {
    /// `?country=` : le sélecteur de pays du client web (initialisé à la
    /// région du navigateur), ou le pays que l'application envoie.
    Requete,
    /// Le réglage [`CLE_PAYS_PODCASTS`].
    Reglage,
    /// La région de la langue de l'interface (`Accept-Language`).
    Langue,
    /// La locale du système du serveur (`LC_ALL`, `LC_MESSAGES`, `LANG`).
    Systeme,
    /// Rien de tout cela : la boutique américaine, comme avant.
    ParDefaut,
}

impl OriginePaysPodcasts {
    pub fn cle(self) -> &'static str {
        match self {
            Self::Requete => "request",
            Self::Reglage => "setting",
            Self::Langue => "language",
            Self::Systeme => "system",
            Self::ParDefaut => "default",
        }
    }
}

/// Un code pays à deux lettres, en minuscules, ou `None`.
fn code_pays(brut: &str) -> Option<String> {
    let c = brut.trim();
    (c.len() == 2 && c.bytes().all(|b| b.is_ascii_alphabetic())).then(|| c.to_ascii_lowercase())
}

/// Le pays par défaut d'une langue SANS région (`fr` → `fr`, `sv` → `se`,
/// `en` → `us`…). Couvre les langues de l'interface ; inconnue : `None`.
fn pays_de_la_langue(langue: &str) -> Option<&'static str> {
    Some(match langue {
        "fr" => "fr",
        "en" => "us",
        "de" => "de",
        "es" | "ca" => "es",
        "it" => "it",
        "nl" => "nl",
        "pt" => "pt",
        "pl" => "pl",
        "sv" => "se",
        "nb" | "no" | "nn" => "no",
        "da" => "dk",
        "fi" => "fi",
        "cs" => "cz",
        "el" => "gr",
        "hu" => "hu",
        "ro" => "ro",
        "ru" => "ru",
        "uk" => "ua",
        "tr" => "tr",
        "ja" => "jp",
        "ko" => "kr",
        "zh" => "cn",
        "vi" => "vn",
        "ar" => "sa",
        "he" => "il",
        _ => return None,
    })
}

/// Le pays d'une étiquette de langue (`fr-FR`, `zh-Hant-TW`, `fr_FR.UTF-8`,
/// `de`) : sa région si elle en porte une à deux lettres, sinon le pays par
/// défaut de la langue. `C`, `POSIX` : `None`.
fn pays_d_une_etiquette(etiquette: &str) -> Option<String> {
    let sans_jeu = etiquette.split(['.', '@']).next().unwrap_or("").trim();
    let mut morceaux = sans_jeu.split(['-', '_']);
    let langue = morceaux.next()?.to_ascii_lowercase();
    if langue.is_empty() || langue == "c" || langue == "posix" {
        return None;
    }
    if let Some(region) = morceaux.find_map(code_pays) {
        return Some(region);
    }
    pays_de_la_langue(&langue).map(str::to_string)
}

/// Le pays de la recherche de podcasts (#7152) : `?country=` du client, sinon
/// le réglage du serveur, sinon la langue de l'interface (première langue de
/// `Accept-Language` par poids qui donne un pays), sinon la locale du
/// système, sinon `us`.
pub fn pays_de_recherche(
    requete: Option<&str>,
    reglage: Option<&str>,
    accept_language: Option<&str>,
    locale_systeme: Option<&str>,
) -> (String, OriginePaysPodcasts) {
    if let Some(p) = requete.and_then(code_pays) {
        return (p, OriginePaysPodcasts::Requete);
    }
    if let Some(p) = reglage.and_then(code_pays) {
        return (p, OriginePaysPodcasts::Reglage);
    }
    if let Some(entete) = accept_language {
        let mut langues: Vec<(f32, &str)> = entete
            .split(',')
            .filter_map(|part| {
                let mut m = part.split(';');
                let tag = m.next()?.trim();
                let q = m
                    .find_map(|p| p.trim().strip_prefix("q="))
                    .map_or(Some(1.0), |v| v.trim().parse::<f32>().ok())?;
                (q > 0.0 && !tag.is_empty() && tag != "*").then_some((q, tag))
            })
            .collect();
        langues.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        if let Some(p) = langues.iter().find_map(|(_, t)| pays_d_une_etiquette(t)) {
            return (p, OriginePaysPodcasts::Langue);
        }
    }
    if let Some(p) = locale_systeme.and_then(pays_d_une_etiquette) {
        return (p, OriginePaysPodcasts::Systeme);
    }
    ("us".into(), OriginePaysPodcasts::ParDefaut)
}

/// La locale du système : `LC_ALL`, puis `LC_MESSAGES`, puis `LANG`.
fn locale_du_systeme() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|v| !v.trim().is_empty())
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
    /// Pays du palmarès d'où vient `source_id` : la recherche du flux part
    /// dans sa boutique Apple (#6770).
    country: Option<String>,
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
    /// Pays du palmarès d'où vient `source_id` (#6770).
    country: Option<String>,
}

/// L'échec de résolution d'un podcast du palmarès, dit au client (#6770) :
/// un code stable et une phrase lisible, au lieu d'un 400 générique.
fn echec_de_resolution(e: &EchecResolution) -> AppError {
    let status = match e {
        EchecResolution::Introuvable => StatusCode::NOT_FOUND,
        EchecResolution::SansFluxPublic => StatusCode::UNPROCESSABLE_ENTITY,
        EchecResolution::Reseau(_) => StatusCode::BAD_GATEWAY,
    };
    AppError {
        status,
        message: e.message().to_string(),
        code: Some(e.code().to_string()),
    }
}
#[derive(Deserialize)]
pub(super) struct PlayEpisodeRequest {
    pub(super) audio_url: String,
    pub(super) title: Option<String>,
    pub(super) podcast_name: Option<String>,
    pub(super) podcast_id: Option<String>,
    pub(super) cover_url: Option<String>,
    pub(super) duration_ms: Option<u64>,
    #[serde(default)]
    pub(super) seek_ms: Option<u64>,
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
        // #7194 (web#2450) — le catalogue de la carte des podcasts.
        .route("/catalogue", get(catalogue_podcasts))
        .route("/episodes/{podcast_id}", get(podcast_episodes))
        .route("/episodes", get(episodes_by_feed_url))
        .route("/play/{zone_id}", post(play_episode))
}
async fn search_podcasts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Value>, AppError> {
    let reglage = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .get(CLE_PAYS_PODCASTS)
        .ok()
        .flatten();
    let accept_language = headers
        .get(axum::http::header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok());
    let (pays, origine) = pays_de_recherche(
        q.country.as_deref(),
        reglage.as_deref(),
        accept_language,
        locale_du_systeme().as_deref(),
    );
    // #7152 — Podcast Index, troisième annuaire, par le relais du site au nom
    // du compte relié ; sans compte, la source est sautée.
    let svc = PodcastService::with_client(state.http_client.clone())
        .avec_relais_podcast_index(RelaisPodcastIndex::du_compte(&state.backend));
    match svc
        .search(&q.q, q.limit, &pays, q.language.as_deref())
        .await
    {
        Ok(results) => Ok(Json(json!({
            "query": q.q,
            "country": pays,
            "country_source": origine.cle(),
            "count": results.len(),
            "items": results,
        }))),
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
        match svc
            .resolve_feed_url(apple_id, body.country.as_deref())
            .await
        {
            Ok(url) => url,
            Err(e) => return echec_de_resolution(&e).into_response(),
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
    let svc = PodcastService::with_client(state.http_client.clone())
        .avec_relais_podcast_index(RelaisPodcastIndex::du_compte(&state.backend));
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
    let svc = PodcastService::with_client(state.http_client.clone())
        .avec_relais_podcast_index(RelaisPodcastIndex::du_compte(&state.backend));
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
/// #7194 (web#2450) — `?countries=fr,us,…` ; absent : les dix-huit boutiques
/// de [`tune_core::streaming::podcast_catalogue::PAYS_PAR_DEFAUT`].
#[derive(Deserialize, Default)]
struct CatalogueQuery {
    countries: Option<String>,
}

/// Au plus tant de palmarès lus à la fois : chacun coûte un appel Apple, une
/// ou deux recherches groupées et ses résolutions de flux.
const PALMARES_EN_PARALLELE: usize = 4;

/// La réponse n'attend pas plus longtemps. Un palmarès plus lent n'est PAS
/// abandonné : sa tâche continue et remplit le cache d'une heure de
/// `top_podcasts`, si bien que l'appel suivant l'obtient tout de suite. La
/// réponse le dit (`pending_countries`) au lieu de le taire.
const DELAI_DU_CATALOGUE: std::time::Duration = std::time::Duration::from_secs(20);

/// `GET /podcasts/catalogue` — les palmarès de plusieurs pays et la sélection
/// éditoriale réunis, sans doublon, pour la carte des podcasts (web#2450).
///
/// Chaque émission porte son rang par pays (`charts`), son pays principal,
/// sa catégorie (`category`, `category_id`) et `readable` : Tune connaît son
/// flux. Aucun fournisseur nouveau : ce sont les palmarès de l'onglet
/// « Populaires », lus pour plusieurs pays.
async fn catalogue_podcasts(
    State(state): State<AppState>,
    Query(q): Query<CatalogueQuery>,
) -> Result<Json<Value>, AppError> {
    use tune_core::streaming::podcast_catalogue::{fusionner, pays_demandes};
    let pays = pays_demandes(q.countries.as_deref()).map_err(|message| AppError {
        status: StatusCode::BAD_REQUEST,
        message,
        code: Some("podcast_catalogue_pays_invalide".into()),
    })?;
    let relais = RelaisPodcastIndex::du_compte(&state.backend);
    let permis = std::sync::Arc::new(tokio::sync::Semaphore::new(PALMARES_EN_PARALLELE));
    let taches: Vec<_> = pays
        .iter()
        .map(|cc| {
            let svc = PodcastService::with_client(state.http_client.clone())
                .avec_relais_podcast_index(relais.clone());
            let permis = permis.clone();
            let cc = cc.clone();
            let tache = tokio::spawn({
                let cc = cc.clone();
                async move {
                    let _permis = permis.acquire_owned().await.ok();
                    svc.top_podcasts(None, &cc).await
                }
            });
            (cc, tache)
        })
        .collect();
    let limite = tokio::time::Instant::now() + DELAI_DU_CATALOGUE;
    let mut palmares = Vec::new();
    let mut en_attente: Vec<String> = Vec::new();
    let mut en_echec: Vec<String> = Vec::new();
    for (cc, mut tache) in taches {
        match tokio::time::timeout_at(limite, &mut tache).await {
            Ok(Ok(Ok(liste))) => palmares.push((cc, liste)),
            Ok(Ok(Err(e))) => {
                warn!(country = %cc, error = %e, "podcast_catalogue_palmares_en_echec");
                en_echec.push(cc);
            }
            Ok(Err(e)) => {
                warn!(country = %cc, error = %e, "podcast_catalogue_tache_interrompue");
                en_echec.push(cc);
            }
            // La tâche continue seule (un JoinHandle lâché ne l'annule pas).
            Err(_) => en_attente.push(cc),
        }
    }
    let selection: Vec<(String, Vec<_>)> = if pays
        .iter()
        .any(|c| c.eq_ignore_ascii_case(PodcastService::CURATED_COUNTRY))
    {
        vec![(
            PodcastService::CURATED_COUNTRY.to_string(),
            PodcastService::curated_french_podcasts(),
        )]
    } else {
        Vec::new()
    };
    let items = fusionner(&palmares, &selection);
    Ok(Json(json!({
        "countries": pays,
        "pending_countries": en_attente,
        "failed_countries": en_echec,
        "complete": en_attente.is_empty() && en_echec.is_empty(),
        "count": items.len(),
        "items": items,
        "genres": PodcastService::available_genres(),
    })))
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
            svc.resolve_feed_url(apple_id, q.country.as_deref())
                .await
                .map_err(|e| echec_de_resolution(&e))?
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
pub(super) async fn play_episode(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<PlayEpisodeRequest>,
) -> impl IntoResponse {
    let title = body.title.as_deref().unwrap_or("Podcast Episode");
    let podcast_name = body.podcast_name.as_deref().unwrap_or("Podcast");
    let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten();
    let navigateur = zone
        .as_ref()
        .is_some_and(|z| z.output_type.as_deref() == Some("browser"));
    let device_id = zone.and_then(|z| z.output_device_id);
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
    let output_arc = match device_id.as_deref() {
        Some(did) => {
            let outputs = state.outputs.lock().await;
            outputs.get(did)
        }
        None => None,
    };
    // #6468 — un renderer réseau (DLNA, OpenHome…) reçoit l'épisode par le
    // mandataire de Tune, en `http://`, comme les radios et les services :
    // beaucoup de renderers DLNA n'ouvrent pas le HTTPS d'un hébergeur de
    // podcasts, acceptent pourtant `SetAVTransportURI` et `Play`, puis
    // retombent en STOPPED. La sortie locale ouvre l'URL elle-même : elle la
    // reçoit inchangée.
    let relais = match output_arc.as_ref() {
        Some(arc) => {
            let output_type = arc.lock().await.output_type().to_string();
            if tune_core::orchestrator::is_network_output_type(Some(&output_type)) {
                let (url, session) = state
                    .orchestrator
                    .relayer_un_flux_distant(
                        &body.audio_url,
                        mime_type,
                        body.duration_ms.map(|d| d as i64),
                        None,
                        None,
                    )
                    .await;
                info!(zone_id, output_type, stream_id = %session, "podcast_episode_relaye");
                Some((url, session))
            } else {
                None
            }
        }
        None => None,
    };
    let url_de_sortie = relais
        .as_ref()
        .map(|(url, _)| url.as_str())
        .unwrap_or(&body.audio_url);
    let np = NowPlaying {
        track_id: None,
        title: title.to_string(),
        artist_name: Some(podcast_name.to_string()),
        album_title: Some(podcast_name.to_string()),
        cover_path: pochette,
        duration_ms: body.duration_ms.unwrap_or(0) as i64,
        source: "podcast".into(),
        source_id: Some(body.audio_url.clone()),
        stream_id: relais.as_ref().map(|(_, session)| session.clone()),
        ..Default::default()
    };
    // #6465 — un épisode n'appartient à aucun album ni playlist joué avant.
    state
        .playback
        .set_session_context(
            zone_id,
            Some("podcast".into()),
            body.podcast_id
                .clone()
                .or_else(|| body.podcast_name.clone()),
            Some("podcast".into()),
            body.podcast_name.clone(),
            np.cover_path.clone(),
        )
        .await;
    if navigateur {
        // Le chemin commun fournit une session Tune et attend la parole du
        // lecteur avant l'historique et la reprise. playback.play seul ne
        // créait ni flux confirmable, ni annonce de démarrage navigateur.
        let demande = tune_core::orchestrator::PlayRequest {
            zone_id,
            source: Some("podcast".into()),
            source_id: Some(body.audio_url.clone()),
            title: Some(title.into()),
            artist_name: Some(podcast_name.into()),
            album_title: Some(podcast_name.into()),
            cover_url: np.cover_path.clone(),
            duration_ms: Some(body.duration_ms.unwrap_or(0) as i64),
            ..Default::default()
        };
        let resultat = if body.seek_ms.is_some() {
            state.orchestrator.play_context_resume(demande).await
        } else {
            state.orchestrator.play(demande).await
        };
        let resultat = match resultat {
            Ok(r) if r.sortie_refusee().is_none() => r,
            Ok(r) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"error":r.error,"code":"podcast_sortie_refuse"})),
                );
            }
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"error":e,"code":"podcast_preparation_refusee"})),
                );
            }
        };
        if resultat.stream_url.is_some() {
            if let Some(ms) = body.seek_ms {
                // L'élément du navigateur applique cet offset au média complet.
                state
                    .playback
                    .seek(zone_id, ms.min(i64::MAX as u64) as i64)
                    .await;
            }
        }
        let zone_state = state.playback.get_state(zone_id).await;
        return (
            StatusCode::OK,
            Json(json!({
                "zone_id":zone_id,"title":title,"podcast":podcast_name,
                "audio_url":resultat.stream_url.clone().unwrap_or(body.audio_url.clone()),
                "stream_url":resultat.stream_url,"mime_type":mime_type,
                "output_sent":false,"error":null,"code":null,"state":zone_state
            })),
        );
    }
    state.playback.play(zone_id, np).await;
    // #6468 — un échec n'est plus un `200` : la réponse porte un vrai code
    // HTTP et un `code` stable, que le client peut afficher.
    let echec: Option<(StatusCode, &'static str, String)> = match (&device_id, &output_arc) {
        (Some(_), Some(output_arc)) => {
            let output = output_arc.lock().await;
            let media = tune_core::outputs::PlayMedia {
                url: url_de_sortie,
                mime_type,
                title: Some(title),
                artist: Some(podcast_name),
                album: Some(podcast_name),
                cover_url: body.cover_url.as_deref(),
                duration_ms: body.duration_ms,
                ..Default::default()
            };
            match output.play_media(&media).await {
                Ok(()) => None,
                Err(e) => Some((
                    StatusCode::BAD_GATEWAY,
                    "podcast_sortie_refuse",
                    format!("Output device error: {e}"),
                )),
            }
        }
        (Some(_), None) => Some((
            StatusCode::SERVICE_UNAVAILABLE,
            "podcast_sortie_introuvable",
            "Device not yet discovered. Please retry in a few seconds.".into(),
        )),
        (None, _) => None,
    };
    let output_sent = device_id.is_some() && echec.is_none();
    if let Some((_, code, ref erreur)) = echec {
        // La zone ne joue rien : ne pas la laisser « en lecture », sans quoi
        // le geste suivant est une Pause que le renderer refuse (701).
        state.playback.stop(zone_id).await;
        warn!(zone_id, title, code, error = %erreur, "podcast_episode_play_failed");
    }
    info!(
        zone_id,
        title,
        podcast = podcast_name,
        output_sent,
        "podcast_episode_play"
    );
    if echec.is_none() {
        if let Some(seek_ms) = body.seek_ms.filter(|v| *v > 0) {
            if let Err(e) = state
                .orchestrator
                .seek(zone_id, seek_ms, device_id.as_deref())
                .await
            {
                state.playback.stop(zone_id).await;
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"error": e.to_string(), "code": "podcast_resume_seek_failed"})),
                );
            }
        }
        if output_sent {
            let s = state.playback.get_state(zone_id).await;
            if let Some(np) = s.now_playing.as_ref() {
                if let Err(e) = tune_core::db::listening_resume::podcast_started_in_context(
                    &state.backend,
                    zone_id,
                    np,
                    s.position_ms,
                    body.podcast_id.as_deref(),
                ) {
                    warn!(zone_id, error = %e, "reprise_podcast_non_enregistree");
                }
            }
        }
    }
    let zone_state = state.playback.get_state(zone_id).await;
    let statut = echec.as_ref().map_or(StatusCode::OK, |(s, _, _)| *s);
    let code = echec.as_ref().map(|(_, c, _)| *c);
    let output_error = echec.map(|(_, _, e)| e);
    (
        statut,
        Json(
            json!({"zone_id": zone_id, "title": title, "podcast": podcast_name, "audio_url": body.audio_url, "mime_type": mime_type, "output_sent": output_sent, "error": output_error, "code": code, "state": zone_state}),
        ),
    )
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

    /// #7152 — le pays de la recherche, dans l'ordre : requête, réglage,
    /// langue de l'interface, système, `us`.
    #[test]
    fn le_pays_de_la_recherche_suit_l_utilisateur_7152() {
        let p = |r, g, a, s| pays_de_recherche(r, g, a, s);
        assert_eq!(
            p(Some("FR"), Some("de"), Some("it-IT"), Some("es_ES.UTF-8")),
            ("fr".into(), OriginePaysPodcasts::Requete)
        );
        assert_eq!(
            p(None, Some("de"), Some("it-IT"), None),
            ("de".into(), OriginePaysPodcasts::Reglage)
        );
        assert_eq!(
            p(Some(""), Some(""), Some("fr-FR,fr;q=0.9,en;q=0.5"), None),
            ("fr".into(), OriginePaysPodcasts::Langue)
        );
        // La région de la langue gagne sur la langue : fr-CA est le Canada.
        assert_eq!(p(None, None, Some("fr-CA"), None).0, "ca");
        // Une langue sans région : son pays par défaut ; le poids q compte.
        assert_eq!(p(None, None, Some("en;q=0.2, sv;q=0.9"), None).0, "se");
        assert_eq!(p(None, None, Some("zh-Hant-TW"), None).0, "tw");
        assert_eq!(
            p(None, None, None, Some("fr_FR.UTF-8")),
            ("fr".into(), OriginePaysPodcasts::Systeme)
        );
        assert_eq!(
            p(None, None, Some("*"), Some("C.UTF-8")),
            ("us".into(), OriginePaysPodcasts::ParDefaut)
        );
        // Un pays illisible n'est pas transmis tel quel.
        assert_eq!(
            p(Some("fra"), None, None, None).1,
            OriginePaysPodcasts::ParDefaut
        );
    }

    /// #7152 — la route ne pose plus `us` d'office : un client qui n'envoie
    /// pas de pays laisse la place au pays de l'utilisateur.
    #[test]
    fn la_recherche_n_impose_plus_us_7152() {
        let uri: axum::http::Uri = "/podcasts/search?q=x".parse().unwrap();
        let q = Query::<SearchQuery>::try_from_uri(&uri).unwrap().0;
        assert_eq!(q.country, None);
        let uri: axum::http::Uri = "/podcasts/search?q=x&country=fr".parse().unwrap();
        let q = Query::<SearchQuery>::try_from_uri(&uri).unwrap().0;
        assert_eq!(q.country.as_deref(), Some("fr"));
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

#[cfg(test)]
#[path = "podcasts/reprise_navigateur_7354_tests.rs"]
mod reprise_navigateur_7354_tests;
