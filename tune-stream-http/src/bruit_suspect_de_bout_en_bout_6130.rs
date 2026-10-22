//! #6130 — l'instrumentation de la voie conversion, de bout en bout : un vrai
//! corps HTTP servi à un client `Lavf/58.45.100`, et le JOURNAL relu.
//!
//! - la ligne `conversion_connexion_contexte` porte, à l'octet, les 44
//!   premiers octets que le client a reçus, et l'en-tête qu'ils décodent ;
//! - un bruit blanc servi lève UN `bruit_suspect`, même sur deux connexions ;
//! - un sinus servi n'en lève aucun ;
//! - et le client reçoit, dans tous les cas, exactement le flux source.
use super::*;
use std::sync::{Arc, Mutex, atomic::Ordering::SeqCst};
use std::time::Duration;

const LAVF: &str = "Lavf/58.45.100";
const SR: u32 = 44_100;
const BLOC: usize = 32_768;

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
    fn lignes(&self, evenement: &str) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .filter(|l| l.contains(evenement))
            .map(str::to_string)
            .collect()
    }
}

fn flux_source(pcm: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
    let entete = tune_core::audio::wav::build_wav_header(2, SR, 16).to_vec();
    let mut blocs = vec![entete.clone()];
    blocs.extend(pcm.chunks(BLOC).map(<[u8]>::to_vec));
    let mut tout = entete;
    tout.extend_from_slice(pcm);
    (tout, blocs)
}

fn pcm16(secondes: f64, mut f: impl FnMut(f64) -> f64) -> Vec<u8> {
    let n = (secondes * f64::from(SR)) as usize;
    let mut v = Vec::with_capacity(n * 4);
    for i in 0..n {
        let x = (f(i as f64 / f64::from(SR)).clamp(-1.0, 1.0) * 32_767.0) as i16;
        v.extend_from_slice(&x.to_le_bytes());
        v.extend_from_slice(&x.to_le_bytes());
    }
    v
}

fn bruit_blanc(secondes: f64) -> Vec<u8> {
    let mut etat: u64 = 0x6130_0000_cafe_f00d;
    pcm16(secondes, |_| {
        etat ^= etat << 13;
        etat ^= etat >> 7;
        etat ^= etat << 17;
        (etat >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    })
}

fn sinus(secondes: f64) -> Vec<u8> {
    pcm16(secondes, |t| {
        0.9 * (2.0 * std::f64::consts::PI * 1_000.0 * t).sin()
    })
}

/// La session de `servir_le_service_en_wav_progressif` : WAV 16 bits chunké,
/// producteur qui émet l'en-tête, contexte posé par l'orchestrateur.
async fn servir(id: &str, blocs: Vec<Vec<u8>>) -> (String, Arc<StreamSession>) {
    let info = StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: SR,
        channels: 2,
        bit_depth: 16,
        duration_ms: None,
        ..StreamInfo::default()
    };
    let session = Arc::new(StreamSession::new(id.into(), info, false, 256));
    session.wav_header_included.store(true, SeqCst);
    let _ = session
        .contexte_de_conversion
        .set(tune_core::http::streamer::ContexteDeConversion {
            zone_id: 10,
            source: "service=qobuz codec=flac annonce=44100/16 decodeur_sortie=44100/16 canaux=2"
                .into(),
            chaine: "tiers:greffon@44100[44100->44100] canaux=2".into(),
            precedente: "radio source=radio format=- frequence=48000 profondeur=- stream_id=fip session_ouverte=true".into(),
        });
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

/// Une connexion `bytes=0-` façon Lavf, lue jusqu'à `au_plus` octets (ou au
/// bout), puis fermée.
async fn lire(url: &str, au_plus: Option<usize>) -> Vec<u8> {
    let mut r = tune_core::http::client::builder()
        .build()
        .expect("client HTTP")
        .get(url)
        .header("User-Agent", LAVF)
        .header("Range", "bytes=0-")
        .send()
        .await
        .expect("réponse");
    let mut recu = Vec::new();
    loop {
        if au_plus.is_some_and(|n| recu.len() >= n) {
            return recu;
        }
        match tokio::time::timeout(Duration::from_secs(10), r.chunk()).await {
            Err(_) => panic!("corps muet après {} octets", recu.len()),
            Ok(Err(e)) => panic!("corps interrompu après {} octets : {e}", recu.len()),
            Ok(Ok(None)) => return recu,
            Ok(Ok(Some(b))) => recu.extend_from_slice(&b),
        }
    }
}

/// Le bilan d'une connexion part quand son corps est lâché, côté serveur :
/// lui laisser le temps d'arriver.
async fn attendre_les_lignes(journal: &Journal, evenement: &str, n: usize) -> Vec<String> {
    for _ in 0..100 {
        if journal.lignes(evenement).len() >= n {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    journal.lignes(evenement)
}

fn abonne(journal: &Journal) -> tracing::subscriber::DefaultGuard {
    let abonne = tracing_subscriber::fmt()
        .with_writer(journal.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_default(abonne)
}

#[tokio::test]
async fn la_ligne_de_connexion_porte_les_44_octets_reellement_envoyes() {
    let journal = Journal::default();
    let _garde = abonne(&journal);
    let (source, blocs) = flux_source(&sinus(2.0));
    let (url, _session) = servir("contexte-6130", blocs).await;

    let sonde = lire(&url, Some(100_000)).await;
    let lecture = lire(&url, None).await;
    assert_eq!(&sonde[..], &source[..sonde.len()], "sonde : flux intact");
    assert_eq!(lecture, source, "lecture : flux intact, à l'octet");

    let lignes = attendre_les_lignes(&journal, "conversion_connexion_contexte", 2).await;
    assert_eq!(lignes.len(), 2, "UNE ligne par connexion :\n{lignes:#?}");
    // Visible avec `--nocapture` : l'exemple de journal de la PR.
    eprintln!("{}", lignes[1]);
    let hex_attendu = bruit_suspect_6130::hex(&lecture[..44]);
    for l in &lignes {
        assert!(l.contains(" INFO "), "{l}");
        assert!(l.contains(&format!("entete_hex={hex_attendu}")), "{l}");
        assert!(
            l.contains("entete_envoye=frequence=44100 profondeur=16 canaux=2 format=1"),
            "{l}"
        );
        assert!(l.contains("entete_conforme_a_la_session=Some(true)"), "{l}");
        assert!(l.contains("session_format=wav 44100/16/2"), "{l}");
        assert!(l.contains("agent=Lavf/58.45.100"), "{l}");
        assert!(l.contains("stream_id=contexte-6130"), "{l}");
        assert!(l.contains("zone_id=10"), "{l}");
        assert!(l.contains("source=service=qobuz codec=flac"), "{l}");
        assert!(
            l.contains("chaine=tiers:greffon@44100[44100->44100]"),
            "{l}"
        );
        assert!(l.contains("precedente=radio source=radio"), "{l}");
        assert!(l.contains("frequence=48000"), "{l}");
    }
    assert!(
        journal.lignes("bruit_suspect —").is_empty(),
        "un sinus n'est pas un bruit"
    );
}

#[tokio::test]
async fn un_bruit_blanc_servi_leve_un_seul_warn_par_flux_et_rien_n_est_modifie() {
    let journal = Journal::default();
    let _garde = abonne(&journal);
    let (source, blocs) = flux_source(&bruit_blanc(6.0));
    let (url, session) = servir("bruit-6130", blocs).await;

    // Deux connexions qui lisent chacune bien plus de 2 s de bruit.
    let premiere = lire(&url, Some(44 + 4 * 44_100 * 3)).await;
    let seconde = lire(&url, None).await;
    assert_eq!(
        &premiere[..],
        &source[..premiere.len()],
        "1re : flux intact"
    );
    assert_eq!(seconde, source, "2e : flux intact, à l'octet");
    assert!(session.bruit_suspect_signale.load(SeqCst));

    attendre_les_lignes(&journal, "conversion_connexion_contexte", 2).await;
    let warns = journal.lignes("bruit_suspect —");
    assert_eq!(warns.len(), 1, "UN warn par flux :\n{warns:#?}");
    let w = &warns[0];
    eprintln!("{w}");
    assert!(w.contains(" WARN "), "{w}");
    assert!(w.contains("stream_id=bruit-6130"), "{w}");
    assert!(w.contains("agent=Lavf/58.45.100"), "{w}");
    assert!(w.contains("precedente=radio source=radio"), "{w}");
    assert!(w.contains("chaine=tiers:greffon"), "{w}");
    assert!(
        w.contains("entete_envoye=frequence=44100 profondeur=16 canaux=2"),
        "{w}"
    );
    assert!(w.contains("debut_du_bruit=44"), "{w}");
    let bloc_hex = w
        .split("bloc_hex=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .expect("bloc_hex");
    assert_eq!(bloc_hex.len(), 2 * 1024, "les 1024 premiers octets du bloc");
    // Le bloc recopié est bien un morceau du flux servi, à sa position.
    let position: usize = w
        .split("position_du_bloc=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .expect("position_du_bloc");
    assert_eq!(
        bloc_hex,
        bruit_suspect_6130::hex(&source[position..position + 1024])
    );
}
