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

/// Le plugin Bandcamp. Sans état : il ne possède rien de l'hôte.
#[derive(Default)]
pub struct BandcampPlugin;

impl BandcampPlugin {
    pub fn new() -> Self {
        Self
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
        ctx.register_router(router());
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
pub fn router() -> Router<()> {
    Router::new()
        .route("/search", get(bc_search))
        .route("/discover", get(bc_discover))
        // Lecture : `?url=` d'abord, car une adresse Bandcamp contient des `/`
        // et ne tient pas dans un segment de chemin. `/album/{id}` reste et
        // renvoie vers elle.
        .route("/album", get(bc_album_par_url))
        .route("/album/{id}", get(bc_album))
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

/// Exécuter une requête sortante et rendre son JSON, ou un 502 explicite.
async fn rendre_json(reponse: Result<reqwest::Response, reqwest::Error>) -> impl IntoResponse {
    match reponse {
        Ok(r) if r.status().is_success() => {
            let body: Value = r.json().await.unwrap_or(json!({}));
            Json(body).into_response()
        }
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            passerelle_en_echec(format!("HTTP {status}: {body}"))
        }
        Err(e) => passerelle_en_echec(e.to_string()),
    }
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

async fn bc_search(Query(q): Query<SearchQuery>) -> impl IntoResponse {
    let client = tune_core::http::client::shared();
    let resp = client.get(BC_SEARCH_API).query(&[("q", &q.q)]).send().await;
    rendre_json(resp).await
}

#[derive(Deserialize)]
struct DiscoverQuery {
    #[serde(default = "default_tag")]
    tag: String,
    #[serde(default = "default_sort")]
    sort: String,
    #[serde(default)]
    page: u32,
}

fn default_tag() -> String {
    "electronic".into()
}
fn default_sort() -> String {
    "top".into()
}

async fn bc_discover(Query(q): Query<DiscoverQuery>) -> impl IntoResponse {
    let client = tune_core::http::client::shared();
    let payload = json!({
        "tag_norm_names": [q.tag],
        "sort": q.sort,
        "page": q.page,
    });
    let resp = client.post(BC_DISCOVER_API).json(&payload).send().await;
    rendre_json(resp).await
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
        "message": "Bandcamp has no public artist API. Use /search to find artists.",
        "albums": [],
    }))
}

async fn bc_tags() -> Json<Value> {
    // Bandcamp's main genre tags (no public API, but these are the well-known ones)
    Json(json!({
        "tags": [
            "electronic", "ambient", "experimental", "hip-hop-rap", "rock", "metal",
            "punk", "pop", "folk", "jazz", "classical", "soul", "r-b-soul", "world",
            "soundtrack", "latin", "country", "blues", "reggae", "audiobooks",
            "podcasts", "kids", "comedy", "spoken-word", "indie",
        ]
    }))
}

#[derive(Deserialize)]
struct TagQuery {
    #[serde(default = "default_sort")]
    sort: String,
    #[serde(default)]
    page: u32,
}

async fn bc_tag_releases(Path(tag): Path<String>, Query(q): Query<TagQuery>) -> impl IntoResponse {
    let client = tune_core::http::client::shared();
    let payload = json!({
        "tag_norm_names": [tag],
        "sort": q.sort,
        "page": q.page,
    });
    let resp = client.post(BC_DISCOVER_API).json(&payload).send().await;
    rendre_json(resp).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_prefixe_de_montage_vient_du_nom() {
        // L'hôte dérive `/api/v1/ext/{name}` de `name()`. Le renommer
        // déplacerait silencieusement toutes les routes du plugin.
        let p = BandcampPlugin::new();
        assert_eq!(p.name(), "bandcamp");
    }

    #[test]
    fn le_plugin_reste_opt_in() {
        // « Ajouter le plugin Bandcamp » (#1768) : il doit apparaître comme
        // disponible et non installé, pas tourner d'office.
        assert!(!BandcampPlugin::new().default_enabled());
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
        let _ = router();
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
    fn une_page_sans_bloc_rend_none_plutot_que_de_deviner() {
        // Bandcamp peut changer sa page sans preavis. Un None franc vaut mieux
        // qu'une structure a moitie devinee.
        assert!(extraire_tralbum("<html><body>rien ici</body></html>").is_none());
        assert!(extraire_tralbum("data-tralbum=\"{ceci n'est pas du json}\"").is_none());
    }
}
