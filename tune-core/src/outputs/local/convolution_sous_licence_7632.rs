//! #7632 — la convolution par réponse impulsionnelle est un droit Premium
//! (`RoomCorrection`, décision du 04/11/2026). Sur la chaîne locale RÉELLE
//! (`apply_local_dsp`, `rebuild_local_convolver`) : sans le droit, le PCM sort
//! identique à l'entrée ; avec, il est convolué ; retiré flux en cours, le
//! moteur s'éteint en fondu, sans saut d'échantillon.

use super::*;

const TAUX: u32 = 48_000;

/// Une RI mono WAV flottant 32 bits : un Dirac de gain 0,25.
fn ri_quart() -> crate::test_scratch::ScratchFile {
    let f = crate::test_scratch::scratch_file("ri-7632-licence-locale", ".wav");
    let taps = 64usize;
    let donnees = taps * 4;
    let mut w = Vec::with_capacity(44 + donnees);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&((36 + donnees) as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&3u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&TAUX.to_le_bytes());
    w.extend_from_slice(&(TAUX * 4).to_le_bytes());
    w.extend_from_slice(&4u16.to_le_bytes());
    w.extend_from_slice(&32u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(donnees as u32).to_le_bytes());
    for i in 0..taps {
        let v: f32 = if i == 0 { 0.25 } else { 0.0 };
        w.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(f.path(), w).unwrap();
    f
}

fn sinus(trames: usize) -> Vec<f32> {
    (0..trames)
        .flat_map(|n| {
            let v = (2.0 * std::f32::consts::PI * 997.0 * n as f32 / TAUX as f32).sin() * 0.5;
            [v, -v]
        })
        .collect()
}

fn passer(sortie: &LocalOutput, pcm: &mut [f32]) {
    apply_local_dsp(
        pcm,
        &sortie.eq,
        &sortie.convolver,
        &sortie.crossfeed,
        &sortie.pure_bypass,
        &sortie.mono_downmix,
        2,
        false,
    );
}

/// Une sortie qui joue du 48 kHz stéréo, IR réglée, droit `autorisee`.
fn sortie(autorisee: bool, ri: &crate::test_scratch::ScratchFile) -> LocalOutput {
    let s = LocalOutput::new("DAC test".to_string());
    s.declare_current_format_for_test(TAUX, 2);
    s.set_convolution_autorisee(autorisee);
    s.set_convolver_ir(&ri.as_str()).unwrap();
    s
}

#[test]
fn sans_licence_le_pcm_sort_identique_a_l_entree_7632() {
    let ri = ri_quart();
    let s = sortie(false, &ri);
    assert!(s.has_convolver(), "l'IR reste mémorisée");
    let entree = sinus(4_800);
    let mut pcm = entree.clone();
    passer(&s, &mut pcm);
    assert_eq!(pcm, entree, "sans Premium, la RI a été convoluée");

    // La piste suivante (ou un changement de format) ne bâtit aucun moteur.
    let bati = rebuild_local_convolver(
        &s.convolver_config,
        &s.convolver,
        &s.convolution_autorisee,
        TAUX,
        2,
    )
    .unwrap();
    assert!(!bati, "un moteur a été bâti sans le droit");
    let mut pcm = entree.clone();
    passer(&s, &mut pcm);
    assert_eq!(pcm, entree);
}

#[test]
fn avec_premium_la_convolution_s_applique_7632() {
    let ri = ri_quart();
    let s = sortie(true, &ri);
    let entree = sinus(4_800);
    let mut pcm = entree.clone();
    passer(&s, &mut pcm);
    let lat = 1024;
    assert_ne!(pcm, entree);
    for n in (lat + 1)..4_800 {
        assert!(
            (pcm[2 * n] - 0.25 * entree[2 * (n - lat)]).abs() < 1e-4,
            "trame {n} : {} au lieu de {}",
            pcm[2 * n],
            0.25 * entree[2 * (n - lat)]
        );
    }
}

/// Le droit retiré pendant la lecture : fondu de 20 ms, puis plus aucune
/// convolution (l'entrée ressort, retardée de la latence, à l'échantillon
/// près), sans aucun saut plus grand que la pente du signal ; à la piste
/// suivante, pas de moteur.
#[test]
fn le_droit_retire_en_lecture_eteint_la_convolution_sans_clic_7632() {
    let ri = ri_quart();
    let s = sortie(true, &ri);
    let lat = 1024;
    let entree = sinus(24_000);
    let mut pcm = entree.clone();
    let coupe = 9_600;
    passer(&s, &mut pcm[..coupe * 2]);
    assert!(s.set_convolution_autorisee(false), "l'état devait changer");
    for bloc in pcm[coupe * 2..].chunks_mut(480 * 2) {
        passer(&s, bloc);
    }
    let fondu = (TAUX / 50) as usize;
    for n in (coupe + fondu)..24_000 {
        assert_eq!(
            pcm[2 * n],
            entree[2 * (n - lat)],
            "trame {n} encore convoluée"
        );
    }
    let pente = entree
        .chunks(2)
        .zip(entree.chunks(2).skip(1))
        .map(|(a, b)| (b[0] - a[0]).abs())
        .fold(0.0f32, f32::max);
    for n in (lat + 1)..24_000 {
        let pas = (pcm[2 * n] - pcm[2 * (n - 1)]).abs();
        assert!(
            pas <= pente * 1.05,
            "saut de {pas} trame {n} (pente {pente}) : clic"
        );
    }
    let bati = rebuild_local_convolver(
        &s.convolver_config,
        &s.convolver,
        &s.convolution_autorisee,
        TAUX,
        2,
    )
    .unwrap();
    assert!(!bati);
}
