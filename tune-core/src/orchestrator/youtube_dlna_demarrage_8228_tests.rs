//! #8228 (balekutu, fil 2724, rc11) — YouTube Music sur une zone DLNA : plus
//! d'une minute avant le Play. Le bras de pré-transcodage téléchargeait le
//! fichier Opus ENTIER avant de créer la session ; `googlevideo` ne débitant
//! qu'environ deux fois le temps réel, l'attente valait la moitié du titre.
//!
//! Le témoin : un amont qui livre le début du fichier puis RETIENT la suite.
//! La session doit exister, et du PCM y couler, avant que la suite parte.
//! Puis, la suite livrée, le flux servi doit être complet : autant de PCM que
//! le décodage intégral du même fichier.

use super::*;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn fixture_opus() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/test.opus"
    ))
    .unwrap()
}

/// Un amont qui envoie `debut` octets du corps, puis attend `suite` avant
/// d'envoyer le reste.
async fn amont_retenu(corps: Vec<u8>, debut: usize, suite: Arc<tokio::sync::Notify>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/videoplayback", listener.local_addr().unwrap());
    tokio::spawn(async move {
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
        let _ = socket.write_all(entete.as_bytes()).await;
        let _ = socket.write_all(&corps[..debut]).await;
        let _ = socket.flush().await;
        suite.notified().await;
        let _ = socket.write_all(&corps[debut..]).await;
        let _ = socket.shutdown().await;
    });
    url
}

fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn youtube_opus_sur_dlna_demarre_avant_la_fin_du_telechargement_8228() {
    let corps = fixture_opus();
    // Tout sauf la DERNIÈRE page Ogg (et ses 10 premiers octets, pour couper
    // en pleine page). Le démuxeur Ogg lit la page qui suit la première page
    // de données avant de rendre sa piste ; les pages de cet échantillon
    // font 15 Ko, un fichier réel en a de quelques Ko.
    let derniere_page = corps
        .windows(4)
        .rposition(|w| w == b"OggS")
        .expect("échantillon Ogg");
    let debut = derniere_page + 10;
    assert!(
        debut < corps.len(),
        "prémisse : une partie du fichier reste retenue"
    );
    let suite = Arc::new(tokio::sync::Notify::new());
    let url = amont_retenu(corps.clone(), debut, suite.clone()).await;

    let orch = orchestrateur();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon", Some("dlna"), Some("dlna:renderer-8228"))
        .unwrap();
    let req = PlayRequest {
        zone_id,
        output_device_id: Some("dlna:renderer-8228".into()),
        source: Some("youtube".into()),
        source_id: Some("8228".into()),
        ..Default::default()
    };
    let flux = crate::streaming::StreamUrl {
        url,
        mime_type: "audio/webm".into(),
        quality: crate::streaming::traits::StreamQuality {
            codec: "OPUS".into(),
            sample_rate: 48_000,
            bit_depth: 16,
            bitrate: Some(128_000),
            channels: 2,
        },
        expires_at: None,
        headers: Vec::new(),
    };

    let (_, sid, mime, _) = tokio::time::timeout(
        Duration::from_secs(10),
        orch.pretranscoder_en_flac(&req, "youtube", &flux, "opus".into(), None),
    )
    .await
    .expect(
        "#8228 : la session n'est créée qu'après le téléchargement COMPLET de l'Opus — \
         le Play attend la moitié de la durée du titre",
    )
    .expect("le bras de pré-transcodage rend le flux");
    assert_eq!(mime, "audio/wav");
    let session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(sid.as_deref().expect("session"))
        .cloned()
        .expect("session inscrite");

    // L'en-tête WAV, puis du PCM, alors que la suite est toujours retenue.
    let mut servis = Vec::new();
    while servis.len() <= 44 {
        let morceau = tokio::time::timeout(Duration::from_secs(10), session.recv_chunk())
            .await
            .expect(
                "#8228 : aucun PCM servi tant que le téléchargement n'est pas fini — \
                 le décodage attend le fichier entier",
            )
            .expect("le canal ne se ferme pas avant la fin");
        servis.extend_from_slice(&morceau);
    }

    // La suite livrée, le flux va jusqu'au bout.
    suite.notify_one();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), session.recv_chunk()).await {
            Ok(Some(c)) => servis.extend_from_slice(&c),
            Ok(None) => break,
            Err(_) => panic!("le canal ne s'est pas fermé après la fin du téléchargement"),
        }
    }

    let fichier = crate::test_scratch::scratch_file("tune-8228-reference", ".opus");
    std::fs::write(&fichier, &corps).unwrap();
    let reference =
        crate::audio::decode::decode_to_pcm(&fichier.to_string_lossy(), None, None, 0.0, 0.0)
            .unwrap();
    assert_eq!(
        servis.len() - 44,
        reference.pcm_bytes().len(),
        "le flux servi pendant le téléchargement doit porter tout le titre"
    );
    assert!(
        servis[44..] == reference.pcm_bytes()[..],
        "le PCM servi au fil de l'eau doit être celui du décodage intégral"
    );
}

/// Une zone plafonnée sous 48 kHz : le décodeur au fil de l'eau ne la sert
/// pas. Le téléchargement complet reste AVANT la session — sinon le Play
/// partirait vers un flux muet pendant tout le téléchargement.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zone_plafonnee_a_44_1_garde_le_telechargement_avant_la_session_8228() {
    let corps = fixture_opus();
    let debut = corps.len() / 2;
    let suite = Arc::new(tokio::sync::Notify::new());
    let url = amont_retenu(corps, debut, suite.clone()).await;
    let orch = orchestrateur();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon", Some("dlna"), Some("dlna:renderer-8228"))
        .unwrap();
    let req = PlayRequest {
        zone_id,
        output_device_id: Some("dlna:renderer-8228".into()),
        source: Some("youtube".into()),
        source_id: Some("8228".into()),
        ..Default::default()
    };
    let flux = crate::streaming::StreamUrl {
        url,
        mime_type: "audio/webm".into(),
        quality: crate::streaming::traits::StreamQuality {
            codec: "OPUS".into(),
            sample_rate: 48_000,
            bit_depth: 16,
            bitrate: Some(128_000),
            channels: 2,
        },
        expires_at: None,
        headers: Vec::new(),
    };
    let liberation = tokio::time::Instant::now() + Duration::from_millis(1_500);
    let s2 = suite.clone();
    tokio::spawn(async move {
        tokio::time::sleep_until(liberation).await;
        s2.notify_one();
    });
    let rendu = tokio::time::timeout(
        Duration::from_secs(20),
        orch.pretranscoder_en_flac(&req, "youtube", &flux, "opus".into(), Some(44_100)),
    )
    .await
    .expect("le téléchargement complet finit")
    .expect("le bras de pré-transcodage rend le flux");
    assert!(
        tokio::time::Instant::now() >= liberation,
        "#8228 : zone à 44,1 kHz, la session est rendue avant la fin du \
         téléchargement alors que le décodeur ne la servira qu'après — flux muet"
    );
    assert_eq!(rendu.2, "audio/wav");
}
