//! #8444 (fil 2761, rc11, Windows, sortie locale WASAPI) — une piste YouTube
//! en Opus restait à 0:00, puis le chien de garde #5522 relançait et arrêtait
//! la zone au bout de 8 s. Le bras local/OAAT téléchargeait le fichier Opus
//! ENTIER avant le premier octet WAV ; `googlevideo` ne débite qu'environ deux
//! fois le temps réel.
//!
//! Le témoin : un faux `googlevideo` qui sert un Opus de 20 s à deux fois le
//! temps réel (10 s de téléchargement, plus que les 8 s du chien de garde). La
//! VRAIE tâche locale/OAAT doit livrer son premier PCM bien avant 8 s, et
//! avant la fin du téléchargement ; puis le titre entier.
//!
//! Aucun appel réseau hors de 127.0.0.1, aucun son : le PCM reste en mémoire.

use super::*;
use crate::http::streamer::StreamInfo;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Le délai du chien de garde #5522 (`DEMARRAGE_FIGE_SECS`).
const CHIEN_DE_GARDE: Duration = Duration::from_secs(8);
const DUREE_DU_TITRE_S: f32 = 20.0;
/// `googlevideo` : environ deux fois le temps réel.
const TELECHARGEMENT: Duration = Duration::from_secs(10);

struct Amont {
    url: String,
    fini: Arc<AtomicBool>,
    ferme: Arc<AtomicBool>,
    tache: tokio::task::JoinHandle<()>,
}

impl Drop for Amont {
    fn drop(&mut self) {
        self.tache.abort();
    }
}

/// Un amont qui sert `corps` en 200, réparti régulièrement sur `duree`.
async fn amont_lent(corps: Vec<u8>, duree: Duration) -> Amont {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/videoplayback", listener.local_addr().unwrap());
    let fini = Arc::new(AtomicBool::new(false));
    let ferme = Arc::new(AtomicBool::new(false));
    let (f, c) = (fini.clone(), ferme.clone());
    let tache = tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut requete = Vec::new();
        let mut octet = [0u8; 1];
        while !requete.ends_with(b"\r\n\r\n") && requete.len() < 16_384 {
            if socket.read_exact(&mut octet).await.is_err() {
                return;
            }
            requete.push(octet[0]);
        }
        let entete = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: audio/webm\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            corps.len()
        );
        if socket.write_all(entete.as_bytes()).await.is_err() {
            c.store(true, Ordering::SeqCst);
            return;
        }
        const TRANCHES: u32 = 100;
        let pas = duree / TRANCHES;
        let taille = corps.len().div_ceil(TRANCHES as usize);
        for tranche in corps.chunks(taille) {
            if socket.write_all(tranche).await.is_err() || socket.flush().await.is_err() {
                c.store(true, Ordering::SeqCst);
                return;
            }
            tokio::time::sleep(pas).await;
        }
        f.store(true, Ordering::SeqCst);
        let _ = socket.shutdown().await;
    });
    Amont {
        url,
        fini,
        ferme,
        tache,
    }
}

/// La tâche locale/OAAT telle que `capturer_pour_le_transcodage` la monte
/// pour une piste YouTube Opus vers une sortie locale (48 kHz, 32 bits).
fn tache_locale(
    streamer: Arc<AudioStreamer>,
    sid: String,
    url: String,
) -> super::super::TranscodageEnTache {
    super::super::TranscodageEnTache {
        is_dash_local: false,
        use_http_range: super::super::decodage_progressif_par_range("youtube", "opus", &url),
        upstream_url: url,
        upstream_headers: Vec::new(),
        codec: "opus".into(),
        sr: 48_000,
        bd: 32,
        ev_bus: None,
        playback: Arc::new(crate::playback::PlaybackManager::new()),
        zone_id: 8444,
        streamer_for_eof: streamer,
        session_id_for_eof: sid,
        attach_levels: false,
        seek_s: 0.0,
        ecretages: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn youtube_opus_sur_sortie_locale_demarre_avant_le_chien_de_garde_8444() {
    let corps = crate::ogg_factice::ogg_opus(8444, DUREE_DU_TITRE_S, &[]);
    let amont = amont_lent(corps.clone(), TELECHARGEMENT).await;
    let streamer = Arc::new(AudioStreamer::new(0));
    let (sid, tx, ready) = streamer
        .create_session(StreamInfo::default(), false, 256)
        .await;
    let session = streamer.sessions_state().lock().await[&sid].clone();
    let debut = std::time::Instant::now();
    let producteur = tokio::spawn(
        super::super::PlaybackOrchestrator::transcoder_le_flux_en_wav(
            tache_locale(streamer.clone(), sid.clone(), amont.url.clone()),
            tx,
            ready,
        ),
    );

    // L'en-tête WAV, puis le premier PCM.
    let mut servis = Vec::new();
    while servis.len() <= 44 {
        let reste = CHIEN_DE_GARDE.saturating_sub(debut.elapsed());
        let morceau = tokio::time::timeout(reste, session.recv_chunk())
            .await
            .expect(
                "#8444 : aucun PCM en 8 s — la sortie locale attend la fin du \
                 téléchargement de l'Opus, le chien de garde #5522 arrête la zone",
            )
            .expect("le canal ne se ferme pas avant la fin");
        servis.extend_from_slice(&morceau);
    }
    let premier_pcm = debut.elapsed();
    assert!(
        !amont.fini.load(Ordering::SeqCst),
        "#8444 : le premier PCM n'est parti qu'après le DERNIER octet amont \
         ({premier_pcm:?})"
    );
    assert!(
        premier_pcm < CHIEN_DE_GARDE / 2,
        "premier PCM trop tardif : {premier_pcm:?}"
    );

    // Puis le titre entier, sans troncature.
    loop {
        match tokio::time::timeout(Duration::from_secs(20), session.recv_chunk()).await {
            Ok(Some(c)) => servis.extend_from_slice(&c),
            Ok(None) => break,
            Err(_) => panic!("le canal ne s'est pas fermé après la fin du téléchargement"),
        }
    }
    tokio::time::timeout(Duration::from_secs(5), producteur)
        .await
        .expect("la tâche rend la main")
        .unwrap();
    assert!(amont.fini.load(Ordering::SeqCst));

    let fichier = crate::test_scratch::scratch_file("tune-8444-reference", ".opus");
    std::fs::write(&fichier, &corps).unwrap();
    let reference =
        crate::audio::decode::decode_to_pcm(&fichier.to_string_lossy(), None, None, 0.0, 0.0)
            .unwrap();
    let trames_reference = reference.samples_i32.len() / reference.channels as usize;
    // Sortie locale : stéréo, 32 bits.
    assert_eq!(
        (servis.len() - 44) / 8,
        trames_reference,
        "le flux servi pendant le téléchargement doit porter tout le titre"
    );
    assert_eq!((servis.len() - 44) % 8, 0);
    assert!(
        streamer.session_alive(&sid).await,
        "la session réussie reste lisible"
    );
}

/// Un Stop pendant le téléchargement : la tâche rend la main, et l'amont voit
/// sa connexion fermée au lieu d'aller au bout (#4234).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_pendant_le_telechargement_opus_libere_l_amont_8444() {
    let corps = crate::ogg_factice::ogg_opus(8445, DUREE_DU_TITRE_S, &[]);
    let amont = amont_lent(corps, TELECHARGEMENT).await;
    let streamer = Arc::new(AudioStreamer::new(0));
    let (sid, tx, ready) = streamer
        .create_session(StreamInfo::default(), false, 256)
        .await;
    let session = streamer.sessions_state().lock().await[&sid].clone();
    let producteur = tokio::spawn(
        super::super::PlaybackOrchestrator::transcoder_le_flux_en_wav(
            tache_locale(streamer.clone(), sid.clone(), amont.url.clone()),
            tx,
            ready,
        ),
    );
    // Le flux a démarré.
    let mut servis = 0;
    while servis <= 44 {
        servis += tokio::time::timeout(CHIEN_DE_GARDE, session.recv_chunk())
            .await
            .expect("premier PCM")
            .expect("canal ouvert")
            .len();
    }
    streamer.remove_session(&sid).await;
    // Le consommateur ne lit plus : vider ce qui reste n'est pas son rôle,
    // mais le canal borné ne doit pas retenir le décodeur.
    drop(session);
    tokio::time::timeout(Duration::from_secs(3), producteur)
        .await
        .expect("la tâche locale doit rendre la main après le retrait de sa session")
        .unwrap();
    let limite = std::time::Instant::now() + Duration::from_secs(3);
    while !amont.ferme.load(Ordering::SeqCst) && std::time::Instant::now() < limite {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        amont.ferme.load(Ordering::SeqCst) && !amont.fini.load(Ordering::SeqCst),
        "le téléchargement d'une piste arrêtée doit s'interrompre, pas aller au bout"
    );
}

/// Un refus amont (403 `googlevideo`) : aucune session vide ne reste (#3287).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refus_amont_opus_retire_la_session_8444() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/videoplayback", listener.local_addr().unwrap());
    let _amont = tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut requete = Vec::new();
        let mut octet = [0u8; 1];
        while !requete.ends_with(b"\r\n\r\n") {
            if socket.read_exact(&mut octet).await.is_err() {
                return;
            }
            requete.push(octet[0]);
        }
        let _ = socket
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        let _ = socket.shutdown().await;
    });
    let streamer = Arc::new(AudioStreamer::new(0));
    let (sid, tx, ready) = streamer
        .create_session(StreamInfo::default(), false, 8)
        .await;
    let session = streamer.sessions_state().lock().await[&sid].clone();
    tokio::time::timeout(
        Duration::from_secs(5),
        super::super::PlaybackOrchestrator::transcoder_le_flux_en_wav(
            tache_locale(streamer.clone(), sid.clone(), url),
            tx,
            ready,
        ),
    )
    .await
    .unwrap();
    assert!(!streamer.session_alive(&sid).await);
    assert!(session.recv_chunk().await.is_none());
}

#[test]
fn seul_l_opus_servi_tel_quel_prend_le_chemin_en_croissance_8444() {
    use super::super::opus_decode_pendant_le_telechargement as en_croissance;
    assert!(en_croissance("opus", 48_000, 0.0));
    assert!(en_croissance("OPUS", 48_000, 0.0));
    // m4a/aac gardent le Range (#1885), le reste le téléchargement complet.
    for codec in ["m4a", "mp4", "aac", "flac", "webm", "mp3"] {
        assert!(!en_croissance(codec, 48_000, 0.0), "{codec}");
    }
    // Hors du contrat du décodeur au fil de l'eau : chemin d'avant.
    assert!(!en_croissance("opus", 44_100, 0.0), "zone plafonnée");
    assert!(!en_croissance("opus", 48_000, 42.0), "seek");
}
