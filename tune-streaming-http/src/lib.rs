use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::event_bus::EventBus;
use tune_core::streaming::ServiceRegistry;
use tune_core::streaming::traits::StreamingService;

pub mod deezer_proxy_handler;

/// Sous-ensemble de l'état serveur nécessaire aux routes des services de
/// streaming. Cette frontière empêche ces routes de dépendre de tout le
/// monolithe `tune-server` et rend leur compilation indépendante.
#[derive(Clone)]
pub struct StreamingHttpState {
    backend: Arc<dyn DbBackend>,
    services: Arc<Mutex<ServiceRegistry>>,
    event_bus: Arc<EventBus>,
}

impl StreamingHttpState {
    pub fn new(
        backend: Arc<dyn DbBackend>,
        services: Arc<Mutex<ServiceRegistry>>,
        event_bus: Arc<EventBus>,
    ) -> Self {
        Self {
            backend,
            services,
            event_bus,
        }
    }

    async fn save_tokens(&self) {
        let registry = self.services.lock().await;
        registry.save_all_tokens(&self.backend).await;
    }
}

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<usize>,
}

/// Look up a service by name. Locks the registry only long enough to clone
/// the Arc, so callers never hold the registry lock across await points.
async fn get_svc(
    state: &StreamingHttpState,
    name: &str,
) -> Result<Arc<RwLock<Box<dyn StreamingService>>>, (StatusCode, String)> {
    let registry = state.services.lock().await;
    registry
        .get(name)
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("unknown service: {name}")))
    // registry lock drops here
}

/// Type de favori de streaming demandé sur `/{service}/favorites/{fav_type}`.
///
/// Ce type existe pour une seule raison : `service_favorites` dispatche le
/// `fav_type` à DEUX endroits — l'aller, et la reprise après rafraîchissement
/// du jeton sur 401. Deux `match` sur une chaîne libre dérivent en silence dès
/// qu'on ajoute un type à l'un et qu'on oublie l'autre. En passant par une
/// énumération, le compilateur refuse le second `match` incomplet (#2370).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TypeFavori {
    Tracks,
    Albums,
    Artists,
    Playlists,
}

impl TypeFavori {
    /// Le client envoie le type au PLURIEL dans l'URL. `None` = type que le
    /// serveur ne sait pas lire.
    fn parse(fav_type: &str) -> Option<Self> {
        match fav_type {
            "tracks" => Some(Self::Tracks),
            "albums" => Some(Self::Albums),
            "artists" => Some(Self::Artists),
            "playlists" => Some(Self::Playlists),
            _ => None,
        }
    }

    /// Clé du tableau dans la réponse JSON attendue par le client.
    fn cle(self) -> &'static str {
        match self {
            Self::Tracks => "tracks",
            Self::Albums => "albums",
            Self::Artists => "artists",
            Self::Playlists => "playlists",
        }
    }
}

/// Convert a service method result into a JSON response (OK -> 200, Err -> 502).
fn svc_response<R: serde::Serialize, E: std::fmt::Display>(result: Result<R, E>) -> Response {
    match result {
        Ok(data) => Json(json!(data)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

/// En-tête complet, et non une durée : `HeaderValue::from_static` exige un
/// littéral, donc séparer la valeur du texte les ferait diverger.
///
/// 1800 s — aligné sur le TTL du cache serveur (`qobuz.rs`) : les sélections
/// changent au mieux une fois par jour.
const CACHE_EDITORIAL: &str = "private, max-age=1800";

/// Comme `svc_response`, mais autorise le navigateur à garder la réponse.
///
/// `private` et non `public` : ces routes sont derrière l'authentification, et
/// même si le contenu est le même pour tous, on ne veut pas qu'un proxy
/// partagé le stocke.
///
/// Réservé au contenu ÉDITORIAL — sélections, nouveautés, genres. JAMAIS les
/// favoris ni les playlists de l'utilisateur : resservir une réponse vieille de
/// trente minutes ferait réapparaître un favori qu'il vient de retirer.
///
/// Une erreur n'est pas mise en cache : un 502 passager deviendrait une panne
/// de trente minutes.
fn svc_response_editorial<R: serde::Serialize, E: std::fmt::Display>(
    result: Result<R, E>,
) -> Response {
    let est_ok = result.is_ok();
    let mut response = svc_response(result);
    if est_ok {
        response.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static(CACHE_EDITORIAL),
        );
    }
    response
}

// ---------------------------------------------------------------------------
// Cache TTL du contenu UTILISATEUR — playlists et favoris (#1621, lot 5)
// ---------------------------------------------------------------------------

/// Durée de vie des listes UTILISATEUR (playlists, favoris) en cache.
///
/// Chaque retour dans la vue streaming relançait la totalité des requêtes —
/// dont les favoris, repaginés en entier auprès du service (#1621). 120 s
/// suffit à absorber ces allers-retours de navigation, et reste assez court
/// pour qu'une modification faite HORS de Tune (l'app Qobuz du téléphone)
/// apparaisse vite. Les mutations faites PAR Tune n'attendent pas ce délai :
/// elles purgent le cache du service ([`purge_contenu_utilisateur`]).
///
/// Ce cache est SERVEUR uniquement — la réponse reste sous le `no-cache` du
/// middleware, contrairement à l'éditorial ([`CACHE_EDITORIAL`]) : un
/// navigateur qui resservirait un favori retiré n'aurait aucun moyen d'être
/// purgé, lui.
const TTL_CONTENU_UTILISATEUR: Duration = Duration::from_secs(120);

/// Une liste utilisateur mémorisée, datée pour l'expiration.
struct EntreeUtilisateur {
    cree: Instant,
    donnees: Value,
}

/// Clé = (service, ressource) — ex. `("qobuz", "favorites/tracks")`.
///
/// `static` et non un champ de [`StreamingHttpState`] : l'état est RECONSTRUIT
/// à chaque requête par `FromRef` (`tune-server/src/state.rs`), un champ n'y
/// survivrait pas d'une requête à l'autre — même raison que
/// [`services_snapshot_cache`].
fn cache_contenu_utilisateur()
-> &'static std::sync::Mutex<HashMap<(String, String), EntreeUtilisateur>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<HashMap<(String, String), EntreeUtilisateur>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// La liste en cache si elle est fraîche, sinon rien.
fn contenu_utilisateur_frais(service: &str, ressource: &str) -> Option<Value> {
    let cache = cache_contenu_utilisateur().lock().ok()?;
    cache
        .get(&(service.to_string(), ressource.to_string()))
        .and_then(|e| {
            if e.cree.elapsed() < TTL_CONTENU_UTILISATEUR {
                tracing::debug!(service, ressource, "streaming_user_cache_hit");
                Some(e.donnees.clone())
            } else {
                None
            }
        })
}

/// Mémorise une liste utilisateur. Les erreurs ne sont JAMAIS mémorisées :
/// c'est à l'appelant de ne passer ici que des succès.
fn memoriser_contenu_utilisateur(service: &str, ressource: &str, donnees: Value) {
    let Ok(mut cache) = cache_contenu_utilisateur().lock() else {
        return;
    };
    cache.insert(
        (service.to_string(), ressource.to_string()),
        EntreeUtilisateur {
            cree: Instant::now(),
            donnees,
        },
    );
}

/// Oublie TOUT le contenu utilisateur d'un service.
///
/// Appelée après chaque mutation — favori ajouté/retiré (y compris la
/// souscription de playlist, qui entre par la même route), playlist
/// créée/supprimée/modifiée — et après tout changement de session (login,
/// logout) : le cache d'un compte ne doit jamais être servi à un autre.
///
/// Sans condition sur le succès : une mutation en échec côté HTTP peut avoir
/// abouti côté service (délai dépassé), et le prix d'une purge de trop est un
/// seul rechargement.
fn purge_contenu_utilisateur(service: &str) {
    let Ok(mut cache) = cache_contenu_utilisateur().lock() else {
        return;
    };
    cache.retain(|(svc, _), _| svc != service);
}

/// Reduce boilerplate for read-only handlers: get_svc + lock + call + respond.
macro_rules! with_svc {
    ($state:expr, $service:expr, |$svc:ident| $body:expr) => {{
        let arc = match get_svc($state, $service).await {
            Ok(s) => s,
            Err(e) => return e.into_response(),
        };
        // `.read()` : les gestionnaires en lecture ne peuvent PAS muter — leurs
        // methodes sont en `&self` — donc l'exclusivite du Mutex leur etait
        // inutile et les serialisait pour rien (#1969).
        let $svc = arc.read().await;
        svc_response($body)
    }};
}

/// Same as `with_svc!` but acquires a mutable lock.
/// Comme `with_svc!`, mais la réponse autorise le cache navigateur.
///
/// Une macro distincte plutôt qu'un drapeau : le choix se voit sur le
/// gestionnaire, à la ligne où on le lit. Un booléen en fin d'appel se recopie
/// sans y penser d'un gestionnaire éditorial vers un gestionnaire de favoris.
macro_rules! with_svc_editorial {
    ($state:expr, $service:expr, |$svc:ident| $body:expr) => {{
        let arc = match get_svc($state, $service).await {
            Ok(s) => s,
            Err(e) => return e.into_response(),
        };
        let $svc = arc.read().await;
        svc_response_editorial($body)
    }};
}
macro_rules! with_svc_mut {
    ($state:expr, $service:expr, |$svc:ident| $body:expr) => {{
        let arc = match get_svc($state, $service).await {
            Ok(s) => s,
            Err(e) => return e.into_response(),
        };
        // `.write()` : ces gestionnaires appellent des methodes `&mut self`
        // (rafraichissement de jeton, favoris, deconnexion). Le compilateur le
        // fait respecter — un `&mut self` ne compile pas sous un `.read()`.
        let mut $svc = arc.write().await;
        svc_response($body)
    }};
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    limit: Option<usize>,
}

pub fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    StreamingHttpState: axum::extract::FromRef<S>,
{
    Router::new()
        .route("/services", get(list_services))
        .route("/status", get(list_services))
        .route("/{service}/status", get(service_status))
        .route("/{service}/auth", post(service_auth))
        .route("/{service}/auth/device-code", post(service_auth))
        .route("/{service}/auth/poll", post(service_auth))
        .route("/{service}/auth/status", get(auth_poll_status))
        .route("/{service}/auth/logout", post(service_logout))
        .route("/{service}/logout", post(service_logout))
        .route("/{service}/disconnect", post(service_logout))
        .route("/compare", get(compare_services))
        .route("/{service}/search", get(service_search))
        .route("/{service}/albums", get(service_albums))
        .route("/{service}/albums/{album_id}", get(service_album))
        .route(
            "/{service}/albums/{album_id}/tracks",
            get(service_album_tracks),
        )
        .route("/{service}/artists/{artist_id}", get(service_artist))
        .route(
            "/{service}/artists/{artist_id}/albums",
            get(service_artist_albums),
        )
        .route(
            "/{service}/artists/{artist_id}/top-tracks",
            get(service_artist_top_tracks),
        )
        .route(
            "/{service}/playlists",
            get(service_playlists).post(service_create_playlist),
        )
        .route(
            "/{service}/playlists/{playlist_id}",
            get(service_playlist).delete(service_delete_playlist),
        )
        .route(
            "/{service}/playlists/{playlist_id}/tracks",
            get(service_playlist_tracks).post(service_add_tracks),
        )
        .route(
            "/{service}/playlists/{playlist_id}/tracks/remove",
            post(service_remove_tracks),
        )
        .route("/{service}/tracks/{track_id}", get(service_track))
        .route("/{service}/tracks/{track_id}/url", get(service_track_url))
        .route("/{service}/featured", get(service_featured))
        .route(
            "/{service}/featured/sections",
            get(service_featured_sections),
        )
        .route(
            "/{service}/featured/{section}",
            get(service_featured_section),
        )
        .route(
            "/{service}/albums/{album_id}/label",
            get(service_album_label),
        )
        .route(
            "/{service}/albums/{album_id}/context",
            get(service_album_context),
        )
        .route("/{service}/playlist-tags", get(service_playlist_tags))
        .route(
            "/{service}/featured-playlists",
            get(service_featured_playlists),
        )
        .route(
            "/{service}/featured-playlists/by-tag",
            get(service_featured_playlists_by_tag),
        )
        .route("/{service}/new-releases", get(service_new_releases))
        .route("/{service}/genres", get(service_genres))
        .route(
            "/{service}/genres/{genre_id}/albums",
            get(service_genre_albums),
        )
        .route("/{service}/favorites/{fav_type}", get(service_favorites))
        .route(
            "/{service}/favorites/{fav_type}/{item_id}",
            post(service_add_favorite).delete(service_remove_favorite),
        )
        .route("/{service}/enable", post(service_enable))
        .route("/{service}/disable", post(service_disable))
        .route("/{service}/auth/url", get(service_auth_url))
        .route("/youtube/home", get(youtube_home))
        .route("/youtube/charts", get(youtube_charts))
        .route("/youtube/moods", get(youtube_moods))
        .route("/youtube/library", get(youtube_library))
        .route("/spotify/callback", get(spotify_callback))
        .route("/tidal/callback", get(tidal_callback))
}

// ---------------------------------------------------------------------------
// Simple read-only handlers (via with_svc!)

// ---------------------------------------------------------------------------

/// `?limit=` est un nombre d'éléments PAR CATÉGORIE (albums, artistes, titres,
/// playlists), pas un total. `limit=0` demande « Tous », à la charge du service
/// de le borner — la recherche Qobuz pagine et s'arrête au plafond documenté
/// dans `qobuz.rs` (#2160). Absent, la valeur reste 20, ce que les clients
/// antérieurs obtenaient déjà.
async fn service_search(
    State(state): State<StreamingHttpState>,
    Path(service): Path<String>,
    Query(q): Query<SearchQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(20);
    with_svc!(&state, &service, |svc| svc.search(&q.q, limit).await)
}

async fn service_albums(
    State(state): State<StreamingHttpState>,
    Path(service): Path<String>,
) -> Response {
    if let Some(donnees) = contenu_utilisateur_frais(&service, "albums") {
        return Json(donnees).into_response();
    }
    with_svc!(&state, &service, |svc| svc.get_user_albums().await.inspect(
        |a| memoriser_contenu_utilisateur(&service, "albums", json!(a))
    ))
}

async fn service_album(
    State(state): State<StreamingHttpState>,
    Path((service, album_id)): Path<(String, String)>,
) -> Response {
    with_svc!(&state, &service, |svc| svc.get_album(&album_id).await)
}

async fn service_album_tracks(
    State(state): State<StreamingHttpState>,
    Path((service, album_id)): Path<(String, String)>,
) -> Response {
    with_svc!(&state, &service, |svc| svc
        .get_album_tracks(&album_id)
        .await)
}

async fn service_artist(
    State(state): State<StreamingHttpState>,
    Path((service, artist_id)): Path<(String, String)>,
) -> Response {
    let arc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let result = {
        let svc = arc.read().await;
        svc.get_artist(&artist_id).await
    };

    // Best-effort: persist a streaming editorial bio (e.g. Qobuz) into a
    // name-matched local artist that has none yet, so the library keeps it.
    // Fire-and-forget so the browse response is never delayed.
    if let Ok(ref artist) = result {
        if let Some(bio) = artist.bio.clone() {
            if bio.len() > 50 {
                let backend = state.backend.clone();
                let name = artist.name.clone();
                let svc_name = service.clone();
                tokio::spawn(async move {
                    let repo = tune_core::db::artist_repo::ArtistRepo::with_backend(backend);
                    if let Ok(Some(local)) = repo.get_by_name(&name) {
                        if let Some(id) = local.id {
                            if local.bio.as_deref().unwrap_or("").is_empty() {
                                let _ = repo.update_bio_full(id, &bio, &svc_name, None, "", "");
                            }
                        }
                    }
                });
            }
        }
    }

    svc_response(result)
}

/// `?offset=` pour un « voir plus » : la discographie s'arrêtait au
/// cinquantième album sans que rien n'indique qu'il y en avait d'autres.
/// Absent, l'offset vaut 0 — les clients antérieurs ne changent pas de
/// comportement.
#[derive(Deserialize)]
struct PageQuery {
    #[serde(default)]
    offset: u32,
}

async fn service_artist_albums(
    State(state): State<StreamingHttpState>,
    Path((service, artist_id)): Path<(String, String)>,
    Query(q): Query<PageQuery>,
) -> Response {
    with_svc!(&state, &service, |svc| svc
        .get_artist_albums_page(&artist_id, q.offset)
        .await)
}

async fn service_artist_top_tracks(
    State(state): State<StreamingHttpState>,
    Path((service, artist_id)): Path<(String, String)>,
) -> Response {
    with_svc!(&state, &service, |svc| svc
        .get_artist_top_tracks(&artist_id)
        .await)
}

async fn service_playlists(
    State(state): State<StreamingHttpState>,
    Path(service): Path<String>,
) -> Response {
    if let Some(donnees) = contenu_utilisateur_frais(&service, "playlists") {
        return Json(donnees).into_response();
    }
    with_svc!(&state, &service, |svc| svc
        .get_user_playlists()
        .await
        .inspect(|p| memoriser_contenu_utilisateur(
            &service,
            "playlists",
            json!(p)
        )))
}

async fn service_playlist(
    State(state): State<StreamingHttpState>,
    Path((service, playlist_id)): Path<(String, String)>,
) -> Response {
    with_svc!(&state, &service, |svc| svc.get_playlist(&playlist_id).await)
}

async fn service_playlist_tracks(
    State(state): State<StreamingHttpState>,
    Path((service, playlist_id)): Path<(String, String)>,
) -> Response {
    with_svc!(&state, &service, |svc| svc
        .get_playlist_tracks(&playlist_id)
        .await)
}

#[derive(Deserialize)]
struct CreatePlaylistBody {
    name: String,
    description: Option<String>,
}

async fn service_create_playlist(
    State(state): State<StreamingHttpState>,
    Path(service): Path<String>,
    Json(body): Json<CreatePlaylistBody>,
) -> Response {
    let reponse = with_svc!(&state, &service, |svc| svc
        .create_playlist(&body.name, body.description.as_deref())
        .await
        .map(|id| json!({ "id": id })));
    purge_contenu_utilisateur(&service);
    reponse
}

#[derive(Deserialize)]
struct AddTracksBody {
    track_ids: Vec<String>,
}

async fn service_add_tracks(
    State(state): State<StreamingHttpState>,
    Path((service, playlist_id)): Path<(String, String)>,
    Json(body): Json<AddTracksBody>,
) -> Response {
    let reponse = with_svc!(&state, &service, |svc| svc
        .add_tracks_to_playlist(&playlist_id, &body.track_ids)
        .await
        .map(|n| json!({ "added": n })));
    purge_contenu_utilisateur(&service);
    reponse
}

async fn service_delete_playlist(
    State(state): State<StreamingHttpState>,
    Path((service, playlist_id)): Path<(String, String)>,
) -> Response {
    let reponse = with_svc!(&state, &service, |svc| svc
        .delete_playlist(&playlist_id)
        .await
        .map(|_| json!({ "ok": true })));
    purge_contenu_utilisateur(&service);
    reponse
}

async fn service_remove_tracks(
    State(state): State<StreamingHttpState>,
    Path((service, playlist_id)): Path<(String, String)>,
    Json(body): Json<AddTracksBody>,
) -> Response {
    let reponse = with_svc!(&state, &service, |svc| svc
        .remove_tracks_from_playlist(&playlist_id, &body.track_ids)
        .await
        .map(|n| json!({ "removed": n })));
    purge_contenu_utilisateur(&service);
    reponse
}

async fn service_track(
    State(state): State<StreamingHttpState>,
    Path((service, track_id)): Path<(String, String)>,
) -> Response {
    with_svc!(&state, &service, |svc| svc.get_track(&track_id).await)
}

async fn service_featured(
    State(state): State<StreamingHttpState>,
    Path(service): Path<String>,
) -> Response {
    with_svc_editorial!(&state, &service, |svc| svc.get_featured().await)
}

async fn service_new_releases(
    State(state): State<StreamingHttpState>,
    Path(service): Path<String>,
) -> Response {
    with_svc_editorial!(&state, &service, |svc| svc.get_new_releases().await)
}

#[derive(Deserialize)]
struct GenreQuery {
    parent_id: Option<String>,
}

async fn service_genres(
    State(state): State<StreamingHttpState>,
    Path(service): Path<String>,
    Query(q): Query<GenreQuery>,
) -> Response {
    let pid = q.parent_id.as_deref();
    with_svc_editorial!(&state, &service, |svc| svc.get_genres(pid).await)
}

async fn service_genre_albums(
    State(state): State<StreamingHttpState>,
    Path((service, genre_id)): Path<(String, String)>,
    Query(q): Query<LimitQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(50);
    with_svc_editorial!(&state, &service, |svc| svc
        .get_genre_albums(&genre_id, limit)
        .await)
}

async fn service_featured_sections(
    State(state): State<StreamingHttpState>,
    Path(service): Path<String>,
) -> Response {
    with_svc_editorial!(&state, &service, |svc| svc.get_featured_sections().await)
}

async fn service_featured_section(
    State(state): State<StreamingHttpState>,
    Path((service, section)): Path<(String, String)>,
) -> Response {
    with_svc_editorial!(&state, &service, |svc| svc
        .get_featured_section(&section)
        .await)
}

async fn service_album_label(
    State(state): State<StreamingHttpState>,
    Path((service, album_id)): Path<(String, String)>,
) -> Response {
    with_svc!(&state, &service, |svc| svc.get_album_label(&album_id).await)
}

async fn service_playlist_tags(
    State(state): State<StreamingHttpState>,
    Path(service): Path<String>,
) -> Response {
    with_svc_editorial!(&state, &service, |svc| svc.get_playlist_tags().await)
}

#[derive(Deserialize)]
struct FeaturedPlaylistsQuery {
    tag: Option<String>,
    genre: Option<String>,
}

async fn service_featured_playlists(
    State(state): State<StreamingHttpState>,
    Path(service): Path<String>,
    Query(q): Query<FeaturedPlaylistsQuery>,
) -> Response {
    with_svc_editorial!(&state, &service, |svc| svc
        .get_featured_playlists(q.tag.as_deref(), q.genre.as_deref())
        .await)
}

#[derive(Deserialize)]
struct ByTagQuery {
    genre: Option<String>,
}

/// Les playlists éditoriales rangées par catégorie, comme le service les
/// présente. Un seul appel : le client n'a pas à lire les tags puis à lancer
/// une requête par tag.
async fn service_featured_playlists_by_tag(
    State(state): State<StreamingHttpState>,
    Path(service): Path<String>,
    Query(q): Query<ByTagQuery>,
) -> Response {
    with_svc_editorial!(&state, &service, |svc| svc
        .get_featured_playlists_by_tag(q.genre.as_deref())
        .await)
}

async fn service_album_context(
    State(state): State<StreamingHttpState>,
    Path((service, album_id)): Path<(String, String)>,
) -> Response {
    with_svc!(&state, &service, |svc| svc
        .get_album_context(&album_id)
        .await)
}

// ---------------------------------------------------------------------------
// Mutable handlers (via with_svc_mut!)

// ---------------------------------------------------------------------------

async fn service_add_favorite(
    State(state): State<StreamingHttpState>,
    Path((service, fav_type, item_id)): Path<(String, String, String)>,
) -> Response {
    let reponse = with_svc_mut!(&state, &service, |svc| svc
        .add_favorite(&fav_type, &item_id)
        .await);
    // Toute écriture de favori passe par ici — y compris une souscription de
    // playlist, quel que soit l'endpoint amont que le connecteur choisit : la
    // purge n'a pas à connaître ce détail.
    purge_contenu_utilisateur(&service);
    reponse
}

async fn service_remove_favorite(
    State(state): State<StreamingHttpState>,
    Path((service, fav_type, item_id)): Path<(String, String, String)>,
) -> Response {
    let reponse = with_svc_mut!(&state, &service, |svc| svc
        .remove_favorite(&fav_type, &item_id)
        .await);
    purge_contenu_utilisateur(&service);
    reponse
}

// ---------------------------------------------------------------------------
// Complex handlers (custom logic beyond simple get_svc + call + respond)

// ---------------------------------------------------------------------------

/// Last successful `list_services` snapshot. `status_all()` locks every service
/// mutex to read its live auth status; during playback a service mutex is held
/// by the streaming operation, so the lock blocks and the 10s timeout fires. The
/// old code then returned `unwrap_or_default()` = an EMPTY map, which makes the
/// client believe no service is authenticated and silently drops Qobuz favoris /
/// playlists mid-playback (forum #1156). Serving the last-known snapshot instead
/// keeps the gate stable across those transient timeouts.
fn services_snapshot_cache() -> &'static Mutex<serde_json::Map<String, Value>> {
    static CACHE: std::sync::OnceLock<Mutex<serde_json::Map<String, Value>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(serde_json::Map::new()))
}

async fn list_services(State(state): State<StreamingHttpState>) -> Json<Value> {
    // Timeout to avoid blocking the Settings page if a streaming service auth check hangs
    let fresh = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let registry = state.services.lock().await;
        let services = registry.status_all().await;
        let mut map = serde_json::Map::new();
        for svc in services {
            if let Some(name) = svc.get("name").and_then(|n| n.as_str()) {
                map.insert(name.to_string(), svc);
            }
        }
        map
    })
    .await;

    match fresh {
        Ok(map) => {
            // Refresh the cache so a later timeout can fall back to this snapshot.
            *services_snapshot_cache().lock().await = map.clone();
            Json(Value::Object(map))
        }
        Err(_) => {
            // Degrade to the last-known snapshot instead of an empty map, so a
            // transient timeout (e.g. a service mutex held during playback) does
            // not wipe the authenticated-service gate on the client (#1156).
            let cached = services_snapshot_cache().lock().await.clone();
            tracing::warn!(
                cached_services = cached.len(),
                "list_services timed out; serving cached snapshot"
            );
            Json(Value::Object(cached))
        }
    }
}

async fn service_status(
    State(state): State<StreamingHttpState>,
    Path(service): Path<String>,
) -> Response {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    let mut svc = svc.write().await;
    let mut status = svc.auth_status().await;
    if !status.authenticated
        && let Ok(poll_status) = svc.authenticate(&json!({"poll": true})).await
        && poll_status.authenticated
    {
        status = poll_status;
        drop(svc);
        state.save_tokens().await;
        purge_contenu_utilisateur(&service);
    }
    Json(json!({
        "service": service,
        "enabled": true,
        "authenticated": status.authenticated,
        "username": status.username,
        "subscription": status.subscription,
    }))
    .into_response()
}

async fn service_auth(
    State(state): State<StreamingHttpState>,
    Path(service): Path<String>,
    raw_body: axum::body::Bytes,
) -> Response {
    let body: Option<Value> = if raw_body.is_empty() {
        None
    } else {
        serde_json::from_slice(&raw_body).ok()
    };

    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    let mut svc = svc.write().await;
    let credentials = body.unwrap_or(json!({"device_flow": true}));

    match svc.authenticate(&credentials).await {
        Ok(status) => {
            drop(svc);
            state.save_tokens().await;
            // Nouveau compte possible : les listes de l'ancien ne valent plus.
            purge_contenu_utilisateur(&service);
            if status.authenticated {
                state.event_bus.emit(
                    "streaming.auth.success",
                    json!({
                        "service": &service,
                        "username": &status.username,
                    }),
                );
            }
            Json(json!({
                "service": service,
                "authenticated": status.authenticated,
                "username": status.username,
                "verification_url": status.verification_url,
                "user_code": status.user_code,
                "device_code": status.device_code,
                "expires_in": status.expires_in,
            }))
            .into_response()
        }
        Err(e) => {
            let err_msg = e.to_string();
            // Log the exact reason: the device-code/auth failure was only
            // returned in the 400 body, invisible in the server logs, so a
            // failing YouTube login (network to Google, rate limit, etc.)
            // could not be diagnosed from a log alone (Fabien).
            tracing::warn!(service = %service, error = %err_msg, "streaming_auth_failed");
            state.event_bus.emit(
                "streaming.auth.failed",
                json!({
                    "service": &service,
                    "error": &err_msg,
                }),
            );
            (StatusCode::BAD_REQUEST, err_msg).into_response()
        }
    }
}

async fn auth_poll_status(
    State(state): State<StreamingHttpState>,
    Path(service): Path<String>,
) -> Response {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };

    let mut svc = svc.write().await;
    let poll_creds = json!({"poll": true});
    match svc.authenticate(&poll_creds).await {
        Ok(status) => {
            let authenticated = status.authenticated;
            let username = status.username.clone();
            if authenticated {
                drop(svc);
                state.save_tokens().await;
                purge_contenu_utilisateur(&service);
            }
            Json(json!({
                "service": service,
                "authenticated": authenticated,
                "username": username,
                // The YouTube client names this account identifier `email`.
                // Keep the generic `username` field and expose the explicit
                // alias so the route fulfils both contracts (#1897).
                "email": status.username,
            }))
            .into_response()
        }
        Err(e) => Json(json!({
            "service": service,
            "authenticated": false,
            "email": Value::Null,
            "message": e.to_string(),
        }))
        .into_response(),
    }
}

async fn service_logout(
    State(state): State<StreamingHttpState>,
    Path(service): Path<String>,
) -> Response {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let mut svc = svc.write().await;
    svc.logout().await.ok();
    drop(svc);
    state.save_tokens().await;
    // Le compte change : ses listes ne doivent pas survivre à la session.
    purge_contenu_utilisateur(&service);
    Json(json!({ "service": service, "status": "logged_out" })).into_response()
}

async fn service_track_url(
    State(state): State<StreamingHttpState>,
    Path((service, track_id)): Path<(String, String)>,
) -> Response {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let mut svc = svc.write().await;

    match svc.get_track_url(&track_id, None).await {
        Ok(url) => Json(json!(url)).into_response(),
        Err(ref e)
            if {
                let msg = e.to_string();
                msg.contains("401") || msg.contains("403")
            } =>
        {
            // Token may have expired — attempt refresh and retry once
            if svc.refresh_if_needed().await.unwrap_or(false) {
                drop(svc);
                state.save_tokens().await;
                let svc = match get_svc(&state, &service).await {
                    Ok(s) => s,
                    Err(e) => return e.into_response(),
                };
                let svc = svc.read().await;
                match svc.get_track_url(&track_id, None).await {
                    Ok(url) => Json(json!(url)).into_response(),
                    Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
                }
            } else {
                (StatusCode::BAD_GATEWAY, e.to_string()).into_response()
            }
        }
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

/// Le dispatch de lecture des favoris, partagé entre l'aller et la reprise
/// après rafraîchissement du jeton — deux copies divergeaient déjà (#2370).
async fn lire_favoris(
    svc: &dyn StreamingService,
    fav_type: &str,
) -> Result<Value, tune_core::TuneError> {
    match TypeFavori::parse(fav_type) {
        Some(TypeFavori::Tracks) => svc.get_user_tracks().await.map(|t| json!({ "tracks": t })),
        Some(TypeFavori::Albums) => svc.get_user_albums().await.map(|a| json!({ "albums": a })),
        Some(TypeFavori::Artists) => svc
            .get_user_artists()
            .await
            .map(|a| json!({ "artists": a })),
        // `get_user_playlists` est une methode REQUISE du trait, deja
        // implementee par tous les connecteurs (Qobuz lit
        // `/playlist/getUserPlaylists`). Rien a inventer ici : le type
        // manquait au dispatch, pas au service.
        Some(TypeFavori::Playlists) => svc
            .get_user_playlists()
            .await
            .map(|p| json!({ "playlists": p })),
        None => Err(format!("unknown favorite type: {fav_type}").into()),
    }
}

async fn service_favorites(
    State(state): State<StreamingHttpState>,
    Path((service, fav_type)): Path<(String, String)>,
) -> Response {
    let arc = match get_svc(&state, &service).await {
        Ok(s) => s,
        // A non-streaming source (e.g. "upnp"/"radio"/"podcast" media-server
        // items) has no streaming favorites. Return an empty list (200) rather
        // than a plain-text 404 so the web client's `.json()` doesn't blow up
        // with "TypeError: (void 0) is not a function" (Yacine, DevTools console:
        // GET /streaming/upnp/favorites/tracks 404).
        Err(_) => {
            let cle = TypeFavori::parse(&fav_type)
                .map(TypeFavori::cle)
                .unwrap_or("tracks");
            return Json(json!({ cle: [] })).into_response();
        }
    };
    let ressource = format!("favorites/{fav_type}");
    if let Some(donnees) = contenu_utilisateur_frais(&service, &ressource) {
        return Json(donnees).into_response();
    }
    // Verrou de LECTURE pour l'aller : le client web demande les trois types
    // de favoris en parallèle (`Promise.all`), et le verrou d'écriture que ce
    // gestionnaire prenait les sérialisait — temps total = somme des trois,
    // pas le max (#1621). Le dispatch n'appelle que des méthodes `&self` ;
    // seul le rafraîchissement de jeton, plus bas, exige l'écriture.
    let result = {
        let svc = arc.read().await;
        lire_favoris(&**svc, &fav_type).await
    };
    match result {
        Ok(data) => {
            memoriser_contenu_utilisateur(&service, &ressource, data.clone());
            Json(data).into_response()
        }
        Err(ref e)
            if {
                let msg = e.to_string();
                msg.contains("401") || msg.contains("403")
            } =>
        {
            // Token expired — attempt refresh and retry
            let rafraichi = {
                let mut svc = arc.write().await;
                svc.refresh_if_needed().await.unwrap_or(false)
            };
            if rafraichi {
                state.save_tokens().await;
                let arc = match get_svc(&state, &service).await {
                    Ok(s) => s,
                    Err(e) => return e.into_response(),
                };
                let svc = arc.read().await;
                match lire_favoris(&**svc, &fav_type).await {
                    Ok(data) => {
                        memoriser_contenu_utilisateur(&service, &ressource, data.clone());
                        Json(data).into_response()
                    }
                    Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
                }
            } else {
                (StatusCode::BAD_GATEWAY, e.to_string()).into_response()
            }
        }
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

async fn service_enable(
    State(state): State<StreamingHttpState>,
    Path(service): Path<String>,
) -> Response {
    if let Ok(svc) = get_svc(&state, &service).await {
        let mut svc = svc.write().await;
        svc.set_enabled(true);
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(&format!("streaming_{service}_enabled"), "true")
        .ok();
    Json(json!({"service": service, "enabled": true})).into_response()
}

async fn service_disable(
    State(state): State<StreamingHttpState>,
    Path(service): Path<String>,
) -> Response {
    if let Ok(svc) = get_svc(&state, &service).await {
        let mut svc = svc.write().await;
        svc.set_enabled(false);
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(&format!("streaming_{service}_enabled"), "false")
        .ok();
    Json(json!({"service": service, "enabled": false})).into_response()
}

async fn service_auth_url(
    State(state): State<StreamingHttpState>,
    Path(service): Path<String>,
) -> Response {
    let svc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let mut svc = svc.write().await;
    match svc.authenticate(&json!({"device_flow": true})).await {
        Ok(status) => Json(json!({
            "url": status.verification_url,
            "user_code": status.user_code,
        }))
        .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Stubs & OAuth callbacks
// ---------------------------------------------------------------------------

async fn youtube_home() -> Json<Value> {
    Json(json!({"sections": [], "message": "YouTube home not yet implemented"}))
}

async fn youtube_charts() -> Json<Value> {
    Json(json!({"charts": [], "message": "YouTube charts not yet implemented"}))
}

async fn youtube_moods() -> Json<Value> {
    Json(json!({"moods": [], "message": "YouTube moods not yet implemented"}))
}

async fn youtube_library() -> Json<Value> {
    Json(json!({"playlists": [], "albums": [], "artists": []}))
}

#[derive(Deserialize)]
struct SpotifyCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn spotify_callback(
    State(state): State<StreamingHttpState>,
    Query(q): Query<SpotifyCallbackQuery>,
) -> Response {
    if let Some(ref error) = q.error {
        return Json(json!({"error": error})).into_response();
    }
    let Some(code) = q.code else {
        return (StatusCode::BAD_REQUEST, "missing code parameter").into_response();
    };
    let svc = match get_svc(&state, "spotify").await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let mut svc = svc.write().await;
    match svc
        .authenticate(&json!({"code": code, "state": q.state}))
        .await
    {
        Ok(status) => {
            drop(svc);
            state.save_tokens().await;
            purge_contenu_utilisateur("spotify");
            Json(json!({
                "authenticated": status.authenticated,
                "username": status.username,
            }))
            .into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct TidalCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn tidal_callback(
    State(state): State<StreamingHttpState>,
    Query(q): Query<TidalCallbackQuery>,
) -> Response {
    if let Some(ref error) = q.error {
        let desc = q.error_description.as_deref().unwrap_or(error);
        return axum::response::Html(format!(
            r#"<!DOCTYPE html><html><body style="font-family:system-ui;background:#1a1a2e;color:#eee;display:flex;justify-content:center;align-items:center;height:100vh;margin:0">
<div style="text-align:center">
<h1 style="color:#ef4444">Tidal Authentication Failed</h1>
<p>{desc}</p>
<p style="color:#888">You can close this tab and try again.</p>
</div></body></html>"#
        ))
        .into_response();
    }

    let Some(code) = q.code else {
        return (StatusCode::BAD_REQUEST, "missing code parameter").into_response();
    };
    let callback_state = q.state.as_deref().unwrap_or("");

    let svc = match get_svc(&state, "tidal").await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let mut svc = svc.write().await;

    // Use authenticate with the code+state credentials (same pattern as Spotify)
    match svc
        .authenticate(&json!({"code": code, "state": callback_state}))
        .await
    {
        Ok(status) => {
            let username = status.username.clone().unwrap_or_default();
            drop(svc);
            state.save_tokens().await;
            purge_contenu_utilisateur("tidal");
            if status.authenticated {
                state.event_bus.emit(
                    "streaming.auth.success",
                    json!({
                        "service": "tidal",
                        "username": &username,
                    }),
                );
            }
            axum::response::Html(format!(
                r#"<!DOCTYPE html><html><body style="font-family:system-ui;background:#1a1a2e;color:#eee;display:flex;justify-content:center;align-items:center;height:100vh;margin:0">
<div style="text-align:center">
<h1 style="color:#4ade80">Tidal Connected!</h1>
<p>Logged in as <strong>{username}</strong></p>
<p style="color:#888">You can close this tab.</p>
<script>setTimeout(function(){{ window.close(); }}, 3000);</script>
</div></body></html>"#
            ))
            .into_response()
        }
        Err(e) => {
            let err_msg = e.to_string();
            state.event_bus.emit(
                "streaming.auth.failed",
                json!({
                    "service": "tidal",
                    "error": &err_msg,
                }),
            );
            axum::response::Html(format!(
                r#"<!DOCTYPE html><html><body style="font-family:system-ui;background:#1a1a2e;color:#eee;display:flex;justify-content:center;align-items:center;height:100vh;margin:0">
<div style="text-align:center">
<h1 style="color:#ef4444">Tidal Authentication Failed</h1>
<p>{err_msg}</p>
<p style="color:#888">You can close this tab and try again.</p>
</div></body></html>"#
            ))
            .into_response()
        }
    }
}

#[derive(Deserialize)]
struct CompareQuery {
    services: String,
    artist: Option<String>,
    album: Option<String>,
}

async fn compare_services(
    State(state): State<StreamingHttpState>,
    Query(q): Query<CompareQuery>,
) -> Response {
    let service_names: Vec<&str> = q.services.split(',').map(|s| s.trim()).collect();
    if service_names.len() < 2 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "need at least 2 services"})),
        )
            .into_response();
    }

    let query = q.artist.as_deref().or(q.album.as_deref()).unwrap_or("");
    if query.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "provide artist or album parameter"})),
        )
            .into_response();
    }

    let registry = state.services.lock().await;
    let mut results: serde_json::Map<String, Value> = serde_json::Map::new();

    for name in &service_names {
        let svc = match registry.get(name) {
            Some(s) => s,
            None => {
                results.insert(name.to_string(), json!({"error": "service not found"}));
                continue;
            }
        };
        let svc = svc.read().await;
        match svc.search(query, 10).await {
            Ok(sr) => {
                results.insert(
                    name.to_string(),
                    json!({
                        "tracks": sr.tracks.len(),
                        "albums": sr.albums.len(),
                        "artists": sr.artists.len(),
                        "results": sr,
                    }),
                );
            }
            Err(e) => {
                results.insert(name.to_string(), json!({"error": e.to_string()}));
            }
        }
    }
    drop(registry);

    Json(json!({
        "query": query,
        "services": results,
    }))
    .into_response()
}

#[cfg(test)]
mod tests_favoris_playlist {
    use super::*;

    /// #2370 — Gros Bidon (fil 1541) : « on ne peut pas mettre une playlist
    /// Qobuz en favori ». `GET /streaming/{service}/favorites/playlists`
    /// retombe aujourd'hui dans le bras par défaut et sort en 400
    /// « unknown favorite type: playlists ». Même si l'écriture était réglée,
    /// AUCUN écran ne pourrait relire le favori.
    #[test]
    fn le_type_playlists_est_un_type_de_favori_connu_du_serveur() {
        let t = TypeFavori::parse("playlists");
        assert!(
            t.is_some(),
            "GET /streaming/<service>/favorites/playlists sort en 400 \
             `unknown favorite type: playlists` : la lecture ne connait pas le type"
        );
        assert_eq!(
            t.unwrap().cle(),
            "playlists",
            "le client attend le tableau sous la cle `playlists`"
        );
    }

    /// La reponse de repli pour une source non-streaming (upnp/radio/podcast)
    /// doit porter la MEME cle que la reponse pleine, sans quoi le client lit
    /// `tracks` la ou il attend `playlists`.
    #[test]
    fn la_cle_de_repli_suit_le_type_demande() {
        for (demande, attendu) in [
            ("tracks", "tracks"),
            ("albums", "albums"),
            ("artists", "artists"),
            ("playlists", "playlists"),
        ] {
            let cle = TypeFavori::parse(demande)
                .map(TypeFavori::cle)
                .unwrap_or("tracks");
            assert_eq!(cle, attendu, "type demande: {demande}");
        }
    }

    /// Un type reellement inconnu reste inconnu : le correctif ouvre la
    /// playlist, il n'ouvre pas la porte a n'importe quelle chaine.
    #[test]
    fn un_type_inconnu_reste_refuse() {
        assert!(TypeFavori::parse("labels").is_none());
        assert!(
            TypeFavori::parse("track").is_none(),
            "le singulier n'est pas le contrat"
        );
        assert!(TypeFavori::parse("").is_none());
    }
}

#[cfg(test)]
mod tests_cache_editorial {
    use super::*;

    /// Une réponse éditorielle valide autorise le navigateur à la resservir.
    #[test]
    fn une_reponse_editoriale_valide_est_cachable() {
        let r: Result<serde_json::Value, String> = Ok(serde_json::json!({"albums": []}));
        let reponse = svc_response_editorial(r);
        assert_eq!(
            reponse
                .headers()
                .get(axum::http::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some(CACHE_EDITORIAL)
        );
    }

    /// Une ERREUR ne doit jamais être mise en cache : un 502 passager
    /// deviendrait une panne de trente minutes, et l'utilisateur n'aurait aucun
    /// moyen de la faire cesser.
    #[test]
    fn une_erreur_n_est_jamais_mise_en_cache() {
        let r: Result<serde_json::Value, String> = Err("upstream 502".into());
        let reponse = svc_response_editorial(r);
        assert!(
            reponse
                .headers()
                .get(axum::http::header::CACHE_CONTROL)
                .is_none(),
            "une erreur ne doit porter aucune politique de cache"
        );
    }

    /// `svc_response` — celui des favoris et des playlists de l'utilisateur —
    /// ne doit RIEN poser. C'est le middleware qui lui applique `no-cache`, et
    /// ce test fige la frontière : si quelqu'un ajoute un jour un en-tête ici
    /// « pour uniformiser », il échoue.
    #[test]
    fn la_reponse_ordinaire_ne_pose_aucune_politique() {
        let r: Result<serde_json::Value, String> = Ok(serde_json::json!([]));
        let reponse = svc_response(r);
        assert!(
            reponse
                .headers()
                .get(axum::http::header::CACHE_CONTROL)
                .is_none(),
            "les routes utilisateur doivent rester sous le no-cache du middleware"
        );
    }

    /// `private`, jamais `public` : ces routes sont derrière l'authentification.
    /// Même si le contenu est identique pour tous, un proxy partagé ne doit pas
    /// le stocker.
    #[test]
    fn le_cache_editorial_est_prive() {
        assert!(CACHE_EDITORIAL.starts_with("private"));
        assert!(!CACHE_EDITORIAL.contains("public"));
    }
}

/// #1621 — lenteur des playlists et favoris Qobuz pour les grosses
/// bibliothèques. Deux mécanismes serveur restaient à livrer après la
/// pagination concurrente (#1623) et le RwLock du registre (#2124) :
///
/// * le cache TTL des listes UTILISATEUR, purgé sur mutation (lot 5) ;
/// * l'aller des favoris sous verrou de LECTURE — le verrou d'écriture
///   sérialisait les trois requêtes parallèles du client (résidu du lot 6).
///
/// Service simulé en mémoire : AUCUN appel réseau. Le cache étant un
/// `static` partagé par tout le processus de test, chaque essai emploie un
/// nom de service qui n'appartient qu'à lui.
#[cfg(test)]
mod tests_cache_utilisateur {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tune_core::TuneError;
    use tune_core::db::sqlite::SqliteDb;
    use tune_core::streaming::traits::{
        AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack,
        StreamUrl,
    };

    /// Un connecteur qui compte ses lectures amont et sait les ralentir.
    struct ServiceCompteur {
        nom: String,
        lectures: Arc<AtomicUsize>,
        delai: Duration,
    }

    impl ServiceCompteur {
        fn lit(&self) {
            self.lectures.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl StreamingService for ServiceCompteur {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
        fn name(&self) -> &str {
            &self.nom
        }
        fn enabled(&self) -> bool {
            true
        }
        fn set_enabled(&mut self, _enabled: bool) {}
        async fn authenticate(&mut self, _c: &Value) -> Result<AuthStatus, TuneError> {
            Ok(AuthStatus::default())
        }
        async fn auth_status(&self) -> AuthStatus {
            AuthStatus::default()
        }
        async fn logout(&mut self) -> Result<(), TuneError> {
            Ok(())
        }
        async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
            Err("hors sujet".into())
        }
        async fn get_track(&self, _t: &str) -> Result<StreamTrack, TuneError> {
            Err("hors sujet".into())
        }
        async fn get_track_url(&self, _t: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
            Err("hors sujet".into())
        }
        async fn get_album(&self, _a: &str) -> Result<StreamAlbum, TuneError> {
            Err("hors sujet".into())
        }
        async fn get_album_tracks(&self, _a: &str) -> Result<Vec<StreamTrack>, TuneError> {
            Err("hors sujet".into())
        }
        async fn get_artist(&self, _a: &str) -> Result<StreamArtist, TuneError> {
            Err("hors sujet".into())
        }
        async fn get_playlist(&self, _p: &str) -> Result<StreamPlaylist, TuneError> {
            Err("hors sujet".into())
        }
        async fn get_playlist_tracks(&self, _p: &str) -> Result<Vec<StreamTrack>, TuneError> {
            Err("hors sujet".into())
        }
        async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
            self.lit();
            tokio::time::sleep(self.delai).await;
            Ok(vec![])
        }
        async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
            self.lit();
            tokio::time::sleep(self.delai).await;
            Ok(vec![])
        }
        async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
            self.lit();
            tokio::time::sleep(self.delai).await;
            Ok(vec![])
        }
        async fn get_user_tracks(&self) -> Result<Vec<StreamTrack>, TuneError> {
            self.lit();
            tokio::time::sleep(self.delai).await;
            Ok(vec![])
        }
        async fn add_favorite(&mut self, _f: &str, _i: &str) -> Result<(), TuneError> {
            Ok(())
        }
    }

    fn etat_essai(nom: &str, lectures: Arc<AtomicUsize>, delai: Duration) -> StreamingHttpState {
        let backend: Arc<dyn DbBackend> =
            Arc::new(SqliteDb::open_in_memory().expect("sqlite en memoire"));
        let mut registre = ServiceRegistry::new();
        registre.register(Box::new(ServiceCompteur {
            nom: nom.to_string(),
            lectures,
            delai,
        }));
        StreamingHttpState::new(
            backend,
            Arc::new(Mutex::new(registre)),
            Arc::new(EventBus::new()),
        )
    }

    /// Lot 5 : dans le TTL, la deuxième lecture ne repart PAS vers le service.
    /// C'était le « rechargement intégral » de l'issue : chaque retour dans la
    /// vue repaginait toute la bibliothèque de favoris.
    #[tokio::test]
    async fn dans_le_ttl_la_relecture_ne_retourne_pas_au_service() {
        let lectures = Arc::new(AtomicUsize::new(0));
        let etat = etat_essai("essai-ttl-relecture", lectures.clone(), Duration::ZERO);

        for _ in 0..2 {
            let r = service_playlists(
                State(etat.clone()),
                Path(String::from("essai-ttl-relecture")),
            )
            .await;
            assert_eq!(r.status(), StatusCode::OK);
        }
        assert_eq!(
            lectures.load(Ordering::SeqCst),
            1,
            "la deuxieme lecture dans le TTL doit etre servie du cache"
        );

        let r = service_favorites(
            State(etat.clone()),
            Path((String::from("essai-ttl-relecture"), String::from("tracks"))),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let r = service_favorites(
            State(etat),
            Path((String::from("essai-ttl-relecture"), String::from("tracks"))),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(
            lectures.load(Ordering::SeqCst),
            2,
            "les favoris aussi doivent etre servis du cache dans le TTL"
        );
    }

    /// Lot 5, invalidation : un favori ajouté purge le cache du service — la
    /// lecture suivante repart au service, elle ne ressort pas la liste d'avant
    /// la mutation. La purge passe par la ROUTE d'écriture, donc une
    /// souscription de playlist (#2370/#2765) la déclenchera aussi, quel que
    /// soit l'endpoint amont choisi par le connecteur.
    #[tokio::test]
    async fn une_mutation_de_favori_purge_le_cache_du_service() {
        let lectures = Arc::new(AtomicUsize::new(0));
        let etat = etat_essai("essai-purge-mutation", lectures.clone(), Duration::ZERO);

        let r = service_playlists(
            State(etat.clone()),
            Path(String::from("essai-purge-mutation")),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);

        let r = service_add_favorite(
            State(etat.clone()),
            Path((
                String::from("essai-purge-mutation"),
                String::from("tracks"),
                String::from("42"),
            )),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);

        let r = service_playlists(State(etat), Path(String::from("essai-purge-mutation"))).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(
            lectures.load(Ordering::SeqCst),
            2,
            "apres une mutation, la lecture doit repartir au service"
        );
    }

    /// La purge est PAR SERVICE : muter Qobuz ne jette pas le cache de Tidal.
    #[test]
    fn la_purge_ne_touche_que_le_service_mute() {
        memoriser_contenu_utilisateur("essai-purge-a", "playlists", json!([1]));
        memoriser_contenu_utilisateur("essai-purge-b", "playlists", json!([2]));
        purge_contenu_utilisateur("essai-purge-a");
        assert!(contenu_utilisateur_frais("essai-purge-a", "playlists").is_none());
        assert_eq!(
            contenu_utilisateur_frais("essai-purge-b", "playlists"),
            Some(json!([2]))
        );
    }

    /// Une entrée plus vieille que le TTL n'est jamais servie.
    #[test]
    fn une_entree_perimee_n_est_pas_servie() {
        let Some(passe) =
            Instant::now().checked_sub(TTL_CONTENU_UTILISATEUR + Duration::from_secs(1))
        else {
            // Horloge trop jeune pour reculer d'autant : rien à prouver ici.
            return;
        };
        cache_contenu_utilisateur()
            .lock()
            .expect("verrou d'essai")
            .insert(
                (String::from("essai-ttl-perime"), String::from("playlists")),
                EntreeUtilisateur {
                    cree: passe,
                    donnees: json!([1]),
                },
            );
        assert!(
            contenu_utilisateur_frais("essai-ttl-perime", "playlists").is_none(),
            "une entree perimee doit etre ignoree"
        );
    }

    /// Résidu du lot 6 : les trois types de favoris demandés EN PARALLÈLE par
    /// le client doivent se charger en parallèle. L'ancien verrou d'écriture
    /// les sérialisait : temps total = somme des trois lectures amont.
    ///
    /// Trois lectures de 200 ms : en parallèle ≈ 200 ms, sérialisées = 600 ms.
    /// Le seuil de 450 ms distingue nettement les deux régimes tout en tolérant
    /// un ordonnanceur chargé.
    #[tokio::test]
    async fn les_trois_types_de_favoris_se_chargent_en_parallele() {
        let lectures = Arc::new(AtomicUsize::new(0));
        let etat = etat_essai(
            "essai-parallele",
            lectures.clone(),
            Duration::from_millis(200),
        );

        let depart = Instant::now();
        let (a, b, c) = tokio::join!(
            service_favorites(
                State(etat.clone()),
                Path((String::from("essai-parallele"), String::from("tracks"))),
            ),
            service_favorites(
                State(etat.clone()),
                Path((String::from("essai-parallele"), String::from("albums"))),
            ),
            service_favorites(
                State(etat),
                Path((String::from("essai-parallele"), String::from("artists"))),
            ),
        );
        let duree = depart.elapsed();

        assert_eq!(a.status(), StatusCode::OK);
        assert_eq!(b.status(), StatusCode::OK);
        assert_eq!(c.status(), StatusCode::OK);
        assert_eq!(lectures.load(Ordering::SeqCst), 3);
        assert!(
            duree < Duration::from_millis(450),
            "trois lectures de 200 ms sous verrou de lecture doivent se \
             recouvrir (~200 ms), pas se suivre (600 ms). Mesure : {duree:?}"
        );
    }

    /// Le logout purge : les listes d'un compte ne survivent pas à sa session.
    #[tokio::test]
    async fn le_logout_purge_le_cache_du_service() {
        let lectures = Arc::new(AtomicUsize::new(0));
        let etat = etat_essai("essai-purge-logout", lectures.clone(), Duration::ZERO);

        let r = service_playlists(
            State(etat.clone()),
            Path(String::from("essai-purge-logout")),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        assert!(contenu_utilisateur_frais("essai-purge-logout", "playlists").is_some());

        let r = service_logout(State(etat), Path(String::from("essai-purge-logout"))).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert!(
            contenu_utilisateur_frais("essai-purge-logout", "playlists").is_none(),
            "apres logout, aucune liste de l'ancien compte ne doit rester servable"
        );
    }
}
