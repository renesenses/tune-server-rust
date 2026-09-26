//! #5103 — un service CONNECTÉ mais DÉSACTIVÉ ne doit plus apparaître.
//!
//! FabienM, fil 1957 (v0.9.165) : YouTube « Connecté, mais désactivé » dans
//! les Réglages, case Actif décochée, et pourtant l'artiste trouvé par la
//! recherche porte le badge YT. `federated_search` ne testait que
//! `auth_status().authenticated`, jamais `enabled()` — alors que les pages
//! artiste, les versions et la reprise des favoris testaient les deux.
//!
//! La règle vit désormais dans `StreamingService::utilisable`. Ces témoins
//! attaquent les ROUTES MONTÉES par `crate::routes::router` : c'est le
//! branchement qu'il faut prouver, pas la conjonction de deux booléens.
//!
//! Deux services simulés, sans réseau : `qobuz`, activé et connecté (le
//! témoin qui doit rester), et `youtube`, connecté mais désactivé (la
//! configuration de la capture).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::TuneError;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

struct ServiceTemoin {
    nom: &'static str,
    actif: bool,
    connecte: bool,
}

#[async_trait::async_trait]
impl StreamingService for ServiceTemoin {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        self.nom
    }
    fn enabled(&self) -> bool {
        self.actif
    }
    fn set_enabled(&mut self, enabled: bool) {
        self.actif = enabled;
    }
    async fn authenticate(&mut self, _c: &Value) -> Result<AuthStatus, TuneError> {
        Ok(self.auth_status().await)
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: self.connecte,
            ..Default::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        Ok(SearchResults {
            tracks: vec![],
            albums: vec![],
            artists: vec![StreamArtist {
                id: format!("{}-neil", self.nom),
                name: "Neil Young".into(),
                image_path: None,
                bio: None,
            }],
            playlists: vec![],
        })
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
        Ok(vec![])
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(vec![])
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(vec![])
    }
}

/// Le serveur de la capture : Qobuz actif et connecté, et un second service
/// `desactive`, connecté mais désactivé.
async fn app(desactive: &'static str) -> axum::Router {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    {
        let mut reg = state.services.lock().await;
        reg.register(Box::new(ServiceTemoin {
            nom: "qobuz",
            actif: true,
            connecte: true,
        }));
        reg.register(Box::new(ServiceTemoin {
            nom: desactive,
            actif: false,
            connecte: true,
        }));
    }
    crate::routes::router(state)
}

async fn get_json(app: &axum::Router, chemin: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = resp.status();
    let corps = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        statut,
        StatusCode::OK,
        "{chemin} : {}",
        String::from_utf8_lossy(&corps)
    );
    serde_json::from_slice(&corps).unwrap()
}

/// ⭐ La capture du fil 1957 : « Neil Young » cherché, YouTube désactivé.
/// Le bloc `services.youtube` ne doit pas exister ; Qobuz, lui, répond.
#[tokio::test]
async fn la_recherche_federee_n_interroge_pas_un_service_desactive() {
    let app = app("youtube").await;
    let rep = get_json(&app, "/api/v1/search?q=neil%20young").await;
    let services = rep["services"].as_object().expect("un objet `services`");
    assert!(
        !services.contains_key("youtube"),
        "YouTube est désactivé : il ne doit pas répondre à la recherche (#5103) ; services = {:?}",
        services.keys().collect::<Vec<_>>()
    );
    // Contre-épreuve : le service activé répond toujours, sinon le témoin
    // serait vert pour une recherche qui ne rend plus rien du tout.
    assert_eq!(
        services["qobuz"]["artists"][0]["name"], "Neil Young",
        "Qobuz, activé et connecté, doit toujours répondre"
    );
}

/// Même sous une liste blanche qui le NOMME : le client qui demande
/// `sources=youtube` (la recherche v2 interroge service par service) ne
/// réveille pas un service désactivé.
#[tokio::test]
async fn nommer_le_service_desactive_dans_sources_ne_le_reveille_pas() {
    let app = app("youtube").await;
    let rep = get_json(&app, "/api/v1/search?q=neil%20young&sources=youtube").await;
    let services = rep["services"].as_object().expect("un objet `services`");
    assert!(
        !services.contains_key("youtube"),
        "`sources=youtube` ne doit pas contourner la case Actif ; services = {:?}",
        services.keys().collect::<Vec<_>>()
    );
}

/// L'accueil : « streaming-highlights » annonçait tout service connecté.
/// Tidal désactivé ne s'y invite plus ; Qobuz reste.
#[tokio::test]
async fn l_accueil_n_annonce_pas_un_service_desactive() {
    let app = app("tidal").await;
    let rep = get_json(&app, "/api/v1/home/streaming-highlights").await;
    let annonces: Vec<&str> = rep["services"]
        .as_array()
        .expect("un tableau `services`")
        .iter()
        .filter_map(|s| s["service"].as_str())
        .collect();
    assert!(
        !annonces.contains(&"tidal"),
        "Tidal est désactivé : l'accueil ne doit pas l'annoncer (#5103) ; {annonces:?}"
    );
    assert!(
        annonces.contains(&"qobuz"),
        "Qobuz doit rester : {annonces:?}"
    );
}

/// La règle elle-même, ses quatre cas : il faut les DEUX.
#[tokio::test]
async fn utilisable_exige_actif_et_connecte() {
    for (actif, connecte, attendu) in [
        (true, true, true),
        (false, true, false),
        (true, false, false),
        (false, false, false),
    ] {
        let svc = ServiceTemoin {
            nom: "x",
            actif,
            connecte,
        };
        assert_eq!(
            svc.utilisable().await,
            attendu,
            "actif={actif}, connecté={connecte}"
        );
    }
}
