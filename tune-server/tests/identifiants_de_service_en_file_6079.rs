//! #6079 (FabienM, Tune Remote Android) — l'album et l'artiste CHEZ LE SERVICE
//! d'un titre de la file.
//!
//! Depuis la file, « Aller à l'album » et « Aller à l'artiste » d'un titre
//! Qobuz ouvraient un mauvais album ou un mauvais artiste. Deux trous côté
//! serveur, sur `GET /zones/{id}/queue` :
//!
//! - `album_id_service` valait `null` pour un titre de service mis SEUL en
//!   file : la référence d'album (`album_ref`) n'était rangée que pour
//!   Bandcamp ;
//! - `artist_id_service` valait `null` en dur : `queue_items` ne gardait pas
//!   l'artiste du service (migration 122 / PG 086, `artist_ref`).
//!
//! Ces témoins n'attaquent que les routes publiques, montées par le vrai
//! routeur, avec des services simulés : ils compilent sur le code d'avant le
//! correctif, et y ROUGISSENT (contre-épreuve).
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! par sa strophe `[[test]]` dans `tune-server/Cargo.toml`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::TuneError;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::models::{Album, Artist, Track};
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};
use tune_server::state::AppState;

/// L'album de service dont les pistes `p1`, `p2`, `p3` font partie.
const ALBUM: &str = "alb-6079";

/// Un service simulé, Qobuz ou Tidal. Sa fiche d'un titre nomme l'album
/// `album` et l'artiste `artiste` ; ses pistes d'album portent chacune leur
/// artiste (`art-p1`, …). `fiches` compte les `get_track` reçus.
struct ServiceSimule {
    nom: &'static str,
    album: Option<&'static str>,
    artiste: Option<&'static str>,
    fiches: Arc<AtomicUsize>,
}

fn piste(id: &str, n: u32, album: Option<&str>, artiste: Option<&str>) -> StreamTrack {
    StreamTrack {
        id: id.into(),
        title: format!("Titre {n}"),
        artist: "Artiste".into(),
        album: Some("Album".into()),
        album_id: album.map(str::to_string),
        duration_ms: 200_000,
        cover_path: None,
        track_number: Some(n),
        disc_number: None,
        explicit: false,
        disponible: None,
        quality: None,
        isrc: None,
        composer: None,
        artist_id: artiste.map(str::to_string),
    }
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
        self.nom
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
        self.fiches.fetch_add(1, Ordering::SeqCst);
        Ok(piste(id, 1, self.album, self.artiste))
    }
    async fn get_track_url(&self, _t: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album(&self, _a: &str) -> Result<StreamAlbum, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album_tracks(&self, a: &str) -> Result<Vec<StreamTrack>, TuneError> {
        if a != ALBUM {
            return Err("album inconnu".into());
        }
        Ok((1..=3)
            .map(|n| {
                let id = format!("p{n}");
                let artiste = format!("art-p{n}");
                piste(&id, n, Some(ALBUM), Some(artiste.as_str()))
            })
            .collect())
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

/// Le banc : Qobuz (`q-alb-7` / `q-art-9`) et Tidal (`t-alb-svc` /
/// `t-art-svc`), et le compte des fiches demandées à chacun.
struct Banc {
    app: axum::Router,
    state: AppState,
    fiches_qobuz: Arc<AtomicUsize>,
    fiches_tidal: Arc<AtomicUsize>,
}

async fn banc() -> Banc {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let fiches_qobuz = Arc::new(AtomicUsize::new(0));
    let fiches_tidal = Arc::new(AtomicUsize::new(0));
    {
        let mut reg = state.services.lock().await;
        reg.register(Box::new(ServiceSimule {
            nom: "qobuz",
            album: Some("q-alb-7"),
            artiste: Some("q-art-9"),
            fiches: fiches_qobuz.clone(),
        }));
        reg.register(Box::new(ServiceSimule {
            nom: "tidal",
            album: Some("t-alb-svc"),
            artiste: Some("t-art-svc"),
            fiches: fiches_tidal.clone(),
        }));
    }
    let app = tune_server::routes::router(state.clone());
    Banc {
        app,
        state,
        fiches_qobuz,
        fiches_tidal,
    }
}

fn zone(state: &AppState, nom: &str) -> i64 {
    ZoneRepo::with_backend(state.backend.clone())
        .create(nom, Some("browser"), None)
        .expect("creation de zone")
}

async fn appeler(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn poster(app: &axum::Router, chemin: &str, corps: &Value) -> (StatusCode, Value) {
    appeler(
        app,
        Request::post(chemin)
            .header("content-type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await
}

/// Les lignes de `GET /zones/{id}/queue`.
async fn file(app: &axum::Router, zid: i64) -> Vec<Value> {
    let (status, corps) = appeler(
        app,
        Request::get(format!("/api/v1/zones/{zid}/queue"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    corps["tracks"].as_array().expect("tracks").clone()
}

/// Ce qu'envoient la recherche, les favoris et l'historique : le titre et
/// l'artiste en TEXTE, aucun identifiant d'album ni d'artiste.
fn titre_seul(source: &str, source_id: &str) -> Value {
    json!({
        "source": source,
        "source_id": source_id,
        "title": "Blue in Green",
        "artist_name": "Miles Davis",
        "album_title": "Kind of Blue",
        "duration_ms": 300_000,
    })
}

/// Le cas de FabienM : un titre Qobuz ajouté seul depuis une recherche. Le
/// client ne donne aucun identifiant ; la ligne prend ceux du service.
#[tokio::test]
async fn un_titre_qobuz_enfile_seul_designe_son_album_et_son_artiste_chez_qobuz() {
    let b = banc().await;
    let zid = zone(&b.state, "salon");
    let (status, corps) = poster(
        &b.app,
        &format!("/api/v1/zones/{zid}/queue/add"),
        &titre_seul("qobuz", "q-1"),
    )
    .await;
    assert!(status.is_success(), "ajout en file : {status} {corps}");

    let lignes = file(&b.app, zid).await;
    assert_eq!(lignes.len(), 1, "{lignes:?}");
    assert_eq!(
        lignes[0]["album_id_service"], "q-alb-7",
        "un titre Qobuz seul doit désigner son album CHEZ QOBUZ : {}",
        lignes[0]
    );
    assert_eq!(
        lignes[0]["artist_id_service"], "q-art-9",
        "un titre Qobuz seul doit désigner son artiste CHEZ QOBUZ : {}",
        lignes[0]
    );
    assert_eq!(b.fiches_qobuz.load(Ordering::SeqCst), 1, "une seule fiche");
}

/// Tidal, quand le client SAIT les identifiants (résultat de recherche) : ils
/// priment, et aucune requête ne part vers le service.
#[tokio::test]
async fn un_titre_tidal_garde_les_identifiants_du_client_sans_requete_de_plus() {
    let b = banc().await;
    let zid = zone(&b.state, "bureau");
    let mut corps = titre_seul("tidal", "t-1");
    corps["album_id_service"] = json!("t-alb-1");
    corps["artist_id_service"] = json!("t-art-2");
    let (status, rep) = poster(&b.app, &format!("/api/v1/zones/{zid}/queue/add"), &corps).await;
    assert!(status.is_success(), "ajout en file : {status} {rep}");

    let lignes = file(&b.app, zid).await;
    assert_eq!(lignes[0]["album_id_service"], "t-alb-1", "{}", lignes[0]);
    assert_eq!(lignes[0]["artist_id_service"], "t-art-2", "{}", lignes[0]);
    assert_eq!(
        b.fiches_tidal.load(Ordering::SeqCst),
        0,
        "le client a tout donné : aucune fiche demandée à Tidal"
    );
}

/// Tidal, quand le client ne sait rien : la ligne prend les identifiants de
/// Tidal, comme pour Qobuz.
#[tokio::test]
async fn un_titre_tidal_enfile_seul_sans_identifiants_les_prend_chez_tidal() {
    let b = banc().await;
    let zid = zone(&b.state, "atelier");
    let (status, rep) = poster(
        &b.app,
        &format!("/api/v1/zones/{zid}/queue/add"),
        &titre_seul("tidal", "t-1"),
    )
    .await;
    assert!(status.is_success(), "ajout en file : {status} {rep}");
    let lignes = file(&b.app, zid).await;
    assert_eq!(lignes[0]["album_id_service"], "t-alb-svc", "{}", lignes[0]);
    assert_eq!(lignes[0]["artist_id_service"], "t-art-svc", "{}", lignes[0]);
}

/// Un LOT (`tracks[]`, « Lire ensuite » d'une sélection) garde les
/// identifiants que le client donne pour chaque ligne, sans requête par ligne.
#[tokio::test]
async fn un_lot_qobuz_et_tidal_garde_les_identifiants_de_chaque_ligne() {
    let b = banc().await;
    let zid = zone(&b.state, "cuisine");
    let mut q = titre_seul("qobuz", "q-1");
    q["album_id_service"] = json!("q-alb-1");
    q["artist_id_service"] = json!("q-art-1");
    let mut t = titre_seul("tidal", "t-2");
    t["album_id_service"] = json!("t-alb-2");
    t["artist_id_service"] = json!("t-art-2");
    let (status, rep) = poster(
        &b.app,
        &format!("/api/v1/zones/{zid}/queue/add"),
        &json!({ "tracks": [q, t] }),
    )
    .await;
    assert!(status.is_success(), "ajout en file : {status} {rep}");

    let lignes = file(&b.app, zid).await;
    let paires: Vec<(Value, Value)> = lignes
        .iter()
        .map(|l| {
            (
                l["album_id_service"].clone(),
                l["artist_id_service"].clone(),
            )
        })
        .collect();
    assert_eq!(
        paires,
        vec![
            (json!("q-alb-1"), json!("q-art-1")),
            (json!("t-alb-2"), json!("t-art-2")),
        ]
    );
    assert_eq!(b.fiches_qobuz.load(Ordering::SeqCst), 0);
    assert_eq!(b.fiches_tidal.load(Ordering::SeqCst), 0);
}

/// Un titre Tidal LU seul joue son album (#5372) : chaque ligne de la file
/// désigne son artiste chez Tidal — déjà dans la réponse du service. La file
/// est écrite avant la lecture, quoi qu'il advienne de celle-ci sur ce banc.
#[tokio::test]
async fn un_titre_tidal_lu_seul_met_son_album_en_file_avec_l_artiste_de_chaque_piste() {
    let b = banc().await;
    {
        // Ce Tidal-là range ses titres dans `ALBUM`.
        let mut reg = b.state.services.lock().await;
        reg.register(Box::new(ServiceSimule {
            nom: "tidal",
            album: Some(ALBUM),
            artiste: Some("art-p2"),
            fiches: b.fiches_tidal.clone(),
        }));
    }
    let zid = zone(&b.state, "veranda");
    let _ = poster(
        &b.app,
        &format!("/api/v1/zones/{zid}/play"),
        &titre_seul("tidal", "p2"),
    )
    .await;

    let lignes = file(&b.app, zid).await;
    let paires: Vec<(Value, Value)> = lignes
        .iter()
        .map(|l| {
            (
                l["album_id_service"].clone(),
                l["artist_id_service"].clone(),
            )
        })
        .collect();
    assert_eq!(
        paires,
        vec![
            (json!(ALBUM), json!("art-p1")),
            (json!(ALBUM), json!("art-p2")),
            (json!(ALBUM), json!("art-p3")),
        ],
        "chaque piste de l'album doit désigner son artiste chez Tidal"
    );
}

/// Contre-épreuve : un identifiant de client inexploitable (blanc dedans)
/// n'entre pas ; la ligne retombe sur celui du service.
#[tokio::test]
async fn un_identifiant_de_client_inexploitable_n_entre_pas() {
    let b = banc().await;
    let zid = zone(&b.state, "garage");
    let mut corps = titre_seul("qobuz", "q-1");
    corps["album_id_service"] = json!("q alb");
    corps["artist_id_service"] = json!("   ");
    let (status, rep) = poster(&b.app, &format!("/api/v1/zones/{zid}/queue/add"), &corps).await;
    assert!(status.is_success(), "ajout en file : {status} {rep}");
    let lignes = file(&b.app, zid).await;
    assert_eq!(lignes[0]["album_id_service"], "q-alb-7", "{}", lignes[0]);
    assert_eq!(lignes[0]["artist_id_service"], "q-art-9", "{}", lignes[0]);
}

/// Un titre LOCAL est inchangé : ses entiers de bibliothèque, et aucun
/// identifiant de service inventé.
#[tokio::test]
async fn un_titre_local_reste_inchange() {
    let b = banc().await;
    let zid = zone(&b.state, "chambre");
    let artist_id = ArtistRepo::with_backend(b.state.backend.clone())
        .create(&Artist::new("Miles Davis".into()))
        .expect("creation d'artiste");
    let album_id = AlbumRepo::with_backend(b.state.backend.clone())
        .create(&Album::new("Kind of Blue".into()))
        .expect("creation d'album");
    let mut t = Track::new("So What".into());
    t.album_id = Some(album_id);
    t.artist_id = Some(artist_id);
    t.format = Some("flac".into());
    t.file_path = Some("/musique/so-what.flac".into());
    t.duration_ms = 240_000;
    let track_id = TrackRepo::with_backend(b.state.backend.clone())
        .create(&t)
        .expect("insertion de piste");

    let (status, rep) = poster(
        &b.app,
        &format!("/api/v1/zones/{zid}/queue/add"),
        &json!({ "track_id": track_id }),
    )
    .await;
    assert!(status.is_success(), "ajout en file : {status} {rep}");
    let lignes = file(&b.app, zid).await;
    assert_eq!(lignes.len(), 1);
    assert_eq!(lignes[0]["album_id"], json!(album_id));
    assert_eq!(lignes[0]["artist_id"], json!(artist_id));
    assert_eq!(lignes[0]["album_id_service"], Value::Null);
    assert_eq!(lignes[0]["artist_id_service"], Value::Null);
    assert_eq!(b.fiches_qobuz.load(Ordering::SeqCst), 0);
    assert_eq!(b.fiches_tidal.load(Ordering::SeqCst), 0);
}
