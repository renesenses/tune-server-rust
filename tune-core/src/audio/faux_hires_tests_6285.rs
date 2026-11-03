//! #6285 — témoins sur des signaux SYNTHÉTIQUES produits par le test : un vrai
//! 24/96, un CD gonflé en 24/96, un CD suréchantillonné à bits bas remplis,
//! un spectre coupé à 16 kHz, un vrai CD. Aucun fichier, aucun son joué.

use super::*;
use realfft::RealFftPlanner;
use realfft::num_complex::Complex;

/// Longueur des signaux : 2^17 échantillons (1,4 s à 96 kHz).
const N: usize = 1 << 17;

/// Générateur pseudo-aléatoire déterministe (LCG), pour les phases.
struct Lcg(u64);
impl Lcg {
    fn suivant(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 40) as f32) / ((1u64 << 24) as f32)
    }
}

/// Un signal mono de spectre ROSE (énergie en 1/f, la pente naturelle d'un
/// enregistrement) de 20 Hz à `bande_hz`, rien au-dessus, normalisé à -6 dBFS
/// de crête, synthétisé par FFT inverse avec des phases aléatoires.
pub(crate) fn rose(sample_rate: u32, bande_hz: f64, graine: u64) -> Vec<f32> {
    let mut plan = RealFftPlanner::<f32>::new();
    let inverse = plan.plan_fft_inverse(N);
    let mut spectre = inverse.make_input_vec();
    let mut alea = Lcg(graine);
    let hz_par_raie = sample_rate as f64 / N as f64;
    for (k, c) in spectre.iter_mut().enumerate() {
        let f = k as f64 * hz_par_raie;
        let phase = alea.suivant() * std::f32::consts::TAU;
        *c = if (20.0..bande_hz).contains(&f) {
            let a = (1.0 / f.sqrt()) as f32;
            Complex::new(a * phase.cos(), a * phase.sin())
        } else {
            Complex::new(0.0, 0.0)
        };
    }
    let n = spectre.len();
    spectre[n - 1].im = 0.0;
    let mut sortie = inverse.make_output_vec();
    inverse.process(&mut spectre, &mut sortie).unwrap();
    let crete = sortie.iter().fold(0f32, |m, s| m.max(s.abs()));
    sortie.iter().map(|s| s / crete * 0.5).collect()
}

/// Quantifie sur `bits` bits (arrondi), en entiers comme les rend le décodeur.
pub(crate) fn quantifier(signal: &[f32], bits: u32) -> Vec<i32> {
    let pleine = (1i64 << (bits - 1)) as f32;
    signal.iter().map(|s| (s * pleine).round() as i32).collect()
}

fn verdict(echantillons: &[i32], sample_rate: u32, bits: u32) -> (Mesure, Verdict) {
    let m = mesurer(echantillons, 1, sample_rate, bits);
    (m, juger(&m))
}

fn coupure_vers(m: &Mesure, attendue: f64) {
    let f = m
        .coupure_hz
        .unwrap_or_else(|| panic!("une coupure était attendue vers {attendue} Hz : {m:?}"));
    assert!(
        (f - attendue).abs() < 400.0,
        "coupure mesurée {f:.0} Hz, attendue vers {attendue} Hz : {m:?}"
    );
}

/// 🔴 Un VRAI 24/96 : contenu jusqu'à 47 kHz, pente naturelle, 24 bits pleins.
#[test]
fn un_vrai_24_96_est_verifie_6285() {
    let pcm = quantifier(&rose(96_000, 47_000.0, 1), 24);
    let (m, v) = verdict(&pcm, 96_000, 24);
    assert_eq!(m.bits_effectifs, Some(24), "{m:?}");
    assert_eq!(
        v,
        Verdict::Verifie,
        "un vrai 24/96 doit être vérifié : {m:?}"
    );
}

/// 🔴 Un CD GONFLÉ en 24/96 : bande de 20 kHz, 16 bits décalés de 8 (les
/// huit bits de poids faible toujours nuls).
#[test]
fn un_cd_gonfle_en_24_96_est_douteux_par_les_bits_6285() {
    let pcm: Vec<i32> = quantifier(&rose(96_000, 20_000.0, 2), 16)
        .into_iter()
        .map(|s| s << 8)
        .collect();
    let (m, v) = verdict(&pcm, 96_000, 24);
    assert_eq!(
        v,
        Verdict::Douteux(Raison::BitsDeRemplissage { effectifs: 16 }),
        "les 8 bits bas sont nuls : {m:?}"
    );
    coupure_vers(&m, 20_000.0);
}

/// 🔴 Un CD SURÉCHANTILLONNÉ dont les bits bas ont été remplis (gain appliqué
/// après conversion) : seule la bande le trahit — rien au-dessus de 21 kHz.
#[test]
fn un_cd_suréchantillonné_est_douteux_par_la_bande_6285() {
    let pcm = quantifier(&rose(96_000, 21_000.0, 3), 24);
    let (m, v) = verdict(&pcm, 96_000, 24);
    assert_eq!(m.bits_effectifs, Some(24), "{m:?}");
    assert!(
        matches!(v, Verdict::Douteux(Raison::BandeDeCd { .. })),
        "un 96 kHz sans rien au-dessus de 21 kHz est un CD suréchantillonné : {v:?} {m:?}"
    );
    coupure_vers(&m, 21_000.0);
}

/// 🔴 Un spectre COUPÉ à 16 kHz dans un FLAC 16/44,1 : la signature d'un MP3.
#[test]
fn un_spectre_coupe_a_16_khz_est_douteux_codec_6285() {
    let pcm = quantifier(&rose(44_100, 16_000.0, 4), 16);
    let (m, v) = verdict(&pcm, 44_100, 16);
    assert!(
        matches!(v, Verdict::Douteux(Raison::CoupureDeCodec { .. })),
        "coupé à 16 kHz : signature d'un codec avec perte — {v:?} {m:?}"
    );
    coupure_vers(&m, 16_000.0);
}

/// La même coupure à 16 kHz dans un conteneur 24/96 : codec, pas CD.
#[test]
fn un_mp3_gonfle_en_24_96_est_douteux_codec_6285() {
    let pcm = quantifier(&rose(96_000, 16_000.0, 5), 24);
    let (m, v) = verdict(&pcm, 96_000, 24);
    assert!(
        matches!(v, Verdict::Douteux(Raison::CoupureDeCodec { .. })),
        "{v:?} {m:?}"
    );
}

/// TÉMOIN — un VRAI CD (bande jusqu'à 21,8 kHz, le filtre anti-repliement)
/// n'est pas soupçonné : pas de faux positif sur la qualité CD.
#[test]
fn un_vrai_cd_est_conforme_6285() {
    let pcm = quantifier(&rose(44_100, 21_800.0, 6), 16);
    let (m, v) = verdict(&pcm, 44_100, 16);
    assert_eq!(v, Verdict::Conforme, "{m:?}");
}

/// Le silence ne se juge pas.
#[test]
fn le_silence_n_est_pas_concluant_6285() {
    let (m, v) = verdict(&vec![0i32; N], 96_000, 24);
    assert_eq!(v, Verdict::NonConcluant, "{m:?}");
}

#[test]
fn bits_effectifs_compte_les_bits_bas_toujours_nuls_6285() {
    assert_eq!(bits_effectifs(&[0x100, -0x300, 0x7f00], 24), Some(16));
    assert_eq!(bits_effectifs(&[1, 0x100], 24), Some(24));
    assert_eq!(bits_effectifs(&[0, 0], 24), None);
}
