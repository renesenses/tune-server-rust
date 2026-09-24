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
use tune_core::favorites_sort::TriFavoris;
use tune_core::streaming::ServiceRegistry;
use tune_core::streaming::traits::StreamingService;

pub mod deezer_proxy_handler;
pub mod etiquettes_langue;

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

/// Le statut HTTP que l'erreur PORTE, ou `None` quand elle ne dit rien de plus
/// que « ça a échoué » — l'appelant garde alors son statut par défaut.
///
/// # Le défaut (#859)
///
/// [`svc_response`] sortait **tout** `Err` en `502 BAD_GATEWAY`. Or 502
/// signifie précisément « la passerelle en amont est en panne ». Mesuré sur le
/// .18 le 12/09/2026, trois essais sur trois :
///
/// ```text
/// GET /api/v1/streaming/bandcamp/playlists → 502 en 5,1 / 8,5 / 4,6 ms
/// corps : « Bandcamp ne fournit pas de playlists »
/// ```
///
/// Quatre à huit millisecondes : aucun aller-retour réseau n'a eu lieu.
/// Ce n'est pas une passerelle en panne, c'est `get_user_playlists` qui refuse
/// délibérément — et le refus était maquillé en panne d'infrastructure. Un
/// testeur qui lit « 502 Bad Gateway » signale une panne serveur ; on cherche
/// une passerelle, un réseau, un service tiers, pour un serveur qui a
/// simplement dit non.
///
/// # Pourquoi une variante et pas le message
///
/// Le message est libre et traduit ; le lire pour décider d'un statut
/// redériverait à la première reformulation. C'est
/// [`TuneError::Unsupported`] — posée par les refus délibérés eux-mêmes — qui
/// porte l'information, et elle seule.
///
/// # Pourquoi 501 et pas 400
///
/// La requête était RECEVABLE : bien formée, sur une route qui existe, pour un
/// service qui existe. Ce n'est donc pas un `400`. C'est le serveur qui
/// n'implémente pas la fonctionnalité pour ce service — la définition même du
/// `501` (RFC 9110 §15.6.2).
///
/// # Ce qui NE bouge pas
///
/// Tout le reste — réseau injoignable, JSON illisible, erreur rendue par le
/// service, cas inconnu — reste une panne d'amont. `None` ici, et l'appelant
/// garde son 502. Remplacer un mensonge par un autre n'aurait rien réparé.
fn statut_porte_par_l_erreur(e: &tune_core::TuneError) -> Option<StatusCode> {
    match e {
        tune_core::TuneError::Unsupported(_) => Some(StatusCode::NOT_IMPLEMENTED),
        _ => None,
    }
}

#[derive(Clone)]
struct StreamingFailure {
    kind: &'static str,
    message: String,
}

/// Convert a service method result into a JSON response (OK -> 200, Err ->
/// 502, sauf refus délibéré -> 501 ; voir [`statut_porte_par_l_erreur`]).
///
/// Le paramètre d'erreur est `TuneError` et non plus un `E: Display` : c'est
/// ce qui permet de lire la VARIANTE au lieu de deviner sur le texte. Les 28
/// gestionnaires qui passent par ici rendent tous déjà un `TuneError`.
fn svc_response<R: serde::Serialize>(result: Result<R, tune_core::TuneError>) -> Response {
    match result {
        Ok(data) => Json(json!(data)).into_response(),
        Err(e) => {
            let status = statut_porte_par_l_erreur(&e).unwrap_or(StatusCode::BAD_GATEWAY);
            let error_kind = match &e {
                tune_core::TuneError::Io(_) => "io",
                tune_core::TuneError::Db(_) => "db",
                tune_core::TuneError::Streaming(_) => "streaming",
                tune_core::TuneError::Audio(_) => "audio",
                tune_core::TuneError::Network(_) => "network",
                tune_core::TuneError::Json(_) => "json",
                tune_core::TuneError::NotFound(_) => "not_found",
                tune_core::TuneError::Config(_) => "config",
                tune_core::TuneError::Unsupported(_) => "unsupported",
                tune_core::TuneError::Other(_) => "other",
            };
            let message = e.to_string();
            let mut response = (status, message.clone()).into_response();
            response.extensions_mut().insert(StreamingFailure {
                kind: error_kind,
                message,
            });
            response
        }
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
/// Comme [`svc_response_editorial`], mais le corps est d'abord relu dans la
/// langue de la requête : tout `name` accompagné d'un `name_i18n` prend le
/// libellé de cette langue (voir `etiquettes_langue`).
///
/// `Vary: Accept-Language` est posé EN MÊME TEMPS que `Cache-Control`. Sans
/// lui, le cache navigateur de trente minutes resservirait à un lecteur
/// roumain la copie française mise en cache par la visite précédente — le
/// défaut corrigé ici réapparaîtrait par le cache.
fn svc_response_editorial_localise<R: serde::Serialize>(
    result: Result<R, tune_core::TuneError>,
    langues: &[String],
) -> Response {
    let result = result.and_then(|valeur| {
        let mut corps =
            serde_json::to_value(valeur).map_err(|e| tune_core::TuneError::Other(e.to_string()))?;
        etiquettes_langue::localiser(&mut corps, langues);
        Ok(corps)
    });
    let est_ok = result.is_ok();
    let mut response = svc_response_editorial(result);
    if est_ok {
        response.headers_mut().insert(
            axum::http::header::VARY,
            axum::http::HeaderValue::from_static("Accept-Language"),
        );
    }
    response
}

fn svc_response_editorial<R: serde::Serialize>(
    result: Result<R, tune_core::TuneError>,
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
///
/// 🔴 `pub` depuis le 21/09/2026. La fusion et la suppression de playlists
/// vivent dans `tune-server` (`playlist_manager.rs`) et appellent le service
/// SANS passer par les routes d'ici : elles ne purgeaient donc rien, et la
/// liste rendue restait la mémorisée — jusqu'à 2 minutes. Bertrand : « Je ne
/// vois pas la playlist résultant du merge ! ». Elle existait chez Qobuz.
pub fn purge_contenu_utilisateur(service: &str) {
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
    /// Curseur, en éléments et PAR CATÉGORIE (#2160). Absent = première page,
    /// ce que tous les clients antérieurs envoient.
    offset: Option<usize>,
}

/// Journalise la cause attachée par le convertisseur commun à la réponse.
/// Les champs restent présents même si le filtre de logs n'accepte que WARN ;
/// le contexte ne dépend pas de l'activation d'un span INFO (#4039).
async fn contexte_diagnostic_streaming(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|p| p.as_str())
        .unwrap_or("unknown")
        .to_owned();
    // Router::nest peut retirer les préfixes de l'URI interne : aligner les
    // segments depuis la fin conserve le bon service sans lire la query.
    let service = route
        .rsplit('/')
        .zip(request.uri().path().rsplit('/'))
        .find_map(|(pattern, value)| (pattern == "{service}").then_some(value))
        .unwrap_or("unknown")
        .to_owned();
    let method = request.method().clone();
    let response = next.run(request).await;
    if let Some(failure) = response.extensions().get::<StreamingFailure>() {
        tracing::warn!(service, route, %method, status = response.status().as_u16(),
            error_kind = failure.kind, error = %failure.message, "streaming_service_error");
    }
    response
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
        .route(
            "/{service}/tracks/{track_id}/similar",
            get(service_track_similar),
        )
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
        .route_layer(axum::middleware::from_fn(contexte_diagnostic_streaming))
}

// ---------------------------------------------------------------------------
// Simple read-only handlers (via with_svc!)

// ---------------------------------------------------------------------------

/// `?limit=` est un nombre d'éléments PAR CATÉGORIE (albums, artistes, titres,
/// playlists), pas un total. `limit=0` demande « Tous », à la charge du service
/// de le borner — la recherche Qobuz pagine et s'arrête au plafond documenté
/// dans `qobuz.rs` (#2160). Absent, la valeur reste 20, ce que les clients
/// antérieurs obtenaient déjà.
///
/// `?offset=` est le curseur d'un « Charger plus » : le rang, par catégorie, du
/// premier élément voulu. Absent = 0.
///
/// La réponse ajoute `offset`, `totals`, `has_more` et `truncated` À CÔTÉ des
/// quatre clés existantes — un client antérieur lit `.albums` comme avant.
/// `truncated` est ce qui empêche de prendre un « Tous » borné à 500 pour un
/// catalogue épuisé.
///
/// **Pas de cache ici, et c'est délibéré.** Le cache de 120 s des listes
/// utilisateur (#2818) est indexé par `(service, ressource)` : une clé de
/// recherche devrait porter la requête, la limite ET le décalage, soit une
/// entrée par frappe de clavier et par page. On ne mémorise donc rien, comme
/// avant.
async fn service_search(
    State(state): State<StreamingHttpState>,
    Path(service): Path<String>,
    Query(q): Query<SearchQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(20);
    let offset = q.offset.unwrap_or(0);
    with_svc!(&state, &service, |svc| svc
        .search_page(&q.q, limit, offset)
        .await)
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
        .await
        .map(dedoublonner_titres_phares))
}

/// Écart de durée sous lequel deux titres phares de même nom et de même
/// interprète sont tenus pour le MÊME enregistrement — la même tolérance que
/// `POINTS_DUREE_QUASI_EGALE` du rapprochement des versions (« le même master,
/// ou son remaster »).
const TOLERANCE_DUREE_TITRES_PHARES_MS: u64 = 2_000;

/// #4444 — FabienM (fil 1839, point 11) : sur la page artiste de Cat Power,
/// « Try Me » (2:19) sort aux rangs 1 ET 4 des TITRES PHARES, sur les DEUX
/// présentations de la page — le doublon est donc en amont du rendu, ici.
///
/// Qobuz (`artist/get?extra=tracks`) rend le même enregistrement une fois par
/// ÉDITION de l'album qui le porte : même titre, même interprète, même durée,
/// même pochette, deux identifiants de piste. Une liste de titres phares est
/// une liste de MORCEAUX ; deux éditions du même morceau y sont une place
/// perdue. On garde la première occurrence — l'ordre est celui du service,
/// par popularité.
///
/// Deux clefs, dans l'ordre : l'identifiant (le même id deux fois est un
/// doublon quelle que soit sa fiche), puis (titre, interprète) à la casse
/// près avec une durée à [`TOLERANCE_DUREE_TITRES_PHARES_MS`] près. Une durée
/// inconnue (0) ne rapproche rien : elle n'est pas un signal.
pub(crate) fn dedoublonner_titres_phares(
    pistes: Vec<tune_core::streaming::traits::StreamTrack>,
) -> Vec<tune_core::streaming::traits::StreamTrack> {
    let mut ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut gardees: Vec<tune_core::streaming::traits::StreamTrack> =
        Vec::with_capacity(pistes.len());
    for piste in pistes {
        if !piste.id.is_empty() && !ids.insert(piste.id.clone()) {
            continue;
        }
        let meme_morceau = |g: &tune_core::streaming::traits::StreamTrack| {
            g.duration_ms > 0
                && piste.duration_ms > 0
                && g.duration_ms.abs_diff(piste.duration_ms) <= TOLERANCE_DUREE_TITRES_PHARES_MS
                && g.title.trim().eq_ignore_ascii_case(piste.title.trim())
                && g.artist.trim().eq_ignore_ascii_case(piste.artist.trim())
        };
        if gardees.iter().any(meme_morceau) {
            continue;
        }
        gardees.push(piste);
    }
    gardees
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

#[derive(Deserialize)]
struct GenreAlbumsQuery {
    limit: Option<usize>,
    /// Rubrique éditoriale à restreindre au genre (#3481) : un identifiant de
    /// `/{service}/featured/sections` (`press-awards`, `ideal-discography`…).
    /// Absente : les nouveautés du genre, ce que tous les clients d'avant
    /// reçoivent.
    section: Option<String>,
}

async fn service_genre_albums(
    State(state): State<StreamingHttpState>,
    Path((service, genre_id)): Path<(String, String)>,
    Query(q): Query<GenreAlbumsQuery>,
) -> Response {
    let limit = q.limit.unwrap_or(50);
    match q.section.as_deref() {
        None => with_svc_editorial!(&state, &service, |svc| svc
            .get_genre_albums(&genre_id, limit)
            .await),
        Some(section) => with_svc_editorial!(&state, &service, |svc| svc
            .get_genre_section(&genre_id, section, limit)
            .await),
    }
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
    headers: axum::http::HeaderMap,
) -> Response {
    let langues = etiquettes_langue::langues_demandees(&headers);
    let arc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let svc = arc.read().await;
    svc_response_editorial_localise(svc.get_playlist_tags().await, &langues)
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
    headers: axum::http::HeaderMap,
) -> Response {
    let langues = etiquettes_langue::langues_demandees(&headers);
    let arc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    let svc = arc.read().await;
    svc_response_editorial_localise(
        svc.get_featured_playlists_by_tag(q.genre.as_deref()).await,
        &langues,
    )
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

/// Nombre de titres rendus par défaut par « Plus comme ça » sur un titre de
/// service : un titre par artiste voisin, comme la radio.
const PLUS_COMME_CA_PAR_DEFAUT: usize = 20;
/// Plafond de `?limit=`. Chaque titre coûte un appel au service (les titres
/// phares d'un voisin), faits l'un après l'autre : au-delà, le clic attend.
const PLUS_COMME_CA_PLAFOND: usize = 50;

#[derive(Deserialize)]
struct SimilairesQuery {
    limit: Option<usize>,
}

/// Le nombre de titres réellement demandé : absent → le défaut, `0` → au
/// moins un, trop grand → le plafond.
fn borne_plus_comme_ca(limit: Option<usize>) -> usize {
    limit
        .unwrap_or(PLUS_COMME_CA_PAR_DEFAUT)
        .clamp(1, PLUS_COMME_CA_PLAFOND)
}

/// `GET /{service}/tracks/{track_id}/similar` — « Plus comme ça » sur un titre
/// de service. Fil 1906 (FabienM), point 3.
///
/// Même algorithme que la reprise automatique de fin de file (voir
/// `tune_core::playback::auto_dj::pistes_similaires_du_service`) : l'artiste
/// du titre, ses voisins, un titre phare par voisin, le titre source exclu.
/// La réponse est une liste de pistes au format des autres routes streaming
/// (`StreamTrack`), que le client lit ou enfile comme d'habitude.
///
/// Réponses :
///  - 404 : service inconnu (comme toutes les routes `/{service}/…`) ;
///  - 501 : le service ne connaît pas ses artistes similaires — aujourd'hui,
///    tous sauf Qobuz. Un refus DIT, pas une liste vide qui laisserait croire
///    à un artiste sans voisin ;
///  - 502 : le titre source n'a pas pu être lu chez le service ;
///  - 200 `[]` : aucun voisin trouvé — une réponse, pas une panne.
///
/// Titres bannis : la radio n'en exclut aucun sur cette base (la fonction
/// `banned` de #4818 n'y est pas) ; la route suit la radio.
async fn service_track_similar(
    State(state): State<StreamingHttpState>,
    Path((service, track_id)): Path<(String, String)>,
    Query(q): Query<SimilairesQuery>,
) -> Response {
    let arc = match get_svc(&state, &service).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    // Le verrou de lecture est RELÂCHÉ avant la recherche des voisins, qui
    // reprend le sien à chaque appel : le garder ici laisserait un écrivain en
    // attente (rafraîchissement de jeton) bloquer toute la requête.
    let source = {
        let svc = arc.read().await;
        if !svc.propose_des_artistes_similaires() {
            return svc_response::<Vec<tune_core::streaming::traits::StreamTrack>>(Err(
                tune_core::TuneError::Unsupported(format!(
                    "{service} ne fournit pas d'artistes similaires : « Plus comme ça » \
                     n'est pas disponible pour ce service"
                )),
            ));
        }
        match svc.get_track(&track_id).await {
            Ok(piste) => piste,
            Err(e) => {
                return svc_response::<Vec<tune_core::streaming::traits::StreamTrack>>(Err(e));
            }
        }
    };
    let borne = borne_plus_comme_ca(q.limit);
    let noms =
        tune_core::playback::auto_dj::similar_artist_names(&state.backend, &source.artist, borne)
            .await;
    let mut exclure: std::collections::HashSet<String> = std::collections::HashSet::new();
    exclure.insert(track_id.clone());
    if !source.id.is_empty() {
        exclure.insert(source.id.clone());
    }
    let similaires = tune_core::playback::auto_dj::pistes_similaires_du_service(
        &arc,
        &source.artist,
        source.artist_id.as_deref(),
        noms,
        borne,
        borne,
        &exclure,
    )
    .await;
    tracing::info!(
        service = %service,
        track_id = %track_id,
        candidats = similaires.candidats,
        depuis_enrichissement = similaires.depuis_enrichissement,
        pistes = similaires.pistes.len(),
        "plus_comme_ca_service"
    );
    svc_response(Ok(similaires.pistes))
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
    // #3489 — Didier (forum 1666) : « trié sur Date d'ajout : l'ordre n'est pas
    // respecté ; je change le sens avec la petite flèche : rien ne change ; "Par
    // défaut" et "Date d'ajout" affichent la même chose ». Les trois symptômes
    // n'en font qu'un : la réponse ne transportait AUCUNE date, donc la clé de
    // tri du client valait la chaîne vide pour toutes les entrées.
    //
    // C'est ICI que le correctif se branche. `get_user_favorites_dated` peut
    // rester parfaite dans Qobuz et Tidal : sans cette ligne, personne ne
    // l'appelle et la route sert exactement ce qu'elle servait avant.
    //
    // `Ok(None)` = « ce connecteur n'a pas de date à ajouter » et non « pas de
    // favoris » : on reprend alors le dispatch typé, inchangé. Les connecteurs
    // qui ne surchargent rien — Deezer, Spotify, YouTube, Amazon, Bandcamp —
    // passent donc par le chemin d'avant, octet pour octet.
    if let Some(items) = svc.get_user_favorites_dated(fav_type).await? {
        let cle = TypeFavori::parse(fav_type)
            .map(TypeFavori::cle)
            .ok_or_else(|| format!("unknown favorite type: {fav_type}"))?;
        let mut reponse = serde_json::Map::new();
        reponse.insert(cle.to_string(), Value::Array(items));
        return Ok(Value::Object(reponse));
    }
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

/// `sort` / `order` sur la liste des favoris d'un service (#2001).
///
/// Facultatifs : absents, la réponse est celle d'avant, dans l'ordre du
/// service.
#[derive(Deserialize)]
struct TriQuery {
    sort: Option<String>,
    order: Option<String>,
}

/// Pose le tri demandé sur une réponse de favoris, **après** le cache.
///
/// Le cache de contenu utilisateur (#1621, PR #2818) mémorise la réponse brute
/// du service sous la clé `(service, "favorites/{type}")`. Trier ici, sur la
/// copie qui part au client, laisse cette clé intacte : quatre tris successifs
/// se servent de la même entrée au lieu d'en fabriquer quatre, et aucune des
/// douze purges n'a de raison de changer.
fn poser_le_tri(mut donnees: Value, q: &TriQuery) -> Value {
    if let Some(tri) = TriFavoris::depuis(q.sort.as_deref(), q.order.as_deref()) {
        tune_core::favorites_sort::trier_liste_json(&mut donnees, tri);
    }
    donnees
}

async fn service_favorites(
    State(state): State<StreamingHttpState>,
    Path((service, fav_type)): Path<(String, String)>,
    Query(tri): Query<TriQuery>,
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
        return Json(poser_le_tri(donnees, &tri)).into_response();
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
            // Le cache reçoit la réponse BRUTE ; le tri ne porte que sur la
            // copie rendue au client.
            memoriser_contenu_utilisateur(&service, &ressource, data.clone());
            Json(poser_le_tri(data, &tri)).into_response()
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
                        Json(poser_le_tri(data, &tri)).into_response()
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
        let r: Result<serde_json::Value, tune_core::TuneError> =
            Ok(serde_json::json!({"albums": []}));
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
        let r: Result<serde_json::Value, tune_core::TuneError> =
            Err(tune_core::TuneError::Streaming("upstream 502".into()));
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
        let r: Result<serde_json::Value, tune_core::TuneError> = Ok(serde_json::json!([]));
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
        AuthStatus, SearchPage, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist,
        StreamTrack, StreamUrl,
    };

    fn piste(id: &str, titre: &str, artiste: &str) -> StreamTrack {
        StreamTrack {
            id: id.to_string(),
            title: titre.to_string(),
            artist: artiste.to_string(),
            album: None,
            album_id: None,
            duration_ms: 0,
            cover_path: None,
            track_number: None,
            disc_number: None,
            explicit: false,
            disponible: None,
            quality: None,
            isrc: None,
            composer: None,
            artist_id: None,
        }
    }

    /// Ce que la route a passé à `search_page` : (requête, limite, décalage).
    /// #2160 — sert de témoin au module `tests_route_recherche`.
    pub(super) type RecherchesVues = Arc<std::sync::Mutex<Vec<(String, usize, usize)>>>;

    /// Un connecteur qui compte ses lectures amont et sait les ralentir.
    pub(super) struct ServiceCompteur {
        pub(super) nom: String,
        pub(super) lectures: Arc<AtomicUsize>,
        pub(super) delai: Duration,
        pub(super) recherches: RecherchesVues,
        /// #3489 — la date brute que ce connecteur prétend porter sur ses
        /// favoris d'album, ou `None` pour un connecteur qui n'en transporte
        /// aucune. `None` par défaut : les essais d'avant ne changent pas.
        pub(super) date_brute: Option<String>,
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
        /// Note ce que la route a réellement transmis, puis rend une page vide.
        async fn search_page(
            &self,
            query: &str,
            limit: usize,
            offset: usize,
        ) -> Result<SearchPage, TuneError> {
            self.recherches.lock().expect("verrou d'essai").push((
                query.to_string(),
                limit,
                offset,
            ));
            Ok(SearchPage::page_unique(SearchResults {
                tracks: vec![],
                albums: vec![],
                artists: vec![],
                playlists: vec![],
            }))
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
        /// Trois pistes dans un ordre qu'aucun tri ne rend par hasard : le
        /// service les donne « 10, 2, Zorro », l'ordre d'ajout que #2001
        /// reproche.
        async fn get_user_tracks(&self) -> Result<Vec<StreamTrack>, TuneError> {
            self.lit();
            tokio::time::sleep(self.delai).await;
            Ok(vec![
                piste("t1", "Volume 10", "Éric Zimmer"),
                piste("t2", "volume 2", "aaron Zed"),
                piste("t3", "Zorro", "Erik Satie"),
            ])
        }
        async fn add_favorite(&mut self, _f: &str, _i: &str) -> Result<(), TuneError> {
            Ok(())
        }

        /// #4444 — ce que Qobuz rend pour Cat Power : le même « Try Me »
        /// (2:19) sous deux identifiants — deux éditions de l'album —, et
        /// un troisième doublon par identifiant strict. Les vrais morceaux
        /// distincts restent dans l'ordre du service.
        async fn get_artist_top_tracks(&self, _a: &str) -> Result<Vec<StreamTrack>, TuneError> {
            let mut try_me = piste("q-1", "Try Me", "Cat Power");
            try_me.duration_ms = 139_000;
            let mut could_we = piste("q-2", "Could We", "Cat Power");
            could_we.duration_ms = 131_000;
            let mut nothing = piste("q-3", "Nothing Compares 2 U", "Cat Power");
            nothing.duration_ms = 357_000;
            let mut try_me_bis = piste("q-4", "Try Me", "Cat Power");
            try_me_bis.duration_ms = 139_000;
            let mut try_me_ter = piste("q-1", "Try Me", "Cat Power");
            try_me_ter.duration_ms = 139_000;
            // Même titre, mais une AUTRE prise : 40 s de plus, elle reste.
            let mut try_me_live = piste("q-5", "Try Me", "Cat Power");
            try_me_live.duration_ms = 179_000;
            Ok(vec![
                try_me,
                could_we,
                nothing,
                try_me_bis,
                try_me_ter,
                try_me_live,
            ])
        }

        /// #3489 — un connecteur qui date ses albums, comme Qobuz et Tidal.
        /// `None` quand `date_brute` est vide : c'est le repli que doivent
        /// suivre tous les connecteurs qui ne surchargent rien.
        async fn get_user_favorites_dated(
            &self,
            fav_type: &str,
        ) -> Result<Option<Vec<Value>>, TuneError> {
            let Some(date) = self.date_brute.as_deref() else {
                return Ok(None);
            };
            if fav_type != "albums" {
                return Ok(None);
            }
            self.lit();
            let brut = json!({ "created": date });
            let mut element = json!({"source_id": "a1", "title": "Live with the Orchestra"});
            tune_core::streaming::favorites_date::greffer_created_at(
                &mut element,
                &brut,
                &["created"],
            );
            Ok(Some(vec![element]))
        }

        /// #3481 — les nouveautés d'un genre, reconnaissables à leur titre.
        async fn get_genre_albums(
            &self,
            genre_id: &str,
            _limit: usize,
        ) -> Result<Vec<StreamAlbum>, TuneError> {
            Ok(vec![StreamAlbum {
                id: "n1".into(),
                title: format!("nouveautes|{genre_id}"),
                ..StreamAlbum::default()
            }])
        }

        /// #3481 — une rubrique d'un genre : le titre dit ce qui est arrivé
        /// au connecteur (rubrique, genre, limite).
        async fn get_genre_section(
            &self,
            genre_id: &str,
            section_id: &str,
            limit: usize,
        ) -> Result<Vec<StreamAlbum>, TuneError> {
            Ok(vec![StreamAlbum {
                id: "r1".into(),
                title: format!("{section_id}|{genre_id}|{limit}"),
                ..StreamAlbum::default()
            }])
        }
    }

    /// #4444 — la route des titres phares ne rend pas deux fois le même
    /// morceau. Le service en donne six lignes dont trois « Try Me » 2:19 ;
    /// l'écran doit en voir un seul, à son rang, et garder la prise live.
    #[tokio::test]
    async fn les_titres_phares_ne_portent_pas_deux_fois_le_meme_morceau() {
        let etat = etat_essai(
            "essai-titres-phares",
            Arc::new(AtomicUsize::new(0)),
            Duration::ZERO,
        );
        let r = service_artist_top_tracks(
            State(etat),
            Path((
                String::from("essai-titres-phares"),
                String::from("cat-power"),
            )),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let octets = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        let v: Value = serde_json::from_slice(&octets).unwrap();
        let titres: Vec<(String, u64)> = v
            .as_array()
            .expect("une liste de pistes")
            .iter()
            .map(|p| {
                (
                    p["title"].as_str().unwrap_or_default().to_string(),
                    p["duration_ms"].as_u64().unwrap_or(0),
                )
            })
            .collect();
        assert_eq!(
            titres,
            vec![
                ("Try Me".to_string(), 139_000),
                ("Could We".to_string(), 131_000),
                ("Nothing Compares 2 U".to_string(), 357_000),
                ("Try Me".to_string(), 179_000),
            ],
            "« Try Me » 2:19 doit sortir UNE fois, au rang 1 ; la prise de 2:59 est un autre enregistrement (#4444)"
        );
    }

    fn etat_essai(nom: &str, lectures: Arc<AtomicUsize>, delai: Duration) -> StreamingHttpState {
        etat_essai_complet(nom, lectures, delai, RecherchesVues::default())
    }

    /// Un état dont le connecteur date — ou non — ses favoris d'album (#3489).
    pub(super) fn etat_essai_date(nom: &str, date_brute: Option<&str>) -> StreamingHttpState {
        let backend: Arc<dyn DbBackend> =
            Arc::new(SqliteDb::open_in_memory().expect("sqlite en memoire"));
        let mut registre = ServiceRegistry::new();
        registre.register(Box::new(ServiceCompteur {
            nom: nom.to_string(),
            lectures: Arc::new(AtomicUsize::new(0)),
            delai: Duration::ZERO,
            recherches: RecherchesVues::default(),
            date_brute: date_brute.map(str::to_string),
        }));
        StreamingHttpState::new(
            backend,
            Arc::new(Mutex::new(registre)),
            Arc::new(EventBus::new()),
        )
    }

    pub(super) fn etat_essai_complet(
        nom: &str,
        lectures: Arc<AtomicUsize>,
        delai: Duration,
        recherches: RecherchesVues,
    ) -> StreamingHttpState {
        let backend: Arc<dyn DbBackend> =
            Arc::new(SqliteDb::open_in_memory().expect("sqlite en memoire"));
        let mut registre = ServiceRegistry::new();
        registre.register(Box::new(ServiceCompteur {
            nom: nom.to_string(),
            lectures,
            delai,
            recherches,
            date_brute: None,
        }));
        StreamingHttpState::new(
            backend,
            Arc::new(Mutex::new(registre)),
            Arc::new(EventBus::new()),
        )
    }

    /// Aucun tri demandé — le cas de tous les appels d'avant #2001.
    fn sans_tri() -> Query<TriQuery> {
        Query(TriQuery {
            sort: None,
            order: None,
        })
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
            sans_tri(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let r = service_favorites(
            State(etat),
            Path((String::from("essai-ttl-relecture"), String::from("tracks"))),
            sans_tri(),
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
                sans_tri(),
            ),
            service_favorites(
                State(etat.clone()),
                Path((String::from("essai-parallele"), String::from("albums"))),
                sans_tri(),
            ),
            service_favorites(
                State(etat),
                Path((String::from("essai-parallele"), String::from("artists"))),
                sans_tri(),
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

    /// Les identifiants des favoris rendus par la route, dans l'ordre.
    async fn identifiants(
        etat: &StreamingHttpState,
        service: &str,
        sort: Option<&str>,
        order: Option<&str>,
    ) -> Vec<String> {
        let r = service_favorites(
            State(etat.clone()),
            Path((service.to_string(), String::from("tracks"))),
            Query(TriQuery {
                sort: sort.map(str::to_string),
                order: order.map(str::to_string),
            }),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&octets).unwrap();
        v["tracks"]
            .as_array()
            .expect("la reponse porte une liste `tracks`")
            .iter()
            .map(|t| t["source_id"].as_str().unwrap().to_string())
            .collect()
    }

    /// #2001 : le tri se pose APRÈS le cache de #1621/#2818.
    ///
    /// Deux choses à la fois, et c'est voulu : quatre tris différents ne font
    /// qu'une seule lecture amont — la clé `(service, ressource)` ne porte pas
    /// le tri — et la lecture SANS tri, faite en dernier, ressort l'ordre du
    /// service : le cache a bien mémorisé la réponse brute, pas une réponse
    /// déjà rangée.
    #[tokio::test]
    async fn le_tri_des_favoris_se_pose_apres_le_cache_sans_le_multiplier() {
        let lectures = Arc::new(AtomicUsize::new(0));
        let etat = etat_essai("essai-tri-favoris", lectures.clone(), Duration::ZERO);
        let svc = "essai-tri-favoris";

        // « volume 2 » avant « Volume 10 » : tri naturel, casse ignoree.
        assert_eq!(
            identifiants(&etat, svc, Some("title"), Some("asc")).await,
            ["t2", "t1", "t3"]
        );
        assert_eq!(
            identifiants(&etat, svc, Some("title"), Some("desc")).await,
            ["t3", "t1", "t2"]
        );
        // « Éric Zimmer » se range entre « aaron Zed » et « Erik Satie ».
        assert_eq!(
            identifiants(&etat, svc, Some("artist"), Some("asc")).await,
            ["t2", "t1", "t3"]
        );
        // Une cle inconnue ne trie pas et ne casse rien.
        assert_eq!(
            identifiants(&etat, svc, Some("bpm"), Some("asc")).await,
            ["t1", "t2", "t3"]
        );
        // Sans parametre : l'ordre du service, intact dans le cache.
        assert_eq!(
            identifiants(&etat, svc, None, None).await,
            ["t1", "t2", "t3"]
        );

        assert_eq!(
            lectures.load(Ordering::SeqCst),
            1,
            "cinq lectures, un seul aller au service : la cle du cache ne doit \
             pas porter le tri"
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

/// #2160 — le curseur et la taille de page de la recherche traversent la route.
///
/// La #2754 a livré la pagination interne du connecteur Qobuz ; ce module
/// vérifie l'autre moitié serveur : que `?limit=` et `?offset=` arrivent bien
/// jusqu'au service, et que leur absence rend exactement ce que rendaient les
/// clients d'avant.
#[cfg(test)]
mod tests_route_recherche {
    use super::tests_cache_utilisateur::{RecherchesVues, etat_essai_complet};
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn requete(q: &str, limit: Option<usize>, offset: Option<usize>) -> Query<SearchQuery> {
        Query(SearchQuery {
            q: q.to_string(),
            limit,
            offset,
        })
    }

    async fn appelle(
        nom: &str,
        requete: Query<SearchQuery>,
    ) -> (StatusCode, Vec<(String, usize, usize)>) {
        let vues = RecherchesVues::default();
        let etat = etat_essai_complet(
            nom,
            Arc::new(AtomicUsize::new(0)),
            Duration::ZERO,
            vues.clone(),
        );
        let r = service_search(State(etat), Path(nom.to_string()), requete).await;
        let statut = r.status();
        let notees = vues.lock().expect("verrou d'essai").clone();
        (statut, notees)
    }

    #[tokio::test]
    async fn le_decalage_de_la_requete_arrive_au_service() {
        let (statut, vues) = appelle(
            "essai-recherche-offset",
            requete("somebody", Some(200), Some(400)),
        )
        .await;

        assert_eq!(statut, StatusCode::OK);
        assert_eq!(
            vues,
            vec![(String::from("somebody"), 200, 400)],
            "`?limit=` et `?offset=` doivent parvenir intacts au connecteur"
        );
    }

    /// Non-régression : la requête que tous les clients installés envoient.
    #[tokio::test]
    async fn sans_parametres_la_route_garde_ses_valeurs_d_avant() {
        let (statut, vues) =
            appelle("essai-recherche-defaut", requete("somebody", None, None)).await;

        assert_eq!(statut, StatusCode::OK);
        assert_eq!(
            vues,
            vec![(String::from("somebody"), 20, 0)],
            "défaut inchangé : 20 par catégorie, première page"
        );
    }

    /// « Tous » reste `limit=0` — la convention des facettes Oxygen, déjà
    /// bornée par le connecteur.
    #[tokio::test]
    async fn tous_passe_par_limit_zero() {
        let (_, vues) = appelle("essai-recherche-tous", requete("jazz", Some(0), None)).await;
        assert_eq!(vues, vec![(String::from("jazz"), 0, 0)]);
    }

    /// La recherche n'entre PAS dans le cache des listes utilisateur (#2818) :
    /// sa clé devrait porter la requête, la limite et le décalage, soit une
    /// entrée par frappe et par page.
    #[tokio::test]
    async fn la_recherche_ne_peuple_pas_le_cache_des_listes_utilisateur() {
        let nom = "essai-recherche-sans-cache";
        let (_, _) = appelle(nom, requete("somebody", Some(50), None)).await;
        for ressource in ["playlists", "albums", "tracks", "artists", "search"] {
            assert!(
                contenu_utilisateur_frais(nom, ressource).is_none(),
                "la recherche ne doit rien mémoriser sous `{ressource}`"
            );
        }
    }
}

#[cfg(test)]
mod tests_favoris_dates {
    use super::tests_cache_utilisateur::etat_essai_date;
    use super::*;

    async fn corps(r: Response) -> Value {
        let octets = axum::body::to_bytes(r.into_body(), 1 << 20)
            .await
            .expect("corps lisible");
        serde_json::from_slice(&octets).expect("json")
    }

    fn sans_tri() -> Query<TriQuery> {
        Query(TriQuery {
            sort: None,
            order: None,
        })
    }

    /// #3489, le site d'appel. `get_user_favorites_dated` peut être parfaite
    /// dans Qobuz et Tidal : si `lire_favoris` ne l'appelle pas, la route sert
    /// ce qu'elle servait avant et le tri « Date d'ajout » reste inerte.
    ///
    /// Contre-épreuve : retirez le `if let Some(items) = …` de `lire_favoris`
    /// et cet essai passe au rouge — la réponse revient à la liste sans date.
    #[tokio::test]
    async fn la_route_sert_la_date_que_le_connecteur_transporte() {
        let etat = etat_essai_date("essai-date-favoris", Some("2019-04-18T09:53:31.000+0000"));
        let r = service_favorites(
            State(etat),
            Path((String::from("essai-date-favoris"), String::from("albums"))),
            sans_tri(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let corps = corps(r).await;
        let albums = corps["albums"].as_array().expect("la cle `albums`");
        assert_eq!(albums.len(), 1);
        assert_eq!(
            albums[0]["created_at"],
            json!("2019-04-18T09:53:31Z"),
            "la route doit transporter la date : c'est l'objet de #3489"
        );
        assert_eq!(
            albums[0]["source_id"],
            json!("a1"),
            "et ne rien perdre du contrat d'avant"
        );
    }

    /// Un connecteur qui ne transporte aucune date garde le comportement
    /// d'avant, à l'octet près : `Ok(None)` veut dire « pas de date », pas
    /// « pas de favoris ». Sans cette moitié, le correctif viderait l'écran
    /// Favoris de Deezer, Spotify, YouTube, Amazon et Bandcamp.
    #[tokio::test]
    async fn un_connecteur_sans_date_rend_exactement_ce_qu_il_rendait() {
        let etat = etat_essai_date("essai-date-absente", None);
        let r = service_favorites(
            State(etat),
            Path((String::from("essai-date-absente"), String::from("tracks"))),
            sans_tri(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let corps = corps(r).await;
        let pistes = corps["tracks"].as_array().expect("la cle `tracks`");
        assert_eq!(pistes.len(), 3, "les trois pistes du service simule");
        assert!(
            pistes.iter().all(|p| p.get("created_at").is_none()),
            "aucune date inventee la ou le service n'en donne pas"
        );
    }
}

/// 🔴 #859 — un refus délibéré ne sort plus en `502 Bad Gateway`, et une vraie
/// panne de passerelle y sort toujours.
///
/// # Ce qui était mesuré
///
/// Sur le .18 en marche, le 12/09/2026, trois essais sur trois :
///
/// ```text
/// GET /api/v1/streaming/bandcamp/playlists → 502 en 5,1 / 8,5 / 4,6 ms
/// corps : « Bandcamp ne fournit pas de playlists »           (36 octets)
/// ```
///
/// Quatre à huit millisecondes : aucun aller-retour réseau. Le témoin de
/// contraste, pris au même moment : `GET /streaming/qobuz/playlists` → 200 en
/// 208 ms, 57 199 octets. Le 502 ne décrivait donc rien de ce qui s'était
/// passé — et une issue a été ouverte sur ce seul motif.
///
/// # Les deux directions, et pourquoi les deux
///
/// Sans le second essai on aurait remplacé un mensonge par un autre : tout
/// sortir en 501 serait aussi faux que tout sortir en 502. Le premier essai
/// rougit si le 502 universel revient ; le second rougit si le 501 devient
/// universel.
///
/// # Ce que ces essais couvrent que le seul `svc_response` ne couvrirait pas
///
/// Le refus du premier essai n'est PAS fabriqué ici : `service_album_label`
/// tombe sur le défaut du trait `get_album_label` (`tune-core`), que le
/// connecteur simulé ne surcharge pas. Le vert prouve donc les DEUX moitiés du
/// correctif — le producteur pose bien un [`tune_core::TuneError::Unsupported`]
/// ET la frontière HTTP le traduit. Reposer le `.into()` d'avant dans
/// `traits.rs` suffit à faire rougir, sans toucher à `svc_response`.
#[cfg(test)]
mod temoin_statut_du_refus_i859 {
    use super::*;
    use tune_core::TuneError;
    use tune_core::db::sqlite::SqliteDb;
    use tune_core::streaming::traits::{
        AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack,
        StreamUrl,
    };

    /// Ce que le connecteur simulé fait de `get_user_playlists`.
    #[derive(Clone, Copy)]
    enum Humeur {
        /// « Ce service ne fournit pas de playlists » — le refus délibéré,
        /// posé exactement comme `hors_portee` de `tune-bandcamp` le pose.
        Refuse,
        /// L'amont est injoignable — la seule situation qu'un 502 décrit.
        PasserelleEnPanne,
    }

    const REFUS: &str = "Bandcamp ne fournit pas de playlists";
    const PANNE: &str = "error sending request for url (https://bandcamp.com/api)";

    struct ServiceDHumeur {
        nom: String,
        humeur: Humeur,
    }

    #[async_trait::async_trait]
    impl StreamingService for ServiceDHumeur {
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
        fn set_enabled(&mut self, _e: bool) {}
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
            Err(TuneError::Streaming(PANNE.into()))
        }
        async fn get_track(&self, _t: &str) -> Result<StreamTrack, TuneError> {
            Err(TuneError::Streaming(PANNE.into()))
        }
        async fn get_track_url(&self, _t: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
            Err(TuneError::Streaming(PANNE.into()))
        }
        async fn get_album(&self, _a: &str) -> Result<StreamAlbum, TuneError> {
            Err(TuneError::Streaming(PANNE.into()))
        }
        async fn get_album_tracks(&self, _a: &str) -> Result<Vec<StreamTrack>, TuneError> {
            Err(TuneError::Streaming(PANNE.into()))
        }
        async fn get_artist(&self, _a: &str) -> Result<StreamArtist, TuneError> {
            Err(TuneError::Streaming(PANNE.into()))
        }
        async fn get_playlist(&self, _p: &str) -> Result<StreamPlaylist, TuneError> {
            Err(TuneError::Streaming(PANNE.into()))
        }
        async fn get_playlist_tracks(&self, _p: &str) -> Result<Vec<StreamTrack>, TuneError> {
            Err(TuneError::Streaming(PANNE.into()))
        }
        /// La route MESURÉE sur le .18 : `GET /streaming/{svc}/playlists`.
        async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
            match self.humeur {
                Humeur::Refuse => Err(TuneError::Unsupported(REFUS.into())),
                Humeur::PasserelleEnPanne => Err(TuneError::Streaming(PANNE.into())),
            }
        }
        async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
            Err(TuneError::Streaming(PANNE.into()))
        }
        async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
            Err(TuneError::Streaming(PANNE.into()))
        }
        // `get_album_label` n'est PAS surchargé : c'est le défaut du trait —
        // le vrai producteur de refus — que l'essai du label interroge.
    }

    /// Chaque essai a SON nom de service : le cache de contenu utilisateur est
    /// un `static` partagé par tout le processus de test.
    fn etat(nom: &'static str, humeur: Humeur) -> (StreamingHttpState, String) {
        let backend: Arc<dyn DbBackend> =
            Arc::new(SqliteDb::open_in_memory().expect("sqlite en memoire"));
        let mut registre = ServiceRegistry::new();
        registre.register(Box::new(ServiceDHumeur {
            nom: nom.to_string(),
            humeur,
        }));
        let etat = StreamingHttpState::new(
            backend,
            Arc::new(Mutex::new(registre)),
            Arc::new(EventBus::new()),
        );
        (etat, nom.to_string())
    }

    async fn texte(r: Response) -> String {
        let octets = axum::body::to_bytes(r.into_body(), 1 << 20)
            .await
            .expect("corps lisible");
        String::from_utf8_lossy(&octets).into_owned()
    }

    #[derive(Clone, Default)]
    struct Journal4039(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Journal4039 {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn diagnostic_4039_nomme_la_route_le_service_et_le_defaut() {
        use tower::ServiceExt;
        use tracing::instrument::WithSubscriber;
        for (suffix, kind, status) in [
            ("artists/42", "streaming", StatusCode::BAD_GATEWAY),
            (
                "albums/42/label",
                "unsupported",
                StatusCode::NOT_IMPLEMENTED,
            ),
        ] {
            let journal = Journal4039::default();
            let writer = journal.clone();
            let subscriber = tracing_subscriber::fmt()
                .json()
                .with_max_level(tracing::Level::WARN)
                .without_time()
                .with_writer(move || writer.clone())
                .finish();
            let (state, name) = etat("diagnostic-4039", Humeur::PasserelleEnPanne);
            let app = Router::new().nest("/api/v1/streaming", router().with_state(state));
            let response = app
                .oneshot(
                    axum::http::Request::builder()
                        .uri(format!(
                            "/api/v1/streaming/{name}/{suffix}?token=secret-4039"
                        ))
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .with_subscriber(subscriber)
                .await
                .unwrap();
            assert_eq!(response.status(), status);
            assert!(
                !response
                    .headers()
                    .contains_key(axum::http::header::CACHE_CONTROL)
            );
            let text = String::from_utf8(journal.0.lock().unwrap().clone()).unwrap();
            let events: Vec<Value> = text
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            let event = events
                .iter()
                .find(|v| v["fields"]["message"] == "streaming_service_error")
                .expect("le 5xx streaming ne laisse aucune cause dans le journal (#4039)");
            assert_eq!(event["fields"]["status"], status.as_u16());
            assert_eq!(event["fields"]["error_kind"], kind);
            assert!(
                event["fields"]["error"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
            );
            assert_eq!(event["fields"]["service"], name);
            assert_eq!(event["fields"]["method"], "GET");
            assert_eq!(
                event["fields"]["route"],
                if kind == "streaming" {
                    "/api/v1/streaming/{service}/artists/{artist_id}"
                } else {
                    "/api/v1/streaming/{service}/albums/{album_id}/label"
                }
            );
            assert!(
                !text.contains("secret-4039"),
                "le contexte ne doit pas journaliser les paramètres privés"
            );
        }
    }

    /// Le défaut mesuré : `GET /streaming/{service}/playlists` sur un
    /// connecteur qui REFUSE. 502 avant, 501 maintenant.
    #[tokio::test]
    async fn un_refus_du_connecteur_ne_sort_plus_en_502() {
        let (etat, nom) = etat("refus-playlists", Humeur::Refuse);
        let r = service_playlists(State(etat), Path(nom)).await;
        let statut = r.status();
        let corps = texte(r).await;
        assert_ne!(
            statut,
            StatusCode::BAD_GATEWAY,
            "« {corps} » n'est pas une passerelle en panne : le serveur a dit non, \
             en 4 ms et sans un octet de reseau (mesure .18 du 12/09/2026)"
        );
        assert_eq!(
            statut,
            StatusCode::NOT_IMPLEMENTED,
            "un refus delibere sort en 501 Not Implemented, corps rendu : « {corps} »"
        );
        assert_eq!(
            corps, REFUS,
            "le corps ne change pas d'un octet : seul le statut change"
        );
    }

    /// L'autre direction, sans laquelle on aurait remplacé un mensonge par un
    /// autre : l'amont injoignable reste un 502, c'est ce que 502 veut dire.
    #[tokio::test]
    async fn une_vraie_panne_de_passerelle_sort_toujours_en_502() {
        let (etat, nom) = etat("panne-playlists", Humeur::PasserelleEnPanne);
        let r = service_playlists(State(etat), Path(nom)).await;
        let statut = r.status();
        let corps = texte(r).await;
        assert_eq!(
            statut,
            StatusCode::BAD_GATEWAY,
            "l'amont injoignable EST une passerelle en panne ; corps rendu : « {corps} »"
        );
        assert!(
            corps.contains("error sending request"),
            "le corps doit rester celui du service : « {corps} »"
        );
    }

    /// Le refus posé par le DÉFAUT DU TRAIT — pas par cet essai — traverse la
    /// route jusqu'au 501. Reposer `"…".into()` dans `traits.rs` fait rougir
    /// ici sans que `svc_response` ait bougé.
    #[tokio::test]
    async fn le_refus_par_defaut_du_trait_sort_aussi_en_501() {
        let (etat, nom) = etat("refus-label", Humeur::PasserelleEnPanne);
        let r = service_album_label(State(etat), Path((nom, String::from("a1")))).await;
        let statut = r.status();
        let corps = texte(r).await;
        assert_eq!(
            statut,
            StatusCode::NOT_IMPLEMENTED,
            "« {corps} » est le refus par defaut du trait, pas une panne d'amont"
        );
        assert_eq!(corps, "labels not supported for this service");
    }

    /// **Les HUIT** refus par défaut du trait, pas seulement celui que la route
    /// du label emprunte.
    ///
    /// L'essai précédent n'en interroge qu'un. Reposer `"…".into()` sur
    /// `create_playlist` — ou sur n'importe lequel des sept autres — ferait
    /// silencieusement redescendre sa route en 502 sans qu'aucun rouge ne
    /// vienne. Un rouge qui ne vient pas est un défaut du témoin : celui-ci
    /// ferme les huit.
    ///
    /// Le connecteur simulé ne surcharge AUCUNE de ces méthodes : c'est bien le
    /// défaut de `tune-core` qui répond.
    #[tokio::test]
    async fn les_huit_refus_par_defaut_du_trait_sont_types() {
        let mut svc = ServiceDHumeur {
            nom: "essai-defauts".into(),
            humeur: Humeur::PasserelleEnPanne,
        };
        let refus: Vec<(&str, TuneError)> = vec![
            (
                "create_playlist",
                svc.create_playlist("x", None).await.unwrap_err(),
            ),
            (
                "add_tracks_to_playlist",
                svc.add_tracks_to_playlist("p", &[]).await.unwrap_err(),
            ),
            (
                "delete_playlist",
                svc.delete_playlist("p").await.unwrap_err(),
            ),
            (
                "remove_tracks_from_playlist",
                svc.remove_tracks_from_playlist("p", &[]).await.unwrap_err(),
            ),
            (
                "get_album_label",
                svc.get_album_label("a").await.unwrap_err(),
            ),
            (
                "get_album_context",
                svc.get_album_context("a").await.unwrap_err(),
            ),
            (
                "add_favorite",
                svc.add_favorite("albums", "i").await.unwrap_err(),
            ),
            (
                "remove_favorite",
                svc.remove_favorite("albums", "i").await.unwrap_err(),
            ),
        ];
        assert_eq!(refus.len(), 8, "les huit defauts, pas sept");
        for (methode, erreur) in refus {
            assert!(
                matches!(erreur, TuneError::Unsupported(_)),
                "le defaut de `{methode}` est un refus delibere, pas une panne \
                 d'amont : {erreur:?}"
            );
            assert_eq!(
                statut_porte_par_l_erreur(&erreur),
                Some(StatusCode::NOT_IMPLEMENTED),
                "et sa route doit donc sortir en 501 : `{methode}`"
            );
        }
    }

    /// Une réponse ÉDITORIALE refusée passe par `svc_response_editorial`, qui
    /// délègue à `svc_response`. Sans cet essai, la moitié éditoriale des
    /// routes pourrait garder le 502 sans que rien ne rougisse.
    #[tokio::test]
    async fn le_chemin_editorial_classe_le_refus_comme_l_autre() {
        let r = svc_response_editorial::<Value>(Err(TuneError::Unsupported(REFUS.into())));
        assert_eq!(r.status(), StatusCode::NOT_IMPLEMENTED);
        assert!(
            r.headers().get(axum::http::header::CACHE_CONTROL).is_none(),
            "un refus ne se met pas en cache trente minutes"
        );
    }

    /// Le 502 reste le DÉFAUT. Aucune autre variante ne doit être promue en
    /// douce : cet essai fige la frontière, variante par variante.
    #[test]
    fn seule_la_variante_du_refus_porte_un_statut() {
        assert_eq!(
            statut_porte_par_l_erreur(&TuneError::Unsupported("x".into())),
            Some(StatusCode::NOT_IMPLEMENTED)
        );
        for panne in [
            TuneError::Streaming("x".into()),
            TuneError::Json(serde_json::from_str::<Value>("{").unwrap_err()),
            TuneError::Db("x".into()),
            TuneError::Config("x".into()),
            TuneError::NotFound("x".into()),
            TuneError::Other("x".into()),
        ] {
            assert_eq!(
                statut_porte_par_l_erreur(&panne),
                None,
                "{panne} doit garder le statut par defaut de l'appelant"
            );
        }
    }
}

/// #3481 — `?section=` sur `/{service}/genres/{genre_id}/albums`.
#[cfg(test)]
mod tests_route_rubrique_par_genre {
    use super::tests_cache_utilisateur::{RecherchesVues, etat_essai_complet};
    use super::*;
    use std::sync::atomic::AtomicUsize;

    async fn titres(nom: &str, limit: Option<usize>, section: Option<&str>) -> Vec<String> {
        let etat = etat_essai_complet(
            nom,
            Arc::new(AtomicUsize::new(0)),
            Duration::ZERO,
            RecherchesVues::default(),
        );
        let r = service_genre_albums(
            State(etat),
            Path((nom.to_string(), "80".to_string())),
            Query(GenreAlbumsQuery {
                limit,
                section: section.map(str::to_string),
            }),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let corps = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .expect("corps lisible");
        let v: Value = serde_json::from_slice(&corps).expect("JSON");
        v.as_array()
            .expect("un tableau d'albums")
            .iter()
            .map(|a| a["title"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// LE défaut de #3481 : la rubrique demandée doit parvenir au connecteur,
    /// avec le genre et la limite.
    #[tokio::test]
    async fn la_rubrique_demandee_parvient_au_connecteur() {
        let vus = titres("essai-genre-rubrique", Some(5), Some("press-awards")).await;
        assert_eq!(
            vus,
            vec![String::from("press-awards|80|5")],
            "`?section=` doit choisir la rubrique du genre, pas les nouveautés (#3481)"
        );
    }

    /// Non-régression : sans `?section=`, la route rend les nouveautés du
    /// genre, comme pour tous les clients installés.
    #[tokio::test]
    async fn sans_rubrique_la_route_rend_les_nouveautes() {
        let vus = titres("essai-genre-defaut", None, None).await;
        assert_eq!(vus, vec![String::from("nouveautes|80")]);
    }
}

/// 🔴 Fuites de français — la réponse éditoriale sort dans la langue demandée.
///
/// Qobuz sert ses libellés de rubriques en objet multilingue ; le connecteur
/// n'en gardait qu'un, le français, pour tout le monde
/// (`tune-core/src/streaming/qobuz.rs:2519`). Le faisceau voyage désormais
/// jusqu'ici, et c'est ici — au plus près de l'affichage, là où
/// `Accept-Language` est lisible — que la langue est choisie.
///
/// Trois propriétés :
///
/// 1. un lecteur roumain reçoit le libellé anglais, pas le français ;
/// 2. **la contre-épreuve** : un lecteur francophone garde le sien, sinon le
///    correctif aurait seulement déplacé la fuite ;
/// 3. `Vary: Accept-Language` accompagne le `Cache-Control` de trente
///    minutes — sans lui, le cache navigateur resservirait la copie française.
#[cfg(test)]
mod temoin_rubriques_dans_la_langue_demandee {
    use super::*;
    use std::collections::BTreeMap;
    use tune_core::streaming::traits::PlaylistTagGroup;

    fn faisceau(fr: &str, en: &str) -> Option<BTreeMap<String, String>> {
        Some(BTreeMap::from([
            ("fr".to_string(), fr.to_string()),
            ("en".to_string(), en.to_string()),
        ]))
    }

    /// Les rubriques telles que Qobuz les sert : le libellé français d'origine
    /// dans `name`, le faisceau complet à côté.
    fn rangees() -> Vec<PlaylistTagGroup> {
        vec![
            PlaylistTagGroup {
                id: "label".into(),
                name: "Histoires de labels".into(),
                name_i18n: faisceau("Histoires de labels", "Label Stories"),
                playlists: Vec::new(),
            },
            PlaylistTagGroup {
                id: "new".into(),
                name: "Nouveautés".into(),
                name_i18n: faisceau("Nouveautés", "New Releases"),
                playlists: Vec::new(),
            },
        ]
    }

    async fn libelles(accept_language: &str) -> (Vec<String>, Option<String>) {
        let mut entetes = axum::http::HeaderMap::new();
        entetes.insert("accept-language", accept_language.parse().unwrap());
        let langues = etiquettes_langue::langues_demandees(&entetes);

        let reponse =
            svc_response_editorial_localise(Ok::<_, tune_core::TuneError>(rangees()), &langues);
        let vary = reponse
            .headers()
            .get(axum::http::header::VARY)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let corps = axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .expect("corps lisible");
        let v: Value = serde_json::from_slice(&corps).expect("JSON");
        let noms = v
            .as_array()
            .expect("un tableau de rangées")
            .iter()
            .map(|r| r["name"].as_str().unwrap_or_default().to_string())
            .collect();
        (noms, vary)
    }

    #[tokio::test]
    async fn un_lecteur_roumain_ne_recoit_plus_le_francais() {
        let (noms, _) = libelles("ro-RO,ro;q=0.9").await;
        assert_eq!(
            noms,
            vec![String::from("Label Stories"), String::from("New Releases")],
            "le roumain n'existe pas chez Qobuz : recours à l'anglais, pas au français"
        );
    }

    #[tokio::test]
    async fn un_lecteur_francophone_garde_son_libelle() {
        // LA contre-épreuve : préférer l'anglais pour tout le monde aurait
        // seulement déplacé la fuite d'une langue à l'autre.
        let (noms, _) = libelles("fr-FR,fr;q=0.9").await;
        assert_eq!(
            noms,
            vec![
                String::from("Histoires de labels"),
                String::from("Nouveautés")
            ]
        );
    }

    #[tokio::test]
    async fn la_reponse_varie_selon_la_langue_demandee() {
        let (_, vary) = libelles("ro").await;
        assert_eq!(
            vary.as_deref(),
            Some("Accept-Language"),
            "sans `Vary`, le cache navigateur de 30 min resservirait la copie française"
        );
    }
}
