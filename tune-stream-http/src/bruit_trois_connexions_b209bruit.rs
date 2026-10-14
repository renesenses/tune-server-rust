//! Zone 10 du .18 (Eversolo DMP-A8, `Lavf/58.45.100`, 11/10 vers 07:06 UTC) :
//! Qobuz FLAC 44,1/16 « Rocket » converti en WAV chunké (égaliseur de zone,
//! `streaming_dsp_progressif_wav_target`), joué comme un FORT bruit blanc.
//!
//! Le journal porte trois `bytes=0-` en deux secondes, avec `Icy-MetaData: 1` :
//! - la 1re (sonde) tire 4,3 Mo puis ferme ;
//! - la 2e, rejouée depuis la retenue, tire 25 Mo en 1,4 s — elle est encore
//!   OUVERTE et LUE quand la 3e arrive (fermée 450 ms après) ;
//! - la 3e, rejouée elle aussi depuis 0, est celle qui joue.
//!
//! Le contrat gardé ici est celui d'un fichier : chacune des trois reçoit le
//! début du flux, à l'octet — un seul en-tête WAV, en tête, puis le PCM 16 bits
//! aligné sans trou ni recollage — même quand la 2e lit encore au moment où la
//! 3e la supersède.
use super::*;
use std::sync::{Arc, atomic::Ordering::SeqCst};
use std::time::Duration;

const LAVF: &str = "Lavf/58.45.100";
/// La trame d'un PCM 16 bits stéréo.
const TRAME: usize = 4;
/// Le lot du décodeur progressif : 32 768 octets, multiple de la trame.
const BLOC: usize = 32_768;

/// Trame `n` : gauche = `n` (16 bits bas), droit = `(n >> 16) ^ 0x5A5A`.
/// Les octets de poids fort ne valent jamais `R` ou `W` consécutifs d'un
/// en-tête : un `RIFF…WAVE` au milieu du corps ne peut venir que d'un en-tête.
fn trame(n: usize) -> [u8; TRAME] {
    let g = (n as u16).to_le_bytes();
    let d = (((n >> 16) as u16) ^ 0x5A5A).to_le_bytes();
    [g[0], g[1], d[0], d[1]]
}

fn flux_source(trames: usize) -> (Vec<u8>, Vec<Vec<u8>>) {
    let entete = tune_core::audio::wav::build_wav_header(2, 44_100, 16).to_vec();
    let pcm: Vec<u8> = (0..trames).flat_map(trame).collect();
    let mut blocs = vec![entete.clone()];
    blocs.extend(pcm.chunks(BLOC).map(<[u8]>::to_vec));
    let mut tout = entete;
    tout.extend_from_slice(&pcm);
    (tout, blocs)
}

/// La session de `ouvrir_la_session_wav` : WAV 16 bits, durée inconnue,
/// producteur qui émet l'en-tête, canal de 256 blocs comme en production.
async fn servir(id: &str, blocs: Vec<Vec<u8>>) -> (String, Arc<StreamSession>) {
    let info = StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 44_100,
        channels: 2,
        bit_depth: 16,
        duration_ms: None,
        ..StreamInfo::default()
    };
    let session = Arc::new(StreamSession::new(id.into(), info, false, 256));
    session.wav_header_included.store(true, SeqCst);
    let tx = session.tx.lock().await.clone().expect("tx");
    session.close_sender().await;
    tokio::spawn(async move {
        for bloc in blocs {
            if tx.send(bloc).await.is_err() {
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

async fn depuis_zero(url: &str, quoi: &str) -> reqwest::Response {
    tune_core::http::client::builder()
        .build()
        .expect("client HTTP")
        .get(url)
        .header("User-Agent", LAVF)
        .header("Range", "bytes=0-")
        .header("Icy-MetaData", "1")
        .send()
        .await
        .unwrap_or_else(|e| panic!("{quoi} : aucune réponse — {e:?}"))
}

async fn lire_au_moins(r: &mut reqwest::Response, au_moins: usize, quoi: &str) -> Vec<u8> {
    let mut recu = Vec::new();
    while recu.len() < au_moins {
        let b = tokio::time::timeout(Duration::from_secs(10), r.chunk())
            .await
            .unwrap_or_else(|_| panic!("{quoi} : corps muet après {} octets", recu.len()))
            .expect("corps")
            .unwrap_or_else(|| panic!("{quoi} : fin du corps après {} octets", recu.len()));
        recu.extend_from_slice(&b);
    }
    recu
}

async fn lire_jusqu_au_bout(mut r: reqwest::Response, mut recu: Vec<u8>, quoi: &str) -> Vec<u8> {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), r.chunk()).await {
            Err(_) => panic!("{quoi} : corps muet après {} octets", recu.len()),
            Ok(Err(e)) => panic!(
                "{quoi} : corps interrompu après {} octets : {e}",
                recu.len()
            ),
            Ok(Ok(None)) => return recu,
            Ok(Ok(Some(b))) => recu.extend_from_slice(&b),
        }
    }
}

/// Positions de tout `RIFF????WAVE` dans `recu`.
fn entetes(recu: &[u8]) -> Vec<usize> {
    recu.windows(12)
        .enumerate()
        .filter(|(_, w)| &w[..4] == b"RIFF" && &w[8..12] == b"WAVE")
        .map(|(i, _)| i)
        .collect()
}

fn verifier(quoi: &str, recu: &[u8], source: &[u8], entier: bool) {
    assert!(
        recu.len() > 44 + TRAME,
        "{quoi} : {} octets seulement",
        recu.len()
    );
    assert_eq!(
        entetes(recu),
        vec![0],
        "{quoi} : l'en-tête WAV doit figurer UNE fois, à l'octet 0"
    );
    if let Some(i) = recu.iter().zip(source).position(|(a, b)| a != b) {
        panic!(
            "{quoi} : premier octet faux à {i} (trame {}, décalage {} modulo la trame) — \
             PCM désaligné ou recollé",
            i.saturating_sub(44) / TRAME,
            i.saturating_sub(44) % TRAME
        );
    }
    assert!(
        recu.len() <= source.len(),
        "{quoi} : plus d'octets que le flux"
    );
    if entier {
        assert_eq!(recu.len(), source.len(), "{quoi} : flux incomplet");
    }
}

/// LE CAS DU JOURNAL : la 3e `bytes=0-` arrive pendant que la 2e LIT encore,
/// à pleine vitesse ; la 2e n'est fermée que 450 ms plus tard.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trois_bytes_0_lavf_la_deuxieme_lisant_encore_rendent_le_wav_attendu() {
    // 3 000 000 trames : 12 Mo, 68 s — sous la retenue (180 s), comme les
    // 25 Mo du journal sous ses 31,7 Mo.
    let (source, blocs) = flux_source(3_000_000);
    let (url, session) = servir("bruit-rocket", blocs).await;

    // ── 1 : la sonde, qui tire un peu puis ferme ──
    let mut sonde = depuis_zero(&url, "connexion 1").await;
    let lu1 = lire_au_moins(&mut sonde, 1_000_000, "connexion 1").await;
    drop(sonde);
    tokio::time::sleep(Duration::from_millis(300)).await;

    // ── 2 : rejouée depuis 0, lue EN CONTINU par une tâche à part ──
    let mut deuxieme = depuis_zero(&url, "connexion 2").await;
    let mut lu2 = lire_au_moins(&mut deuxieme, 4_000_000, "connexion 2").await;
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let lecteur2 = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stop_rx => return lu2,
                b = deuxieme.chunk() => match b {
                    Ok(Some(b)) => lu2.extend_from_slice(&b),
                    _ => return lu2,
                },
            }
        }
    });

    // ── 3 : supersède la 2e, qui lit encore ──
    let mut troisieme = depuis_zero(&url, "connexion 3").await;
    let lu3 = lire_au_moins(&mut troisieme, 44 + TRAME, "connexion 3").await;
    tokio::time::sleep(Duration::from_millis(450)).await;
    let _ = stop_tx.send(());
    let lu2 = lecteur2.await.expect("lecteur 2");
    let lu3 = lire_jusqu_au_bout(troisieme, lu3, "connexion 3").await;
    let tire = session.octets_du_canal.load(SeqCst);

    verifier("connexion 1", &lu1, &source, false);
    verifier("connexion 2", &lu2, &source, false);
    verifier("connexion 3", &lu3, &source, true);
    assert_eq!(tire, source.len() as u64, "tuyau tiré en entier");
}

/// Même scène, mais la 3e arrive pendant que la 2e est ENCORE dans sa
/// relecture de la retenue (pas encore au direct).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn la_troisieme_pendant_la_relecture_de_la_deuxieme_rend_le_wav_attendu() {
    let (source, blocs) = flux_source(3_000_000);
    let (url, _session) = servir("bruit-rocket-relecture", blocs).await;

    let mut sonde = depuis_zero(&url, "connexion 1").await;
    let lu1 = lire_au_moins(&mut sonde, 6_000_000, "connexion 1").await;
    drop(sonde);

    let mut deuxieme = depuis_zero(&url, "connexion 2").await;
    let lu2 = lire_au_moins(&mut deuxieme, 500_000, "connexion 2").await;
    let mut troisieme = depuis_zero(&url, "connexion 3").await;
    let lu3 = lire_au_moins(&mut troisieme, 44 + TRAME, "connexion 3").await;
    let lu2 = lire_jusqu_au_bout(deuxieme, lu2, "connexion 2").await;
    let lu3 = lire_jusqu_au_bout(troisieme, lu3, "connexion 3").await;

    verifier("connexion 1", &lu1, &source, false);
    verifier("connexion 2", &lu2, &source, false);
    verifier("connexion 3", &lu3, &source, true);
}
