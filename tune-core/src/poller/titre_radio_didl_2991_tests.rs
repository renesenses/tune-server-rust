//! #2991 — de bout en bout dans le sondeur : un changement de titre radio lu
//! dans le flux ICY d'une fausse station doit, sur une zone RÉGLÉE, produire
//! un `SetNextAVTransportURI` portant le titre et la pochette vers le faux
//! renderer DLNA de la zone. Zone non réglée : aucune action SOAP.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::dlna::titre_radio_2991::cle_de_zone;
use crate::outputs::dlna::titre_radio_2991::tests::{
    FauxRenderer, URL_RADIO, faux_renderer, poser_la_radio,
};
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlayState, PlaybackManager, ZoneState};
use crate::poller::PositionPoller;

const DEVICE: &str = "uuid:2991-rs250a";
const POCHETTE: &str = "http://img.example.test/covers/l/9039.jpg";

/// Une fausse station ICY : `icy-metaint: 16`, puis un bloc qui annonce le
/// morceau et sa pochette dans `StreamUrl`.
async fn fausse_station() -> (String, tokio::task::JoinHandle<()>) {
    let meta = format!("StreamTitle='Lyle Lovett - Friend Of The Devil';StreamUrl='{POCHETTE}';");
    let blocs = meta.len().div_ceil(16);
    let mut corps = vec![0u8; 16];
    corps.push(blocs as u8);
    let mut charge = meta.into_bytes();
    charge.resize(blocs * 16, 0);
    corps.extend_from_slice(&charge);
    corps.extend_from_slice(&[0u8; 16]);
    let app = axum::Router::new().route(
        "/station",
        axum::routing::get(move || {
            let corps = corps.clone();
            async move {
                (
                    [("icy-metaint", "16"), ("content-type", "audio/mpeg")],
                    corps,
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/station", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, task)
}

struct Banc {
    poller: PositionPoller,
    db: Arc<dyn DbBackend>,
    zone_id: i64,
    renderer: FauxRenderer,
}

async fn banc() -> Banc {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("Rose", Some("dlna"), Some(DEVICE))
        .unwrap();
    let renderer = faux_renderer(false).await;
    let sortie = renderer.sortie(DEVICE);
    // La radio est posée comme l'orchestrateur la pose : la sortie retient
    // l'URI acceptée.
    poser_la_radio(&sortie).await;
    let playback = Arc::new(PlaybackManager::new());
    let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
    outputs.lock().await.register(Box::new(sortie));
    let services = Arc::new(Mutex::new(crate::streaming::ServiceRegistry::new()));
    let orchestrator = Arc::new(PlaybackOrchestrator::new(
        db.clone(),
        playback.clone(),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        services,
        outputs.clone(),
        None,
    ));
    let poller = PositionPoller::new(
        orchestrator,
        playback,
        outputs,
        db.clone(),
        Arc::new(Mutex::new(HashMap::new())),
    );
    Banc {
        poller,
        db,
        zone_id,
        renderer,
    }
}

fn zone_en_radio(url_station: &str) -> ZoneState {
    ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Station test 2991".into(),
            album_title: Some("Station test 2991".into()),
            source: "radio".into(),
            source_id: Some(url_station.to_string()),
            stream_id: crate::http::streamer::stream_id_de_l_url(URL_RADIO).map(str::to_string),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Le relai part dans une tâche : on l'attend, borné.
async fn attendre_set_next(r: &FauxRenderer, attente: Duration) -> usize {
    let debut = std::time::Instant::now();
    loop {
        let n = r.actions("SetNextAVTransportURI").len();
        if n > 0 || debut.elapsed() >= attente {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn zone_reglee_le_changement_de_titre_radio_part_en_didl_avec_titre_et_pochette() {
    let b = banc().await;
    SettingsRepo::with_backend(b.db.clone())
        .set(&cle_de_zone(b.zone_id), "next")
        .unwrap();
    let (url_station, _station) = fausse_station().await;
    let zone = zone_en_radio(&url_station);

    b.poller.refresh_radio_metadata(b.zone_id, &zone).await;

    assert_eq!(
        attendre_set_next(&b.renderer, Duration::from_secs(5)).await,
        1,
        "zone réglée : le changement de titre doit produire UN SetNextAVTransportURI ; reçu : {:?}",
        b.renderer.recues.lock().unwrap()
    );
    let corps = b.renderer.actions("SetNextAVTransportURI")[0].corps.clone();
    assert!(
        corps.contains(&format!("<NextURI>{URL_RADIO}</NextURI>")),
        "la MÊME URI de flux doit être rejouée : {corps}"
    );
    assert!(
        corps.contains("Friend Of The Devil"),
        "titre absent : {corps}"
    );
    assert!(corps.contains("Lyle Lovett"), "artiste absent : {corps}");
    assert!(corps.contains(POCHETTE), "pochette absente : {corps}");
}

#[tokio::test]
async fn zone_non_reglee_aucun_envoi_didl() {
    let b = banc().await;
    let (url_station, _station) = fausse_station().await;
    let zone = zone_en_radio(&url_station);
    let avant = b.renderer.total();

    b.poller.refresh_radio_metadata(b.zone_id, &zone).await;

    assert_eq!(
        attendre_set_next(&b.renderer, Duration::from_millis(800)).await,
        0,
        "zone non réglée : aucun DIDL ne doit partir"
    );
    assert_eq!(
        b.renderer.total(),
        avant,
        "zone non réglée : aucune action SOAP de plus ; reçu : {:?}",
        b.renderer.recues.lock().unwrap()
    );
}
