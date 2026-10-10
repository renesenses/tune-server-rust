// Resolution ASYMETRIQUE, et non une union : `rc` apportait ici DEUX imports.
//   - `ActiveProfile` est le fond du correctif #2560 (cloisonnement des
//     favoris) : il reste, sans quoi on perdrait un correctif de securite ;
//   - `crate::routes::panne_sql::OuDefautJournalise` est la FORME d'avant :
//     ce lot a deplace le module dans `tune-http-types`, il n'existe plus
//     sous `crate::routes`. L'import vit desormais plus bas, sous son nouveau
//     chemin. Le garder ici casserait `tune-server` lui-meme.
use crate::routes::active_profile::ActiveProfile;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_http_types::panne_sql::OuDefautJournalise;

use tune_core::db::backend::ToSqlValue;
use tune_core::db::favorite_facets_repo::FavoriteFacetsRepo;
use tune_core::db::profile_repo::ProfileRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::streaming_favorites_repo::StreamingFavoritesRepo;
use tune_core::favorites_sort::TriFavoris;
use tune_core::license::Feature;

use crate::state::AppState;

#[derive(Deserialize)]
struct CreateProfile {
    #[serde(alias = "username")]
    name: String,
    #[serde(alias = "display_name")]
    avatar_color: Option<String>,
}

#[derive(Deserialize)]
struct UpdateProfile {
    #[serde(alias = "display_name")]
    name: Option<String>,
    #[serde(alias = "avatar_path")]
    avatar_color: Option<String>,
}

#[derive(Deserialize)]
struct FavoriteAction {
    item_type: String,
    item_id: i64,
}

/// `item_type` filtre, `sort`/`order` rangent (#2001).
///
/// Les deux derniers sont **facultatifs** : absents, la liste est rendue
/// exactement comme avant (`ORDER BY created_at DESC`), et le code emprunte le
/// même chemin qu'auparavant. `sort` accepte `added`, `title`, `artist`,
/// `album` (et leurs équivalents français) ; `order` vaut `asc` par défaut.
#[derive(Deserialize)]
struct FavoritesQuery {
    item_type: Option<String>,
    sort: Option<String>,
    order: Option<String>,
}

/// Favori de FACETTE (#2442) : un label n'a pas d'identifiant entier, il est
/// désigné par sa valeur telle que la facette la sélectionne. D'où `value:
/// String` et non `item_id: i64`.
#[derive(Deserialize)]
struct FacetFavoriteAction {
    facet: String,
    value: String,
}

#[derive(Deserialize)]
struct FacetsQuery {
    facet: Option<String>,
}

/// Add a streaming (Tidal/Qobuz/…) item to a profile's favorites. Metadata is
/// stored alongside so the list needs no per-item hydration.
#[derive(Deserialize)]
struct StreamingFavoriteAdd {
    item_type: String,
    service: String,
    #[serde(alias = "id")]
    service_id: String,
    title: Option<String>,
    #[serde(alias = "artist_name")]
    artist: Option<String>,
    #[serde(alias = "album_title")]
    album: Option<String>,
    #[serde(alias = "cover_path", alias = "cover")]
    cover_url: Option<String>,
    /// #5530 — le marquage « généré par IA » que le service donne à l'album
    /// (Qobuz : `ai_generated` d'`album/get`), quand le client l'a. Absent :
    /// rien n'est posé.
    #[serde(default, alias = "album_ai_generated")]
    ai_generated: Option<bool>,
    /// #5997 — l'ISRC d'une piste, quand le client le connaît : il resserre
    /// le rapprochement avec la bibliothèque locale (forum #2127).
    #[serde(default)]
    isrc: Option<String>,
}

#[derive(Deserialize)]
struct StreamingFavoriteRemove {
    item_type: String,
    service: String,
    #[serde(alias = "id")]
    service_id: String,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct SwitchProfile {
    profile_id: i64,
    pin: Option<String>,
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
}

#[derive(Deserialize)]
struct HistoryQuery {
    limit: Option<i64>,
}

#[derive(Deserialize)]
struct CheckFavoritesBody {
    item_type: String,
    item_ids: Vec<i64>,
}

/// Ordre manuel d'un onglet de favoris locaux (#2001, piste 2).
///
/// `item_ids` est la liste **complète et ordonnée** de l'onglet : ce que le
/// client voit à l'écran, de haut en bas, après le glisser-déposer. Le serveur
/// numérote 1..n et renvoie en fin d'ordre tout favori de l'onglet absent de la
/// liste — voir `ProfileRepo::reorder_favorites` pour les trois garanties.
#[derive(Deserialize)]
struct ReorderFavorites {
    item_type: String,
    item_ids: Vec<i64>,
}

/// Un favori de service désigné par sa clé — `(service, service_id)` — puisque
/// ces éléments n'ont pas d'`item_id` entier.
#[derive(Deserialize)]
struct StreamingFavoriteRef {
    service: String,
    service_id: String,
}

#[derive(Deserialize)]
struct ReorderStreamingFavorites {
    item_type: String,
    items: Vec<StreamingFavoriteRef>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_profiles).post(create_profile))
        .route("/active", get(get_active_profile))
        .route("/current", get(get_active_profile))
        .route("/switch", post(switch_profile))
        .route("/deactivate", post(deactivate_profile))
        .route("/search", get(search_profiles))
        .route(
            "/{id}",
            get(get_profile).put(update_profile).delete(delete_profile),
        )
        .route("/{id}/activate", post(activate_profile))
        .route("/{id}/favorites", get(list_favorites))
        .route("/{id}/favorites/add", post(add_favorite))
        .route("/{id}/favorites/remove", post(remove_favorite))
        // Ordre manuel des favoris locaux (#2001, piste 2) : le geste de Tades,
        // qui a essayé de déplacer ses favoris à la souris et n'a rien trouvé.
        .route("/{id}/favorites/reorder", post(reorder_favorites))
        .route("/{id}/favorites/streaming", get(list_streaming_favorites))
        .route(
            "/{id}/favorites/streaming/add",
            post(add_streaming_favorite),
        )
        .route(
            "/{id}/favorites/streaming/remove",
            post(remove_streaming_favorite),
        )
        // Reprise des favoris posés CHEZ le service (#3419).
        .route(
            "/{id}/favorites/streaming/sync",
            post(sync_streaming_favorites),
        )
        .route(
            "/{id}/favorites/streaming/reorder",
            post(reorder_streaming_favorites),
        )
        // État du miroir des favoris de service (#5997).
        .route(
            "/{id}/favorites/streaming/miroir",
            get(etat_miroir_streaming_favorites),
        )
        // Favoris de facette (label, et demain genre/format/année) — #2442.
        .route("/{id}/favorites/facets", get(list_facet_favorites))
        .route("/{id}/favorites/facets/add", post(add_facet_favorite))
        .route("/{id}/favorites/facets/remove", post(remove_facet_favorite))
        .route(
            "/{id}/settings",
            get(profile_settings).post(update_profile_settings),
        )
        .route("/{id}/stats", get(profile_stats))
        .route("/{id}/history", get(profile_history))
        .route("/{id}/favorites/check", post(check_favorites))
}

async fn list_profiles(State(state): State<AppState>) -> Json<Value> {
    let repo = ProfileRepo::with_backend(state.backend.clone());
    // Don't swallow a DB error as an empty list: the web client would then
    // auto-create "Default" and mask real profiles with no visible error.
    // Log it so a schema/column drift on an older DB is diagnosable.
    let items = repo.list().unwrap_or_else(|e| {
        tracing::error!(error = %e, "list_profiles: repo.list() failed, returning empty");
        Vec::new()
    });
    Json(json!(items))
}

async fn get_active_profile(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let profile_id: i64 = settings
        .get("active_profile_id")
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let repo = ProfileRepo::with_backend(state.backend.clone());
    let profile = repo.get(profile_id).ok().flatten();
    Json(json!({
        "active_profile_id": profile_id,
        "profile": profile,
    }))
}

async fn switch_profile(
    State(state): State<AppState>,
    Json(body): Json<SwitchProfile>,
) -> impl IntoResponse {
    let repo = ProfileRepo::with_backend(state.backend.clone());
    match repo.get(body.profile_id) {
        Ok(Some(profile)) => {
            let settings =
                tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
            settings
                .set("active_profile_id", &body.profile_id.to_string())
                .ok();
            Json(json!({
                "active_profile_id": body.profile_id,
                "profile": profile,
            }))
            .into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, "profile not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn deactivate_profile(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    settings.set("active_profile_id", "1").ok();
    Json(json!({ "active_profile_id": serde_json::Value::Null }))
}

async fn activate_profile(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = ProfileRepo::with_backend(state.backend.clone());
    match repo.get(id) {
        Ok(Some(profile)) => {
            let settings =
                tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
            settings.set("active_profile_id", &id.to_string()).ok();
            Json(json!({
                "active_profile_id": id,
                "profile": profile,
            }))
            .into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, "profile not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn get_profile(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = ProfileRepo::with_backend(state.backend.clone());
    match repo.get(id) {
        Ok(Some(p)) => Json(json!(p)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn create_profile(
    State(state): State<AppState>,
    Json(body): Json<CreateProfile>,
) -> impl IntoResponse {
    let repo = ProfileRepo::with_backend(state.backend.clone());

    // Free tier: max 1 profile (the default). Premium: unlimited.
    let is_premium = state.license.check_feature(Feature::MultiProfiles).await;
    if !is_premium {
        let count = repo.count().unwrap_or(0);
        if count >= 1 {
            return (
                StatusCode::PAYMENT_REQUIRED,
                Json(json!({
                    "error": "premium_required",
                    "feature": "multi_profiles",
                    "message": "Free tier allows 1 profile. Upgrade to Premium for unlimited profiles.",
                })),
            )
                .into_response();
        }
    }

    match repo.create(&body.name, None, body.avatar_color.as_deref()) {
        Ok(id) => {
            // Return the full profile object so the web client can use it directly
            let profile = repo.get(id).ok().flatten();
            let value = profile
                .map(|p| json!(p))
                .unwrap_or_else(|| json!({"id": id, "name": body.name}));
            (StatusCode::CREATED, Json(value)).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn update_profile(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateProfile>,
) -> impl IntoResponse {
    let repo = ProfileRepo::with_backend(state.backend.clone());
    match repo.update(id, body.name.as_deref(), body.avatar_color.as_deref()) {
        Ok(_) => {
            // Return the updated profile so the client can use it directly
            match repo.get(id) {
                Ok(Some(profile)) => Json(json!(profile)).into_response(),
                Ok(None) => {
                    (StatusCode::NOT_FOUND, "profile not found after update").into_response()
                }
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
            }
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_profile(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = ProfileRepo::with_backend(state.backend.clone());
    match repo.delete(id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

/// Le `{id}` du chemin est celui de l'appelant, sinon `404` (#2560).
///
/// **Point de refus unique** de la famille `/profiles/{id}/favorites*` : dix
/// routes qui lisaient et écrivaient sur le profil nommé par le CHEMIN, sans
/// jamais le confronter à l'identité de l'appelant. Les identifiants de
/// profils sont de petits entiers séquentiels : les énumérer donnait les
/// favoris — locaux, streaming et de facette — de tout le foyer, en lecture
/// comme en écriture.
///
/// L'identité vient de [`ActiveProfile`], qui applique déjà la convention du
/// dépôt (*en-tête = qui agit*) et, auth activée, lie `X-Profile-Id` au
/// porteur du jeton. Le chemin ne dit plus que *sur quoi*.
///
/// `owned_or_404` de `routes::playlists` répond à une AUTRE question — « cette
/// playlist appartient-elle au profil ? » — et se type sur un `PlaylistRepo`
/// et une `Playlist` : elle ne peut pas porter celle-ci, où le `{id}` du
/// chemin *est* le profil. C'est donc bien un mécanisme par intention, et
/// celui-ci est le seul de la sienne.
///
/// **404, jamais 403**, comme la #3073 : un 403 confirmerait l'existence du
/// profil et rendrait l'énumération exploitable.
fn profil_du_chemin_ou_404(chemin: i64, appelant: ActiveProfile) -> Result<(), Response> {
    if chemin == appelant.id() {
        return Ok(());
    }
    tracing::debug!(
        chemin,
        appelant = appelant.id(),
        "favoris_profil_du_chemin_refuse"
    );
    Err((
        StatusCode::NOT_FOUND,
        Json(json!({"error": "profile not found"})),
    )
        .into_response())
}

async fn list_favorites(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    profil: ActiveProfile,
    Query(q): Query<FavoritesQuery>,
) -> Response {
    if let Err(r) = profil_du_chemin_ou_404(id, profil) {
        return r;
    }
    let repo = ProfileRepo::with_backend(state.backend.clone());
    let items = match TriFavoris::depuis(q.sort.as_deref(), q.order.as_deref()) {
        Some(tri) => repo.list_favorites_sorted(id, q.item_type.as_deref(), tri),
        None => repo.list_favorites(id, q.item_type.as_deref()),
    }
    .unwrap_or_default();
    Json(json!(items)).into_response()
}

async fn add_favorite(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    profil: ActiveProfile,
    Json(body): Json<FavoriteAction>,
) -> impl IntoResponse {
    if let Err(r) = profil_du_chemin_ou_404(id, profil) {
        return r;
    }
    let repo = ProfileRepo::with_backend(state.backend.clone());
    match repo.add_favorite(id, &body.item_type, body.item_id) {
        // Return a JSON body: web clients call response.json() and an empty
        // 201/204 body threw "Invalid JSON response". (Elie)
        Ok(_) => (StatusCode::CREATED, Json(json!({"ok": true}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

async fn remove_favorite(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    profil: ActiveProfile,
    Json(body): Json<FavoriteAction>,
) -> impl IntoResponse {
    if let Err(r) = profil_du_chemin_ou_404(id, profil) {
        return r;
    }
    let repo = ProfileRepo::with_backend(state.backend.clone());
    match repo.remove_favorite(id, &body.item_type, body.item_id) {
        Ok(_) => (StatusCode::OK, Json(json!({"ok": true}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

/// `POST /profiles/{id}/favorites/reorder` — pose l'ordre manuel d'un onglet
/// (#2001, piste 2). Se relit ensuite par `GET …/favorites?sort=manual`.
///
/// Rend `{"ok": true, "ordered": n}`, où `n` est le nombre de favoris
/// effectivement rangés : un identifiant que le profil n'a pas en favori est
/// ignoré sans erreur, donc `n` plus petit que la liste envoyée signale au
/// client que sa vue était périmée.
async fn reorder_favorites(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    profil: ActiveProfile,
    Json(body): Json<ReorderFavorites>,
) -> Response {
    // #2560 : *header = qui agit*, le chemin ne dit que *sur quoi*. Un
    // reordonnancement est une ECRITURE de favoris — sans cette garde, le
    // profil 2 reecrirait l'ordre du profil 1 en nommant son `{id}` dans
    // l'URL, et les identifiants de profil sont de petits entiers
    // sequentiels. La garde est arrivee APRES le commit d'origine (30/08) :
    // le rejouer tel quel rouvrait la faille sur deux routes neuves.
    if let Err(r) = profil_du_chemin_ou_404(id, profil) {
        return r;
    }
    let repo = ProfileRepo::with_backend(state.backend.clone());
    match repo.reorder_favorites(id, &body.item_type, &body.item_ids) {
        Ok(n) => (StatusCode::OK, Json(json!({"ok": true, "ordered": n}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

// --- Streaming favorites (Tidal/Qobuz/… items, stored separately from the
// integer-keyed local `favorites`) ---

async fn list_streaming_favorites(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    profil: ActiveProfile,
    Query(q): Query<FavoritesQuery>,
) -> Response {
    if let Err(r) = profil_du_chemin_ou_404(id, profil) {
        return r;
    }
    // #5997 — l'ouverture des Favoris rafraîchit les services en miroir dont
    // le cache court est périmé, puis donne à ce profil les favoris communs
    // qu'il n'a pas encore. La réponse reste un TABLEAU : un ancien client
    // obtient le miroir sans rien changer.
    let miroirs = services_en_miroir(&state).await;
    rafraichir_les_miroirs(&state, &miroirs, id, false, ATTENTE_RAFRAICHISSEMENT_LISTE).await;
    let noms: Vec<String> = miroirs.iter().map(|(n, _)| n.clone()).collect();
    if let Err(e) =
        tune_core::streaming::favorites_mirror::aligner_profil(&state.backend, id, &noms)
    {
        tracing::warn!(profile_id = id, erreur = %e, "favoris_miroir_alignement_impossible");
    }
    let repo = StreamingFavoritesRepo::with_backend(state.backend.clone());
    let items = match TriFavoris::depuis(q.sort.as_deref(), q.order.as_deref()) {
        Some(tri) => repo.list_sorted(id, q.item_type.as_deref(), tri),
        None => repo.list(id, q.item_type.as_deref()),
    }
    .unwrap_or_default();
    let mut reponse = Json(json!(items)).into_response();
    if let Ok(v) = axum::http::HeaderValue::from_str(statut_des_miroirs(&noms)) {
        reponse.headers_mut().insert("x-tune-favoris-miroir", v);
    }
    reponse
}

/// Ce que l'ouverture des Favoris attend du rafraîchissement, au plus. Au-delà
/// la liste part avec ce qu'elle a ; le rafraîchissement continue en fond et
/// la lecture suivante le verra.
const ATTENTE_RAFRAICHISSEMENT_LISTE: std::time::Duration = std::time::Duration::from_secs(5);

/// Les services du registre dont les favoris sont tenus en miroir (#5997),
/// connectés ou non : un service déconnecté peut porter des écritures en
/// attente, et ses favoris restent communs à tous les profils.
pub(crate) async fn services_en_miroir(
    state: &AppState,
) -> Vec<(String, tune_core::streaming::favorites_mirror::ServiceArc)> {
    let arcs: Vec<(String, tune_core::streaming::favorites_mirror::ServiceArc)> = {
        let registre = state.services.lock().await;
        registre
            .list()
            .into_iter()
            .filter_map(|nom| registre.get(&nom).map(|arc| (nom, arc)))
            .collect()
    };
    let mut out = Vec::new();
    for (nom, arc) in arcs {
        if arc.read().await.favoris_miroir() {
            out.push((nom, arc));
        }
    }
    out
}

/// Le service nommé, s'il est en miroir ET connecté.
///
/// Un service en miroir mais DÉCONNECTÉ (jamais configuré, session fermée)
/// garde le chemin d'avant : favori propre au profil, `201 {"ok":true}`. Rien
/// ne se perd pour autant : la ligne naît `miroir_etat` NULL, et le premier
/// rafraîchissement après la connexion l'adopte et la pousse au service.
/// Une PANNE d'un service connecté, elle, passe par le miroir et rend 202.
async fn service_en_miroir(
    state: &AppState,
    service: &str,
) -> Option<tune_core::streaming::favorites_mirror::ServiceArc> {
    let arc = state.services.lock().await.get(service)?;
    let pret = {
        let svc = arc.read().await;
        svc.favoris_miroir() && svc.utilisable().await
    };
    pret.then_some(arc)
}

/// Rafraîchit les miroirs (périmés, ou tous si `forcer`) dans une tâche à
/// part, attendue au plus `attente`. Rend les bilans des passages faits.
pub(crate) async fn rafraichir_les_miroirs(
    state: &AppState,
    miroirs: &[(String, tune_core::streaming::favorites_mirror::ServiceArc)],
    profile_id: i64,
    forcer: bool,
    attente: std::time::Duration,
) -> serde_json::Map<String, Value> {
    if miroirs.is_empty() {
        return serde_json::Map::new();
    }
    let miroirs = miroirs.to_vec();
    let backend = state.backend.clone();
    let tache = tokio::spawn(async move {
        let mut bilans = serde_json::Map::new();
        for (nom, arc) in miroirs {
            if let Some(b) = tune_core::streaming::favorites_mirror::rafraichir_si_perime(
                &arc, &backend, profile_id, forcer,
            )
            .await
            {
                tune_streaming_http::purge_contenu_utilisateur(&nom);
                bilans.insert(nom, json!(b));
            }
        }
        bilans
    });
    match tokio::time::timeout(attente, tache).await {
        Ok(Ok(bilans)) => bilans,
        Ok(Err(e)) => {
            tracing::warn!(erreur = %e, "favoris_miroir_tache_interrompue");
            serde_json::Map::new()
        }
        Err(_) => {
            tracing::info!(
                profile_id,
                "favoris_miroir_rafraichissement_continue_en_fond"
            );
            serde_json::Map::new()
        }
    }
}

/// L'en-tête `X-Tune-Favoris-Miroir` : `aucun`, `echec`, `en_attente` ou `ok`.
fn statut_des_miroirs(noms: &[String]) -> &'static str {
    if noms.is_empty() {
        return "aucun";
    }
    let etats: Vec<_> = noms
        .iter()
        .map(|n| tune_core::streaming::favorites_mirror::etat(n))
        .collect();
    if etats.iter().any(|e| e.statut == "echec") {
        "echec"
    } else if etats.iter().any(|e| e.en_attente > 0) {
        "en_attente"
    } else {
        "ok"
    }
}

/// `GET /profiles/{id}/favorites/streaming/miroir` — l'état du miroir de
/// chaque service (#5997) : dernier rafraîchissement, statut, motif, nombre
/// d'écritures en attente. De quoi dire « Qobuz n'a pas suivi ».
async fn etat_miroir_streaming_favorites(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    profil: ActiveProfile,
) -> Response {
    if let Err(r) = profil_du_chemin_ou_404(id, profil) {
        return r;
    }
    use tune_core::streaming::favorites_mirror as miroir;
    let mut services = serde_json::Map::new();
    for (nom, _) in services_en_miroir(&state).await {
        let mut etat = miroir::etat(&nom);
        etat.en_attente = miroir::compter_en_attente(&state.backend, &nom);
        services.insert(nom, json!(etat));
    }
    Json(json!({
        "ttl_s": miroir::ttl().as_secs(),
        "periode_s": miroir::periode().map(|p| p.as_secs()).unwrap_or(0),
        "services": services,
    }))
    .into_response()
}

async fn add_streaming_favorite(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    profil: ActiveProfile,
    Json(body): Json<StreamingFavoriteAdd>,
) -> impl IntoResponse {
    if let Err(r) = profil_du_chemin_ou_404(id, profil) {
        return r;
    }
    // #5997 — service en miroir : le serveur ajoute CHEZ le service, puis
    // pose le favori pour tous les profils. Un échec chez le service ne perd
    // rien (ligne en attente, retentée) et le dit (202 + motif).
    if let Some(arc) = service_en_miroir(&state, &body.service).await {
        let fav = tune_core::streaming::favorites_mirror::FavoriMiroir {
            item_type: body.item_type.clone(),
            service_id: body.service_id.clone(),
            title: body.title.clone(),
            artist: body.artist.clone(),
            album: body.album.clone(),
            cover_url: body.cover_url.clone(),
            ai_generated: body.ai_generated,
            isrc: body.isrc.clone(),
            created_at: None,
        };
        return match tune_core::streaming::favorites_mirror::ajouter(&arc, &state.backend, id, &fav)
            .await
        {
            Ok(p) => {
                tune_streaming_http::purge_contenu_utilisateur(&body.service);
                let statut = match p {
                    tune_core::streaming::favorites_mirror::Propagation::Propage => {
                        StatusCode::CREATED
                    }
                    _ => StatusCode::ACCEPTED,
                };
                (
                    statut,
                    Json(json!({"ok": true, "miroir": p.en_json(&body.service)})),
                )
                    .into_response()
            }
            Err(e) => {
                (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response()
            }
        };
    }
    let repo = StreamingFavoritesRepo::with_backend(state.backend.clone());
    match repo.add(
        id,
        &body.item_type,
        &body.service,
        &body.service_id,
        body.title.as_deref(),
        body.artist.as_deref(),
        body.album.as_deref(),
        body.cover_url.as_deref(),
    ) {
        Ok(_) => {
            if let Some(ia) = body.ai_generated
                && let Err(e) =
                    repo.poser_ia(id, &body.item_type, &body.service, &body.service_id, ia)
            {
                // Le favori est écrit ; seul le marquage manque.
                tracing::warn!(erreur = %e, "favori_service_marquage_ia_impossible");
            }
            (StatusCode::CREATED, Json(json!({"ok": true}))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

async fn remove_streaming_favorite(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    profil: ActiveProfile,
    Json(body): Json<StreamingFavoriteRemove>,
) -> impl IntoResponse {
    if let Err(r) = profil_du_chemin_ou_404(id, profil) {
        return r;
    }
    // #5997 — service en miroir : retrait CHEZ le service, puis de tous les
    // profils ; si le service ne suit pas, le favori est masqué et retenté.
    if let Some(arc) = service_en_miroir(&state, &body.service).await {
        return match tune_core::streaming::favorites_mirror::retirer(
            &arc,
            &state.backend,
            &body.item_type,
            &body.service_id,
        )
        .await
        {
            Ok(p) => {
                tune_streaming_http::purge_contenu_utilisateur(&body.service);
                let statut = match p {
                    tune_core::streaming::favorites_mirror::Propagation::Propage => StatusCode::OK,
                    _ => StatusCode::ACCEPTED,
                };
                (
                    statut,
                    Json(json!({"ok": true, "miroir": p.en_json(&body.service)})),
                )
                    .into_response()
            }
            Err(e) => {
                (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response()
            }
        };
    }
    let repo = StreamingFavoritesRepo::with_backend(state.backend.clone());
    match repo.remove(id, &body.item_type, &body.service, &body.service_id) {
        Ok(_) => (StatusCode::OK, Json(json!({"ok": true}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

/// `?service=` : ne reprendre qu'un service. Absent, tous ceux qui sont
/// authentifiés.
#[derive(Deserialize)]
struct SyncQuery {
    service: Option<String>,
}

/// Reprend dans `streaming_favorites` les favoris posés CHEZ Qobuz/Tidal/…
/// (#3419).
///
/// Bertrand, 05/09/2026 : « j'ai 3 pistes Qobuz en favori !! » — et sa règle
/// « Favori · est · Piste » rendait 0 album. Elle avait raison : ces favoris
/// n'existaient pas dans la base de Tune. Seul le cœur cliqué DANS Tune
/// écrivait la table ; celui cliqué dans l'application du service — le geste
/// de loin le plus fréquent — n'y arrivait jamais.
///
/// **Le profil est celui de l'APPELANT**, comme les neuf autres routes de la
/// famille (`profil_du_chemin_ou_404`, #2560). Un import est une écriture de
/// favoris : le laisser viser un profil nommé dans le chemin rouvrirait très
/// exactement la faille que ce garde a fermée.
///
/// La reprise n'ajoute jamais qu'au profil interrogé, et ne retire rien : voir
/// [`tune_core::streaming::favorites_import`] pour ce qui est délibérément
/// laissé de côté (réconciliation, colonne d'origine, périodicité).
async fn sync_streaming_favorites(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    profil: ActiveProfile,
    Query(q): Query<SyncQuery>,
) -> Response {
    if let Err(r) = profil_du_chemin_ou_404(id, profil) {
        return r;
    }
    let comptes = reprendre_les_favoris(&state, id, q.service.as_deref()).await;
    Json(json!({ "profile_id": id, "services": comptes })).into_response()
}

/// Le corps de la reprise, partagé entre la route et le passage de démarrage.
///
/// Deux appelants, une seule règle : n'interroger que les services **activés
/// et authentifiés**. Un service déconnecté rendrait une erreur par type et
/// gonflerait `echecs` sans rien apprendre à personne.
///
/// Les `Arc` du registre sont clonés d'abord, verrou relâché : le registre est
/// un `Mutex` et les lectures qui suivent sont longues (réseau).
pub(crate) async fn reprendre_les_favoris(
    state: &AppState,
    profile_id: i64,
    service_demande: Option<&str>,
) -> Value {
    let arcs: Vec<(String, _)> = {
        let registre = state.services.lock().await;
        registre
            .list()
            .into_iter()
            .filter(|nom| service_demande.is_none_or(|d| d == nom))
            .filter_map(|nom| registre.get(&nom).map(|arc| (nom, arc)))
            .collect()
    };

    let mut comptes = serde_json::Map::new();
    let mut miroirs = Vec::new();
    for (nom, arc) in arcs {
        let svc = arc.read().await;
        if !svc.utilisable().await {
            continue;
        }
        // #5997 — un service en miroir fait le rafraîchissement complet
        // (ajouts ET retraits, écritures en attente poussées), plus bas.
        if svc.favoris_miroir() {
            drop(svc);
            miroirs.push((nom, arc));
            continue;
        }
        let stats = tune_core::streaming::favorites_import::reprendre_les_favoris_du_service(
            &**svc,
            profile_id,
            &state.backend,
        )
        .await;
        comptes.insert(nom, json!(stats));
    }
    // Forcé, et attendu sans borne courte : c'est une demande explicite (route
    // `sync`) ou le passage de démarrage, pas l'ouverture d'un écran.
    comptes.extend(
        rafraichir_les_miroirs(
            state,
            &miroirs,
            profile_id,
            true,
            std::time::Duration::from_secs(120),
        )
        .await,
    );
    Value::Object(comptes)
}

/// `POST /profiles/{id}/favorites/streaming/reorder` — jumelle de
/// `reorder_favorites` pour les favoris de service **enregistrés chez Tune**.
///
/// ⚠️ Ne concerne PAS `/api/v1/streaming/{service}/favorites/{type}`, qui lit
/// les favoris directement chez Qobuz/Tidal : Tune ne possède pas ces lignes,
/// le service en renvoie un jeu différent à chaque resynchronisation, et leur
/// donner un rang durable demanderait une table de correspondance — arbitrage
/// non rendu (#2001).
async fn reorder_streaming_favorites(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    profil: ActiveProfile,
    Json(body): Json<ReorderStreamingFavorites>,
) -> Response {
    // Meme garde que la jumelle locale (#2560).
    if let Err(r) = profil_du_chemin_ou_404(id, profil) {
        return r;
    }
    let repo = StreamingFavoritesRepo::with_backend(state.backend.clone());
    let items: Vec<(String, String)> = body
        .items
        .into_iter()
        .map(|r| (r.service, r.service_id))
        .collect();
    match repo.reorder(id, &body.item_type, &items) {
        Ok(n) => (StatusCode::OK, Json(json!({"ok": true, "ordered": n}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

// --- Favoris de facette (label…) : une VALEUR, pas un identifiant (#2442) ---

async fn list_facet_favorites(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    profil: ActiveProfile,
    Query(q): Query<FacetsQuery>,
) -> Response {
    if let Err(r) = profil_du_chemin_ou_404(id, profil) {
        return r;
    }
    let repo = FavoriteFacetsRepo::with_backend(state.backend.clone());
    // Site nommé par la #2861 : une panne de base rendait `200 []`, que le
    // client ne distingue pas d'une liste de favoris vide. La réponse reste la
    // même — retirer ses favoris à quelqu'un parce qu'une requête a échoué
    // serait pire —, mais l'échec laisse désormais une trace.
    let items = repo.list(id, q.facet.as_deref()).ou_defaut_journalise();
    Json(json!(items)).into_response()
}

async fn add_facet_favorite(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    profil: ActiveProfile,
    Json(body): Json<FacetFavoriteAction>,
) -> impl IntoResponse {
    if let Err(r) = profil_du_chemin_ou_404(id, profil) {
        return r;
    }
    let repo = FavoriteFacetsRepo::with_backend(state.backend.clone());
    match repo.add(id, &body.facet, &body.value) {
        Ok(_) => (StatusCode::CREATED, Json(json!({"ok": true}))).into_response(),
        // Une valeur vide est une demande MALFORMÉE, pas une panne du serveur :
        // un 500 enverrait chercher la cause en base.
        Err(e) if e == "valeur de facette vide" => {
            (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

async fn remove_facet_favorite(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    profil: ActiveProfile,
    Json(body): Json<FacetFavoriteAction>,
) -> impl IntoResponse {
    if let Err(r) = profil_du_chemin_ou_404(id, profil) {
        return r;
    }
    let repo = FavoriteFacetsRepo::with_backend(state.backend.clone());
    match repo.remove(id, &body.facet, &body.value) {
        Ok(_) => (StatusCode::OK, Json(json!({"ok": true}))).into_response(),
        Err(e) if e == "valeur de facette vide" => {
            (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

// --- Advanced profile routes ---

async fn profile_settings(State(state): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("profile_{id}_settings");
    let value = settings
        .get(&key)
        .ok()
        .flatten()
        .unwrap_or_else(|| "{}".to_string());
    let parsed: Value = serde_json::from_str(&value).unwrap_or(json!({}));
    Json(parsed)
}

async fn update_profile_settings(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("profile_{id}_settings");
    let serialized = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
    match settings.set(&key, &serialized) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn profile_stats(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let b = &state.backend;

    // Favorites by type
    let fav_result: Result<Vec<(String, i64)>, String> = b
        .query_many(
            "SELECT item_type, COUNT(*) FROM favorites WHERE profile_id = ? GROUP BY item_type",
            &[&id as &dyn ToSqlValue],
        )
        .map(|rows| {
            rows.into_iter()
                .map(|r| {
                    (
                        r.first().and_then(|v| v.as_string()).unwrap_or_default(),
                        r.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                    )
                })
                .collect()
        });

    let mut favorites_by_type = json!({});
    if let Ok(rows) = &fav_result {
        for (item_type, count) in rows {
            favorites_by_type[item_type] = json!(count);
        }
    }

    // Per-profile listening stats (profile_id NULL = legacy entries, belong to default)
    let profile_filter = if id == 1 {
        // Default profile sees entries with profile_id = 1 OR NULL (legacy)
        "(profile_id = 1 OR profile_id IS NULL)".to_string()
    } else {
        format!("profile_id = {id}")
    };

    let listens_row = b
        .query_one(
            &format!(
                "SELECT COUNT(*), COALESCE(SUM(duration_ms), 0) \
                 FROM listen_history WHERE {profile_filter}"
            ),
            &[],
        )
        .ok()
        .flatten()
        .unwrap_or_default();

    let total_listens = listens_row.first().and_then(|v| v.as_i64()).unwrap_or(0);
    let total_ms = listens_row.get(1).and_then(|v| v.as_i64()).unwrap_or(0);

    let top_artists: Vec<serde_json::Value> = b
        .query_many(
            &format!(
                "SELECT artist_name, COUNT(*) as plays FROM listen_history \
                 WHERE {profile_filter} AND artist_name IS NOT NULL \
                 GROUP BY artist_name ORDER BY plays DESC LIMIT 10"
            ),
            &[],
        )
        .ou_defaut_journalise()
        .into_iter()
        .map(|cols| {
            json!({
                "artist": cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                "plays": cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect();

    let top_tracks: Vec<serde_json::Value> = b
        .query_many(
            &format!(
                "SELECT title, artist_name, COUNT(*) as plays FROM listen_history \
                 WHERE {profile_filter} \
                 GROUP BY title, artist_name ORDER BY plays DESC LIMIT 10"
            ),
            &[],
        )
        .ou_defaut_journalise()
        .into_iter()
        .map(|cols| {
            json!({
                "title": cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                "artist": cols.get(1).and_then(|v| v.as_string()),
                "plays": cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect();

    // Ratings count
    let ratings_count = b
        .query_one(
            "SELECT COUNT(*) FROM album_ratings WHERE profile_id = ?",
            &[&id as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0);

    Json(json!({
        "profile_id": id,
        "favorites_by_type": favorites_by_type,
        "listening": {
            "total_listens": total_listens,
            "total_duration_ms": total_ms,
            "total_hours": (total_ms as f64 / 3_600_000.0 * 10.0).round() / 10.0,
            "top_artists": top_artists,
            "top_tracks": top_tracks,
        },
        "ratings_count": ratings_count,
    }))
    .into_response()
}

async fn profile_history(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<HistoryQuery>,
) -> Json<Value> {
    let limit = q.limit.unwrap_or(50);
    let profile_filter = if id == 1 {
        "(profile_id = 1 OR profile_id IS NULL)".to_string()
    } else {
        format!("profile_id = {id}")
    };
    let sql = format!(
        "SELECT id, track_id, title, artist_name, album_title, source, source_id, \
         album_id, duration_ms, listened_at, zone_id \
         FROM listen_history WHERE {profile_filter} \
         ORDER BY listened_at DESC LIMIT ?",
    );
    let rows = state
        .backend
        .query_many(&sql, &[&limit as &dyn ToSqlValue])
        .ou_defaut_journalise();
    let items: Vec<Value> = rows
        .iter()
        .map(|cols| {
            json!({
                "id": cols.first().and_then(|v| v.as_i64()),
                "track_id": cols.get(1).and_then(|v| v.as_i64()),
                "title": cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                "artist_name": cols.get(3).and_then(|v| v.as_string()),
                "album_title": cols.get(4).and_then(|v| v.as_string()),
                "source": cols.get(5).and_then(|v| v.as_string()).unwrap_or_else(|| "local".into()),
                "source_id": cols.get(6).and_then(|v| v.as_string()),
                "album_id": cols.get(7).and_then(|v| v.as_i64()),
                "duration_ms": cols.get(8).and_then(|v| v.as_i64()).unwrap_or(0),
                "listened_at": cols.get(9).and_then(|v| v.as_string()),
                "zone_id": cols.get(10).and_then(|v| v.as_i64()),
            })
        })
        .collect();
    Json(json!(items))
}

async fn search_profiles(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Json<Value> {
    let repo = ProfileRepo::with_backend(state.backend.clone());
    let all = repo.list().unwrap_or_default();
    let query = q.q.unwrap_or_default().to_lowercase();
    if query.is_empty() {
        return Json(json!(all));
    }
    let filtered: Vec<_> = all
        .into_iter()
        .filter(|p| p.name.to_lowercase().contains(&query))
        .collect();
    Json(json!(filtered))
}

async fn check_favorites(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    profil: ActiveProfile,
    Json(body): Json<CheckFavoritesBody>,
) -> Response {
    if let Err(r) = profil_du_chemin_ou_404(id, profil) {
        return r;
    }
    let repo = ProfileRepo::with_backend(state.backend.clone());
    let results: Vec<Value> = body
        .item_ids
        .iter()
        .map(|&item_id| {
            let is_fav = repo
                .is_favorite(id, &body.item_type, item_id)
                .unwrap_or(false);
            json!({ "item_id": item_id, "is_favorite": is_fav })
        })
        .collect();
    Json(json!(results)).into_response()
}
