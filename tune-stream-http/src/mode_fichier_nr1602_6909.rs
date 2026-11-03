//! Requête du fil 2410 (réponse 9268) : WinampMPEG/2.8 demande Streaming
//! pour un FLAC pré-transcodé. Le serveur répondait Interactive sur GET,
//! Range et HEAD. Ce témoin HTTP ne simule pas le décodeur du NR1602 et
//! ne prouve pas que ce désaccord causait sa fermeture de connexion.
use super::*;
use futures_util::StreamExt;
use std::sync::Arc;

async fn verifier(head: bool, range: bool) {
    // Octets synthétiques : le service fichier ne décode pas le FLAC.
    let fichier = tempfile::NamedTempFile::new().unwrap();
    let contenu: Vec<u8> = (0..1_048_576).map(|i| (i % 251) as u8).collect();
    std::fs::write(fichier.path(), &contenu).unwrap();
    let info = StreamInfo {
        format: "flac".into(),
        mime_type: "audio/flac".into(),
        sample_rate: 44_100,
        bit_depth: 16,
        channels: 2,
        ..StreamInfo::default()
    };
    let session = Arc::new(StreamSession::new("nr1602".into(), info, false, 8));
    *session.file_path.lock().await = Some(fichier.path().to_string_lossy().into_owned());
    let sessions: SharedSessions = Arc::new(tokio::sync::Mutex::new(
        [("nr1602".to_string(), session)].into_iter().collect(),
    ));
    for (demande, attendu) in [
        (Some("Streaming"), "Streaming"),
        (Some("Background"), "Background"),
        (Some("Interactive"), "Interactive"),
        (Some(" Streaming "), "Streaming"),
        (None, "Interactive"),
        (Some("inconnu"), "Interactive"),
    ] {
        let mut req = HeaderMap::new();
        req.insert("User-Agent", HeaderValue::from_static("WinampMPEG/2.8"));
        if let Some(mode) = demande {
            req.insert("transferMode.dlna.org", HeaderValue::from_static(mode));
        }
        if range {
            req.insert("Range", HeaderValue::from_static("bytes=65536-"));
        }
        let reponse = if head {
            handle_head(Path("nr1602.flac".into()), State(sessions.clone()), req).await
        } else {
            handle_stream(Path("nr1602.flac".into()), State(sessions.clone()), req).await
        };
        assert_eq!(
            reponse.status(),
            if range {
                StatusCode::PARTIAL_CONTENT
            } else {
                StatusCode::OK
            }
        );
        assert_eq!(
            reponse.headers()["transferMode.dlna.org"],
            attendu,
            "#6909 : le fichier FLAC contredit le mode DLNA demandé ({demande:?}, head={head}, range={range})"
        );
        assert_eq!(reponse.headers()["Content-Type"], "audio/flac");
        assert_eq!(reponse.headers()["Accept-Ranges"], "bytes");
        let debut = if range { 65536 } else { 0 };
        assert_eq!(
            reponse.headers()["Content-Length"],
            (contenu.len() - debut).to_string()
        );
        if range {
            assert_eq!(
                reponse.headers()["Content-Range"],
                "bytes 65536-1048575/1048576"
            );
        }
        let mut flux = reponse.into_body().into_data_stream();
        let mut recu = Vec::new();
        while let Some(bloc) = flux.next().await {
            recu.extend_from_slice(&bloc.unwrap());
        }
        if head {
            assert!(recu.is_empty());
        } else {
            assert_eq!(
                recu,
                contenu[debut..],
                "les octets du fichier doivent rester identiques"
            );
        }
    }
}

#[tokio::test]
async fn get_flac_respecte_streaming_6909() {
    verifier(false, false).await;
}

#[tokio::test]
async fn range_flac_respecte_streaming_6909() {
    verifier(false, true).await;
}

#[tokio::test]
async fn head_flac_respecte_streaming_6909() {
    verifier(true, false).await;
}
