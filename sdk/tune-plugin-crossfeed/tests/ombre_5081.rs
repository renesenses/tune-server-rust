//! #5081 — l'ombre de la tête vue du greffon : réglages, bornes, défaut
//! éteint au bit près, et changement à chaud.
use serde_json::json;
use tune_plugin_crossfeed::{Crossfeed, CrossfeedProcessor, CrossfeedSettings, OmbreDeTete};
use tune_plugin_sdk::audio::*;

fn traiter(p: &mut dyn Processor, f: AudioFormat, samples: &mut [f32]) {
    p.process(
        &mut AudioBlock::new(f, SamplesMut::F32(samples), 4096).unwrap(),
        BlockContext {
            zone_id: 1,
            generation: 1,
            position_frames: 0,
        },
    )
    .unwrap();
}

fn signal() -> Vec<f32> {
    (0..4096)
        .map(|i| (i as f32 * 0.137).sin() * 0.4 + (i as f32 * 0.9).cos() * 0.2)
        .collect()
}

#[test]
fn les_defauts_sont_eteint_700_hz_6_db_5081() {
    let s = CrossfeedSettings::default();
    assert!(!s.head_shadow_enabled);
    assert_eq!((s.cutoff_hz, s.slope_db_per_octave), (700.0, 6.0));
}

/// Interrupteur éteint, coupure et pente réglées ailleurs que par défaut :
/// la sortie du greffon est celle du moteur SANS filtre, au bit près.
#[test]
fn interrupteur_eteint_la_sortie_est_celle_sans_filtre_au_bit_pres_5081() {
    let f = AudioFormat::new(48000, ChannelLayout::Stereo, SampleEncoding::F32).unwrap();
    let mut p = Crossfeed
        .prepare(
            f,
            4096,
            &json!({"enabled":true,"amount":0.3,"delay_ms":0.3,
                    "head_shadow_enabled":false,"cutoff_hz":1200.0,"slope_db_per_octave":3.0}),
        )
        .unwrap();
    let mut obtenu = signal();
    traiter(&mut *p, f, &mut obtenu);
    let mut attendu = signal();
    CrossfeedProcessor::new(48000, 0.3, 0.3).process_interleaved(&mut attendu);
    assert!(
        obtenu
            .iter()
            .map(|x| x.to_bits())
            .eq(attendu.iter().map(|x| x.to_bits())),
        "interrupteur « ombre de la tête » éteint, et pourtant la sortie n'est pas celle sans filtre"
    );
}

/// Allumé, le greffon applique le filtre du moteur, et le changement à chaud
/// passe par le même héritage que le moteur (fondu compris).
#[test]
fn allume_et_change_a_chaud_comme_le_moteur_5081() {
    let f = AudioFormat::new(48000, ChannelLayout::Stereo, SampleEncoding::F32).unwrap();
    let reglage = |on: bool, fc: f32, pente: f32| {
        json!({"enabled":true,"amount":0.3,"delay_ms":0.3,
               "head_shadow_enabled":on,"cutoff_hz":fc,"slope_db_per_octave":pente})
    };
    let mut p = Crossfeed
        .prepare(f, 4096, &reglage(true, 700.0, 6.0))
        .unwrap();
    let mut obtenu = signal();
    traiter(&mut *p, f, &mut obtenu[..1024]);
    p.update(&reglage(true, 1200.0, 3.0)).unwrap();
    for bloc in obtenu[1024..].chunks_mut(128) {
        traiter(&mut *p, f, bloc);
    }
    let ombre = |fc: f32, pente: f32| {
        Some(OmbreDeTete {
            cutoff_hz: fc,
            slope_db_per_octave: pente,
        })
    };
    let mut attendu = signal();
    let mut ancien = CrossfeedProcessor::avec_ombre(48000, 0.3, 0.3, ombre(700.0, 6.0));
    ancien.process_interleaved(&mut attendu[..1024]);
    let mut neuf = CrossfeedProcessor::avec_ombre(48000, 0.3, 0.3, ombre(1200.0, 3.0));
    neuf.inherit_state_from(&ancien);
    neuf.process_interleaved(&mut attendu[1024..]);
    assert_eq!(obtenu, attendu);
    let mut sans = signal();
    CrossfeedProcessor::new(48000, 0.3, 0.3).process_interleaved(&mut sans);
    assert_ne!(obtenu, sans, "le filtre n'a rien changé");
}

/// Éteindre le crossfeed alors que le filtre tournait : le fondu se joue
/// jusqu'au bout, la force nulle ne le coupe pas net.
#[test]
fn eteindre_le_crossfeed_filtre_se_fait_en_fondu_5081() {
    let f = AudioFormat::new(48000, ChannelLayout::Stereo, SampleEncoding::F32).unwrap();
    let mut p = Crossfeed
        .prepare(
            f,
            4096,
            &json!({"enabled":true,"amount":0.3,"delay_ms":0.3,"head_shadow_enabled":true}),
        )
        .unwrap();
    let mut s = signal();
    traiter(&mut *p, f, &mut s[..2048]);
    p.update(&json!({"enabled":false})).unwrap();
    let entree = s[2048..2176].to_vec();
    traiter(&mut *p, f, &mut s[2048..2176]);
    assert_ne!(&s[2048..2176], &entree[..], "coupé net, sans fondu");
}

#[test]
fn les_bornes_sont_refusees_5081() {
    let f = AudioFormat::new(48000, ChannelLayout::Stereo, SampleEncoding::F32).unwrap();
    for hors in [
        json!({"enabled":true,"head_shadow_enabled":true,"cutoff_hz":150.0}),
        json!({"enabled":true,"head_shadow_enabled":true,"cutoff_hz":25000.0}),
        json!({"enabled":true,"head_shadow_enabled":true,"slope_db_per_octave":2.0}),
        json!({"enabled":true,"head_shadow_enabled":true,"slope_db_per_octave":7.0}),
    ] {
        assert!(Crossfeed.prepare(f, 512, &hors).is_err(), "{hors} accepté");
    }
    for bornes in [
        json!({"enabled":true,"head_shadow_enabled":true,"cutoff_hz":200.0,"slope_db_per_octave":3.0}),
        json!({"enabled":true,"head_shadow_enabled":true,"cutoff_hz":20000.0,"slope_db_per_octave":6.0}),
    ] {
        assert!(
            Crossfeed.prepare(f, 512, &bornes).is_ok(),
            "{bornes} refusé"
        );
    }
}
