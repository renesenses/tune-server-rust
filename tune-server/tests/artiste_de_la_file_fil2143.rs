//! L'artiste d'un titre de la FILE — fil forum 2143 (FabienM), #5758.
//!
//! Le jumeau de `album_de_la_file_fil2143.rs` (point 8) : le menu d'un titre
//! de la file n'offrait pas « Aller à l'artiste » pour une piste LOCALE, faute
//! d'identifiant d'artiste sur les lignes de `GET /zones/{id}/queue`. Chaque
//! ligne porte désormais deux clefs additives, toujours présentes :
//!
//! - `artist_id` : l'entier de bibliothèque d'une ligne locale ;
//! - `artist_id_service` : l'artiste chez le service. `queue_items` ne le
//!   garde pas (seule la référence d'album l'est, migration 114) : `null`.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! par sa strophe `[[test]]` dans `tune-server/Cargo.toml`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::models::{Artist, Track};
use tune_core::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_server::state::AppState;

#[tokio::test]
async fn chaque_ligne_de_file_designe_son_artiste() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state.clone());
    let zid = ZoneRepo::with_backend(state.backend.clone())
        .create("salon", Some("mock"), Some("sortie-salon"))
        .expect("creation de zone");

    // Une piste locale rangée sous un artiste de la bibliothèque, et une piste
    // locale SANS artiste (fichier mal étiqueté).
    let artist_id = ArtistRepo::with_backend(state.backend.clone())
        .create(&Artist::new("Miles Davis".into()))
        .expect("creation d'artiste");
    let tracks = TrackRepo::with_backend(state.backend.clone());
    let mut t = Track::new("So What".into());
    t.artist_id = Some(artist_id);
    t.format = Some("flac".into());
    t.file_path = Some("/musique/so-what.flac".into());
    t.duration_ms = 240_000;
    let avec_artiste = tracks.create(&t).expect("insertion de piste");
    let mut t = Track::new("Sans nom".into());
    t.file_path = Some("/musique/sans-nom.flac".into());
    t.duration_ms = 100_000;
    let sans_artiste = tracks.create(&t).expect("insertion de piste");

    PlayQueueRepo::with_backend(state.backend.clone())
        .insert_at(
            zid,
            &[
                QueueInput::Local {
                    track_id: avec_artiste,
                },
                QueueInput::Streaming {
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
                },
                QueueInput::Local {
                    track_id: sans_artiste,
                },
            ],
            None,
        )
        .expect("mise en file");

    let resp = app
        .oneshot(
            Request::get(format!("/api/v1/zones/{zid}/queue"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let corps: Value = serde_json::from_slice(&bytes).unwrap();
    let lignes = corps["tracks"].as_array().expect("tracks");
    assert_eq!(lignes.len(), 3, "{corps}");

    // La ligne locale : l'entier de bibliothèque de son artiste.
    assert_eq!(
        lignes[0]["artist_id"],
        Value::from(artist_id),
        "une ligne locale doit porter l'artiste de sa piste (« Aller à l'artiste »)"
    );
    assert_eq!(lignes[0]["artist_id_service"], Value::Null);

    // La ligne de service : aucun entier de bibliothèque inventé, et pas
    // d'artiste de service, que la file ne garde pas.
    assert_eq!(lignes[1]["artist_id"], Value::Null);
    assert_eq!(lignes[1]["artist_id_service"], Value::Null);

    // Une piste locale sans artiste : rien n'est inventé.
    assert_eq!(lignes[2]["artist_id"], Value::Null);

    for (i, l) in lignes.iter().enumerate() {
        assert!(l.get("artist_id").is_some(), "ligne {i} : clef artist_id");
        assert!(
            l.get("artist_id_service").is_some(),
            "ligne {i} : clef artist_id_service"
        );
    }
}
