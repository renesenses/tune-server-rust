//! Les identifiants qu'il faut aux clients pour des liens EXACTS — #8327.
//!
//! Sans eux, les apps retrouvaient album et artiste par leur NOM, et un
//! homonyme menait ailleurs. Ce banc garde, route par route, que la clé est
//! là, qu'elle porte la bonne valeur, et qu'elle est absente (ou `null`)
//! proprement quand rien n'est connu :
//!
//! - `GET /zones/{id}` : `current_track.album_id_service`,
//!   `current_track.artist_id_service`, `composer`, `label`, `artists` ;
//! - `GET /history/` : `artist_id` ;
//! - `GET /history/top-tracks` : `album_id`, `artist_id` ;
//! - `GET /history/dashboard` : `top_artists[].artist_id`,
//!   `top_tracks[].album_id`, `top_tracks[].artist_id`.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! par sa strophe `[[test]]` dans `tune-server/Cargo.toml`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::history_repo::{HistoryRepo, ListenRecord};
use tune_core::db::models::{Album, Artist, Track};
use tune_core::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::playback::NowPlaying;
use tune_server::state::AppState;

async fn get(state: &AppState, uri: &str) -> Value {
    let app = tune_server::routes::router(state.clone());
    let resp = app
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn etat() -> (AppState, i64) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zid = ZoneRepo::with_backend(state.backend.clone())
        .create("salon", Some("mock"), Some("sortie-salon"))
        .expect("creation de zone");
    (state, zid)
}

fn artiste(state: &AppState, nom: &str) -> i64 {
    ArtistRepo::with_backend(state.backend.clone())
        .create(&Artist::new(nom.into()))
        .expect("creation d'artiste")
}

/// Une piste locale « Miles Davis & John Coltrane », sur un album, avec
/// compositeur et label. Rend `(track_id, album_id, artist_id)`.
fn piste_locale(state: &AppState, avec_balises: bool) -> (i64, i64, i64) {
    let duo = artiste(state, "Miles Davis & John Coltrane");
    let mut album = Album::new("Kind of Blue".into());
    album.artist_id = Some(duo);
    let album_id = AlbumRepo::with_backend(state.backend.clone())
        .create(&album)
        .expect("creation d'album");
    let mut t = Track::new("So What".into());
    t.artist_id = Some(duo);
    t.album_id = Some(album_id);
    t.format = Some("flac".into());
    t.file_path = Some("/musique/so-what.flac".into());
    t.duration_ms = 240_000;
    if avec_balises {
        t.composer = Some("Miles Davis".into());
        t.label = Some("Columbia".into());
    }
    let id = TrackRepo::with_backend(state.backend.clone())
        .create(&t)
        .expect("insertion de piste");
    (id, album_id, duo)
}

#[tokio::test]
async fn zone_piste_de_service_porte_ses_identifiants_de_service() {
    let (state, zid) = etat();
    let file = PlayQueueRepo::with_backend(state.backend.clone());
    file.insert_at(
        zid,
        &[QueueInput::Streaming {
            source: "qobuz".into(),
            source_id: "q-1".into(),
            title: "Blue in Green".into(),
            artist: "Miles Davis".into(),
            album: Some("Kind of Blue".into()),
            cover_url: None,
            duration_ms: 300_000,
            track_number: Some(3),
            disc_number: None,
            album_ref: Some("q-alb-7".into()),
            artist_ref: Some("q-art-9".into()),
        }],
        None,
    )
    .expect("mise en file");
    file.set_current_pos(zid, 0).expect("ligne courante");
    state
        .playback
        .play(
            zid,
            NowPlaying {
                title: "Blue in Green".into(),
                artist_name: Some("Miles Davis".into()),
                album_title: Some("Kind of Blue".into()),
                source: "qobuz".into(),
                source_id: Some("q-1".into()),
                duration_ms: 300_000,
                ..Default::default()
            },
        )
        .await;

    for uri in [format!("/api/v1/zones/{zid}"), "/api/v1/zones".to_string()] {
        let corps = get(&state, &uri).await;
        let zone = if corps.is_array() {
            corps
                .as_array()
                .unwrap()
                .iter()
                .find(|z| z["id"] == json!(zid))
                .cloned()
                .expect("zone listee")
        } else {
            corps
        };
        let piste = &zone["current_track"];
        assert_eq!(
            piste["album_id_service"], "q-alb-7",
            "{uri} : l'album CHEZ LE SERVICE de la ligne qui joue : {piste}"
        );
        assert_eq!(piste["artist_id_service"], "q-art-9", "{uri} : {piste}");
        assert_eq!(
            piste["artists"],
            json!([{ "name": "Miles Davis", "id_service": "q-art-9", "role": "main" }]),
            "{uri} : {piste}"
        );
        // Une piste de service n'a ni compositeur ni label de bibliothèque :
        // les clés sont absentes, pas vides.
        assert!(piste.get("composer").is_none(), "{uri} : {piste}");
        assert!(piste.get("label").is_none(), "{uri} : {piste}");
    }
}

#[tokio::test]
async fn zone_piste_locale_porte_compositeur_label_et_artistes_confirmes() {
    let (state, zid) = etat();
    let (track_id, album_id, duo) = piste_locale(&state, true);
    let miles = artiste(&state, "Miles Davis");
    let trane = artiste(&state, "John Coltrane");
    let track = TrackRepo::with_backend(state.backend.clone())
        .get(track_id)
        .unwrap()
        .unwrap();
    state
        .playback
        .play(zid, NowPlaying::from_track(&track))
        .await;

    let zone = get(&state, &format!("/api/v1/zones/{zid}")).await;
    let piste = &zone["current_track"];
    assert_eq!(piste["album_id"], json!(album_id), "{piste}");
    assert_eq!(piste["artist_id"], json!(duo), "{piste}");
    assert_eq!(piste["composer"], "Miles Davis", "{piste}");
    assert_eq!(piste["label"], "Columbia", "{piste}");
    // Piste locale : aucun identifiant de service inventé.
    assert_eq!(piste["album_id_service"], Value::Null, "{piste}");
    assert_eq!(piste["artist_id_service"], Value::Null, "{piste}");
    assert!(piste.get("album_id_service").is_some(), "clé présente");
    // « A & B » coupé parce que la bibliothèque connaît A ET B, chacun avec
    // SON identifiant — pas celui du duo.
    assert_eq!(
        piste["artists"],
        json!([
            { "name": "Miles Davis", "id": miles, "role": "main" },
            { "name": "John Coltrane", "id": trane, "role": "main" },
        ]),
        "{piste}"
    );
}

#[tokio::test]
async fn zone_piste_locale_sans_balises_ni_artistes_connus() {
    let (state, zid) = etat();
    let (track_id, _album_id, duo) = piste_locale(&state, false);
    let track = TrackRepo::with_backend(state.backend.clone())
        .get(track_id)
        .unwrap()
        .unwrap();
    state
        .playback
        .play(zid, NowPlaying::from_track(&track))
        .await;
    let zone = get(&state, &format!("/api/v1/zones/{zid}")).await;
    let piste = &zone["current_track"];
    assert!(piste.get("composer").is_none(), "{piste}");
    assert!(piste.get("label").is_none(), "{piste}");
    // Ni « Miles Davis » ni « John Coltrane » en bibliothèque : le crédit
    // reste entier, avec l'identifiant de la piste.
    assert_eq!(
        piste["artists"],
        json!([{ "name": "Miles Davis & John Coltrane", "id": duo, "role": "main" }]),
        "{piste}"
    );
}

#[tokio::test]
async fn historique_et_classements_portent_album_et_artiste() {
    let (state, zid) = etat();
    let (track_id, album_id, duo) = piste_locale(&state, false);
    let historique = HistoryRepo::with_backend(state.backend.clone());
    for _ in 0..2 {
        historique
            .record(&ListenRecord {
                track_id: Some(track_id),
                title: "So What".into(),
                artist_name: Some("Miles Davis & John Coltrane".into()),
                album_title: Some("Kind of Blue".into()),
                source: "local".into(),
                album_id: Some(album_id),
                duration_ms: 240_000,
                zone_id: Some(zid),
                ..Default::default()
            })
            .expect("ecoute");
    }
    // Une écoute de service : pas de fiche de bibliothèque.
    historique
        .record(&ListenRecord {
            title: "Naima".into(),
            artist_name: Some("Inconnu du Service".into()),
            album_title: Some("Giant Steps".into()),
            source: "qobuz".into(),
            source_id: Some("q-5".into()),
            duration_ms: 200_000,
            zone_id: Some(zid),
            ..Default::default()
        })
        .expect("ecoute");

    // GET /history/ : `artist_id` de la piste écoutée.
    let h = get(&state, "/api/v1/history/?limit=10").await;
    let items = h["items"].as_array().expect("items");
    let locale = items
        .iter()
        .find(|i| i["title"] == "So What")
        .expect("ecoute locale");
    assert_eq!(locale["artist_id"], json!(duo), "{locale}");
    let service = items
        .iter()
        .find(|i| i["title"] == "Naima")
        .expect("ecoute de service");
    assert!(
        service.get("artist_id").is_some(),
        "clé présente : {service}"
    );
    assert_eq!(service["artist_id"], Value::Null, "{service}");

    // GET /history/top-tracks : album et artiste de la piste.
    let top = get(&state, "/api/v1/history/top-tracks?limit=10").await;
    let so_what = top
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["title"] == "So What")
        .expect("So What classee");
    assert_eq!(so_what["album_id"], json!(album_id), "{so_what}");
    assert_eq!(so_what["artist_id"], json!(duo), "{so_what}");
    let naima = top
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["title"] == "Naima")
        .expect("Naima classee");
    assert_eq!(naima["album_id"], Value::Null, "{naima}");
    assert_eq!(naima["artist_id"], Value::Null, "{naima}");

    // GET /history/dashboard : classements du tableau de bord.
    let d = get(&state, "/api/v1/history/dashboard?period=all").await;
    let artistes = d["top_artists"].as_array().expect("top_artists");
    let a = artistes
        .iter()
        .find(|a| a["artist_name"] == "Miles Davis & John Coltrane")
        .unwrap_or_else(|| panic!("artiste classe : {d}"));
    assert_eq!(a["artist_id"], json!(duo), "{a}");
    let inconnu = artistes
        .iter()
        .find(|a| a["artist_name"] == "Inconnu du Service")
        .unwrap_or_else(|| panic!("artiste de service classe : {d}"));
    assert!(inconnu.get("artist_id").is_none(), "absent : {inconnu}");
    let pistes = d["top_tracks"].as_array().expect("top_tracks");
    let p = pistes
        .iter()
        .find(|p| p["title"] == "So What")
        .unwrap_or_else(|| panic!("piste classee : {d}"));
    assert_eq!(p["album_id"], json!(album_id), "{p}");
    assert_eq!(p["artist_id"], json!(duo), "{p}");
    let n = pistes
        .iter()
        .find(|p| p["title"] == "Naima")
        .unwrap_or_else(|| panic!("piste de service classee : {d}"));
    assert!(
        n.get("album_id").is_none() && n.get("artist_id").is_none(),
        "{n}"
    );
}

#[test]
fn pistes_et_albums_portent_la_liste_des_artistes() {
    use tune_core::streaming::{StreamAlbum, StreamTrack};
    let piste: StreamTrack = serde_json::from_value(json!({
        "source_id": "t-1", "title": "Get Lucky",
        "artist_name": "Daft Punk feat. Pharrell Williams",
        "album_title": null, "album_id": "a-1", "duration_ms": 1,
        "cover_path": null, "track_number": null, "disc_number": null,
        "explicit": false, "quality": null, "artist_id": null
    }))
    .expect("lecture d'une piste au format d'avant");
    let v = serde_json::to_value(&piste).unwrap();
    assert_eq!(v["source_id"], "t-1", "forme dérivée intacte : {v}");
    assert_eq!(
        v["artists"],
        json!([
            { "name": "Daft Punk", "role": "main" },
            { "name": "Pharrell Williams", "role": "featured" },
        ]),
        "{v}"
    );
    // L'aller-retour JSON reste possible (clé `artists` ignorée en lecture).
    let relue: StreamTrack = serde_json::from_value(v).expect("relecture");
    assert_eq!(relue.artist, piste.artist);

    let album = StreamAlbum {
        id: "a-1".into(),
        title: "Kind of Blue".into(),
        artist: "Miles Davis".into(),
        artist_id: Some("q-art-9".into()),
        ..Default::default()
    };
    let v = serde_json::to_value(&album).unwrap();
    assert_eq!(
        v["artists"],
        json!([{ "name": "Miles Davis", "id_service": "q-art-9", "role": "main" }]),
        "{v}"
    );
    // Sans artiste : clé absente.
    let v = serde_json::to_value(StreamAlbum::default()).unwrap();
    assert!(v.get("artists").is_none(), "{v}");

    let mut t = Track::new("So What".into());
    t.artist_id = Some(4);
    t.artist_name = Some("Miles Davis".into());
    let v = serde_json::to_value(&t).unwrap();
    assert_eq!(
        v["artists"],
        json!([{ "name": "Miles Davis", "id": 4, "role": "main" }]),
        "{v}"
    );
    let relue: Track = serde_json::from_value(v).expect("relecture d'une piste");
    assert_eq!(relue.artist_id, Some(4));
}
