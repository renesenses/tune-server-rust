//! La conversion WAV 24 bits progressive, éprouvée sur un VRAI FLAC 24 bits.
//!
//! Zone 10 du .18 (Eversolo DMP-A8, `dlna_wav24`, 08/10) : une piste Qobuz
//! FLAC 24/44,1 servie en WAV 24 bits converti se jouait « déformée, avec du
//! bruit, tout le morceau ». Deux causes possibles côté serveur : le relais
//! entre connexions (gardé dans `tune-stream-http`) ou la conversion elle-même
//! — ordre des octets, 24 bits empaquetés contre 32, droitisation.
//!
//! Ce témoin tranche la seconde. Il fait passer `ref_24_96000_stereo.flac`
//! par le décodeur PROGRESSIF — celui de la conversion DLNA, pas
//! `decode_to_pcm` que garde `flac_empreintes_reference.rs` — et exige :
//!
//! 1. un premier bloc qui est un en-tête WAV PCM 24 bits stéréo de 44 octets,
//!    seul dans son bloc ;
//! 2. des blocs suivants tous multiples de la trame de 6 octets ;
//! 3. un PCM, relu en 24 bits petit-boutiste signé, identique échantillon pour
//!    échantillon à celui de `flac -d` (libFLAC 1.5.0) : même empreinte que la
//!    table de `flac_empreintes_reference.rs`.
//!
//! Deux entrées sont éprouvées : le fichier (repli par téléchargement) et une
//! source passée en `MediaSource` (le chemin du décodage par `Range`, qui ne
//! diffère que par la source d'octets).

use md5::{Digest, Md5};

const FLAC_24: &str = "tests/fixtures/flac/ref_24_96000_stereo.flac";
/// Empreinte `flac -d` de la fixture : voir `flac_empreintes_reference.rs`.
const MD5_FLAC_D: &str = "5647a1733e4ec46e7a1dd00e10feaf3c";
const ECHANTILLONS: usize = 19_200;

fn chemin() -> String {
    format!("{}/{FLAC_24}", env!("CARGO_MANIFEST_DIR"))
}

/// Recueille les blocs que le décodeur pousse dans le canal de la session.
async fn recueillir(mut rx: tokio::sync::mpsc::Receiver<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut blocs = Vec::new();
    while let Some(b) = rx.recv().await {
        blocs.push(b);
    }
    blocs
}

fn verifier(blocs: &[Vec<u8>], cadence: u32) {
    assert!(!blocs.is_empty(), "aucun bloc produit");
    let entete = &blocs[0];
    assert_eq!(
        entete.len(),
        44,
        "le premier bloc doit être l'en-tête WAV seul (44 octets), pas {}",
        entete.len()
    );
    assert_eq!(&entete[0..4], b"RIFF");
    assert_eq!(&entete[8..16], b"WAVEfmt ");
    assert_eq!(u16::from_le_bytes([entete[20], entete[21]]), 1, "PCM");
    assert_eq!(u16::from_le_bytes([entete[22], entete[23]]), 2, "canaux");
    assert_eq!(
        u32::from_le_bytes([entete[24], entete[25], entete[26], entete[27]]),
        cadence,
        "cadence"
    );
    assert_eq!(
        u32::from_le_bytes([entete[28], entete[29], entete[30], entete[31]]),
        cadence * 6,
        "débit"
    );
    assert_eq!(u16::from_le_bytes([entete[32], entete[33]]), 6, "bloc");
    assert_eq!(u16::from_le_bytes([entete[34], entete[35]]), 24, "bits");
    assert_eq!(&entete[36..40], b"data");

    let mut pcm = Vec::new();
    for (i, b) in blocs[1..].iter().enumerate() {
        assert_eq!(
            b.len() % 6,
            0,
            "bloc {} de {} octets : pas un multiple de la trame 24 bits stéréo",
            i + 1,
            b.len()
        );
        pcm.extend_from_slice(b);
    }
    let echantillons: Vec<i32> = pcm
        .as_chunks::<3>()
        .0
        .iter()
        .map(|b| ((b[0] as i32) | ((b[1] as i32) << 8) | ((b[2] as i32) << 16)) << 8 >> 8)
        .collect();
    assert_eq!(echantillons.len(), ECHANTILLONS, "nombre d'échantillons");
    let mut h = Md5::new();
    for s in &echantillons {
        h.update(s.to_le_bytes());
    }
    let md5: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        md5, MD5_FLAC_D,
        "le WAV 24 bits progressif ne porte pas le PCM de `flac -d` : la conversion \
         24 bits altère le signal (ordre des octets, largeur ou droitisation)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn la_conversion_wav24_progressive_rend_le_pcm_de_flac_d() {
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4096);
    let (levels_tx, _levels_rx) = tokio::sync::mpsc::unbounded_channel();
    let ready = std::sync::Arc::new(tokio::sync::Notify::new());
    let collecte = tokio::spawn(recueillir(rx));
    let p = chemin();
    let r = tokio::task::spawn_blocking(move || {
        tune_core::audio::decode::decode_to_pcm_streaming_seeked(
            &p,
            Some(96_000),
            Some(2),
            Some(24),
            tx,
            32768,
            ready,
            levels_tx,
            0.0,
        )
    })
    .await
    .expect("join")
    .expect("décodage");
    assert_eq!(r, (24, 96_000));
    let blocs = collecte.await.expect("collecte");
    verifier(&blocs, 96_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn la_conversion_wav24_par_source_rend_le_pcm_de_flac_d() {
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4096);
    let collecte = tokio::spawn(recueillir(rx));
    let fichier = std::fs::File::open(chemin()).expect("fixture");
    tokio::task::spawn_blocking(move || {
        tune_core::audio::decode::decode_source_to_pcm_streaming(
            Box::new(fichier),
            "flac",
            Some(24),
            tx,
            32768,
        )
    })
    .await
    .expect("join")
    .expect("décodage");
    let blocs = collecte.await.expect("collecte");
    verifier(&blocs, 96_000);
}
