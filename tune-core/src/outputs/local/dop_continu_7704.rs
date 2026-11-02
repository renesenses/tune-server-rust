//! #7704 — le rappel partagé `f32` ne coupe plus jamais un porteur DoP.
//!
//! Puits factice : le tampon que le rappel réel remplit, période après
//! période, concaténé dans `au_dac`. On y vérifie le motif de repos (0x69,
//! jamais des zéros) et l'alternance des marqueurs, trame à trame, à travers
//! la pause, la famine, l'arrêt, la recherche et le changement de piste.

use super::*;
use crate::audio::dop_continu::{haut_f32, repos_f32};
use crate::audio::dsd_to_dop::DsdToDoP;

const TAUX: u32 = 176_400;
const CANAUX: u16 = 2;
const PERIODE: usize = 512; // échantillons (256 trames)

/// DoP 24 bits LE (`DsdToDoP`) → les `f32` que l'étage pousse dans l'anneau.
fn dop_f32(trames: usize, charge: u8) -> Vec<f32> {
    let mut e = DsdToDoP::new(CANAUX as usize, false);
    let mut octets = e.feed(&vec![charge; trames * 2 * CANAUX as usize]);
    octets.extend_from_slice(&e.terminer());
    octets
        .chunks_exact(3)
        .map(|c| {
            let w = ((c[0] as i32) | ((c[1] as i32) << 8) | ((c[2] as i32) << 16)) << 8 >> 8;
            w as f32 / 8_388_608.0
        })
        .collect()
}

struct Banc {
    ring: RingBuf,
    volume: AtomicU32,
    paused: AtomicBool,
    silent: AtomicBool,
    data_started: AtomicBool,
    ramp: crate::audio::soft_mute::SoftMuteRamp,
    au_dac: Vec<f32>,
}

impl Banc {
    fn monter() -> Self {
        Self {
            ring: RingBuf::new(TAUX as usize * 2),
            volume: AtomicU32::new(1000),
            paused: AtomicBool::new(false),
            silent: AtomicBool::new(false),
            data_started: AtomicBool::new(false),
            ramp: crate::audio::soft_mute::SoftMuteRamp::new(TAUX, CANAUX),
            au_dac: Vec::new(),
        }
    }

    fn periodes(&mut self, n: usize) {
        for _ in 0..n {
            let mut tampon = vec![0.123f32; PERIODE];
            render_local_shared_f32_callback(
                &self.ring,
                &self.volume,
                &self.paused,
                &self.silent,
                &self.data_started,
                &mut self.ramp,
                0, // DoP : rampe désarmée (`soft_mute::armed_ms`)
                PERIODE,
                &mut tampon,
            );
            self.au_dac.extend_from_slice(&tampon);
        }
    }

    /// Le verdict du puits factice, à partir de la première trame DoP.
    fn juger(&self) {
        let trames: Vec<&[f32]> = self.au_dac.chunks_exact(CANAUX as usize).collect();
        let debut = trames
            .iter()
            .position(|t| matches!(haut_f32(t[0]), 0x05 | 0xFA))
            .expect("aucune trame DoP au DAC");
        let mut precedent: Option<u8> = None;
        for (i, t) in trames[debut..].iter().enumerate() {
            let m = haut_f32(t[0]);
            assert!(
                (m == 0x05 || m == 0xFA) && t.iter().all(|&s| haut_f32(s) == m),
                "trame {i} sans marqueur DoP au DAC (#7704) : {:?} — le porteur est \
                 interrompu, le DAC sort du DSD",
                t
            );
            assert!(
                t.iter().all(|&s| s != 0.0),
                "trame {i} : des zéros envoyés au DAC (#7704)"
            );
            assert_ne!(
                precedent,
                Some(m),
                "trame {i} : alternance 0x05/0xFA rompue"
            );
            precedent = Some(m);
        }
    }

    fn trames_de_repos(&self) -> usize {
        self.au_dac
            .chunks_exact(CANAUX as usize)
            .filter(|t| {
                t.iter()
                    .all(|&s| s == repos_f32(0x05) || s == repos_f32(0xFA))
            })
            .count()
    }
}

#[test]
fn la_pause_rend_des_trames_de_repos_dop_et_la_reprise_garde_l_alternance() {
    let mut b = Banc::monter();
    b.ring.push(&dop_f32(4_000, 0xAA));
    b.periodes(4);
    b.paused.store(true, Ordering::Relaxed);
    b.periodes(5);
    b.paused.store(false, Ordering::Relaxed);
    b.periodes(4);
    b.juger();
    assert!(
        b.trames_de_repos() >= 5 * PERIODE / 2,
        "la pause n'est pas du repos DoP"
    );
}

#[test]
fn la_famine_puis_l_arret_restent_en_repos_dop() {
    let mut b = Banc::monter();
    // 301 trames : la famine tombe au milieu d'une période, sur une trame impaire.
    b.ring.push(&dop_f32(301, 0x33));
    b.periodes(3);
    b.silent.store(true, Ordering::Relaxed); // stop() / silence forcé
    b.periodes(3);
    b.juger();
}

/// Recherche : l'anneau est vidé et re-rempli par une tranche qui repart de
/// 0x05, après des périodes de pré-remplissage. Changement de piste : la
/// suivante arrive bout à bout dans l'anneau.
#[test]
fn la_recherche_et_le_changement_de_piste_ne_rompent_pas_l_alternance() {
    let mut b = Banc::monter();
    b.ring.push(&dop_f32(1_001, 0xAA));
    b.periodes(2);
    // Recherche : on jette ce qui reste, le producteur re-amorce.
    let mut poubelle = vec![0.0f32; b.ring.available()];
    b.ring.pop(&mut poubelle);
    b.data_started.store(false, Ordering::Release);
    b.periodes(2); // pré-remplissage pas atteint
    b.ring.push(&dop_f32(999, 0x96));
    b.ring.push(&dop_f32(777, 0x0F)); // piste suivante enchaînée
    b.periodes(16);
    b.juger();
}

/// Contre-épreuve PCM : le silence d'un flux PCM reste fait de zéros.
#[test]
fn un_flux_pcm_garde_son_silence_de_zeros() {
    let mut b = Banc::monter();
    let pcm: Vec<f32> = (0..PERIODE * 4)
        .map(|i| (i as f32 * 0.003).sin() * 0.25)
        .collect();
    b.ring.push(&pcm);
    b.periodes(4);
    b.paused.store(true, Ordering::Relaxed);
    b.periodes(2);
    assert!(b.au_dac[PERIODE * 4..].iter().all(|&s| s == 0.0));
    assert_eq!(&b.au_dac[..PERIODE * 4], &pcm[..]);
}
