use serde_json::json;
use tune_plugin_crossfeed_pro::{CrossfeedPro, CrossfeedProSettings, Preset, presets};
use tune_plugin_sdk::audio::*;

const FS: u32 = 48_000;

fn contexte() -> BlockContext {
    BlockContext {
        zone_id: 1,
        generation: 1,
        position_frames: 0,
    }
}

fn traiter(p: &mut dyn Processor, f: AudioFormat, samples: &mut [f32]) {
    p.process(
        &mut AudioBlock::new(f, SamplesMut::F32(samples), 4096).unwrap(),
        contexte(),
    )
    .unwrap();
}

fn reglage(extra: serde_json::Value) -> serde_json::Value {
    let mut base = json!({"enabled": true, "amount": 0.2});
    base.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    base
}

/// Plus grand écart entre deux échantillons successifs du canal DROIT, sur
/// `[debut, fin)` trames.
fn pire_marche(s: &[f32], debut: usize, fin: usize) -> f32 {
    (debut.max(1)..fin)
        .map(|n| (s[2 * n + 1] - s[2 * (n - 1) + 1]).abs())
        .fold(0.0, f32::max)
}

/// `audio-live-update` : changer dosage, retard, ombre de la tête et
/// coupe-bas EN COURS DE LECTURE ne fait aucune marche. Le signal est un
/// sinus grave sur L seul : le canal droit ne porte que la voie croisée, dont
/// un changement brutal se verrait comme un saut d'un échantillon à l'autre.
/// Borne : la plus grande marche des régimes établis avant et après le
/// changement, à 10 % près.
#[test]
fn un_reglage_change_en_lecture_ne_fait_aucune_marche() {
    let f = AudioFormat::new(FS, ChannelLayout::Stereo, SampleEncoding::F32).unwrap();
    let changements = [
        json!({"amount": 0.6}),
        json!({"delay_ms": 0.9}),
        json!({"head_shadow": true, "head_shadow_hz": 200.0}),
        json!({"low_cut": true}),
    ];
    for changement in changements {
        let mut p = CrossfeedPro
            .prepare(f, 4096, &reglage(json!({"delay_ms": 0.0})))
            .unwrap();
        let n = FS as usize; // 1 s
        let mut s: Vec<f32> = (0..n)
            .flat_map(|i| {
                let v = 0.5 * (2.0 * std::f64::consts::PI * 60.0 * i as f64 / f64::from(FS)).sin();
                [v as f32, 0.0]
            })
            .collect();
        let bascule = n / 2 + 37; // pas sur un passage par zéro
        for bloc in s[..2 * bascule].chunks_mut(2 * 64) {
            traiter(&mut *p, f, bloc);
        }
        p.update(&reglage(changement.clone())).unwrap();
        for bloc in s[2 * bascule..].chunks_mut(2 * 64) {
            traiter(&mut *p, f, bloc);
        }
        let avant = pire_marche(&s, bascule - FS as usize / 10, bascule);
        let apres = pire_marche(&s, bascule + FS as usize / 10, n);
        let autour = pire_marche(&s, bascule - 10, bascule + FS as usize / 20);
        let borne = avant.max(apres) * 1.1;
        assert!(
            autour <= borne,
            "marche au changement {changement} : {autour:e}, régimes établis {avant:e} avant et {apres:e} après"
        );
    }
}

/// Le chemin entier du greffon rend une source mono intacte au bit près, en
/// 16, 24 et 32 bits, filtres et garde actifs.
#[test]
fn une_source_mono_traverse_le_greffon_au_bit_pres() {
    let r = reglage(json!({"amount": 0.6, "head_shadow": true, "low_cut": true}));
    let stereo = |mots: &[i32]| -> Vec<i32> { mots.iter().flat_map(|m| [*m, *m]).collect() };

    let entree: Vec<i16> = stereo(&[-32_768, -16_385, -1, 0, 1, 16_384, 32_767])
        .iter()
        .map(|m| *m as i16)
        .collect();
    let f = AudioFormat::new(44_100, ChannelLayout::Stereo, SampleEncoding::S16).unwrap();
    let mut p = CrossfeedPro.prepare(f, 64, &r).unwrap();
    let mut sortie = entree.clone();
    p.process(
        &mut AudioBlock::new(f, SamplesMut::S16(&mut sortie), 64).unwrap(),
        contexte(),
    )
    .unwrap();
    assert_eq!(sortie, entree, "16 bits");

    let entree: Vec<u8> = stereo(&[-8_388_608, -1, 0, 1, 8_388_607])
        .iter()
        .flat_map(|m| {
            let b = m.to_le_bytes();
            [b[0], b[1], b[2]]
        })
        .collect();
    let f = AudioFormat::new(96_000, ChannelLayout::Stereo, SampleEncoding::S24Le).unwrap();
    let mut p = CrossfeedPro.prepare(f, 64, &r).unwrap();
    let mut sortie = entree.clone();
    p.process(
        &mut AudioBlock::new(f, SamplesMut::S24Le(&mut sortie), 64).unwrap(),
        contexte(),
    )
    .unwrap();
    assert_eq!(sortie, entree, "24 bits");

    let entree = stereo(&[i32::MIN, -1, 0, 1, 8_388_607 << 8, i32::MAX]);
    let f = AudioFormat::new(192_000, ChannelLayout::Stereo, SampleEncoding::S32).unwrap();
    let mut p = CrossfeedPro.prepare(f, 64, &r).unwrap();
    let mut sortie = entree.clone();
    p.process(
        &mut AudioBlock::new(f, SamplesMut::S32(&mut sortie), 64).unwrap(),
        contexte(),
    )
    .unwrap();
    assert_eq!(sortie, entree, "32 bits");
}

#[test]
fn contournements_et_refus_du_multicanal() {
    let s = json!({"enabled": true});
    let mut ctx = PlaybackContext {
        zone_id: 1,
        source: SourceKind::Streaming,
        delivery: Delivery::NetworkFile,
        pure: true,
        protected_bitstream: false,
    };
    assert_eq!(
        CrossfeedPro.assess(&ctx, &s).unwrap(),
        Applicability::Bypass(BypassReason::Pure)
    );
    ctx.pure = false;
    ctx.protected_bitstream = true;
    assert_eq!(
        CrossfeedPro.assess(&ctx, &s).unwrap(),
        Applicability::Bypass(BypassReason::ProtectedBitstream)
    );
    ctx.protected_bitstream = false;
    assert_eq!(
        CrossfeedPro.assess(&ctx, &json!({})).unwrap(),
        Applicability::Bypass(BypassReason::Disabled)
    );
    let f = AudioFormat::new(48_000, ChannelLayout::Discrete(6), SampleEncoding::F32).unwrap();
    assert!(CrossfeedPro.prepare(f, 512, &s).is_err());
}

/// Les défauts de #5039, et les bornes des réglages.
#[test]
fn defauts_et_bornes() {
    let d = CrossfeedProSettings::default();
    assert!(!d.enabled);
    assert_eq!(d.amount, 0.30);
    assert_eq!(d.delay_ms, 0.3);
    assert!(!d.head_shadow, "ombre de la tête DÉSACTIVÉE par défaut");
    assert!(!d.low_cut, "coupe-bas désactivé par défaut");
    assert!(d.phase_guard, "garde de phase active par défaut");
    let f = AudioFormat::new(48_000, ChannelLayout::Stereo, SampleEncoding::F32).unwrap();
    for faux in [
        json!({"amount": 0.19}),
        json!({"amount": 0.61}),
        json!({"delay_ms": -0.1}),
        json!({"head_shadow_hz": 99.0}),
        json!({"head_shadow_hz": 10_001.0}),
        json!({"phase_guard_ms": 19.0}),
        json!({"phase_guard_ms": 51.0}),
    ] {
        assert!(
            CrossfeedPro.prepare(f, 64, &faux).is_err(),
            "accepté à tort : {faux}"
        );
    }
}

/// Un préréglage fixe dosage, retard et ombre de la tête ; l'utilisateur peut
/// ensuite retoucher, le greffon accepte le résultat.
#[test]
fn un_prereglage_se_pose_puis_se_retouche() {
    let mut s = CrossfeedProSettings::default();
    s.apply_preset(Preset::JanMeier);
    assert!(s.head_shadow);
    assert_eq!(s.head_shadow_hz, 650.0);
    assert_eq!(s.delay_ms, 0.0);
    s.head_shadow_hz = 800.0;
    s.enabled = true;
    let f = AudioFormat::new(48_000, ChannelLayout::Stereo, SampleEncoding::F32).unwrap();
    assert!(
        CrossfeedPro
            .prepare(f, 64, &serde_json::to_value(&s).unwrap())
            .is_ok()
    );
    let liste = presets();
    let ids: Vec<_> = liste
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["bs2b", "chu_moy", "jan_meier"]);
}
