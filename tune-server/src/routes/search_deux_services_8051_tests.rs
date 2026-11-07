//! #8051 — la recherche globale ne rendait que Qobuz, Spotify connecté.
//!
//! Fil ASR : « Afficher plus » ou non, aucun titre ni album Spotify. Un
//! Spotify en échec était retiré de `services` sans un mot pour le client :
//! pour l'écran, c'était « aucun résultat ». Ces témoins attaquent la route
//! montée, avec deux services simulés tous deux activés et connectés, sans
//! réseau.

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
    /// `Some(motif)` : la recherche échoue avec ce motif.
    echec: Option<&'static str>,
}

fn piste(service: &str) -> StreamTrack {
    StreamTrack {
        id: format!("{service}-so-what"),
        title: "So What".into(),
        artist: "Miles Davis".into(),
        album: Some("Kind of Blue".into()),
        album_id: Some(format!("{service}-kob")),
        duration_ms: 562_000,
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
        true
    }
    fn set_enabled(&mut self, _enabled: bool) {}
    async fn authenticate(&mut self, _c: &Value) -> Result<AuthStatus, TuneError> {
        Ok(self.auth_status().await)
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: true,
            ..Default::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        if let Some(motif) = self.echec {
            return Err(motif.into());
        }
        Ok(SearchResults {
            tracks: vec![piste(self.nom)],
            albums: vec![StreamAlbum {
                id: format!("{}-kob", self.nom),
                title: "Kind of Blue".into(),
                artist: "Miles Davis".into(),
                ..Default::default()
            }],
            artists: vec![],
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

/// Qobuz et Spotify, tous deux activés et connectés ; Spotify échoue avec
/// `echec_spotify` s'il est donné.
async fn app(echec_spotify: Option<&'static str>) -> axum::Router {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    {
        let mut reg = state.services.lock().await;
        reg.register(Box::new(ServiceTemoin {
            nom: "qobuz",
            echec: None,
        }));
        reg.register(Box::new(ServiceTemoin {
            nom: "spotify",
            echec: echec_spotify,
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

/// ⭐ Le témoin de l'issue : deux services connectés, les deux répondent,
/// titres ET albums, en mode paginé (celui de la recherche v2) comme hors
/// pagination.
#[tokio::test]
async fn la_recherche_globale_rend_qobuz_et_spotify_8051() {
    let app = app(None).await;
    for chemin in [
        "/api/v1/search?q=miles%20davis&paged=true",
        "/api/v1/search?q=miles%20davis",
    ] {
        let rep = get_json(&app, chemin).await;
        for service in ["qobuz", "spotify"] {
            let bloc = &rep["services"][service];
            assert_eq!(
                bloc["tracks"][0]["source_id"],
                format!("{service}-so-what"),
                "{chemin} : les titres {service} doivent être rendus ; services = {}",
                rep["services"]
            );
            assert_eq!(
                bloc["albums"][0]["source_id"],
                format!("{service}-kob"),
                "{chemin} : les albums {service} doivent être rendus"
            );
        }
        assert!(
            rep.get("services_errors").is_none(),
            "{chemin} : aucun échec à signaler, la clé reste absente"
        );
    }
}

/// Un Spotify qui échoue n'est plus avalé : Qobuz répond toujours, et
/// `services_errors.spotify` dit pourquoi Spotify n'a rien rendu.
#[tokio::test]
async fn l_echec_de_spotify_est_remonte_au_client_8051() {
    let app = app(Some("token expired")).await;
    for chemin in [
        "/api/v1/search?q=miles%20davis&paged=true",
        "/api/v1/search?q=miles%20davis",
    ] {
        let rep = get_json(&app, chemin).await;
        assert_eq!(
            rep["services"]["qobuz"]["tracks"][0]["source_id"], "qobuz-so-what",
            "{chemin} : Qobuz doit toujours répondre"
        );
        assert!(
            rep["services"].get("spotify").is_none(),
            "{chemin} : un Spotify en échec n'a pas de bloc de résultats"
        );
        assert_eq!(
            rep["services_errors"]["spotify"], "token expired",
            "#8051 : {chemin} doit dire que Spotify a échoué, et pourquoi ; réponse = {rep}"
        );
    }
}

/// Le motif remonté ne porte pas la partie requête d'une adresse (une
/// erreur réseau cite l'URL appelée), et reste borné.
#[test]
fn le_motif_public_ne_porte_ni_requete_ni_longueur_8051() {
    let motif = super::motif_public(
        "error sending request for url (https://api.example/search?q=x&token=secret) : délai",
    );
    assert_eq!(
        motif,
        "error sending request for url (https://api.example/search) : délai"
    );
    assert!(!motif.contains("secret"));
    assert_eq!(super::motif_public(&"é".repeat(500)).chars().count(), 200);
}
