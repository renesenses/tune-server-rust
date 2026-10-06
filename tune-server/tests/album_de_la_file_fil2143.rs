//! L'album d'un titre de la FILE — fil forum 2143 (FabienM), points 5, 6 et 8.
//!
//! - **Points 5 et 6** : sur un titre Bandcamp lancé hors de la page de son
//!   album (historique, file, favori), le titre d'album de « Lecture en cours »
//!   ouvrait la Recherche, et le réglage « Lecture en cours ouvre l'album »
//!   ouvrait l'écran Lecture en cours. Cause : Bandcamp refuse toujours la
//!   fiche d'une piste seule (`get_track`), donc `GET /zones/{id}/album-en-cours`
//!   rendait 404 `service_injoignable`. La ligne de file savait pourtant son
//!   album depuis l'enfilage (`album_ref`, migration 114, #5706).
//! - **Point 8** : « Aller à l'album » manquait au menu d'un titre de la file,
//!   faute d'identifiant d'album sur les lignes de `GET /zones/{id}/queue`.
//!
//! Ces témoins attaquent les routes publiques, montées par le vrai routeur.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! par sa strophe `[[test]]` dans `tune-server/Cargo.toml`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::TuneError;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::models::{Album, Track};
use tune_core::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};
use tune_server::state::AppState;

/// La page d'album Bandcamp : c'est l'identifiant d'album que le service
/// donne à ses pistes (`StreamTrack.album_id`) et qu'il sait rouvrir.
const PAGE: &str = "https://framewerk.bandcamp.com/album/love-parade";
/// L'identifiant d'une piste Bandcamp : son URL de flux.
const PISTE_BC: &str = "https://t4.bcbits.com/stream/abc/mp3-128/111";

/// Un service simulé. `album_du_service` : ce que `get_track` nomme comme
/// album — `None` pour un service qui, comme Bandcamp, REFUSE la fiche d'une
/// piste seule.
struct ServiceSimule {
    nom: String,
    album_du_service: Option<String>,
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
    async fn get_track(&self, id: &str) -> Result<StreamTrack, TuneError> {
        let Some(album) = self.album_du_service.clone() else {
            // Le refus de `plugins/tune-bandcamp/src/service.rs`.
            return Err("de fiche pour une piste seule — passer par son album".into());
        };
        Ok(StreamTrack {
            id: id.into(),
            title: "Titre".into(),
            artist: "Artiste".into(),
            album: None,
            album_id: Some(album),
            duration_ms: 200_000,
            cover_path: None,
            track_number: None,
            disc_number: None,
            explicit: false,
            disponible: None,
            quality: None,
            isrc: None,
            composer: None,
            artist_id: Some("art-9".into()),
        })
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
    async fn get_artist_top_tracks(&self, _a: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(Vec::new())
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

async fn app_et_etat() -> (axum::Router, AppState) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    {
        let mut reg = state.services.lock().await;
        reg.register(Box::new(ServiceSimule {
            nom: "bandcamp".into(),
            album_du_service: None,
        }));
        reg.register(Box::new(ServiceSimule {
            nom: "qobuz".into(),
            album_du_service: Some("q-alb-7".into()),
        }));
    }
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn lire(app: &axum::Router, chemin: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn zone(state: &AppState, nom: &str) -> i64 {
    ZoneRepo::with_backend(state.backend.clone())
        .create(nom, Some("mock"), Some(format!("sortie-{nom}").as_str()))
        .expect("creation de zone")
}

/// Une piste locale rangée dans un album de la bibliothèque. Rend
/// `(track_id, album_id)`.
fn piste_locale_dans_un_album(state: &AppState) -> (i64, i64) {
    let album_id = AlbumRepo::with_backend(state.backend.clone())
        .create(&Album::new("Kind of Blue".into()))
        .expect("creation d'album");
    let mut t = Track::new("So What".into());
    t.album_id = Some(album_id);
    t.format = Some("flac".into());
    t.file_path = Some("/musique/so-what.flac".into());
    t.duration_ms = 240_000;
    let track_id = TrackRepo::with_backend(state.backend.clone())
        .create(&t)
        .expect("insertion de piste");
    (track_id, album_id)
}

fn de_service(source: &str, source_id: &str, album_ref: Option<&str>) -> QueueInput {
    QueueInput::Streaming {
        source: source.into(),
        source_id: source_id.into(),
        title: "Meet Her At The Love Parade".into(),
        artist: "Framewerk".into(),
        album: Some("Love Parade".into()),
        cover_url: None,
        duration_ms: 300_000,
        track_number: Some(2),
        disc_number: None,
        album_ref: album_ref.map(String::from),
    }
}

fn enfiler(state: &AppState, zone_id: i64, lignes: &[QueueInput]) {
    PlayQueueRepo::with_backend(state.backend.clone())
        .insert_at(zone_id, lignes, None)
        .expect("mise en file");
}

/// Fait jouer une piste de service sur la zone, en geste de PISTE (lancée
/// depuis l'historique, la file ou un favori — pas depuis sa page d'album).
async fn jouer(state: &AppState, zone_id: i64, source: &str, source_id: &str) {
    state
        .playback
        .set_session_context(
            zone_id,
            Some("track".into()),
            Some(source_id.into()),
            Some(source.into()),
            None,
            None,
        )
        .await;
    state
        .playback
        .play(
            zone_id,
            tune_core::playback::NowPlaying {
                title: "Meet Her At The Love Parade".into(),
                source: source.into(),
                source_id: Some(source_id.into()),
                ..Default::default()
            },
        )
        .await;
}

// ─── Point 8 : la file porte l'album de chaque ligne ────────────────────────

#[tokio::test]
async fn chaque_ligne_de_file_designe_son_album() {
    let (app, state) = app_et_etat().await;
    let zid = zone(&state, "salon");
    let (track_id, album_id) = piste_locale_dans_un_album(&state);
    enfiler(
        &state,
        zid,
        &[
            QueueInput::Local { track_id },
            de_service("bandcamp", PISTE_BC, Some(PAGE)),
            de_service("qobuz", "q-1", None),
        ],
    );

    let (status, corps) = lire(&app, &format!("/api/v1/zones/{zid}/queue")).await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    let lignes = corps["tracks"].as_array().expect("tracks");
    assert_eq!(lignes.len(), 3, "{corps}");

    // La ligne locale : l'entier de bibliothèque, sans identifiant de service.
    assert_eq!(
        lignes[0]["album_id"],
        Value::from(album_id),
        "une ligne locale doit porter l'album de sa piste (« Aller à l'album »)"
    );
    assert_eq!(lignes[0]["album_id_service"], Value::Null);

    // La ligne Bandcamp : l'album chez son service, pas d'entier local.
    assert_eq!(lignes[1]["album_id"], Value::Null);
    assert_eq!(
        lignes[1]["album_id_service"], PAGE,
        "une ligne de service doit porter l'album que la source a donné à l'enfilage"
    );

    // Une ligne de service sans référence : rien n'est inventé.
    assert_eq!(lignes[2]["album_id"], Value::Null);
    assert_eq!(lignes[2]["album_id_service"], Value::Null);

    // La référence interne de resignature (#5706) reste hors du JSON.
    for (i, l) in lignes.iter().enumerate() {
        assert!(
            l.get("album_ref").is_none(),
            "ligne {i} : `album_ref` ne doit pas apparaître dans la file"
        );
        assert!(l.get("album_id").is_some(), "ligne {i} : clef album_id");
        assert!(
            l.get("album_id_service").is_some(),
            "ligne {i} : clef album_id_service"
        );
    }
}

// ─── Points 5 et 6 : l'album en cours d'un titre Bandcamp ───────────────────

/// Le cas de FabienM : un titre Bandcamp lancé hors de sa page d'album.
#[tokio::test]
async fn un_titre_bandcamp_hors_de_son_album_rend_son_album() {
    let (app, state) = app_et_etat().await;
    let zid = zone(&state, "bureau");
    enfiler(&state, zid, &[de_service("bandcamp", PISTE_BC, Some(PAGE))]);
    jouer(&state, zid, "bandcamp", PISTE_BC).await;

    let (status, corps) = lire(&app, &format!("/api/v1/zones/{zid}/album-en-cours")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "un titre Bandcamp dont la file sait l'album doit rendre cet album : {corps}"
    );
    assert_eq!(corps["kind"], "streaming");
    assert_eq!(corps["service"], "bandcamp");
    assert_eq!(corps["album_id"], PAGE);
    assert_eq!(corps["origin"], "queue_entry");
}

/// Le service qui sait répondre passe d'abord : il nomme aussi l'artiste, et
/// la file ne doit pas le court-circuiter.
#[tokio::test]
async fn un_service_qui_repond_garde_la_main() {
    let (app, state) = app_et_etat().await;
    let zid = zone(&state, "cuisine");
    enfiler(
        &state,
        zid,
        &[de_service("qobuz", "q-1", Some("q-alb-ancien"))],
    );
    jouer(&state, zid, "qobuz", "q-1").await;

    let (status, corps) = lire(&app, &format!("/api/v1/zones/{zid}/album-en-cours")).await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_eq!(corps["album_id"], "q-alb-7");
    assert_eq!(corps["artist_id"], "art-9");
    assert_eq!(corps["origin"], "service_lookup");
}

/// Contre-épreuve : une ligne qui n'est PAS celle qui joue ne prête pas son
/// album. Sans référence sur la bonne ligne, la route garde son 404.
#[tokio::test]
async fn la_file_ne_prete_pas_l_album_d_une_voisine() {
    let (app, state) = app_et_etat().await;
    let zid = zone(&state, "chambre");
    enfiler(
        &state,
        zid,
        &[
            de_service("bandcamp", "https://t4.bcbits.com/stream/autre", Some(PAGE)),
            de_service("bandcamp", PISTE_BC, None),
        ],
    );
    jouer(&state, zid, "bandcamp", PISTE_BC).await;

    let (status, corps) = lire(&app, &format!("/api/v1/zones/{zid}/album-en-cours")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{corps}");
    assert_eq!(corps["reason"], "service_injoignable");
}

// ─── Web#1923, #1924, #1926 : un titre Bandcamp rejoué SEUL ─────────────────
//
// Rejoué depuis l'historique, un favori ou « Lire », un titre Bandcamp entre en
// file par `source` + `source_id`, sans référence d'album : la ligne ne savait
// rien, et la route rendait encore 404. Tune avait pourtant rangé la page de
// l'album à la première écoute (`listen_history.album_ref`, migration 114).

/// La même piste sous une AUTRE signature : l'URL de flux change d'un jour à
/// l'autre, l'identité de la piste (son chemin) non.
fn signee(ts: u64) -> String {
    format!("{PISTE_BC}?p=0&ts={ts}&t=abcd&token={ts}_x")
}

/// Une écoute passée de la piste Bandcamp, avec la page de son album.
fn ecoute_passee(state: &AppState, source_id: &str) {
    tune_core::db::history_repo::HistoryRepo::with_backend(state.backend.clone())
        .record(&tune_core::db::history_repo::ListenRecord {
            title: "Meet Her At The Love Parade".into(),
            source: "bandcamp".into(),
            source_id: Some(source_id.into()),
            duration_ms: 300_000,
            album_ref: Some(PAGE.into()),
            ..Default::default()
        })
        .expect("écoute passée");
}

#[tokio::test]
async fn un_titre_bandcamp_rejoue_seul_retrouve_l_album_de_son_historique() {
    let (app, state) = app_et_etat().await;
    let zid = zone(&state, "atelier");
    ecoute_passee(&state, &signee(1_790_000_000));
    let piste = signee(1_791_000_000);
    enfiler(&state, zid, &[de_service("bandcamp", &piste, None)]);
    jouer(&state, zid, "bandcamp", &piste).await;

    let (status, corps) = lire(&app, &format!("/api/v1/zones/{zid}/album-en-cours")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "un titre Bandcamp dont Tune connaît la page d'album doit la rendre : {corps}"
    );
    assert_eq!(corps["kind"], "streaming");
    assert_eq!(corps["service"], "bandcamp");
    assert_eq!(corps["album_id"], PAGE);
    assert_eq!(corps["origin"], "stored_reference");
}

#[tokio::test]
async fn un_titre_bandcamp_enfile_seul_porte_l_album_connu() {
    let (app, state) = app_et_etat().await;
    let zid = zone(&state, "veranda");
    ecoute_passee(&state, &signee(1_790_000_000));
    let piste = signee(1_791_000_000);

    let corps_requete = serde_json::json!({
        "source": "bandcamp",
        "source_id": piste,
        "title": "Meet Her At The Love Parade",
        "artist_name": "Framewerk",
        "duration_ms": 300_000,
    });
    let resp = app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/zones/{zid}/queue/add"))
                .header("content-type", "application/json")
                .body(Body::from(corps_requete.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "ajout en file : {}",
        resp.status()
    );

    let (status, corps) = lire(&app, &format!("/api/v1/zones/{zid}/queue")).await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    let lignes = corps["tracks"].as_array().expect("tracks");
    assert_eq!(lignes.len(), 1, "{corps}");
    assert_eq!(
        lignes[0]["album_id_service"], PAGE,
        "la ligne Bandcamp doit désigner la page de son album : {corps}"
    );
}

/// Contre-épreuve : la référence d'une AUTRE piste Bandcamp ne se prête pas.
#[tokio::test]
async fn la_reference_d_une_autre_piste_ne_se_prete_pas() {
    let (app, state) = app_et_etat().await;
    let zid = zone(&state, "grenier");
    ecoute_passee(&state, "https://t4.bcbits.com/stream/abc/mp3-128/999?ts=1");
    let piste = signee(1_791_000_000);
    enfiler(&state, zid, &[de_service("bandcamp", &piste, None)]);
    jouer(&state, zid, "bandcamp", &piste).await;

    let (status, corps) = lire(&app, &format!("/api/v1/zones/{zid}/album-en-cours")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{corps}");
}
