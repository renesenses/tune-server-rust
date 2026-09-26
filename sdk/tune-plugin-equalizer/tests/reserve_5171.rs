//! #5171 — la réserve « Sûre » (défaut) est celle d'avant, au bit près.
//!
//! Les empreintes ci-dessous ont été relevées sur le moteur d'AVANT #5171
//! (`origin/main` = 596f3a08, sur Shrek), avec ce même fichier : elles figent
//! ce que rendait l'égaliseur — pré-gain et échantillons, aux quatre formats
//! et à deux débits — pour la courbe de Thierry et deux préréglages. Un profil
//! sans le champ `headroom_mode`, ou avec `"safe"`, doit les rendre encore.

use tune_plugin_equalizer::{EqBandSpec, EqProcessor, EqProfile, HeadroomMode};

/// La courbe de Thierry (#5069, 25/09/2026) : graphique 31 bandes, Q = 4,32
/// (celui de la grille 31 bandes du client web), le reste à 0.
pub fn courbe_de_thierry() -> EqProfile {
    const GRILLE: [f64; 31] = [
        20.0, 25.0, 31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 315.0, 400.0,
        500.0, 630.0, 800.0, 1000.0, 1250.0, 1600.0, 2000.0, 2500.0, 3150.0, 4000.0, 5000.0,
        6300.0, 8000.0, 10000.0, 12500.0, 16000.0, 20000.0,
    ];
    let gain = |f: f64| match f as u32 {
        20 | 25 => 3.5,
        31 => 3.0,
        40 => 2.5,
        50 => 1.5,
        63 => 1.0,
        80 => 0.5,
        250 => -1.0,
        315 => -1.5,
        400 => -1.0,
        1000 | 1250 => 1.0,
        1600 | 2000 => 2.5,
        2500 => 1.0,
        4000 => -0.5,
        5000 => -1.5,
        8000 | 10000 | 12500 | 16000 => 1.5,
        _ => 0.0,
    };
    EqProfile {
        enabled: true,
        bands: GRILLE
            .iter()
            .map(|&freq| EqBandSpec {
                freq,
                gain: gain(freq),
                q: 4.32,
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

fn prereglage(bandes: &[(f64, f64)], q: f64) -> EqProfile {
    EqProfile {
        enabled: true,
        bands: bandes
            .iter()
            .map(|&(freq, gain)| EqBandSpec {
                freq,
                gain,
                q,
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

/// Les trois profils figés : Thierry, un « rock » dix bandes, un tilt 3 filtres.
fn profils() -> Vec<(&'static str, EqProfile)> {
    vec![
        ("thierry", courbe_de_thierry()),
        (
            "rock",
            prereglage(
                &[
                    (31.0, 5.0),
                    (62.0, 4.0),
                    (125.0, 3.0),
                    (250.0, 1.0),
                    (500.0, -1.0),
                    (1000.0, -1.0),
                    (2000.0, 1.0),
                    (4000.0, 3.0),
                    (8000.0, 4.0),
                    (16000.0, 5.0),
                ],
                1.41,
            ),
        ),
        (
            "tilt",
            EqProfile {
                enabled: true,
                bass_gain_db: 6.0,
                mid_gain_db: -2.0,
                treble_gain_db: 4.0,
                ..Default::default()
            },
        ),
    ]
}

fn fnv(octets: impl IntoIterator<Item = u8>) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    for o in octets {
        h ^= u64::from(o);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// Un signal déterministe stéréo : sinus graves et médiums, un carré pleine
/// échelle (le front qui fait sonner les cloches), un bruit congruentiel.
fn signal(trames: usize) -> Vec<f64> {
    let mut graine = 0x2545_f491_u32;
    let mut v = Vec::with_capacity(trames * 2);
    for i in 0..trames {
        let t = i as f64;
        let x = if i < trames / 3 {
            0.6 * (t * 0.0031).sin() + 0.35 * (t * 0.071).sin()
        } else if i < 2 * trames / 3 {
            if (i / 97) % 2 == 0 { 0.999 } else { -0.999 }
        } else {
            graine = graine.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (graine as f64 / u32::MAX as f64) * 1.8 - 0.9
        };
        v.push(x);
        v.push(-0.8 * x);
    }
    v
}

/// L'empreinte d'un profil : pré-gains et sorties aux quatre formats, à
/// 44,1 et 96 kHz, en deux blocs pour exercer l'état entre blocs.
pub fn empreinte(profil: &EqProfile) -> u64 {
    let mut tout: Vec<u8> = Vec::new();
    for sr in [44_100_u32, 96_000] {
        let entree = signal(6_000);
        let eq = EqProcessor::new(profil, sr, 2);
        for ch in 0..2 {
            tout.extend(eq.preamp_db(ch).unwrap().to_bits().to_le_bytes());
        }
        for profondeur in [16_u16, 24, 32] {
            let octets = usize::from(profondeur / 8);
            let echelle = (1_i64 << (profondeur - 1)) as f64;
            let mut pcm: Vec<u8> = entree
                .iter()
                .flat_map(|&x| {
                    let v = (x * echelle).round().clamp(-echelle, echelle - 1.0) as i32;
                    v.to_le_bytes()[..octets].to_vec()
                })
                .collect();
            let mut p = EqProcessor::new(profil, sr, 2);
            let milieu = pcm.len() / 2 / (octets * 2) * (octets * 2);
            let (a, b) = pcm.split_at_mut(milieu);
            p.process_pcm(a, profondeur);
            p.process_pcm(b, profondeur);
            tout.extend(pcm);
        }
        let mut f: Vec<f32> = entree.iter().map(|&x| x as f32).collect();
        let mut p = EqProcessor::new(profil, sr, 2);
        let (a, b) = f.split_at_mut(3_000 * 2);
        p.process_interleaved(a);
        p.process_interleaved(b);
        tout.extend(f.iter().flat_map(|x| x.to_bits().to_le_bytes()));
    }
    fnv(tout)
}

/// Relevées sur le moteur d'avant #5171 (voir l'en-tête).
const EMPREINTES_AVANT_5171: [(&str, u64); 3] = [
    ("thierry", 0x66ea_dc67_81c7_a48d),
    ("rock", 0xa2e0_22bb_675d_0c93),
    ("tilt", 0x5382_56e0_34fe_6ca8),
];

#[test]
fn la_reserve_sure_rend_les_memes_octets_qu_avant_5171() {
    for ((nom, profil), (nom_fige, fige)) in profils().into_iter().zip(EMPREINTES_AVANT_5171) {
        assert_eq!(nom, nom_fige);
        // Le profil tel qu'un client d'avant l'enregistrait, sans le champ.
        let json = serde_json::to_value(&profil).unwrap();
        assert!(
            json.get("headroom_mode").is_none(),
            "un profil « Sûr » doit s'écrire comme avant, sans `headroom_mode` : {json}"
        );
        let relu: EqProfile = serde_json::from_value(json.clone()).unwrap();
        let mut explicite = json;
        explicite["headroom_mode"] = "safe".into();
        let explicite: EqProfile = serde_json::from_value(explicite).unwrap();
        for (etiquette, p) in [("sans le champ", relu), ("\"safe\"", explicite)] {
            let e = empreinte(&p);
            println!("{nom} ({etiquette}) : {e:#018x}");
            assert_eq!(
                e, fige,
                "{nom} ({etiquette}) : la réserve Sûre ne rend plus les octets d'avant #5171 \
                 ({e:#018x} au lieu de {fige:#018x})"
            );
        }
    }
}

#[test]
fn une_valeur_inconnue_retombe_sur_la_reserve_sure() {
    let mut json = serde_json::to_value(courbe_de_thierry()).unwrap();
    json["headroom_mode"] = "turbo".into();
    let p: EqProfile = serde_json::from_value(json).expect("le profil reste lisible");
    assert_eq!(p.headroom_mode, HeadroomMode::Safe);
    assert!(p.enabled && p.bands.len() == 31);
}
