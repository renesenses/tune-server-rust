//! #9450 — sur la voie mandataire, une piste n'est tirée qu'UNE fois de
//! l'amont : le `GET` du renderer (`Range: bytes=0-`) nourrit aussi la prise
//! des VU-mètres, qui n'ouvre plus sa propre connexion.
use super::*;
use axum::extract::{Path, State};
use futures_util::StreamExt;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tune_core::http::streamer::SharedSessions;

/// Un CDN qui compte ses connexions et sert `corps` en entier à chacune.
fn cdn_qui_compte(corps: Vec<u8>) -> (String, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let n = Arc::new(AtomicUsize::new(0));
    let compte = n.clone();
    std::thread::spawn(move || {
        use std::io::Write;
        for flux in listener.incoming() {
            let Ok(mut s) = flux else { break };
            compte.fetch_add(1, Ordering::SeqCst);
            let corps = corps.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let requete = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                if requete.contains("range: bytes=0-0") {
                    let _ = write!(
                        s,
                        "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/flac\r\nContent-Range: bytes 0-0/{}\r\nContent-Length: 1\r\nConnection: close\r\n\r\n",
                        corps.len()
                    );
                    let _ = s.write_all(&corps[..1]);
                    return;
                }
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/flac\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    corps.len()
                );
                let _ = s.write_all(&corps);
            });
        }
    });
    (format!("http://127.0.0.1:{port}/piste.flac"), n)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn une_seule_connexion_amont_par_piste_sur_la_voie_mandataire_9450() {
    let fichier: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    let (amont, connexions) = cdn_qui_compte(fichier.clone());
    let sid = "s9450-une-connexion";
    let info = StreamInfo {
        format: "flac".into(),
        mime_type: "audio/flac".into(),
        sample_rate: 96_000,
        bit_depth: 24,
        channels: 2,
        ..StreamInfo::default()
    };
    let session = StreamSession::new(sid.to_string(), info, false, 128);
    *session.proxy_url.lock().await = Some(amont);
    let session = Arc::new(session);
    let sessions: SharedSessions = Arc::new(tokio::sync::Mutex::new(
        [(sid.to_string(), session.clone())].into_iter().collect(),
    ));

    // La sonde se branche à la création de la session, avant le renderer.
    let lecteur = session.prise_des_niveaux.brancher(Box::new(|| false));
    let sonde = std::thread::spawn(move || {
        let mut lecteur = lecteur;
        let mut v = Vec::new();
        lecteur.read_to_end(&mut v).unwrap();
        v
    });

    // Le renderer.
    let mut h = HeaderMap::new();
    h.insert("User-Agent", "Renderer/1.0".parse().unwrap());
    h.insert("Range", "bytes=0-".parse().unwrap());
    let rep = handle_stream(Path(format!("{sid}.flac")), State(sessions.clone()), h).await;
    assert_eq!(rep.status(), StatusCode::PARTIAL_CONTENT);
    let mut flux = rep.into_body().into_data_stream();
    let mut servi = Vec::new();
    while let Some(m) = flux.next().await {
        servi.extend_from_slice(&m.unwrap());
    }
    assert_eq!(servi, fichier, "le renderer reçoit le fichier verbatim");

    let lu = tokio::task::spawn_blocking(move || sonde.join().unwrap())
        .await
        .unwrap();
    assert_eq!(lu, fichier, "la sonde lit les mêmes octets, dans l'ordre");
    assert_eq!(
        connexions.load(Ordering::SeqCst),
        1,
        "une seule connexion amont pour la piste (#9450)"
    );
}

/// #9450, revue — un renderer qui fait un HEAD AVANT tout GET : la longueur
/// n'est plus apprise du GET de la sonde (qui n'existe plus). Le HEAD la
/// demande à l'amont (`bytes=0-0`) et annonce un fichier recherchable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_head_d_abord_annonce_la_longueur_9450() {
    let fichier = vec![5u8; 123_456];
    let (amont, _connexions) = cdn_qui_compte(fichier.clone());
    let sid = "s9450-head-d-abord";
    let info = StreamInfo {
        format: "flac".into(),
        mime_type: "audio/flac".into(),
        sample_rate: 44_100,
        bit_depth: 16,
        channels: 2,
        ..StreamInfo::default()
    };
    let session = StreamSession::new(sid.to_string(), info, false, 128);
    *session.proxy_url.lock().await = Some(amont);
    let session = Arc::new(session);
    let sessions: SharedSessions = Arc::new(tokio::sync::Mutex::new(
        [(sid.to_string(), session.clone())].into_iter().collect(),
    ));
    let mut h = HeaderMap::new();
    h.insert("User-Agent", "Renderer/1.0".parse().unwrap());
    let rep = handle_head(Path(format!("{sid}.flac")), State(sessions), h).await;
    assert_eq!(
        rep.headers()
            .get("Content-Length")
            .and_then(|v| v.to_str().ok()),
        Some("123456"),
        "le HEAD d'abord doit annoncer la longueur de l'amont"
    );
    assert!(
        rep.headers()
            .get("contentFeatures.dlna.org")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("DLNA.ORG_OP=01")),
        "recherche par octets annoncée"
    );
    assert_eq!(session.longueur_amont(), Some(123_456));
}
