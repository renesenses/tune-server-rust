//! #7704 — puits exclusifs : repos DoP, observation et raccord sans allocation.
#![cfg_attr(
    not(any(target_os = "windows", target_os = "macos", test)),
    allow(dead_code)
)]
use super::local::RingBuf;
#[cfg(any(target_os = "windows", target_os = "linux", test))]
use super::local::{NativePcmRing, gain_au_rendu_7264::GainAuRendu};
use crate::audio::dop_continu::{SuiviDoP, haut_f32, repos_f32};
#[cfg(any(target_os = "windows", target_os = "linux", test))]
use crate::audio::dop_continu::{haut_i32, repos_i32};

fn raccord<T: Copy>(
    dop: &mut SuiviDoP,
    premier: Option<u8>,
    out: &mut [T],
    zero: T,
    repos: impl FnMut(u8) -> T,
) -> usize {
    let ch = dop.canaux();
    if dop.actif() && premier.is_some() && premier == dop.dernier_marqueur() && out.len() >= ch {
        dop.combler(&mut out[..ch], zero, repos);
        ch
    } else {
        0
    }
}

/// Rend `true` sur famine, hors pause/silence de libération.
#[allow(clippy::too_many_arguments)]
#[cfg(any(target_os = "windows", target_os = "linux", test))]
pub(crate) fn rendre_natif<T: Copy>(
    ring: &NativePcmRing,
    out: &mut [T],
    bits: u16,
    gain: &mut GainAuRendu,
    dop: &mut SuiviDoP,
    muet: bool,
    zero: T,
    map: impl Fn(i32) -> T,
) -> bool {
    if muet {
        dop.combler(out, zero, |m| map(repos_i32(m)));
        ring.starvation().record_silent_period(out.len());
        return false;
    }
    let debut = raccord(
        dop,
        ring.premier_en_attente().map(haut_i32),
        out,
        zero,
        |m| map(repos_i32(m)),
    );
    ring.starvation().record_silent_period(debut);
    let n = out.len() - debut;
    let lus = ring.pop_mapped_au_gain(&mut out[debut..debut + n], bits, gain, |w| {
        if bits >= 24 {
            dop.observer_marqueur(haut_i32(w));
        }
        map(w)
    });
    let fin = debut + lus;
    dop.combler(&mut out[fin..], zero, |m| map(repos_i32(m)));
    fin < out.len()
}

/// Même contrat pour un anneau flottant (CoreAudio/ASIO).
pub(crate) fn rendre_flottant<T: Copy>(
    ring: &RingBuf,
    out: &mut [T],
    gain: f64,
    dop: &mut SuiviDoP,
    muet: bool,
    zero: T,
    mut map: impl FnMut(f32, f64) -> T,
) -> bool {
    if muet {
        dop.combler(out, zero, |m| map(repos_f32(m), 1.0));
        ring.starvation().record_silent_period(out.len());
        return false;
    }
    let debut = raccord(
        dop,
        ring.premier_en_attente().map(haut_f32),
        out,
        zero,
        |m| map(repos_f32(m), 1.0),
    );
    ring.starvation().record_silent_period(debut);
    let n = out.len() - debut;
    let lus = ring.pop_mapped(&mut out[debut..debut + n], |s| {
        dop.observer_marqueur(haut_f32(s));
        map(s, gain)
    });
    let fin = debut + lus;
    dop.combler(&mut out[fin..], zero, |m| map(repos_f32(m), 1.0));
    fin < out.len()
}

#[cfg(any(target_os = "windows", target_os = "linux", test))]
fn combler_octets(dop: &mut SuiviDoP, out: &mut [u8], bits: u16) {
    let b = usize::from(bits / 8);
    let ch = dop.canaux();
    for trame in out.chunks_exact_mut(ch * b) {
        let mot = dop.marqueur_de_repos().map(repos_i32).unwrap_or(0);
        let octets = mot.to_le_bytes();
        for mot in trame.chunks_exact_mut(b) {
            mot.copy_from_slice(&octets[4 - b..]);
        }
    }
}

/// WASAPI : mots 16/24/32 bits sérialisés dans le tampon du pilote.
#[cfg(any(target_os = "windows", target_os = "linux", test))]
pub(crate) fn rendre_octets(
    ring: &NativePcmRing,
    out: &mut [u8],
    bits: u16,
    gain: &mut GainAuRendu,
    dop: &mut SuiviDoP,
    muet: bool,
) -> bool {
    let b = usize::from(bits / 8);
    let frame = dop.canaux() * b;
    if muet {
        combler_octets(dop, out, bits);
        ring.starvation().record_silent_period(out.len() / b);
        return false;
    }
    let mut debut = 0;
    if dop.actif()
        && out.len() >= frame
        && ring.premier_en_attente().map(haut_i32) == dop.dernier_marqueur()
    {
        combler_octets(dop, &mut out[..frame], bits);
        debut = frame;
    }
    ring.starvation().record_silent_period(debut / b);
    let n = out.len() - debut;
    let lus = ring.pop_pcm_bytes_au_gain(&mut out[debut..debut + n], bits, gain);
    if bits >= 24 {
        for mot in out[debut..debut + lus].chunks_exact(b) {
            dop.observer_marqueur(mot[b - 1]);
        }
    }
    combler_octets(dop, &mut out[debut + lus..], bits);
    debut + lus < out.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, atomic::AtomicU32};

    fn gain() -> GainAuRendu {
        GainAuRendu::nouveau(
            Arc::new(AtomicU32::new(
                crate::audio::volume_scale::UNITE_DU_GAIN_DE_RENDU,
            )),
            176_400,
            2,
        )
    }
    fn mots(n: usize) -> Vec<i32> {
        (0..n)
            .flat_map(|i| {
                let m = if i % 2 == 0 { 0x05u32 } else { 0xFA };
                [((m << 24) | 0x123400) as i32; 2]
            })
            .collect()
    }
    fn verifier(flux: &[i32]) {
        let mut precedent = None;
        for trame in flux.chunks_exact(2) {
            let m = haut_i32(trame[0]);
            assert!(matches!(m, 0x05 | 0xFA), "marqueur absent : {trame:?}");
            assert_eq!(haut_i32(trame[1]), m);
            assert_ne!(precedent, Some(m), "rupture de phase");
            precedent = Some(m);
        }
    }
    fn repos(flux: &[i32]) {
        assert!(flux.iter().all(|w| (*w as u32 & 0x00ffff00) == 0x00696900));
    }

    #[test]
    fn dop_7704_wasapi_pause_recherche_famine_arret_et_reprise() {
        for bits in [24, 32] {
            let ring = NativePcmRing::new(1024);
            ring.marquer_passage_intact(true);
            let source = mots(64);
            ring.push(&source);
            let mut suivi = SuiviDoP::new(2);
            let mut g = gain();
            let mut flux = Vec::new();
            // Reconnaissance, pause impaire, reprise avec raccord, famine,
            // pré-remplissage de recherche et silence précédant la fermeture.
            for (trames, muet) in [
                (32, false),
                (3, true),
                (33, false),
                (3, false),
                (3, true),
                (3, true),
            ] {
                let b = usize::from(bits / 8);
                let mut out = vec![0xcc; trames * 2 * b];
                rendre_octets(&ring, &mut out, bits, &mut g, &mut suivi, muet);
                let rendu: Vec<i32> = out
                    .chunks_exact(b)
                    .map(|w| {
                        let mut octets = [0; 4];
                        octets[4 - b..].copy_from_slice(w);
                        i32::from_le_bytes(octets)
                    })
                    .collect();
                if muet || trames == 3 {
                    repos(&rendu);
                }
                flux.extend(rendu);
            }
            verifier(&flux);
            let reels: Vec<_> = flux
                .into_iter()
                .filter(|w| (*w as u32 & 0x00ffff00) == 0x00123400)
                .collect();
            assert_eq!(reels, source, "aucun mot réel perdu ou altéré");
        }
    }

    #[test]
    fn dop_7704_asio_natif_repos_et_raccord() {
        let ring = NativePcmRing::new(1024);
        ring.marquer_passage_intact(true);
        ring.push(&mots(64));
        let mut suivi = SuiviDoP::new(2);
        let mut g = gain();
        let mut flux = Vec::new();
        for (n, muet) in [(64, false), (6, true), (66, false), (6, false), (6, true)] {
            let mut out = vec![0; n];
            rendre_natif(&ring, &mut out, 24, &mut g, &mut suivi, muet, 0, |w| w);
            if muet || n == 6 {
                repos(&out);
            }
            flux.extend(out);
        }
        verifier(&flux);
        let releve = ring.starvation().snapshot();
        assert_eq!(releve.events, 1, "la pause ne compte pas comme famine");
        assert_eq!(
            releve.missing_samples, 6,
            "le repos ne masque pas la famine"
        );
        assert_eq!(
            releve.served_samples, 148,
            "les périodes muettes avancent l'horloge"
        );
    }

    #[test]
    fn dop_7704_coreaudio_asio_flottant_repos_et_raccord() {
        let ring = RingBuf::new(1024);
        let source: Vec<_> = mots(64)
            .iter()
            .map(|w| (*w >> 8) as f32 / 8_388_608.0)
            .collect();
        ring.push(&source);
        let mut suivi = SuiviDoP::new(2);
        let mut flux = Vec::new();
        for (n, muet) in [(64, false), (6, true), (66, false), (6, false), (6, true)] {
            let mut out = vec![0.0; n];
            rendre_flottant(&ring, &mut out, 1.0, &mut suivi, muet, 0.0, |s, g| {
                (f64::from(s) * g) as f32
            });
            let rendu: Vec<_> = out
                .iter()
                .map(|s| ((*s * 8_388_608.0) as i32) << 8)
                .collect();
            if muet || n == 6 {
                repos(&rendu);
            }
            flux.extend(rendu);
        }
        verifier(&flux);
    }

    #[test]
    fn dop_7704_pcm_conserve_des_zeros() {
        let ring = NativePcmRing::new(128);
        let mut g = gain();
        let mut suivi = SuiviDoP::new(2);
        for muet in [false, true] {
            let mut out = [42; 24];
            rendre_octets(&ring, &mut out, 24, &mut g, &mut suivi, muet);
            assert_eq!(out, [0; 24]);
        }
        let ring = RingBuf::new(128);
        let mut out = [1.0; 8];
        rendre_flottant(&ring, &mut out, 1.0, &mut suivi, true, 0.0, |s, g| {
            (f64::from(s) * g) as f32
        });
        assert_eq!(out, [0.0; 8]);
    }
}
