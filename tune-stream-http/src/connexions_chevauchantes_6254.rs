//! #6254 (second relevé) — connexions `bytes=0-` qui se chevauchent sur une
//! conversion WAV progressive chunkée, et phase du tuyau après l'en-tête.
//!
//! Journal du .18 (13/10/2026 08:51, zone 10 = Eversolo DMP-A8,
//! `Lavf/58.45.100`, FLAC 44,1/16 → WAV chunké, pas de `wav_length`) : trois
//! `bytes=0-` — A ferme après 3,5 Mo, B et C rejouent depuis 0 la retenue, B
//! reste ouverte 138 s pendant que C tire 27 Mo en 1,5 s. Question : une
//! connexion peut-elle perdre des blocs tirés par une autre, ou recevoir un
//! flux décalé d'une fraction de trame ?
use super::*;
use futures_util::StreamExt;
use std::sync::{Arc, atomic::Ordering::SeqCst};

const LAVF: &str = "Lavf/58.45.100";

/// Octet `i` du flux : un décalage ou un saut se voit.
fn octet(i: usize) -> u8 {
    ((i % 251) as u8) ^ ((i / 251) as u8)
}

fn flux(canaux: u16, taux: u32, bits: u16, pcm: usize) -> Vec<u8> {
    let mut v = tune_core::audio::wav::build_wav_header(canaux, taux, bits).to_vec();
    v.extend((44..44 + pcm).map(octet));
    v
}

/// Une conversion WAV progressive (pas de durée : chunkée), dont le
/// producteur émet l'en-tête et pousse `blocs` dans le canal.
async fn session_chunkee(
    id: &str,
    taux: u32,
    bits: u16,
    blocs: Vec<Vec<u8>>,
) -> (
    tune_core::http::streamer::SharedSessions,
    Arc<StreamSession>,
    tokio::task::JoinHandle<()>,
) {
    let info = StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: taux,
        channels: 2,
        bit_depth: bits,
        duration_ms: None,
        ..StreamInfo::default()
    };
    let session = Arc::new(StreamSession::new(id.into(), info, false, 16));
    session.wav_header_included.store(true, SeqCst);
    let tx = session.tx.lock().await.clone().expect("tx");
    session.close_sender().await;
    let producteur = tokio::spawn(async move {
        for bloc in blocs {
            if tx.send(bloc).await.is_err() {
                return;
            }
        }
    });
    let sessions: tune_core::http::streamer::SharedSessions = Arc::new(tokio::sync::Mutex::new(
        [(id.to_string(), session.clone())].into_iter().collect(),
    ));
    (sessions, session, producteur)
}

async fn ouvrir(
    sessions: &tune_core::http::streamer::SharedSessions,
    id: &str,
) -> axum::body::BodyDataStream {
    let mut req = HeaderMap::new();
    req.insert("Range", "bytes=0-".parse().unwrap());
    req.insert("User-Agent", LAVF.parse().unwrap());
    handle_stream(
        axum::extract::Path(format!("{id}.wav")),
        axum::extract::State(sessions.clone()),
        req,
    )
    .await
    .into_body()
    .into_data_stream()
}

/// Lit au plus `n` octets (tout si `None`) ; s'arrête à la fin du corps.
async fn lire(corps: &mut axum::body::BodyDataStream, n: Option<usize>, recu: &mut Vec<u8>) {
    while n.is_none_or(|n| recu.len() < n) {
        match tokio::time::timeout(std::time::Duration::from_secs(10), corps.next()).await {
            Ok(Some(Ok(b))) => recu.extend_from_slice(&b),
            Ok(_) => return,
            Err(_) => panic!("le corps est resté muet 10 s ({} octets reçus)", recu.len()),
        }
    }
}

fn premier_ecart(recu: &[u8], attendu: &[u8]) -> Option<usize> {
    recu.iter().zip(attendu).position(|(a, b)| a != b)
}

fn verifier_prefixe(quoi: &str, bits: u16, recu: &[u8], source: &[u8]) {
    assert!(
        recu.len() <= source.len(),
        "{bits} bits, {quoi} : {} octets reçus pour un flux de {}",
        recu.len(),
        source.len()
    );
    if let Some(i) = premier_ecart(recu, source) {
        panic!(
            "{bits} bits, {quoi} : l'octet {i} diffère du flux de référence — un bloc perdu ou \
             un décalage de {} octets modulo la trame",
            i % (2 * bits as usize / 8)
        );
    }
}

/// Le scénario du journal, en 16 et en 24 bits, avec des blocs de canal de
/// 65 536 octets (4 modulo une trame de 6). Chaque connexion doit recevoir un
/// PRÉFIXE EXACT du flux de référence, et la dernière le flux entier.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trois_bytes_0_chevauchants_recoivent_chacun_le_flux_exact() {
    for bits in [16u16, 24] {
        let source = flux(2, 44_100, bits, 6_000_000);
        let blocs: Vec<Vec<u8>> = source.chunks(65_536).map(<[u8]>::to_vec).collect();
        let id = format!("i6254-chevauche-{bits}");
        let (sessions, _session, producteur) = session_chunkee(&id, 44_100, bits, blocs).await;

        // A : la sonde, tire 1 Mo puis ferme.
        let mut a = ouvrir(&sessions, &id).await;
        let mut recu_a = Vec::new();
        lire(&mut a, Some(1_000_000), &mut recu_a).await;
        drop(a);
        verifier_prefixe("connexion A", bits, &recu_a, &source);

        // B : rejoue depuis 0, lit un peu, puis reste ouverte sans lire.
        let mut b = ouvrir(&sessions, &id).await;
        let mut recu_b = Vec::new();
        lire(&mut b, Some(300_000), &mut recu_b).await;

        // C : rejoue depuis 0 et tire tout, pendant que B est ouverte.
        let mut c = ouvrir(&sessions, &id).await;
        let mut recu_c = Vec::new();
        lire(&mut c, None, &mut recu_c).await;
        verifier_prefixe("connexion C", bits, &recu_c, &source);
        assert_eq!(
            recu_c.len(),
            source.len(),
            "{bits} bits, connexion C : le flux entier est attendu"
        );

        // B reprend : elle a cédé le canal, mais ce qu'elle a rendu est exact.
        lire(&mut b, None, &mut recu_b).await;
        verifier_prefixe("connexion B", bits, &recu_b, &source);
        producteur.await.unwrap();
    }
}

/// L'en-tête mis de côté, puis le tuyau là où il en est
/// (`debut_de_piste_perdu`) : le premier octet après l'en-tête doit ouvrir une
/// trame. Les blocs du canal sont décalés de 2 octets par rapport à la
/// grille (premier bloc = en-tête + 2 octets), comme un producteur qui
/// découpe un flux d'octets sans souci des trames.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l_entete_rejoue_puis_le_direct_reste_en_phase() {
    for bits in [16u16, 24] {
        let trame = 2 * u64::from(bits / 8);
        // 8 kHz : la fenêtre de retenue tombe à son plancher (8 Mio).
        let source = flux(2, 8_000, bits, 14_000_000);
        let mut blocs = vec![source[..46].to_vec()];
        blocs.extend(source[46..].chunks(6 * 4096).map(<[u8]>::to_vec));
        let id = format!("i6254-phase-{bits}");
        let (sessions, session, producteur) = session_chunkee(&id, 8_000, bits, blocs).await;

        // Une première connexion tire assez pour que la retenue glisse.
        let mut premiere = ouvrir(&sessions, &id).await;
        let mut recu = Vec::new();
        lire(&mut premiere, Some(11_000_000), &mut recu).await;
        drop(premiere);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            session.etendue_retenue().0 > 0,
            "mise en scène : la retenue doit avoir glissé"
        );
        let tuyau = session.octets_du_canal.load(SeqCst);
        assert_ne!(
            (tuyau - 44) % trame,
            0,
            "mise en scène : le tuyau doit être hors trame"
        );

        // La lecture depuis le début : l'en-tête, puis le direct.
        let mut seconde = ouvrir(&sessions, &id).await;
        let mut recu = Vec::new();
        lire(&mut seconde, Some(44 + 65_536), &mut recu).await;
        assert_eq!(
            &recu[..44],
            &source[..44],
            "{bits} bits : l'en-tête d'abord"
        );
        let depart = tuyau + (44 + trame - tuyau % trame) % trame;
        assert_eq!((depart - 44) % trame, 0);
        let d = depart as usize;
        let n = recu.len() - 44;
        if recu[44..] != source[d..d + n] {
            let au_tuyau = recu[44..] == source[tuyau as usize..tuyau as usize + n];
            panic!(
                "{bits} bits : après l'en-tête, le direct doit partir de la frontière de trame \
                 {depart} (tuyau à {tuyau}) ; il part {} — chaque échantillon est lu de \
                 travers, c'est du bruit blanc",
                if au_tuyau {
                    format!("de l'octet {tuyau}, hors trame")
                } else {
                    "d'ailleurs".to_string()
                }
            );
        }
        drop(seconde);
        producteur.abort();
    }
}
