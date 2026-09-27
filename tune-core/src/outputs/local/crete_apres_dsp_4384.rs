//! #4384 (GgB, fil 1797) — la crête d'une sortie locale est relevée APRÈS le
//! DSP, dans le référentiel de la piste.
//!
//! Le cas qui départage : une bande d'égaliseur à −12 dB posée sur la
//! fréquence qui porte la crête. Le signal qui part vers le DAC perd 12 dB ;
//! le gain MOYEN de la courbe — la seule chose que le crête-mètre savait
//! reporter depuis #4685 — bouge à peine. Ces témoins font tourner la VRAIE
//! boucle producteur (décodage → `apply_local_dsp` → puits) et lisent ce
//! qu'elle a relevé.

use std::io::Cursor;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::mpsc;

use super::empreinte_du_puits_r1::{DspAuRepos, etage};
use super::{BoucleProducteur, CompteursDePiste, RoleDeLaBoucle};
use crate::audio::crete_de_sortie::CretesDeSortie;
use crate::audio::eq::{EqBandSpec, EqProcessor, EqProfile};
use crate::outputs::traits::{CaptureOutput, FormatOuvert};

const CADENCE: u32 = 44_100;
const AMPLITUDE: f64 = 0.9;

/// Une seconde de sinus à 1 kHz, 16 bits stéréo, crête `AMPLITUDE`.
fn sinus_1khz() -> Vec<u8> {
    let mut octets = Vec::with_capacity(CADENCE as usize * 4);
    for i in 0..CADENCE {
        let t = f64::from(i) / f64::from(CADENCE);
        let s = (AMPLITUDE * (2.0 * std::f64::consts::PI * 1000.0 * t).sin() * 32767.0) as i16;
        for _ in 0..2 {
            octets.extend_from_slice(&s.to_le_bytes());
        }
    }
    octets
}

fn egaliseur_coupe_1khz_de_12_db() -> EqProcessor {
    let profil = EqProfile {
        enabled: true,
        bands: vec![EqBandSpec {
            freq: 1000.0,
            gain: -12.0,
            q: 1.0,
            band_type: "peak".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    EqProcessor::new(&profil, CADENCE, 2)
}

fn db(lineaire: f64) -> f64 {
    20.0 * lineaire.log10()
}

/// Fait tourner la boucle producteur sur une seconde de sinus, à partir de
/// `depart_ms` dans la piste, et rend ce qu'elle a relevé.
fn jouer(dsp: &DspAuRepos, depart_ms: u64) -> CretesDeSortie {
    let cretes = CretesDeSortie::new();
    let arret = AtomicBool::new(false);
    let disparu = AtomicBool::new(false);
    let position = AtomicU64::new(0);
    let constat = std::sync::Mutex::new(None);
    let (_tx, rx) = mpsc::channel();
    let producteur = BoucleProducteur {
        role: RoleDeLaBoucle::PisteInitiale,
        backend: "capture",
        device_name: "X230",
        cle_de_flux: None,
        stop_rx: &rx,
        force_silent: &arret,
        device_gone: &disparu,
        position_ms: &position,
        open_failure: &constat,
        debut_du_flux: std::time::Instant::now(),
        duree_de_la_piste_ms: &super::DUREE_DE_PISTE_INCONNUE,
        cretes_de_sortie: Some(&cretes),
    };
    let mut conversion = etage(dsp, Vec::new(), CADENCE, 2, 16, CADENCE, 2);
    let mut puits = CaptureOutput::ouvert(FormatOuvert::new(CADENCE, 2));
    let mut compteurs = CompteursDePiste {
        total_bytes_read: 0,
        total_frames_fed: 0,
        seek_offset: depart_ms,
        skip_bytes: 0,
        skipped_bytes: 0,
        premiere_donnee_journalisee: true,
    };
    producteur.tourner(
        &mut Cursor::new(sinus_1khz()),
        &mut [0; 4096],
        &mut conversion,
        &mut puits,
        &mut |_, _, _| false,
        &mut compteurs,
        &mut |_| true,
    );
    cretes
}

/// Sans DSP, la crête relevée est celle du fichier : rien n'est inventé.
#[test]
fn i4384_sans_dsp_la_crete_relevee_est_celle_du_fichier() {
    let dsp = DspAuRepos::neuf();
    let cretes = jouer(&dsp, 0);
    let (g, d) = cretes
        .crete_entre(500.0, 540.0)
        .expect("la boucle producteur doit relever la fenêtre 500-540 ms");
    assert!(
        (db(g) - db(AMPLITUDE)).abs() < 0.2 && (db(d) - db(AMPLITUDE)).abs() < 0.2,
        "sans DSP : {:.2} / {:.2} dBFS attendus {:.2}",
        db(g),
        db(d),
        db(AMPLITUDE)
    );
}

/// Le cas de l'issue : −12 dB d'égaliseur sur la crête ⇒ la crête qui part
/// vers le DAC baisse de 12 dB. Et ce que le crête-mètre reportait jusqu'ici
/// (gain MOYEN de la courbe) en est loin : c'est l'écart que l'instrument
/// montrait.
#[test]
fn i4384_une_bande_a_moins_12_db_sur_la_crete_baisse_la_crete_relevee_de_12_db() {
    let dsp = DspAuRepos::neuf();
    let eq = egaliseur_coupe_1khz_de_12_db();
    let gain_moyen_db = eq.gain_moyen_db();
    *dsp.eq.lock().unwrap() = Some(eq);
    let cretes = jouer(&dsp, 0);
    // Au-delà du transitoire d'attaque du biquad.
    let (g, d) = cretes
        .crete_entre(500.0, 540.0)
        .expect("la boucle producteur doit relever la fenêtre 500-540 ms");
    let baisse_g = db(g) - db(AMPLITUDE);
    let baisse_d = db(d) - db(AMPLITUDE);
    assert!(
        (baisse_g + 12.0).abs() < 0.5 && (baisse_d + 12.0).abs() < 0.5,
        "−12 dB d'égaliseur sur la crête ⇒ −12 dB relevés, lu {baisse_g:.2} / {baisse_d:.2}"
    );
    assert!(
        gain_moyen_db > -6.0,
        "le gain MOYEN de la courbe ({gain_moyen_db:.2} dB) n'est pas la crête : \
         c'est pourquoi il fallait mesurer après le DSP"
    );
}

/// Les tranches sont datées dans la PISTE : un départ à 60 s (saut, reprise)
/// se retrouve à 60 s, pas à 0.
#[test]
fn i4384_les_tranches_sont_datees_dans_la_piste() {
    let dsp = DspAuRepos::neuf();
    let cretes = jouer(&dsp, 60_000);
    assert!(cretes.crete_entre(500.0, 540.0).is_none());
    assert!(cretes.crete_entre(60_500.0, 60_540.0).is_some());
}
