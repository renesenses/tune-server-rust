//! #6255 — `bruit_suspect_reseau` de bout en bout : le VRAI routeur, servi
//! sur une socket, un client `Lavf/58.45.100`, et ce que le serveur en dit
//! (journal + registre du rapport de bug).
//!
//! WAV 44,1 kHz / 24 bits synthétique (sinus à −6 dBFS + bruit à −60 dBFS) :
//! - `bytes=44-` : aucune alerte ;
//! - reprise de phase 4 (le cas du 13/10) : `reprise_hors_trame`, et
//!   `decodage_decale_probable` tant que le serveur sert l'octet demandé ;
//! - un en-tête RIFF au milieu du flux : `entete_au_milieu` ;
//! - et le client reçoit toujours exactement les octets de la source.
use super::*;
use std::sync::{Arc, Mutex, atomic::Ordering::SeqCst};
use std::time::Duration;

const LAVF: &str = "Lavf/58.45.100";
const SR: u32 = 44_100;
const TRAME: u64 = 6;

#[derive(Clone, Default)]
struct Journal(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Journal {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Journal {
    type Writer = Journal;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Journal {
    fn warns(&self, stream_id: &str) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .filter(|l| l.contains(" WARN ") && l.contains("bruit_suspect_reseau"))
            .filter(|l| l.contains(&format!("stream_id={stream_id} ")))
            .map(str::to_string)
            .collect()
    }
}

fn abonne(journal: &Journal) -> tracing::subscriber::DefaultGuard {
    let abonne = tracing_subscriber::fmt()
        .with_writer(journal.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_default(abonne)
}

/// Sinus 440 Hz à −6 dBFS + bruit faible, 24 bits stéréo, en-tête compris.
fn musique24(secondes: f64) -> Vec<u8> {
    let mut etat: u64 = 0x6255_6255;
    let mut v = tune_core::audio::wav::build_wav_header(2, SR, 24).to_vec();
    let n = (secondes * f64::from(SR)) as usize;
    let pe = f64::from((1u32 << 23) - 1);
    for i in 0..n {
        etat ^= etat << 13;
        etat ^= etat >> 7;
        etat ^= etat << 17;
        let bruit = (etat >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0;
        let t = i as f64 / f64::from(SR);
        let x = 0.5 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() + 0.001 * bruit;
        let x = (x * pe) as i32;
        v.extend_from_slice(&x.to_le_bytes()[..3]);
        v.extend_from_slice(&x.to_le_bytes()[..3]);
    }
    v
}

fn info24(duree_ms: Option<u64>) -> StreamInfo {
    StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: SR,
        channels: 2,
        bit_depth: 24,
        duration_ms: duree_ms,
        ..StreamInfo::default()
    }
}

async fn routeur(id: &str, session: Arc<StreamSession>) -> String {
    let sessions: tune_core::http::streamer::SharedSessions = Arc::new(tokio::sync::Mutex::new(
        [(id.to_string(), session)].into_iter().collect(),
    ));
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(ecoute, router(sessions)).await.ok();
    });
    format!("http://127.0.0.1:{port}/stream/{id}.wav")
}

/// Une session FICHIER : le WAV est écrit sur disque et servi par `serve_file`.
/// `scratch_file` et non un chemin composé à la main : le garde
/// `aucune_fuite_de_temporaires` (#3030) refuse le second. Le fichier part
/// quand le test lâche le `ScratchFile` rendu, panique comprise.
async fn servir_un_fichier(id: &str, wav: &[u8]) -> (String, tune_core::test_scratch::ScratchFile) {
    let fichier = tune_core::test_scratch::scratch_file(&format!("tune-6255-{id}"), ".wav");
    std::fs::write(fichier.path(), wav).unwrap();
    let session = Arc::new(StreamSession::new(id.into(), info24(None), false, 4));
    *session.file_path.lock().await = Some(fichier.path().to_string_lossy().into_owned());
    (routeur(id, session).await, fichier)
}

/// Une session de CONVERSION (le producteur émet l'en-tête dans le canal),
/// avec sa durée : la retenue sert les reprises à l'octet (#5426).
/// `avec_duree = false` : flux chunké, sans longueur annoncée.
async fn servir_une_conversion(id: &str, source: Vec<u8>, avec_duree: bool) -> String {
    let pcm = (source.len() - 44) as u64;
    let duree_ms = avec_duree.then(|| pcm * 1000 / (u64::from(SR) * TRAME));
    let session = Arc::new(StreamSession::new(id.into(), info24(duree_ms), false, 16));
    session.wav_header_included.store(true, SeqCst);
    let _ = session
        .contexte_de_conversion
        .set(tune_core::http::streamer::ContexteDeConversion {
            zone_id: 10,
            ..Default::default()
        });
    let tx = session.tx.lock().await.clone().expect("tx");
    session.close_sender().await;
    tokio::spawn(async move {
        for bloc in source.chunks(32_768) {
            if tx.send(bloc.to_vec()).await.is_err() {
                return;
            }
        }
    });
    routeur(id, session).await
}

/// Une connexion `bytes=N-` façon Lavf, lue jusqu'à `au_plus` octets (ou au
/// bout). Rend le début annoncé par `Content-Range` et les octets reçus.
async fn lire(url: &str, debut: u64, au_plus: Option<usize>) -> (u64, Vec<u8>) {
    let mut r = tune_core::http::client::builder()
        .build()
        .expect("client HTTP")
        .get(url)
        .header("User-Agent", LAVF)
        .header("Range", format!("bytes={debut}-"))
        .send()
        .await
        .expect("réponse");
    let servi_depuis = r
        .headers()
        .get("Content-Range")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes "))
        .and_then(|v| v.split('-').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut recu = Vec::new();
    loop {
        if au_plus.is_some_and(|n| recu.len() >= n) {
            return (servi_depuis, recu);
        }
        match tokio::time::timeout(Duration::from_secs(10), r.chunk()).await {
            Err(_) => panic!("corps muet après {} octets", recu.len()),
            Ok(Err(e)) => panic!("corps interrompu après {} octets : {e}", recu.len()),
            Ok(Ok(None)) => return (servi_depuis, recu),
            Ok(Ok(Some(b))) => recu.extend_from_slice(&b),
        }
    }
}

/// Les causes notées au registre pour ce flux, après que les corps lâchés
/// ont eu le temps de juger leur dernière fenêtre.
async fn causes(stream_id: &str, au_moins: usize) -> Vec<&'static str> {
    let lire = || -> Vec<&'static str> {
        tune_core::http::bruit_reseau::derniers()
            .into_iter()
            .filter(|e| e.stream_id == stream_id)
            .map(|e| e.cause)
            .collect()
    };
    for _ in 0..50 {
        if lire().len() >= au_moins {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    lire()
}

/// L'offset de phase 4 du 13/10, transposé au flux de test (~0,5 s).
fn reprise_de_phase_4() -> u64 {
    let n = 44 + TRAME * 22_050 + 4;
    assert_eq!((n - 44) % TRAME, 4);
    n
}

#[tokio::test]
async fn fichier_servi_depuis_44_aucune_alerte() {
    let journal = Journal::default();
    let _garde = abonne(&journal);
    let wav = musique24(2.0);
    let (url, _fichier) = servir_un_fichier("f-44", &wav).await;
    let (_, recu) = lire(&url, 44, None).await;
    assert_eq!(recu, wav[44..], "flux intact");
    let (_, recu) = lire(&url, 0, None).await;
    assert_eq!(recu, wav, "flux intact");
    assert!(causes("f-44", 1).await.is_empty());
    assert!(journal.warns("f-44").is_empty());
}

#[tokio::test]
async fn fichier_repris_en_phase_4_leve_reprise_hors_trame_et_decodage_decale() {
    let journal = Journal::default();
    let _garde = abonne(&journal);
    let wav = musique24(2.0);
    let (url, _fichier) = servir_un_fichier("f-phase4", &wav).await;
    let n = reprise_de_phase_4();
    let (servi, recu) = lire(&url, n, None).await;
    assert_eq!(
        recu,
        wav[servi as usize..],
        "flux intact depuis le Content-Range"
    );
    let c = causes("f-phase4", 2).await;
    assert!(c.contains(&"reprise_hors_trame"), "{c:?}");
    // Le décodage décalé ne se dit que si le serveur a servi l'octet hors
    // trame ; une reprise réalignée sur la trame ne le produit plus.
    assert_eq!(
        c.contains(&"decodage_decale_probable"),
        (servi - 44) % TRAME != 0,
        "servi depuis {servi} : {c:?}"
    );
    let warns = journal.warns("f-phase4");
    eprintln!("{}", warns.join("\n"));
    let w = warns
        .iter()
        .find(|l| l.contains("cause=\"reprise_hors_trame\""))
        .expect("WARN reprise_hors_trame");
    for attendu in [
        "voie=\"fichier\"",
        "agent=Lavf/58.45.100",
        &format!("offset_demande=Some({n})"),
        "phase=4",
        "trame=6",
        "position_s=0.5",
        "format=44100/24/2",
    ] {
        assert!(w.contains(attendu), "{attendu} absent de : {w}");
    }
    assert!(!c.contains(&"signal_bruite"), "{c:?}");
}

#[tokio::test]
async fn conversion_reprise_en_phase_4_depuis_la_retenue() {
    let journal = Journal::default();
    let _garde = abonne(&journal);
    let source = musique24(3.0);
    let url = servir_une_conversion("c-phase4", source.clone(), true).await;
    // Le lecteur lit le début, coupe, puis reprend hors trame EN DEÇÀ de ce
    // que le tuyau a rendu : la retenue sert la reprise (#5426).
    let (_, debut) = lire(&url, 0, Some(400_000)).await;
    assert_eq!(&debut[..], &source[..debut.len()]);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let n = reprise_de_phase_4();
    let (servi, recu) = lire(&url, n, Some(300_000)).await;
    assert_eq!(
        &recu[..],
        &source[servi as usize..servi as usize + recu.len()],
        "reprise intacte depuis le Content-Range"
    );
    let c = causes("c-phase4", 2).await;
    assert!(c.contains(&"reprise_hors_trame"), "{c:?}");
    assert_eq!(
        c.contains(&"decodage_decale_probable"),
        (servi - 44) % TRAME != 0,
        "servi depuis {servi} : {c:?}"
    );
    let warns = journal.warns("c-phase4");
    eprintln!("{}", warns.join("\n"));
    assert!(
        warns
            .iter()
            .any(|l| l.contains("cause=\"reprise_hors_trame\"") && l.contains("zone_id=10")),
        "{warns:#?}"
    );
    let rapport = tune_core::http::bruit_reseau::section_du_rapport(
        &tune_core::http::bruit_reseau::derniers(),
        |_| None,
    );
    assert!(
        rapport.contains("`reprise_hors_trame` conversion — zone 10"),
        "{rapport}"
    );
}

#[tokio::test]
async fn conversion_depuis_0_aucune_alerte() {
    let source = musique24(2.0);
    let url = servir_une_conversion("c-0", source.clone(), true).await;
    let (_, recu) = lire(&url, 0, None).await;
    assert_eq!(recu, source, "flux intact");
    assert!(causes("c-0", 1).await.is_empty());
}

#[tokio::test]
async fn conversion_avec_un_entete_au_milieu() {
    let journal = Journal::default();
    let _garde = abonne(&journal);
    // Deux WAV collés : le second en-tête tombe au milieu du flux.
    let un = musique24(1.0);
    let mut source = un.clone();
    source.extend_from_slice(&un);
    let url = servir_une_conversion("c-entete", source.clone(), false).await;
    let (_, recu) = lire(&url, 0, None).await;
    assert_eq!(recu, source, "flux intact : le détecteur ne retire rien");
    let c = causes("c-entete", 1).await;
    assert_eq!(c, vec!["entete_au_milieu"], "{c:?}");
    let warns = journal.warns("c-entete");
    assert!(
        warns
            .iter()
            .any(|l| l.contains(&format!("a l'octet {} du flux", un.len()))),
        "{warns:#?}"
    );
}
