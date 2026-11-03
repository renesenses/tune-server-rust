//! #6612 — une plage qui commence AU-DELÀ de la longueur connue ne part pas
//! au CDN.
//!
//! Fil 2346 (Cyrille, 1.0.0-rc6) : Yamaha R-N2000A en DLNA, Qobuz relayé. À
//! la reprise après une pause, le Yamaha rouvre avec
//! `Range: bytes=4294949906-` (2^32 − 17 390) pour un fichier de 75,7 Mo
//! (`content_length_amont=75738034`). `proxy_stream` transmettait la plage au
//! CDN (`proxy_forward_resume_range`), qui répondait 416, renvoyé tel quel :
//! le renderer restait à l'arrêt.
//!
//! Décision de Bertrand (20/10) : la plage ne part plus au CDN ; Tune rend le
//! 416 lui-même et NOTE la plage sur la session, que le sondeur relit pour
//! reprendre la zone par un nouveau flux à la position mémorisée (voir
//! `tune-core/src/orchestrator/plage_hors_fichier_6612.rs`). Rien ne change
//! pour une plage valide, une longueur inconnue ou un agent Lavf. La voie
//! fichier avait le même trou : même règle.
//!
//! Le CDN factice COMPTE les requêtes qu'il reçoit et retient leur `Range`.
//!
//! `tune-stream-http` n'a PAS `autotests = false` : ce fichier est compilé
//! sans déclaration `[[test]]`.
//!
//! Refs renesenses/tune-server-rust#6612

use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tune_core::http::streamer::{SharedSessions, StreamInfo, StreamSession};

const YAMAHA: &str = "Network_Module/1.0 (R-N2000A)";
const LAVF: &str = "Lavf/58.45.100";
/// La plage du journal de Cyrille : 2^32 − 17 390.
const PLAGE_DU_YAMAHA: &str = "bytes=4294949906-";

/// Les `Range` reçus par le CDN, une entrée par requête (`-` sans `Range`).
type Recues = Arc<Mutex<Vec<String>>>;

/// Un CDN factice, comme Akamai : `bytes=N-` avec N dans le fichier ⇒ 206
/// exact ; N au-delà ⇒ 416 `bytes */total` ; sans `Range` ⇒ 200.
async fn cdn(corps: Vec<u8>) -> (String, Recues) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/file/track.flac?etsp=1790000000&sig=cafe6612",
        listener.local_addr().unwrap()
    );
    let recues: Recues = Arc::default();
    let journal = recues.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let corps = corps.clone();
            let journal = journal.clone();
            tokio::spawn(async move {
                let mut requete = Vec::new();
                let mut octet = [0u8; 1];
                while !requete.ends_with(b"\r\n\r\n") && requete.len() < 16_384 {
                    if socket.read_exact(&mut octet).await.is_err() {
                        return;
                    }
                    requete.push(octet[0]);
                }
                let texte = String::from_utf8_lossy(&requete).to_string();
                let range = texte.lines().find_map(|l| {
                    l.strip_prefix("Range: ")
                        .or(l.strip_prefix("range: "))
                        .map(str::to_string)
                });
                journal
                    .lock()
                    .unwrap()
                    .push(range.clone().unwrap_or_else(|| "-".into()));
                let debut = range
                    .as_deref()
                    .and_then(|r| r.strip_prefix("bytes="))
                    .and_then(|r| r.split('-').next()?.parse::<usize>().ok());
                let total = corps.len();
                let (statut, entetes, tranche): (&str, String, &[u8]) = match debut {
                    Some(n) if n < total => (
                        "206 Partial Content",
                        format!(
                            "Content-Range: bytes {n}-{}/{total}\r\nContent-Length: {}\r\n",
                            total - 1,
                            total - n
                        ),
                        &corps[n..],
                    ),
                    Some(_) => (
                        "416 Range Not Satisfiable",
                        format!("Content-Range: bytes */{total}\r\nContent-Length: 0\r\n"),
                        &[],
                    ),
                    None => ("200 OK", format!("Content-Length: {total}\r\n"), &corps[..]),
                };
                let entete = format!(
                    "HTTP/1.1 {statut}\r\nContent-Type: audio/flac\r\nAccept-Ranges: bytes\r\n{entetes}Connection: close\r\n\r\n"
                );
                let _ = socket.write_all(entete.as_bytes()).await;
                let _ = socket.write_all(tranche).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (url, recues)
}

fn info_flac() -> StreamInfo {
    StreamInfo {
        format: "flac".into(),
        mime_type: "audio/flac".into(),
        sample_rate: 96_000,
        bit_depth: 24,
        channels: 2,
        ..StreamInfo::default()
    }
}

async fn sessions_de(id: &str, session: StreamSession) -> (SharedSessions, Arc<StreamSession>) {
    let session = Arc::new(session);
    let sessions: SharedSessions = Arc::new(tokio::sync::Mutex::new(
        [(id.to_string(), session.clone())].into_iter().collect(),
    ));
    (sessions, session)
}

async fn session_mandataire(id: &str, amont: String) -> (SharedSessions, Arc<StreamSession>) {
    let session = StreamSession::new(id.to_string(), info_flac(), false, 16);
    *session.proxy_url.lock().await = Some(amont);
    sessions_de(id, session).await
}

/// `GET` du flux : (statut, `Content-Range` servi, octets reçus).
async fn get(
    sessions: &SharedSessions,
    id: &str,
    agent: &str,
    range: Option<&str>,
) -> (u16, Option<String>, usize) {
    let mut h = axum::http::HeaderMap::new();
    h.insert("User-Agent", agent.parse().unwrap());
    if let Some(r) = range {
        h.insert("Range", r.parse().unwrap());
    }
    let rep =
        tune_stream_http::handle_stream(Path(format!("{id}.flac")), State(sessions.clone()), h)
            .await;
    let statut = rep.status().as_u16();
    let content_range = rep
        .headers()
        .get("Content-Range")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let mut flux = rep.into_body().into_data_stream();
    let mut n = 0;
    while let Some(bloc) = tokio::time::timeout(std::time::Duration::from_secs(10), flux.next())
        .await
        .expect("le corps doit arriver")
    {
        n += bloc.expect("bloc lisible").len();
    }
    (statut, content_range, n)
}

fn recues(r: &Recues) -> Vec<String> {
    r.lock().unwrap().clone()
}

const TOTAL: usize = 64_000;

fn corps() -> Vec<u8> {
    (0..TOTAL).map(|i| (i % 251) as u8).collect()
}

/// 🔴 Le cas du fil 2346 : longueur connue, plage hors fichier ⇒ aucune
/// requête au CDN, 416 `bytes */total`, et la plage est notée sur la session
/// pour la reprise par un nouveau flux.
#[tokio::test]
async fn plage_hors_fichier_ne_part_pas_au_cdn_et_demande_la_reprise_6612() {
    let (amont, cdn_recues) = cdn(corps()).await;
    let id = "mandataire-6612";
    let (sessions, session) = session_mandataire(id, amont).await;

    // La sonde des VU-mètres (sans Range) apprend la longueur du CDN.
    assert_eq!(get(&sessions, id, "-", None).await, (200, None, TOTAL));
    assert_eq!(session.longueur_amont(), Some(TOTAL as u64));
    assert_eq!(recues(&cdn_recues), vec!["-".to_string()]);
    assert_eq!(session.age_plage_hors_fichier(), None);

    // La reprise du Yamaha.
    let (statut, content_range, _) = get(&sessions, id, YAMAHA, Some(PLAGE_DU_YAMAHA)).await;
    assert_eq!(statut, 416, "la plage ne désigne aucun octet du flux");
    assert_eq!(
        content_range.as_deref(),
        Some(format!("bytes */{TOTAL}").as_str())
    );
    assert_eq!(
        recues(&cdn_recues),
        vec!["-".to_string()],
        "#6612 : une plage au-delà de la longueur connue ne doit PAS partir au CDN"
    );
    assert!(
        session.age_plage_hors_fichier().is_some(),
        "#6612 : la session doit noter la plage, pour la reprise par un nouveau flux"
    );
}

/// Une plage valide part au CDN exactement comme avant, et ne note rien.
#[tokio::test]
async fn plage_valide_inchangee_6612() {
    let (amont, cdn_recues) = cdn(corps()).await;
    let id = "mandataire-6612-valide";
    let (sessions, session) = session_mandataire(id, amont).await;
    assert_eq!(get(&sessions, id, "-", None).await, (200, None, TOTAL));

    let (statut, content_range, lus) = get(&sessions, id, YAMAHA, Some("bytes=1000-")).await;
    assert_eq!(statut, 206);
    assert_eq!(
        content_range.as_deref(),
        Some(format!("bytes 1000-{}/{TOTAL}", TOTAL - 1).as_str())
    );
    assert_eq!(lus, TOTAL - 1000);
    assert_eq!(
        recues(&cdn_recues),
        vec!["-".to_string(), "bytes=1000-".to_string()],
        "la reprise valide est transmise au CDN"
    );
    // La dernière plage valide (le dernier octet) aussi.
    let (statut, _, lus) = get(
        &sessions,
        id,
        YAMAHA,
        Some(&format!("bytes={}-", TOTAL - 1)),
    )
    .await;
    assert_eq!((statut, lus), (206, 1));
    assert_eq!(recues(&cdn_recues).len(), 3);
    assert_eq!(session.age_plage_hors_fichier(), None);
}

/// Longueur inconnue (aucun `GET` ne l'a encore apprise) : la plage part au
/// CDN comme avant, et le 416 du CDN est rendu tel quel. Rien n'est noté.
#[tokio::test]
async fn longueur_inconnue_inchangee_6612() {
    let (amont, cdn_recues) = cdn(corps()).await;
    let id = "mandataire-6612-inconnue";
    let (sessions, session) = session_mandataire(id, amont).await;
    assert_eq!(session.longueur_amont(), None);

    let (statut, content_range, _) = get(&sessions, id, YAMAHA, Some(PLAGE_DU_YAMAHA)).await;
    assert_eq!(statut, 416);
    assert_eq!(
        content_range.as_deref(),
        Some(format!("bytes */{TOTAL}").as_str())
    );
    assert_eq!(
        recues(&cdn_recues),
        vec![PLAGE_DU_YAMAHA.to_string()],
        "longueur inconnue : la plage part au CDN, comme avant"
    );
    assert_eq!(session.age_plage_hors_fichier(), None);
}

/// Un agent Lavf : rien ne change, même hors fichier.
#[tokio::test]
async fn lavf_inchange_6612() {
    let (amont, cdn_recues) = cdn(corps()).await;
    let id = "mandataire-6612-lavf";
    let (sessions, session) = session_mandataire(id, amont).await;
    assert_eq!(get(&sessions, id, "-", None).await, (200, None, TOTAL));

    let (statut, _, _) = get(&sessions, id, LAVF, Some(PLAGE_DU_YAMAHA)).await;
    assert_eq!(statut, 416);
    assert_eq!(
        recues(&cdn_recues),
        vec!["-".to_string(), PLAGE_DU_YAMAHA.to_string()],
        "Lavf : la plage part au CDN, comme avant"
    );
    assert_eq!(session.age_plage_hors_fichier(), None);
}

/// La voie FICHIER avait le même trou : le 416 local laissait le renderer à
/// l'arrêt. Même règle : la plage est notée ; une plage valide ne note rien.
#[tokio::test]
async fn fichier_local_meme_regle_6612() {
    let fichier = tune_core::test_scratch::scratch_file("6612-fichier", ".flac");
    let chemin = fichier.path().to_path_buf();
    std::fs::write(&chemin, corps()).expect("fichier de test");
    let id = "fichier-6612";
    let session = StreamSession::new(id.to_string(), info_flac(), false, 16);
    *session.file_path.lock().await = Some(chemin.to_string_lossy().into_owned());
    let (sessions, session) = sessions_de(id, session).await;

    let (statut, _, lus) = get(&sessions, id, YAMAHA, Some("bytes=1000-")).await;
    assert_eq!((statut, lus), (206, TOTAL - 1000));
    assert_eq!(
        session.age_plage_hors_fichier(),
        None,
        "plage valide : rien"
    );

    let (statut, content_range, _) = get(&sessions, id, LAVF, Some(PLAGE_DU_YAMAHA)).await;
    assert_eq!(statut, 416);
    assert_eq!(
        content_range.as_deref(),
        Some(format!("bytes */{TOTAL}").as_str())
    );
    assert_eq!(
        session.age_plage_hors_fichier(),
        None,
        "Lavf : rien ne change"
    );

    let (statut, _, _) = get(&sessions, id, YAMAHA, Some(PLAGE_DU_YAMAHA)).await;
    assert_eq!(statut, 416);
    assert!(
        session.age_plage_hors_fichier().is_some(),
        "#6612 : la voie fichier doit noter la plage hors fichier"
    );
}
