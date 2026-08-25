//! Bandcamp as a native [`TunePlugin`] (#1768).
//!
//! Extrait mot pour mot de `tune-server`'s always-on core (`routes/bandcamp.rs`)
//! pour que le serveur nu ne le porte plus : construire
//! `tune-server --features bandcamp` remonte ces routes, montées par l'hôte sur
//! `/api/v1/ext/bandcamp/…` (le préfixe vient de `name()` — un plugin ne choisit
//! jamais le sien).
//!
//! Bandcamp est **natif** et non WASM, comme `tune-dj`, mais pour une raison
//! différente : il n'a besoin d'aucun accès audio, seulement de parler HTTP à
//! un tiers. Le WASM lui coûterait un pont réseau sans rien lui apporter.
//!
//! Pas de `HostServices` ici, contrairement à `tune-dj` et `tune-karaoke` : ce
//! plugin ne touche ni la base, ni la lecture, ni les réglages. Son routeur est
//! sans état et passe par `tune_core::http::client::shared()` — le client
//! partagé du serveur, celui vers lequel #1749 a fait converger tous les
//! appelants. Lui inventer un `HostServices` vide serait de la cérémonie.
//!
//! # Portée
//!
//! Recherche, découverte et tags sont **déplacés** depuis le cœur, à
//! l'identique. S'y ajoute la **lecture** : `GET /album?url=…` résout une page
//! publique en pistes jouables.
//!
//! Cette lecture est du `mp3-128`, et c'est la SEULE qualité que Bandcamp
//! serve sans session d'achat — vérifié en sondant l'API avant d'écrire :
//! `redownload_urls`, qui porte les fichiers achetés en lossless, reste vide
//! sans le cookie de session.
//!
//! Je recommandais de ne pas l'exposer, précisément parce que Tune s'adresse à
//! des gens qui règlent leur chaîne au bit près. Bertrand a tranché l'inverse.
//! La contrepartie est tenue ici : la qualité est annoncée sur l'album, sur
//! chaque piste, et accompagnée d'un `lossless: false` — pour que personne ne
//! juge un disque sur 128 kbit/s en croyant l'avoir entendu.
//!
//! Reste au lot 2 la collection d'un acheteur, qui rapproche ses achats de sa
//! bibliothèque locale. Voir #1768.

use async_trait::async_trait;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::event_bus::TuneEvent;
use tune_core::plugin_sdk::{PluginContext, TunePlugin};

const BC_SEARCH_API: &str = "https://bandcamp.com/api/bcsearch_public_api/1/autocomplete_elastic";
const BC_DISCOVER_API: &str = "https://bandcamp.com/api/discover/3/get_web";

/// Services de l'hôte remis au plugin à la construction.
///
/// Le lot 1 n'en avait **aucun**, et c'était juste : recherche et découverte
/// ne touchent ni la base, ni la lecture, ni les réglages. Le lot 2 mémorise
/// un pseudo Bandcamp et le `fan_id` résolu — il lui faut donc la base. La
/// décision d'hier n'était pas fausse, elle a cessé de l'être ; passée
/// explicitement ici plutôt que tirée du `PluginContext`, comme `tune-dj`.
pub struct HostServices {
    pub backend: std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
}

/// Le plugin Bandcamp. Possède la base pour les réglages du lot 2.
pub struct BandcampPlugin {
    backend: std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
}

impl BandcampPlugin {
    pub fn new(services: HostServices) -> Self {
        Self {
            backend: services.backend,
        }
    }
}

#[async_trait]
impl TunePlugin for BandcampPlugin {
    fn name(&self) -> &str {
        "bandcamp"
    }
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }
    fn description(&self) -> &str {
        "Bandcamp : recherche, découverte, tags et lecture (mp3-128)"
    }
    /// Opt-in, comme `dj` et `karaoke` : le plugin apparaît dans le
    /// gestionnaire comme disponible et non installé, et c'est bien ce qui a
    /// été demandé — « ajouter le plugin Bandcamp » (#1768).
    fn default_enabled(&self) -> bool {
        false
    }

    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        ctx.register_router(router(self.backend.clone()));
        Ok(())
    }

    async fn teardown(&mut self) -> Result<(), String> {
        Ok(())
    }

    /// Bandcamp ne réagit à aucun événement : c'est une source de navigation,
    /// pas un observateur de la lecture. Surcharge explicite en no-op pour ne
    /// pas recevoir tout le bus pour rien.
    async fn on_event(&mut self, _event: &TuneEvent) {}
}

/// Le routeur Bandcamp, `Router<()>` pour que l'hôte le monte sous
/// `/api/v1/ext/bandcamp`. Routes identiques à l'ancien `routes/bandcamp.rs`.
/// État du routeur : la base, capturée pour que le routeur reste un
/// `Router<()>` comme l'hôte l'exige, sans fuite de type `tune-server`.
#[derive(Clone)]
struct EtatBandcamp {
    backend: std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
}

pub fn router(backend: std::sync::Arc<dyn tune_core::db::backend::DbBackend>) -> Router<()> {
    let etat = EtatBandcamp { backend };
    Router::new()
        // Lot 2 — la collection d'un acheteur.
        .route("/collection/link", axum::routing::post(bc_lier_compte))
        .route("/collection", get(bc_collection))
        .with_state(etat.clone())
        .merge(routes_publiques())
}

/// Les routes qui n'ont besoin d'aucun état — lot 1, inchangées.
fn routes_publiques() -> Router<()> {
    Router::new()
        .route("/search", get(bc_search))
        .route("/discover", get(bc_discover))
        // Lecture : `?url=` d'abord, car une adresse Bandcamp contient des `/`
        // et ne tient pas dans un segment de chemin. `/album/{id}` reste et
        // renvoie vers elle.
        .route("/album", get(bc_album_par_url))
        .route("/album/{id}", get(bc_album))
        // `?url=` d'abord, comme pour l'album : une adresse Bandcamp
        // contient des `/` et ne tient pas dans un segment de chemin.
        .route("/artist", get(bc_artiste_par_url))
        .route("/artist/{id}", get(bc_artist))
        .route("/tags", get(bc_tags))
        .route("/tag/{tag}", get(bc_tag_releases))
}

/// Réponse d'erreur commune aux trois appels sortants.
///
/// Les trois faisaient le même `match` de six lignes ; le factoriser évite
/// qu'une amélioration n'atterrisse que dans l'un d'eux.
fn passerelle_en_echec(detail: String) -> axum::response::Response {
    (StatusCode::BAD_GATEWAY, Json(json!({ "error": detail }))).into_response()
}

/// Récupérer le JSON d'une réponse sortante, ou l'erreur déjà mise en forme.
///
/// Remplace l'ancien `rendre_json`, qui recopiait la réponse de Bandcamp
/// telle quelle. Ce passe-plat est ce qui a rendu la panne invisible : un
/// corps `{"error":true,"error_message":"missing key p"}` traversait le
/// plugin avec un HTTP 200 et n'échouait que dans le navigateur. Rendre le
/// `Value` à l'appelant l'oblige à regarder ce qu'il a reçu avant de le
/// servir.
async fn json_sortant(
    reponse: Result<reqwest::Response, reqwest::Error>,
) -> Result<Value, axum::response::Response> {
    match reponse {
        Ok(r) if r.status().is_success() => Ok(r.json().await.unwrap_or(json!({}))),
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            Err(passerelle_en_echec(format!("HTTP {status}: {body}")))
        }
        Err(e) => Err(passerelle_en_echec(e.to_string())),
    }
}

/// Format de pochette servi aux grilles.
///
/// Choisi en mesurant, pas en devinant. Les variantes du même visuel :
///
/// | code | pixels | poids |
/// |------|--------|-------|
/// | `_3`  | 100    | 2,8 Ko |
/// | `_7`  | 150    | 4,7 Ko |
/// | `_2`  | 350    | 12,5 Ko |
/// | `_16` | 700    | 29 Ko |
/// | `_10` | 1150   | 50 Ko |
///
/// Les vignettes font 9 rem, soit ~144 px, donc ~288 px sur un écran à double
/// densité : `_2` les couvre. `_10`, servi au départ, pesait quatre fois plus
/// pour un résultat identique à l'œil — 48 résultats de découverte faisaient
/// 2,4 Mo au lieu de 600 Ko.
const POCHETTE_GRILLE: &str = "_2";

/// URL de pochette Bandcamp pour un `art_id`.
///
/// ⚠️ Le préfixe `a` n'est pas décoratif : `.../img/4029072179_2.jpg` répond
/// **404**, `.../img/a4029072179_2.jpg` répond 200. C'est exactement le piège
/// dans lequel le champ `img` de l'API de recherche fait tomber — voir
/// [`pochette_de_resultat`].
fn pochette(art_id: Option<&Value>) -> Option<String> {
    let id = art_id?.as_i64()?;
    (id > 0).then(|| format!("https://f4.bcbits.com/img/a{id}{POCHETTE_GRILLE}.jpg"))
}

/// Pochette d'un résultat de recherche, selon son genre.
///
/// Bandcamp se contredit d'un type à l'autre, et il a fallu le mesurer :
///
/// - pour un **album ou une piste**, le champ `img` qu'il rend est **cassé** —
///   il omet le préfixe `a` de l'identifiant et renvoie un 404. L'`art_id`
///   étant fourni à côté, on reconstruit l'adresse nous-mêmes ;
/// - pour un **artiste**, il n'y a pas d'`art_id`, et le champ `img` est
///   correct tel quel (identifiant zéro-padé, sans préfixe) : on le reprend.
///
/// Sans cette distinction, la grille de recherche affichait des cadres vides —
/// des URL bien formées, présentes dans la réponse, et toutes en 404.
fn pochette_de_resultat(r: &Value) -> Option<Value> {
    match pochette(r.get("art_id")) {
        Some(u) => Some(Value::String(u)),
        None => r.get("img").cloned().filter(|v| v.is_string()),
    }
}

/// Reconstruire l'adresse publique d'un album ou d'une piste depuis les
/// `url_hints` de la découverte.
///
/// Bandcamp ne renvoie pas l'URL montée : il donne les morceaux. Un artiste
/// qui a payé un domaine propre est servi dessus, sinon c'est son
/// sous-domaine. Sans cette adresse, un résultat de découverte n'est pas
/// jouable — c'est elle que `/album?url=` attend.
fn url_depuis_hints(hints: &Value) -> Option<String> {
    let slug = hints.get("slug")?.as_str()?;
    let genre = match hints.get("item_type").and_then(|v| v.as_str()) {
        Some("t") => "track",
        _ => "album",
    };
    let racine = match hints.get("custom_domain").and_then(|v| v.as_str()) {
        Some(d) if !d.is_empty() => format!("https://{d}"),
        _ => format!("https://{}.bandcamp.com", hints.get("subdomain")?.as_str()?),
    };
    Some(format!("{racine}/{genre}/{slug}"))
}

/// Mettre les items de découverte à la forme que l'écran consomme.
///
/// La version précédente recopiait la réponse de Bandcamp telle quelle. C'est
/// ce qui a rendu la panne invisible : le corps `{"error":true}` traversait le
/// plugin avec un 200 et n'échouait que dans le navigateur. Normaliser ici
/// donne une forme stable, et un champ absent chez Bandcamp devient `null`
/// chez nous plutôt qu'un écran vide sans explication.
fn normaliser_decouverte(brut: &Value) -> Vec<Value> {
    let Some(items) = brut.get("items").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|it| {
            let url = it.get("url_hints").and_then(url_depuis_hints)?;
            Some(json!({
                "id": it.get("id"),
                "titre": it.get("primary_text"),
                "artiste": it.get("secondary_text"),
                "url": url,
                "pochette": pochette(it.get("art_id")),
                "genre": it.get("genre_text"),
                "lieu": it.get("location_text"),
                // Extrait offert par Bandcamp sur la page de découverte : de
                // quoi écouter avant d'ouvrir l'album. Même qualité que le
                // reste, donc annoncée pareil.
                "extrait": it
                    .get("featured_track")
                    .and_then(|t| t.get("file"))
                    .and_then(|f| f.get(BC_STREAM_QUALITY))
                    .and_then(|u| u.as_str()),
                "qualite": BC_STREAM_QUALITY,
                "lossless": false,
            }))
        })
        .collect()
}

/// Recherche de PISTES, pour l'hôte (« Autres versions » de l'accueil).
///
/// Même point d'entrée que [`bc_search`], restreint aux pistes
/// (`search_filter: "t"`), rendu à plat : titre, artiste, pochette, lien.
/// Une erreur réseau rend une liste vide — la section de l'accueil vaut
/// mieux incomplète que muette.
pub async fn rechercher_pistes(titre: &str) -> Vec<serde_json::Value> {
    let client = tune_core::http::client::shared();
    let Ok(resp) = client
        .post(BC_SEARCH_API)
        .json(&json!({
            "search_text": titre,
            "search_filter": "t",
            "full_page": false,
            "fan_id": null,
        }))
        .send()
        .await
    else {
        return vec![];
    };
    let Ok(brut) = resp.json::<serde_json::Value>().await else {
        return vec![];
    };
    brut.get("auto")
        .and_then(|a| a.get("results"))
        .and_then(|r| r.as_array())
        .map(|rs| {
            rs.iter()
                .filter(|r| r.get("type").and_then(|t| t.as_str()) == Some("t"))
                .map(|r| {
                    json!({
                        "title": r.get("name"),
                        "artist_name": r.get("band_name"),
                        "album_title": r.get("album_name"),
                        "cover_url": pochette_de_resultat(r),
                        "url": r.get("item_url_path").or_else(|| r.get("item_url_root")),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

/// Recherche Bandcamp.
///
/// **POST**, et non GET : `autocomplete_elastic` répond 404 à un GET, ce qui
/// remontait en 502 côté Tune. Le corps attend `search_text`, pas `q`. Sondé
/// contre l'API réelle avant réécriture — les deux erreurs venaient d'une
/// signature supposée et jamais vérifiée.
async fn bc_search(Query(q): Query<SearchQuery>) -> impl IntoResponse {
    let client = tune_core::http::client::shared();
    let resp = client
        .post(BC_SEARCH_API)
        .json(&json!({
            "search_text": q.q,
            "search_filter": "",
            "full_page": false,
            "fan_id": null,
        }))
        .send()
        .await;

    let brut = match json_sortant(resp).await {
        Ok(v) => v,
        Err(e) => return e,
    };

    let resultats = brut
        .get("auto")
        .and_then(|a| a.get("results"))
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();

    // Bandcamp mélange les trois genres dans une seule liste, distingués par
    // `type`. Les séparer ici plutôt que dans l'écran : c'est une convention
    // de leur API, pas une affaire d'affichage.
    let (mut artistes, mut albums, mut pistes) = (vec![], vec![], vec![]);
    for r in &resultats {
        let commun = json!({
            "id": r.get("id"),
            "titre": r.get("name"),
            "artiste": r.get("band_name"),
            "url": r.get("item_url_path").or_else(|| r.get("item_url_root")),
            "pochette": pochette_de_resultat(r),
            "lieu": r.get("location"),
            "album": r.get("album_name"),
        });
        match r.get("type").and_then(|t| t.as_str()) {
            Some("b") => artistes.push(commun),
            Some("a") => albums.push(commun),
            Some("t") => pistes.push(commun),
            _ => {}
        }
    }

    Json(json!({
        "q": q.q,
        "artistes": artistes,
        "albums": albums,
        "pistes": pistes,
        "qualite": BC_STREAM_QUALITY,
        "lossless": false,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct DiscoverQuery {
    #[serde(default = "default_tag")]
    tag: String,
    #[serde(default = "default_sort")]
    sort: String,
    #[serde(default)]
    page: u32,
    /// Sous-genre facultatif (`post-rock`, `math-rock`…). Absent, on explore
    /// le genre entier — c'est le comportement d'avant, inchangé.
    #[serde(default)]
    subgenre: Option<String>,
}

fn default_tag() -> String {
    "electronic".into()
}
fn default_sort() -> String {
    "top".into()
}

/// Appel commun à `/discover` et `/tag/{tag}` : les deux interrogent le même
/// point d'entrée, seule la provenance du genre change.
///
/// `get_web` est un **GET à paramètres**, pas un POST JSON. Les trois clés
/// `g` (genre), `s` (tri) et `p` (page) sont toutes obligatoires : il en
/// manquait deux, et Bandcamp répondait `{"error":true,"error_message":
/// "missing key p"}` avec un HTTP 200 — d'où une panne qu'aucun code de statut
/// ne signalait.
/// `t` porte le **sous-genre** (`post-rock`, `math-rock`…). Vide, il équivaut à
/// son absence : Bandcamp rend alors le genre entier, exactement comme avant.
async fn decouvrir(
    tag: &str,
    sort: &str,
    page: u32,
    subgenre: Option<&str>,
) -> axum::response::Response {
    let client = tune_core::http::client::shared();
    let resp = client
        .get(BC_DISCOVER_API)
        .query(&[
            ("g", tag),
            ("s", sort),
            ("p", &page.to_string()),
            ("gn", "0"),
            ("f", "all"),
            ("t", subgenre.unwrap_or_default()),
        ])
        .send()
        .await;

    let brut = match json_sortant(resp).await {
        Ok(v) => v,
        Err(e) => return e,
    };

    // Bandcamp signale ses propres refus dans le corps, avec un 200. Les
    // laisser passer serait reproduire exactement le bug corrigé ici.
    if brut.get("error").and_then(|e| e.as_bool()) == Some(true) {
        let detail = brut
            .get("error_message")
            .and_then(|m| m.as_str())
            .unwrap_or("refus sans motif");
        return passerelle_en_echec(format!("Bandcamp: {detail}"));
    }

    let items = normaliser_decouverte(&brut);
    Json(json!({
        "tag": tag,
        "sous_genre": subgenre,
        "sort": sort,
        "page": page,
        "items": items,
        "qualite": BC_STREAM_QUALITY,
        "lossless": false,
    }))
    .into_response()
}

async fn bc_discover(Query(q): Query<DiscoverQuery>) -> impl IntoResponse {
    decouvrir(&q.tag, &q.sort, q.page, q.subgenre.as_deref()).await
}

/// Qualité **unique** que Bandcamp sert sans session. Reprise telle quelle
/// dans chaque réponse de lecture.
///
/// Ce n'est pas un détail décoratif : Tune s'adresse à des gens qui règlent
/// leur chaîne au bit près. Un flux à 128 kbit/s doit être annoncé comme tel
/// partout où il apparaît, sinon quelqu'un l'écoutera en croyant juger la
/// qualité d'un disque.
const BC_STREAM_QUALITY: &str = "mp3-128";

/// Extraire le bloc `data-tralbum` d'une page album ou piste Bandcamp.
///
/// L'ancien `/album/{id}` répondait « Bandcamp has no public album API ».
/// C'est faux : la page publique embarque tout — titres, durées, et une URL de
/// flux par piste — dans un attribut HTML échappé. Aucune session requise.
///
/// Fonction pure sur le HTML, donc testable sans réseau. Rend `None` quand
/// l'attribut est absent ou illisible : Bandcamp peut changer sa page sans
/// préavis, et un `None` franc vaut mieux qu'une structure à moitié devinée.
fn extraire_tralbum(page: &str) -> Option<Value> {
    let debut = page.find("data-tralbum=\"")? + "data-tralbum=\"".len();
    let reste = &page[debut..];
    let fin = reste.find('"')?;
    let brut = &reste[..fin];
    serde_json::from_str(&deshtmliser(brut)).ok()
}

/// Déséchapper les entités HTML que Bandcamp met dans l'attribut.
///
/// `&amp;` en dernier : le faire en premier retransformerait un `&amp;quot;`
/// littéral en guillemet, et corromprait le JSON.
fn deshtmliser(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Mettre un `data-tralbum` en forme pour un client Tune.
///
/// Ne rend que les pistes réellement **jouables** : `streaming` vrai et une
/// URL présente. Une piste en précommande ou non encodée apparaîtrait sinon
/// dans la file et échouerait à la lecture.
fn album_jouable(tralbum: &Value) -> Value {
    let pistes: Vec<Value> = tralbum["trackinfo"]
        .as_array()
        .map(|v| v.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|t| {
            let url = t["file"][BC_STREAM_QUALITY].as_str()?;
            if t["streaming"].as_i64().unwrap_or(0) == 0 {
                return None;
            }
            Some(json!({
                "track_id": t["track_id"],
                "num": t["track_num"],
                "title": t["title"],
                "artist": t["artist"].as_str().or_else(|| tralbum["artist"].as_str()),
                "duration_s": t["duration"],
                "stream_url": url,
                "quality": BC_STREAM_QUALITY,
            }))
        })
        .collect();

    json!({
        "type": "album",
        "url": tralbum["url"],
        "artist": tralbum["artist"],
        "title": tralbum["current"]["title"],
        "art_id": tralbum["art_id"],
        // La pochette RÉSOLUE, et pas seulement l'`art_id` brut. La découverte
        // et la recherche la servent déjà ; l'album, lui, laissait le client
        // recomposer l'URL — c'est-à-dire réinventer le préfixe `a` dont
        // l'oubli renvoyait un 404 (#1768). Une lecture vers une zone a besoin
        // d'une pochette : c'est elle qui part dans le DIDL du renderer.
        "pochette": pochette(tralbum.get("art_id")),
        "track_count": pistes.len(),
        "tracks": pistes,
        // Annoncé à chaque réponse, pas seulement par piste : un client qui
        // n'affiche que l'album doit pouvoir le dire à l'utilisateur.
        "quality": BC_STREAM_QUALITY,
        "lossless": false,
        "quality_note": "Bandcamp ne sert que du MPG 128 kbit/s sans session \
                         d'achat. Pour la qualité d'origine, télécharger \
                         l'album acheté et le lire depuis la bibliothèque.",
    })
}

#[derive(Deserialize)]
struct AlbumQuery {
    /// URL complète de la page album ou piste Bandcamp.
    url: String,
}

/// `GET /album?url=…` — résout une page Bandcamp en album jouable.
///
/// Query et non segment de chemin : une adresse Bandcamp contient des `/` et
/// ne tient pas dans un `{id}`.
async fn bc_album_par_url(Query(q): Query<AlbumQuery>) -> impl IntoResponse {
    if !q.url.starts_with("https://") || !q.url.contains("bandcamp.com") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "url must be an https bandcamp.com address",
            })),
        )
            .into_response();
    }
    let client = tune_core::http::client::shared();
    let reponse = client
        .get(&q.url)
        // Bandcamp rend une page réduite, sans `data-tralbum`, à un client
        // qui ne s'annonce pas comme un navigateur.
        .header("User-Agent", "Mozilla/5.0 (compatible; Tune)")
        .send()
        .await;
    let page = match reponse {
        Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
        Ok(r) => return passerelle_en_echec(format!("HTTP {}", r.status())),
        Err(e) => return passerelle_en_echec(e.to_string()),
    };
    match extraire_tralbum(&page) {
        Some(t) => Json(album_jouable(&t)).into_response(),
        None => passerelle_en_echec(
            "page Bandcamp sans bloc `data-tralbum` — la page a changé, ou ce n'est pas un album"
                .into(),
        ),
    }
}

async fn bc_album(Path(id): Path<String>) -> Json<Value> {
    Json(json!({
        "id": id,
        "type": "album",
        "message": "Use /album?url=<full bandcamp album url> to resolve playable tracks.",
        "tracks": [],
    }))
}

async fn bc_artist(Path(id): Path<String>) -> Json<Value> {
    Json(json!({
        "id": id,
        "type": "artist",
        "message": "Use /artist?url=<bandcamp artist url> to list a discography.",
        "albums": [],
    }))
}

/// Extraire la discographie de la page `/music` d'un artiste.
///
/// L'ancien `/artist/{id}` répondait « Bandcamp has no public artist API ».
/// C'était faux, comme ça l'était pour les albums : la page publique porte une
/// `<ol id="music-grid">` dont chaque `<li>` donne le titre, le lien relatif et
/// la pochette. Aucune session requise.
///
/// Bandcamp y sert lui-même ses vignettes en `_2` — la même taille que
/// [`POCHETTE_GRILLE`], choisie indépendamment en mesurant. On reprend l'URL
/// telle quelle plutôt que de la reconstruire : elle est déjà juste, préfixe
/// `a` compris.
///
/// Fonction pure sur le HTML, donc testable sans réseau. Rend une liste vide
/// quand la grille est absente — Bandcamp peut changer sa page sans préavis, et
/// une liste vide franche vaut mieux qu'une structure devinée.
fn extraire_discographie(page: &str, racine: &str) -> Vec<Value> {
    let Some(debut) = page.find("id=\"music-grid\"") else {
        return Vec::new();
    };
    let grille = &page[debut..];
    let fin = grille.find("</ol>").unwrap_or(grille.len());
    let grille = &grille[..fin];

    let mut sortie = Vec::new();
    for bloc in grille.split("<li ").skip(1) {
        // Le lien est relatif (`/album/mon-disque`) : le rendre absolu ici, car
        // c'est lui que `/album?url=` recevra, et il exige une adresse complète.
        let Some(href) = attribut(bloc, "href=\"") else {
            continue;
        };
        if !href.starts_with("/album/") && !href.starts_with("/track/") {
            continue;
        }
        let titre = entre(bloc, "class=\"title\">", "</p>")
            .map(|t| deshtmliser(t.trim()))
            .unwrap_or_default();
        if titre.is_empty() {
            continue;
        }
        sortie.push(json!({
            "titre": titre,
            "url": format!("{}{}", racine.trim_end_matches('/'), href),
            "pochette": attribut(bloc, "src=\"").map(|s| deshtmliser(&s)),
            "type": if href.starts_with("/track/") { "track" } else { "album" },
        }));
    }
    sortie
}

/// Valeur d'un attribut HTML repéré par son préfixe (`href="`, `src="`).
fn attribut(bloc: &str, prefixe: &str) -> Option<String> {
    let i = bloc.find(prefixe)? + prefixe.len();
    let reste = &bloc[i..];
    let j = reste.find('"')?;
    Some(reste[..j].to_string())
}

/// Texte entre deux bornes, sans les bornes.
fn entre<'a>(bloc: &'a str, ouvre: &str, ferme: &str) -> Option<&'a str> {
    let i = bloc.find(ouvre)? + ouvre.len();
    let reste = &bloc[i..];
    let j = reste.find(ferme)?;
    Some(&reste[..j])
}

#[derive(Deserialize)]
struct ArtistQuery {
    url: String,
}

/// La discographie publique d'un artiste, à partir de l'adresse de sa page.
///
/// Comme `/album`, l'adresse passe par `?url=` : une URL Bandcamp contient des
/// `/` et ne tient pas dans un segment de chemin.
async fn bc_artiste_par_url(Query(q): Query<ArtistQuery>) -> impl IntoResponse {
    if !q.url.starts_with("https://") || !q.url.contains("bandcamp.com") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "url must be an https bandcamp.com address" })),
        )
            .into_response();
    }
    // La discographie vit sur `/music`, pas sur la racine — une racine seule
    // affiche l'album mis en avant, ce qui donnerait un seul résultat.
    let racine = q
        .url
        .split("/music")
        .next()
        .unwrap_or(&q.url)
        .trim_end_matches('/')
        .to_string();
    let cible = format!("{racine}/music");

    let client = tune_core::http::client::shared();
    let reponse = client.get(&cible).send().await;
    let page = match reponse {
        Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
        Ok(r) => return passerelle_en_echec(format!("HTTP {}", r.status())),
        Err(e) => return passerelle_en_echec(e.to_string()),
    };

    let albums = extraire_discographie(&page, &racine);
    Json(json!({
        "type": "artist",
        "url": racine,
        "albums": albums,
        "count": albums.len(),
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Genres et sous-genres — lus chez Bandcamp, plus devinés
// ---------------------------------------------------------------------------

/// La page publique `/discover` embarque son propre état initial dans un
/// attribut `data-blob`, dont `appData.initialState` porte les 27 genres ET
/// les 237 sous-genres avec leur genre parent. C'est la seule source publique
/// qui les donne — l'API `get_web` les consomme sans jamais les énumérer.
const BC_DISCOVER_PAGE: &str = "https://bandcamp.com/discover";

/// Repli hors ligne : les 27 genres réels de Bandcamp, sans leurs sous-genres.
///
/// L'ancienne liste en dur était devinée, et deux de ses entrées — `indie` et
/// `soul` — **n'existent pas** chez Bandcamp : `indie` y est un sous-genre de
/// `rock`, et le genre s'appelle `r-b-soul`. Or `get_web` **ignore en silence**
/// un genre inconnu et renvoie un flux non filtré, sans erreur : `g=indie`,
/// `g=soul` et un genre inventé rendent tous les trois exactement la même
/// liste. Ces deux entrées offraient donc à l'utilisateur un genre qui n'en
/// était pas un, et rien ne le lui disait.
///
/// Elle manquait par ailleurs `acoustic`, `alternative`, `devotional` et
/// `funk`, qui existent bel et bien.
const BC_GENRES_REPLI: &[&str] = &[
    "electronic",
    "rock",
    "metal",
    "alternative",
    "hip-hop-rap",
    "experimental",
    "punk",
    "folk",
    "pop",
    "ambient",
    "soundtrack",
    "world",
    "jazz",
    "acoustic",
    "funk",
    "r-b-soul",
    "devotional",
    "classical",
    "reggae",
    "podcasts",
    "country",
    "spoken-word",
    "comedy",
    "blues",
    "kids",
    "audiobooks",
    "latin",
];

/// Catalogue mémorisé, et l'instant où il a été lu. Les genres de Bandcamp ne
/// bougent pas d'une heure à l'autre : une lecture par jour suffit, et un
/// échec n'est pas mémorisé — on retentera au prochain appel plutôt que de
/// servir le repli pendant vingt-quatre heures.
static CATALOGUE: tokio::sync::Mutex<Option<(std::time::Instant, Value)>> =
    tokio::sync::Mutex::const_new(None);

const CATALOGUE_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Extraire `{genres, subgenres}` du `data-blob` d'une page `/discover`.
///
/// Séparé de l'appel réseau pour être testable sans Bandcamp : c'est la partie
/// qui peut se tromper.
fn catalogue_depuis_page(html: &str) -> Option<Value> {
    let debut = html.find("data-blob=\"")? + "data-blob=\"".len();
    let reste = &html[debut..];
    let fin = reste.find('"')?;
    let blob = decoder_entites(&reste[..fin]);
    let v: Value = serde_json::from_str(&blob).ok()?;
    let etat = v.get("appData")?.get("initialState")?;

    let sous = etat.get("subgenres").and_then(|s| s.as_array());
    let genres: Vec<Value> = etat
        .get("genres")?
        .as_array()?
        .iter()
        .filter_map(|g| {
            let slug = g.get("slug").and_then(|s| s.as_str())?;
            let enfants: Vec<Value> = sous
                .into_iter()
                .flatten()
                .filter(|s| s.get("parentSlug").and_then(|p| p.as_str()) == Some(slug))
                .filter_map(|s| {
                    Some(json!({
                        "slug": s.get("slug").and_then(|x| x.as_str())?,
                        "label": s.get("label").and_then(|x| x.as_str()).unwrap_or_default(),
                    }))
                })
                .collect();
            Some(json!({
                "slug": slug,
                "label": g.get("label").and_then(|s| s.as_str()).unwrap_or(slug),
                "sous_genres": enfants,
            }))
        })
        .collect();

    if genres.is_empty() {
        return None;
    }
    let tags: Vec<&str> = genres
        .iter()
        .filter_map(|g| g.get("slug").and_then(|s| s.as_str()))
        .collect();
    Some(json!({ "tags": tags, "genres": genres, "source": "bandcamp" }))
}

/// Les seules entités qui apparaissent dans un attribut HTML échappé par
/// Bandcamp. Pas de dépendance pour cinq cas.
fn decoder_entites(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn catalogue_de_repli() -> Value {
    let genres: Vec<Value> = BC_GENRES_REPLI
        .iter()
        .map(|g| json!({ "slug": g, "label": g, "sous_genres": [] }))
        .collect();
    json!({ "tags": BC_GENRES_REPLI, "genres": genres, "source": "repli" })
}

async fn bc_tags() -> Json<Value> {
    let mut cache = CATALOGUE.lock().await;
    if let Some((lu, v)) = cache.as_ref()
        && lu.elapsed() < CATALOGUE_TTL
    {
        return Json(v.clone());
    }

    let client = tune_core::http::client::shared();
    let frais = match client.get(BC_DISCOVER_PAGE).send().await {
        Ok(r) => r
            .text()
            .await
            .ok()
            .as_deref()
            .and_then(catalogue_depuis_page),
        Err(_) => None,
    };

    match frais {
        Some(v) => {
            *cache = Some((std::time::Instant::now(), v.clone()));
            Json(v)
        }
        // Échec non mémorisé : le prochain appel retentera. Mieux vaut 27
        // genres justes sans sous-genres qu'une page vide.
        None => Json(catalogue_de_repli()),
    }
}

#[derive(Deserialize)]
struct TagQuery {
    #[serde(default = "default_sort")]
    sort: String,
    #[serde(default)]
    page: u32,
    /// Sous-genre facultatif (`post-rock`, `math-rock`…), transmis tel quel.
    #[serde(default)]
    subgenre: Option<String>,
}

async fn bc_tag_releases(Path(tag): Path<String>, Query(q): Query<TagQuery>) -> impl IntoResponse {
    decouvrir(&tag, &q.sort, q.page, q.subgenre.as_deref()).await
}

// ---------------------------------------------------------------------------
// Lot 2 — la collection d'un acheteur
// ---------------------------------------------------------------------------

const BC_COLLECTION_API: &str = "https://bandcamp.com/api/fancollection/1/collection_items";
/// Jeton de départ : « tout ce qui est plus ancien que jamais », c'est-à-dire
/// la page la plus récente. Convention de Bandcamp, pas la nôtre.
const BC_JETON_DEBUT: &str = "9999999999::a::";
const CLE_PSEUDO: &str = "bandcamp_username";
const CLE_FAN_ID: &str = "bandcamp_fan_id";

/// Extraire le `fan_id` d'une page de profil Bandcamp publique.
///
/// La page ne l'expose que sous forme **échappée** dans un attribut HTML
/// (`fan_id&quot;:897100`), jamais en JSON nu — vérifié sur une page réelle.
/// Et elle contient plusieurs occurrences, dont au moins une SANS chiffres :
/// s'arrêter à la première rendrait `None` sur un profil parfaitement valide.
/// On parcourt donc jusqu'à en trouver une qui porte un nombre.
fn extraire_fan_id(page: &str) -> Option<i64> {
    for motif in ["fan_id&quot;:", "\"fan_id\":"] {
        let mut reste = page;
        while let Some(i) = reste.find(motif) {
            reste = &reste[i + motif.len()..];
            let chiffres: String = reste.chars().take_while(|c| c.is_ascii_digit()).collect();
            if !chiffres.is_empty() {
                if let Ok(n) = chiffres.parse::<i64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

#[derive(Deserialize)]
struct LierBody {
    /// Pseudo Bandcamp — PAS un identifiant de connexion.
    username: String,
}

/// `POST /collection/link` — mémoriser un pseudo et résoudre son `fan_id`.
///
/// Aucun mot de passe, aucun cookie : le pseudo suffit, parce que la page de
/// profil est publique. C'est délibéré et non une limitation subie — voir la
/// note de portée en tête de module.
async fn bc_lier_compte(
    axum::extract::State(etat): axum::extract::State<EtatBandcamp>,
    Json(body): Json<LierBody>,
) -> impl IntoResponse {
    let pseudo = body.username.trim().to_string();
    if pseudo.is_empty() || pseudo.contains('/') {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "username invalide"})),
        )
            .into_response();
    }
    let client = tune_core::http::client::shared();
    let url = format!("https://bandcamp.com/{pseudo}");
    let reponse = client
        .get(&url)
        .header("User-Agent", "Mozilla/5.0 (compatible; Tune)")
        .send()
        .await;
    let page = match reponse {
        Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
        Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("aucun profil Bandcamp public pour « {pseudo} »")})),
            )
                .into_response();
        }
        Ok(r) => return passerelle_en_echec(format!("HTTP {}", r.status())),
        Err(e) => return passerelle_en_echec(e.to_string()),
    };
    let Some(fan_id) = extraire_fan_id(&page) else {
        return passerelle_en_echec(
            "profil trouvé mais sans fan_id — la page a changé, ou le profil est privé".into(),
        );
    };
    let reglages = tune_core::db::settings_repo::SettingsRepo::with_backend(etat.backend.clone());
    let _ = reglages.set(CLE_PSEUDO, &pseudo);
    let _ = reglages.set(CLE_FAN_ID, &fan_id.to_string());
    Json(json!({ "username": pseudo, "fan_id": fan_id, "linked": true })).into_response()
}

#[derive(Deserialize)]
struct CollectionQuery {
    /// Jeton de pagination rendu par l'appel précédent (`last_token`).
    older_than_token: Option<String>,
    #[serde(default = "default_count")]
    count: u32,
}

fn default_count() -> u32 {
    50
}

/// `GET /collection` — la collection du compte lié, page par page.
///
/// Bandcamp pagine par curseur (`older_than_token`), pas par numéro : le
/// client réémet le `last_token` de la réponse précédente jusqu'à
/// `more_available: false`. La collection d'un acheteur de longue date dépasse
/// largement une page.
async fn bc_collection(
    axum::extract::State(etat): axum::extract::State<EtatBandcamp>,
    Query(q): Query<CollectionQuery>,
) -> impl IntoResponse {
    let reglages = tune_core::db::settings_repo::SettingsRepo::with_backend(etat.backend.clone());
    let fan_id = reglages
        .get(CLE_FAN_ID)
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok());
    let Some(fan_id) = fan_id else {
        return (
            StatusCode::PRECONDITION_REQUIRED,
            Json(json!({
                "error": "aucun compte Bandcamp lié",
                "detail": "POST /collection/link avec {\"username\": \"…\"} d'abord.",
            })),
        )
            .into_response();
    };
    let jeton = q
        .older_than_token
        .unwrap_or_else(|| BC_JETON_DEBUT.to_string());
    let client = tune_core::http::client::shared();
    let reponse = client
        .post(BC_COLLECTION_API)
        .json(&json!({
            "fan_id": fan_id,
            "older_than_token": jeton,
            "count": q.count.clamp(1, 100),
        }))
        .send()
        .await;
    let brut: Value = match reponse {
        Ok(r) if r.status().is_success() => r.json().await.unwrap_or(json!({})),
        Ok(r) => return passerelle_en_echec(format!("HTTP {}", r.status())),
        Err(e) => return passerelle_en_echec(e.to_string()),
    };
    Json(collection_mise_en_forme(&brut, fan_id)).into_response()
}

/// Mettre une page de collection en forme pour un client Tune.
///
/// Ne garde que ce qui sert au rapprochement avec la bibliothèque locale
/// (lot 3) : qui, quoi, et de quel type. Le reste de la charge Bandcamp —
/// prix, dates d'achat, compteurs — n'a pas à traverser l'API de Tune.
fn collection_mise_en_forme(brut: &Value, fan_id: i64) -> Value {
    let articles: Vec<Value> = brut["items"]
        .as_array()
        .map(|v| v.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|it| {
            json!({
                "artist": it["band_name"],
                "title": it["item_title"],
                "type": it["item_type"],
                "url": it["item_url"],
                "art_id": it["item_art_id"],
            })
        })
        .collect();
    json!({
        "fan_id": fan_id,
        "count": articles.len(),
        "items": articles,
        // Curseur à réémettre tel quel pour la page suivante.
        "more_available": brut["more_available"].as_bool().unwrap_or(false),
        "last_token": brut["last_token"],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un plugin de test avec une base en mémoire.
    ///
    /// Le lot 2 lui a donné un `HostServices` ; les tests du lot 1 le
    /// construisaient sans argument et ne compilaient plus. `cargo check` ne
    /// compile pas le code de test — seul `cargo test` l'a montré.
    fn plugin_de_test() -> BandcampPlugin {
        let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
        BandcampPlugin::new(HostServices {
            backend: std::sync::Arc::new(db),
        })
    }

    /// Un item de découverte tel que Bandcamp le rend, réduit aux champs lus.
    fn item_decouverte() -> Value {
        json!({
            "id": 3179309048_i64,
            "primary_text": "Kind Of Grunge",
            "secondary_text": "Ulysses Owens Jr.",
            "art_id": 4214215264_i64,
            "genre_text": "jazz",
            "location_text": "New York, New York",
            "url_hints": {
                "subdomain": "ulyssesowensjr",
                "custom_domain": null,
                "slug": "kind-of-grunge",
                "item_type": "a"
            },
            "featured_track": { "file": { "mp3-128": "https://t4.bcbits.com/stream/x" } }
        })
    }

    #[test]
    fn l_adresse_d_un_resultat_se_reconstruit_depuis_les_hints() {
        // Bandcamp ne rend pas l'URL montée : sans cette reconstruction, un
        // résultat de découverte n'est pas jouable — `/album?url=` l'attend.
        let hints = item_decouverte();
        let hints = hints.get("url_hints").unwrap();
        assert_eq!(
            url_depuis_hints(hints).unwrap(),
            "https://ulyssesowensjr.bandcamp.com/album/kind-of-grunge"
        );
    }

    #[test]
    fn un_domaine_propre_l_emporte_sur_le_sous_domaine() {
        // Un artiste qui a payé un domaine doit être servi dessus.
        let hints = json!({
            "subdomain": "monlabel",
            "custom_domain": "disques.example",
            "slug": "opus-1",
            "item_type": "a"
        });
        assert_eq!(
            url_depuis_hints(&hints).unwrap(),
            "https://disques.example/album/opus-1"
        );
    }

    #[test]
    fn une_piste_ne_se_range_pas_sous_album() {
        let hints = json!({
            "subdomain": "x", "custom_domain": null,
            "slug": "y", "item_type": "t"
        });
        assert_eq!(
            url_depuis_hints(&hints).unwrap(),
            "https://x.bandcamp.com/track/y"
        );
    }

    #[test]
    fn la_decouverte_normalisee_porte_de_quoi_jouer_et_afficher() {
        let brut = json!({ "items": [item_decouverte()] });
        let items = normaliser_decouverte(&brut);
        assert_eq!(items.len(), 1);
        let a = &items[0];
        assert_eq!(a["titre"], "Kind Of Grunge");
        assert_eq!(a["artiste"], "Ulysses Owens Jr.");
        assert_eq!(
            a["url"],
            "https://ulyssesowensjr.bandcamp.com/album/kind-of-grunge"
        );
        // `_2` (350 px, 12,5 Ko) et non `_10` (1150 px, 50 Ko) : les vignettes
        // font 9 rem, la grande taille pesait quatre fois plus pour un résultat
        // identique à l'œil. Changement délibéré — ce test l'a bien attrapé.
        assert_eq!(a["pochette"], "https://f4.bcbits.com/img/a4214215264_2.jpg");
        assert_eq!(a["extrait"], "https://t4.bcbits.com/stream/x");
        // La qualité voyage avec chaque item : #1768 exige qu'un flux à
        // 128 kbit/s soit annoncé partout où il apparaît.
        assert_eq!(a["qualite"], BC_STREAM_QUALITY);
        assert_eq!(a["lossless"], false);
    }

    #[test]
    fn un_item_sans_adresse_est_ecarte_plutot_que_rendu_injouable() {
        // Mieux vaut un résultat de moins qu'une vignette qui ne s'ouvre pas.
        let brut = json!({ "items": [ { "primary_text": "Sans hints" } ] });
        assert!(normaliser_decouverte(&brut).is_empty());
    }

    #[test]
    fn un_refus_de_bandcamp_ne_devient_pas_une_liste_vide_silencieuse() {
        // La panne corrigée ici : Bandcamp répond `{"error":true}` avec un
        // HTTP 200. `decouvrir` le convertit en 502 ; ce test verrouille le
        // fait que le corps de refus ne porte aucun item à afficher.
        let refus = json!({ "error": true, "error_message": "missing key p" });
        assert!(normaliser_decouverte(&refus).is_empty());
        assert_eq!(refus["error"], true);
    }

    #[test]
    fn une_pochette_sans_art_id_ne_s_invente_pas() {
        assert!(pochette(None).is_none());
        assert!(pochette(Some(&json!(null))).is_none());
        assert!(pochette(Some(&json!(0))).is_none());
    }

    #[test]
    fn la_pochette_porte_le_prefixe_a_sans_lequel_bandcamp_repond_404() {
        // Mesuré : `.../img/4029072179_2.jpg` → 404, `.../a4029072179_2.jpg` → 200.
        let u = pochette(Some(&json!(4029072179_i64))).unwrap();
        assert_eq!(u, "https://f4.bcbits.com/img/a4029072179_2.jpg");
        assert!(u.contains("/img/a"), "le prefixe `a` est obligatoire");
    }

    #[test]
    fn un_album_de_recherche_ignore_le_img_casse_de_bandcamp() {
        // Le champ `img` que Bandcamp rend pour un album OMET le prefixe `a` :
        // l'URL est bien formee, presente, et tombe en 404. C'est ce qui
        // affichait des cadres vides dans la grille de recherche.
        let album = json!({
            "type": "a",
            "art_id": 4029072179_i64,
            "img": "https://f4.bcbits.com/img/4029072179_3.jpg",
        });
        assert_eq!(
            pochette_de_resultat(&album).unwrap(),
            json!("https://f4.bcbits.com/img/a4029072179_2.jpg")
        );
    }

    #[test]
    fn un_artiste_garde_son_img_qui_lui_est_correct() {
        // Un artiste n'a pas d'`art_id`, et son `img` est juste tel quel
        // (identifiant zero-pade, sans prefixe) — verifie en 200.
        let artiste = json!({
            "type": "b",
            "img": "https://f4.bcbits.com/img/0035340864_23.jpg",
        });
        assert_eq!(
            pochette_de_resultat(&artiste).unwrap(),
            json!("https://f4.bcbits.com/img/0035340864_23.jpg")
        );
        assert!(pochette_de_resultat(&json!({ "type": "b" })).is_none());
    }

    /// Une `music-grid` reduite aux attributs lus, telle que Bandcamp la rend.
    const GRILLE: &str = r#"<ol id="music-grid" class="editable-grid music-grid columns-3 public">
      <li data-item-id="album-1297872477" data-band-id="3966070289" class="music-grid-item square">
        <a href="/album/prime-example">
          <div class="art"><img src="https://f4.bcbits.com/img/a0808959197_2.jpg" alt="" /></div>
          <p class="title"> Prime Example &amp; co </p>
        </a>
      </li>
      <li data-item-id="track-42" class="music-grid-item">
        <a href="/track/un-titre">
          <div class="art"><img src="https://f4.bcbits.com/img/a1_2.jpg" alt="" /></div>
          <p class="title">Un titre</p>
        </a>
      </li>
      <li class="music-grid-item"><a href="/community"><p class="title">Pas un disque</p></a></li>
    </ol>"#;

    #[test]
    fn la_discographie_se_lit_sur_la_page_music() {
        // L'ancien `/artist/{id}` repondait « Bandcamp has no public artist
        // API ». Faux, comme ca l'etait pour les albums.
        let d = extraire_discographie(GRILLE, "https://ulysseshellier.bandcamp.com/");
        assert_eq!(d.len(), 2, "le lien /community n'est pas un disque");
        assert_eq!(d[0]["titre"], "Prime Example & co");
        assert_eq!(
            d[0]["url"],
            "https://ulysseshellier.bandcamp.com/album/prime-example"
        );
        assert_eq!(
            d[0]["pochette"],
            "https://f4.bcbits.com/img/a0808959197_2.jpg"
        );
        assert_eq!(d[0]["type"], "album");
        // Une piste isolee est marquee comme telle : `/album?url=` la resout
        // aussi, mais l'ecran doit pouvoir la presenter differemment.
        assert_eq!(d[1]["type"], "track");
    }

    #[test]
    fn une_url_relative_devient_absolue_sans_double_barre() {
        // La racine arrive parfois avec une barre finale, parfois sans : les
        // deux doivent donner la meme adresse, car `/album?url=` la rejettera
        // si elle est malformee.
        let avec = extraire_discographie(GRILLE, "https://x.bandcamp.com/");
        let sans = extraire_discographie(GRILLE, "https://x.bandcamp.com");
        assert_eq!(avec[0]["url"], sans[0]["url"]);
        assert_eq!(avec[0]["url"], "https://x.bandcamp.com/album/prime-example");
    }

    #[test]
    fn une_page_sans_grille_rend_une_liste_vide_franche() {
        assert!(extraire_discographie("<html>rien</html>", "https://x.bandcamp.com").is_empty());
    }

    #[test]
    fn le_prefixe_de_montage_vient_du_nom() {
        // L'hôte dérive `/api/v1/ext/{name}` de `name()`. Le renommer
        // déplacerait silencieusement toutes les routes du plugin.
        let p = plugin_de_test();
        assert_eq!(p.name(), "bandcamp");
    }

    #[test]
    fn le_plugin_reste_opt_in() {
        // « Ajouter le plugin Bandcamp » (#1768) : il doit apparaître comme
        // disponible et non installé, pas tourner d'office.
        assert!(!plugin_de_test().default_enabled());
    }

    #[test]
    fn les_valeurs_par_defaut_de_decouverte_sont_preservees() {
        // Déplacement à l'identique : ces deux valeurs étaient celles du
        // `routes/bandcamp.rs` d'origine, et un client existant en dépend.
        assert_eq!(default_tag(), "electronic");
        assert_eq!(default_sort(), "top");
    }

    #[test]
    fn toutes_les_routes_dorigine_sont_reprises() {
        // Le routeur doit se construire sans panique : une route mal formée
        // (accolades dépareillées dans un motif de chemin) panique à la
        // construction, pas à la compilation.
        let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
        let _ = router(std::sync::Arc::new(db));
    }

    /// Fragment de page RÉEL, réduit : l'attribut tel que Bandcamp l'émet,
    /// entités HTML comprises. Fabriquer du JSON propre ici ne testerait pas
    /// le déséchappement, qui est justement la partie fragile.
    const PAGE: &str = r#"<html><body>
      <script data-tralbum="{&quot;url&quot;:&quot;https://x.bandcamp.com/album/y&quot;,&quot;artist&quot;:&quot;Andrew Huang&quot;,&quot;art_id&quot;:4034627626,&quot;current&quot;:{&quot;title&quot;:&quot;CXM 1978&quot;},&quot;trackinfo&quot;:[{&quot;track_id&quot;:1,&quot;track_num&quot;:1,&quot;title&quot;:&quot;CXM 1978&quot;,&quot;artist&quot;:null,&quot;duration&quot;:138.772,&quot;streaming&quot;:1,&quot;file&quot;:{&quot;mp3-128&quot;:&quot;https://bandcamp.com/stream_redirect?enc=mp3-128&amp;track_id=1&quot;}},{&quot;track_id&quot;:2,&quot;track_num&quot;:2,&quot;title&quot;:&quot;Precommande&quot;,&quot;duration&quot;:10.0,&quot;streaming&quot;:0,&quot;file&quot;:{&quot;mp3-128&quot;:&quot;https://x/2&quot;}},{&quot;track_id&quot;:3,&quot;track_num&quot;:3,&quot;title&quot;:&quot;Sans encodage&quot;,&quot;duration&quot;:9.0,&quot;streaming&quot;:1,&quot;file&quot;:{}}]}"></script>
    </body></html>"#;

    #[test]
    fn extrait_le_bloc_tralbum_dune_page_reelle() {
        // L'ancien talon affirmait « Bandcamp has no public album API ».
        // C'est faux : la page publique porte tout (#1768).
        let t = extraire_tralbum(PAGE).expect("bloc data-tralbum absent");
        assert_eq!(t["artist"], "Andrew Huang");
        assert_eq!(t["current"]["title"], "CXM 1978");
        assert_eq!(t["trackinfo"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn ne_garde_que_les_pistes_reellement_jouables() {
        // Une precommande (`streaming: 0`) ou une piste sans encodage
        // atterrirait dans la file et echouerait a la lecture.
        let t = extraire_tralbum(PAGE).unwrap();
        let a = album_jouable(&t);
        assert_eq!(a["track_count"], 1);
        let pistes = a["tracks"].as_array().unwrap();
        assert_eq!(pistes[0]["title"], "CXM 1978");
        // L'artiste de piste est nul : on retombe sur celui de l'album.
        assert_eq!(pistes[0]["artist"], "Andrew Huang");
        assert_eq!(pistes[0]["duration_s"], 138.772);
    }

    #[test]
    fn le_deshtmlisage_restitue_une_url_avec_esperluette() {
        // `&amp;` doit redevenir `&`, sinon l'URL de flux est cassee. Et il
        // doit passer en DERNIER : l'inverse transformerait un `&amp;quot;`
        // litteral en guillemet et corromprait le JSON.
        let t = extraire_tralbum(PAGE).unwrap();
        let a = album_jouable(&t);
        let u = a["tracks"][0]["stream_url"].as_str().unwrap();
        assert!(
            u.contains("enc=mp3-128&track_id=1"),
            "URL mal deshtmlisee : {u}"
        );
        assert!(!u.contains("&amp;"));
    }

    #[test]
    fn la_qualite_est_annoncee_a_tous_les_niveaux() {
        // Decision produit : Bertrand a choisi d'exposer la lecture malgre le
        // mp3-128. La contrepartie est de l'annoncer partout — album ET piste
        // — pour que personne ne juge un disque sur 128 kbit/s (#1768).
        let t = extraire_tralbum(PAGE).unwrap();
        let a = album_jouable(&t);
        assert_eq!(a["quality"], "mp3-128");
        assert_eq!(a["lossless"], false);
        assert!(a["quality_note"].as_str().unwrap().contains("128"));
        assert_eq!(a["tracks"][0]["quality"], "mp3-128");
    }

    #[test]
    fn lalbum_sert_la_pochette_resolue_pas_seulement_lart_id() {
        // Une lecture vers une zone envoie la pochette dans le DIDL du
        // renderer. La laisser recomposer par le client, c'est lui faire
        // réinventer le préfixe `a` — celui dont l'oubli renvoyait un 404.
        let t = extraire_tralbum(PAGE).unwrap();
        let a = album_jouable(&t);
        assert_eq!(a["pochette"], "https://f4.bcbits.com/img/a4034627626_2.jpg");
    }

    #[test]
    fn une_page_sans_bloc_rend_none_plutot_que_de_deviner() {
        // Bandcamp peut changer sa page sans preavis. Un None franc vaut mieux
        // qu'une structure a moitie devinee.
        assert!(extraire_tralbum("<html><body>rien ici</body></html>").is_none());
        assert!(extraire_tralbum("data-tralbum=\"{ceci n'est pas du json}\"").is_none());
    }

    /// Un extrait fidèle du `data-blob` de `bandcamp.com/discover` : deux
    /// genres, trois sous-genres, dont un rattaché à un autre parent — pour
    /// vérifier que le rattachement se fait par `parentSlug` et pas par ordre.
    const BLOB: &str = r#"<div id="DiscoverApp" data-blob="{&quot;appData&quot;:{&quot;initialState&quot;:{&quot;genres&quot;:[{&quot;id&quot;:23,&quot;label&quot;:&quot;rock&quot;,&quot;slug&quot;:&quot;rock&quot;},{&quot;id&quot;:10,&quot;label&quot;:&quot;electronic&quot;,&quot;slug&quot;:&quot;electronic&quot;}],&quot;subgenres&quot;:[{&quot;id&quot;:1,&quot;label&quot;:&quot;techno&quot;,&quot;slug&quot;:&quot;techno&quot;,&quot;parentSlug&quot;:&quot;electronic&quot;},{&quot;id&quot;:2,&quot;label&quot;:&quot;post-rock&quot;,&quot;slug&quot;:&quot;post-rock&quot;,&quot;parentSlug&quot;:&quot;rock&quot;},{&quot;id&quot;:3,&quot;label&quot;:&quot;math rock&quot;,&quot;slug&quot;:&quot;math-rock&quot;,&quot;parentSlug&quot;:&quot;rock&quot;}]}}}"></div>"#;

    #[test]
    fn le_catalogue_rattache_chaque_sous_genre_a_son_parent() {
        let c = catalogue_depuis_page(BLOB).unwrap();
        assert_eq!(c["tags"], json!(["rock", "electronic"]));
        let rock = &c["genres"][0];
        assert_eq!(rock["slug"], "rock");
        assert_eq!(rock["sous_genres"][0]["slug"], "post-rock");
        assert_eq!(rock["sous_genres"][1]["slug"], "math-rock");
        assert_eq!(rock["sous_genres"][1]["label"], "math rock");
        assert_eq!(rock["sous_genres"].as_array().unwrap().len(), 2);
        assert_eq!(c["genres"][1]["sous_genres"][0]["slug"], "techno");
        assert_eq!(c["source"], "bandcamp");
    }

    #[test]
    fn une_page_sans_blob_exploitable_rend_none_plutot_que_de_deviner() {
        // Même règle que pour `extraire_tralbum` : Bandcamp peut changer sa
        // page sans préavis, et un None franc bascule sur le repli au lieu de
        // servir une liste à moitié devinée.
        assert!(catalogue_depuis_page("<html>rien</html>").is_none());
        assert!(catalogue_depuis_page("data-blob=\"{pas du json}\"").is_none());
        // Blob valide mais sans genres : une liste vide n'est pas un catalogue.
        assert!(
            catalogue_depuis_page(
                "data-blob=\"{&quot;appData&quot;:{&quot;initialState&quot;:{&quot;genres&quot;:[]}}}\""
            )
            .is_none()
        );
    }

    #[test]
    fn le_catalogue_tient_sans_sous_genres() {
        // `subgenres` absent : les genres restent servis, chacun avec une
        // liste vide. C'est exactement ce que rendait l'ancienne route.
        let sans = "data-blob=\"{&quot;appData&quot;:{&quot;initialState&quot;:{&quot;genres&quot;:[{&quot;label&quot;:&quot;jazz&quot;,&quot;slug&quot;:&quot;jazz&quot;}]}}}\"";
        let c = catalogue_depuis_page(sans).unwrap();
        assert_eq!(c["genres"][0]["slug"], "jazz");
        assert_eq!(c["genres"][0]["sous_genres"], json!([]));
    }

    /// L'ancienne liste en dur proposait `indie` et `soul`, qui ne sont pas des
    /// genres Bandcamp — et `get_web` ignore un genre inconnu EN SILENCE, en
    /// rendant un flux non filtré. Vérifié à la main contre l'API : `g=indie`,
    /// `g=soul` et un genre inventé rendent la même liste, au même ordre.
    #[test]
    fn le_repli_ne_propose_aucun_genre_que_bandcamp_ignore() {
        let c = catalogue_de_repli();
        let tags: Vec<&str> = c["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap())
            .collect();
        assert!(!tags.contains(&"indie"), "indie est un sous-genre de rock");
        assert!(!tags.contains(&"soul"), "le genre s'appelle r-b-soul");
        assert!(tags.contains(&"r-b-soul"));
        // Et les quatre genres réels que l'ancienne liste oubliait.
        for g in ["acoustic", "alternative", "devotional", "funk"] {
            assert!(tags.contains(&g), "{g} manque au repli");
        }
        assert_eq!(tags.len(), 27);
        assert_eq!(c["source"], "repli");
    }

    #[test]
    fn le_repli_annonce_chaque_genre_sans_sous_genre() {
        // Hors ligne on ne connaît pas les sous-genres : les annoncer vides
        // laisse le client masquer la rangée, plutôt que de lui faire croire
        // à un catalogue complet.
        let c = catalogue_de_repli();
        for g in c["genres"].as_array().unwrap() {
            assert_eq!(g["sous_genres"], json!([]));
        }
    }

    #[test]
    fn les_entites_html_de_bandcamp_sont_decodees() {
        assert_eq!(decoder_entites("&quot;a&quot;"), "\"a\"");
        assert_eq!(decoder_entites("d&#39;ici"), "d'ici");
        assert_eq!(decoder_entites("a &lt;b&gt; c"), "a <b> c");
    }
}

/// L'adresse de la page d'un artiste Bandcamp, cherchée par son nom.
///
/// Rend `None` quand la recherche ne trouve rien, ou quand le premier résultat
/// n'est pas un artiste (`type == "b"`). On ne prend QUE le premier : au-delà,
/// on choisirait un homonyme, et une nouveauté attribuée au mauvais artiste est
/// pire qu'une nouveauté manquée.
pub async fn adresse_artiste(nom: &str) -> Option<String> {
    let client = tune_core::http::client::shared();
    let brut: Value = client
        .post(BC_SEARCH_API)
        .json(&json!({
            "search_text": nom,
            // Ne demander QUE des artistes : sans ce filtre, un album portant
            // le nom cherché passerait devant et on suivrait son adresse.
            "search_filter": "b",
            "full_page": false,
            "fan_id": null,
        }))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;

    let premier = brut
        .get("auto")?
        .get("results")?
        .as_array()?
        .iter()
        .find(|r| r.get("type").and_then(|t| t.as_str()) == Some("b"))?;

    // Le nom doit correspondre : la recherche Bandcamp est floue, et rend
    // volontiers un artiste voisin quand le nom exact n'existe pas.
    let trouve = premier.get("name").and_then(|n| n.as_str())?;
    if !trouve.eq_ignore_ascii_case(nom.trim()) {
        return None;
    }

    let url = premier
        .get("item_url_path")
        .or_else(|| premier.get("item_url_root"))
        .and_then(|u| u.as_str())?;
    Some(url.trim_end_matches('/').to_string())
}

/// Les parutions publiques d'un artiste, la plus récente d'abord.
///
/// Chaque entrée porte `titre`, `url`, `pochette` et `type`. C'est l'**url**
/// qui sert d'identité à la veille — un titre peut changer, l'adresse non —
/// mais le titre et la pochette sont nécessaires pour montrer la nouveauté à
/// l'écran, d'où le rendu complet plutôt qu'une simple liste d'adresses.
///
/// C'est la matière de la veille : Bandcamp ne datant pas sa discographie, ce
/// sont ces adresses qu'on compare d'un passage à l'autre
/// (`tune_core::bandcamp_veille`).
pub async fn parutions_discographie(racine: &str) -> Vec<Value> {
    let client = tune_core::http::client::shared();
    let cible = format!("{}/music", racine.trim_end_matches('/'));
    let Ok(reponse) = client.get(&cible).send().await else {
        return Vec::new();
    };
    if !reponse.status().is_success() {
        return Vec::new();
    }
    let Ok(page) = reponse.text().await else {
        return Vec::new();
    };
    extraire_discographie(&page, racine)
}
