//! Contrôle parental, #8088 : « Masquer les contenus explicites », de bout en
//! bout par l'API.
//!
//! - pour le profil dont la case est cochée, les pistes et albums marqués
//!   explicites disparaissent de la bibliothèque, de la recherche et des pages
//!   d'un service ; la fiche d'un album explicite est refusée (403) ;
//! - leur lecture est refusée : 403, code `parental_explicit_refused`, phrase
//!   traduite ;
//! - une piste sans marquage reste visible et lisible ;
//! - un autre profil, ou le même sans la case, voit tout.
//!
//! Le service est une doublure nommée `qobuz` (aucun appel réel) ; les
//! marques locales sont posées en base comme le scan les écrit.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::TuneError;
use tune_core::db::backend::ToSqlValue;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

const ADMIN: i64 = 1;
const ENFANT: i64 = 2;
const ZONE: i64 = 80880;
/// Album local 10 : une piste explicite (100), une sans marquage (101).
/// Album local 20 : une piste propre (200).
const PISTE_EXPLICITE: i64 = 100;
const PISTE_SANS_MARQUAGE: i64 = 101;
const PISTE_PROPRE: i64 = 200;

fn piste_de_service(id: &str) -> StreamTrack {
    StreamTrack {
        id: id.into(),
        title: format!("Titre {id}"),
        artist: "Artiste du banc".into(),
        album: Some("Album du banc".into()),
        album_id: Some("alb".into()),
        duration_ms: 180_000,
        cover_path: None,
        track_number: Some(1),
        disc_number: None,
        // Les identifiants « e… » sont marqués explicites par le service.
        explicit: id.starts_with('e'),
        quality: None,
        isrc: None,
        disponible: None,
        composer: None,
        artist_id: None,
    }
}

fn album_de_service(id: &str) -> StreamAlbum {
    StreamAlbum {
        id: id.into(),
        title: format!("Album {id}"),
        artist: "Artiste du banc".into(),
        explicit: Some(id.starts_with('e')),
        ..Default::default()
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
            tracks: vec![piste_de_service("e1"), piste_de_service("p1")],
            albums: vec![album_de_service("ea"), album_de_service("pa")],
            artists: Vec::new(),
            playlists: Vec::new(),
        })
    }
    async fn get_track(&self, id: &str) -> Result<StreamTrack, TuneError> {
        Ok(piste_de_service(id))
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err("le banc ne sert aucun flux".into())
    }
    async fn get_album(&self, id: &str) -> Result<StreamAlbum, TuneError> {
        Ok(album_de_service(id))
    }
    async fn get_album_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(vec![piste_de_service("e2"), piste_de_service("p2")])
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(vec![piste_de_service("e3"), piste_de_service("p3")])
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
    sql(
        "INSERT INTO artists (id, name) VALUES (7, 'Artiste local')",
        &[],
    );
    for (id, titre) in [(10i64, "Album local explicite"), (20, "Album local propre")] {
        sql(
            "INSERT INTO albums (id, title, artist_id) VALUES (?, ?, 7)",
            &[&id, &titre],
        );
    }
    for (id, album, marque) in [
        (PISTE_EXPLICITE, 10i64, Some(1i64)),
        (PISTE_SANS_MARQUAGE, 10, None),
        (PISTE_PROPRE, 20, Some(0)),
    ] {
        sql(
            "INSERT INTO tracks (id, title, artist_id, album_id, file_path, format, explicit) \
             VALUES (?, ?, 7, ?, ?, 'flac', ?)",
            &[
                &id,
                &format!("Piste locale {id}"),
                &album,
                &format!("/musique/{id}.flac"),
                &marque,
            ],
        );
    }
    sql(
        "INSERT INTO zones (id, name, output_type, output_device_id) \
         VALUES (?, 'Zone du banc', 'local', 'local:banc-8088')",
        &[&ZONE],
    );
    state.services.lock().await.register(Box::new(Doublure));
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    profil: i64,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("X-Profile-Id", profil.to_string())
        .header("Accept-Language", "fr")
        .header("content-type", "application/json")
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

async fn poser(app: &axum::Router, revision: i64, masquer: bool) -> i64 {
    let (statut, corps) = appel(
        app,
        "PUT",
        &format!("/api/v1/profiles/{ENFANT}/politique-parentale"),
        ADMIN,
        Some(json!({"revision": revision, "politique": {"masquer_explicite": masquer}})),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["politique"]["masquer_explicite"], json!(masquer));
    corps["revision"].as_i64().unwrap()
}

/// Tous les entiers `id` et chaînes `source_id` d'une réponse, à toute
/// profondeur : ce que le profil voit.
fn identites(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Array(a) => a.iter().for_each(|e| identites(e, out)),
        Value::Object(o) => {
            if let Some(id) = o.get("id").and_then(Value::as_i64) {
                out.push(id.to_string());
            }
            if let Some(id) = o.get("source_id").and_then(Value::as_str) {
                out.push(id.to_string());
            }
            o.values().for_each(|e| identites(e, out));
        }
        _ => {}
    }
}

async fn vu(app: &axum::Router, chemin: &str, profil: i64) -> Vec<String> {
    let (statut, corps) = appel(app, "GET", chemin, profil, None).await;
    assert_eq!(statut, StatusCode::OK, "{chemin} : {corps}");
    let mut out = Vec::new();
    identites(&corps, &mut out);
    out
}

/// Chaque liste, et ce qui doit y manquer (`masque`) ou y rester (`garde`)
/// pour le profil dont la case est cochée.
const LISTES: &[(&str, &[&str], &[&str])] = &[
    ("/api/v1/library/tracks", &["100"], &["101", "200"]),
    ("/api/v1/library/albums", &["10"], &["20"]),
    ("/api/v1/library/search?q=locale", &["100"], &["101", "200"]),
    (
        "/api/v1/streaming/qobuz/search?q=banc",
        &["e1", "ea"],
        &["p1", "pa"],
    ),
    ("/api/v1/streaming/qobuz/albums/pa/tracks", &["e2"], &["p2"]),
    (
        "/api/v1/streaming/qobuz/playlists/pl/tracks",
        &["e3"],
        &["p3"],
    ),
];

#[tokio::test]
async fn les_listes_masquent_l_explicite_8088() {
    let (app, _state) = app().await;
    // Sans politique : tout est là, pour les deux profils.
    for (chemin, masque, garde) in LISTES {
        for profil in [ADMIN, ENFANT] {
            let v = vu(&app, chemin, profil).await;
            for id in masque.iter().chain(garde.iter()) {
                assert!(
                    v.contains(&id.to_string()),
                    "prémisse {chemin} ({profil}) : {id} absent de {v:?}"
                );
            }
        }
    }
    let revision = poser(&app, 0, true).await;
    for (chemin, masque, garde) in LISTES {
        let v = vu(&app, chemin, ENFANT).await;
        for id in *masque {
            assert!(
                !v.contains(&id.to_string()),
                "{chemin} : {id} explicite encore visible : {v:?}"
            );
        }
        for id in *garde {
            assert!(
                v.contains(&id.to_string()),
                "{chemin} : {id} masqué à tort : {v:?}"
            );
        }
        // L'autre profil n'est pas touché.
        let v = vu(&app, chemin, ADMIN).await;
        for id in *masque {
            assert!(
                v.contains(&id.to_string()),
                "{chemin} (admin) : {id} masqué à tort"
            );
        }
    }
    // La fiche d'un album explicite est refusée, celle d'un album propre non.
    let (statut, corps) = appel(
        &app,
        "GET",
        "/api/v1/streaming/qobuz/albums/ea",
        ENFANT,
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::FORBIDDEN, "{corps}");
    assert_eq!(corps["code"], "parental_explicit_refused");
    let (statut, _) = appel(
        &app,
        "GET",
        "/api/v1/streaming/qobuz/albums/pa",
        ENFANT,
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK);

    // Case décochée : tout revient.
    poser(&app, revision, false).await;
    for (chemin, masque, _) in LISTES {
        let v = vu(&app, chemin, ENFANT).await;
        for id in *masque {
            assert!(
                v.contains(&id.to_string()),
                "{chemin} : case décochée, {id} doit revenir"
            );
        }
    }
}

#[tokio::test]
async fn la_lecture_de_l_explicite_est_refusee_8088() {
    let (app, _state) = app().await;
    poser(&app, 0, true).await;
    let jouer = format!("/api/v1/zones/{ZONE}/play");
    for corps in [
        json!({"track_id": PISTE_EXPLICITE}),
        json!({"source": "qobuz", "source_id": "e9", "title": "Titre e9"}),
    ] {
        let (statut, reponse) = appel(&app, "POST", &jouer, ENFANT, Some(corps.clone())).await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "{corps} : {reponse}");
        assert_eq!(reponse["code"], "parental_explicit_refused", "{reponse}");
        assert!(
            reponse["message"]
                .as_str()
                .unwrap_or("")
                .contains("explicite"),
            "la phrase dit pourquoi : {reponse}"
        );
    }
    // Sans marquage, la garde laisse passer (la lecture peut échouer plus
    // loin, sans sortie réelle : seul compte l'absence du refus parental).
    for corps in [
        json!({"track_id": PISTE_SANS_MARQUAGE}),
        json!({"track_id": PISTE_PROPRE}),
        json!({"source": "qobuz", "source_id": "p9", "title": "Titre p9"}),
    ] {
        let (statut, reponse) = appel(&app, "POST", &jouer, ENFANT, Some(corps.clone())).await;
        assert_ne!(
            reponse["code"], "parental_explicit_refused",
            "{corps} : {statut} {reponse}"
        );
    }
    // L'administrateur n'est pas supervisé.
    let (_, reponse) = appel(
        &app,
        "POST",
        &jouer,
        ADMIN,
        Some(json!({"track_id": PISTE_EXPLICITE})),
    )
    .await;
    assert_ne!(reponse["code"], "parental_explicit_refused", "{reponse}");
}
