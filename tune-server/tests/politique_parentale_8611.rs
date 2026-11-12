//! #8611 — un artiste bloqué l'est partout pour un profil supervisé, de bout
//! en bout par l'API (suite de #7913) :
//!
//! - playlists, historique (`/history`, `/profiles/{id}/history`), favoris de
//!   la bibliothèque et des services, résultats et fiches des services ne le
//!   montrent plus ;
//! - sa LECTURE est refusée : `POST /zones/{id}/play` (piste locale ou de
//!   service), flux d'une piste par son identifiant
//!   (`/library/tracks/{id}/audio`, adresse du flux d'un service) — 403,
//!   code stable `parental_content_blocked`, phrase traduite ;
//! - l'administrateur, témoin, voit et joue tout.
//!
//! Le service est une doublure nommée `qobuz` (aucun appel réel).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::TuneError;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::history_repo::{HistoryRepo, ListenRecord};
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

const ADMIN: i64 = 1;
const ENFANT: i64 = 2;
const ZONE: i64 = 86110;
const INTERDIT: &str = "Groupe Interdit";
const LIBRE: &str = "Groupe Libre";
const CODE: &str = "parental_content_blocked";

/// Les identifiants de service qui commencent par `interdit` sont de
/// l'artiste bloqué.
fn artiste_de(id: &str) -> String {
    if id.starts_with("interdit") {
        INTERDIT.into()
    } else {
        LIBRE.into()
    }
}

fn piste(id: &str) -> StreamTrack {
    StreamTrack {
        id: id.into(),
        title: format!("Titre {id}"),
        artist: artiste_de(id),
        album: None,
        album_id: None,
        duration_ms: 180_000,
        cover_path: None,
        track_number: Some(1),
        disc_number: None,
        explicit: false,
        quality: None,
        isrc: None,
        disponible: None,
        composer: None,
        artist_id: None,
    }
}

fn album(id: &str) -> StreamAlbum {
    StreamAlbum {
        id: id.into(),
        title: format!("Album {id}"),
        artist: artiste_de(id),
        ..Default::default()
    }
}

fn artiste(id: &str) -> StreamArtist {
    StreamArtist {
        id: id.into(),
        name: artiste_de(id),
        image_path: None,
        bio: None,
    }
}

struct Doublure;

#[async_trait::async_trait]
impl StreamingService for Doublure {
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
        true
    }
    fn set_enabled(&mut self, _enabled: bool) {}
    async fn authenticate(&mut self, _c: &Value) -> Result<AuthStatus, TuneError> {
        Ok(self.auth_status().await)
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: true,
            username: Some("banc".into()),
            ..Default::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        Ok(SearchResults {
            tracks: vec![piste("interdit-t"), piste("libre-t")],
            albums: vec![album("interdit-a"), album("libre-a")],
            artists: vec![artiste("interdit-r"), artiste("libre-r")],
            playlists: Vec::new(),
        })
    }
    async fn get_track(&self, id: &str) -> Result<StreamTrack, TuneError> {
        Ok(piste(id))
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err("le banc ne sert aucun flux".into())
    }
    async fn get_album(&self, id: &str) -> Result<StreamAlbum, TuneError> {
        Ok(album(id))
    }
    async fn get_album_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(vec![piste("interdit-2"), piste("libre-2")])
    }
    async fn get_artist(&self, id: &str) -> Result<StreamArtist, TuneError> {
        Ok(artiste(id))
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(vec![piste("interdit-3"), piste("libre-3")])
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

/// Bibliothèque : artiste 7 (bloqué), album 70, piste 700 ; artiste 8
/// (libre), album 80, piste 800.
async fn app() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let db = &state.backend;
    let sql = |s: &str, p: &[&dyn ToSqlValue]| {
        db.execute(s, p).unwrap();
    };
    sql(
        "INSERT INTO profiles (id, username, display_name, is_admin) VALUES (2, 'enfant', 'Enfant', 0)",
        &[],
    );
    for (id, nom) in [(7i64, INTERDIT), (8, LIBRE)] {
        sql("INSERT INTO artists (id, name) VALUES (?, ?)", &[&id, &nom]);
        sql(
            "INSERT INTO albums (id, title, artist_id) VALUES (?, ?, ?)",
            &[&(id * 10), &format!("Album de {nom}"), &id],
        );
        sql(
            "INSERT INTO tracks (id, title, artist_id, album_id, file_path, format) VALUES (?, ?, ?, ?, ?, 'flac')",
            &[
                &(id * 100),
                &format!("Titre de {nom}"),
                &id,
                &(id * 10),
                &format!("/musique/absent-8611/{id}.flac"),
            ],
        );
    }
    sql(
        "INSERT INTO zones (id, name, output_type, output_device_id) \
         VALUES (?, 'Zone du banc', 'local', 'local:banc-8611')",
        &[&ZONE],
    );
    state.services.lock().await.register(Box::new(Doublure));
    let historique = HistoryRepo::with_backend(state.backend.clone());
    for profil in [ADMIN, ENFANT] {
        for (track_id, artiste, source, source_id) in [
            (Some(700i64), INTERDIT, "local", None),
            (Some(800), LIBRE, "local", None),
            (None, INTERDIT, "qobuz", Some("interdit-h")),
            (None, LIBRE, "qobuz", Some("libre-h")),
        ] {
            historique
                .record(&ListenRecord {
                    track_id,
                    title: format!("Écoute de {artiste}"),
                    artist_name: Some(artiste.into()),
                    source: source.into(),
                    source_id: source_id.map(String::from),
                    duration_ms: 180_000,
                    profile_id: Some(profil),
                    ..Default::default()
                })
                .unwrap();
        }
    }
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn appel_brut(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    profil: Option<i64>,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("Accept-Language", "fr")
        .header("content-type", "application/json");
    if let Some(p) = profil {
        req = req.header("X-Profile-Id", p.to_string());
    }
    let req = req
        .body(
            corps
                .map(|c| Body::from(c.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let reponse = app.clone().oneshot(req).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    profil: i64,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    appel_brut(app, methode, chemin, Some(profil), corps).await
}

async fn superviser(app: &axum::Router) {
    let (statut, corps) = appel(
        app,
        "PUT",
        &format!("/api/v1/profiles/{ENFANT}/politique-parentale"),
        ADMIN,
        Some(json!({"revision": 0, "politique": {
            "artistes_bloques": [{"id": 7, "nom": INTERDIT}],
        }})),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
}

/// Une playlist (700, 800) et les favoris (artiste, album, piste des deux
/// artistes ; un favori de service de chacun) pour chaque profil.
async fn garnir(app: &axum::Router) -> i64 {
    let (statut, p) = appel(
        app,
        "POST",
        "/api/v1/playlists",
        ADMIN,
        Some(json!({"name": "Mélange 8611"})),
    )
    .await;
    assert!(statut.is_success(), "{statut} {p}");
    let pid = p["id"].as_i64().expect("identifiant de la playlist");
    let (statut, r) = appel(
        app,
        "POST",
        &format!("/api/v1/playlists/{pid}/tracks"),
        ADMIN,
        Some(json!({"track_ids": [700, 800]})),
    )
    .await;
    assert!(statut.is_success(), "{statut} {r}");
    for profil in [ADMIN, ENFANT] {
        for (genre, id) in [
            ("artist", 7),
            ("artist", 8),
            ("album", 70),
            ("album", 80),
            ("track", 700),
            ("track", 800),
        ] {
            let (statut, r) = appel(
                app,
                "POST",
                &format!("/api/v1/profiles/{profil}/favorites/add"),
                profil,
                Some(json!({"item_type": genre, "item_id": id})),
            )
            .await;
            assert!(statut.is_success(), "{statut} {r}");
        }
        for (id, titre, artiste) in [
            ("d1", "Album fav interdit", INTERDIT),
            ("d2", "Album fav libre", LIBRE),
        ] {
            let (statut, r) = appel(
                app,
                "POST",
                &format!("/api/v1/profiles/{profil}/favorites/streaming/add"),
                profil,
                Some(
                    json!({"item_type": "album", "service": "deezer", "service_id": id,
                            "title": titre, "artist": artiste}),
                ),
            )
            .await;
            assert!(statut.is_success(), "{statut} {r}");
        }
    }
    pid
}

fn contient(corps: &Value, aiguille: &str) -> bool {
    corps.to_string().contains(aiguille)
}

#[tokio::test]
async fn les_listes_restantes_ne_montrent_plus_l_artiste_bloque_8611() {
    let (app, _state) = app().await;
    let pid = garnir(&app).await;
    let listes = |profil: i64| {
        vec![
            format!("/api/v1/playlists/{pid}/tracks"),
            "/api/v1/history".to_string(),
            format!("/api/v1/profiles/{profil}/history"),
            format!("/api/v1/profiles/{profil}/favorites/streaming"),
            "/api/v1/streaming/qobuz/search?q=banc".to_string(),
            "/api/v1/streaming/qobuz/albums/libre-a/tracks".to_string(),
        ]
    };
    // Prémisse : sans politique, l'artiste paraît partout.
    for chemin in listes(ENFANT) {
        let (statut, corps) = appel(&app, "GET", &chemin, ENFANT, None).await;
        assert_eq!(statut, StatusCode::OK, "{chemin} : {corps}");
        assert!(contient(&corps, INTERDIT), "prémisse {chemin} : {corps}");
    }
    superviser(&app).await;
    for chemin in listes(ENFANT) {
        let (statut, corps) = appel(&app, "GET", &chemin, ENFANT, None).await;
        assert_eq!(statut, StatusCode::OK, "{chemin} : {corps}");
        assert!(
            !contient(&corps, INTERDIT),
            "{chemin} : l'artiste bloqué paraît encore : {corps}"
        );
        assert!(
            contient(&corps, LIBRE),
            "{chemin} : retiré à tort : {corps}"
        );
    }
    // Témoin administrateur : rien n'est retiré.
    for chemin in listes(ADMIN) {
        let (statut, corps) = appel(&app, "GET", &chemin, ADMIN, None).await;
        assert_eq!(statut, StatusCode::OK, "{chemin} : {corps}");
        assert!(contient(&corps, INTERDIT), "{chemin} (admin) : {corps}");
    }

    // Favoris de la bibliothèque : des identifiants, résolus en base.
    let favoris = |corps: &Value| -> Vec<(String, i64)> {
        corps
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .map(|f| {
                (
                    f["item_type"].as_str().unwrap_or_default().to_string(),
                    f["item_id"].as_i64().unwrap_or_default(),
                )
            })
            .collect()
    };
    let (statut, corps) = appel(
        &app,
        "GET",
        &format!("/api/v1/profiles/{ENFANT}/favorites"),
        ENFANT,
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let mut vus = favoris(&corps);
    vus.sort();
    assert_eq!(
        vus,
        vec![
            ("album".to_string(), 80),
            ("artist".to_string(), 8),
            ("track".to_string(), 800)
        ],
        "{corps}"
    );
    let (_, corps) = appel(
        &app,
        "GET",
        &format!("/api/v1/profiles/{ADMIN}/favorites"),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(favoris(&corps).len(), 6, "témoin administrateur : {corps}");

    // Fiches des services : refusées pour l'artiste bloqué.
    for chemin in [
        "/api/v1/streaming/qobuz/albums/interdit-a",
        "/api/v1/streaming/qobuz/artists/interdit-r",
        "/api/v1/streaming/qobuz/tracks/interdit-t",
    ] {
        let (statut, corps) = appel(&app, "GET", chemin, ENFANT, None).await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "{chemin} : {corps}");
        assert_eq!(corps["code"], CODE, "{chemin}");
        let (statut, corps) = appel(&app, "GET", chemin, ADMIN, None).await;
        assert_eq!(statut, StatusCode::OK, "{chemin} (admin) : {corps}");
    }
    for chemin in [
        "/api/v1/streaming/qobuz/albums/libre-a",
        "/api/v1/streaming/qobuz/artists/libre-r",
    ] {
        let (statut, corps) = appel(&app, "GET", chemin, ENFANT, None).await;
        assert_eq!(statut, StatusCode::OK, "{chemin} : {corps}");
    }
}

#[tokio::test]
async fn la_lecture_d_un_artiste_bloque_est_refusee_8611() {
    let (app, _state) = app().await;
    superviser(&app).await;
    let jouer = format!("/api/v1/zones/{ZONE}/play");
    for corps in [
        json!({"track_id": 700}),
        json!({"source": "qobuz", "source_id": "interdit-9", "title": "Titre"}),
        // Le nom porté par la demande ne trompe pas : le service est relu.
        json!({"source": "qobuz", "source_id": "interdit-9", "title": "Titre", "artist_name": LIBRE}),
    ] {
        let (statut, reponse) = appel(&app, "POST", &jouer, ENFANT, Some(corps.clone())).await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "{corps} : {reponse}");
        assert_eq!(reponse["code"], CODE, "{reponse}");
        assert!(
            reponse["message"].as_str().unwrap_or("").contains("bloqué"),
            "la phrase dit pourquoi : {reponse}"
        );
    }
    for corps in [
        json!({"track_id": 800}),
        json!({"source": "qobuz", "source_id": "libre-9", "title": "Titre"}),
    ] {
        let (statut, reponse) = appel(&app, "POST", &jouer, ENFANT, Some(corps.clone())).await;
        assert_ne!(reponse["code"], CODE, "{corps} : {statut} {reponse}");
    }
    // Témoin administrateur.
    for corps in [
        json!({"track_id": 700}),
        json!({"source": "qobuz", "source_id": "interdit-9", "title": "Titre"}),
    ] {
        let (statut, reponse) = appel(&app, "POST", &jouer, ADMIN, Some(corps.clone())).await;
        assert_ne!(
            reponse["code"], CODE,
            "{corps} (admin) : {statut} {reponse}"
        );
    }
}

#[tokio::test]
async fn le_flux_d_une_piste_bloquee_est_refuse_8611() {
    let (app, _state) = app().await;
    superviser(&app).await;
    for (chemin, bloque) in [
        ("/api/v1/library/tracks/700/audio", true),
        ("/api/v1/library/tracks/800/audio", false),
        ("/api/v1/streaming/qobuz/tracks/interdit-5/url", true),
        ("/api/v1/streaming/qobuz/tracks/libre-5/url", false),
    ] {
        for methode in ["GET", "HEAD"] {
            if methode == "HEAD" && chemin.ends_with("/url") {
                continue;
            }
            let (statut, corps) = appel(&app, methode, chemin, ENFANT, None).await;
            if bloque {
                assert_eq!(
                    statut,
                    StatusCode::FORBIDDEN,
                    "{methode} {chemin} : {corps}"
                );
                if methode == "GET" {
                    assert_eq!(corps["code"], CODE, "{chemin}");
                }
            } else {
                assert_ne!(
                    statut,
                    StatusCode::FORBIDDEN,
                    "{methode} {chemin} : {corps}"
                );
            }
            // Témoin administrateur.
            let (statut, corps) = appel(&app, methode, chemin, ADMIN, None).await;
            assert_ne!(
                statut,
                StatusCode::FORBIDDEN,
                "{methode} {chemin} (admin) : {corps}"
            );
        }
    }
    // Un renderer qui tire le flux sans se nommer n'est pas jugé ici : c'est
    // l'orchestrateur, gardé, qui lui a confié la piste.
    let (statut, corps) =
        appel_brut(&app, "GET", "/api/v1/library/tracks/700/audio", None, None).await;
    assert_ne!(statut, StatusCode::FORBIDDEN, "{corps}");
}
