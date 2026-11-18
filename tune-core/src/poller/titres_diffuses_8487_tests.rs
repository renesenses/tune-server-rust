//! #8487 — de bout en bout dans le sondeur, sans aucune vraie radio : une
//! fausse station ICY locale annonce des titres successifs ; chaque morceau
//! est noté UNE fois sous la ligne de la station (#7965), le bruit et les
//! relectures du même titre ne le sont pas, et l'historique garde une seule
//! ligne de station.
use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::history_repo::HistoryRepo;
use crate::db::radio_titres_diffuses::pour_ecoutes;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlayState, PlaybackManager, ZoneState};
use crate::poller::PositionPoller;

// Un nom neutre : « BBC », « FIP », « Radio Paradise »… enverraient le
// sondeur chez le vrai diffuseur (`radio_metadata`), pas sur la fausse station.
const STATION: &str = "Radio Témoin 8487";

/// Une fausse station ICY (`icy-metaint: 16`) dont le `StreamTitle` se
/// change en cours d'essai.
async fn fausse_station(titre: Arc<std::sync::Mutex<String>>) -> String {
    let app = axum::Router::new().route(
        "/station",
        axum::routing::get(move || {
            let titre = titre.clone();
            async move {
                let meta = format!("StreamTitle='{}';", titre.lock().unwrap());
                let blocs = meta.len().div_ceil(16);
                let mut corps = vec![0u8; 16];
                corps.push(blocs as u8);
                let mut charge = meta.into_bytes();
                charge.resize(blocs * 16, 0);
                corps.extend_from_slice(&charge);
                corps.extend_from_slice(&[0u8; 16]);
                (
                    [("icy-metaint", "16"), ("content-type", "audio/mpeg")],
                    corps,
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/station", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

fn sondeur(db: Arc<dyn DbBackend>) -> PositionPoller {
    let playback = Arc::new(PlaybackManager::new());
    let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
    let orchestrator = Arc::new(PlaybackOrchestrator::new(
        db.clone(),
        playback.clone(),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(Mutex::new(crate::streaming::ServiceRegistry::new())),
        outputs.clone(),
        None,
    ));
    PositionPoller::new(
        orchestrator,
        playback,
        outputs,
        db,
        Arc::new(Mutex::new(HashMap::new())),
    )
}

/// L'état de zone tel que le laisse le tour précédent : `now_playing` porte
/// déjà le titre en cours ; seul le flux dit s'il a changé.
fn zone_en_radio(url: &str, titre: &str) -> ZoneState {
    ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: titre.into(),
            album_title: Some(STATION.into()),
            source: "radio".into(),
            source_id: Some(url.to_string()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[tokio::test]
async fn chaque_morceau_diffuse_est_note_une_fois_sous_sa_station_8487() {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let zone = ZoneRepo::with_backend(db.clone())
        .create("Parents", None, None)
        .unwrap();
    let titre = Arc::new(std::sync::Mutex::new(
        "King Gizzard & the Lizard Wizard - Rattlesnake".to_string(),
    ));
    let url = fausse_station(titre.clone()).await;
    let poller = sondeur(db.clone());

    // Avant que la station soit notée (zone navigateur pas encore
    // confirmée) : rien.
    poller
        .refresh_radio_metadata(zone, &zone_en_radio(&url, STATION))
        .await;
    let station = db
        .execute_returning_id(
            "INSERT INTO listen_history (title, source, source_id, zone_id, context_type, \
             context_source, context_title) VALUES (?, 'radio', ?, ?, 'radio', 'radio', ?)",
            &[&STATION as &dyn ToSqlValue, &url, &zone, &STATION],
        )
        .unwrap();

    // Le tour suivant note le titre ; les relectures du même titre, non.
    for _ in 0..3 {
        poller
            .refresh_radio_metadata(zone, &zone_en_radio(&url, "Rattlesnake"))
            .await;
    }
    *titre.lock().unwrap() = STATION.into();
    poller
        .refresh_radio_metadata(zone, &zone_en_radio(&url, "Rattlesnake"))
        .await;
    *titre.lock().unwrap() = "Fontaines D.C. - Starburster".into();
    poller
        .refresh_radio_metadata(zone, &zone_en_radio(&url, STATION))
        .await;
    poller
        .refresh_radio_metadata(zone, &zone_en_radio(&url, "Starburster"))
        .await;

    let notes = pour_ecoutes(&db, &[station]).unwrap();
    let notes: Vec<(&str, Option<&str>)> = notes
        .get(&station)
        .map(|v| {
            v.iter()
                .map(|t| (t.titre.as_str(), t.artiste.as_deref()))
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(
        notes,
        vec![
            ("Starburster", Some("Fontaines D.C.")),
            ("Rattlesnake", Some("King Gizzard & the Lizard Wizard")),
        ],
        "deux morceaux diffusés, chacun noté une fois sous la station, le nom de la station écarté"
    );
    let (liste, total) = HistoryRepo::with_backend(db.clone())
        .recent_paginated(50, 0)
        .unwrap();
    assert_eq!(
        (liste.len(), total),
        (1, 1),
        "l'historique garde UNE ligne de station : {liste:?}"
    );
}
