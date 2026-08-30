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
    /// Réponses de `/album/get`, par identifiant d'album.
    ///
    /// Cache SÉPARÉ de l'éditorial, et non une clé de plus dedans, pour deux
    /// raisons qui tiennent toutes les deux au volume : le détail d'un album
    /// porte sa liste de pistes complète — un ordre de grandeur au-dessus
    /// d'une liste de genres — et `retirer_les_chevrons_dementis` relit le
    /// cache éditorial par la clé. Y verser les albums d'une navigation en
    /// chasserait les démentis de genre qu'il est le seul à porter.
    cache_album: Mutex<HashMap<String, EntreeCache>>,
    /// Base d'API forcée, pour brancher les LECTURES sur un serveur simulé.
    ///
    /// `None` en production : l'ordre direct/proxy habituel s'applique. Seul
    /// `api_get` la consulte — le login et les écritures gardent l'ordre de
    /// production, de sorte qu'une base d'essai ne puisse jamais recevoir
    /// d'identifiants.
    base_forcee: Option<String>,
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

/// Durée de vie du détail d'album en cache.
///
/// Cinq minutes, et non les trente de l'éditorial : ce cache n'existe pas pour
/// épargner du réseau à la journée, mais pour qu'UNE ouverture d'album ne
/// paie qu'UN aller-retour au lieu de quatre (#2190). Cinq minutes couvrent
/// largement l'ouverture, la lecture qui suit, et un aller-retour dans la
/// vue ; au-delà, rien ne justifie de garder une liste de pistes en mémoire.
const TTL_ALBUM: Duration = Duration::from_secs(300);

/// Nombre d'albums gardés en mémoire.
///
/// Ce plafond est TENU, contrairement à [`MAX_ENTREES_CACHE`] qui ne purge que
/// les entrées expirées et peut donc croître si elles sont toutes fraîches.
/// Une liste de pistes pèse, et une navigation d'une heure traverse facilement
/// des centaines d'albums : ici on évince la plus ancienne.
const MAX_ALBUMS_CACHE: usize = 32;

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

/// Les quatre catégories que `/catalog/search` rend dans une même réponse.
const CATEGORIES_RECHERCHE: [&str; 4] = ["tracks", "albums", "artists", "playlists"];

/// Taille de page de `/catalog/search`.
///
/// Qobuz ne rend jamais plus de 50 éléments par catégorie et par requête, quel
/// que soit le `limit` demandé : au-delà, il faut un `offset`, pas un plus
/// grand `limit`.
const TAILLE_PAGE_RECHERCHE: usize = 50;

/// Plafond d'une recherche « Tous » (#2160).
///
/// « Tous » ne peut pas vouloir dire « tout le catalogue » : une requête
/// courante annonce des dizaines de milliers de titres, soit des centaines
/// d'allers-retours et une limitation Akamai assurée. 500 par catégorie — dix
/// pages — reprend le plafond déjà retenu pour les sélections éditoriales
/// (#1969).
const PLAFOND_RECHERCHE: usize = 500;

/// Combien d'éléments par catégorie une recherche doit réellement ramener.
///
/// `0` est le « Tous » du client, même convention que les facettes Oxygen. Et
/// toute valeur est bornée : un client qui demanderait 100 000 ne doit pas se
/// traduire en deux mille requêtes chez Qobuz.
fn plafond_recherche(limite: usize) -> usize {
    if limite == 0 {
        PLAFOND_RECHERCHE
    } else {
        limite.min(PLAFOND_RECHERCHE)
    }
}

/// Un volume DÉJÀ borné par `plafond_recherche` demande-t-il plusieurs pages ?
///
/// Prend le plafond et non la limite brute : le seul appelant en production
/// tient déjà le plafond, et lui faire repasser par `plafond_recherche`
/// laisserait deux traductions « Tous » → 500 vivre côte à côte.
fn recherche_paginee(plafond: usize) -> bool {
    plafond > TAILLE_PAGE_RECHERCHE
}

/// Décalages des pages restantes d'une recherche, après la première.
///
/// Une seule requête `/catalog/search` rend les quatre catégories : on ne
/// pagine donc pas quatre fois, on pagine une fois jusqu'à ce que la catégorie
/// la PLUS fournie soit épuisée ou que le plafond soit atteint. Les catégories
/// déjà épuisées rendront simplement des pages vides, que la fusion ignore.
///
/// `depart` est le curseur du client (#2160) : le plafond se compte À PARTIR
/// de lui, pas depuis le début du catalogue. Un « Charger plus » à `depart =
/// 500` demandant 200 doit rendre les 200 suivants, pas zéro.
fn offsets_recherche(
    comptes: &[usize],
    totaux: &[usize],
    plafond: usize,
    depart: usize,
) -> Vec<usize> {
    let compte_max = comptes.iter().copied().max().unwrap_or(0);
    let total_max = totaux.iter().copied().max().unwrap_or(0);
    let restant = total_max.saturating_sub(depart);
    remaining_page_offsets_bornees(compte_max, restant, TAILLE_PAGE_RECHERCHE, plafond)
        .into_iter()
        .map(|relatif| relatif + depart)
        .collect()
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
            cache_album: Mutex::new(HashMap::new()),
            base_forcee: None,
        }
    }

    /// Même service, mais dont les LECTURES tapent sur `base` au lieu de
    /// l'API Qobuz. Réservé aux essais : il n'existe aucun chemin de
    /// production qui construise un service ainsi.
    #[cfg(test)]
    fn avec_base_forcee(base: impl Into<String>) -> Self {
        let mut svc = Self::new(String::from("app-id-essai"), String::from("secret-essai"));
        svc.base_forcee = Some(base.into());
        svc
    }

    /// (primaire, secours) pour une LECTURE.
    fn bases_de_lecture(&self) -> (&str, &str) {
        match self.base_forcee {
            Some(ref base) => (base.as_str(), base.as_str()),
            None => endpoint_order(self.proxy_first),
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
        let (primary, fallback) = self.bases_de_lecture();
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

    /// La réponse de `/album/get` pour cet album — une seule fois par album.
    ///
    /// **Le défaut que ceci corrige (#2190).** Ouvrir un album déclenche
    /// jusqu'à QUATRE requêtes HTTP côté client — `/albums/{id}`,
    /// `/albums/{id}/tracks`, `/albums/{id}/context`, `/albums/{id}/label` —
    /// et chacune repartait chercher auprès de Qobuz **exactement la même**
    /// réponse `/album/get?album_id={id}`. La lecture qui suit en ajoutait une
    /// cinquième (`playback.rs` refait `get_album_tracks`). Cinq allers-retours
    /// pour une donnée, sur un service dont un aller-retour coûte des
    /// centaines de millisecondes : c'est le « slow to load » d'Alex Campbell.
    ///
    /// **Pourquoi c'est cachable.** Tout ce que nous lisons de cette réponse
    /// est du CATALOGUE : `map_album` (titre, artiste, pochette, année,
    /// qualité), `map_track` (titre, interprète, ISRC, numéro de piste),
    /// `get_album_context` (genre, label), `get_album_label` (label). **Aucun
    /// champ propre au compte n'est consulté** — ni `favorited_at`, ni
    /// `purchasable`, ni `streamable`. Contrairement aux favoris ou aux
    /// playlists de l'utilisateur, resservir cette réponse ne peut donc pas
    /// faire réapparaître ce qu'il vient de retirer. Si un jour un mappeur se
    /// met à lire un champ dépendant du compte, ce cache devra sauter — c'est
    /// la seule condition, et elle est ici pour être relue.
    async fn detail_album(&self, album_id: &str) -> Result<serde_json::Value, String> {
        if let Some(donnees) = self.album_en_cache(album_id) {
            debug!(album_id, "qobuz_album_cache_hit");
            return Ok(donnees);
        }
        let donnees = self
            .api_get("/album/get", &[("album_id", album_id)])
            .await?;
        self.memoriser_album(album_id, donnees.clone());
        Ok(donnees)
    }

    /// Le détail d'album mémorisé s'il est encore frais, sinon rien.
    fn album_en_cache(&self, album_id: &str) -> Option<serde_json::Value> {
        let cache = self.cache_album.lock().ok()?;
        cache.get(album_id).and_then(|e| {
            if e.cree.elapsed() < TTL_ALBUM {
                Some(e.donnees.clone())
            } else {
                None
            }
        })
    }

    /// Mémorise un détail d'album, en tenant le plafond.
    ///
    /// On retire d'abord les périmés ; s'il n'en restait aucun, on évince la
    /// plus ANCIENNE entrée. Se contenter de la purge des périmés — ce que
    /// fait `cache_set` — laisserait la table croître sans borne pendant une
    /// navigation soutenue, chaque entrée portant une liste de pistes.
    fn memoriser_album(&self, album_id: &str, donnees: serde_json::Value) {
        let Ok(mut cache) = self.cache_album.lock() else {
            return;
        };
        if cache.len() >= MAX_ALBUMS_CACHE {
            cache.retain(|_, e| e.cree.elapsed() < TTL_ALBUM);
        }
        if cache.len() >= MAX_ALBUMS_CACHE {
            if let Some(plus_ancienne) = cache
                .iter()
                .min_by_key(|(_, e)| e.cree)
                .map(|(cle, _)| cle.clone())
            {
                cache.remove(&plus_ancienne);
            }
        }
        cache.insert(
            album_id.to_string(),
            EntreeCache {
                donnees,
                cree: Instant::now(),
            },
        );
    }

    /// Une collection de titres rattachée à un artiste, lue sous la clé qui
    /// porte le nom de l'`extra` demandé (#2568).
    ///
    /// `/artist/get` range chaque extra sous son propre nom : demander
    /// `tracks_appears_on` et relire `tracks` ne peut rien rendre. Passer
    /// l'`extra` en paramètre lie les deux, et le lien ne peut plus se défaire.
    ///
    /// `api_get_editorial` et non `api_get` : le catalogue d'un artiste ne
    /// dépend pas de ce que l'utilisateur vient de cliquer, contrairement à ses
    /// favoris. La radio d'autoplay redemande les mêmes artistes à chaque fin
    /// de piste — le cache lui évite un aller-retour par titre.
    ///
    /// **La liste de paramètres ci-dessous est la clé de cache** : la modifier
    /// change la clé. Les tests la reconstruisent à l'identique via
    /// `cle_cache`, et divergeraient en silence — ils appelleraient le réseau.
    async fn pistes_extra_artiste(
        &self,
        artist_id: &str,
        extra: &str,
    ) -> Result<Vec<StreamTrack>, TuneError> {
        let data = self
            .api_get_editorial(
                "/artist/get",
                &[("artist_id", artist_id), ("extra", extra), ("limit", "20")],
            )
            .await?;
        Ok(data[extra]["items"]
            .as_array()
            .map(|items| items.iter().map(Self::map_track).collect())
            .unwrap_or_default())
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

    /// Rôles Qobuz d'écriture pure (comparés sans espaces) : un intervenant qui
    /// n'a que ceux-là est l'auteur de l'œuvre, pas son interprète.
    fn role_d_ecriture(role: &str) -> bool {
        matches!(
            role.replace(' ', "").as_str(),
            "Composer" | "ComposerLyricist" | "Lyricist" | "Author" | "Writer" | "MusicPublisher"
        )
    }

    /// Les rôles crédités à `nom` dans la chaîne Qobuz `performers`
    /// (« Nom, Rôle1, Rôle2 - Nom2, Rôle3 »). `None` si le nom n'y figure pas.
    /// Le préfixe est comparé tel quel, ce qui tolère les noms contenant une
    /// virgule (« Blood, Sweat & Tears, MainArtist »).
    fn roles_credites<'a>(performers: &'a str, nom: &str) -> Option<Vec<&'a str>> {
        performers.split(" - ").find_map(|entree| {
            let reste = entree.trim().strip_prefix(nom)?;
            if reste.is_empty() {
                return Some(Vec::new());
            }
            let roles = reste.strip_prefix(", ")?;
            Some(roles.split(", ").map(str::trim).collect())
        })
    }

    /// Le premier intervenant crédité `MainArtist` dans la chaîne `performers`.
    fn main_artist_des_roles(performers: &str) -> Option<String> {
        performers.split(" - ").find_map(|entree| {
            let mut segments = entree.trim().split(", ");
            let nom = segments.next()?.trim();
            (!nom.is_empty() && segments.any(|r| r.trim().replace(' ', "") == "MainArtist"))
                .then(|| nom.to_string())
        })
    }

    /// L'interprète à afficher comme « artiste » d'une piste Qobuz (#1407).
    ///
    /// En classique, Qobuz place souvent le compositeur dans `performer` (voire
    /// dans l'artiste d'album), et « Lecture en cours » affichait Chopin au lieu
    /// de la pianiste. Priorité : interprète d'abord — le compositeur ne sert
    /// JAMAIS de valeur d'« artiste » quand un interprète est identifiable.
    ///
    /// 1. `performer.name`, sauf si la chaîne de rôles `performers` (ou, à
    ///    défaut, l'égalité avec `composer.name`) le désigne comme auteur pur ;
    /// 2. le premier `MainArtist` de la chaîne de rôles (rôle d'interprète
    ///    prouvé, même s'il est aussi le compositeur — cas de l'auteur qui joue
    ///    ses propres œuvres) ;
    /// 3. `artist.name` de la piste puis `album.artist.name`, hors compositeur
    ///    et hors « Various Artists » (qui n'apprend rien à l'auditeur) ;
    /// 4. repli historique inchangé : `performer.name` puis `artist.name`.
    fn artiste_interprete(item: &serde_json::Value) -> String {
        let vide = |s: &&str| !s.trim().is_empty();
        let compositeur = item["composer"]["name"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let performer = item["performer"]["name"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let roles = item["performers"].as_str().unwrap_or("");

        if let Some(p) = performer {
            let auteur_pur = match Self::roles_credites(roles, p) {
                Some(credits) if !credits.is_empty() => {
                    credits.iter().all(|r| Self::role_d_ecriture(r))
                }
                // Pas d'info de rôle exploitable : on ne l'écarte que si son nom
                // est exactement celui du compositeur.
                _ => compositeur == Some(p),
            };
            if !auteur_pur {
                return p.into();
            }
        }
        if let Some(main) = Self::main_artist_des_roles(roles) {
            return main;
        }
        for candidat in [
            item["artist"]["name"].as_str(),
            item["album"]["artist"]["name"].as_str(),
        ]
        .into_iter()
        .flatten()
        .map(str::trim)
        {
            if !candidat.is_empty()
                && compositeur != Some(candidat)
                && !candidat.eq_ignore_ascii_case("various artists")
            {
                return candidat.into();
            }
        }
        performer
            .or_else(|| item["artist"]["name"].as_str().filter(vide))
            .unwrap_or("")
            .into()
    }

    fn map_track(item: &serde_json::Value) -> StreamTrack {
        let album = &item["album"];
        StreamTrack {
            id: item["id"].as_u64().unwrap_or(0).to_string(),
            title: item["title"].as_str().unwrap_or("").into(),
            artist: Self::artiste_interprete(item),
            composer: item["composer"]["name"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(Into::into),
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
        Self::elements(data, "playlists")
            .iter()
            .map(Self::map_featured_playlist)
            .collect()
    }

    /// Le tableau `items` d'une catégorie, vide si la catégorie est absente.
    fn elements<'a>(data: &'a serde_json::Value, categorie: &str) -> &'a [serde_json::Value] {
        data[categorie]["items"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Ce qu'une page rend pour une catégorie, et ce que Qobuz annonce en tout.
    fn compte_et_total(data: &serde_json::Value, categorie: &str) -> (usize, usize) {
        (
            Self::elements(data, categorie).len(),
            data[categorie]["total"].as_u64().unwrap_or(0) as usize,
        )
    }

    /// Concatène les pages d'une recherche, dans l'ordre des décalages, et
    /// borne CHAQUE catégorie au plafond demandé.
    ///
    /// Le plafond s'applique catégorie par catégorie, comme le demande #2160 :
    /// « 200 » veut dire deux cents albums ET deux cents artistes ET deux
    /// cents titres, pas deux cents éléments toutes catégories confondues.
    /// Une catégorie épuisée avant les autres reste plus courte — on ne
    /// complète pas, on ne tronque pas les autres pour autant.
    fn fusionner_recherche(pages: &[serde_json::Value], plafond: usize) -> SearchResults {
        let mut resultats = SearchResults {
            tracks: Vec::new(),
            albums: Vec::new(),
            artists: Vec::new(),
            playlists: Vec::new(),
        };
        for page in pages {
            resultats
                .tracks
                .extend(Self::elements(page, "tracks").iter().map(Self::map_track));
            resultats
                .albums
                .extend(Self::elements(page, "albums").iter().map(Self::map_album));
            resultats
                .artists
                .extend(Self::elements(page, "artists").iter().map(Self::map_artist));
            resultats.playlists.extend(Self::search_playlists(page));
        }
        resultats.tracks.truncate(plafond);
        resultats.albums.truncate(plafond);
        resultats.artists.truncate(plafond);
        resultats.playlists.truncate(plafond);
        resultats
    }

    /// Les pages brutes d'une recherche, à partir de `depart`.
    ///
    /// La première apprend les `total`, les suivantes sont demandées par
    /// `offset` de 50 en 50. Avant #2160, `search` n'émettait qu'un seul
    /// `/catalog/search` avec le `limit` demandé. Or Qobuz plafonne une page à
    /// 50 : demander 200 rendait 50, et les 150 autres n'étaient jamais
    /// récupérés.
    ///
    /// À `depart = 0` et pour une page ou moins, la requête émise est
    /// exactement celle d'avant #2160 — sans paramètre `offset`. Ce n'est pas
    /// de la coquetterie : c'est le chemin que prennent la transfert de
    /// playlist (10), les sondes (5) et le défaut de route (20).
    async fn pages_de_recherche(
        &self,
        query: &str,
        plafond: usize,
        depart: usize,
    ) -> Result<Vec<serde_json::Value>, TuneError> {
        use futures_util::StreamExt;
        const MAX_CONCURRENT_PAGES: usize = 4;

        let base_params: [(&str, &str); 1] = [("query", query)];
        let taille_premiere = plafond.min(TAILLE_PAGE_RECHERCHE);
        let premiere = if depart == 0 {
            self.api_get(
                "/catalog/search",
                &[("query", query), ("limit", &taille_premiere.to_string())],
            )
            .await?
        } else {
            self.api_get_page("/catalog/search", &base_params, depart, taille_premiere)
                .await?
        };

        let mut pages = vec![premiere];
        if !recherche_paginee(plafond) {
            return Ok(pages);
        }

        let (comptes, totaux): (Vec<usize>, Vec<usize>) = CATEGORIES_RECHERCHE
            .iter()
            .map(|categorie| Self::compte_et_total(&pages[0], categorie))
            .unzip();
        let offsets = offsets_recherche(&comptes, &totaux, plafond, depart);
        info!(
            plafond,
            depart,
            pages = offsets.len() + 1,
            totaux = ?totaux,
            "qobuz_search_paginate"
        );

        if !offsets.is_empty() {
            // `buffered` conserve l'ordre des pages : la liste fusionnée est
            // celle qu'une boucle séquentielle aurait produite.
            let suivantes: Vec<Result<serde_json::Value, String>> =
                futures_util::stream::iter(offsets.into_iter().map(|offset| {
                    let base_params = &base_params;
                    async move {
                        self.api_get_page(
                            "/catalog/search",
                            base_params,
                            offset,
                            TAILLE_PAGE_RECHERCHE,
                        )
                        .await
                    }
                }))
                .buffered(MAX_CONCURRENT_PAGES)
                .collect()
                .await;
            for page in suivantes {
                pages.push(page?);
            }
        }

        Ok(pages)
    }

    /// Les totaux annoncés par Qobuz, lus sur la première page.
    fn totaux_recherche(page: &serde_json::Value) -> SearchTotals {
        SearchTotals {
            tracks: Self::compte_et_total(page, "tracks").1,
            albums: Self::compte_et_total(page, "albums").1,
            artists: Self::compte_et_total(page, "artists").1,
            playlists: Self::compte_et_total(page, "playlists").1,
        }
    }

    /// Fusionne les pages ET dit ce qu'il reste derrière (#2160).
    ///
    /// `has_more` vaut pour AU MOINS une catégorie : un écran en onglets
    /// affiche « Charger plus » dès qu'un onglet a une suite, pas seulement
    /// quand les quatre en ont une.
    ///
    /// `truncated` sépare deux fins de liste que le client ne peut pas
    /// distinguer autrement : « Qobuz n'a plus rien » et « NOTRE plafond a
    /// coupé ». Sans lui, un « Tous » rendant 500 sur 5 000 passerait pour
    /// exhaustif.
    fn page_de_recherche(pages: &[serde_json::Value], plafond: usize, depart: usize) -> SearchPage {
        let results = Self::fusionner_recherche(pages, plafond);
        let totals = pages
            .first()
            .map(Self::totaux_recherche)
            .unwrap_or_default();
        let par_categorie = [
            (results.tracks.len(), totals.tracks),
            (results.albums.len(), totals.albums),
            (results.artists.len(), totals.artists),
            (results.playlists.len(), totals.playlists),
        ];
        let has_more = par_categorie
            .iter()
            .any(|(rendu, total)| depart + rendu < *total);
        let truncated = par_categorie
            .iter()
            .any(|(rendu, total)| *rendu == plafond && depart + rendu < *total);
        SearchPage {
            results,
            offset: depart,
            totals,
            has_more,
            truncated,
        }
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
        // `has_children` n'existe PAS chez Qobuz : c'est NOUS qui le
        // fabriquons. Deux champs de la réponse peuvent l'ÉNONCER —
        // `subgenresCount` (rendu par `/genre/list`) et `subgenres` (rendu par
        // `/genre/get`). En l'absence des deux, Qobuz ne dit rien, et la seule
        // réponse honnête est « non ».
        //
        // Il y avait ici une troisième source : la forme du `slug`. Un slug
        // sans « / » était réputé désigner un genre racine, donc pourvu
        // d'enfants — le commentaire d'origine disait lui-même « typically ».
        // C'est une invention : aucun champ de la réponse ne l'affirme. Elle
        // produisait le défaut #2115 — « Be Bop » annonçant un sous-menu qui
        // rend `[]`, et l'utilisateur devant un écran vide. Un chevron absent
        // se rattrape d'un clic ; un chevron menteur fait douter de tout
        // l'écran. On n'annonce plus que ce qui est ÉNONCÉ.
        let has_children = item["subgenresCount"]
            .as_u64()
            .map(|n| n > 0)
            .or_else(|| item["subgenres"].as_array().map(|a| !a.is_empty()))
            .unwrap_or(false);

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

    /// Les paramètres de `/genre/list`, construits en UN SEUL endroit.
    ///
    /// La clé du cache éditorial est calculée sur ces paramètres exacts. Si la
    /// liste divergeait entre l'appel qui écrit le cache et la relecture faite
    /// par `retirer_les_chevrons_dementis`, cette dernière chercherait une clé
    /// qui n'existe pas : le garde-fou ne se déclencherait JAMAIS, et en
    /// silence. Une seule source pour les deux.
    fn params_genres(parent_id: Option<&str>) -> Vec<(&str, &str)> {
        let mut params: Vec<(&str, &str)> = vec![("offset", "0"), ("limit", "500")];
        if let Some(pid) = parent_id {
            params.push(("parent_id", pid));
        }
        params
    }

    /// Les genres d'une réponse `/genre/list`, quelle que soit sa forme.
    fn extraire_genres(data: &serde_json::Value) -> Vec<StreamGenre> {
        data["genres"]["items"]
            .as_array()
            .or_else(|| data["genres"].as_array())
            .or_else(|| data.as_array())
            .map(|items| items.iter().map(Self::map_genre).collect())
            .unwrap_or_default()
    }

    /// Retire le chevron des nœuds dont on a DÉJÀ constaté qu'ils ne rendent
    /// rien.
    ///
    /// Qobuz peut annoncer `subgenresCount > 0` sur un sous-genre dont
    /// `/genre/list?parent_id=<lui>` rend `[]`. Ce mensonge-là, nous ne pouvons
    /// pas le corriger à la source. Mais dès qu'il a été constaté une fois, la
    /// réponse vide est dans le cache éditorial : on la relit et on retire
    /// l'affordance, au lieu de la laisser mentir une seconde fois.
    ///
    /// **Coût réseau : zéro.** Aucune requête n'est déclenchée — on ne relit
    /// que ce que la navigation de l'utilisateur a déjà rapporté. Le verrou est
    /// pris une seule fois pour toute la liste, et la donnée n'est pas clonée :
    /// une liste de genres peut compter jusqu'à 500 entrées (`limit`).
    ///
    /// **Durée de vie du démenti : celle du cache éditorial**, `TTL_EDITORIAL`
    /// (30 min). Pas de nouvel état, pas de nouvelle purge à écrire — et si
    /// Qobuz finit par peupler ce sous-genre, le chevron revient au plus tard
    /// une demi-heure plus tard.
    fn retirer_les_chevrons_dementis(&self, genres: &mut [StreamGenre]) {
        if !genres.iter().any(|g| g.has_children) {
            return;
        }
        let Ok(cache) = self.cache_editorial.lock() else {
            return;
        };
        for genre in genres.iter_mut().filter(|g| g.has_children) {
            let cle = Self::cle_cache("/genre/list", &Self::params_genres(Some(&genre.id)));
            let Some(entree) = cache.get(&cle) else {
                continue;
            };
            if entree.cree.elapsed() >= TTL_EDITORIAL {
                continue;
            }
            if Self::extraire_genres(&entree.donnees).is_empty() {
                info!(genre_id = %genre.id, genre_name = %genre.name, "qobuz_chevron_dementi");
                genre.has_children = false;
            }
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

    /// `limit` est un nombre d'éléments PAR CATÉGORIE. `0` vaut « Tous »,
    /// borné par `PLAFOND_RECHERCHE` (#2160). Jusqu'à 50, une seule requête —
    /// exactement ce que faisait la recherche avant ; au-delà, on pagine.
    async fn search(&self, query: &str, limit: usize) -> Result<SearchResults, TuneError> {
        let plafond = plafond_recherche(limit);
        let pages = self.pages_de_recherche(query, plafond, 0).await?;
        Ok(Self::fusionner_recherche(&pages, plafond))
    }

    /// `offset` est un curseur PAR CATÉGORIE, en éléments (#2160) : c'est ce
    /// qu'un « Charger plus » renvoie après avoir affiché `offset` lignes.
    ///
    /// Le plafond se compte à partir du curseur — `offset=500&limit=200` rend
    /// les 200 suivants, il ne rend pas zéro sous prétexte que 500 est déjà le
    /// plafond d'une requête.
    async fn search_page(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<SearchPage, TuneError> {
        let plafond = plafond_recherche(limit);
        let pages = self.pages_de_recherche(query, plafond, offset).await?;
        Ok(Self::page_de_recherche(&pages, plafond, offset))
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
        let data = self.detail_album(album_id).await?;
        Ok(Self::map_album(&data))
    }

    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        let data = self.detail_album(album_id).await?;
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
        let album_artist = data["artist"]["name"].as_str().map(String::from);

        let tracks = data["tracks"]["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        // Les items d'album/get n'ont pas de sous-objet "album" :
                        // sans injection, artiste_interprete (#1407) ne voit pas
                        // l'artiste d'album comme repli face au compositeur.
                        let item_enrichi =
                            match (&album_artist, item["album"]["artist"]["name"].as_str()) {
                                (Some(nom), None) => {
                                    let mut clone = item.clone();
                                    clone["album"]["artist"]["name"] = nom.as_str().into();
                                    Some(clone)
                                }
                                _ => None,
                            };
                        let mut t = Self::map_track(item_enrichi.as_ref().unwrap_or(item));
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
        let params = Self::params_genres(parent_id);
        let data = self
            .api_get_editorial("/genre/list", &params)
            .await
            .map_err(|e| {
                info!(error = %e, "qobuz_genres_failed");
                e
            })?;
        let mut genres: Vec<StreamGenre> = Self::extraire_genres(&data);
        if genres.is_empty() {
            info!(raw = %data, "qobuz_genres_empty_response");
        }
        self.retirer_les_chevrons_dementis(&mut genres);
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
        let album = self.detail_album(album_id).await?;
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
        let album = self.detail_album(album_id).await?;
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

    /// Les meilleurs titres d'un artiste — les SIENS d'abord (#2568).
    ///
    /// Cette fonction ne demandait que `extra=tracks_appears_on`. Ce champ ne
    /// nomme pas les titres de l'artiste : il nomme, mot pour mot, ceux sur
    /// lesquels il *apparaît* — invitations, compilations, participations.
    /// L'unique appelant, `poller.rs`, écrit pourtant en commentaire « les
    /// titres DE l'artiste » : l'écart était dans le code, pas dans
    /// l'intention. Les cinq autres services demandent bien le catalogue de
    /// l'artiste (`deezer.rs` `/artist/{id}/top`, `tidal.rs`
    /// `/artists/{id}/toptracks`, `spotify.rs` `/artists/{id}/top-tracks`) :
    /// Qobuz était le seul à part.
    ///
    /// Deux extras distincts, donc, et dans cet ordre :
    ///  1. `extra=tracks` — le catalogue de l'artiste. `artist/get` l'accepte :
    ///     le 400 relevé sur `extra=similarArtists` énumère lui-même ses
    ///     valeurs (« accepted values are albums, tracks, playlists, … »),
    ///     voir `get_similar_artists` juste en dessous.
    ///  2. `extra=tracks_appears_on` — le comportement d'avant, conservé en
    ///     REPLI. Un artiste qui n'a rien à son nom chez Qobuz (un invité, un
    ///     chef d'orchestre) continue de rendre quelque chose plutôt que rien.
    ///
    /// Chaque réponse est lue sous SA propre clé. L'ancien code lisait
    /// `tracks_appears_on` puis se rabattait sur `tracks` **dans la même
    /// réponse** — un repli qui ne pouvait pas se déclencher, puisqu'on
    /// n'avait jamais demandé `tracks`, et qui laissait croire que les deux
    /// cas étaient couverts.
    ///
    /// Aucun tri n'est appliqué : l'ordre est celui que Qobuz rend. Classer
    /// nous-mêmes sur une popularité que Qobuz ne donne pas reviendrait à
    /// fabriquer le « best of » au lieu de le relayer.
    async fn get_artist_top_tracks(&self, artist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        match self.pistes_extra_artiste(artist_id, "tracks").await {
            Ok(pistes) if !pistes.is_empty() => return Ok(pistes),
            Ok(_) => debug!(artist_id, "qobuz_artiste_sans_titres_a_lui"),
            // Un échec sur le premier extra ne condamne pas la demande : le
            // repli reste à tenter, et c'est lui qui décidera du 500.
            Err(e) => warn!(artist_id, error = %e, "qobuz_artiste_extra_tracks_echec"),
        }
        self.pistes_extra_artiste(artist_id, "tracks_appears_on")
            .await
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

    // ── #1407 : « Lecture en cours » affichait le compositeur ─────────────

    /// Le cas du fil forum : en classique, Qobuz met le compositeur dans
    /// `performer`. La chaîne de rôles désigne l'interprète réel — c'est elle
    /// qui doit gagner, et le compositeur rejoint son propre champ.
    #[test]
    fn le_compositeur_place_dans_performer_cede_a_l_interprete_des_roles() {
        let json = json!({
            "id": 1,
            "title": "Nocturne No. 2",
            "performer": {"name": "Frédéric Chopin"},
            "composer": {"name": "Frédéric Chopin"},
            "performers": "Frédéric Chopin, Composer - Martha Argerich, Piano, MainArtist",
            "album": {"title": "Chopin: Nocturnes", "artist": {"name": "Martha Argerich"}},
            "duration": 271,
        });
        let track = QobuzService::map_track(&json);
        assert_eq!(track.artist, "Martha Argerich");
        assert_eq!(track.composer.as_deref(), Some("Frédéric Chopin"));
    }

    /// Même sans objet `composer`, un performer crédité uniquement à
    /// l'écriture dans la chaîne de rôles n'est pas l'interprète.
    #[test]
    fn un_performer_credite_auteur_seul_cede_au_main_artist_meme_sans_champ_composer() {
        let json = json!({
            "id": 2,
            "title": "Main Title",
            "performer": {"name": "John Williams"},
            "performers": "John Williams, Composer, ComposerLyricist - Boston Pops Orchestra, Orchestra, MainArtist",
            "album": {"title": "Star Wars"},
            "duration": 300,
        });
        let track = QobuzService::map_track(&json);
        assert_eq!(track.artist, "Boston Pops Orchestra");
    }

    /// Le compositeur qui joue ses propres œuvres EST l'interprète : le rôle
    /// MainArtist/Piano dans la chaîne le prouve, il reste l'artiste affiché.
    #[test]
    fn le_compositeur_interprete_de_ses_oeuvres_reste_l_artiste() {
        let json = json!({
            "id": 3,
            "title": "Nuvole Bianche",
            "performer": {"name": "Ludovico Einaudi"},
            "composer": {"name": "Ludovico Einaudi"},
            "performers": "Ludovico Einaudi, Composer, MainArtist, Piano",
            "album": {"title": "Una Mattina", "artist": {"name": "Various Artists"}},
            "duration": 358,
        });
        let track = QobuzService::map_track(&json);
        assert_eq!(track.artist, "Ludovico Einaudi");
    }

    /// Sans chaîne de rôles, l'égalité performer = compositeur suffit à
    /// écarter le performer : l'artiste d'album (l'interprète) le remplace.
    #[test]
    fn sans_roles_le_performer_egal_au_compositeur_replie_sur_l_artiste_d_album() {
        let json = json!({
            "id": 4,
            "title": "Violin Concerto in D",
            "performer": {"name": "Jean Sibelius"},
            "composer": {"name": "Jean Sibelius"},
            "album": {"title": "Sibelius", "artist": {"name": "Hilary Hahn"}},
            "duration": 512,
        });
        let track = QobuzService::map_track(&json);
        assert_eq!(track.artist, "Hilary Hahn");
        assert_eq!(track.composer.as_deref(), Some("Jean Sibelius"));
    }

    /// Quand AUCUN interprète distinct n'existe (l'album du compositeur, tout
    /// le monde porte son nom), le repli historique garde le performer plutôt
    /// que de rendre une chaîne vide — et jamais « Various Artists ».
    #[test]
    fn sans_interprete_distinct_le_performer_reste_plutot_que_vide_ou_various() {
        let tout_chopin = json!({
            "id": 5,
            "title": "Prélude",
            "performer": {"name": "Frédéric Chopin"},
            "composer": {"name": "Frédéric Chopin"},
            "artist": {"name": "Frédéric Chopin"},
            "album": {"title": "Chopin", "artist": {"name": "Frédéric Chopin"}},
            "duration": 100,
        });
        assert_eq!(
            QobuzService::map_track(&tout_chopin).artist,
            "Frédéric Chopin"
        );

        let compilation = json!({
            "id": 6,
            "title": "Prélude",
            "performer": {"name": "Frédéric Chopin"},
            "composer": {"name": "Frédéric Chopin"},
            "album": {"title": "100 Classical Hits", "artist": {"name": "Various Artists"}},
            "duration": 100,
        });
        assert_eq!(
            QobuzService::map_track(&compilation).artist,
            "Frédéric Chopin",
            "« Various Artists » n'apprend rien : mieux vaut le repli historique"
        );
    }

    /// Hors classique rien ne change : performer distinct du compositeur,
    /// il reste l'artiste — le compositeur va dans son champ à lui.
    #[test]
    fn un_performer_distinct_du_compositeur_garde_la_main() {
        let json = json!({
            "id": 7,
            "title": "Take Five",
            "performer": {"name": "The Dave Brubeck Quartet"},
            "composer": {"name": "Paul Desmond"},
            "album": {"title": "Time Out"},
            "duration": 324,
        });
        let track = QobuzService::map_track(&json);
        assert_eq!(track.artist, "The Dave Brubeck Quartet");
        assert_eq!(track.composer.as_deref(), Some("Paul Desmond"));
    }

    /// La chaîne de rôles tolère un nom d'interprète contenant une virgule.
    #[test]
    fn un_nom_d_interprete_avec_virgule_est_reconnu_dans_les_roles() {
        let roles = "Blood, Sweat & Tears, MainArtist - Steve Katz, Composer";
        assert_eq!(
            QobuzService::roles_credites(roles, "Blood, Sweat & Tears"),
            Some(vec!["MainArtist"])
        );
    }

    /// Les items d'album/get n'ont pas de sous-objet `album` : l'artiste
    /// d'album (l'interprète, en tête de réponse) doit être injecté pour que
    /// le compositeur placé dans `performer` ne gagne pas faute de repli.
    #[tokio::test]
    async fn get_album_tracks_replie_sur_l_artiste_d_album_face_au_compositeur() {
        use axum::routing::get;
        use axum::{Json, Router};

        let app = Router::new().route(
            "/album/get",
            get(|| async {
                Json(json!({
                    "id": "abc123",
                    "title": "Bach: Suites pour violoncelle",
                    "artist": {"name": "Ophélie Gaillard"},
                    "image": {"large": "http://img.qobuz.com/bach.jpg"},
                    "tracks": {"items": [
                        {
                            "id": 11,
                            "title": "Suite No. 1: Prélude",
                            "performer": {"name": "Johann Sebastian Bach"},
                            "composer": {"name": "Johann Sebastian Bach"},
                            "duration": 150,
                        },
                    ]},
                }))
            }),
        );
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("port libre");
        let adresse = ecoute.local_addr().expect("adresse locale");
        tokio::spawn(async move {
            let _ = axum::serve(ecoute, app).await;
        });

        let svc = QobuzService::avec_base_forcee(format!("http://{adresse}"));
        let pistes = svc
            .get_album_tracks("abc123")
            .await
            .expect("serveur simulé");

        assert_eq!(pistes.len(), 1);
        assert_eq!(
            pistes[0].artist, "Ophélie Gaillard",
            "l'interprète de l'album, pas le compositeur logé dans performer"
        );
        assert_eq!(pistes[0].composer.as_deref(), Some("Johann Sebastian Bach"));
        assert_eq!(
            pistes[0].album.as_deref(),
            Some("Bach: Suites pour violoncelle")
        );
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

    /// #2115 — la forme du `slug` ne promet plus rien.
    ///
    /// C'est le test de non-régression du défaut signalé par Cyrille Moutia
    /// (fil 1490) : « Be Bop » portait un chevron, et le déplier rendait `[]`.
    /// Aucun champ de la réponse Qobuz n'énonçait de descendance — seule la
    /// forme du slug la laissait supposer. Un slug n'est pas une preuve.
    #[test]
    fn un_slug_ne_promet_aucun_sous_menu() {
        // Slug racine, sans « / » : c'était le cas qui rendait `true`.
        let racine = json!({"id": 40, "name": "Rock", "slug": "rock"});
        assert!(
            !QobuzService::map_genre(&racine).has_children,
            "un slug sans « / » ne dit RIEN de la descendance"
        );

        // Le cas exact du ticket : un sous-genre de niveau 2, sans
        // `subgenresCount` ni `subgenres`, dont le slug ne porte pas de « / ».
        let be_bop = json!({"id": 81, "name": "Be Bop", "slug": "be-bop"});
        assert!(
            !QobuzService::map_genre(&be_bop).has_children,
            "#2115 : « Be Bop » ne doit plus annoncer un sous-menu vide"
        );

        // Slug hiérarchique : inchangé, mais pour la même raison désormais.
        let hard_rock = json!({"id": 41, "name": "Hard Rock", "slug": "rock/hard-rock"});
        assert!(!QobuzService::map_genre(&hard_rock).has_children);

        // Et sans le moindre champ, le silence de Qobuz reste un « non ».
        let muet = json!({"id": 42, "name": "Muet"});
        assert!(!QobuzService::map_genre(&muet).has_children);
    }

    /// L'inverse du précédent : quand Qobuz ÉNONCE une descendance, on la
    /// relaie. Sans ce test, `has_children = false` en dur passerait le
    /// test ci-dessus et supprimerait tout l'arbre.
    #[test]
    fn un_compte_annonce_par_qobuz_est_relaye() {
        let dit_par_le_compte = json!({"id": 80, "name": "Jazz", "subgenresCount": 15});
        assert!(QobuzService::map_genre(&dit_par_le_compte).has_children);

        let dit_par_le_tableau = json!({"id": 80, "name": "Jazz", "subgenres": [{"id": 81}]});
        assert!(QobuzService::map_genre(&dit_par_le_tableau).has_children);
    }

    /// Le cache éditorial, prérempli à la main : AUCUN appel réseau.
    ///
    /// `api_get_editorial` sert le cache avant de toucher au réseau, donc un
    /// `get_genres` dont la clé est en cache n'ouvre pas de socket. Ce test
    /// tomberait en erreur de connexion s'il en ouvrait une.
    fn service_avec_genres_en_cache(entrees: &[(Option<&str>, serde_json::Value)]) -> QobuzService {
        let svc = QobuzService::new("app".into(), "secret".into());
        for (parent, donnees) in entrees {
            let cle = QobuzService::cle_cache("/genre/list", &QobuzService::params_genres(*parent));
            svc.cache_set(cle, donnees.clone());
        }
        svc
    }

    /// #2115, deuxième garde-fou — si Qobuz ment MALGRÉ un `subgenresCount`,
    /// le vide déjà constaté retire le chevron. Zéro requête supplémentaire.
    #[tokio::test]
    async fn un_enfant_deja_constate_vide_perd_son_chevron() {
        let svc = service_avec_genres_en_cache(&[
            (
                Some("80"),
                json!({"genres": {"items": [
                    {"id": 81, "name": "Be Bop", "subgenresCount": 3},
                    {"id": 82, "name": "Cool jazz", "subgenresCount": 7},
                ]}}),
            ),
            // « Be Bop » a été déplié une fois : Qobuz a rendu une liste vide.
            (Some("81"), json!({"genres": {"items": []}})),
        ]);

        let genres = svc.get_genres(Some("80")).await.expect("cache servi");
        assert_eq!(genres.len(), 2);
        assert!(
            !genres[0].has_children,
            "« Be Bop » a été constaté vide : plus de chevron"
        );
        assert!(
            genres[1].has_children,
            "« Cool jazz » n'a jamais été déplié : on ne retire rien sans preuve"
        );
    }

    /// Le pendant du précédent : un enfant constaté PEUPLÉ garde son chevron.
    /// Sans lui, un `has_children = false` inconditionnel passerait.
    #[tokio::test]
    async fn un_enfant_constate_peuple_garde_son_chevron() {
        let svc = service_avec_genres_en_cache(&[
            (
                Some("80"),
                json!({"genres": {"items": [{"id": 81, "name": "Be Bop", "subgenresCount": 3}]}}),
            ),
            (
                Some("81"),
                json!({"genres": {"items": [{"id": 999, "name": "Hard bop"}]}}),
            ),
        ]);

        let genres = svc.get_genres(Some("80")).await.expect("cache servi");
        assert!(genres[0].has_children);
    }

    /// Un démenti périmé ne compte plus : la durée de vie du retrait est
    /// celle du cache éditorial, et pas davantage.
    #[tokio::test]
    async fn un_dementi_perime_ne_retire_plus_le_chevron() {
        let svc = service_avec_genres_en_cache(&[(
            Some("80"),
            json!({"genres": {"items": [{"id": 81, "name": "Be Bop", "subgenresCount": 3}]}}),
        )]);
        // Le vide constaté il y a plus de TTL_EDITORIAL.
        let cle = QobuzService::cle_cache("/genre/list", &QobuzService::params_genres(Some("81")));
        svc.cache_editorial.lock().unwrap().insert(
            cle,
            EntreeCache {
                donnees: json!({"genres": {"items": []}}),
                cree: Instant::now() - TTL_EDITORIAL - Duration::from_secs(1),
            },
        );

        let genres = svc.get_genres(Some("80")).await.expect("cache servi");
        assert!(
            genres[0].has_children,
            "un constat périmé n'autorise plus à retirer l'affordance"
        );
    }

    /// La clé lue par le garde-fou DOIT être celle écrite par l'appel. Si les
    /// deux divergent, le retrait ne se déclenche jamais — en silence.
    #[test]
    fn la_cle_du_dementi_est_celle_de_l_appel() {
        assert_eq!(
            QobuzService::cle_cache("/genre/list", &QobuzService::params_genres(Some("81"))),
            QobuzService::cle_cache(
                "/genre/list",
                &[("offset", "0"), ("limit", "500"), ("parent_id", "81")]
            )
        );
        assert_eq!(
            QobuzService::cle_cache("/genre/list", &QobuzService::params_genres(None)),
            QobuzService::cle_cache("/genre/list", &[("offset", "0"), ("limit", "500")])
        );
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

    // ── #2568 : « top tracks » demandait les participations ───────────────

    /// Le cache éditorial prérempli pour `/artist/get`, un extra par entrée :
    /// AUCUN appel réseau, donc aucun identifiant Qobuz en jeu.
    ///
    /// La liste de paramètres doit être MOT POUR MOT celle de
    /// `pistes_extra_artiste` : la clé est calculée dessus. Si elle divergeait,
    /// le cache manquerait et le test partirait sur le réseau — c'est le seul
    /// symptôme, et il est bruyant (erreur de connexion, jamais un faux vert).
    fn service_avec_extras_artiste(
        artist_id: &str,
        entrees: &[(&str, serde_json::Value)],
    ) -> QobuzService {
        let svc = QobuzService::new("app".into(), "secret".into());
        for (extra, donnees) in entrees {
            let cle = QobuzService::cle_cache(
                "/artist/get",
                &[("artist_id", artist_id), ("extra", extra), ("limit", "20")],
            );
            svc.cache_set(cle, donnees.clone());
        }
        svc
    }

    fn piste_qobuz(id: u64, titre: &str, interprete: &str, album: &str) -> serde_json::Value {
        json!({
            "id": id,
            "title": titre,
            "performer": {"name": interprete},
            "album": {"title": album, "id": 900 + id},
            "duration": 200,
        })
    }

    /// Le cœur de #2568 : un « best of » sert le catalogue de l'artiste, pas
    /// les disques où il est invité.
    ///
    /// Qobuz répond aux deux extras. L'ancien code ne demandait que
    /// `tracks_appears_on` et rendait donc « Comfortably Numb (live) »,
    /// interprété par Roger Waters, sous une étiquette « Pink Floyd ».
    #[tokio::test]
    async fn le_best_of_sert_les_titres_de_l_artiste_pas_ses_participations() {
        let svc = service_avec_extras_artiste(
            "38324",
            &[
                (
                    "tracks",
                    json!({"tracks": {"items": [
                        piste_qobuz(1, "Money", "Pink Floyd", "The Dark Side of the Moon"),
                    ]}}),
                ),
                (
                    "tracks_appears_on",
                    json!({"tracks_appears_on": {"items": [
                        piste_qobuz(2, "Comfortably Numb (live)", "Roger Waters", "In the Flesh"),
                    ]}}),
                ),
            ],
        );

        let pistes = svc
            .get_artist_top_tracks("38324")
            .await
            .expect("cache servi");

        assert_eq!(pistes.len(), 1, "1 extra examiné, 1 piste attendue");
        assert_eq!(pistes[0].title, "Money");
        assert_eq!(
            pistes[0].artist, "Pink Floyd",
            "l'interprète est relayé tel quel — jamais réécrit avec l'artiste demandé"
        );
        assert_eq!(
            pistes[0].album.as_deref(),
            Some("The Dark Side of the Moon")
        );
    }

    /// Le pendant : sans titres à son nom, les participations restent servies.
    /// Sans ce test, supprimer purement le repli passerait pour un correctif.
    #[tokio::test]
    async fn sans_titres_a_lui_les_participations_restent_servies() {
        let svc = service_avec_extras_artiste(
            "7777",
            &[
                ("tracks", json!({"tracks": {"items": []}})),
                (
                    "tracks_appears_on",
                    json!({"tracks_appears_on": {"items": [
                        piste_qobuz(3, "So What", "Miles Davis", "Kind of Blue"),
                        piste_qobuz(4, "Blue in Green", "Miles Davis", "Kind of Blue"),
                    ]}}),
                ),
            ],
        );

        let pistes = svc
            .get_artist_top_tracks("7777")
            .await
            .expect("cache servi");

        assert_eq!(pistes.len(), 2, "2 participations en cache, 2 servies");
        assert_eq!(pistes[0].title, "So What");
        assert_eq!(pistes[1].title, "Blue in Green");
    }
}

/// Ouverture d'un album : un seul aller-retour amont, pas quatre (#2190).
///
/// Aucun de ces essais ne touche l'API Qobuz : ils parlent à un serveur simulé
/// lié sur `127.0.0.1:0`, qui COMPTE les `/album/get` qu'il reçoit. C'est ce
/// compteur qui est la mesure — la durée en découle.
#[cfg(test)]
mod tests_cache_album {
    use super::*;
    use axum::extract::Query;
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::json;
    use std::collections::HashMap as Carte;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Un Qobuz simulé qui rend un album complet et compte ses `/album/get`.
    ///
    /// `latence` simule le coût réel d'un aller-retour vers Qobuz : c'est lui
    /// qu'on paie quatre fois quand rien n'est partagé.
    async fn qobuz_album_simule(latence: Duration) -> (String, Arc<AtomicUsize>) {
        let appels = Arc::new(AtomicUsize::new(0));
        let compteur = appels.clone();

        let app = Router::new()
            .route(
                "/album/get",
                get(move |Query(q): Query<Carte<String, String>>| {
                    let compteur = compteur.clone();
                    async move {
                        compteur.fetch_add(1, Ordering::SeqCst);
                        if !latence.is_zero() {
                            tokio::time::sleep(latence).await;
                        }
                        let id = q.get("album_id").cloned().unwrap_or_default();
                        Json(json!({
                            "id": id,
                            "title": format!("album-{id}"),
                            "artist": {"name": "Ella Fitzgerald"},
                            "image": {"large": "http://img.qobuz.test/a.jpg"},
                            "genre": {"id": 64, "name": "Jazz"},
                            "label": {"id": 7, "name": "Verve"},
                            "tracks_count": 1,
                            "tracks": {"items": [
                                {"id": 11, "title": "Summertime", "duration": 200}
                            ]},
                        }))
                    }
                }),
            )
            // `get_album_label` pagine ensuite le catalogue du label ; il n'est
            // pas le sujet ici, donc le simulé le rend vide en une page.
            .route(
                "/label/get",
                get(|| async { Json(json!({"albums": {"items": [], "total": 0}})) }),
            );

        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("port libre");
        let adresse = ecoute.local_addr().expect("adresse locale");
        tokio::spawn(async move {
            let _ = axum::serve(ecoute, app).await;
        });
        (format!("http://{adresse}"), appels)
    }

    /// LE défaut de la #2190. Le client demande quatre choses à l'ouverture
    /// d'un album ; le serveur repartait quatre fois chercher la MÊME réponse.
    #[tokio::test]
    async fn une_ouverture_d_album_ne_paie_qu_un_aller_retour() {
        let (base, appels) = qobuz_album_simule(Duration::ZERO).await;
        let svc = QobuzService::avec_base_forcee(base);

        let album = svc.get_album("42").await.expect("serveur simulé");
        let pistes = svc.get_album_tracks("42").await.expect("serveur simulé");
        let contexte = svc.get_album_context("42").await.expect("serveur simulé");
        let label = svc.get_album_label("42").await.expect("serveur simulé");

        assert_eq!(
            appels.load(Ordering::SeqCst),
            1,
            "quatre routes, une seule lecture amont — c'est le défaut de la #2190"
        );

        // Et les quatre réponses restent justes : le cache ne dégrade rien.
        assert_eq!(album.title, "album-42");
        assert_eq!(album.artist, "Ella Fitzgerald");
        assert_eq!(pistes.len(), 1);
        assert_eq!(pistes[0].title, "Summertime");
        assert_eq!(contexte.genre_name.as_deref(), Some("Jazz"));
        assert_eq!(contexte.label_name.as_deref(), Some("Verve"));
        assert_eq!(label.name, "Verve");
    }

    /// Le cache est par album : deux albums ne peuvent pas se confondre.
    #[tokio::test]
    async fn deux_albums_distincts_gardent_chacun_leur_reponse() {
        let (base, appels) = qobuz_album_simule(Duration::ZERO).await;
        let svc = QobuzService::avec_base_forcee(base);

        let premier = svc.get_album("1").await.expect("serveur simulé");
        let second = svc.get_album("2").await.expect("serveur simulé");
        let relecture = svc.get_album("1").await.expect("serveur simulé");

        assert_eq!(
            appels.load(Ordering::SeqCst),
            2,
            "deux albums = deux lectures ; la relecture du premier est servie du cache"
        );
        assert_eq!(premier.title, "album-1");
        assert_eq!(second.title, "album-2");
        assert_eq!(relecture.title, "album-1", "pas de confusion entre albums");
    }

    /// La mesure, sur une latence amont réaliste : l'ouverture ne paie plus
    /// qu'UN aller-retour, pas la somme de trois.
    #[tokio::test]
    async fn l_ouverture_ne_paie_plus_qu_une_latence_amont() {
        const LATENCE: Duration = Duration::from_millis(200);
        let (base, appels) = qobuz_album_simule(LATENCE).await;
        let svc = QobuzService::avec_base_forcee(base);

        let debut = Instant::now();
        svc.get_album("7").await.expect("serveur simulé");
        svc.get_album_tracks("7").await.expect("serveur simulé");
        svc.get_album_context("7").await.expect("serveur simulé");
        let ecoule = debut.elapsed();

        assert_eq!(appels.load(Ordering::SeqCst), 1);
        assert!(
            ecoule < LATENCE * 2,
            "l'ouverture a pris {ecoule:?} ; avant le correctif elle payait 3 × {LATENCE:?}"
        );
    }

    /// Une navigation longue ne doit pas faire enfler la mémoire : chaque
    /// entrée porte une liste de pistes complète.
    #[tokio::test]
    async fn le_cache_d_albums_tient_son_plafond() {
        let (base, _) = qobuz_album_simule(Duration::ZERO).await;
        let svc = QobuzService::avec_base_forcee(base);

        for i in 0..(MAX_ALBUMS_CACHE + 8) {
            svc.get_album(&i.to_string()).await.expect("serveur simulé");
        }

        let gardes = svc.cache_album.lock().expect("verrou d'essai").len();
        assert!(
            gardes <= MAX_ALBUMS_CACHE,
            "{gardes} albums gardés pour un plafond de {MAX_ALBUMS_CACHE}"
        );
    }
}

/// Pagination de la recherche Qobuz (#2160).
///
/// Aucun de ces essais n'appelle l'API Qobuz : les décisions de pagination et
/// la fusion sont des fonctions pures, et le seul aller-retour HTTP se fait
/// contre un serveur simulé lié sur `127.0.0.1:0`.
#[cfg(test)]
mod tests_recherche_paginee {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap as Carte;
    use std::sync::Arc;

    #[test]
    fn plafond_recherche_traduit_tous_et_borne_les_demandes_extravagantes() {
        // Les quatre choix offerts par l'écran.
        assert_eq!(plafond_recherche(50), 50);
        assert_eq!(plafond_recherche(100), 100);
        assert_eq!(plafond_recherche(200), 200);
        // « Tous » : 0 est la convention du client, bornée ici.
        assert_eq!(plafond_recherche(0), PLAFOND_RECHERCHE);
        // Et un client qui demanderait n'importe quoi ne déclenche pas deux
        // mille requêtes chez Qobuz.
        assert_eq!(plafond_recherche(100_000), PLAFOND_RECHERCHE);
        // Non-régression : les appels internes existants (recherche fédérée à
        // 30, sonde de santé à 10, défaut de route à 20) restent inchangés.
        assert_eq!(plafond_recherche(30), 30);
        assert_eq!(plafond_recherche(20), 20);
        assert_eq!(plafond_recherche(10), 10);
    }

    #[test]
    fn seules_les_demandes_au_dela_d_une_page_paginent() {
        // Composé avec `plafond_recherche` : c'est l'enchaînement exact de la
        // production, y compris la traduction de « Tous ».
        let pagine = |limite: usize| recherche_paginee(plafond_recherche(limite));
        assert!(!pagine(10));
        assert!(!pagine(20));
        assert!(!pagine(30));
        assert!(!pagine(50), "50 tient dans une page Qobuz");
        assert!(pagine(51));
        assert!(pagine(100));
        assert!(pagine(200));
        assert!(pagine(0), "« Tous » pagine");
    }

    #[test]
    fn offsets_recherche_couvre_exactement_le_volume_demande() {
        // Première page pleine partout, Qobuz annonce beaucoup plus.
        let comptes = [50, 50, 50, 50];
        let totaux = [5000, 3000, 900, 120];
        assert_eq!(offsets_recherche(&comptes, &totaux, 100, 0), vec![50]);
        assert_eq!(
            offsets_recherche(&comptes, &totaux, 200, 0),
            vec![50, 100, 150]
        );
        assert_eq!(
            offsets_recherche(&comptes, &totaux, PLAFOND_RECHERCHE, 0).len(),
            9,
            "500 par catégorie = dix pages, dont une déjà chargée"
        );
    }

    #[test]
    fn offsets_recherche_s_arrete_a_la_categorie_la_plus_fournie() {
        // Une seule requête sert les quatre catégories : c'est le plus gros
        // `total` qui commande, pas le premier.
        let comptes = [50, 7, 0, 0];
        let totaux = [60, 7, 0, 0];
        assert_eq!(offsets_recherche(&comptes, &totaux, 500, 0), vec![50]);
    }

    #[test]
    fn offsets_recherche_ne_pagine_pas_derriere_une_premiere_page_incomplete() {
        // Page incomplète = Qobuz n'a plus rien, quel que soit le plafond.
        assert!(offsets_recherche(&[12, 3, 0, 0], &[12, 3, 0, 0], 500, 0).is_empty());
        assert!(offsets_recherche(&[0, 0, 0, 0], &[0, 0, 0, 0], 500, 0).is_empty());
    }

    /// Une page de recherche : `n` éléments par catégorie, numérotés à partir
    /// de `depart`, et le `total` que Qobuz annonce.
    fn page(depart: usize, n: usize, total: usize) -> serde_json::Value {
        let elements = |prefixe: &str| -> Vec<serde_json::Value> {
            (depart..depart + n)
                .map(|i| {
                    json!({
                        "id": i as u64,
                        "title": format!("{prefixe}-{i}"),
                        "name": format!("{prefixe}-{i}"),
                    })
                })
                .collect()
        };
        json!({
            "tracks": {"items": elements("piste"), "total": total},
            "albums": {"items": elements("album"), "total": total},
            "artists": {"items": elements("artiste"), "total": total},
            "playlists": {"items": elements("liste"), "total": total},
        })
    }

    #[test]
    fn fusionner_recherche_concatene_les_pages_dans_l_ordre_recu() {
        let pages = [page(0, 50, 200), page(50, 50, 200), page(100, 50, 200)];
        let r = QobuzService::fusionner_recherche(&pages, 200);
        assert_eq!(r.tracks.len(), 150);
        assert_eq!(r.tracks[0].id, "0");
        assert_eq!(r.tracks[49].id, "49");
        assert_eq!(r.tracks[50].id, "50", "la 2e page suit la 1re, sans trou");
        assert_eq!(r.tracks[149].id, "149");
        assert_eq!(r.albums.len(), 150);
        assert_eq!(r.artists.len(), 150);
        assert_eq!(r.playlists.len(), 150);
    }

    #[test]
    fn fusionner_recherche_borne_chaque_categorie_au_plafond() {
        let pages = [page(0, 50, 500), page(50, 50, 500), page(100, 50, 500)];
        let r = QobuzService::fusionner_recherche(&pages, 100);
        assert_eq!(r.tracks.len(), 100, "100 demandés, 100 rendus");
        assert_eq!(r.tracks[99].id, "99");
        assert_eq!(r.albums.len(), 100);
    }

    #[test]
    fn fusionner_recherche_ne_complete_pas_une_categorie_courte() {
        // Le plafond est un maximum PAR catégorie, pas un quota : une
        // catégorie épuisée reste courte, les autres ne sont pas tronquées.
        let page_mixte = json!({
            "tracks": {"items": (0..50).map(|i| json!({"id": i as u64, "title": "t"})).collect::<Vec<_>>(), "total": 50},
            "artists": {"items": [{"id": 1, "name": "Miles Davis"}], "total": 1},
        });
        let r = QobuzService::fusionner_recherche(&[page_mixte], 200);
        assert_eq!(r.tracks.len(), 50);
        assert_eq!(r.artists.len(), 1);
        assert!(r.albums.is_empty(), "catégorie absente = liste vide");
        assert!(r.playlists.is_empty());
    }

    /// Serveur qui imite `/catalog/search` : il rend `limit` éléments par
    /// catégorie à partir de `offset` — jamais au-delà du `total` annoncé — et
    /// NOTE chaque décalage reçu, ce qui est la preuve cherchée.
    async fn qobuz_simule(totaux: Carte<&'static str, usize>) -> (String, Arc<Mutex<Vec<usize>>>) {
        qobuz_simule_interne(totaux, false).await
    }

    /// Comme `qobuz_simule`, mais chaque page repond d'autant plus vite que son
    /// decalage est grand : les reponses arrivent donc dans l'ordre INVERSE de
    /// celui ou elles ont ete demandees. Sans ce decalage artificiel, un
    /// serveur local repond toujours dans l'ordre de la demande et un essai
    /// d'ordre ne prouve rien.
    async fn qobuz_simule_desordre(
        totaux: Carte<&'static str, usize>,
    ) -> (String, Arc<Mutex<Vec<usize>>>) {
        qobuz_simule_interne(totaux, true).await
    }

    async fn qobuz_simule_interne(
        totaux: Carte<&'static str, usize>,
        desordre: bool,
    ) -> (String, Arc<Mutex<Vec<usize>>>) {
        use axum::extract::Query as ExtraitQuery;
        use axum::routing::get;
        use axum::{Json, Router};

        let vus: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
        let vus_srv = vus.clone();
        let totaux = Arc::new(totaux);

        let app = Router::new().route(
            "/catalog/search",
            get(
                move |ExtraitQuery(p): ExtraitQuery<Carte<String, String>>| {
                    let vus = vus_srv.clone();
                    let totaux = totaux.clone();
                    async move {
                        let offset: usize =
                            p.get("offset").and_then(|v| v.parse().ok()).unwrap_or(0);
                        let limit: usize = p.get("limit").and_then(|v| v.parse().ok()).unwrap_or(0);
                        vus.lock().expect("verrou d'essai").push(offset);
                        if desordre {
                            let attente = 400u64.saturating_sub(offset as u64);
                            tokio::time::sleep(Duration::from_millis(attente)).await;
                        }

                        let mut corps = serde_json::Map::new();
                        for (categorie, total) in totaux.iter() {
                            let fin = (offset + limit).min(*total);
                            let items: Vec<serde_json::Value> = (offset..fin)
                                .map(|i| {
                                    json!({
                                        "id": i as u64,
                                        "title": format!("{categorie}-{i}"),
                                        "name": format!("{categorie}-{i}"),
                                    })
                                })
                                .collect();
                            corps.insert(
                                (*categorie).to_string(),
                                json!({"items": items, "total": *total}),
                            );
                        }
                        Json(serde_json::Value::Object(corps))
                    }
                },
            ),
        );

        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("port libre");
        let adresse = ecoute.local_addr().expect("adresse locale");
        tokio::spawn(async move {
            let _ = axum::serve(ecoute, app).await;
        });
        (format!("http://{adresse}"), vus)
    }

    fn decalages(vus: &Arc<Mutex<Vec<usize>>>) -> Vec<usize> {
        let mut v = vus.lock().expect("verrou d'essai").clone();
        v.sort_unstable();
        v
    }

    #[tokio::test]
    async fn au_dela_de_cinquante_la_recherche_demande_les_pages_suivantes() {
        let (base, vus) = qobuz_simule(Carte::from([
            ("tracks", 200usize),
            ("albums", 200),
            ("artists", 200),
            ("playlists", 200),
        ]))
        .await;
        let svc = QobuzService::avec_base_forcee(base);

        let r = svc
            .search("miles davis", 200)
            .await
            .expect("serveur simulé");

        assert_eq!(
            decalages(&vus),
            vec![0, 50, 100, 150],
            "quatre pages par offset — c'est le défaut de la #2160"
        );
        assert_eq!(r.tracks.len(), 200);
        assert_eq!(r.albums.len(), 200);
        assert_eq!(r.artists.len(), 200);
        assert_eq!(r.tracks[0].id, "0");
        assert_eq!(r.tracks[199].id, "199", "les pages arrivent dans l'ordre");
    }

    #[tokio::test]
    async fn les_pages_sont_rangees_par_decalage_et_non_par_ordre_d_arrivee() {
        // Le serveur rend ici la DERNIERE page en premier. Si les reponses
        // etaient concatenees dans l'ordre ou elles arrivent, l'ecran
        // afficherait le titre 150 en 51e position.
        let (base, vus) = qobuz_simule_desordre(Carte::from([("tracks", 200usize)])).await;
        let svc = QobuzService::avec_base_forcee(base);

        let r = svc
            .search("miles davis", 200)
            .await
            .expect("serveur simule");

        assert_eq!(decalages(&vus), vec![0, 50, 100, 150]);
        assert_eq!(r.tracks.len(), 200);
        let ids: Vec<String> = r.tracks.iter().map(|t| t.id.clone()).collect();
        let attendus: Vec<String> = (0..200).map(|i| i.to_string()).collect();
        assert_eq!(
            ids, attendus,
            "les pages lentes ne doublent pas les rapides"
        );
    }

    #[tokio::test]
    async fn une_recherche_de_cinquante_tient_en_un_seul_aller_retour() {
        // Non-régression : le comportement d'avant la #2160 pour tout ce qui
        // demande une page ou moins.
        let (base, vus) = qobuz_simule(Carte::from([("tracks", 200usize)])).await;
        let svc = QobuzService::avec_base_forcee(base);

        let r = svc.search("miles davis", 50).await.expect("serveur simulé");

        assert_eq!(decalages(&vus), vec![0], "une seule requête");
        assert_eq!(r.tracks.len(), 50);
    }

    #[tokio::test]
    async fn tous_pagine_jusqu_au_plafond_documente_et_pas_au_dela() {
        let (base, vus) = qobuz_simule(Carte::from([("tracks", 5000usize)])).await;
        let svc = QobuzService::avec_base_forcee(base);

        let r = svc.search("jazz", 0).await.expect("serveur simulé");

        assert_eq!(r.tracks.len(), PLAFOND_RECHERCHE);
        assert_eq!(
            decalages(&vus).len(),
            PLAFOND_RECHERCHE / TAILLE_PAGE_RECHERCHE,
            "dix pages, pas cent : « Tous » est borné"
        );
    }

    #[tokio::test]
    async fn la_pagination_s_arrete_quand_qobuz_n_a_plus_rien() {
        let (base, vus) = qobuz_simule(Carte::from([("tracks", 60usize), ("albums", 10)])).await;
        let svc = QobuzService::avec_base_forcee(base);

        let r = svc.search("obscur", 200).await.expect("serveur simulé");

        assert_eq!(
            decalages(&vus),
            vec![0, 50],
            "200 demandés mais 60 existants : deux pages, pas quatre"
        );
        assert_eq!(r.tracks.len(), 60);
        assert_eq!(r.albums.len(), 10);
    }

    // -----------------------------------------------------------------------
    // Curseur et compte-rendu de page (#2160, second lot)
    //
    // La #2754 avait livré la pagination INTERNE : une demande de 200 va
    // chercher quatre pages. Restaient deux choses sans lesquelles un écran ne
    // peut pas construire un « Charger plus » : un CURSEUR pour demander la
    // suite, et un compte-rendu disant s'il en reste.

    #[tokio::test]
    async fn un_curseur_decale_les_pages_demandees_a_qobuz() {
        let (base, vus) = qobuz_simule(Carte::from([("tracks", 5000usize)])).await;
        let svc = QobuzService::avec_base_forcee(base);

        let p = svc
            .search_page("jazz", 100, 500)
            .await
            .expect("serveur simulé");

        assert_eq!(
            decalages(&vus),
            vec![500, 550],
            "le curseur décale les décalages : on reprend où l'écran s'est arrêté"
        );
        assert_eq!(p.offset, 500, "la page se nomme elle-même");
        assert_eq!(p.results.tracks.len(), 100);
        assert_eq!(
            p.results.tracks[0].id, "500",
            "la suite commence au curseur, pas au début"
        );
        assert_eq!(p.results.tracks[99].id, "599");
    }

    #[tokio::test]
    async fn le_plafond_se_compte_a_partir_du_curseur_et_non_du_debut() {
        // Le piège : `PLAFOND_RECHERCHE` vaut 500. Un « Charger plus » posté à
        // 500 rendrait ZÉRO si le plafond se comptait depuis le début du
        // catalogue — l'écran resterait bloqué sur sa cinquième page.
        let (base, _vus) = qobuz_simule(Carte::from([("tracks", 5000usize)])).await;
        let svc = QobuzService::avec_base_forcee(base);

        let p = svc
            .search_page("jazz", 0, 500)
            .await
            .expect("serveur simulé");

        assert_eq!(p.results.tracks.len(), PLAFOND_RECHERCHE);
        assert_eq!(p.results.tracks[0].id, "500");
        assert_eq!(p.results.tracks[499].id, "999");
    }

    #[tokio::test]
    async fn la_page_annonce_les_totaux_de_qobuz_et_qu_il_en_reste() {
        let (base, _vus) = qobuz_simule(Carte::from([
            ("tracks", 5000usize),
            ("albums", 300),
            ("artists", 12),
            ("playlists", 0),
        ]))
        .await;
        let svc = QobuzService::avec_base_forcee(base);

        let p = svc
            .search_page("jazz", 50, 0)
            .await
            .expect("serveur simulé");

        assert_eq!(
            p.totals,
            SearchTotals {
                tracks: 5000,
                albums: 300,
                artists: 12,
                playlists: 0,
            },
            "les totaux sont ceux annoncés par Qobuz, pas la taille du tableau rendu"
        );
        assert_eq!(p.results.tracks.len(), 50);
        assert_eq!(
            p.results.artists.len(),
            12,
            "une catégorie courte reste courte"
        );
        assert!(p.has_more, "50 rendus sur 5000 : il en reste");
    }

    #[tokio::test]
    async fn tous_se_declare_tronque_quand_le_plafond_a_coupe() {
        // Sans ce drapeau, un écran prendrait 500 titres sur 5000 pour la
        // totalité — c'est la question posée par FabienM sur le fil 1611.
        let (base, _vus) = qobuz_simule(Carte::from([("tracks", 5000usize)])).await;
        let svc = QobuzService::avec_base_forcee(base);

        let p = svc.search_page("jazz", 0, 0).await.expect("serveur simulé");

        assert_eq!(p.results.tracks.len(), PLAFOND_RECHERCHE);
        assert!(p.truncated, "« Tous » borné à 500 sur 5000 annoncés");
        assert!(p.has_more);
        assert_eq!(p.totals.tracks, 5000);
    }

    #[tokio::test]
    async fn une_page_qui_epuise_le_catalogue_n_annonce_aucune_suite() {
        let (base, _vus) = qobuz_simule(Carte::from([("tracks", 30usize), ("albums", 7)])).await;
        let svc = QobuzService::avec_base_forcee(base);

        let p = svc
            .search_page("obscur", 50, 0)
            .await
            .expect("serveur simulé");

        assert_eq!(p.results.tracks.len(), 30);
        assert!(!p.has_more, "tout est rendu : pas de « Charger plus »");
        assert!(!p.truncated, "c'est Qobuz qui s'arrête, pas notre plafond");
    }

    #[tokio::test]
    async fn la_derniere_page_d_un_catalogue_epuise_ne_rappelle_pas_a_la_suite() {
        // 120 titres, l'écran en a déjà 100 : la page suivante rend les 20
        // derniers et referme.
        let (base, _vus) = qobuz_simule(Carte::from([("tracks", 120usize)])).await;
        let svc = QobuzService::avec_base_forcee(base);

        let p = svc
            .search_page("jazz", 100, 100)
            .await
            .expect("serveur simulé");

        assert_eq!(p.results.tracks.len(), 20);
        assert_eq!(p.offset, 100);
        assert!(!p.has_more, "100 + 20 = 120 = le total annoncé");
    }

    #[tokio::test]
    async fn sans_curseur_une_page_de_cinquante_tient_toujours_en_un_aller_retour() {
        // Non-régression du chemin d'avant #2160 à travers la NOUVELLE méthode :
        // c'est celui qu'empruntent le défaut de route (20), les sondes (5) et
        // le transfert de playlist (10).
        let (base, vus) = qobuz_simule(Carte::from([("tracks", 200usize)])).await;
        let svc = QobuzService::avec_base_forcee(base);

        let p = svc
            .search_page("miles", 50, 0)
            .await
            .expect("serveur simulé");

        assert_eq!(decalages(&vus), vec![0], "une seule requête");
        assert_eq!(p.results.tracks.len(), 50);
        assert_eq!(p.offset, 0);
    }

    /// `search()` et `search_page(.., 0)` doivent rendre les MÊMES éléments :
    /// deux chemins qui divergeraient laisseraient l'ancien client et le
    /// nouveau afficher deux listes différentes pour la même requête.
    #[tokio::test]
    async fn la_page_et_la_recherche_historique_rendent_la_meme_liste() {
        let (base, _vus) = qobuz_simule(Carte::from([("tracks", 300usize), ("albums", 80)])).await;
        let svc = QobuzService::avec_base_forcee(base);

        let ancienne = svc.search("jazz", 200).await.expect("serveur simulé");
        let page = svc
            .search_page("jazz", 200, 0)
            .await
            .expect("serveur simulé");

        let ids = |v: &[StreamTrack]| v.iter().map(|t| t.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&ancienne.tracks), ids(&page.results.tracks));
        assert_eq!(ancienne.albums.len(), page.results.albums.len());
    }
}
