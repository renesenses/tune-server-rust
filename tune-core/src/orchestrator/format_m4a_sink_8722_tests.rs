//! #8722 — négociation réelle avec un Sink HTTP synthétique, sans appareil.
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::zone_repo::ZoneRepo;
use crate::orchestrator::{PlayRequest, PlaybackOrchestrator};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Sonde(tokio::task::JoinHandle<()>);
impl Drop for Sonde {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn renderer(sink: &'static str) -> (crate::outputs::dlna::DlnaOutput, Sonde) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let body = format!(
                r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:GetProtocolInfoResponse xmlns:u="urn:schemas-upnp-org:service:ConnectionManager:1"><Source></Source><Sink>{sink}</Sink></u:GetProtocolInfoResponse></s:Body></s:Envelope>"#
            );
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..n]);
                if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..pos]);
                    let len = headers
                        .lines()
                        .find_map(|line| {
                            let (k, v) = line.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if request.len() >= pos + 4 + len {
                        break;
                    }
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    (
        crate::outputs::dlna::DlnaOutput::new(
            "WiiM synthétique 8722".into(),
            "uuid:8722".into(),
            base.clone(),
            format!("{base}/avtransport"),
            format!("{base}/rendering"),
            Some(format!("{base}/connection")),
        ),
        Sonde(handle),
    )
}

async fn verifier(sink: &'static str, attendu: &str, signature: &[u8]) {
    verifier_regle(sink, attendu, signature, None, false).await;
}

async fn verifier_regle(
    sink: &'static str,
    attendu: &str,
    signature: &[u8],
    plafond: Option<u32>,
    strict: bool,
) {
    verifier_source(sink, attendu, signature, plafond, strict, false).await;
}

async fn verifier_source(
    sink: &'static str,
    attendu: &str,
    signature: &[u8],
    plafond: Option<u32>,
    strict: bool,
    aac: bool,
) {
    let sqlite = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    sqlite.init_schema().unwrap();
    crate::db::migrations::run_migrations(&sqlite).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(sqlite);
    let orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    );
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("source.m4a");
    std::fs::copy(
        if aac {
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/aac320.m4a")
        } else {
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/alac/ref_16_44100_stereo.m4a"
            )
        },
        &file,
    )
    .unwrap();
    let chemin = file.to_string_lossy().into_owned();
    db.execute(
        "INSERT INTO tracks (id,title,file_path,format,duration_ms,sample_rate,bit_depth,channels) VALUES (8722,'Synthétique',?,?,1000,44100,16,2)",
        &[&chemin as &dyn ToSqlValue, &if aac { "aac" } else { "alac" }],
    ).unwrap();
    let zone = ZoneRepo::with_backend(db.clone())
        .create("Banc 8722", Some("dlna"), Some("uuid:8722"))
        .unwrap();
    if let Some(plafond) = plafond {
        ZoneRepo::with_backend(db.clone())
            .update_max_sample_rate(zone, Some(plafond))
            .unwrap();
    }
    if strict {
        crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
            .set(&crate::audio::bitperfect_strict::cle_de_zone(zone), "true")
            .unwrap();
    }
    let (output, _server) = renderer(sink).await;
    orch.outputs.lock().await.register(Box::new(output));
    let resolved = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        orch.resolve_stream(&PlayRequest {
            zone_id: zone,
            track_id: Some(8722),
            source: Some("local".into()),
            output_device_id: Some("uuid:8722".into()),
            ..Default::default()
        }),
    )
    .await
    .expect("résolution bornée");
    if attendu == "refus_strict" {
        let erreur = resolved
            .err()
            .expect("refus strict malgré MP4 natif annoncé");
        assert!(erreur.contains("bitperfect_strict_refused"), "{erreur}");
        return;
    }
    if attendu == "refus" {
        let erreur = resolved
            .err()
            .expect("Sink incompatible : refus avant Play");
        assert!(
            erreur.contains("aucun format de sortie compatible"),
            "{erreur}"
        );
        return;
    }
    let resolved = resolved.expect("format accepté par le Sink");
    assert_eq!(
        resolved.mime_type, attendu,
        "#8722 : le Sink doit décider du format réellement servi"
    );
    if let Some(plafond) = plafond {
        assert!(resolved.sample_rate.is_some_and(|hz| hz <= plafond));
    }
    let sid = resolved.stream_id.expect("session");
    let session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(&sid)
        .cloned()
        .unwrap();
    let served = session
        .file_path
        .lock()
        .await
        .clone()
        .expect("fichier servi");
    let bytes = std::fs::read(served).unwrap();
    if attendu.starts_with("audio/L16") {
        let source = session
            .source_virtuelle
            .lock()
            .unwrap()
            .clone()
            .expect("L16 réellement servi");
        // Extraire indépendamment le bloc PCM du WAV de travail.
        assert_eq!(&bytes[..4], b"RIFF");
        let mut pos = 12;
        let pcm = loop {
            let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
            if &bytes[pos..pos + 4] == b"data" {
                break &bytes[pos + 8..pos + 8 + len];
            }
            pos += 8 + len + len % 2;
        };
        let attendu: Vec<u8> = pcm.chunks_exact(2).flat_map(|p| [p[1], p[0]]).collect();
        assert!(!attendu.is_empty());
        assert_eq!(source.total(), attendu.len() as u64);
        assert_eq!(session.info.file_size, Some(source.total()));
        assert_eq!(session.info.mime_type, resolved.mime_type);
        for (debut, longueur) in [(0, source.total()), (1, 7), (source.total() - 3, 10)] {
            let mut rendu = Vec::new();
            source
                .lire(debut, longueur, &mut |bloc| {
                    rendu.extend(bloc);
                    Ok(())
                })
                .unwrap();
            let fin = (debut + longueur).min(source.total()) as usize;
            assert_eq!(rendu, attendu[debut as usize..fin]);
        }
        orch.streamer.remove_session(&sid).await;
        return;
    }
    orch.streamer.remove_session(&sid).await;
    assert!(
        bytes
            .windows(signature.len())
            .take(16)
            .any(|w| w == signature),
        "#8722 : les octets doivent correspondre au MIME, pas seulement son étiquette"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m4a_8722_sink_mp4_sans_wav_garde_le_conteneur_natif() {
    verifier("http-get:*:audio/mp4:*", "audio/mp4", b"ftyp").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m4a_8722_sink_flac_seul_recoit_du_vrai_flac() {
    verifier("http-get:*:audio/flac:*", "audio/flac", b"fLaC").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m4a_8722_le_natif_prime_sur_flac_sans_traitement() {
    verifier(
        "http-get:*:audio/mp4:*,http-get:*:audio/flac:*",
        "audio/mp4",
        b"ftyp",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m4a_8722_l_alias_x_m4a_autorise_aussi_le_conteneur_natif() {
    verifier("http-get:*:audio/x-m4a:*", "audio/mp4", b"ftyp").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m4a_8722_l16_seul_sert_du_pcm_reseau_et_des_plages_exactes() {
    verifier(
        "http-get:*:audio/L16;rate=44100;channels=2:*",
        "audio/L16;rate=44100;channels=2",
        b"",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m4a_8722_sink_incompatible_est_refuse_avant_play() {
    verifier("http-get:*:audio/mpeg:*", "refus", b"").await;
    verifier("http-get:*:audio/L16;rate=48000;channels=2:*", "refus", b"").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m4a_8722_alias_wave_recoit_un_vrai_riff() {
    verifier("http-get:*:audio/vnd.wave:*", "audio/wav", b"RIFF").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m4a_8722_le_sink_mp4_ne_contourne_pas_le_plafond_ni_le_strict() {
    let sink = "http-get:*:audio/mp4:*,http-get:*:audio/flac:*";
    verifier_regle(sink, "audio/flac", b"fLaC", Some(22050), false).await;
    verifier_regle(sink, "refus_strict", b"", Some(22050), true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m4a_8722_aac_scanned_dans_mp4_annonce_le_conteneur() {
    verifier_source(
        "http-get:*:audio/mp4:*",
        "audio/mp4",
        b"ftyp",
        None,
        false,
        true,
    )
    .await;
}
