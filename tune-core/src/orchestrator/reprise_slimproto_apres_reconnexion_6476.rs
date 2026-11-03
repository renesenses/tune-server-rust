//! #6476 — fil 2322 (rc6, AudioLinux) : après une reconnexion de Squeezelite,
//! « Lecture » dans Tune n'envoie que `strm u` (reprise) et jamais `strm s` :
//! le lecteur, qui a perdu son flux avec sa connexion, reste muet.
//!
//! Le banc joue la reconnexion pour de vrai : le VRAI serveur SlimProto
//! (`handle_client`) sur un port local, un faux Squeezelite qui envoie son
//! HELO par TCP, reçoit `strm s` puis `strm p`, coupe sa connexion et revient
//! avec un second HELO. On appelle alors le VRAI `resume` de l'orchestrateur
//! et on lit ce que le lecteur reçoit sur le fil.
//!
//! Cause établie par ce banc : à la reconnexion, le serveur enregistre une
//! sortie NEUVE (état de lecture neuf, aucun média), mais la zone est
//! toujours « en pause » côté Tune. `SlimProtoOutput` ne disait pas qu'elle
//! n'avait plus de flux (`device_released_on_pause()` restait à `false` par
//! défaut) et son `resume()` envoyait `strm u` sans condition : l'orchestrateur
//! reprenait « sur place » un lecteur qui n'avait rien à reprendre.
//!
//! Contre-épreuve : rendre `device_released_on_pause()` à `false` dans
//! `outputs/slimproto.rs` fait tomber
//! `apres_reconnexion_la_lecture_renvoie_le_flux_et_pas_seulement_strm_u`.
use super::PlaybackOrchestrator;
use super::reprise_dlna_position_2095::QobuzDeBanc;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::{AudioStreamer, StreamInfo};
use crate::outputs::registry::OutputRegistry;
use crate::outputs::traits::PlayMedia;
use crate::playback::{NowPlaying, PlayState, PlaybackManager};
use crate::slimproto::SlimProtoServer;
use crate::streaming::ServiceRegistry;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

const MAC: [u8; 6] = [0xa0, 0x36, 0x9f, 0x00, 0x64, 0x76];
const APPAREIL: &str = "slimproto-a0:36:9f:00:64:76";
const NOM: &str = "AudioLinux";
const POSITION_PAUSE_MS: i64 = 40_000;

/// Un faux Squeezelite : connexion de contrôle, HELO à l'ancienne (type,
/// micrologiciel, MAC, canaux Wi-Fi, nom).
async fn helo(adresse: std::net::SocketAddr) -> TcpStream {
    let mut flux = TcpStream::connect(adresse).await.unwrap();
    let mut charge = vec![12u8, 1];
    charge.extend_from_slice(&MAC);
    charge.extend_from_slice(&[0, 0]);
    charge.extend_from_slice(NOM.as_bytes());
    flux.write_all(b"HELO").await.unwrap();
    flux.write_u32(charge.len() as u32).await.unwrap();
    flux.write_all(&charge).await.unwrap();
    flux
}

/// Les commandes `strm` reçues par le lecteur pendant `duree`, battements de
/// cœur (`strm t`) exclus.
async fn commandes_strm(flux: &mut TcpStream, duree: Duration) -> Vec<char> {
    let mut vues = Vec::new();
    let fin = tokio::time::Instant::now() + duree;
    loop {
        let reste = fin.saturating_duration_since(tokio::time::Instant::now());
        if reste.is_zero() {
            return vues;
        }
        let Ok(Ok(longueur)) = tokio::time::timeout(reste, flux.read_u16()).await else {
            return vues;
        };
        let mut trame = vec![0u8; longueur as usize];
        flux.read_exact(&mut trame).await.unwrap();
        if &trame[..4] == b"strm" && trame[4] != b't' {
            vues.push(trame[4] as char);
        }
    }
}

async fn attendre_la_sortie(orch: &PlaybackOrchestrator, presente: bool) {
    for _ in 0..200 {
        if orch.outputs.lock().await.get(APPAREIL).is_some() == presente {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("la sortie SlimProto n'est pas devenue presente={presente}");
}

/// Une zone SlimProto en pause à 0:40 sur une piste Qobuz dont la session de
/// flux est vivante, le lecteur ayant reçu `strm s` puis `strm p`.
async fn zone_slimproto_en_pause() -> (PlaybackOrchestrator, i64, std::net::SocketAddr, TcpStream) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let mut services = ServiceRegistry::new();
    services.register(Box::new(QobuzDeBanc));
    let orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(services)),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );

    let serveur = Arc::new(SlimProtoServer::new_with_state(
        db,
        Arc::new(crate::event_bus::EventBus::new()),
        Arc::clone(&orch.outputs),
        "127.0.0.1".into(),
    ));
    let ecoute = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((flux, pair)) = ecoute.accept().await {
            let serveur = Arc::clone(&serveur);
            tokio::spawn(async move {
                let _ = serveur.handle_client(flux, pair).await;
            });
        }
    });

    let mut lecteur = helo(adresse).await;
    attendre_la_sortie(&orch, true).await;
    let (zone_id, cree) = ZoneRepo::with_backend(orch.db.clone())
        .get_or_create(NOM, Some("slimproto"), APPAREIL)
        .unwrap();
    assert!(!cree, "prémisse : le HELO a créé la zone");

    let sid = orch
        .streamer
        .create_proxy_session(
            StreamInfo {
                format: "flac".into(),
                mime_type: "audio/flac".into(),
                sample_rate: 44_100,
                bit_depth: 16,
                channels: 2,
                ..Default::default()
            },
            "https://127.0.0.1:9/file/6476.flac".into(),
            false,
        )
        .await;
    orch.playback
        .play(
            zone_id,
            NowPlaying {
                title: "Blue Moon Revisited".into(),
                source: "qobuz".into(),
                source_id: Some("6476".into()),
                stream_id: Some(sid.clone()),
                duration_ms: 245_000,
                ..Default::default()
            },
        )
        .await;
    {
        let sortie = { orch.outputs.lock().await.get(APPAREIL) }.unwrap();
        let sortie = sortie.lock().await;
        sortie
            .play_media(&PlayMedia {
                url: &format!("http://127.0.0.1:8888/stream/{sid}.flac"),
                mime_type: "audio/flac",
                ..PlayMedia::default()
            })
            .await
            .unwrap();
        sortie.pause().await.unwrap();
    }
    orch.playback
        .update_position(zone_id, POSITION_PAUSE_MS)
        .await;
    orch.playback.pause(zone_id).await;
    assert_eq!(
        commandes_strm(&mut lecteur, Duration::from_millis(300)).await,
        vec!['s', 'p'],
        "prémisse : le lecteur a reçu le flux puis la pause"
    );
    assert!(orch.streamer.session_alive(&sid).await);
    (orch, zone_id, adresse, lecteur)
}

/// LE défaut : Squeezelite se reconnecte pendant la pause, puis « Lecture ».
/// Le lecteur doit recevoir un `strm s` (le flux à rétablir), pas un `strm u`
/// seul sur un flux qu'il n'a plus.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apres_reconnexion_la_lecture_renvoie_le_flux_et_pas_seulement_strm_u() {
    let (orch, zone_id, adresse, lecteur) = zone_slimproto_en_pause().await;

    // Squeezelite redémarre : la connexion de contrôle tombe, puis revient.
    drop(lecteur);
    attendre_la_sortie(&orch, false).await;
    let mut lecteur = helo(adresse).await;
    attendre_la_sortie(&orch, true).await;

    let resultat = orch.resume(zone_id, Some(APPAREIL)).await;
    let recues = commandes_strm(&mut lecteur, Duration::from_millis(500)).await;

    assert!(
        recues.contains(&'s'),
        "après une reconnexion, Lecture doit renvoyer le flux (strm s) ; \
         reçu {recues:?}, resume = {resultat:?}"
    );
    assert!(resultat.is_ok(), "la reprise aboutit : {resultat:?}");
    assert_eq!(
        orch.playback.get_state(zone_id).await.state,
        PlayState::Playing
    );
}

/// Témoin : sans reconnexion, la reprise reste un `strm u` sur place.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sans_reconnexion_la_reprise_reste_un_strm_u() {
    let (orch, zone_id, _adresse, mut lecteur) = zone_slimproto_en_pause().await;

    orch.resume(zone_id, Some(APPAREIL))
        .await
        .expect("reprise ordinaire");

    assert_eq!(
        commandes_strm(&mut lecteur, Duration::from_millis(500)).await,
        vec!['u']
    );
}
