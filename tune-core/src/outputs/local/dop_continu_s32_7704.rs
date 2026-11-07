//! #7704 — le rappel natif ALSA S32 ne coupe plus jamais un porteur DoP.
//! Puits factice : le tampon du pilote, période après période.

use super::*;
use crate::audio::dop_continu::haut_i32;
use crate::audio::dsd_to_dop::DsdToDoP;
use crate::audio::soft_mute::SoftMuteRamp;

fn dop_i32(trames: usize) -> Vec<i32> {
    let mut e = DsdToDoP::new(2, false);
    let mut octets = e.feed(&vec![0xA5; trames * 4]);
    octets.extend_from_slice(&e.terminer());
    pcm_bytes_to_native_i32(&octets, 24)
}

#[test]
fn pause_famine_et_arret_rendent_des_trames_de_repos_dop_en_s32() {
    let anneau = NativePcmRing::new(1 << 16);
    anneau.push(&dop_i32(1_201));
    let volume = AtomicU32::new(UNITE_DU_GAIN_DE_RENDU);
    let paused = AtomicBool::new(false);
    let silent = AtomicBool::new(false);
    let data_started = AtomicBool::new(true);
    let mut rampe = SoftMuteRamp::new(176_400, 2);
    let mut au_dac = Vec::new();
    for p in 0..14 {
        paused.store((3..6).contains(&p), Ordering::Relaxed);
        silent.store(p >= 12, Ordering::Relaxed);
        let mut tampon = vec![0x5A5A_5A5Ai32; 512];
        rendre_une_periode_native_s32(
            &anneau,
            &volume,
            &paused,
            &silent,
            &data_started,
            &mut rampe,
            0,
            0,
            &mut tampon,
        );
        au_dac.extend_from_slice(&tampon);
    }
    let mut precedent = None;
    for (i, t) in au_dac.chunks_exact(2).enumerate() {
        let m = haut_i32(t[0]);
        assert!(
            (m == 0x05 || m == 0xFA) && haut_i32(t[1]) == m && t[0] != 0,
            "trame {i} sans marqueur DoP au DAC (#7704) : {t:08x?}"
        );
        assert_ne!(precedent, Some(m), "trame {i} : alternance rompue");
        precedent = Some(m);
    }
}
