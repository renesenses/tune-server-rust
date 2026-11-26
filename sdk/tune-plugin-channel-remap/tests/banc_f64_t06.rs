//! TESS T06 — banc de la réaffectation des canaux en `f64`.
//!
//! Aucun son. Référence calculée À PART : le produit matrice × trame écrit
//! ici, en double-double, depuis les coefficients compilés de la [`Matrice`]
//! (normalisation comprise). Épreuve : écart < 1e-15 ; contre-épreuve : le
//! chemin `f32` (`appliquer_f32`) en est à ~1e-8. Les recopies
//! (permutations, routes muettes) sont exactes au bit près.

use std::f64::consts::PI;

use tune_plugin_channel_remap::*;
use tune_plugin_sdk::audio::*;

const N: usize = 4_096;

#[derive(Clone, Copy, Default)]
struct Dd(f64, f64);
fn deux_sommes(a: f64, b: f64) -> Dd {
    let s = a + b;
    let bb = s - a;
    Dd(s, (a - (s - bb)) + (b - bb))
}
fn deux_produits(a: f64, b: f64) -> Dd {
    let p = a * b;
    Dd(p, a.mul_add(b, -p))
}
impl Dd {
    fn plus(self, o: Dd) -> Dd {
        let s = deux_sommes(self.0, o.0);
        let t = deux_sommes(self.1, o.1);
        let s = deux_sommes(s.0, s.1 + t.0);
        deux_sommes(s.0, s.1 + t.1)
    }
}

/// Réglages : les préréglages livrés, et une matrice libre EXTRÊME (gains
/// aux bornes −60 et +12 dB, sans normalisation).
fn reglages() -> Vec<(String, ChannelRemapSettings)> {
    let mut v: Vec<(String, ChannelRemapSettings)> = PREREGLAGES
        .iter()
        .map(|id| (id.to_string(), prereglage(id).unwrap()))
        .collect();
    v.push((
        "extrême 6→2".into(),
        ChannelRemapSettings {
            enabled: true,
            inputs: 6,
            outputs: 2,
            gains_db: vec![
                vec![Some(12.0), None, Some(-60.0), Some(-3.0), Some(-7.5), None],
                vec![None, Some(-0.1), Some(-60.0), Some(-3.0), None, Some(11.9)],
            ],
            normalize: false,
            preset: None,
        },
    ));
    v
}

/// Signal multicanal en double précision (non représentable en `f32`),
/// chaque canal distinct, crêtes faibles (aucune somme ne dépasse 1).
fn signal(canaux: usize) -> Vec<f64> {
    (0..N * canaux)
        .map(|k| {
            let (t, c) = (k / canaux, k % canaux);
            0.03 * (2.0 * PI * (110.0 + 37.0 * c as f64) * t as f64 / 48_000.0).sin()
                + 1e-9 * (c as f64 + 1.0)
        })
        .collect()
}

fn reference(m: &Matrice, x: &[f64]) -> Vec<f64> {
    let (n, s) = (usize::from(m.entrees()), usize::from(m.sorties()));
    let mut y = vec![0.0; x.len() / n * s];
    for (e, o) in x.chunks_exact(n).zip(y.chunks_exact_mut(s)) {
        for (j, v) in o.iter_mut().enumerate() {
            let mut acc = Dd::default();
            for (i, xi) in e.iter().enumerate() {
                acc = acc.plus(deux_produits(m.coefficient(j, i), *xi));
            }
            *v = acc.0 + acc.1;
        }
    }
    y
}

fn ecart_max(a: &[f64], b: &[f64]) -> f64 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0, f64::max)
}
fn db(x: f64) -> f64 {
    20.0 * x.max(1e-300).log10()
}

/// ÉPREUVE et CONTRE-ÉPREUVE, sur chaque réglage ; mesure de l'écart avec
/// l'ancien chemin `f32` (inaudible) et du bruit de calcul.
#[test]
fn soixante_quatre_bits_reels_et_inaudible() {
    for (nom, r) in reglages() {
        let m = Matrice::depuis_reglage(&r).unwrap();
        let (n, s) = (usize::from(m.entrees()), usize::from(m.sorties()));
        let x = signal(n);
        let attendu = reference(&m, &x);
        let mut y64 = vec![f64::NAN; N * s];
        assert_eq!(m.appliquer_f64_dans(&x, &mut y64), N);
        let x32: Vec<f32> = x.iter().map(|&v| v as f32).collect();
        let y32: Vec<f64> = m.appliquer_f32(&x32).into_iter().map(f64::from).collect();
        let (e64, e32) = (ecart_max(&y64, &attendu), ecart_max(&y32, &attendu));
        println!(
            "T06 réaffectation {nom:<18} {n}→{s} : écart f64 {:>7.1} dBFS, f32 {:>7.1} dBFS (f32↔f64 {:>7.1} dBFS)",
            db(e64),
            db(e32),
            db(ecart_max(&y64, &y32))
        );
        assert!(e64 < 1e-15, "{nom} : {e64:e}");
        assert!(e32 > 1e-12, "{nom} : le banc ne discrimine pas ({e32:e})");
        assert!(db(ecart_max(&y64, &y32)) < -120.0, "{nom}");
        if m.est_recopie() {
            // Une recopie est exacte au bit près.
            assert_eq!(e64, 0.0, "{nom} : recopie non exacte");
        }
        // Sur place (N → N) : même résultat, au bit près.
        if n == s {
            let mut t = x.clone();
            let mut trame = vec![0.0; n];
            assert!(m.appliquer_f64_sur_place(&mut t, &mut trame));
            assert_eq!(t, y64, "{nom} : sur place ≠ vers un tampon");
        }
    }
}

/// Une sortie trop petite borne le nombre de trames, sans paniquer ni
/// écrire au-delà.
#[test]
fn la_sortie_bornee_borne_les_trames() {
    let m = Matrice::depuis_reglage(&prereglage("quad_to_stereo").unwrap()).unwrap();
    let x = signal(4);
    let mut y = vec![0.0; 2 * 10 + 1];
    assert_eq!(m.appliquer_f64_dans(&x, &mut y), 10);
    assert_eq!(y[20], 0.0);
}

/// Le greffon SDK (N → N) accepte `F64` (manifeste `audio-f64`) et rend
/// exactement la matrice en `f64`.
#[test]
fn le_greffon_sdk_traite_le_f64_tel_quel() {
    let manifeste: serde_json::Value =
        serde_json::from_str(include_str!("../manifest.json")).unwrap();
    assert!(
        manifeste["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "audio-f64")
    );
    let r = prereglage("mono").unwrap();
    let m = Matrice::depuis_reglage(&r).unwrap();
    let f = AudioFormat::new(48_000, ChannelLayout::Stereo, SampleEncoding::F64).unwrap();
    let mut greffon = ChannelRemap
        .prepare(f, 512, &serde_json::to_value(&r).unwrap())
        .unwrap();
    let x = signal(2);
    let mut attendu = vec![0.0; x.len()];
    m.appliquer_f64_dans(&x, &mut attendu);
    let mut y = x.clone();
    for morceau in y.chunks_mut(2 * 512) {
        let trames = morceau.len() / 2;
        greffon
            .process(
                &mut AudioBlock::new(f, SamplesMut::F64(morceau), trames).unwrap(),
                BlockContext {
                    zone_id: 1,
                    generation: 1,
                    position_frames: 0,
                },
            )
            .unwrap();
    }
    assert_eq!(y, attendu);
}

/// Banc CPU (`--release -- --ignored --nocapture`) : 5.1 → stéréo ITU,
/// 96 kHz, 60 s d'audio, `f32` (avec son allocation de tampon) contre `f64`
/// (sans allocation).
#[test]
#[ignore = "banc CPU : cargo test --release -- --ignored --nocapture"]
fn banc_cpu_reaffectation_96_khz() {
    const SECONDES: usize = 60;
    const BLOC: usize = 1_024;
    let m = Matrice::depuis_reglage(&prereglage("5_1_to_stereo_itu").unwrap()).unwrap();
    let x: Vec<f64> = signal(6)[..6 * BLOC].to_vec();
    let x32: Vec<f32> = x.iter().map(|&v| v as f32).collect();
    let blocs = SECONDES * 96_000 / BLOC;
    let mut y = vec![0.0; 2 * BLOC];
    let mesurer = |f: &mut dyn FnMut()| {
        let mut meilleur = f64::MAX;
        for _ in 0..3 {
            let t = std::time::Instant::now();
            for _ in 0..blocs {
                f();
            }
            meilleur = meilleur.min(t.elapsed().as_secs_f64());
        }
        meilleur
    };
    let mut garde = 0.0_f32;
    let s32 = mesurer(&mut || garde += m.appliquer_f32(&x32)[0]);
    let s64 = mesurer(&mut || {
        m.appliquer_f64_dans(&x, &mut y);
    });
    println!(
        "T06 CPU réaffectation 5.1→2.0 96 kHz, {SECONDES} s : f32 {s32:.3} s ({:.3} % d'un cœur), f64 {s64:.3} s ({:.3} %), ratio {:.3} [{garde}]",
        100.0 * s32 / SECONDES as f64,
        100.0 * s64 / SECONDES as f64,
        s64 / s32
    );
}
