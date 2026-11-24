//! #9450 (fil 3033, rc13.1) — sur la voie mandataire, chaque piste recevait
//! DEUX `GET` sur le même `stream_id` : celui de la sonde des VU-mètres, sans
//! `Range` ni `User-Agent`, ~1 ms après `initial_prebuffer_done` et avant le
//! `SetAVTransportURI`, puis celui du renderer. Le mandataire ouvrait une
//! connexion au CDN pour chacun : le morceau était tiré deux fois en entier.
//!
//! Le banc passe par la vraie porte de la sonde
//! (`PlaybackOrchestrator::spawn_proxy_levels_probe`, celle de
//! `resolve_stream`) avec une session mandataire vivante, et une adresse de
//! flux qui pointe sur un écouteur qui compte ses connexions : c'est lui qui
//! tient lieu du serveur de flux de Tune. Contre-épreuve : avant le correctif,
//! la sonde s'y connecte (1 connexion, test rouge).
use crate::db::backend::DbBackend;
use crate::event_bus::{EventBus, TuneEvent};
use crate::http::streamer::{AudioStreamer, StreamInfo};
use crate::orchestrator::PlaybackOrchestrator;
use crate::playback::{NowPlaying, PlaybackManager};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn wav_silencieux(secs: u32) -> Vec<u8> {
    let sr: u32 = 44_100;
    let data = vec![0u8; sr as usize * 4 * secs as usize];
    let mut wav = Vec::with_capacity(44 + data.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&sr.to_le_bytes());
    wav.extend_from_slice(&(sr * 4).to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    wav
}

/// Un « serveur de flux » qui compte les connexions et sert `corps` à chacune.
fn serveur_qui_compte(corps: Vec<u8>) -> (u16, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let n = Arc::new(AtomicUsize::new(0));
    let compte = n.clone();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for flux in listener.incoming() {
            let Ok(mut s) = flux else { break };
            compte.fetch_add(1, Ordering::SeqCst);
            let corps = corps.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    corps.len()
                );
                let _ = s.write_all(&corps);
            });
        }
    });
    (port, n)
}

struct Banc {
    orch: PlaybackOrchestrator,
    streamer: Arc<AudioStreamer>,
    rx: tokio::sync::broadcast::Receiver<TuneEvent>,
    zone_id: i64,
}

async fn banc() -> Banc {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let playback = Arc::new(PlaybackManager::new());
    let streamer = Arc::new(AudioStreamer::new(0));
    let mut orch = PlaybackOrchestrator::new(
        db,
        playback.clone(),
        streamer.clone(),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    );
    let bus = Arc::new(EventBus::new());
    let rx = bus.subscribe();
    orch.event_bus = Some(bus);
    let zone_id = 9450;
    playback.play(zone_id, NowPlaying::default()).await;
    Banc {
        orch,
        streamer,
        rx,
        zone_id,
    }
}

async fn session_mandataire(streamer: &AudioStreamer) -> String {
    let info = StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 44_100,
        bit_depth: 16,
        channels: 2,
        ..Default::default()
    };
    streamer
        .create_proxy_session(info, "https://cdn.exemple.invalid/piste.wav".into(), false)
        .await
}

async fn trames_recues(rx: &mut tokio::sync::broadcast::Receiver<TuneEvent>, secs: u64) -> u32 {
    let mut n = 0;
    let fin = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        let reste = fin.saturating_duration_since(tokio::time::Instant::now());
        if reste.is_zero() {
            break;
        }
        match tokio::time::timeout(reste, rx.recv()).await {
            Ok(Ok(ev)) if ev.event_type == "playback.audio_levels" => n += 1,
            Ok(Ok(_)) => {}
            _ => break,
        }
    }
    n
}

/// Le témoin de #9450 : la sonde d'une session mandataire n'ouvre AUCUNE
/// connexion sur `/stream/<id>` — le renderer reste le seul à tirer la piste,
/// donc le mandataire n'ouvre qu'une connexion amont.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn la_sonde_d_une_session_mandataire_n_ouvre_aucune_connexion() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async move {
        let b = banc().await;
        let (port, connexions) = serveur_qui_compte(wav_silencieux(2));
        let sid = session_mandataire(&b.streamer).await;
        let url = format!("http://127.0.0.1:{port}/stream/{sid}.wav");

        b.orch
            .spawn_proxy_levels_probe(b.zone_id, url, "wav".into())
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;

        assert_eq!(
            connexions.load(Ordering::SeqCst),
            0,
            "la sonde des VU-mètres a ouvert sa propre connexion au flux : le \
         mandataire tire alors la piste une seconde fois depuis l'amont (#9450)"
        );
        b.streamer.remove_session(&sid).await;
    })
    .await
    .expect("témoin #9450 bloqué : 30 s dépassées");
}

/// Les VU-mètres ne sont pas perdus pour autant : les octets que le
/// mandataire relaie au renderer (ici recopiés à la main dans la prise, comme
/// le fait `resumable_proxy_body`) suffisent à la sonde.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn la_sonde_decode_les_octets_relayes_au_renderer() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async move {
        let mut b = banc().await;
        let sid = session_mandataire(&b.streamer).await;
        let url = format!("http://127.0.0.1:9/stream/{sid}.wav");

        b.orch
            .spawn_proxy_levels_probe(b.zone_id, url, "wav".into())
            .await;
        let session = b
            .streamer
            .sessions_state()
            .lock()
            .await
            .get(&sid)
            .cloned()
            .unwrap();
        assert!(
            session.prise_des_niveaux.branchee(),
            "la prise est branchée avant que l'URL parte au renderer"
        );
        let wav = wav_silencieux(3);
        let total = wav.len() as u64;
        for (i, bloc) in wav.chunks(64 * 1024).enumerate() {
            session
                .prise_des_niveaux
                .nourrir(1, (i * 64 * 1024) as u64, bloc, Some(total));
        }
        let n = trames_recues(&mut b.rx, 5).await;
        assert!(
            n >= 40,
            "3 s relayées ⇒ ~75 fenêtres de 40 ms sur le bus ; reçu {n}"
        );
        b.streamer.remove_session(&sid).await;
    })
    .await
    .expect("témoin #9450 bloqué : 30 s dépassées");
}

/// Une adresse qui n'est pas une session de Tune (épisode, Bandcamp) garde
/// sa connexion propre, désormais marquée de l'agent interne.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_adresse_exterieure_garde_sa_connexion() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async move {
        let b = banc().await;
        let (port, connexions) = serveur_qui_compte(wav_silencieux(1));
        let url = format!("http://127.0.0.1:{port}/episode.wav");
        b.orch
            .spawn_proxy_levels_probe(b.zone_id, url, "wav".into())
            .await;
        let fin = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while connexions.load(Ordering::SeqCst) == 0 && tokio::time::Instant::now() < fin {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(connexions.load(Ordering::SeqCst), 1);
    })
    .await
    .expect("témoin #9450 bloqué : 30 s dépassées");
}

/// #9450, revue — le renderer saute dans le fichier (nouvelle connexion
/// `bytes=N-` loin de la position suivie) : la sonde se RECALE, en-tête gardé
/// puis la suite, au lieu de rester muette jusqu'à la fin de la piste.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_saut_du_renderer_recale_la_sonde() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async move {
        let mut b = banc().await;
        let sid = session_mandataire(&b.streamer).await;
        let url = format!("http://127.0.0.1:9/stream/{sid}.wav");
        b.orch
            .spawn_proxy_levels_probe(b.zone_id, url, "wav".into())
            .await;
        let session = b
            .streamer
            .sessions_state()
            .lock()
            .await
            .get(&sid)
            .cloned()
            .unwrap();
        // 6 s de WAV 44,1/16 stéréo : 176 400 octets par seconde après 44.
        let wav = wav_silencieux(6);
        let total = wav.len() as u64;
        let seconde = 176_400usize;
        // Connexion 1 : la première seconde.
        session
            .prise_des_niveaux
            .nourrir(1, 0, &wav[..44 + seconde], Some(total));
        // Le renderer joue la première seconde, puis saute à 4 s : sa position
        // rapportée suit, comme le sondeur la relèverait.
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        let playback = b.orch.playback.clone();
        let zone_id = b.zone_id;
        let horloge = tokio::spawn(async move {
            let t0 = tokio::time::Instant::now();
            loop {
                let ms = 4_000 + t0.elapsed().as_millis() as i64;
                playback.update_position(zone_id, ms).await;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        });
        // Connexion 2 : saut à 4 s, jusqu'à la fin.
        let saut = 44 + 4 * seconde;
        for (i, bloc) in wav[saut..].chunks(64 * 1024).enumerate() {
            session
                .prise_des_niveaux
                .nourrir(2, (saut + i * 64 * 1024) as u64, bloc, Some(total));
        }
        // 1 s + 2 s décodées ≈ 75 fenêtres ; sans recalage, ~25 seulement.
        let n = trames_recues(&mut b.rx, 6).await;
        horloge.abort();
        assert!(
            n >= 60,
            "la sonde doit se recaler après le saut et décoder la suite ; reçu {n} fenêtres"
        );
        b.streamer.remove_session(&sid).await;
    })
    .await
    .expect("témoin #9450 bloqué : 30 s dépassées");
}

/// #9450, revue — le balayage d'inactivité coupe la prise et oublie la prise
/// garée, comme `remove_session`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_balayage_d_inactivite_coupe_et_oublie_la_prise() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async move {
        let b = banc().await;
        let sid = session_mandataire(&b.streamer).await;
        let lecteur = b
            .streamer
            .brancher_la_prise_des_niveaux(&sid, Box::new(|| false))
            .await
            .unwrap();
        crate::http::prise_des_niveaux_9450::garer(&sid, lecteur);
        let session = b
            .streamer
            .sessions_state()
            .lock()
            .await
            .get(&sid)
            .cloned()
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let retirees = b
            .streamer
            .cleanup_stale_sessions_with(
                std::time::Duration::from_millis(1),
                std::time::Duration::from_secs(3600),
            )
            .await;
        assert_eq!(retirees, 1);
        assert!(!session.prise_des_niveaux.branchee(), "prise coupée");
        assert!(
            !crate::http::prise_des_niveaux_9450::est_garee(&sid),
            "prise garée oubliée"
        );
    })
    .await
    .expect("témoin #9450 bloqué : 30 s dépassées");
}

/// #9450, revue — la piste remplacée arrête le lecteur d'une prise muette :
/// pas de sonde garée jusqu'au balayage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn la_piste_remplacee_arrete_la_sonde_d_une_prise_muette() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async move {
        let b = banc().await;
        let sid = session_mandataire(&b.streamer).await;
        let url = format!("http://127.0.0.1:9/stream/{sid}.wav");
        b.orch
            .spawn_proxy_levels_probe(b.zone_id, url, "wav".into())
            .await;
        let session = b
            .streamer
            .sessions_state()
            .lock()
            .await
            .get(&sid)
            .cloned()
            .unwrap();
        assert!(session.prise_des_niveaux.branchee());
        // La zone passe à une autre piste ; rien n'a jamais coulé.
        b.orch.playback.bump_generation(b.zone_id).await;
        let fin = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut pos = 0u64;
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        while session.prise_des_niveaux.branchee() && tokio::time::Instant::now() < fin {
            // Un bloc relayé fait voir à la prise que le lecteur est parti.
            session.prise_des_niveaux.nourrir(1, pos, &[0u8; 4], None);
            pos += 4;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(
            !session.prise_des_niveaux.branchee(),
            "le lecteur doit voir la piste remplacée et lâcher la prise"
        );
        b.streamer.remove_session(&sid).await;
    })
    .await
    .expect("témoin #9450 bloqué : 30 s dépassées");
}

// ── Suite de la relecture de #9458 ──────────────────────────────────────

/// Point 1 — la prise garée d'une session a été débranchée (plafond
/// dépassé) : la sonde ne la reprend pas, elle branche un lecteur neuf.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_prise_garee_morte_est_remplacee_par_un_lecteur_neuf() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async move {
        let b = banc().await;
        let sid = session_mandataire(&b.streamer).await;
        let session = b
            .streamer
            .sessions_state()
            .lock()
            .await
            .get(&sid)
            .cloned()
            .unwrap();
        session.prise_des_niveaux.poser_le_debit(1);
        let fin = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let garee = session
            .prise_des_niveaux
            .brancher(Box::new(move || std::time::Instant::now() >= fin));
        crate::http::prise_des_niveaux_9450::garer(&sid, garee);
        let bloc = vec![0u8; 1024 * 1024];
        for i in 0..10u64 {
            session
                .prise_des_niveaux
                .nourrir(1, i * bloc.len() as u64, &bloc, None);
        }
        assert!(
            !session.prise_des_niveaux.branchee(),
            "mise en scène : débranchée"
        );
        let url = format!("http://127.0.0.1:9/stream/{sid}.wav");
        b.orch
            .spawn_proxy_levels_probe(b.zone_id, url, "wav".into())
            .await;
        assert!(
            session.prise_des_niveaux.branchee(),
            "un lecteur neuf doit être branché à la place de la prise garée morte"
        );
        b.streamer.remove_session(&sid).await;
    })
    .await
    .expect("témoin #9450 bloqué : 30 s dépassées");
}

/// Point 2 — le décodeur freiné (30 s d'avance, position immobile) voit le
/// saut du renderer et se recale, au lieu de rester freiné sur l'ancienne
/// position.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn le_decodeur_freine_reagit_a_un_saut() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async move {
        let b = banc().await;
        let sid = session_mandataire(&b.streamer).await;
        let url = format!("http://127.0.0.1:9/stream/{sid}.wav");
        b.orch
            .spawn_proxy_levels_probe(b.zone_id, url, "wav".into())
            .await;
        let session = b
            .streamer
            .sessions_state()
            .lock()
            .await
            .get(&sid)
            .cloned()
            .unwrap();
        // 35 s relayées d'un coup : le décodeur s'arrête à ~30 s d'avance.
        let wav = wav_silencieux(60);
        let total = wav.len() as u64;
        let seconde = 176_400usize;
        for (i, bloc) in wav[..44 + 35 * seconde].chunks(256 * 1024).enumerate() {
            session
                .prise_des_niveaux
                .nourrir(1, (i * 256 * 1024) as u64, bloc, Some(total));
        }
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        // Le renderer saute en avant, à 50 s (nouvelle connexion).
        let saut = 44 + 50 * seconde;
        session
            .prise_des_niveaux
            .nourrir(2, saut as u64, &wav[saut..saut + seconde], Some(total));
        assert!(
            session.prise_des_niveaux.saut_en_attente(),
            "mise en scène : saut envoyé"
        );
        let fin = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while session.prise_des_niveaux.saut_en_attente() && tokio::time::Instant::now() < fin {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            !session.prise_des_niveaux.saut_en_attente(),
            "le décodeur freiné doit prendre le saut et se recaler"
        );
        b.streamer.remove_session(&sid).await;
    })
    .await
    .expect("témoin #9450 bloqué : 30 s dépassées");
}
