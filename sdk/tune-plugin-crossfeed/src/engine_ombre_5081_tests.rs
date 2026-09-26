//! #5081 — les témoins du filtre d'ombre de la tête, au niveau du moteur :
//! pente mesurée AU SIGNAL, Mid préservé, absence de saut au changement de
//! réglage, niveau moyen calculé = mesuré.

use super::*;

/// La pente mesurée au SIGNAL : un sinus calé à droite (L = 0) traverse le
/// vrai processeur, sans retard, et on relève l'amplitude du terme croisé
/// reçu à gauche, filtre allumé, à `fc` et à `8·fc`.
fn pente_mesuree_au_signal(sr: u32, fc: f32, pente: f32) -> f64 {
    let niveau = |hz: f64| -> f64 {
        let ombre = OmbreDeTete {
            cutoff_hz: fc,
            slope_db_per_octave: pente,
        };
        let mut cf = CrossfeedProcessor::avec_ombre(sr, 0.25, 0.0, Some(ombre));
        let trames = sr as usize; // 1 s
        let mut s: Vec<f32> = (0..trames)
            .flat_map(|n| {
                let x = (2.0 * std::f64::consts::PI * hz * n as f64 / f64::from(sr)).sin();
                [0.0, (0.5 * x) as f32]
            })
            .collect();
        cf.process_interleaved(&mut s);
        // Crête du canal gauche sur la seconde moitié (transitoire passé).
        let crete = s
            .as_chunks::<2>()
            .0
            .iter()
            .skip(trames / 2)
            .map(|p| f64::from(p[0]).abs())
            .fold(0.0, f64::max);
        20.0 * crete.log10()
    };
    let fc = f64::from(fc);
    (niveau(fc) - niveau(8.0 * fc)) / 3.0
}

/// #5081 — la pente RÉELLE, mesurée sur le signal qui traverse le moteur,
/// reste à ±0,5 dB/oct de la consigne pour 3, 4,5 et 6 dB/oct.
#[test]
fn la_pente_mesuree_au_signal_suit_la_consigne_5081() {
    for (sr, fc) in [(48_000_u32, 700.0_f32), (44_100, 1200.0), (96_000, 200.0)] {
        for consigne in [3.0_f32, 4.5, 6.0] {
            let mesuree = pente_mesuree_au_signal(sr, fc, consigne);
            eprintln!(
                "crossfeed {sr} Hz, fc {fc} Hz : consigne {consigne} dB/oct, mesurée au signal {mesuree:.2} dB/oct"
            );
            assert!(
                (mesuree - f64::from(consigne)).abs() <= 0.5,
                "pente mesurée {mesuree:.3} dB/oct pour une consigne de {consigne} dB/oct \
                 ({sr} Hz, fc {fc} Hz) : hors de la tolérance de ±0,5"
            );
        }
    }
}

/// L'implémentation d'AVANT #5081, recopiée telle quelle : l'oracle du
/// « au bit près ».
fn historique(sr: u32, amount: f32, delay_ms: f32, samples: &mut [f32]) {
    let d = retard_en_echantillons(sr, delay_ms);
    let (mut rl, mut rr, mut pos) = (vec![0.0_f32; d], vec![0.0_f32; d], 0);
    for f in 0..samples.len() / 2 {
        let (l, r) = (samples[2 * f], samples[2 * f + 1]);
        let (ld, rd) = if d == 0 {
            (l, r)
        } else {
            let v = (rl[pos], rr[pos]);
            rl[pos] = l;
            rr[pos] = r;
            pos = (pos + 1) % d;
            v
        };
        samples[2 * f] = (l + amount * (rd - ld)).clamp(-1.0, 1.0);
        samples[2 * f + 1] = (r + amount * (ld - rd)).clamp(-1.0, 1.0);
    }
}

/// #5081 — filtre éteint, le moteur rend l'implémentation d'avant au BIT près.
#[test]
fn filtre_eteint_la_sortie_est_celle_d_avant_au_bit_pres_5081() {
    let signal: Vec<f32> = (0..8192)
        .map(|i| ((i as f32 * 0.137).sin() * 0.6 + (i as f32 * 0.021).cos() * 0.3) * 0.9)
        .collect();
    for (sr, amount, delay) in [
        (44_100, 0.3_f32, 0.3_f32),
        (96_000, 0.5, 0.0),
        (48_000, 0.25, 1.0),
    ] {
        let mut attendu = signal.clone();
        historique(sr, amount, delay, &mut attendu);
        let mut obtenu = signal.clone();
        let mut cf = CrossfeedProcessor::avec_ombre(sr, amount, delay, None);
        for bloc in obtenu.chunks_mut(256) {
            cf.process_interleaved(bloc);
        }
        assert!(
            obtenu
                .iter()
                .map(|x| x.to_bits())
                .eq(attendu.iter().map(|x| x.to_bits())),
            "filtre d'ombre éteint : la sortie n'est plus celle d'avant au bit près \
             ({sr} Hz, a={amount}, d={delay} ms)"
        );
    }
}

/// Le Mid (L + R) reste conservé filtre allumé, et une source mono passe
/// au bit près : le filtre ne touche qu'au terme croisé.
#[test]
fn filtre_allume_le_mid_est_conserve_et_le_mono_intact_5081() {
    let ombre = Some(OmbreDeTete {
        cutoff_hz: 1200.0,
        slope_db_per_octave: 3.0,
    });
    let mut cf = CrossfeedProcessor::avec_ombre(48_000, 0.4, 0.3, ombre);
    let entree: Vec<f32> = (0..4096)
        .map(|i| (i as f32 * 0.31).sin() * if i % 2 == 0 { 0.5 } else { -0.3 })
        .collect();
    let mut sortie = entree.clone();
    cf.process_interleaved(&mut sortie);
    for (e, s) in entree
        .as_chunks::<2>()
        .0
        .iter()
        .zip(sortie.as_chunks::<2>().0)
    {
        assert!(((e[0] + e[1]) - (s[0] + s[1])).abs() < 1e-6, "Mid perdu");
    }
    let mono: Vec<f32> = (0..2048)
        .flat_map(|i| {
            let v = (i as f32 * 0.05).sin() * 0.7;
            [v, v]
        })
        .collect();
    let mut sortie = mono.clone();
    CrossfeedProcessor::avec_ombre(48_000, 0.4, 0.3, ombre).process_interleaved(&mut sortie);
    assert_eq!(sortie, mono);
}

/// Plus grand écart entre deux échantillons consécutifs du canal gauche,
/// sur `[debut, fin)` en trames.
fn plus_grand_pas(s: &[f32], debut: usize, fin: usize) -> f32 {
    let g: Vec<f32> = s.as_chunks::<2>().0.iter().map(|p| p[0]).collect();
    g[debut.max(1)..fin]
        .iter()
        .zip(&g[debut.max(1) - 1..fin - 1])
        .map(|(a, b)| (a - b).abs())
        .fold(0.0, f32::max)
}

/// #5081 — un changement de réglage en cours de lecture, le filtre en jeu,
/// ne fait AUCUN saut : le pas d'un échantillon au suivant autour du
/// changement ne dépasse pas celui du régime établi (marge de 25 %).
///
/// Sans fondu, le nouveau filtre part d'un état nul pendant que le terme
/// croisé valait déjà `a·(Rd − Ld)` : le canal gauche tombe d'un coup.
#[test]
fn un_changement_de_reglage_ne_fait_aucun_saut_5081() {
    let sr = 48_000_u32;
    let ombre = |fc: f32, pente: f32| {
        Some(OmbreDeTete {
            cutoff_hz: fc,
            slope_db_per_octave: pente,
        })
    };
    // (avant, après) : allumer, changer coupure et pente, éteindre.
    for (avant, apres) in [
        (None, ombre(700.0, 6.0)),
        (ombre(700.0, 6.0), ombre(200.0, 3.0)),
        (ombre(1200.0, 4.5), None),
    ] {
        let trames = sr as usize / 2;
        let bascule = trames / 2;
        // Un sinus de 100 Hz calé à droite : le terme croisé est tout le
        // canal gauche.
        let signal: Vec<f32> = (0..trames)
            .flat_map(|n| {
                let x = (2.0 * std::f64::consts::PI * 100.0 * n as f64 / f64::from(sr)).sin();
                [0.0, (0.5 * x) as f32]
            })
            .collect();
        let mut s = signal.clone();
        let mut cf = CrossfeedProcessor::avec_ombre(sr, 0.3, 0.3, avant);
        cf.process_interleaved(&mut s[..2 * bascule]);
        let mut suivant = CrossfeedProcessor::avec_ombre(sr, 0.3, 0.3, apres);
        suivant.inherit_state_from(&cf);
        for bloc in s[2 * bascule..].chunks_mut(128) {
            suivant.process_interleaved(bloc);
        }
        let fondu = (f64::from(sr) * DUREE_DU_FONDU_S) as usize;
        let regime = plus_grand_pas(&s, bascule - 4800, bascule).max(plus_grand_pas(
            &s,
            bascule + fondu + 2400,
            trames,
        ));
        let autour = plus_grand_pas(&s, bascule - 8, bascule + fondu + 8);
        eprintln!(
            "{avant:?} → {apres:?} : pas en régime {regime:.5}, autour du changement {autour:.5}"
        );
        assert!(
            autour <= 1.25 * regime,
            "saut au changement de réglage {avant:?} → {apres:?} : pas de {autour:.5} \
             contre {regime:.5} en régime établi"
        );
    }
}

/// #4685 × #5081 — la compensation de niveau tient compte du filtre : le
/// niveau moyen calculé retrouve le RMS MESURÉ au quart de dB près.
#[test]
fn le_gain_moyen_avec_ombre_retrouve_le_rms_mesure_5081() {
    use tune_plugin_audio_support::niveau_moyen::{multisinus_rose, rms_db};
    for (sr, amount, delay, fc, pente) in [
        (44_100_u32, 0.30_f32, 0.5_f32, 700.0_f32, 6.0_f32),
        (48_000, 0.40, 0.7, 1200.0, 3.0),
        (96_000, 0.50, 0.0, 200.0, 4.5),
    ] {
        let ombre = Some(OmbreDeTete {
            cutoff_hz: fc,
            slope_db_per_octave: pente,
        });
        let trames = sr as usize * 2;
        let mid = multisinus_rose(sr, trames, 0x4685);
        let side = multisinus_rose(sr, trames, 0x1234_5678);
        let k = (1.0_f64 / 3.0).sqrt();
        let entree: Vec<f32> = mid
            .iter()
            .zip(side.iter())
            .flat_map(|(m, s)| [(m + k * s) as f32, (m - k * s) as f32])
            .collect();
        let mut sortie = entree.clone();
        CrossfeedProcessor::avec_ombre(sr, amount, delay, ombre).process_interleaved(&mut sortie);
        let gauche = |v: &[f32]| -> Vec<f64> {
            v.as_chunks::<2>()
                .0
                .iter()
                .skip(trames / 4)
                .map(|p| f64::from(p[0]))
                .collect()
        };
        let mesure = rms_db(gauche(&sortie)) - rms_db(gauche(&entree));
        let calcule = gain_moyen_db_avec_ombre(sr, amount, delay, ombre);
        eprintln!(
            "ombre {fc} Hz / {pente} dB/oct, a={amount} : calculé {calcule:+.3} dB, mesuré {mesure:+.3} dB"
        );
        assert!(
            (calcule - mesure).abs() < 0.25,
            "calculé {calcule:.3} dB ≠ mesuré {mesure:.3} dB ({sr} Hz, a={amount}, fc {fc}, {pente} dB/oct)"
        );
    }
    // Éteint : exactement le calcul d'avant.
    assert_eq!(
        gain_moyen_db_avec_ombre(44_100, 0.3, 0.3, None),
        gain_moyen_db(44_100, 0.3, 0.3)
    );
}
