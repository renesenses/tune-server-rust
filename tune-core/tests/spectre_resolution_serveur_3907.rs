//! Ce que l'analyse spectrale du serveur RÉSOUT vraiment, mesuré (#3907).
//!
//! ## Le signalement
//!
//! Pascal (bluevelvet), Tune 0.9.144 Windows/WASAPI, fil 1765 : « l'échelle
//! des fréquences affichée sous les barres commence seulement à 250 Hz […]
//! Pourtant, les barres représentent visiblement aussi les fréquences situées
//! en dessous. » Il écoutait en 96 kHz.
//!
//! ## Ce que ce fichier établit, et pourquoi il existe
//!
//! Le premier diagnostic accusait le serveur : « la FFT de 2 048 points ne
//! distingue pas les bandes basses, allongez-la ». C'est FAUX depuis #2866
//! (v0.9.129, `SPECTRUM_FFT_MAX = 8192`) : la fenêtre de 40 ms garde toutes
//! ses trames, et la résolution VRAIE est constante à 25 Hz de 44,1 à
//! 192 kHz. Le repère 125 Hz est donc légitime à 96 kHz, et c'est le CLIENT
//! qui le perd — `tune-web-client`, `src/lib/spectrumScale.ts:49`, recopie en
//! dur `SERVER_FFT_SIZE = 2048` et rejoue la troncature d'avant #2866 pour
//! décider où poser un repère, au lieu de lire les trois champs que le
//! serveur publie EXPRÈS pour ça : `spectrum_frames`,
//! `spectrum_resolution_hz` et `spectrum_resolved` (un booléen par bande).
//!
//! Ce fichier ne corrige rien : il **cloue le contrat serveur** sur lequel ce
//! correctif client va s'appuyer. Sans lui, un retour en arrière sur la taille
//! de FFT ou sur la fenêtre d'analyse ferait mentir le client sans qu'aucune
//! porte ne rougisse — le client aurait troqué une constante périmée contre un
//! champ non gardé.
//!
//! La charge utile vient d'un SIGNAL, pas du témoin : une sinusoïde est
//! synthétisée puis passée au vrai `compute_levels`, et c'est la bande que
//! l'analyse ALLUME qu'on interroge. Un test qui se contenterait de relire
//! `band_resolved` recopierait l'arithmétique qu'il prétend garder.
//!
//! ⚠️ `tune-core` porte `autotests = false` — ce fichier n'est compilé que
//! parce qu'il est déclaré `[[test]]` dans `tune-core/Cargo.toml`.

use tune_core::audio::levels::{SPECTRUM_BANDS, compute_levels};

/// Une fenêtre d'analyse du forwarder : 40 ms, comme celles que
/// `crate::audio::tap::send_windowed_pcm` découpe.
const FENETRE_MS: u32 = 40;

/// PCM stéréo 16 bits little-endian portant une sinusoïde à `hz`, pleine
/// échelle à −6 dBFS, sur `FENETRE_MS` millisecondes.
fn sinus(hz: f64, sample_rate: u32) -> Vec<u8> {
    let frames = (sample_rate as u64 * FENETRE_MS as u64 / 1000) as usize;
    let mut pcm = Vec::with_capacity(frames * 4);
    for n in 0..frames {
        let t = n as f64 / sample_rate as f64;
        let v = (0.5 * (2.0 * std::f64::consts::PI * hz * t).sin() * i16::MAX as f64) as i16;
        pcm.extend_from_slice(&v.to_le_bytes());
        pcm.extend_from_slice(&v.to_le_bytes());
    }
    pcm
}

/// Index de la bande la plus forte, lue sur le niveau ABSOLU (`spectrum_db`).
fn bande_allumee(db: &[f32]) -> usize {
    db.iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
        .expect("un spectre non vide")
}

/// ⭐ #3907 — à 96 kHz, l'analyse RÉSOUT la bande où tombe 125 Hz.
///
/// C'est le fait qui autorise le client à poser de nouveau le repère 125 Hz à
/// cette fréquence d'échantillonnage, et qui fait tomber la thèse « allongez
/// la FFT » : elle est déjà longue.
#[test]
fn a_96_khz_la_bande_de_125_hz_est_resolue_et_c_est_elle_qui_s_allume() {
    let pcm = sinus(125.0, 96_000);
    let lvl = compute_levels(&pcm, 16, 2, 96_000);

    println!(
        "96 kHz — frames={} fft={} resolution={:.2} Hz",
        lvl.spectrum_frames, lvl.spectrum_fft_size, lvl.spectrum_resolution_hz
    );

    assert_eq!(
        lvl.spectrum_frames, 3_840,
        "la fenêtre de 40 ms porte 3 840 trames à 96 kHz, et #2866 les garde \
         TOUTES : c'est ce qui donne les 25 Hz. Si ce nombre retombe à 2 048, \
         la moitié du signal repart à la poubelle et le client a raison de \
         masquer ses repères graves"
    );
    assert!(
        (lvl.spectrum_resolution_hz - 25.0).abs() < 0.01,
        "la résolution VRAIE doit valoir 25 Hz à 96 kHz (96000/3840), pas les \
         46,9 Hz de `sample_rate / 2048` que le client recopie en dur \
         (`spectrumScale.ts:49`). Mesuré : {:.2} Hz",
        lvl.spectrum_resolution_hz
    );

    let b = bande_allumee(&lvl.spectrum_db);
    println!(
        "96 kHz — 125 Hz allume la bande {b} (centre annoncé {:.1} Hz, résolue : {})",
        lvl.spectrum_hz[b], lvl.spectrum_resolved[b]
    );
    assert!(
        lvl.spectrum_resolved[b],
        "la bande qu'un 125 Hz ALLUME à 96 kHz doit être annoncée résolue — \
         c'est ce qui autorise un repère à cette fréquence. Bande {b}, centre \
         annoncé {:.1} Hz",
        lvl.spectrum_hz[b]
    );
    assert!(
        (lvl.spectrum_hz[b] as f64 - 125.0).abs() <= lvl.spectrum_resolution_hz as f64,
        "le centre ANNONCÉ de la bande allumée doit décrire le son qui \
         l'allume, à une résolution près : bande {b}, annoncée {:.1} Hz pour \
         un 125 Hz",
        lvl.spectrum_hz[b]
    );
}

/// ⭐ LE TÉMOIN — `spectrum_resolved` n'est pas un tableau de `true`.
///
/// Sans lui, le test ci-dessus passerait contre une implémentation qui
/// déclarerait TOUTES les bandes résolues, c'est-à-dire contre l'invention que
/// #2866 et #2081 ont justement chassée. Les bandes du grave sont plus
/// ÉTROITES que la résolution — la première fait 4,8 Hz pour 25 Hz d'analyse —
/// et elles doivent le dire.
#[test]
fn temoin_les_bandes_les_plus_graves_ne_sont_pas_resolues() {
    let pcm = sinus(125.0, 96_000);
    let lvl = compute_levels(&pcm, 16, 2, 96_000);
    assert_eq!(
        lvl.spectrum_resolved.len(),
        SPECTRUM_BANDS,
        "un booléen par bande"
    );
    assert!(
        !lvl.spectrum_resolved[0],
        "la bande 0 court de 20 à 24,8 Hz, soit 4,8 Hz de large contre 25 Hz \
         de résolution : l'analyse ne la sépare pas de sa voisine et le champ \
         doit le dire. S'il annonçait `true`, le client reposerait des repères \
         inventés — exactement le défaut de #2081"
    );
    let resolues = lvl.spectrum_resolved.iter().filter(|b| **b).count();
    assert!(
        resolues > 0 && resolues < SPECTRUM_BANDS,
        "le drapeau doit PARTAGER les bandes, pas les déclarer toutes pareilles : \
         {resolues} résolues sur {SPECTRUM_BANDS}"
    );
}

/// La TABLE que le client doit reproduire, mesurée sur les quatre cadences
/// courantes : première bande résolue, son centre annoncé, et la résolution
/// vraie. C'est elle qui montre que le bas de l'échelle ne dépend plus de la
/// fréquence d'échantillonnage depuis #2866 — la thèse « 250 Hz à 96 kHz,
/// 500 Hz à 192 kHz » décrit le monde d'AVANT.
#[test]
fn table_des_premieres_bandes_resolues_par_cadence() {
    for sr in [44_100u32, 48_000, 96_000, 192_000] {
        let lvl = compute_levels(&sinus(1_000.0, sr), 16, 2, sr);
        let premiere = lvl.spectrum_resolved.iter().position(|b| *b);
        println!(
            "{sr:>6} Hz — frames={:>5} fft={:>5} resolution={:>6.2} Hz — \
             première bande résolue : {:?} (centre {:.1} Hz)",
            lvl.spectrum_frames,
            lvl.spectrum_fft_size,
            lvl.spectrum_resolution_hz,
            premiere,
            premiere.map(|i| lvl.spectrum_hz[i]).unwrap_or(0.0),
        );
        assert!(
            (lvl.spectrum_resolution_hz - 25.0).abs() < 0.01,
            "#2866 rend la résolution CONSTANTE à 25 Hz sur toute la gamme : \
             {sr} Hz donne {:.2} Hz",
            lvl.spectrum_resolution_hz
        );
        let i = premiere.expect("au moins une bande résolue");
        assert!(
            lvl.spectrum_hz[i] <= 130.0,
            "à {sr} Hz la première bande résolue est annoncée à {:.1} Hz : le \
             repère ISO 125 Hz doit rester posable à toutes les cadences \
             (c'est ce que le client perd en croyant la FFT à 2048)",
            lvl.spectrum_hz[i]
        );
    }
}
