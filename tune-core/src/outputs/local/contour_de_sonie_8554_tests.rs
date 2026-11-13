//! #8554 — le contour de loudness sur la sortie locale : installé sans
//! égaliseur, il relève le grave à bas volume, suit le volume, survit aux
//! changements d'égaliseur, ne touche rien quand il est éteint ou au niveau de
//! référence, et rien sous PURE.

use std::sync::atomic::Ordering;

use super::{
    ContourVoulu, LocalOutput, apply_local_dsp, local_dsp_is_identity, local_dsp_runtime_state,
};
use crate::audio::contour_de_sonie::ReglageContour;
use crate::audio::eq::{EqProcessor, EqProfile};
use crate::outputs::traits::{OutputDspState, OutputTarget};

const TAUX: u32 = 48_000;

fn voulu() -> Option<ContourVoulu> {
    Some(ContourVoulu {
        reglage: ReglageContour {
            enabled: true,
            ..Default::default()
        },
        sample_rate: TAUX,
        channels: 2,
    })
}

fn sinus(f: f64, trames: usize) -> Vec<f32> {
    (0..trames)
        .flat_map(|n| {
            let s = ((std::f64::consts::TAU * f * n as f64 / f64::from(TAUX)).sin() * 0.1) as f32;
            [s, s]
        })
        .collect()
}

fn rms_db(x: &[f32]) -> f64 {
    let rms = (x.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / x.len() as f64).sqrt();
    20.0 * (rms / (0.1 / 2f64.sqrt())).log10()
}

fn traiter(sortie: &LocalOutput, samples: &mut [f32]) {
    for bloc in samples.chunks_mut(4_800 * 2) {
        apply_local_dsp(
            bloc,
            &sortie.eq,
            &sortie.convolver,
            &sortie.crossfeed,
            &sortie.pure_bypass,
            &sortie.mono_downmix,
            2,
            false,
        );
    }
}

fn gain_cible_du_grave(sortie: &LocalOutput) -> f64 {
    sortie
        .eq
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|p| p.contour())
        .map(|c| c.consigne_cible().gains_db[0])
        .expect("contour installé")
}

#[tokio::test]
async fn sans_egaliseur_le_contour_releve_le_grave_a_bas_volume() {
    let sortie = LocalOutput::new("DAC test".to_string());
    sortie.set_volume(0.1).await.unwrap(); // −20 dB
    sortie.set_contour_de_sonie(voulu());
    assert!(
        !sortie.has_eq(),
        "le porteur du contour n'est pas un égaliseur"
    );
    let mut grave = sinus(50.0, TAUX as usize);
    traiter(&sortie, &mut grave);
    let g = rms_db(&grave[grave.len() / 2..]);
    assert!(g > 5.0, "50 Hz relevé de {g:.2} dB seulement à −20 dB");
    assert!(!local_dsp_is_identity(
        &sortie.eq,
        &sortie.convolver,
        &sortie.crossfeed,
        &sortie.pure_bypass,
        &sortie.mono_downmix
    ));
    assert_eq!(
        local_dsp_runtime_state(
            &sortie.eq,
            &sortie.convolver,
            &sortie.crossfeed,
            &sortie.pure_bypass,
            &sortie.mono_downmix,
            false
        ),
        OutputDspState::Applied,
        "le chemin du signal ne doit pas se dire intact"
    );
}

#[tokio::test]
async fn le_contour_suit_le_volume() {
    let sortie = LocalOutput::new("DAC test".to_string());
    sortie.set_volume(0.1).await.unwrap();
    sortie.set_contour_de_sonie(voulu());
    let a_moins_20 = gain_cible_du_grave(&sortie);
    sortie.set_volume(0.01).await.unwrap(); // −40 dB
    let a_moins_40 = gain_cible_du_grave(&sortie);
    assert!(a_moins_40 > a_moins_20 + 3.0, "{a_moins_20} → {a_moins_40}");
    sortie.set_volume(1.0).await.unwrap();
    assert!(
        gain_cible_du_grave(&sortie).abs() < 1e-9,
        "nul à la référence"
    );
}

#[tokio::test]
async fn aucun_changement_quand_desactive_ou_a_la_reference() {
    let entree = sinus(50.0, 9_600);
    let sortie = LocalOutput::new("DAC test".to_string());
    sortie.set_volume(0.1).await.unwrap();
    sortie.set_contour_de_sonie(None);
    assert!(sortie.eq.lock().unwrap().is_none(), "rien d'installé");
    let mut x = entree.clone();
    traiter(&sortie, &mut x);
    assert_eq!(x, entree);

    let reference = LocalOutput::new("DAC test".to_string());
    reference.set_volume(1.0).await.unwrap();
    reference.set_contour_de_sonie(voulu());
    let mut x = entree.clone();
    traiter(&reference, &mut x);
    assert_eq!(
        x, entree,
        "au niveau de référence, aucun échantillon ne change"
    );
}

#[tokio::test]
async fn inactif_sous_pure() {
    let sortie = LocalOutput::new("DAC test".to_string());
    sortie.set_volume(0.1).await.unwrap();
    sortie.set_contour_de_sonie(voulu());
    sortie.pure_bypass.store(true, Ordering::Relaxed);
    let entree = sinus(50.0, 9_600);
    let mut x = entree.clone();
    traiter(&sortie, &mut x);
    assert_eq!(x, entree, "PURE : chemin intouché");
    // Reposé sous PURE, le contour n'est même pas installé.
    let pure = LocalOutput::new("DAC test".to_string());
    pure.set_volume(0.1).await.unwrap();
    pure.pure_bypass.store(true, Ordering::Relaxed);
    pure.set_contour_de_sonie(voulu());
    assert!(pure.eq.lock().unwrap().is_none());
}

#[tokio::test]
async fn le_contour_survit_aux_changements_d_egaliseur() {
    let sortie = LocalOutput::new("DAC test".to_string());
    sortie.set_volume(0.1).await.unwrap();
    sortie.set_contour_de_sonie(voulu());
    let profil = EqProfile {
        enabled: true,
        bands: crate::audio::eq_presets::bandes("rock").unwrap(),
        ..Default::default()
    };
    sortie.replace_eq_live(Some(EqProcessor::new(&profil, TAUX, 2)));
    assert!(sortie.has_eq());
    assert!(
        sortie.consigne_du_contour().is_some(),
        "contour passé à l'égaliseur"
    );
    sortie.replace_eq_live(None);
    assert!(
        sortie.consigne_du_contour().is_some(),
        "contour gardé à la coupure de l'EQ"
    );
    sortie.set_eq(None);
    assert!(
        sortie.consigne_du_contour().is_some(),
        "contour gardé au début de piste"
    );
}

#[tokio::test]
async fn retire_il_redescend_puis_quitte_la_chaine() {
    let sortie = LocalOutput::new("DAC test".to_string());
    sortie.set_volume(0.1).await.unwrap();
    sortie.set_contour_de_sonie(voulu());
    sortie.set_contour_de_sonie(None);
    assert!(
        sortie.consigne_du_contour().is_some_and(|c| !c.est_nulle()),
        "pas de marche : le grave redescend en douceur"
    );
    let mut x = sinus(50.0, TAUX as usize * 2);
    traiter(&sortie, &mut x);
    assert!(
        sortie.eq.lock().unwrap().is_none(),
        "revenu à zéro, il quitte la chaîne"
    );
}
