//! Revue de #9561 (défaut 2) — la sortie locale qui JOUE suit le changement
//! de headroom de la chaîne (Auto ↔ manuel) : la marge du filtre est reprise
//! sans attendre une nouvelle réponse impulsionnelle. Mesures sur la chaîne
//! locale réelle (`apply_local_dsp`).
//!
//! - manuel → Auto : le filtre reçoit sa marge, plus de saturation ;
//! - Auto → manuel : la marge retirée remonte en ~0,5 s (jamais plus de
//!   1,5 dB par 10 ms), jamais au-dessus du niveau d'avant le filtre.

use super::*;
use crate::audio::convolver::ConvolverConfig;
use crate::tess::chaine::Headroom;

const TAUX: u32 = 48_000;
/// Fenêtre de mesure : 10 ms.
const FENETRE: usize = (TAUX / 100) as usize;

/// Un filtre plat de +6,02 dB (Dirac de 2), en WAV.
fn ri_plus_6() -> crate::test_scratch::ScratchFile {
    let f = crate::test_scratch::scratch_file("ri-9561-marge-locale", ".wav");
    let taps = 256usize;
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
        let v: f32 = if i == 0 { 2.0 } else { 0.0 };
        w.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(f.path(), w).unwrap();
    f
}

/// Le filtre en Auto : marge −(6,02 + 0,5) dB dans le convolueur.
fn auto(ri: &crate::test_scratch::ScratchFile) -> ConvolverConfig {
    ConvolverConfig::from_wav(&ri.as_str())
        .unwrap()
        .avec_headroom(Headroom::Auto {})
}

/// Le filtre en manuel, M portée par l'étage des égaliseurs (gain fixe) :
/// 0 dB dans le convolueur.
fn manuel(ri: &crate::test_scratch::ScratchFile) -> ConvolverConfig {
    ConvolverConfig::from_wav(&ri.as_str())
        .unwrap()
        .avec_headroom(Headroom::Manual { db: -8.0 })
        .avec_egaliseur_amont(Some(crate::tess::egaliseurs_en_serie::gain_fixe(-8.0)))
}

fn sinus(depuis: usize, trames: usize, a: f32) -> Vec<f32> {
    (depuis..depuis + trames)
        .flat_map(|n| {
            let v = (2.0 * std::f32::consts::PI * 997.0 * n as f32 / TAUX as f32).sin() * a;
            [v, v]
        })
        .collect()
}

/// La sortie qui joue du 48 kHz stéréo, filtre `config` en service.
fn sortie(config: ConvolverConfig) -> LocalOutput {
    let s = LocalOutput::new("DAC test 9561".to_string());
    s.declare_current_format_for_test(TAUX, 2);
    s.set_convolution_autorisee(true);
    s.set_convolver_config(config).unwrap();
    rebuild_local_convolver(
        &s.convolver_config,
        &s.convolver,
        &s.convolution_autorisee,
        TAUX,
        2,
    )
    .unwrap();
    s
}

/// Joue `secondes` de sinus d'amplitude `a` ; rend les crêtes par fenêtre.
fn jouer(s: &LocalOutput, position: &mut usize, secondes: f64, a: f32) -> Vec<f64> {
    let trames = (f64::from(TAUX) * secondes) as usize;
    let mut cretes = Vec::new();
    for _ in 0..trames / FENETRE {
        let mut pcm = sinus(*position, FENETRE, a);
        *position += FENETRE;
        apply_local_dsp(
            &mut pcm,
            &s.eq,
            &s.convolver,
            &s.crossfeed,
            &s.pure_bypass,
            &s.mono_downmix,
            2,
            false,
        );
        cretes.push(pcm.iter().fold(0.0f64, |m, x| m.max(f64::from(x.abs()))));
    }
    cretes
}

fn db(x: f64) -> f64 {
    20.0 * x.log10()
}

/// Manuel → Auto, flux en cours : la marge du filtre est reprise tout de
/// suite. Avant le correctif, la route ne reconstruisait pas le convolueur :
/// le filtre restait sans marge et un sinus à −1 dBFS sortait à +5 dBFS.
#[test]
fn manuel_vers_auto_le_filtre_reprend_sa_marge_9561() {
    let ri = ri_plus_6();
    let a = 10f32.powf(-1.0 / 20.0);
    let s = sortie(manuel(&ri));
    let mut n = 0;
    let avant = jouer(&s, &mut n, 0.3, a);
    assert!(
        db(*avant.last().unwrap()) > 4.9,
        "manuel : filtre sans marge au convolueur ({avant:?})"
    );
    assert!(
        s.reposer_la_convolution_si_la_marge_change(auto(&ri))
            .unwrap()
    );
    let apres = jouer(&s, &mut n, 0.5, a);
    // Fondu de bascule (réchauffe ≤ 1 280 trames + 20 ms) : régime établi
    // dès 60 ms.
    for c in &apres[6..] {
        assert!(*c <= 1.0, "saturation : {:.2} dBFS ({apres:?})", db(*c));
        assert!((db(*c) - (-1.0 - 0.5)).abs() < 0.05, "{apres:?}");
    }
    // Même marge : la sortie garde son moteur (pas de FFT ni de bascule).
    assert!(
        !s.reposer_la_convolution_si_la_marge_change(auto(&ri))
            .unwrap()
    );
}

/// Auto → manuel, flux en cours : la marge de −6,5 dB est retirée du filtre
/// (l'étage des égaliseurs pose M) ; la hausse prend ~0,5 s, sans marche, et
/// s'arrête au niveau du filtre sans marge. Une borne de volume (l'auditeur
/// a monté le son de 6 dB depuis la pose du filtre) l'arrête au niveau
/// d'avant le filtre.
#[test]
fn auto_vers_manuel_la_hausse_prend_un_demi_seconde_bornee_9561() {
    let ri = ri_plus_6();
    let a = 0.25f32;
    for (volume, cible_db) in [(None, 20.0 * 2f64.log10()), (Some(2.0), 0.0)] {
        let s = sortie(auto(&ri));
        let mut n = 0;
        let avant = jouer(&s, &mut n, 0.3, a);
        let niveau_avant = *avant.last().unwrap();
        assert!((db(niveau_avant / f64::from(a)) - -0.5).abs() < 0.05);
        if let Some(rapport) = volume {
            // Le repère de #6741 : volume retenu à la pose, puis remonté.
            s.volume_avant_convolution.store(1000, Ordering::SeqCst);
            s.user_volume
                .store((1000.0 * rapport) as u32, Ordering::SeqCst);
        }
        assert!(
            s.reposer_la_convolution_si_la_marge_change(manuel(&ri))
                .unwrap()
        );
        let apres = jouer(&s, &mut n, 1.0, a);
        let mut precedent = niveau_avant;
        for c in &apres {
            assert!(
                db(*c / precedent) < 1.5,
                "marche de {:.2} dB ({apres:?})",
                db(*c / precedent)
            );
            assert!(
                db(*c / f64::from(a)) <= cible_db + 0.05,
                "au-dessus du niveau d'avant ({apres:?})"
            );
            precedent = *c;
        }
        // À 100 ms, encore loin (sans borne : 6,5 dB à remonter) ; à 700 ms
        // (réchauffe comprise), arrivé.
        if volume.is_none() {
            assert!(db(apres[10] / f64::from(a)) < cible_db - 1.0, "{apres:?}");
        }
        assert!(
            (db(apres[70] / f64::from(a)) - cible_db).abs() < 0.05,
            "{apres:?}"
        );
    }
}
