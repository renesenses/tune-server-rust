//! #5975 — zone navigateur sous Safari 18.6 : une piste de la bibliothèque
//! convertie en WAV, dont la base ignore la durée, ne démarre pas.
//!
//! La radio joue depuis #6669 : Safari (CoreMedia) ouvre chaque média par
//! la sonde `Range: bytes=0-1` et exige un 206 exact. Une conversion WAV
//! SANS durée connue n'a pas de `wav_length` : la sonde recevait un 200
//! chunké, sans longueur ni plage, le contrat que Safari refusait déjà sur la
//! radio. Ces témoins rejouent les requêtes de Safari (agent exact,
//! `Accept: */*`) contre le vrai routeur, servi sur une socket.
use super::*;
use std::sync::{Arc, atomic::Ordering::SeqCst};
use std::time::Duration;
use tune_core::http::streamer::LIVE_BOUNDED_TOTAL_LEN;

/// L'agent de Safari 18.6 sous macOS.
const SAFARI_MAC: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.6 Safari/605.1.15";
const FIREFOX: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:140.0) Gecko/20100101 Firefox/140.0";

fn octet(i: usize) -> u8 {
    ((i % 251) as u8).wrapping_add((i / 65_536) as u8)
}

/// Le flux tel que le producteur l'émet pour une durée inconnue : en-tête WAV
/// (dans le canal) puis `pcm` octets identifiables.
fn flux_source(pcm: usize) -> Vec<u8> {
    let mut v = tune_core::audio::wav::build_wav_header(2, 44_100, 16).to_vec();
    v.extend((0..pcm).map(octet));
    v
}

/// Une conversion WAV de la zone navigateur dont la base ignore la durée
/// (`duration_ms = Some(0)`, comme `resolve_local` la pose), servie par le
/// vrai routeur. Rend l'URL et la session.
async fn servir(id: &str, source: Vec<u8>) -> (String, Arc<StreamSession>) {
    let info = StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 44_100,
        channels: 2,
        bit_depth: 16,
        duration_ms: Some(0),
        ..StreamInfo::default()
    };
    assert_eq!(
        info.wav_content_length(),
        None,
        "le cas visé : longueur inconnue"
    );
    let session = Arc::new(StreamSession::new(id.into(), info, false, 64));
    session.wav_header_included.store(true, SeqCst);
    let tx = session.tx.lock().await.clone().expect("tx");
    session.close_sender().await;
    tokio::spawn(async move {
        for bloc in source.chunks(32_768) {
            if tx.send(bloc.to_vec()).await.is_err() {
                return;
            }
        }
    });
    let sessions: tune_core::http::streamer::SharedSessions = Arc::new(tokio::sync::Mutex::new(
        [(id.to_string(), session.clone())].into_iter().collect(),
    ));
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(ecoute, router(sessions)).await.ok();
    });
    (format!("http://127.0.0.1:{port}/stream/{id}.wav"), session)
}

async fn demander(url: &str, agent: &str, range: &str) -> reqwest::Response {
    tune_core::http::client::builder()
        .build()
        .expect("client HTTP")
        .get(url)
        .header("User-Agent", agent)
        .header("Accept", "*/*")
        .header("Range", range)
        .send()
        .await
        .unwrap_or_else(|e| panic!("`Range: {range}` : aucune réponse — {e:?}"))
}

fn entete(r: &reqwest::Response, nom: &str) -> String {
    r.headers()
        .get(nom)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// Lit ce que le corps livre, sans juger de sa fin.
async fn lire_ce_qui_vient(mut r: reqwest::Response) -> Vec<u8> {
    let mut recu = Vec::new();
    while let Ok(Ok(Some(b))) = tokio::time::timeout(Duration::from_secs(10), r.chunk()).await {
        recu.extend_from_slice(&b);
    }
    recu
}

/// Lit le corps jusqu'au bout, en exigeant une fin PROPRE : une connexion
/// coupée avant la fin perdrait le dernier bloc de la piste.
async fn lire_jusqu_au_bout(mut r: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut recu = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), r.chunk()).await {
            Err(_) => return Err(format!("corps muet après {} octets", recu.len())),
            Ok(Err(e)) => {
                return Err(format!(
                    "corps interrompu après {} octets : {e}",
                    recu.len()
                ));
            }
            Ok(Ok(None)) => return Ok(recu),
            Ok(Ok(Some(b))) => recu.extend_from_slice(&b),
        }
    }
}

/// LE DÉFAUT. La sonde de Safari sur une conversion sans durée recevait
/// `200 OK` sans `Content-Range` : CoreMedia refuse le média.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn la_sonde_de_safari_sur_une_piste_sans_duree_recoit_un_206_exact() {
    let (url, _session) = servir("i5975-sonde", flux_source(200_000)).await;
    let sonde = demander(&url, SAFARI_MAC, "bytes=0-1").await;
    assert_eq!(
        sonde.status(),
        reqwest::StatusCode::PARTIAL_CONTENT,
        "#5975 : la sonde `bytes=0-1` de Safari sur une conversion sans durée doit \
         recevoir un 206, pas le contrat chunké que CoreMedia refuse"
    );
    assert_eq!(
        entete(&sonde, "Content-Range"),
        format!("bytes 0-1/{LIVE_BOUNDED_TOTAL_LEN}")
    );
    assert_eq!(entete(&sonde, "Content-Length"), "2");
    assert_eq!(entete(&sonde, "Accept-Ranges"), "bytes");
    assert_eq!(entete(&sonde, "Transfer-Encoding"), "");
    assert_eq!(lire_ce_qui_vient(sonde).await, b"RI".to_vec());
}

/// Après la sonde, la vraie lecture (`bytes=0-`) reçoit un 206 et la piste
/// ENTIÈRE depuis son premier octet : la sonde n'a rien fait perdre.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apres_la_sonde_la_lecture_de_safari_part_du_debut_sans_perte() {
    let source = flux_source(200_000);
    let (url, _session) = servir("i5975-suite", source.clone()).await;
    let _ = lire_ce_qui_vient(demander(&url, SAFARI_MAC, "bytes=0-1").await).await;
    let lecture = demander(&url, SAFARI_MAC, "bytes=0-").await;
    assert_eq!(lecture.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        entete(&lecture, "Content-Range"),
        format!(
            "bytes 0-{}/{LIVE_BOUNDED_TOTAL_LEN}",
            LIVE_BOUNDED_TOTAL_LEN - 1
        )
    );
    assert_eq!(
        entete(&lecture, "Content-Length"),
        "",
        "le total est un plafond : un Content-Length jusqu'à lui ferait couper la fin"
    );
    let recu = lire_jusqu_au_bout(lecture)
        .await
        .expect("la piste doit finir proprement");
    assert_eq!(recu.len(), source.len(), "la piste doit arriver en entier");
    assert!(
        recu == source,
        "la piste doit arriver à l'octet, en-tête compris"
    );
}

/// Une fois la conversion finie, une reprise au-delà de sa fin reçoit le 416
/// d'un fichier, qui donne la longueur vraie — pas un 206 vide.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_reprise_de_safari_apres_la_fin_recoit_un_416() {
    let source = flux_source(100_000);
    let (url, _session) = servir("i5975-fin", source.clone()).await;
    let recu = lire_ce_qui_vient(demander(&url, SAFARI_MAC, "bytes=0-").await).await;
    assert_eq!(recu.len(), source.len());
    let reprise = demander(&url, SAFARI_MAC, &format!("bytes={}-", source.len())).await;
    assert_eq!(reprise.status(), reqwest::StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        entete(&reprise, "Content-Range"),
        format!("bytes */{}", source.len())
    );
}

/// Témoin négatif : Firefox garde le contrat chunké d'avant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn firefox_garde_le_contrat_chunke() {
    let (url, _session) = servir("i5975-firefox", flux_source(100_000)).await;
    let r = demander(&url, FIREFOX, "bytes=0-1").await;
    assert_eq!(r.status(), reqwest::StatusCode::OK);
    assert_eq!(entete(&r, "Content-Range"), "");
    assert_eq!(entete(&r, "Accept-Ranges"), "");
}
