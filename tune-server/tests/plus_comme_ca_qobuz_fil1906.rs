//! « Plus comme ça » sur un titre de service — fil 1906 (FabienM), point 3.
//!
//! `GET /api/v1/streaming/{service}/tracks/{track_id}/similar` rend les titres
//! voisins d'un titre Qobuz, par l'algorithme de la radio de fin de file
//! (`auto_dj::pistes_similaires_du_service`). Ce témoin attaque la ROUTE MONTÉE
//! par `tune_server::routes::router`, pas la fonction : ce qui doit être
//! prouvé ici est le câblage — la route existe, elle exclut le titre source,
//! et un service sans similarité reçoit un refus explicite (501) plutôt
//! qu'une liste vide.
//!
//! Les services sont simulés et l'API d'enrichissement pointe sur un port
//! fermé : rien ne sort sur Internet.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::collections::HashMap;
use tower::ServiceExt;
use tune_core::TuneError;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

fn piste(id: &str, artiste: &str, artiste_id: Option<&str>) -> StreamTrack {
    StreamTrack {
        id: id.into(),
        title: format!("Titre {id}"),
        artist: artiste.into(),
        album: None,
        album_id: None,
        duration_ms: 200_000,
        cover_path: None,
        track_number: None,
        disc_number: None,
        explicit: false,
        disponible: None,
        quality: None,
        isrc: None,
        composer: None,
        artist_id: artiste_id.map(str::to_owned),
    }
}

fn artiste(id: &str, nom: &str) -> StreamArtist {
    StreamArtist {
        id: id.into(),
        name: nom.into(),
        image_path: None,
        bio: None,
    }
}

/// Un service simulé. `similarite` décide s'il se déclare capable de
/// « Plus comme ça » — Qobuz oui, Tidal non.
struct ServiceSimule {
    nom: String,
    similarite: bool,
}

#[async_trait::async_trait]
impl StreamingService for ServiceSimule {
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
        Ok(SearchResults {
            tracks: Vec::new(),
            albums: Vec::new(),
            artists: Vec::new(),
            playlists: Vec::new(),
        })
    }
    /// Le titre source : « Try Me » de l'artiste g.
    async fn get_track(&self, id: &str) -> Result<StreamTrack, TuneError> {
        if id == "source" {
            Ok(piste("source", "Graine", Some("g")))
        } else {
            Err(TuneError::NotFound(format!("piste {id}")))
        }
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
    /// A a pour premier titre phare... le titre source : il doit être sauté.
    async fn get_artist_top_tracks(&self, artist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        let mut catalogue: HashMap<&str, Vec<StreamTrack>> = HashMap::new();
        catalogue.insert(
            "a",
            vec![piste("source", "A", Some("a")), piste("a2", "A", Some("a"))],
        );
        catalogue.insert("b", vec![piste("b1", "B", Some("b"))]);
        Ok(catalogue.remove(artist_id).unwrap_or_default())
    }
    async fn get_similar_artists(
        &self,
        artist_id: &str,
        _limit: usize,
    ) -> Result<Vec<StreamArtist>, TuneError> {
        if artist_id == "g" {
            Ok(vec![artiste("a", "A"), artiste("b", "B")])
        } else {
            Ok(Vec::new())
        }
    }
    fn propose_des_artistes_similaires(&self) -> bool {
        self.similarite
    }
    async fn get_playlist(&self, _p: &str) -> Result<StreamPlaylist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist_tracks(&self, _p: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(Vec::new())
    }
}

async fn app() -> axum::Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    // L'API d'enrichissement sur un port fermé : refus immédiat, zéro voisin
    // de ce côté, et la route passe aux voisins du service — hors réseau.
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set("artist_enrichment_api", "http://127.0.0.1:9")
        .unwrap();
    {
        let mut reg = state.services.lock().await;
        reg.register(Box::new(ServiceSimule {
            nom: "qobuz-simule".into(),
            similarite: true,
        }));
        reg.register(Box::new(ServiceSimule {
            nom: "tidal-simule".into(),
            similarite: false,
        }));
    }
    tune_server::routes::router(state)
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Vec<u8>) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

#[tokio::test]
async fn un_titre_qobuz_rend_ses_voisins_sans_lui_meme() {
    let app = app().await;
    let (status, corps) = get(&app, "/api/v1/streaming/qobuz-simule/tracks/source/similar").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&corps)
    );
    let pistes: Vec<Value> = serde_json::from_slice(&corps).unwrap();
    let ids: Vec<&str> = pistes
        .iter()
        .map(|p| p["source_id"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(
        ids,
        vec!["a2", "b1"],
        "un titre par voisin, le titre source exclu même quand un voisin le \
         porte en tête"
    );
    // Le format des autres routes streaming : le client lit `source_id`,
    // `artist_name`, `title`.
    assert_eq!(pistes[0]["artist_name"], "A");
    assert_eq!(pistes[0]["title"], "Titre a2");
}

/// #4806 suite — un titre de ce service BANNI par le profil actif ne revient
/// jamais par « Plus comme ça » (ni par la radio de fin de file, qui partage
/// `hidden_repo::exclure_les_titres_de_service_bannis`). Témoin : sans
/// bannissement, `a2` est proposé.
#[tokio::test]
async fn un_titre_de_service_banni_n_est_jamais_propose() {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set("artist_enrichment_api", "http://127.0.0.1:9")
        .unwrap();
    state
        .services
        .lock()
        .await
        .register(Box::new(ServiceSimule {
            nom: "qobuz-simule".into(),
            similarite: true,
        }));
    let app = tune_server::routes::router(state.clone());
    let ids = |corps: &[u8]| -> Vec<String> {
        let pistes: Vec<Value> = serde_json::from_slice(corps).unwrap();
        pistes
            .iter()
            .map(|p| p["source_id"].as_str().unwrap_or_default().to_owned())
            .collect()
    };

    let (status, corps) = get(&app, "/api/v1/streaming/qobuz-simule/tracks/source/similar").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&corps), vec!["a2", "b1"], "témoin");

    let bans = tune_core::db::hidden_repo::HiddenRepo::with_backend(state.backend.clone());
    assert!(
        bans.ban_streaming_track(
            1,
            &tune_core::db::hidden_repo::TitreDeService {
                source: "Qobuz-Simule".into(),
                source_id: "a2".into(),
                ..Default::default()
            },
        )
        .unwrap()
    );
    let (status, corps) = get(&app, "/api/v1/streaming/qobuz-simule/tracks/source/similar").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&corps), vec!["b1"], "le titre banni n'est plus proposé");

    // Un AUTRE profil que celui des sélections automatiques ne compte pas.
    bans.unban_streaming_track(1, "qobuz-simule", "a2").unwrap();
    bans.ban_streaming_track(
        2,
        &tune_core::db::hidden_repo::TitreDeService {
            source: "qobuz-simule".into(),
            source_id: "a2".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let (_, corps) = get(&app, "/api/v1/streaming/qobuz-simule/tracks/source/similar").await;
    assert_eq!(ids(&corps), vec!["a2", "b1"], "banni chez 2, pas chez 1");
}

#[tokio::test]
async fn la_borne_limit_est_tenue() {
    let app = app().await;
    let (status, corps) = get(
        &app,
        "/api/v1/streaming/qobuz-simule/tracks/source/similar?limit=1",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let pistes: Vec<Value> = serde_json::from_slice(&corps).unwrap();
    assert_eq!(pistes.len(), 1);
}

/// Tidal, Bandcamp… ne connaissent pas leurs artistes similaires : refus
/// explicite, pas une liste vide.
#[tokio::test]
async fn un_service_sans_similarite_recoit_un_501() {
    let app = app().await;
    let (status, corps) = get(&app, "/api/v1/streaming/tidal-simule/tracks/source/similar").await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert!(
        String::from_utf8_lossy(&corps).contains("tidal-simule"),
        "le refus nomme le service : {}",
        String::from_utf8_lossy(&corps)
    );
}

#[tokio::test]
async fn un_service_inconnu_recoit_un_404() {
    let app = app().await;
    let (status, _) = get(&app, "/api/v1/streaming/inconnu/tracks/source/similar").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Le titre source introuvable chez le service : l'erreur remonte, pas une
/// liste vide trompeuse.
#[tokio::test]
async fn un_titre_introuvable_ne_rend_pas_une_liste_vide() {
    let app = app().await;
    let (status, _) = get(&app, "/api/v1/streaming/qobuz-simule/tracks/absent/similar").await;
    assert!(
        !status.is_success(),
        "un titre illisible ne doit pas passer pour « aucun voisin » (statut {status})"
    );
}
