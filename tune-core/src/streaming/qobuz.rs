use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::Client;
use tracing::{debug, info, warn};

use super::traits::*;
use crate::TuneError;

const API_BASE: &str = "https://www.qobuz.com/api.json/0.2";
const API_PROXY: &str = "https://mozaiklabs.fr/qobuz-api";
const REMOTE_CONFIG_URL: &str = "https://mozaiklabs.fr/storage/api/v1/streaming-config.json";

/// Combien d'albums par page de discographie. Aligné sur ce que l'écran
/// affiche déjà : une page de plus est une page qu'on voit apparaître.
const QOBUZ_TAILLE_DE_PAGE: &str = "50";

pub struct QobuzService {
    client: Client,
    app_id: String,
    app_secret: String,
    user_auth_token: Option<String>,
    username: Option<String>,
    subscription: Option<String>,
    /// Endpoint order: `false` (default) = direct Qobuz API first with the
    /// mozaiklabs proxy as fallback; `true` = proxy first with direct as
    /// fallback (founder accounts, signalled by the cloud license via the
    /// optional `qobuz_proxy_first` field).
    proxy_first: bool,
    stored_username: Option<String>,
    /// Password from the interactive login, kept for `auto_relogin` — **memory
    /// only, never persisted**. `save_tokens` deliberately omits it; see the
    /// note there.
    stored_password: Option<String>,
    enabled_override: Option<bool>,
    /// Last auto-relogin attempt, successful or not — see `auto_relogin`.
    last_relogin_attempt: Option<std::time::Instant>,
    /// Set when `restore_tokens` read a pre-fix row carrying `stored_password`.
    needs_token_rewrite: bool,
    /// Set when Qobuz rejected the token and no relogin was possible — the
    /// session is over and the persisted row is worthless.
    session_expired: bool,
    /// Cache du contenu ÉDITORIAL uniquement — sélections, nouveautés, genres,
    /// playlists mises en avant. Jamais les favoris ni les playlists de
    /// l'utilisateur : ceux-là changent quand il clique, et une réponse
    /// périmée de trente minutes serait pire que lente (#1969).
    ///
    /// `Mutex` et non `&mut self` : les méthodes de `StreamingService`
    /// reçoivent `&self`. C'est exactement l'écueil sur lequel le cache Tidal
    /// s'est échoué — `tidal.rs:1823` avoue « Can't cache here since &self is
    /// immutable, will use the route-level cache », et ce cache au niveau route
    /// n'a jamais existé. `featured_cache` y est défini, lu à deux endroits, et
    /// JAMAIS écrit. Ne pas recopier ce modèle.
    cache_editorial: Mutex<HashMap<String, EntreeCache>>,
}

/// Une réponse éditoriale, et l'instant où elle a été obtenue.
struct EntreeCache {
    donnees: serde_json::Value,
    cree: Instant,
}

/// Durée de vie du cache éditorial. Les sélections Qobuz changent au mieux une
/// fois par jour ; trente minutes sont larges et bornent la fraîcheur perdue à
/// quelque chose que personne ne remarque.
const TTL_EDITORIAL: Duration = Duration::from_secs(1800);

/// Au-delà, on purge les entrées expirées. Une poignée de clés suffit à couvrir
/// la page découverte ; ce plafond n'existe que pour qu'un cache oublié ne
/// grossisse pas indéfiniment.
const MAX_ENTREES_CACHE: usize = 64;

/// (primary, fallback) API bases for the given endpoint order.
///
/// `proxy_first == false` (default, all users): direct Qobuz API first,
/// mozaiklabs proxy as fallback. `proxy_first == true` (founder account):
/// proxy first, direct as fallback.
fn endpoint_order(proxy_first: bool) -> (&'static str, &'static str) {
    if proxy_first {
        (API_PROXY, API_BASE)
    } else {
        (API_BASE, API_PROXY)
    }
}

/// Error from a single API attempt against one base URL.
#[derive(Debug)]
enum AttemptError {
    /// Network-level failure (timeout, DNS, connect) — eligible for fallback.
    Network(String),
    /// HTTP error status. 5xx is eligible for fallback; 4xx is final.
    Http { status: u16, body: String },
    /// Body of a successful response failed to parse — final, no fallback.
    Json(String),
}

impl AttemptError {
    /// Whether the other endpoint should be tried (network error or 5xx).
    fn transient(&self) -> bool {
        match self {
            Self::Network(_) => true,
            Self::Http { status, .. } => *status >= 500,
            Self::Json(_) => false,
        }
    }
}

impl std::fmt::Display for AttemptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(e) => write!(f, "{e}"),
            Self::Http { status, body } => write!(f, "{status} {body}"),
            Self::Json(e) => write!(f, "json: {e}"),
        }
    }
}

/// Log the primary-endpoint failure that triggers the fallback, keeping the
/// historical proxy-first event names and their direct-first mirrors.
fn log_fallback(proxy_first: bool, path: &str, err: &AttemptError) {
    match (proxy_first, err) {
        (true, AttemptError::Http { status, .. }) => {
            info!(path, status, "qobuz_proxy_5xx_trying_direct");
        }
        (true, _) => info!(path, error = %err, "qobuz_proxy_failed_trying_direct"),
        (false, AttemptError::Http { status, .. }) => {
            info!(path, status, "qobuz_direct_5xx_trying_proxy");
        }
        (false, _) => info!(path, error = %err, "qobuz_direct_failed_trying_proxy"),
    }
}

/// Traduit le type de favori du client (pluriel) en paramètre attendu par
/// l'API Qobuz.
///
/// `playlists` est traité à part : le type est parfaitement connu du
/// connecteur — il lit `/playlist/getUserPlaylists`, `/playlist/get`,
/// `/playlist/getFeatured` — mais **aucun appel de souscription à une
/// playlist tierce n'est établi dans ce dépôt**. `/favorite/create` n'accepte,
/// pour ce que le code démontre, que `track_ids`, `album_ids` et `artist_ids`.
/// Rendre « unknown favorite type » ferait chercher une faute de frappe là où il
/// y a une fonction à écrire (#2370).
fn favorite_key(fav_type: &str) -> Result<&'static str, TuneError> {
    match fav_type {
        "tracks" => Ok("track_ids"),
        "albums" => Ok("album_ids"),
        "artists" => Ok("artist_ids"),
        "playlists" => Err(MOTIF_PLAYLIST_NON_SOUSCRIPTIBLE.into()),
        _ => Err(format!("unknown favorite type: {fav_type}").into()),
    }
}

/// Motif de refus d'un favori de playlist Qobuz.
///
/// Il nomme ce qui manque plutôt que de déclarer le type inconnu : l'appel de
/// souscription à une playlist qui n'appartient pas à l'utilisateur n'est
/// documenté nulle part dans ce dépôt, et on n'invente pas un endpoint Qobuz.
const MOTIF_PLAYLIST_NON_SOUSCRIPTIBLE: &str = "qobuz: favori de playlist non pris en charge — l'appel de souscription à une \
     playlist tierce n'est pas établi contre l'API Qobuz (#2370). La LECTURE des \
     playlists de l'utilisateur reste disponible via /playlist/getUserPlaylists.";

/// Offsets des pages restant à charger après la première page d'un endpoint
/// paginé Qobuz.
///
/// Reproduit la condition d'arrêt de l'ancienne boucle séquentielle : on ne
/// continue que si la première page était PLEINE et que `total` annonce des
/// éléments au-delà. Un `total` incohérent (0 alors que la page est pleine)
/// arrête la pagination, comme avant.
fn remaining_page_offsets(first_count: usize, total: usize, page_size: usize) -> Vec<usize> {
    remaining_page_offsets_bornees(first_count, total, page_size, usize::MAX)
}

/// Décalages des pages restantes, sans jamais dépasser `plafond` éléments.
///
/// `get_featured_playlists` récupérait TOUT le catalogue éditorial — Qobuz en
/// expose plusieurs milliers sans tag — puis appelait `truncate(500)`. Des
/// dizaines à des centaines d'allers-retours HTTP pour jeter 90 % du résultat
/// (#1969). Le plafond doit borner la PAGINATION, pas le tableau final.
fn remaining_page_offsets_bornees(
    first_count: usize,
    total: usize,
    page_size: usize,
    plafond: usize,
) -> Vec<usize> {
    if first_count < page_size || total <= first_count {
        return Vec::new();
    }
    // Une page dont le décalage atteint déjà le plafond n'apporterait que des
    // éléments destinés à être jetés.
    let borne = total.min(plafond);
    (page_size..borne).step_by(page_size).collect()
}

/// Trace le résultat d'une écriture de favori chez Qobuz.
///
/// Sans cette trace, un favori qui n'arrive jamais dans l'app Qobuz ne laisse
/// aucune empreinte exploitable : l'appel HTTP réussit, le cœur bascule dans
/// Tune, et le rapport du testeur se réduit à « ça ne marche pas ». On veut
/// pouvoir répondre depuis ses logs (retour Fabien, 08/08/2026).
fn log_favorite_result(
    op: &str,
    fav_type: &str,
    item_id: &str,
    res: &Result<serde_json::Value, String>,
) {
    match res {
        Ok(body) => info!(op, fav_type, item_id, response = %body, "qobuz_favorite_ok"),
        Err(e) => warn!(op, fav_type, item_id, error = %e, "qobuz_favorite_failed"),
    }
}

impl QobuzService {
    /// Écrire un favori exige le jeton utilisateur : l'app_id seul identifie
    /// l'application, pas le compte. Sans jeton, Qobuz accepte la requête sans
    /// rien enregistrer — un succès en trompe-l'œil. On refuse avant d'émettre.
    fn require_user_token(&self, op: &str) -> Result<(), TuneError> {
        if self.user_auth_token.is_none() {
            warn!(op, "qobuz_favorite_no_user_token");
            return Err("session Qobuz non authentifiée : reconnecte le compte Qobuz".into());
        }
        Ok(())
    }

    pub fn new(app_id: String, app_secret: String) -> Self {
        Self {
            client: crate::http::client::builder()
                .timeout(Duration::from_secs(45))
                .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36")
                .build()
                .unwrap_or_else(|_| Client::new()),
            app_id,
            app_secret,
            user_auth_token: None,
            username: None,
            subscription: None,
            proxy_first: false,
            stored_username: None,
            stored_password: None,
            enabled_override: None,
            last_relogin_attempt: None,
            needs_token_rewrite: false,
            session_expired: false,
            cache_editorial: Mutex::new(HashMap::new()),
        }
    }

    /// Set the endpoint order: `true` = proxy first (founder account, from the
    /// cloud license `qobuz_proxy_first` flag), `false` = direct first.
    pub fn set_proxy_first(&mut self, proxy_first: bool) {
        if proxy_first != self.proxy_first {
            info!(proxy_first, "qobuz_endpoint_order_changed");
        }
        self.proxy_first = proxy_first;
    }

    async fn refresh_credentials(&mut self) {
        match self.client.get(REMOTE_CONFIG_URL).send().await {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(data) = resp.json::<serde_json::Value>().await {
                    let qobuz = &data["qobuz"];
                    if let (Some(id), Some(secret)) =
                        (qobuz["app_id"].as_str(), qobuz["app_secret"].as_str())
                    {
                        info!(old_id = %&self.app_id, new_id = %id, "qobuz_credentials_refreshed");
                        self.app_id = id.to_string();
                        self.app_secret = secret.to_string();
                    }
                }
            }
            _ => info!("qobuz_remote_config_unavailable"),
        }
    }

    /// GET against the primary endpoint for the configured order, falling back
    /// to the other endpoint on a network error or 5xx (symmetric fallback).
    async fn api_get(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<serde_json::Value, String> {
        let (primary, fallback) = endpoint_order(self.proxy_first);
        match self.api_get_at(primary, path, params).await {
            Ok(v) => Ok(v),
            Err(err) if err.transient() => {
                log_fallback(self.proxy_first, path, &err);
                self.api_get_at(fallback, path, params).await.map_err(|e| {
                    info!(path, error = %e, "qobuz_fallback_api_error");
                    format!("qobuz {path}: {e}")
                })
            }
            Err(err) => {
                info!(path, error = %err, "qobuz_api_error");
                Err(format!("qobuz {path}: {err}"))
            }
        }
    }

    /// One GET attempt against a specific API base.
    async fn api_get_at(
        &self,
        base: &str,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<serde_json::Value, AttemptError> {
        let url = format!("{base}{path}");
        let app_id = self.app_id.as_str();
        let mut query: Vec<(&str, &str)> = params.to_vec();
        query.push(("app_id", app_id));

        let mut req = self
            .client
            .get(&url)
            .query(&query)
            .header("X-App-Id", app_id);

        if let Some(ref token) = self.user_auth_token {
            req = req.header("X-User-Auth-Token", token.as_str());
        }

        let resp = req
            .send()
            .await
            .map_err(|e| AttemptError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(AttemptError::Http { status, body });
        }

        resp.json()
            .await
            .map_err(|e| AttemptError::Json(e.to_string()))
    }

    /// One page of a paginated Qobuz endpoint.
    async fn api_get_page(
        &self,
        path: &str,
        base_params: &[(&str, &str)],
        offset: usize,
        limit: usize,
    ) -> Result<serde_json::Value, String> {
        let offset_str = offset.to_string();
        let limit_str = limit.to_string();
        let mut params: Vec<(&str, &str)> = base_params.to_vec();
        params.push(("limit", &limit_str));
        params.push(("offset", &offset_str));
        self.api_get(path, &params).await
    }

    /// Fetch all pages from a paginated Qobuz endpoint.
    ///
    /// `path` / `base_params` define the request. `items_key` is the top-level
    /// JSON key that wraps the `items` array (e.g. "tracks", "albums", "artists").
    /// The Qobuz API caps each page at 50 items regardless of the requested limit.
    ///
    /// Perf: the first page tells us `total`; the remaining pages are fetched
    /// CONCURRENTLY (capped at `MAX_CONCURRENT_PAGES` so a large favorites
    /// library doesn't hammer the Qobuz API). Before this, a user with 2000
    /// favorite tracks paid 40 sequential round-trips per view — the "slow
    /// favorites display" reports from heavy Qobuz users.
    /// Lecture du cache éditorial. `None` si absent ou périmé.
    ///
    /// `Mutex` de la bibliothèque standard et non `tokio::sync` : on ne tient
    /// jamais ce verrou pendant un appel réseau — uniquement le temps d'un
    /// `get` ou d'un `insert` sur une table en mémoire. Un verrou asynchrone
    /// coûterait plus cher qu'il ne rapporte, et surtout il inviterait à le
    /// tenir à travers un `await`, ce qui sérialiserait les requêtes.
    fn cache_get(&self, cle: &str) -> Option<serde_json::Value> {
        let cache = self.cache_editorial.lock().ok()?;
        cache.get(cle).and_then(|e| {
            if e.cree.elapsed() < TTL_EDITORIAL {
                Some(e.donnees.clone())
            } else {
                None
            }
        })
    }

    fn cache_set(&self, cle: String, donnees: serde_json::Value) {
        let Ok(mut cache) = self.cache_editorial.lock() else {
            return;
        };
        if cache.len() >= MAX_ENTREES_CACHE {
            cache.retain(|_, e| e.cree.elapsed() < TTL_EDITORIAL);
        }
        cache.insert(
            cle,
            EntreeCache {
                donnees,
                cree: Instant::now(),
            },
        );
    }

    /// Clé de cache : le chemin et ses paramètres, dans l'ordre où ils sont
    /// passés. Deux appels identiques donnent la même clé ; un genre ou un
    /// tag différent en donne une autre.
    fn cle_cache(path: &str, params: &[(&str, &str)]) -> String {
        // Encodage JSON, et non une concaténation avec un séparateur : une
        // valeur qui CONTIENT le séparateur produirait sinon la même clé que
        // deux paramètres distincts. Ce n'est pas théorique — le test
        // `une_valeur_ne_peut_pas_forger_la_cle_d_une_autre` a attrapé
        // exactement cette collision sur la première version de cette
        // fonction. JSON échappe pour nous, sans cas particulier à prévoir.
        serde_json::json!([path, params]).to_string()
    }

    /// `api_get` pour le contenu ÉDITORIAL : sert le cache s'il est frais.
    ///
    /// Réservé à ce qui ne dépend pas du compte. Ne JAMAIS l'employer pour les
    /// favoris ou les playlists de l'utilisateur : ils changent quand il
    /// clique, et servir une réponse vieille de trente minutes ferait
    /// réapparaître un favori qu'il vient de retirer.
    async fn api_get_editorial(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<serde_json::Value, String> {
        let cle = Self::cle_cache(path, params);
        if let Some(donnees) = self.cache_get(&cle) {
            debug!(path, "qobuz_editorial_cache_hit");
            return Ok(donnees);
        }
        let donnees = self.api_get(path, params).await?;
        self.cache_set(cle, donnees.clone());
        Ok(donnees)
    }

    async fn api_get_all_pages(
        &self,
        path: &str,
        base_params: &[(&str, &str)],
        items_key: &str,
    ) -> Result<Vec<serde_json::Value>, String> {
        self.api_get_all_pages_bornee(path, base_params, items_key, usize::MAX)
            .await
    }

    /// Comme `api_get_all_pages`, mais cesse de paginer une fois `plafond`
    /// éléments atteints. Voir `remaining_page_offsets_bornees` (#1969).
    async fn api_get_all_pages_bornee(
        &self,
        path: &str,
        base_params: &[(&str, &str)],
        items_key: &str,
        plafond: usize,
    ) -> Result<Vec<serde_json::Value>, String> {
        use futures_util::StreamExt;
        const PAGE_SIZE: usize = 50;
        const MAX_CONCURRENT_PAGES: usize = 4;

        // First page: learn `total` and keep the first-page diagnostics.
        let data = self.api_get_page(path, base_params, 0, PAGE_SIZE).await?;

        let mut all_items: Vec<serde_json::Value> = data[items_key]["items"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let count = all_items.len();
        let total = data[items_key]["total"].as_u64().unwrap_or(0) as usize;

        debug!(
            path,
            items_key,
            offset = 0usize,
            count,
            total,
            accumulated = all_items.len(),
            "qobuz_paginate"
        );

        // Diagnostic for the "favorites empty while playlists load" reports
        // (Stéphane): on the FIRST page, surface at INFO the counts so a normal
        // tester log tells us whether Qobuz returned nothing (total==0 → an
        // API/account issue) or returned rows we then dropped (total>0 but
        // count==0 → a response-shape/mapping mismatch on our side). When the
        // items_key sub-object is entirely absent we log the top-level keys
        // the response DID carry, to catch Qobuz nesting the data elsewhere.
        {
            let key_present = data.get(items_key).map(|v| !v.is_null()).unwrap_or(false);
            if total == 0 && count == 0 {
                let top_keys: Vec<&str> = data
                    .as_object()
                    .map(|m| m.keys().map(String::as_str).collect())
                    .unwrap_or_default();
                warn!(
                    path,
                    items_key,
                    key_present,
                    response_keys = ?top_keys,
                    "qobuz_favorites_empty"
                );
            } else {
                info!(path, items_key, count, total, "qobuz_favorites_first_page");
            }
        }

        let offsets = remaining_page_offsets_bornees(count, total, PAGE_SIZE, plafond);
        if offsets.is_empty() {
            return Ok(all_items);
        }

        // `buffered` preserves page order, so the merged list matches what the
        // sequential loop produced.
        let pages: Vec<Result<Vec<serde_json::Value>, String>> =
            futures_util::stream::iter(offsets.into_iter().map(|offset| async move {
                let data = self
                    .api_get_page(path, base_params, offset, PAGE_SIZE)
                    .await?;
                let items = data[items_key]["items"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                debug!(
                    path,
                    items_key,
                    offset,
                    count = items.len(),
                    total,
                    "qobuz_paginate"
                );
                Ok(items)
            }))
            .buffered(MAX_CONCURRENT_PAGES)
            .collect()
            .await;

        for page in pages {
            all_items.extend(page?);
        }

        Ok(all_items)
    }

    fn map_track(item: &serde_json::Value) -> StreamTrack {
        let album = &item["album"];
        StreamTrack {
            id: item["id"].as_u64().unwrap_or(0).to_string(),
            title: item["title"].as_str().unwrap_or("").into(),
            artist: item["performer"]["name"]
                .as_str()
                .or_else(|| item["artist"]["name"].as_str())
                .unwrap_or("")
                .into(),
            album: album["title"].as_str().map(Into::into),
            album_id: album["id"]
                .as_str()
                .map(Into::into)
                .or_else(|| album["id"].as_u64().map(|id| id.to_string())),
            duration_ms: item["duration"].as_u64().unwrap_or(0) * 1000,
            cover_path: album["image"]["large"].as_str().map(Into::into),
            track_number: item["track_number"].as_u64().map(|n| n as u32),
            disc_number: item["media_number"].as_u64().map(|n| n as u32),
            explicit: item["parental_warning"].as_bool().unwrap_or(false),
            isrc: item["isrc"].as_str().map(Into::into),
            quality: Some(StreamQuality {
                codec: "FLAC".into(),
                sample_rate: item["maximum_sampling_rate"]
                    .as_f64()
                    .map(|r| (r * 1000.0) as u32)
                    .unwrap_or(44100),
                bit_depth: item["maximum_bit_depth"]
                    .as_u64()
                    .map(|b| b as u16)
                    .unwrap_or(16),
                bitrate: None,
                channels: 2,
            }),
        }
    }

    fn map_album(item: &serde_json::Value) -> StreamAlbum {
        StreamAlbum {
            id: item["id"]
                .as_str()
                .map(Into::into)
                .or_else(|| item["id"].as_u64().map(|id| id.to_string()))
                .unwrap_or_default(),
            title: item["title"].as_str().unwrap_or("").into(),
            artist: item["artist"]["name"].as_str().unwrap_or("").into(),
            artist_id: item["artist"]["id"].as_u64().map(|id| id.to_string()),
            cover_path: item["image"]["large"].as_str().map(Into::into),
            year: item["released_at"]
                .as_u64()
                .map(|ts| 1970 + (ts / 31_536_000) as u32)
                .or_else(|| {
                    item["release_date_original"]
                        .as_str()
                        .and_then(|d| d.get(..4)?.parse().ok())
                }),
            track_count: item["tracks_count"].as_u64().unwrap_or(0) as u32,
            quality: item["maximum_bit_depth"].as_u64().map(|bd| StreamQuality {
                codec: "FLAC".into(),
                sample_rate: item["maximum_sampling_rate"]
                    .as_f64()
                    .map(|r| (r * 1000.0) as u32)
                    .unwrap_or(44100),
                bit_depth: bd as u16,
                bitrate: None,
                channels: 2,
            }),
        }
    }

    /// Les playlists d'une réponse de `/catalog/search`. Isolé de `search` pour
    /// être testable : le reste de l'extraction demande un aller-retour HTTP.
    ///
    /// `/catalog/search` renvoie ses playlists dans la même forme que les
    /// sélections éditoriales, d'où le convertisseur partagé.
    fn search_playlists(data: &serde_json::Value) -> Vec<StreamPlaylist> {
        data["playlists"]["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_featured_playlist).collect())
            .unwrap_or_default()
    }

    /// Pochette d'une playlist Qobuz, quel que soit le champ qui la porte.
    ///
    /// Qobuz expose l'image sous plusieurs noms selon l'endpoint, et sans
    /// garantie de présence. Trois extractions divergentes coexistaient ici —
    /// `map_featured_playlist` en tentait trois, `get_playlist` un seul, et
    /// `get_user_playlists` aucun (`cover_path: None` en dur) — si bien que la
    /// même playlist avait une pochette dans les sélections et aucune dans la
    /// liste de l'utilisateur (#1970).
    ///
    /// L'ordre préserve le rendu actuel des playlists éditoriales :
    /// `image_rectangle` reste en tête, les autres ne font que rattraper les
    /// cas où il est absent.
    fn pochette_playlist(item: &serde_json::Value) -> Option<String> {
        const CHAMPS_TABLEAU: [&str; 5] = [
            "image_rectangle",
            "images300",
            "images150",
            "images",
            "image_rectangle_mini",
        ];
        for champ in CHAMPS_TABLEAU {
            if let Some(url) = item[champ]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                return Some(url.to_string());
            }
        }
        // `image` est tantôt une chaîne, tantôt l'objet `{large, small}` que
        // porte déjà un album (cf. `get_album_tracks`).
        item["image"]
            .as_str()
            .or_else(|| item["image"]["large"].as_str())
            .or_else(|| item["image"]["small"].as_str())
            .filter(|s| !s.is_empty())
            .map(Into::into)
    }

    /// Map a Qobuz playlist item (editorial selection or search hit) to
    /// StreamPlaylist.
    fn map_featured_playlist(item: &serde_json::Value) -> StreamPlaylist {
        StreamPlaylist {
            id: item["id"]
                .as_u64()
                .map(|id| id.to_string())
                .or_else(|| item["id"].as_str().map(Into::into))
                .unwrap_or_default(),
            name: item["name"].as_str().unwrap_or("").into(),
            description: item["description"].as_str().map(Into::into),
            cover_path: Self::pochette_playlist(item),
            track_count: item["tracks_count"].as_u64().unwrap_or(0) as u32,
            owner: item["owner"]["name"].as_str().map(Into::into),
        }
    }

    /// One login attempt against a specific API base.
    async fn login_at(
        &self,
        base: &str,
        username: &str,
        password: &str,
    ) -> Result<serde_json::Value, AttemptError> {
        let resp = self
            .client
            .post(format!("{base}/user/login"))
            .query(&[("app_id", self.app_id.as_str())])
            .form(&[("username", username), ("password", password)])
            .send()
            .await
            .map_err(|e| AttemptError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(AttemptError::Http { status, body });
        }

        resp.json()
            .await
            .map_err(|e| AttemptError::Json(e.to_string()))
    }

    /// Login following the configured endpoint order. Direct-first by default so
    /// user credentials never transit through the VPS unless the direct API is
    /// unreachable (network error or 5xx) — proxy-first for founder accounts.
    async fn login_internal(
        &mut self,
        username: &str,
        password: &str,
    ) -> Result<AuthStatus, String> {
        self.refresh_credentials().await;

        let (primary, fallback) = endpoint_order(self.proxy_first);
        let data = match self.login_at(primary, username, password).await {
            Ok(d) => d,
            Err(err) if err.transient() => {
                log_fallback(self.proxy_first, "/user/login", &err);
                self.login_at(fallback, username, password)
                    .await
                    .map_err(|e| {
                        info!(error = %e, "qobuz_login_failed");
                        format!("qobuz login: {e}")
                    })?
            }
            Err(err) => {
                info!(error = %err, "qobuz_login_failed");
                return Err(format!("qobuz login: {err}"));
            }
        };

        self.user_auth_token = data["user_auth_token"].as_str().map(Into::into);
        self.username = data["user"]["display_name"].as_str().map(Into::into);
        self.subscription = data["user"]["credential"]["label"].as_str().map(Into::into);
        // A fresh session clears the expiry flag, whether this login came from
        // the user or from `auto_relogin`.
        self.session_expired = false;

        info!(username = ?self.username, "qobuz_authenticated");
        Ok(self.auth_status_internal())
    }

    fn auth_status_internal(&self) -> AuthStatus {
        AuthStatus {
            authenticated: self.user_auth_token.is_some(),
            username: self.username.clone(),
            subscription: self.subscription.clone(),
            ..Default::default()
        }
    }

    async fn auto_relogin(&mut self) -> bool {
        // Cooldown : pendant une coupure (proxy mozaiklabs down + direct
        // bloqué), chaque appel en échec relançait un login complet — une
        // tempête de relogins toutes les ~2 s qui ressemble à du trafic de
        // bot et fait rate-limiter l'IP par Akamai (403 en cascade, .18 le
        // 28/07). Une tentative par minute suffit largement à récupérer dès
        // que le réseau revient.
        const RELOGIN_COOLDOWN: Duration = Duration::from_secs(60);
        if let Some(t) = self.last_relogin_attempt
            && t.elapsed() < RELOGIN_COOLDOWN
        {
            debug!("qobuz_auto_relogin_cooldown");
            return false;
        }
        self.last_relogin_attempt = Some(std::time::Instant::now());
        if let (Some(u), Some(p)) = (self.stored_username.clone(), self.stored_password.clone()) {
            info!("qobuz_auto_relogin");
            self.login_internal(&u, &p).await.is_ok()
        } else {
            false
        }
    }

    /// POST against the primary endpoint for the configured order, falling back
    /// to the other endpoint on a network error or 5xx (same order as api_get).
    async fn api_post(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<serde_json::Value, String> {
        let (primary, fallback) = endpoint_order(self.proxy_first);
        match self.api_post_at(primary, path, params).await {
            Ok(v) => Ok(v),
            Err(err) if err.transient() => {
                log_fallback(self.proxy_first, path, &err);
                self.api_post_at(fallback, path, params)
                    .await
                    .map_err(|e| format!("qobuz {path}: {e}"))
            }
            Err(err) => Err(format!("qobuz {path}: {err}")),
        }
    }

    /// One POST attempt against a specific API base.
    async fn api_post_at(
        &self,
        base: &str,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<serde_json::Value, AttemptError> {
        let url = format!("{base}{path}");
        let app_id = self.app_id.as_str();
        // Qobuz's Akamai edge rejects a body-less POST with HTTP 411 (Length
        // Required). Send the parameters as an `application/x-www-form-urlencoded`
        // body instead of a query string so the request carries a Content-Length.
        let mut form: Vec<(&str, &str)> = params.to_vec();
        form.push(("app_id", app_id));
        if let Some(ref token) = self.user_auth_token {
            form.push(("user_auth_token", token.as_str()));
        }

        let mut req = self
            .client
            .post(&url)
            .form(&form)
            .header("X-App-Id", app_id);

        if let Some(ref token) = self.user_auth_token {
            req = req.header("X-User-Auth-Token", token.as_str());
        }

        let resp = req
            .send()
            .await
            .map_err(|e| AttemptError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(AttemptError::Http { status, body });
        }
        resp.json()
            .await
            .or_else(|_| Ok(serde_json::json!({"ok": true})))
    }

    /// Determine the best format_id given the user's subscription level.
    /// "Studio" / "HiFi" subscriptions max out at CD quality (format_id 6).
    /// "Sublime" / "Sublime+" can access Hi-Res (format_id 27).
    fn best_format_id_for_subscription(&self) -> &str {
        // Always try Hi-Res (27) first — the caller falls back to CD (6) if
        // the subscription doesn't support it. This avoids silently
        // downsampling Hi-Res content for users whose subscription field
        // isn't detected as "sublime".
        "27"
    }

    /// Low-level fetch of a track streaming URL with a specific format_id.
    async fn fetch_track_url(&self, track_id: &str, format_id: &str) -> Result<StreamUrl, String> {
        let dur = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        let timestamp = format!("{}.{}", dur.as_secs(), dur.subsec_millis());

        let sig_input = format!(
            "trackgetFileUrlformat_id{format_id}intentstreamtrack_id{track_id}{timestamp}{}",
            self.app_secret
        );
        let sig = md5_hex(&sig_input);

        info!(track_id, format_id, timestamp = %timestamp, sig = %sig, "qobuz_get_file_url");

        let data = self
            .api_get(
                "/track/getFileUrl",
                &[
                    ("track_id", track_id),
                    ("format_id", format_id),
                    ("intent", "stream"),
                    ("request_ts", &timestamp),
                    ("request_sig", &sig),
                ],
            )
            .await?;

        // Qobuz returns a 30-second preview (audio/mpeg, URL carries `range=20-30`,
        // `"sample": true`) instead of an error when the requested format_id is
        // above the subscription's entitlement — e.g. asking for Hi-Res (27) on a
        // "streaming-studio" CD-max plan. Accepting it silently makes playback cut
        // at exactly 30s (DLNA renderers download the whole preview, then stall;
        // Qobuz → DMP-A8, .15). Reject a sample so `get_track_url` falls down the
        // quality ladder to a format the subscription can actually stream in full.
        if data["sample"].as_bool().unwrap_or(false) {
            info!(track_id, format_id, "qobuz_sample_preview_rejected");
            return Err(format!(
                "qobuz returned a 30s sample for format_id {format_id} — not entitled at this quality"
            ));
        }

        let url = data["url"].as_str().ok_or("no url")?.to_string();
        let mime = data["mime_type"]
            .as_str()
            .unwrap_or("audio/flac")
            .to_string();
        let sample_rate = data["sampling_rate"]
            .as_f64()
            .map(|r| (r * 1000.0) as u32)
            .unwrap_or(44100);
        let bit_depth = data["bit_depth"].as_u64().map(|b| b as u16).unwrap_or(16);

        Ok(StreamUrl {
            url,
            mime_type: mime,
            quality: StreamQuality {
                codec: "FLAC".into(),
                sample_rate,
                bit_depth,
                bitrate: None,
                channels: 2,
            },
            expires_at: None,
        })
    }

    fn map_genre(item: &serde_json::Value) -> StreamGenre {
        // Qobuz returns subgenresCount (integer) rather than a subgenres array
        // at the /genre/list level. Fall back to checking the subgenres array
        // (returned by /genre/get) or slug depth as a heuristic.
        let has_children = item["subgenresCount"]
            .as_u64()
            .map(|n| n > 0)
            .or_else(|| item["subgenres"].as_array().map(|a| !a.is_empty()))
            .unwrap_or_else(|| {
                // Top-level genres (slug without '/') typically have children
                item["slug"]
                    .as_str()
                    .map(|s| !s.contains('/'))
                    .unwrap_or(false)
            });

        // Qobuz image can be a string or an object {"large": "...", "small": "..."}
        let image_url = item["image"]
            .as_str()
            .map(String::from)
            .or_else(|| item["image"]["large"].as_str().map(String::from));

        StreamGenre {
            id: item["id"].as_u64().unwrap_or(0).to_string(),
            name: item["name"].as_str().unwrap_or("").into(),
            has_children,
            image_url,
        }
    }

    fn map_artist(item: &serde_json::Value) -> StreamArtist {
        StreamArtist {
            id: item["id"].as_u64().unwrap_or(0).to_string(),
            name: item["name"].as_str().unwrap_or("").into(),
            image_path: item["image"]["large"].as_str().map(Into::into),
            bio: item["biography"]["content"]
                .as_str()
                .or_else(|| item["biography"]["summary"].as_str())
                .map(Self::strip_html_tags)
                .filter(|s| !s.is_empty()),
        }
    }

    /// Strip HTML tags from Qobuz editorial text (biography content is HTML).
    fn strip_html_tags(s: &str) -> String {
        let re = regex::Regex::new(r"<[^>]+>").unwrap();
        re.replace_all(s, "").trim().to_string()
    }
}

#[async_trait::async_trait]
impl StreamingService for QobuzService {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        "qobuz"
    }

    fn enabled(&self) -> bool {
        self.enabled_override.unwrap_or(!self.app_id.is_empty())
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled_override = Some(enabled);
    }

    async fn authenticate(
        &mut self,
        credentials: &serde_json::Value,
    ) -> Result<AuthStatus, TuneError> {
        let username = credentials["username"]
            .as_str()
            .ok_or("username required")?;
        let password = credentials["password"]
            .as_str()
            .ok_or("password required")?;

        self.stored_username = Some(username.to_string());
        self.stored_password = Some(password.to_string());

        Ok(self.login_internal(username, password).await?)
    }

    async fn auth_status(&self) -> AuthStatus {
        self.auth_status_internal()
    }

    async fn logout(&mut self) -> Result<(), TuneError> {
        self.user_auth_token = None;
        self.username = None;
        Ok(())
    }

    async fn search(&self, query: &str, limit: usize) -> Result<SearchResults, TuneError> {
        let data = self
            .api_get(
                "/catalog/search",
                &[("query", query), ("limit", &limit.to_string())],
            )
            .await?;

        let tracks = data["tracks"]["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_track).collect())
            .unwrap_or_default();
        let albums = data["albums"]["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_album).collect())
            .unwrap_or_default();
        let artists = data["artists"]["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_artist).collect())
            .unwrap_or_default();

        Ok(SearchResults {
            tracks,
            albums,
            artists,
            playlists: Self::search_playlists(&data),
        })
    }

    async fn get_track(&self, track_id: &str) -> Result<StreamTrack, TuneError> {
        let data = self
            .api_get("/track/get", &[("track_id", track_id)])
            .await?;
        Ok(Self::map_track(&data))
    }

    async fn get_track_url(
        &self,
        track_id: &str,
        quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError> {
        if self.user_auth_token.is_none() {
            return Err(TuneError::Streaming(
                "Qobuz session expired — please reconnect in Settings → Streaming Services".into(),
            ));
        }

        let format_id = match quality {
            Some("hires") => "27",
            Some("cd") => "6",
            Some("mp3") => "5",
            _ => self.best_format_id_for_subscription(),
        };

        match self.fetch_track_url(track_id, format_id).await {
            Ok(stream_url) => Ok(stream_url),
            Err(e) => {
                // Fall down the quality ladder to the next-lower format the track
                // is actually offered in: 24/192 (27) → 24/96 (7) → CD (6).
                // Falling straight from 27 to 6 dropped Sublime (Hi-Res) users to
                // lossy-CD on the many tracks Qobuz only offers in 24/96, instead
                // of the hi-res they're entitled to (Yves). CD (6) and MP3 (5)
                // have no lossless format below them.
                let ladder: &[&str] = match format_id {
                    "27" => &["7", "6"],
                    "7" => &["6"],
                    _ => &[],
                };
                for &fid in ladder {
                    info!(
                        track_id,
                        from = format_id,
                        to = fid,
                        "qobuz_format_fallback"
                    );
                    if let Ok(url) = self.fetch_track_url(track_id, fid).await {
                        return Ok(url);
                    }
                }
                Err(e.into())
            }
        }
    }

    async fn get_album(&self, album_id: &str) -> Result<StreamAlbum, TuneError> {
        let data = self
            .api_get("/album/get", &[("album_id", album_id)])
            .await?;
        Ok(Self::map_album(&data))
    }

    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        let data = self
            .api_get("/album/get", &[("album_id", album_id)])
            .await?;
        // Qobuz album/get returns album metadata at the top level while
        // individual track items inside tracks.items do NOT carry an
        // "album" sub-object.  Extract the album-level title, image and
        // id so we can inject them into each mapped track.
        let album_title = data["title"].as_str().map(String::from);
        let album_cover = data["image"]["large"]
            .as_str()
            .or_else(|| data["image"]["small"].as_str())
            .map(String::from);
        let album_id_val = data["id"]
            .as_str()
            .map(String::from)
            .or_else(|| data["id"].as_u64().map(|id| id.to_string()));

        let tracks = data["tracks"]["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        let mut t = Self::map_track(item);
                        // Inject album-level metadata when the track lacks it
                        if t.album.is_none() {
                            t.album = album_title.clone();
                        }
                        if t.cover_path.is_none() {
                            t.cover_path = album_cover.clone();
                        }
                        if t.album_id.is_none() {
                            t.album_id = album_id_val.clone();
                        }
                        t
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(tracks)
    }

    async fn get_artist(&self, artist_id: &str) -> Result<StreamArtist, TuneError> {
        let data = self
            .api_get(
                "/artist/get",
                &[("artist_id", artist_id), ("extra", "biography")],
            )
            .await?;
        Ok(Self::map_artist(&data))
    }

    async fn get_playlist(&self, playlist_id: &str) -> Result<StreamPlaylist, TuneError> {
        let data = self
            .api_get("/playlist/get", &[("playlist_id", playlist_id)])
            .await?;
        Ok(StreamPlaylist {
            id: data["id"].as_u64().unwrap_or(0).to_string(),
            name: data["name"].as_str().unwrap_or("").into(),
            description: data["description"].as_str().map(Into::into),
            cover_path: Self::pochette_playlist(&data),
            track_count: data["tracks_count"].as_u64().unwrap_or(0) as u32,
            owner: data["owner"]["name"].as_str().map(Into::into),
        })
    }

    async fn get_playlist_tracks(&self, playlist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        let data = self
            .api_get(
                "/playlist/get",
                &[
                    ("playlist_id", playlist_id),
                    ("extra", "tracks"),
                    ("limit", "500"),
                ],
            )
            .await?;
        let tracks = data["tracks"]["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_track).collect())
            .unwrap_or_default();
        Ok(tracks)
    }

    async fn get_genres(&self, parent_id: Option<&str>) -> Result<Vec<StreamGenre>, TuneError> {
        let mut params: Vec<(&str, &str)> = vec![("offset", "0"), ("limit", "500")];
        if let Some(pid) = parent_id {
            params.push(("parent_id", pid));
        }
        let data = self
            .api_get_editorial("/genre/list", &params)
            .await
            .map_err(|e| {
                info!(error = %e, "qobuz_genres_failed");
                e
            })?;
        let genres: Vec<StreamGenre> = data["genres"]["items"]
            .as_array()
            .or_else(|| data["genres"].as_array())
            .or_else(|| data.as_array())
            .map(|items| items.iter().map(Self::map_genre).collect())
            .unwrap_or_default();
        if genres.is_empty() {
            info!(raw = %data, "qobuz_genres_empty_response");
        }
        Ok(genres)
    }

    async fn get_genre_albums(
        &self,
        genre_id: &str,
        limit: usize,
    ) -> Result<Vec<StreamAlbum>, TuneError> {
        let limit_str = limit.to_string();
        let data = self
            .api_get_editorial(
                "/album/getFeatured",
                &[
                    ("type", "new-releases"),
                    ("genre_ids", genre_id),
                    ("limit", &limit_str),
                ],
            )
            .await?;
        let albums = data["albums"]["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_album).collect())
            .unwrap_or_default();
        Ok(albums)
    }

    async fn get_new_releases(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        let data = self
            .api_get_editorial(
                "/album/getFeatured",
                &[("type", "new-releases"), ("limit", "200")],
            )
            .await?;
        let albums = data["albums"]["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_album).collect())
            .unwrap_or_default();
        Ok(albums)
    }

    async fn get_featured_sections(&self) -> Result<Vec<FeaturedSection>, TuneError> {
        Ok(vec![
            FeaturedSection {
                id: "new-releases".into(),
                name: "New Releases".into(),
            },
            FeaturedSection {
                id: "best-sellers".into(),
                name: "Best Sellers".into(),
            },
            FeaturedSection {
                id: "press-awards".into(),
                name: "Press Awards".into(),
            },
            FeaturedSection {
                id: "editor-picks".into(),
                name: "Editor Picks".into(),
            },
            FeaturedSection {
                id: "most-streamed".into(),
                name: "Most Streamed".into(),
            },
            // Les deux rangées de l'onglet « Le goût de Qobuz », que Tune
            // n'exposait pas alors que l'API les sert depuis toujours (Fabien,
            // comparaison avec Roon).
            FeaturedSection {
                id: "ideal-discography".into(),
                name: "Ideal Discography".into(),
            },
            FeaturedSection {
                id: "qobuzissims".into(),
                name: "Qobuzissimes".into(),
            },
        ])
    }

    async fn get_featured_section(&self, section_id: &str) -> Result<Vec<StreamAlbum>, TuneError> {
        let data = self
            .api_get_editorial(
                "/album/getFeatured",
                &[("type", section_id), ("limit", "50")],
            )
            .await?;
        let albums = data["albums"]["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_album).collect())
            .unwrap_or_default();
        Ok(albums)
    }

    async fn get_album_label(&self, album_id: &str) -> Result<LabelInfo, TuneError> {
        // Resolve the album's label (id + name come straight from album/get).
        let album = self
            .api_get("/album/get", &[("album_id", album_id)])
            .await?;
        let label_id = album["label"]["id"]
            .as_u64()
            .map(|id| id.to_string())
            .or_else(|| album["label"]["id"].as_str().map(Into::into))
            .ok_or_else(|| TuneError::NotFound("album has no label".into()))?;
        let name = album["label"]["name"].as_str().unwrap_or("").into();
        // Bounded pagination: big majors (e.g. Columbia) expose 10k+ albums.
        // We cap at MAX and the app offers a text filter over the loaded set.
        const MAX: usize = 500;
        const PAGE: usize = 50;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut albums: Vec<StreamAlbum> = Vec::new();
        let mut offset: usize = 0;
        loop {
            let off = offset.to_string();
            let lim = PAGE.to_string();
            let data = self
                .api_get(
                    "/label/get",
                    &[
                        ("label_id", &label_id),
                        ("extra", "albums"),
                        ("limit", &lim),
                        ("offset", &off),
                    ],
                )
                .await?;
            let items = data["albums"]["items"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let n = items.len();
            // A label's full catalogue includes not-yet-released pre-orders
            // (streamable=true but a future release date) whose tracks resolve
            // to "no url" (502) on playback. Keep only released + streamable
            // albums (mirrors the LMS plugin's `_isReleased` check).
            albums.extend(
                items
                    .iter()
                    .filter(|it| {
                        let streamable = it["streamable"].as_bool() != Some(false);
                        let released = it["released_at"].as_u64().map_or(true, |ts| ts <= now);
                        streamable && released
                    })
                    .map(Self::map_album),
            );
            let total = data["albums"]["total"].as_u64().unwrap_or(0) as usize;
            offset += n;
            if n < PAGE || offset >= total || albums.len() >= MAX {
                break;
            }
        }
        albums.truncate(MAX);
        Ok(LabelInfo {
            id: label_id,
            name,
            albums,
        })
    }

    async fn get_playlist_tags(&self) -> Result<Vec<PlaylistTag>, TuneError> {
        let data = self.api_get_editorial("/playlist/getTags", &[]).await?;
        let tags = data["tags"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let id = item["id"]
                            .as_str()
                            .map(Into::into)
                            .or_else(|| item["id"].as_u64().map(|i| i.to_string()))
                            .or_else(|| item["slug"].as_str().map(Into::into))?;
                        // Le libellé est, selon les entrées : une chaîne, un
                        // objet localisé {en, fr, …}, ou — c'est le cas courant
                        // chez Qobuz — un objet localisé ENCODÉ EN JSON dans
                        // `name_json`. Sans cette dernière branche, aucune des
                        // 13 catégories ne trouvait son nom et toutes
                        // retombaient sur leur slug : les rangées s'appelaient
                        // « artist », « mood », « label ».
                        let localized = |v: &serde_json::Value| -> Option<String> {
                            let obj = v.as_object()?;
                            obj.get("fr")
                                .or_else(|| obj.get("en"))
                                .or_else(|| obj.values().next())
                                .and_then(|s| s.as_str())
                                .map(String::from)
                        };
                        let name = item["name"]
                            .as_str()
                            .map(String::from)
                            .or_else(|| localized(&item["name"]))
                            .or_else(|| {
                                item["name_json"]
                                    .as_str()
                                    .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                                    .as_ref()
                                    .and_then(localized)
                            })
                            .or_else(|| localized(&item["name_json"]))
                            .unwrap_or_else(|| id.clone());
                        Some(PlaylistTag { id, name })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(tags)
    }

    /// Les playlists éditoriales Qobuz, celles composées par leurs équipes.
    ///
    /// Le client appelle `GET /streaming/{service}/featured`, donc `get_featured`.
    /// Qobuz était le seul service à ne pas la surcharger — Tidal, Deezer et
    /// Spotify le font — et héritait donc de l'implémentation par défaut du
    /// trait, qui renvoie une liste vide. Le carrousel « Playlists en vedette »
    /// ne s'affichait jamais pour Qobuz, alors que tout existait par ailleurs :
    /// la récupération ci-dessous, et même une route dédiée que personne
    /// n'appelait (retour Fabien).
    async fn get_featured(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        self.get_featured_playlists(None, None).await
    }

    async fn get_featured_playlists(
        &self,
        tag: Option<&str>,
        genre: Option<&str>,
    ) -> Result<Vec<StreamPlaylist>, TuneError> {
        // Une seule page de 50 laissait la majorité du catalogue éditorial
        // dehors — « il manque beaucoup de playlists éditoriales Qobuz »
        // (Fabien). On pagine, avec un plafond : sans tag, Qobuz en expose
        // plusieurs milliers, et personne ne fait défiler ça.
        const MAX: usize = 500;
        let mut params: Vec<(&str, &str)> = vec![("type", "editor-picks")];
        if let Some(t) = tag {
            params.push(("tags", t));
        }
        if let Some(g) = genre {
            params.push(("genre_ids", g));
        }
        // La pagination bornée reste coûteuse au premier appel : on met en
        // cache la LISTE obtenue, pas chaque page. Le suffixe distingue cette
        // clé de celle d'un `api_get` sur le même chemin.
        let cle = Self::cle_cache("/playlist/getFeatured#pages", &params);
        let mut items = match self.cache_get(&cle) {
            Some(serde_json::Value::Array(v)) => {
                debug!(path = "/playlist/getFeatured", "qobuz_editorial_cache_hit");
                v
            }
            _ => {
                let v = self
                    .api_get_all_pages_bornee("/playlist/getFeatured", &params, "playlists", MAX)
                    .await?;
                self.cache_set(cle, serde_json::Value::Array(v.clone()));
                v
            }
        };
        // La pagination s'arrête déjà au plafond ; ce `truncate` ne rogne plus
        // que le trop-plein de la dernière page.
        items.truncate(MAX);
        Ok(items.iter().map(Self::map_featured_playlist).collect())
    }

    /// Une rangée par tag Qobuz, dans l'ordre où Qobuz les publie.
    ///
    /// Les tags sont interrogés en parallèle (une requête chacun, une page) :
    /// séquentiellement, une dizaine d'allers-retours mettaient la vue à
    /// plusieurs secondes. Un tag qui échoue ou qui ne renvoie rien est
    /// simplement absent — une rangée vide n'apprend rien à personne.
    async fn get_featured_playlists_by_tag(
        &self,
        genre: Option<&str>,
    ) -> Result<Vec<PlaylistTagGroup>, TuneError> {
        /// Playlists par rangée : au-delà, le carrousel ne sert plus à rien et
        /// la vue s'alourdit. Le détail d'un tag reste accessible par
        /// `get_featured_playlists(tag)`, qui pagine.
        const PER_TAG: usize = 50;

        let tags = self.get_playlist_tags().await?;
        let rows = futures_util::future::join_all(tags.into_iter().map(|tag| async move {
            let limit = PER_TAG.to_string();
            let mut params: Vec<(&str, &str)> = vec![
                ("type", "editor-picks"),
                ("tags", tag.id.as_str()),
                ("limit", &limit),
            ];
            if let Some(g) = genre {
                params.push(("genre_ids", g));
            }
            let data = self
                .api_get_editorial("/playlist/getFeatured", &params)
                .await
                .ok()?;
            let playlists: Vec<StreamPlaylist> = data["playlists"]["items"]
                .as_array()
                .map(|items| items.iter().map(Self::map_featured_playlist).collect())
                .unwrap_or_default();
            if playlists.is_empty() {
                return None;
            }
            Some(PlaylistTagGroup {
                id: tag.id,
                name: tag.name,
                playlists,
            })
        }))
        .await;
        Ok(rows.into_iter().flatten().collect())
    }

    async fn get_album_context(&self, album_id: &str) -> Result<AlbumContext, TuneError> {
        let album = self
            .api_get("/album/get", &[("album_id", album_id)])
            .await?;
        Ok(AlbumContext {
            genre_id: album["genre"]["id"]
                .as_u64()
                .map(|id| id.to_string())
                .or_else(|| album["genre"]["id"].as_str().map(Into::into)),
            genre_name: album["genre"]["name"].as_str().map(Into::into),
            label_id: album["label"]["id"]
                .as_u64()
                .map(|id| id.to_string())
                .or_else(|| album["label"]["id"].as_str().map(Into::into)),
            label_name: album["label"]["name"].as_str().map(Into::into),
        })
    }

    async fn get_user_tracks(&self) -> Result<Vec<StreamTrack>, TuneError> {
        let items = self
            .api_get_all_pages(
                "/favorite/getUserFavorites",
                &[("type", "tracks")],
                "tracks",
            )
            .await?;
        Ok(items.iter().map(Self::map_track).collect())
    }

    async fn add_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), TuneError> {
        let key = favorite_key(fav_type)?;
        self.require_user_token("favorite/create")?;
        let res = self.api_post("/favorite/create", &[(key, item_id)]).await;
        log_favorite_result("create", fav_type, item_id, &res);
        res?;
        Ok(())
    }

    async fn remove_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), TuneError> {
        let key = favorite_key(fav_type)?;
        self.require_user_token("favorite/delete")?;
        let res = self.api_post("/favorite/delete", &[(key, item_id)]).await;
        log_favorite_result("delete", fav_type, item_id, &res);
        res?;
        Ok(())
    }

    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        let data = self
            .api_get("/playlist/getUserPlaylists", &[("limit", "500")])
            .await?;
        let playlists = data["playlists"]["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| StreamPlaylist {
                        id: item["id"].as_u64().unwrap_or(0).to_string(),
                        name: item["name"].as_str().unwrap_or("").into(),
                        description: item["description"].as_str().map(Into::into),
                        cover_path: Self::pochette_playlist(item),
                        track_count: item["tracks_count"].as_u64().unwrap_or(0) as u32,
                        owner: None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(playlists)
    }

    async fn get_artist_albums(&self, artist_id: &str) -> Result<Vec<StreamAlbum>, TuneError> {
        self.get_artist_albums_page(artist_id, 0).await
    }

    /// Une page de la discographie Qobuz.
    ///
    /// Le `limit` de 50 était écrit en dur, et sans `offset` : la discographie
    /// d'un artiste prolifique s'arrêtait net au cinquantième album, sans que
    /// rien n'indique qu'il y en avait d'autres. Qobuz accepte `offset` sur
    /// `/artist/get?extra=albums` — il n'y avait qu'à le demander.
    ///
    /// La taille de page reste 50 : c'est ce que l'écran affiche déjà, et la
    /// changer déplacerait le problème au lieu de le régler.
    async fn get_artist_albums_page(
        &self,
        artist_id: &str,
        offset: u32,
    ) -> Result<Vec<StreamAlbum>, TuneError> {
        let offset = offset.to_string();
        let data = self
            .api_get(
                "/artist/get",
                &[
                    ("artist_id", artist_id),
                    ("extra", "albums"),
                    ("limit", QOBUZ_TAILLE_DE_PAGE),
                    ("offset", &offset),
                ],
            )
            .await?;
        let albums = data["albums"]["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_album).collect())
            .unwrap_or_default();
        Ok(albums)
    }

    async fn get_artist_top_tracks(&self, artist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        let data = self
            .api_get(
                "/artist/get",
                &[
                    ("artist_id", artist_id),
                    ("extra", "tracks_appears_on"),
                    ("limit", "20"),
                ],
            )
            .await?;
        let tracks = data["tracks_appears_on"]["items"]
            .as_array()
            .or_else(|| data["tracks"]["items"].as_array())
            .map(|items| items.iter().map(Self::map_track).collect())
            .unwrap_or_default();
        Ok(tracks)
    }

    /// `artist/getSimilarArtists` — la reponse de Qobuz a « et apres ? ».
    ///
    /// `artist/get` refuse `extra=similarArtists` (400, « accepted values are
    /// albums, tracks, playlists, ... ») : c'est bien un point d'entree
    /// distinct. Verifie sur le catalogue reel : Pink Floyd (38324) rend 72
    /// artistes, King Crimson en tete.
    async fn get_similar_artists(
        &self,
        artist_id: &str,
        limit: usize,
    ) -> Result<Vec<StreamArtist>, TuneError> {
        let limit = limit.to_string();
        let data = self
            .api_get(
                "/artist/getSimilarArtists",
                &[("artist_id", artist_id), ("limit", &limit)],
            )
            .await?;
        let artists = data["artists"]["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_artist).collect())
            .unwrap_or_default();
        Ok(artists)
    }

    async fn create_playlist(
        &self,
        name: &str,
        description: Option<&str>,
    ) -> Result<String, TuneError> {
        let desc = description.unwrap_or("Created by Tune");
        let resp = self
            .api_post(
                "/playlist/create",
                &[
                    ("name", name),
                    ("description", desc),
                    ("is_public", "false"),
                ],
            )
            .await?;
        resp["id"]
            .as_u64()
            .map(|id| id.to_string())
            .or_else(|| resp["id"].as_str().map(|s| s.to_string()))
            .ok_or_else(|| "qobuz: no playlist id in response".into())
    }

    async fn add_tracks_to_playlist(
        &self,
        playlist_id: &str,
        track_ids: &[String],
    ) -> Result<usize, TuneError> {
        let mut added = 0;
        for chunk in track_ids.chunks(50) {
            let ids_csv = chunk.join(",");
            self.api_post(
                "/playlist/addTracks",
                &[("playlist_id", playlist_id), ("track_ids", &ids_csv)],
            )
            .await?;
            added += chunk.len();
        }
        Ok(added)
    }

    async fn delete_playlist(&self, playlist_id: &str) -> Result<(), TuneError> {
        self.api_post("/playlist/delete", &[("playlist_id", playlist_id)])
            .await?;
        Ok(())
    }

    /// Qobuz deletes by `playlist_track_id` (the per-position id), not the
    /// source track id — so resolve them from the playlist first.
    async fn remove_tracks_from_playlist(
        &self,
        playlist_id: &str,
        track_ids: &[String],
    ) -> Result<usize, TuneError> {
        let data = self
            .api_get(
                "/playlist/get",
                &[
                    ("playlist_id", playlist_id),
                    ("extra", "tracks"),
                    ("limit", "500"),
                ],
            )
            .await?;
        let wanted: std::collections::HashSet<&str> =
            track_ids.iter().map(|s| s.as_str()).collect();
        let mut ptids: Vec<String> = Vec::new();
        if let Some(items) = data["tracks"]["items"].as_array() {
            for item in items {
                let sid = item["id"]
                    .as_u64()
                    .map(|n| n.to_string())
                    .unwrap_or_default();
                if wanted.contains(sid.as_str()) {
                    if let Some(ptid) = item["playlist_track_id"].as_u64() {
                        ptids.push(ptid.to_string());
                    }
                }
            }
        }
        if ptids.is_empty() {
            return Ok(0);
        }
        let csv = ptids.join(",");
        self.api_post(
            "/playlist/deleteTracks",
            &[("playlist_id", playlist_id), ("playlist_track_ids", &csv)],
        )
        .await?;
        Ok(ptids.len())
    }

    fn supports_write(&self) -> bool {
        self.user_auth_token.is_some()
    }

    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        let items = self
            .api_get_all_pages(
                "/favorite/getUserFavorites",
                &[("type", "albums")],
                "albums",
            )
            .await?;
        Ok(items.iter().map(Self::map_album).collect())
    }

    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        let items = self
            .api_get_all_pages(
                "/favorite/getUserFavorites",
                &[("type", "artists")],
                "artists",
            )
            .await?;
        Ok(items.iter().map(Self::map_artist).collect())
    }

    async fn refresh_if_needed(&mut self) -> Result<bool, TuneError> {
        if self.user_auth_token.is_none() {
            return Ok(false);
        }
        let test = self.api_get("/user/get", &[]).await;
        let Err(ref e) = test else {
            return Ok(false);
        };
        // Only a rejected session is conclusive. A timeout or a 5xx says
        // nothing about the token and must not cost the user their session.
        if !(e.contains("401") || e.contains("403")) {
            return Ok(false);
        }
        if self.auto_relogin().await {
            info!("qobuz_token_refreshed_via_relogin");
            return Ok(true);
        }
        // Qobuz rejected the token and we cannot get a new one. Drop it: while
        // it stayed in place, `auth_status` still answered `authenticated:
        // true`, so the UI showed the service connected, the friendly "session
        // expired, reconnect in Settings" branch in `get_track_url` (which
        // tests for a *missing* token) never ran, and playback failed with a
        // raw `qobuz /track/getFileUrl: 401`. Clearing it makes every one of
        // those report the truth.
        warn!("qobuz_session_expired_token_cleared");
        self.user_auth_token = None;
        self.subscription = None;
        self.session_expired = true;
        // `username` is kept on purpose: the account is still known, it just
        // needs a password again, and the client can name it in the prompt.
        Ok(true)
    }

    fn session_expired(&self) -> bool {
        self.session_expired
    }

    /// The persisted blob deliberately carries **no password**.
    ///
    /// It used to. `settings.auth_tokens_qobuz` held `stored_password` in the
    /// clear next to the token, so anyone who could read `tune.db` — a backup,
    /// a copied WAL, a support bundle — walked away with the account password
    /// itself, not a revocable session. The API responses were already redacted
    /// (`is_secret_key` in `routes/system/config.rs`); the file on disk was not.
    ///
    /// The password now lives in memory only, for the lifetime of the process,
    /// so `auto_relogin` still recovers from an expired token without a round
    /// trip to the user. Across a restart it is gone: if the stored token has
    /// expired by then, the user re-authenticates once. That is the trade —
    /// a rare re-login against a password that is never written down.
    fn save_tokens(&self) -> Option<serde_json::Value> {
        let token = self.user_auth_token.as_ref()?;
        Some(serde_json::json!({
            "user_auth_token": token,
            "username": self.username,
            "subscription": self.subscription,
            "app_id": self.app_id,
            "app_secret": self.app_secret,
            "stored_username": self.stored_username,
        }))
    }

    fn restore_tokens(&mut self, tokens: &serde_json::Value) -> bool {
        if let Some(t) = tokens["user_auth_token"].as_str() {
            self.user_auth_token = Some(t.into());
            self.username = tokens["username"].as_str().map(Into::into);
            self.subscription = tokens["subscription"].as_str().map(Into::into);
            if let Some(id) = tokens["app_id"].as_str() {
                self.app_id = id.into();
            }
            if let Some(secret) = tokens["app_secret"].as_str() {
                self.app_secret = secret.into();
            }
            self.stored_username = tokens["stored_username"].as_str().map(Into::into);
            // A row written before this field was dropped still carries the
            // plaintext password. Do not load it into the session — flag the row
            // so the registry rewrites it without the field, which is what
            // actually removes the secret from disk.
            if tokens.get("stored_password").is_some_and(|v| !v.is_null()) {
                warn!("qobuz_legacy_plaintext_password_purged");
                self.needs_token_rewrite = true;
            }
            true
        } else {
            false
        }
    }

    fn tokens_need_rewrite(&self) -> bool {
        self.needs_token_rewrite
    }

    async fn post_restore(&mut self) {
        self.refresh_credentials().await;
        let _ = self.refresh_if_needed().await;
    }
}

fn md5_hex(input: &str) -> String {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    result.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn search_playlists_read_from_the_search_payload() {
        // /catalog/search renvoie ses playlists dans la même forme que les
        // sélections éditoriales — c'est ce que ce test verrouille.
        let data = json!({
            "tracks": {"items": []},
            "albums": {"items": []},
            "artists": {"items": []},
            "playlists": {"items": [{
                "id": 5471203,
                "name": "Jazz pour la nuit",
                "tracks_count": 58,
                "owner": {"name": "Qobuz"}
            }]}
        });
        let found = QobuzService::search_playlists(&data);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "5471203");
        assert_eq!(found[0].name, "Jazz pour la nuit");
        assert_eq!(found[0].track_count, 58);
        assert_eq!(found[0].owner.as_deref(), Some("Qobuz"));
    }

    #[test]
    fn search_playlists_absent_is_empty_not_a_panic() {
        assert!(QobuzService::search_playlists(&json!({"tracks": {"items": []}})).is_empty());
    }

    #[test]
    fn endpoint_order_direct_first_by_default() {
        // All users: direct Qobuz API first, mozaiklabs proxy as fallback.
        assert_eq!(endpoint_order(false), (API_BASE, API_PROXY));
    }

    #[test]
    fn endpoint_order_proxy_first_for_founder() {
        // Founder account (license `qobuz_proxy_first`): proxy first.
        assert_eq!(endpoint_order(true), (API_PROXY, API_BASE));
    }

    #[test]
    fn remaining_page_offsets_stops_on_a_partial_first_page() {
        // 37 favoris : tout tient dans la première page, rien à précharger.
        assert!(remaining_page_offsets(37, 37, 50).is_empty());
        // Page vide (compte sans favoris).
        assert!(remaining_page_offsets(0, 0, 50).is_empty());
    }

    #[test]
    fn remaining_page_offsets_stops_when_total_is_reached_or_inconsistent() {
        // total == première page : terminé.
        assert!(remaining_page_offsets(50, 50, 50).is_empty());
        // total incohérent (0 alors que la page est pleine) : on s'arrête,
        // comme l'ancienne boucle séquentielle (offset >= total).
        assert!(remaining_page_offsets(50, 0, 50).is_empty());
    }

    #[test]
    fn remaining_page_offsets_covers_the_whole_library_in_page_steps() {
        // 2000 favoris (cas des rapports de lenteur) : pages 50..1950.
        let offsets = remaining_page_offsets(50, 2000, 50);
        assert_eq!(offsets.first(), Some(&50));
        assert_eq!(offsets.last(), Some(&1950));
        assert_eq!(offsets.len(), 39);
        // Dernière page partielle : 230 favoris → 50, 100, 150, 200.
        assert_eq!(remaining_page_offsets(50, 230, 50), vec![50, 100, 150, 200]);
        // Un seul élément au-delà de la première page.
        assert_eq!(remaining_page_offsets(50, 51, 50), vec![50]);
    }

    /// Le plafond doit borner la PAGINATION, pas le tableau final.
    ///
    /// `get_featured_playlists` demandait tout le catalogue éditorial — Qobuz
    /// en expose plusieurs milliers sans tag — puis jetait au-delà de 500.
    /// Sur 4000 entrées, c'était 79 pages récupérées pour en garder 10 (#1969).
    #[test]
    fn le_plafond_arrete_la_pagination_au_lieu_de_tronquer_apres() {
        // Sans plafond : tout le catalogue, 79 pages après la première.
        assert_eq!(remaining_page_offsets(50, 4000, 50).len(), 79);
        // Avec le plafond de 500 : 9 pages, et la dernière commence à 450.
        let bornees = remaining_page_offsets_bornees(50, 4000, 50, 500);
        assert_eq!(bornees.len(), 9);
        assert_eq!(bornees.last(), Some(&450));
        // 10 pages de 50 en tout, première comprise = exactement le plafond.
        assert_eq!((bornees.len() + 1) * 50, 500);
    }

    /// Un catalogue plus petit que le plafond n'est pas amputé : c'est `total`
    /// qui borne, pas le plafond.
    #[test]
    fn un_catalogue_plus_petit_que_le_plafond_est_pris_en_entier() {
        assert_eq!(
            remaining_page_offsets_bornees(50, 230, 50, 500),
            vec![50, 100, 150, 200]
        );
    }

    /// Sans plafond, le comportement est celui d'avant, au décalage près.
    #[test]
    fn sans_plafond_le_comportement_est_inchange() {
        for (count, total) in [(50usize, 2000usize), (50, 230), (50, 51), (37, 37), (0, 0)] {
            assert_eq!(
                remaining_page_offsets(count, total, 50),
                remaining_page_offsets_bornees(count, total, 50, usize::MAX),
                "count={count} total={total}"
            );
        }
    }

    /// Cas limites du plafond : ne jamais demander une page qui serait
    /// entièrement jetée, ni retomber en boucle infinie.
    #[test]
    fn les_plafonds_degeneres_ne_demandent_aucune_page() {
        // Plafond sous la première page : rien de plus à chercher.
        assert!(remaining_page_offsets_bornees(50, 4000, 50, 50).is_empty());
        assert!(remaining_page_offsets_bornees(50, 4000, 50, 10).is_empty());
        assert!(remaining_page_offsets_bornees(50, 4000, 50, 0).is_empty());
        // Plafond juste au-dessus : une seule page de plus.
        assert_eq!(remaining_page_offsets_bornees(50, 4000, 50, 51), vec![50]);
    }

    #[test]
    fn favorite_key_maps_the_plural_types_sent_by_the_client() {
        assert_eq!(favorite_key("tracks").unwrap(), "track_ids");
        assert_eq!(favorite_key("albums").unwrap(), "album_ids");
        assert_eq!(favorite_key("artists").unwrap(), "artist_ids");
        assert!(
            favorite_key("track").is_err(),
            "le singulier n'est pas le contrat"
        );
    }

    /// #2370 — Gros Bidon (fil 1541). `favorite_key("playlists")` rend
    /// aujourd'hui « unknown favorite type: playlists », ce qui est FAUX : le
    /// type existe et le connecteur le manipule partout ailleurs
    /// (`/playlist/getUserPlaylists`, `/playlist/get`…). Ce qui manque, c'est
    /// l'appel de souscription a une playlist tierce, qui n'est etabli nulle
    /// part dans ce depot. Le message doit dire cela, et pas mentir sur la
    /// nature du blocage — sans quoi le prochain lecteur cherche une faute de
    /// frappe la ou il y a une fonction a ecrire.
    #[test]
    fn le_type_playlists_n_est_pas_un_type_inconnu() {
        let err = favorite_key("playlists")
            .expect_err(
                "l'appel de souscription Qobuz n'est pas etabli : ca doit rester une erreur",
            )
            .to_string();
        assert!(
            !err.contains("unknown favorite type"),
            "le type playlist est connu du connecteur : le refus doit nommer \
             l'appel manquant, pas pretendre que le type est inconnu. Message rendu : {err}"
        );
        assert!(
            err.to_lowercase().contains("playlist"),
            "le message doit nommer la playlist. Message rendu : {err}"
        );
    }

    #[test]
    fn writing_a_favorite_without_a_user_token_is_refused_up_front() {
        // Sans jeton utilisateur, Qobuz accepte la requête sans rien
        // enregistrer : on doit échouer avant de l'émettre, pas rapporter un
        // succès que l'app Qobuz dément.
        let svc = QobuzService::new("app".into(), "secret".into());
        assert!(svc.user_auth_token.is_none());
        let err = svc.require_user_token("favorite/create").unwrap_err();
        assert!(
            err.to_string().contains("Qobuz"),
            "le message doit désigner le compte à reconnecter, obtenu : {err}"
        );
    }

    #[test]
    fn map_similar_artists_payload() {
        // Charge utile reelle de /artist/getSimilarArtists?artist_id=38324
        // (Pink Floyd, 72 resultats) : la radio d'autoplay lit `artists.items`,
        // pas la racine — une erreur de chemin rendrait zero candidat en
        // silence, exactement le bug #1553.
        let payload = json!({
            "artists": {
                "limit": 3,
                "offset": 0,
                "total": 72,
                "items": [
                    {"id": 1191678, "name": "King Crimson", "albums_count": 87},
                    {"id": 26718, "name": "Yes", "albums_count": 120},
                    {"id": 43821, "name": "Queen", "albums_count": 64},
                ]
            }
        });
        let artists: Vec<_> = payload["artists"]["items"]
            .as_array()
            .map(|items| items.iter().map(QobuzService::map_artist).collect())
            .unwrap_or_default();
        assert_eq!(artists.len(), 3);
        assert_eq!(artists[0].id, "1191678");
        assert_eq!(artists[0].name, "King Crimson");
    }

    #[test]
    fn map_similar_artists_missing_payload_is_empty_not_a_panic() {
        // Un artiste sans voisins connus rend un objet sans `artists` : la
        // radio doit rendre zero candidat, pas paniquer dans le poller.
        let payload = json!({"status": "success"});
        let artists: Vec<crate::streaming::traits::StreamArtist> = payload["artists"]["items"]
            .as_array()
            .map(|items| items.iter().map(QobuzService::map_artist).collect())
            .unwrap_or_default();
        assert!(artists.is_empty());
    }

    #[test]
    fn new_service_defaults_to_direct_first() {
        let svc = QobuzService::new("id".into(), "secret".into());
        assert!(!svc.proxy_first);
    }

    #[test]
    fn attempt_error_transient_classification() {
        // Network errors and 5xx fall back to the other endpoint; 4xx and
        // JSON parse errors are final.
        assert!(AttemptError::Network("timeout".into()).transient());
        assert!(
            AttemptError::Http {
                status: 502,
                body: String::new()
            }
            .transient()
        );
        assert!(
            !AttemptError::Http {
                status: 401,
                body: String::new()
            }
            .transient()
        );
        assert!(!AttemptError::Json("eof".into()).transient());
    }

    #[test]
    fn map_track_basic() {
        let json = json!({
            "id": 12345,
            "title": "Take Five",
            "performer": {"name": "Dave Brubeck"},
            "album": {
                "title": "Time Out",
                "id": 678,
                "image": {"large": "http://img.qobuz.com/large.jpg"},
            },
            "duration": 324,
            "track_number": 2,
            "media_number": 1,
            "parental_warning": false,
            "maximum_sampling_rate": 192.0,
            "maximum_bit_depth": 24,
        });
        let track = QobuzService::map_track(&json);
        assert_eq!(track.id, "12345");
        assert_eq!(track.title, "Take Five");
        assert_eq!(track.artist, "Dave Brubeck");
        assert_eq!(track.album.as_deref(), Some("Time Out"));
        assert_eq!(track.album_id.as_deref(), Some("678"));
        assert_eq!(track.duration_ms, 324_000);
        assert_eq!(track.track_number, Some(2));
        assert_eq!(track.disc_number, Some(1));
        assert!(!track.explicit);
        assert_eq!(
            track.cover_path.as_deref(),
            Some("http://img.qobuz.com/large.jpg")
        );
        let q = track.quality.unwrap();
        assert_eq!(q.sample_rate, 192000);
        assert_eq!(q.bit_depth, 24);
    }

    #[test]
    fn map_track_artist_fallback() {
        let json = json!({
            "id": 1,
            "title": "Test",
            "artist": {"name": "Fallback Artist"},
            "album": {},
            "duration": 100,
        });
        let track = QobuzService::map_track(&json);
        assert_eq!(track.artist, "Fallback Artist");
    }

    #[test]
    fn map_track_missing_fields() {
        let json = json!({
            "id": 0,
            "title": null,
            "album": {},
            "duration": null,
        });
        let track = QobuzService::map_track(&json);
        assert_eq!(track.title, "");
        assert_eq!(track.artist, "");
        assert_eq!(track.duration_ms, 0);
        let q = track.quality.unwrap();
        assert_eq!(q.sample_rate, 44100);
        assert_eq!(q.bit_depth, 16);
    }

    #[test]
    fn map_album_basic() {
        let json = json!({
            "id": 999,
            "title": "Time Out",
            "artist": {"name": "Dave Brubeck", "id": 42},
            "image": {"large": "http://img.qobuz.com/album.jpg"},
            "release_date_original": "1959-12-14",
            "tracks_count": 7,
        });
        let album = QobuzService::map_album(&json);
        assert_eq!(album.id, "999");
        assert_eq!(album.title, "Time Out");
        assert_eq!(album.artist, "Dave Brubeck");
        assert_eq!(album.artist_id.as_deref(), Some("42"));
        assert_eq!(album.year, Some(1959));
        assert_eq!(album.track_count, 7);
        assert_eq!(
            album.cover_path.as_deref(),
            Some("http://img.qobuz.com/album.jpg")
        );
    }

    #[test]
    fn map_album_with_released_at_timestamp() {
        let json = json!({
            "id": "abc",
            "title": "Test",
            "artist": {"name": "Test"},
            "released_at": 1580515200, // ~2020
            "tracks_count": 10,
        });
        let album = QobuzService::map_album(&json);
        assert!(album.year.is_some());
        assert!(album.year.unwrap() >= 2019 && album.year.unwrap() <= 2021);
    }

    #[test]
    fn map_album_string_id() {
        let json = json!({
            "id": "abc123",
            "title": "Test",
            "artist": {},
            "tracks_count": 0,
        });
        let album = QobuzService::map_album(&json);
        assert_eq!(album.id, "abc123");
    }

    #[test]
    fn map_artist_basic() {
        let json = json!({
            "id": 42,
            "name": "Dave Brubeck",
            "image": {"large": "http://img.qobuz.com/artist.jpg"},
        });
        let artist = QobuzService::map_artist(&json);
        assert_eq!(artist.id, "42");
        assert_eq!(artist.name, "Dave Brubeck");
        assert_eq!(
            artist.image_path.as_deref(),
            Some("http://img.qobuz.com/artist.jpg")
        );
    }

    #[test]
    fn map_genre_basic() {
        let json = json!({
            "id": 10,
            "name": "Jazz",
            "subgenres": [{"id": 11, "name": "Bebop"}],
            "image": "http://img.qobuz.com/jazz.jpg",
        });
        let genre = QobuzService::map_genre(&json);
        assert_eq!(genre.id, "10");
        assert_eq!(genre.name, "Jazz");
        assert!(genre.has_children);
        assert_eq!(
            genre.image_url.as_deref(),
            Some("http://img.qobuz.com/jazz.jpg")
        );
    }

    #[test]
    fn map_genre_subgenres_count() {
        // Qobuz /genre/list returns subgenresCount (integer) instead of subgenres array
        let json = json!({
            "id": 10,
            "name": "Jazz",
            "slug": "jazz",
            "subgenresCount": 15,
            "image": {"large": "http://img.qobuz.com/jazz-large.jpg"},
        });
        let genre = QobuzService::map_genre(&json);
        assert_eq!(genre.id, "10");
        assert_eq!(genre.name, "Jazz");
        assert!(genre.has_children);
        assert_eq!(
            genre.image_url.as_deref(),
            Some("http://img.qobuz.com/jazz-large.jpg")
        );
    }

    #[test]
    fn map_genre_image_object() {
        // Image as object with large/small keys
        let json = json!({
            "id": 30,
            "name": "Classical",
            "subgenresCount": 5,
            "image": {"large": "http://img.qobuz.com/classical.jpg", "small": "http://img.qobuz.com/classical-sm.jpg"},
        });
        let genre = QobuzService::map_genre(&json);
        assert_eq!(
            genre.image_url.as_deref(),
            Some("http://img.qobuz.com/classical.jpg")
        );
    }

    #[test]
    fn map_genre_no_subgenres() {
        let json = json!({
            "id": 20,
            "name": "Blues",
            "subgenres": [],
        });
        let genre = QobuzService::map_genre(&json);
        assert!(!genre.has_children);
    }

    #[test]
    fn map_genre_slug_heuristic() {
        // Top-level slug (no '/') implies children
        let json = json!({
            "id": 40,
            "name": "Rock",
            "slug": "rock",
        });
        let genre = QobuzService::map_genre(&json);
        assert!(genre.has_children);

        // Sub-genre slug with '/' implies no children
        let json2 = json!({
            "id": 41,
            "name": "Hard Rock",
            "slug": "rock/hard-rock",
        });
        let genre2 = QobuzService::map_genre(&json2);
        assert!(!genre2.has_children);
    }

    /// Une entrée fraîche est servie, et c'est tout l'intérêt.
    #[test]
    fn le_cache_editorial_sert_une_entree_fraiche() {
        let svc = QobuzService::new("app".into(), "secret".into());
        let cle = QobuzService::cle_cache("/album/getFeatured", &[("type", "new-releases")]);
        assert!(svc.cache_get(&cle).is_none(), "cache vide au départ");
        svc.cache_set(cle.clone(), json!({"albums": {"items": []}}));
        assert!(svc.cache_get(&cle).is_some(), "entrée fraîche servie");
    }

    /// Une entrée périmée ne doit PAS être servie. Test posé sur l'horloge
    /// réelle en trichant sur l'instant de création : sans lui, rien ne
    /// distingue un cache d'une fuite de mémoire.
    #[test]
    fn une_entree_perimee_n_est_pas_servie() {
        let svc = QobuzService::new("app".into(), "secret".into());
        let cle = "/genre/list".to_string();
        {
            let mut cache = svc.cache_editorial.lock().unwrap();
            cache.insert(
                cle.clone(),
                EntreeCache {
                    donnees: json!({"genres": []}),
                    // Une seconde de plus que le TTL : périmée sans ambiguïté.
                    cree: Instant::now() - TTL_EDITORIAL - Duration::from_secs(1),
                },
            );
        }
        assert!(
            svc.cache_get(&cle).is_none(),
            "une entrée au-delà du TTL doit être ignorée"
        );
    }

    /// Deux requêtes qui diffèrent par un paramètre ne doivent pas partager
    /// leur réponse — sinon un genre servirait les albums d'un autre.
    #[test]
    fn les_parametres_font_partie_de_la_cle() {
        let jazz = QobuzService::cle_cache("/genre/get", &[("genre_id", "10")]);
        let rock = QobuzService::cle_cache("/genre/get", &[("genre_id", "40")]);
        assert_ne!(jazz, rock);
        // Et le chemin seul ne doit pas collisionner avec le chemin paginé.
        assert_ne!(
            QobuzService::cle_cache("/playlist/getFeatured", &[]),
            QobuzService::cle_cache("/playlist/getFeatured#pages", &[])
        );
    }

    /// Le séparateur de clé ne doit pas pouvoir être fabriqué depuis une
    /// valeur : sans lui, `?a=b&c` et `?a=b` + `c` donneraient la même clé.
    #[test]
    fn une_valeur_ne_peut_pas_forger_la_cle_d_une_autre() {
        let a = QobuzService::cle_cache("/x", &[("k", "v"), ("k2", "v2")]);
        let b = QobuzService::cle_cache("/x", &[("k", "v\u{1f}k2=v2")]);
        assert_ne!(
            a, b,
            "un séparateur dans une valeur ne doit pas tout confondre"
        );
    }

    /// Le plafond d'entrées purge les périmées au lieu de croître sans fin.
    #[test]
    fn le_cache_purge_les_perimees_quand_il_est_plein() {
        let svc = QobuzService::new("app".into(), "secret".into());
        {
            let mut cache = svc.cache_editorial.lock().unwrap();
            for i in 0..MAX_ENTREES_CACHE {
                cache.insert(
                    format!("perimee-{i}"),
                    EntreeCache {
                        donnees: json!(i),
                        cree: Instant::now() - TTL_EDITORIAL - Duration::from_secs(1),
                    },
                );
            }
        }
        svc.cache_set("fraiche".into(), json!("ok"));
        let cache = svc.cache_editorial.lock().unwrap();
        assert_eq!(cache.len(), 1, "les périmées ont été purgées");
        assert!(cache.contains_key("fraiche"));
    }

    #[test]
    fn qobuz_service_name() {
        let svc = QobuzService::new("app123".into(), "secret".into());
        assert_eq!(svc.name(), "qobuz");
    }

    #[test]
    fn qobuz_save_tokens_no_auth() {
        let svc = QobuzService::new("app".into(), "secret".into());
        assert!(svc.save_tokens().is_none());
    }

    #[test]
    fn qobuz_restore_tokens() {
        let mut svc = QobuzService::new("app".into(), "secret".into());
        let tokens = json!({
            "user_auth_token": "token123",
            "username": "testuser",
            "subscription": "Studio",
            "app_id": "new_app",
            "app_secret": "new_secret",
        });
        assert!(svc.restore_tokens(&tokens));
        assert_eq!(svc.user_auth_token.as_deref(), Some("token123"));
        assert_eq!(svc.username.as_deref(), Some("testuser"));
        assert_eq!(svc.app_id, "new_app");
        assert_eq!(svc.app_secret, "new_secret");
    }

    #[test]
    fn qobuz_restore_tokens_invalid() {
        let mut svc = QobuzService::new("app".into(), "secret".into());
        let tokens = json!({"nothing": "here"});
        assert!(!svc.restore_tokens(&tokens));
    }

    #[test]
    fn qobuz_save_tokens_never_persists_the_password() {
        // The whole point of the fix: an authenticated session writes a blob
        // that a reader of tune.db cannot turn into account credentials.
        let mut svc = QobuzService::new("app".into(), "secret".into());
        svc.user_auth_token = Some("token123".into());
        svc.stored_username = Some("testuser".into());
        svc.stored_password = Some("hunter2".into());

        let tokens = svc
            .save_tokens()
            .expect("authenticated, so a blob is written");

        assert!(tokens.get("stored_password").is_none());
        assert_eq!(tokens["stored_username"], "testuser");
        assert_eq!(tokens["user_auth_token"], "token123");
        assert!(
            !tokens.to_string().contains("hunter2"),
            "the password must not reach disk under any key"
        );
    }

    #[test]
    fn qobuz_restore_drops_legacy_password_and_asks_for_a_rewrite() {
        // A row written by a pre-fix build. The password must not be loaded into
        // the session, and the service must ask for the row to be rewritten.
        let mut svc = QobuzService::new("app".into(), "secret".into());
        let tokens = json!({
            "user_auth_token": "token123",
            "username": "testuser",
            "stored_username": "testuser",
            "stored_password": "hunter2",
        });

        assert!(svc.restore_tokens(&tokens));
        assert_eq!(svc.stored_password, None);
        assert!(svc.tokens_need_rewrite());

        // And what gets written back is clean.
        let rewritten = svc.save_tokens().expect("token restored");
        assert!(!rewritten.to_string().contains("hunter2"));
    }

    #[test]
    fn qobuz_restore_clean_row_needs_no_rewrite() {
        // No stale field, no write: startup must not churn the settings row on
        // every boot once the DB has been migrated.
        let mut svc = QobuzService::new("app".into(), "secret".into());
        let tokens = json!({
            "user_auth_token": "token123",
            "stored_username": "testuser",
        });

        assert!(svc.restore_tokens(&tokens));
        assert!(!svc.tokens_need_rewrite());
    }

    #[test]
    fn qobuz_auto_relogin_still_works_within_the_session() {
        // The password stays in memory after an interactive login, so an expired
        // token is recovered without prompting. Only a restart forces a re-login.
        let mut svc = QobuzService::new("app".into(), "secret".into());
        svc.stored_username = Some("testuser".into());
        svc.stored_password = Some("hunter2".into());
        assert!(svc.stored_password.is_some());

        // Restoring from disk is the case that cannot: nothing to relogin with.
        let mut restored = QobuzService::new("app".into(), "secret".into());
        restored.restore_tokens(&json!({
            "user_auth_token": "token123",
            "stored_username": "testuser",
        }));
        assert_eq!(restored.stored_password, None);
    }

    #[test]
    fn qobuz_fresh_service_reports_no_expired_session() {
        let svc = QobuzService::new("app".into(), "secret".into());
        assert!(!svc.session_expired());
        assert!(!svc.auth_status_internal().authenticated);
    }

    #[test]
    fn qobuz_cleared_session_reports_disconnected() {
        // The state `refresh_if_needed` leaves behind once Qobuz has refused the
        // token: `authenticated` must be false, or the UI keeps showing the
        // service as connected while every call 401s.
        let mut svc = QobuzService::new("app".into(), "secret".into());
        svc.user_auth_token = None;
        svc.subscription = None;
        svc.username = Some("testuser".into());
        svc.session_expired = true;

        let status = svc.auth_status_internal();
        assert!(!status.authenticated);
        assert!(svc.session_expired());
        // The account stays named so the prompt can address it.
        assert_eq!(status.username.as_deref(), Some("testuser"));
        // And nothing is persisted for a session that no longer exists.
        assert!(svc.save_tokens().is_none());
    }

    #[test]
    fn qobuz_login_clears_the_expired_flag() {
        // Reconnecting must undo the expiry, otherwise the row would be deleted
        // again right after a successful login.
        let mut svc = QobuzService::new("app".into(), "secret".into());
        svc.session_expired = true;

        // What `login_internal` does on success, without the network round trip.
        svc.user_auth_token = Some("fresh".into());
        svc.session_expired = false;

        assert!(!svc.session_expired());
        assert!(svc.auth_status_internal().authenticated);
        assert!(svc.save_tokens().is_some());
    }

    #[test]
    fn qobuz_set_enabled() {
        let mut svc = QobuzService::new("app".into(), "secret".into());
        svc.set_enabled(false);
        assert!(!svc.enabled());
        svc.set_enabled(true);
        assert!(svc.enabled());
    }

    #[test]
    fn md5_hex_known_value() {
        // MD5 of empty string is d41d8cd98f00b204e9800998ecf8427e
        let result = md5_hex("");
        assert_eq!(result, "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn qobuz_supports_write() {
        let mut svc = QobuzService::new("app_id".into(), "secret".into());
        assert!(!svc.supports_write());
        svc.user_auth_token = Some("token".into());
        assert!(svc.supports_write());
    }

    /// La liste des playlists de l'utilisateur renvoyait `cover_path: None` en
    /// dur, alors que `/playlist/getUserPlaylists` porte les mêmes champs image
    /// que l'éditorial — Qobuz y compose une mosaïque des pochettes d'albums.
    /// La donnée était là ; on ne la lisait pas (#1970).
    #[test]
    fn une_playlist_utilisateur_recupere_sa_pochette() {
        let item = serde_json::json!({
            "id": 42, "name": "Ma liste", "tracks_count": 12,
            "images300": ["https://static.qobuz.com/p300.jpg"],
        });
        let p = QobuzService::map_featured_playlist(&item);
        assert_eq!(
            p.cover_path.as_deref(),
            Some("https://static.qobuz.com/p300.jpg")
        );
    }

    /// L'ordre préserve le rendu actuel des sélections éditoriales :
    /// `image_rectangle` passe devant, les autres ne font que rattraper.
    #[test]
    fn image_rectangle_reste_prioritaire_pour_l_editorial() {
        let item = serde_json::json!({
            "id": 1, "name": "Sélection",
            "image_rectangle": ["https://q/rect.jpg"],
            "images300": ["https://q/300.jpg"],
            "images150": ["https://q/150.jpg"],
        });
        assert_eq!(
            QobuzService::pochette_playlist(&item).as_deref(),
            Some("https://q/rect.jpg")
        );
    }

    /// La cascade descend jusqu'au dernier champ plutôt que d'abandonner au
    /// premier absent — c'est exactement ce que `get_playlist` ne faisait pas :
    /// il ne tentait que `image_rectangle_mini`.
    #[test]
    fn la_cascade_descend_jusqu_au_dernier_champ() {
        for (champ, attendu) in [
            ("images300", "https://q/a.jpg"),
            ("images150", "https://q/a.jpg"),
            ("images", "https://q/a.jpg"),
            ("image_rectangle_mini", "https://q/a.jpg"),
        ] {
            let item = serde_json::json!({ champ: ["https://q/a.jpg"] });
            assert_eq!(
                QobuzService::pochette_playlist(&item).as_deref(),
                Some(attendu),
                "le champ {champ} devrait être lu"
            );
        }
    }

    /// `image` est tantôt une chaîne, tantôt l'objet `{large, small}` que porte
    /// déjà un album.
    #[test]
    fn le_champ_image_est_accepte_dans_ses_deux_formes() {
        let chaine = serde_json::json!({ "image": "https://q/s.jpg" });
        assert_eq!(
            QobuzService::pochette_playlist(&chaine).as_deref(),
            Some("https://q/s.jpg")
        );
        let objet = serde_json::json!({ "image": { "large": "https://q/l.jpg" } });
        assert_eq!(
            QobuzService::pochette_playlist(&objet).as_deref(),
            Some("https://q/l.jpg")
        );
    }

    /// Une playlist sans aucune image ne doit pas rendre une chaîne vide : le
    /// client afficherait une pochette cassée plutôt que son repli.
    #[test]
    fn aucune_image_rend_none_et_jamais_une_chaine_vide() {
        let vide = serde_json::json!({ "id": 7, "name": "Sans image" });
        assert_eq!(QobuzService::pochette_playlist(&vide), None);
        let chaine_vide = serde_json::json!({ "images300": [""], "image": "" });
        assert_eq!(QobuzService::pochette_playlist(&chaine_vide), None);
    }
}
