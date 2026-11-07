//! #7703 — la quantification finale des chemins natifs exclusifs se fait APRÈS
//! le volume logiciel, en flottant, et avec un dither TPDF.
//!
//! Le puits factice est le vecteur de mots que `prepare_windows_native_pcm`
//! rend : c'est exactement ce que l'étage natif pousse dans l'anneau du
//! périphérique (WASAPI le resérialise en octets hauts, ASIO le lit tel quel).

use super::*;

fn dsp_vide() -> (
    std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    AtomicBool,
) {
    (
        std::sync::Mutex::new(None),
        std::sync::Mutex::new(None),
        std::sync::Mutex::new(None),
        AtomicBool::new(false),
    )
}

/// Un bloc 16 bits stéréo constant de `mot` LSB, `trames` trames.
fn bloc_16(mot: i16, trames: usize) -> Vec<u8> {
    (0..trames * 2).flat_map(|_| mot.to_le_bytes()).collect()
}

/// Le puits : les mots 16 bits rendus, ramenés en LSB.
fn au_puits_16(octets: &[u8], volume_units: u32) -> Vec<i32> {
    let (eq, conv, cf, pure) = dsp_vide();
    let prepared = prepare_windows_native_pcm_a_voies_egales(
        octets,
        16,
        2,
        false,
        false,
        volume_units,
        &eq,
        &conv,
        &cf,
        &pure,
        &AtomicBool::new(false),
        None,
    )
    .expect("bloc PCM 16 bits");
    assert!(!prepared.dop);
    prepared.samples.iter().map(|w| w >> 16).collect()
}

/// Le scénario de l'audit : piste 16 bits, volume à 3 % (≈ −30 dB). Un mot
/// source de 10 LSB vaut 0,3 LSB après le gain. L'arrondi nu le rend à 0 —
/// le signal DISPARAÎT, et l'erreur (−0,3 LSB) suit exactement le signal :
/// c'est une distorsion, pas un bruit. Avec le dither, la moyenne au puits
/// retrouve les 0,3 LSB.
#[test]
fn a_volume_reduit_le_mot_16_bits_est_dithere_apres_le_gain() {
    let trames = 48_000;
    let mots = au_puits_16(&bloc_16(10, trames), 30);
    let moyenne = mots.iter().map(|&m| f64::from(m)).sum::<f64>() / mots.len() as f64;
    assert!(
        (moyenne - 0.3).abs() < 0.03,
        "quantification finale sans dither après le volume (#7703) : moyenne au \
         puits {moyenne:+.4} LSB pour 0,3 LSB attendus — le signal atténué est \
         arrondi à nu, l'erreur est corrélée au signal"
    );
    // TPDF ±1 LSB : jamais plus d'un LSB d'écart avec la valeur idéale arrondie.
    assert!(
        mots.iter().all(|&m| (-1..=2).contains(&m)),
        "bruit hors de ±1 LSB"
    );
}

/// L'ordre gain → dither : le gain s'applique AVANT la quantification. Si le
/// mot était quantifié puis multiplié, un mot source de 10 LSB à 50 % ne
/// donnerait jamais une moyenne de 5 LSB à ±0,05 près avec des mots entiers
/// tous dans [4, 6] — c'est ce que mesure la deuxième assertion.
#[test]
fn le_gain_precede_la_quantification() {
    let mots = au_puits_16(&bloc_16(10, 48_000), 500);
    let moyenne = mots.iter().map(|&m| f64::from(m)).sum::<f64>() / mots.len() as f64;
    assert!((moyenne - 5.0).abs() < 0.05, "moyenne {moyenne}");
    assert!(mots.iter().all(|&m| (4..=6).contains(&m)));
}

/// Le chemin bit-perfect ne reçoit AUCUN bruit : volume unité, DSP absent.
#[test]
fn a_volume_unite_sans_dsp_les_mots_sortent_intacts() {
    let mots = au_puits_16(&bloc_16(10, 4_800), 1000);
    assert!(
        mots.iter().all(|&m| m == 10),
        "le chemin bit-perfect a été bruité"
    );
}

/// À 32 bits, pas de dither : l'arrondi reste exact et déterministe.
#[test]
fn a_32_bits_aucun_dither() {
    let s = [0.25f32, -0.5, 0.0, 1.0e-6];
    let mots = f32_to_native_i32(&s, 32);
    let attendu: Vec<i32> = s
        .iter()
        .map(|&v| {
            (f64::from(v) * 2_147_483_648.0)
                .round()
                .clamp(-2_147_483_648.0, 2_147_483_647.0) as i32
        })
        .collect();
    assert_eq!(mots, attendu);
}

/// Même bloc, même bruit : la quantification est reproductible.
#[test]
fn le_dither_est_deterministe() {
    let s: Vec<f32> = (0..1024).map(|i| (i as f32 * 0.01).sin() * 0.001).collect();
    assert_eq!(f32_to_native_i32(&s, 16), f32_to_native_i32(&s, 16));
}

/// Un bloc déjà exact (gain unité, matrice qui choisit ou coupe des voies)
/// sort à l'identité : pas de requantification, pas de dither.
#[test]
fn un_bloc_exact_sort_sans_bruit() {
    let s: Vec<f32> = [0i32, 1000, -2000, 0, 32_767, -32_768]
        .iter()
        .map(|&m| m as f32 / 32_768.0)
        .collect();
    let mots: Vec<i32> = f32_to_native_i32(&s, 16).iter().map(|w| w >> 16).collect();
    assert_eq!(mots, vec![0, 1000, -2000, 0, 32_767, -32_768]);
}
