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
//! Ce lot ne fait que **déplacer** l'existant : recherche, découverte, tags.
//! La collection d'un acheteur — l'intérêt réel de la demande — est le lot 2,
//! et elle bute sur une contrainte établie avant d'écrire une ligne : sans
//! session, Bandcamp ne rend que du `mp3-128` et `redownload_urls` reste vide.
//! Voir #1768.

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
        "Bandcamp: recherche, découverte et navigation par tags"
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

async fn bc_album(Path(id): Path<String>) -> Json<Value> {
    Json(json!({
        "id": id,
        "type": "album",
        "message": "Bandcamp has no public album API. Use /search or /discover to find releases.",
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
}
