//! #6254 — une reprise WAV au milieu d'une trame se sert depuis la frontière
//! de trame qui la précède.
//!
//! Journal du .18 (13/10/2026, 1.0.0-rc4, zone 10 = Eversolo DMP-A8, agent
//! `Lavf/58.45.100`) : FLAC 44,1/24 converti en WAV, flux de 63 897 644
//! octets. `bytes=0-`, `bytes=44-` (36 239 196 octets tirés), puis
//! `bytes=31958520-`, servi à l'octet depuis la retenue (#5426). Or
//! (31958520 − 44) mod 6 = 4 : le lecteur décode la nouvelle connexion comme
//! si elle commençait sur une trame — bruit blanc fort à 120,78 s, la position
//! exacte de la reprise.
use super::*;
use futures_util::StreamExt;
use std::sync::{Arc, atomic::Ordering::SeqCst};

const LAVF: &str = "Lavf/58.45.100";
const NAVIGATEUR: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:140.0) Gecko/20100101 Firefox/140.0";
/// La longueur du flux du journal.
const LONGUEUR: u64 = 63_897_644;
const REPRISE: u64 = 31_958_520;
/// `44 + ((31958520 − 44) / 6) × 6`.
const REPRISE_ALIGNEE: u64 = 31_958_516;

/// Octet `i` du FLUX (en-tête compris) : un décalage de quelques octets se voit.
fn octet(i: usize) -> u8 {
    ((i % 251) as u8) ^ ((i / 251) as u8)
}

fn flux_source() -> Vec<u8> {
    let mut v = tune_core::audio::wav::build_wav_header(2, 44_100, 24).to_vec();
    v.extend((44..LONGUEUR as usize).map(octet));
    v
}

async fn demander(
    sessions: &tune_core::http::streamer::SharedSessions,
    id: &str,
    range: &str,
    agent: &str,
) -> Response {
    let mut req = HeaderMap::new();
    req.insert("Range", range.parse().unwrap());
    req.insert("User-Agent", agent.parse().unwrap());
    handle_stream(
        axum::extract::Path(format!("{id}.wav")),
        axum::extract::State(sessions.clone()),
        req,
    )
    .await
}

fn entete(r: &Response, nom: &str) -> String {
    r.headers()
        .get(nom)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// Lit au moins `n` octets du corps, puis abandonne la connexion.
async fn lire(r: Response, n: usize) -> Vec<u8> {
    let mut corps = r.into_body().into_data_stream();
    let mut recu = Vec::with_capacity(n);
    while recu.len() < n {
        let b = tokio::time::timeout(std::time::Duration::from_secs(10), corps.next())
            .await
            .expect("le corps doit avancer")
            .expect("le corps ne doit pas finir si tôt")
            .expect("corps");
        recu.extend_from_slice(&b);
    }
    recu
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn la_reprise_du_journal_part_d_une_frontiere_de_trame() {
    let source = Arc::new(flux_source());
    let info = StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 44_100,
        channels: 2,
        bit_depth: 24,
        duration_ms: Some(241_490),
        file_size: Some(LONGUEUR),
        ..StreamInfo::default()
    };
    let id = "i6254-eversolo";
    let session = Arc::new(StreamSession::new(id.into(), info, false, 64));
    session.wav_header_included.store(true, SeqCst);
    let tx = session.tx.lock().await.clone().expect("tx");
    session.close_sender().await;
    let src = source.clone();
    let producteur = tokio::spawn(async move {
        for bloc in src.chunks(32_768) {
            if tx.send(bloc.to_vec()).await.is_err() {
                return;
            }
        }
    });
    let sessions: tune_core::http::streamer::SharedSessions = Arc::new(tokio::sync::Mutex::new(
        [(id.to_string(), session.clone())].into_iter().collect(),
    ));

    // ── Connexion 1 : `bytes=44-`, qui tire 36 Mo puis se ferme ──
    let premiere = demander(&sessions, id, "bytes=44-", LAVF).await;
    assert_eq!(premiere.status(), StatusCode::PARTIAL_CONTENT);
    let recu = lire(premiere, 36_239_196).await;
    assert_eq!(&recu[..4096], &source[44..44 + 4096]);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let (retenue_debut, retenue_fin) = session.etendue_retenue();
    assert!(
        retenue_debut <= REPRISE_ALIGNEE && REPRISE < retenue_fin,
        "mise en scène : la reprise doit tomber dans la retenue ({retenue_debut}..{retenue_fin})"
    );

    // ── Connexion 2 : la reprise du journal, au milieu d'une trame ──
    let seconde = demander(&sessions, id, &format!("bytes={REPRISE}-"), LAVF).await;
    assert_eq!(seconde.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        entete(&seconde, "Content-Range"),
        format!("bytes {REPRISE_ALIGNEE}-{}/{LONGUEUR}", LONGUEUR - 1),
        "`bytes={REPRISE}-` tombe 4 octets après une frontière de trame de 6 : le 206 doit \
         partir de la frontière ({REPRISE_ALIGNEE}), sans quoi le lecteur décode chaque \
         échantillon de travers — le bruit blanc de #6254"
    );
    assert_eq!(
        entete(&seconde, "Content-Length"),
        (LONGUEUR - REPRISE_ALIGNEE).to_string()
    );
    let suite = lire(seconde, 256 * 1024).await;
    let a = REPRISE_ALIGNEE as usize;
    assert!(
        suite[..] == source[a..a + suite.len()],
        "le premier octet servi doit être celui de l'offset {REPRISE_ALIGNEE} du flux"
    );

    // ── Contre-champ : un navigateur garde l'octet demandé ──
    let navigateur = demander(&sessions, id, &format!("bytes={REPRISE}-"), NAVIGATEUR).await;
    assert_eq!(
        entete(&navigateur, "Content-Range"),
        format!("bytes {REPRISE}-{}/{LONGUEUR}", LONGUEUR - 1),
        "un navigateur range la reprise dans un cache à l'octet : pas de réalignement"
    );
    let suite = lire(navigateur, 4096).await;
    let r = REPRISE as usize;
    assert!(suite[..] == source[r..r + suite.len()]);

    producteur.abort();
}

#[test]
fn l_alignement_suit_la_grille_des_trames() {
    assert_eq!(
        super::reprise_alignee_sur_la_trame(REPRISE, 6),
        REPRISE_ALIGNEE
    );
    // Déjà alignée, dans l'en-tête, ou trame inconnue : rien ne bouge.
    assert_eq!(
        super::reprise_alignee_sur_la_trame(44 + 6 * 7, 6),
        44 + 6 * 7
    );
    assert_eq!(super::reprise_alignee_sur_la_trame(43, 6), 43);
    assert_eq!(super::reprise_alignee_sur_la_trame(44, 6), 44);
    assert_eq!(super::reprise_alignee_sur_la_trame(49, 6), 44);
    assert_eq!(super::reprise_alignee_sur_la_trame(1_000_001, 1), 1_000_001);
    assert_eq!(super::reprise_alignee_sur_la_trame(1_000_001, 0), 1_000_001);
    // Stéréo 16 bits : trame de 4.
    assert_eq!(
        super::reprise_alignee_sur_la_trame(44 + 4 * 10 + 3, 4),
        44 + 4 * 10
    );
    assert!(super::agent_navigateur(Some(NAVIGATEUR)));
    assert!(!super::agent_navigateur(Some(LAVF)));
    assert!(!super::agent_navigateur(None));
}
