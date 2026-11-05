//! #7859 — l'analyse PAR FENÊTRES, sur des signaux SYNTHÉTIQUES produits par
//! le test : une coupure limitée à une partie du morceau, que le spectre
//! moyen masque ; un master sombre sans coupure ; un vrai hi-res dont le
//! niveau varie. Aucun fichier, aucun son joué.

use super::tests::{quantifier, rose};
use super::*;
use realfft::RealFftPlanner;
use realfft::num_complex::Complex;

/// Longueur d'un segment (celle de [`rose`]) : 2^17 échantillons.
const SEGMENT: usize = 1 << 17;

/// Des segments mis bout à bout, chacun avec son gain en dB.
fn bout_a_bout(segments: &[(Vec<f32>, f32)]) -> Vec<f32> {
    segments
        .iter()
        .flat_map(|(s, gain_db)| {
            let g = 10f32.powf(gain_db / 20.0);
            s.iter().map(move |x| x * g)
        })
        .collect()
}

/// Un master SOMBRE : spectre rose jusqu'à `coude_hz`, puis une pente
/// naturelle de 18 dB par octave en plus, jusqu'à Nyquist. Aucune falaise :
/// l'aigu décroît sans jamais s'arrêter net.
fn sombre(sample_rate: u32, coude_hz: f64, graine: u64) -> Vec<f32> {
    let mut plan = RealFftPlanner::<f32>::new();
    let inverse = plan.plan_fft_inverse(SEGMENT);
    let mut spectre = inverse.make_input_vec();
    let mut alea = graine;
    let hz_par_raie = sample_rate as f64 / SEGMENT as f64;
    for (k, c) in spectre.iter_mut().enumerate() {
        alea = alea
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let phase = ((alea >> 40) as f32) / ((1u64 << 24) as f32) * std::f32::consts::TAU;
        let f = k as f64 * hz_par_raie;
        *c = if f >= 20.0 {
            let a = (1.0 / f.sqrt()) / (1.0 + (f / coude_hz).powi(2)).powf(1.5);
            Complex::new(a as f32 * phase.cos(), a as f32 * phase.sin())
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

/// 🔴 Le cas de l'issue : un 24/96 dont les deux premiers tiers montent à
/// 47 kHz et le dernier tiers est coupé à 20 kHz (un passage d'un CD
/// suréchantillonné). Le spectre MOYEN garde sa falaise à 47 kHz et rendait
/// « vérifié » ; par fenêtres, le dernier tiers est en cause et le morceau
/// est douteux.
#[test]
fn une_coupure_sur_un_tiers_du_morceau_est_detectee_7859() {
    let signal = bout_a_bout(&[
        (rose(96_000, 47_000.0, 21), 0.0),
        (rose(96_000, 47_000.0, 22), 0.0),
        (rose(96_000, 20_000.0, 23), 0.0),
    ]);
    let pcm = quantifier(&signal, 24);

    // Ce que voyait l'ancien spectre moyen : pas de coupure suspecte.
    let moyenne = coupure_du_spectre_moyen(&pcm, 1, 96_000, 24);
    assert!(
        moyenne.is_none_or(|f| !coupure_suspecte(96_000, f)),
        "le spectre moyen devait masquer la coupure (sinon le témoin ne prouve rien) : {moyenne:?}"
    );

    let m = mesurer(&pcm, 1, 96_000, 24);
    let v = juger(&m);
    assert!(
        matches!(v, Verdict::Douteux(Raison::BandeDeCd { .. })),
        "un tiers du morceau coupé à 20 kHz doit rendre le morceau douteux, \
         pas {v:?} ; part coupée {:.2} — {m:?}",
        m.part_coupee()
    );
    let f = m.coupure_hz.expect("la coupure retenue");
    assert!((f - 20_000.0).abs() < 400.0, "coupure retenue {f:.0} Hz : {m:?}");
    // Les fenêtres en cause sont celles du dernier tiers, et seulement elles.
    let debut_du_tiers = 2.0 * SEGMENT as f64 / 96_000.0;
    let trame_s = TAILLE_FFT as f64 / 96_000.0;
    assert!(!m.fenetres_coupees.is_empty());
    for c in &m.fenetres_coupees {
        assert!(
            c.fin_s > debut_du_tiers + trame_s,
            "fenêtre en cause hors du passage coupé : {c:?}"
        );
    }
    let fin = 3.0 * SEGMENT as f64 / 96_000.0;
    assert!(m.fenetres_coupees.last().unwrap().fin_s <= fin + 1e-9);
}

/// Quelques fenêtres coupées sur tout un morceau (moins de
/// [`PART_COUPEE_MIN`]) ne suffisent pas : le morceau reste vérifié, mais les
/// fenêtres en cause sont listées.
#[test]
fn quelques_fenetres_coupees_ne_suffisent_pas_7859() {
    let mut segments: Vec<(Vec<f32>, f32)> = (0..9)
        .map(|i| (rose(96_000, 47_000.0, 30 + i), 0.0))
        .collect();
    segments.insert(4, (rose(96_000, 20_000.0, 40), 0.0));
    let m = mesurer(&quantifier(&bout_a_bout(&segments), 24), 1, 96_000, 24);
    assert!(m.part_coupee() > 0.0 && m.part_coupee() < PART_COUPEE_MIN, "{m:?}");
    assert_eq!(juger(&m), Verdict::Verifie, "{m:?}");
}

/// TÉMOIN — un master SOMBRE (pente forte dès 6 kHz, sans falaise), dont le
/// niveau varie d'un passage à l'autre jusqu'à -40 dB, n'est pas accusé,
/// en 24/96 comme en 16/44,1 : aucune fenêtre n'est en cause.
#[test]
fn un_master_sombre_sans_coupure_n_est_pas_accuse_7859() {
    for (sample_rate, bits, coude, attendu) in [
        (96_000, 24, 6_000.0, Verdict::Verifie),
        (44_100, 16, 4_000.0, Verdict::Conforme),
    ] {
        let signal = bout_a_bout(&[
            (sombre(sample_rate, coude, 51), 0.0),
            (sombre(sample_rate, coude, 52), -20.0),
            (sombre(sample_rate, coude, 53), -40.0),
            (sombre(sample_rate, coude, 54), -6.0),
        ]);
        let m = mesurer(&quantifier(&signal, bits), 1, sample_rate, bits);
        assert!(m.fenetres_jugees > 0, "{m:?}");
        assert!(
            m.fenetres_coupees.is_empty(),
            "master sombre {sample_rate} Hz : aucune fenêtre ne doit être en cause — {m:?}"
        );
        assert_eq!(juger(&m), attendu, "master sombre {sample_rate} Hz : {m:?}");
    }
}

/// TÉMOIN — un VRAI 24/96 sur tout le morceau, passages forts et faibles :
/// vérifié, aucune fenêtre en cause, toutes les fenêtres jugées.
#[test]
fn un_vrai_hi_res_n_est_pas_accuse_7859() {
    let signal = bout_a_bout(&[
        (rose(96_000, 47_000.0, 61), 0.0),
        (rose(96_000, 47_000.0, 62), -20.0),
        (rose(96_000, 47_000.0, 63), -40.0),
        (rose(96_000, 47_000.0, 64), -3.0),
    ]);
    let m = mesurer(&quantifier(&signal, 24), 1, 96_000, 24);
    assert_eq!(juger(&m), Verdict::Verifie, "{m:?}");
    assert!(m.fenetres_coupees.is_empty(), "{m:?}");
    assert_eq!(
        m.fenetres_jugees as usize,
        fenetres(4 * SEGMENT / TAILLE_FFT, FENETRES_MAX).len(),
        "{m:?}"
    );
}

/// Les fenêtres couvrent toute la tranche, du début à la fin, sans dépasser
/// [`FENETRES_MAX`].
#[test]
fn les_fenetres_sont_reparties_sur_toute_la_duree_7859() {
    let f = fenetres(10_000, FENETRES_MAX);
    assert_eq!(f.len(), FENETRES_MAX);
    assert_eq!(f[0], 0..TRAMES_PAR_FENETRE);
    assert_eq!(f.last().unwrap().end, 10_000);
    assert!(f.windows(2).all(|w| w[0].start < w[1].start));
    assert_eq!(fenetres(3, FENETRES_MAX), vec![0..3]);
    assert_eq!(fenetres(8, FENETRES_MAX), vec![0..4, 4..8]);
    assert!(fenetres(0, FENETRES_MAX).is_empty());
}

/// Les tranches d'un même morceau : les fenêtres en cause portent la position
/// de leur tranche dans le morceau, et le budget de fenêtres est partagé.
#[test]
fn les_fenetres_en_cause_portent_leur_position_dans_le_morceau_7859() {
    let pleine = quantifier(&rose(96_000, 47_000.0, 71), 24);
    let coupee = quantifier(&rose(96_000, 20_000.0, 72), 24);
    let m = mesurer_des_tranches(&[(0.0, pleine.as_slice()), (120.0, coupee.as_slice())], 1, 96_000, 24);
    assert!(matches!(juger(&m), Verdict::Douteux(Raison::BandeDeCd { .. })), "{m:?}");
    assert!(m.fenetres_coupees.iter().all(|c| c.debut_s >= 120.0), "{m:?}");
    assert!(m.fenetres_jugees as usize <= FENETRES_MAX);
}
