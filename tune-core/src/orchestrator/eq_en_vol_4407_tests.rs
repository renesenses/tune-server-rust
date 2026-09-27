//! #4407 — un changement d'égaliseur sur une zone DLNA qui joue une RADIO ne
//! refait plus la session UPnP.
//!
//! Le défaut (Jean Valjean, Marantz ND8006, fil 1771) : `apply_eq_change`
//! programmait `replay_programme` ; 500 ms plus tard, `replay_zone_at_position`
//! créait une NOUVELLE session HTTP, reconnectait la station, pré-tamponnait
//! 2,37 s, puis envoyait `Stop` + `SetAVTransportURI` + `Play` au renderer :
//! 2,787 s de silence, pour une radio où il n'y a aucune position à reprendre.
//!
//! Le banc : une vraie station HTTP locale (la fixture MP3), le vrai chemin
//! de lecture (`replay_zone_at_position` → `play` → `resolve_stream` →
//! `servir_la_radio_au_reseau` → décodeur radio), un renderer factice qui
//! compte ce qu'il reçoit. N'emploie que des API qui existaient AVANT le
//! correctif : le même fichier, posé sur la base du lot, doit rougir.
use super::{PlaybackOrchestrator, PorteeDuReglage};
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::outputs::mock::MockOutput;
use crate::playback::NowPlaying;
use std::sync::Arc;

const MP3: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/test.mp3"
));

fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    )
}

/// Une station sans fin sur un port éphémère : chaque connexion reçoit le
/// MP3 de la fixture, sans `Content-Length`, comme une vraie radio.
fn station() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for mut s in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nConnection: close\r\n\r\n"
                );
                let _ = s.write_all(MP3);
                let _ = s.flush();
            });
        }
    });
    format!("http://{addr}/mellow")
}

fn profil_audible(gain: f64) -> crate::audio::eq::EqProfile {
    crate::audio::eq::EqProfile {
        enabled: true,
        bands: vec![crate::audio::eq::EqBandSpec {
            freq: 80.0,
            gain,
            q: 0.71,
            band_type: "low_shelf".into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn ecrire_profil(orch: &PlaybackOrchestrator, zone_id: i64, profil: &crate::audio::eq::EqProfile) {
    let settings = SettingsRepo::with_backend(orch.db.clone());
    settings
        .set(
            &format!("zone_{zone_id}_eq_profile"),
            &serde_json::to_string(profil).unwrap(),
        )
        .unwrap();
    settings.set("plugin_equalizer_installed", "true").unwrap();
}

/// `(play_media, stop)` reçus par le renderer factice. Sur un `DlnaOutput`,
/// `play_media` est la séquence `Stop` → `SetAVTransportURI` → `Play`.
async fn commandes(orch: &PlaybackOrchestrator, device_id: &str) -> (usize, u64) {
    let outputs = orch.outputs.lock().await;
    let out = outputs.get(device_id).expect("sortie enregistrée");
    let guard = out.lock().await;
    let mock = guard.as_any().downcast_ref::<MockOutput>().expect("mock");
    (mock.play_call_count().await, mock.stop_call_count())
}

fn relances_programmees(orch: &PlaybackOrchestrator, zone_id: i64) -> u64 {
    orch.eq_replay_gen
        .lock()
        .unwrap()
        .get(&zone_id)
        .copied()
        .unwrap_or(0)
}

async fn flux_joue(orch: &PlaybackOrchestrator, zone_id: i64) -> Option<String> {
    orch.playback
        .get_state(zone_id)
        .await
        .now_playing
        .and_then(|np| np.stream_id)
}

/// Le décodeur de la session alimente-t-il encore le flux ?
async fn session_vivante(orch: &PlaybackOrchestrator, sid: &str) -> bool {
    let session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(sid)
        .cloned();
    session.is_some_and(|s| !s.producer_done.load(std::sync::atomic::Ordering::Relaxed))
}

/// Zone DLNA qui joue une radio par le VRAI chemin de lecture.
async fn zone_dlna_qui_joue_une_radio(device_id: &str) -> (Arc<PlaybackOrchestrator>, i64) {
    let orch = Arc::new(orchestrateur());
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Marantz ND8006", Some("dlna"), Some(device_id))
        .unwrap();
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(device_id, "Marantz ND8006").with_type("dlna"),
    ));
    let url = station();
    orch.playback
        .play(
            zone_id,
            NowPlaying {
                title: "Home".into(),
                source: "radio".into(),
                source_id: Some(url),
                ..Default::default()
            },
        )
        .await;
    orch.replay_zone_at_position(zone_id, 0, "temoin_4407_radio")
        .await
        .expect("la radio doit partir vers le renderer");
    let sid = flux_joue(&orch, zone_id)
        .await
        .expect("point de départ : la zone joue une session Tune");
    assert!(
        session_vivante(&orch, &sid).await,
        "point de départ : le décodeur radio alimente la session"
    );
    (orch, zone_id)
}

async fn laisser_passer_l_anti_rebond() {
    tokio::time::sleep(std::time::Duration::from_millis(
        PlaybackOrchestrator::EQ_REPLAY_DEBOUNCE_MS + 900,
    ))
    .await;
}

/// LE défaut du ticket : activer puis changer l'égaliseur sur une radio DLNA.
///
/// Avant : `Relance`, une relance programmée par geste, et après
/// l'anti-rebond un nouveau `play_media` (nouvelle session UPnP) sur le
/// renderer. Après : le décodeur radio relève l'égaliseur en vol — portée
/// `Immediate`, aucune relance, aucun `SetAVTransportURI`, même session, et
/// son décodeur tourne toujours.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn egaliseur_change_sur_une_radio_dlna_sans_nouvelle_session_upnp_4407() {
    let device_id = "dlna:uuid-56fcb4ae-4407-radio";
    let (orch, zone_id) = zone_dlna_qui_joue_une_radio(device_id).await;
    let avant = commandes(&orch, device_id).await;
    let sid = flux_joue(&orch, zone_id).await.unwrap();
    let generation = orch.playback.get_state(zone_id).await.track_generation;

    // 1. Activer un égaliseur audible (le flux n'en portait aucun).
    ecrire_profil(&orch, zone_id, &profil_audible(8.0));
    assert!(
        orch.zone_has_active_eq(zone_id),
        "prémisse : l'égaliseur change le signal"
    );
    assert_eq!(
        orch.apply_eq_change_portee(zone_id).await,
        PorteeDuReglage::Immediate,
        "l'égaliseur est posé dans le flux radio en cours, pas au bout d'une relance"
    );
    // 2. Le changer encore, dans la même seconde (curseur qu'on fait glisser).
    ecrire_profil(&orch, zone_id, &profil_audible(-4.0));
    assert_eq!(
        orch.apply_eq_change_portee(zone_id).await,
        PorteeDuReglage::Immediate
    );
    // 3. Le couper : retirer l'égaliseur du flux, toujours sans relance.
    let mut coupe = profil_audible(-4.0);
    coupe.enabled = false;
    ecrire_profil(&orch, zone_id, &coupe);
    assert_eq!(
        orch.apply_eq_change_portee(zone_id).await,
        PorteeDuReglage::Immediate
    );

    assert_eq!(
        relances_programmees(&orch, zone_id),
        0,
        "aucune relance ne doit être programmée : la radio n'a pas de position à reprendre"
    );
    laisser_passer_l_anti_rebond().await;
    assert_eq!(
        commandes(&orch, device_id).await,
        avant,
        "zéro Stop/SetAVTransportURI/Play vers le renderer : aucune nouvelle session UPnP"
    );
    assert_eq!(
        flux_joue(&orch, zone_id).await.as_deref(),
        Some(sid.as_str()),
        "la zone joue toujours la même session HTTP"
    );
    assert!(
        session_vivante(&orch, &sid).await,
        "le décodeur radio alimente toujours la session : le flux continue"
    );
    assert_eq!(
        orch.playback.get_state(zone_id).await.track_generation,
        generation,
        "la lecture n'a pas été refaite"
    );
}
