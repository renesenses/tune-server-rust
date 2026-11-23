//! TESS T06 — banc sonore du crossfeed v1 (gratuit) en `f64`.
//!
//! Aucun son : tout est calculé en mémoire.
//!
//! - **Épreuve « 64 bits réels »**, deux références calculées À PART :
//!   - sans ombre de la tête : la formule du crossfeed (`L + a·(Rd − Ld)`,
//!     `R + a·(Ld − Rd)`, retard de `d` trames), écrite ici et calculée en
//!     double-double (exacte à ~1e-30) ;
//!   - avec ombre (filtre sur le terme croisé) : la convolution directe,
//!     somme compensée, du signal par les quatre réponses impulsionnelles
//!     du système stéréo (gauche → gauche, gauche → droite, …). Récurrence
//!     et convolution ne s'accordent à 1e-12 que sans arrondi `f32`.
//! - **Contre-épreuve** : le chemin `f32` d'avant échoue aux mêmes bancs.
//! - **Inaudible** : ancien chemin `f32` et chemin `f64` à moins de
//!   −120 dBFS ; bruit de calcul du `f64` ≥ 60 dB sous celui du `f32`.
//! - **Réglage changé en lecture** (fondu de 10 ms de #5081) : même
//!   transition en `f64` qu'en `f32`, et pas de pointe au-delà du signal établi.

use std::f64::consts::PI;

use tune_plugin_crossfeed::{Crossfeed, CrossfeedProcessor, OmbreDeTete};
use tune_plugin_sdk::audio::*;

const SR: u32 = 48_000;
const N: usize = 4_000;

struct Alea(u64);
impl Alea {
    fn suivant(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }
}

/// Réglages : ordinaire, extrêmes (force maximale 0,5, retard nul et retard
/// maximal de 5 ms), avec et sans ombre (coupure et pente aux bornes).
fn reglages() -> Vec<(&'static str, f32, f32, Option<OmbreDeTete>)> {
    let ombre = |c: f32, p: f32| {
        Some(OmbreDeTete {
            cutoff_hz: c,
            slope_db_per_octave: p,
        })
    };
    vec![
        ("défaut 0,3/0,3 ms", 0.3, 0.3, None),
        ("extrême 0,5/0 ms", 0.5, 0.0, None),
        ("extrême 0,5/5 ms", 0.5, 5.0, None),
        ("ombre défaut", 0.3, 0.3, Some(OmbreDeTete::default())),
        ("ombre 200 Hz/6 dB", 0.5, 5.0, ombre(200.0, 6.0)),
        ("ombre 20 kHz/3 dB", 0.5, 0.0, ombre(20_000.0, 3.0)),
    ]
}

/// Stéréo entrelacée, `N` trames, L ≠ R, en double précision. Crêtes ≤ 0,45 :
/// le garde-fou à ±1 du crossfeed n'entre jamais en jeu (le système reste
/// linéaire, la référence est exacte).
fn signaux() -> Vec<(&'static str, Vec<f64>)> {
    let sinus = |f: f64| -> Vec<f64> {
        (0..N)
            .flat_map(|i| {
                let t = i as f64 / f64::from(SR);
                [
                    0.3 * (2.0 * PI * f * t).sin(),
                    0.15 * (2.0 * PI * f * 1.01 * t + 1.0).sin(),
                ]
            })
            .collect()
    };
    let mut alea = Alea(0x2545_f491_4f6c_dd1d);
    let bruit: Vec<f64> = (0..2 * N).map(|_| 0.22 * alea.suivant()).collect();
    let mut impulsion = vec![0.0; 2 * N];
    impulsion[2 * 11] = 0.4;
    impulsion[2 * 503 + 1] = -0.35;
    vec![
        ("sinus 1 kHz", sinus(1_000.0)),
        ("sinus 60 Hz", sinus(60.0)),
        ("sinus 15 kHz", sinus(15_000.0)),
        ("bruit blanc", bruit),
        ("impulsion", impulsion),
    ]
}

fn moteur(r: &(&str, f32, f32, Option<OmbreDeTete>)) -> CrossfeedProcessor {
    CrossfeedProcessor::avec_ombre(SR, r.1, r.2, r.3)
}

fn par_f64(r: &(&str, f32, f32, Option<OmbreDeTete>), x: &[f64], bloc: usize) -> Vec<f64> {
    let mut m = moteur(r);
    let mut y = x.to_vec();
    for morceau in y.chunks_mut(2 * bloc) {
        m.process_interleaved_f64(morceau);
    }
    y
}

fn par_f32(r: &(&str, f32, f32, Option<OmbreDeTete>), x: &[f64], bloc: usize) -> Vec<f64> {
    let mut m = moteur(r);
    let mut y: Vec<f32> = x.iter().map(|&v| v as f32).collect();
    for morceau in y.chunks_mut(2 * bloc) {
        m.process_interleaved(morceau);
    }
    y.into_iter().map(f64::from).collect()
}

// --- Référence 1 : la formule, en double-double --------------------------

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
    fn fois(self, c: f64) -> Dd {
        let p = deux_produits(self.0, c);
        deux_sommes(p.0, p.1 + self.1 * c)
    }
}

fn reference_formule(amount: f32, delay_ms: f32, x: &[f64]) -> Vec<f64> {
    let a = f64::from(amount);
    // Le retard du moteur : round(d / 1000 · sr), calculé comme lui en f32.
    let d = ((delay_ms.clamp(0.0, 5.0) / 1000.0) * SR as f32).round() as usize;
    let trames = x.len() / 2;
    let mut y = vec![0.0; x.len()];
    for n in 0..trames {
        let (l, r) = (x[2 * n], x[2 * n + 1]);
        let (ld, rd) = if n >= d {
            (x[2 * (n - d)], x[2 * (n - d) + 1])
        } else {
            (0.0, 0.0)
        };
        let diff = deux_sommes(rd, -ld);
        let c = diff.fois(a);
        y[2 * n] = Dd(l, 0.0).plus(c).0 + Dd(l, 0.0).plus(c).1;
        let mc = Dd(-c.0, -c.1);
        y[2 * n + 1] = Dd(r, 0.0).plus(mc).0 + Dd(r, 0.0).plus(mc).1;
    }
    y
}

// --- Référence 2 : convolution par les réponses impulsionnelles ----------

fn reponses(r: &(&str, f32, f32, Option<OmbreDeTete>), n: usize) -> [[Vec<f64>; 2]; 2] {
    // h[entrée][sortie]
    let rep = |entree: usize| {
        let mut x = vec![0.0; 2 * n];
        x[entree] = 1.0;
        // Impulsion unité : le garde-fou à ±1 ne mord pas (|L + c| ≤ 1 ici,
        // force ≤ 0,5) ; vérifié par l'assertion ci-dessous.
        moteur(r).process_interleaved_f64(&mut x);
        let g: Vec<f64> = x.iter().step_by(2).copied().collect();
        let d: Vec<f64> = x.iter().skip(1).step_by(2).copied().collect();
        assert!(g.iter().chain(&d).all(|v| v.abs() < 1.0 || *v == 1.0));
        [g, d]
    };
    [rep(0), rep(1)]
}

fn convoluer(h: &[f64], x: &[f64]) -> Vec<f64> {
    (0..x.len())
        .map(|n| {
            let (mut s, mut c) = (0.0_f64, 0.0_f64);
            for k in 0..=n {
                let t = h[k] * x[n - k];
                let v = s + t;
                c += if s.abs() >= t.abs() {
                    (s - v) + t
                } else {
                    (t - v) + s
                };
                s = v;
            }
            s + c
        })
        .collect()
}

fn reference_convolution(r: &(&str, f32, f32, Option<OmbreDeTete>), x: &[f64]) -> Vec<f64> {
    let trames = x.len() / 2;
    let h = reponses(r, trames);
    let xs = [
        x.iter().step_by(2).copied().collect::<Vec<_>>(),
        x.iter().skip(1).step_by(2).copied().collect::<Vec<_>>(),
    ];
    let mut y = vec![0.0; x.len()];
    for sortie in 0..2 {
        let a = convoluer(&h[0][sortie], &xs[0]);
        let b = convoluer(&h[1][sortie], &xs[1]);
        for n in 0..trames {
            y[2 * n + sortie] = a[n] + b[n];
        }
    }
    y
}

fn reference(r: &(&str, f32, f32, Option<OmbreDeTete>), x: &[f64]) -> Vec<f64> {
    match r.3 {
        None => reference_formule(r.1, r.2, x),
        Some(_) => reference_convolution(r, x),
    }
}

fn ecart_max(a: &[f64], b: &[f64]) -> f64 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0, f64::max)
}
fn ecart_rms(a: &[f64], b: &[f64]) -> f64 {
    (a.iter().zip(b).map(|(x, y)| (x - y).powi(2)).sum::<f64>() / a.len() as f64).sqrt()
}
fn db(x: f64) -> f64 {
    20.0 * x.max(1e-300).log10()
}

/// ÉPREUVE et CONTRE-ÉPREUVE « 64 bits réels ».
#[test]
fn soixante_quatre_bits_reels_contre_la_reference_hors_ligne() {
    for r in reglages() {
        for (nom_s, x) in signaux() {
            let reference = reference(&r, &x);
            let y64 = par_f64(&r, &x, 333);
            let y32 = par_f32(&r, &x, 333);
            let (e64, e32) = (ecart_max(&y64, &reference), ecart_max(&y32, &reference));
            println!(
                "T06 crossfeed {:<20} {nom_s:<12} écart f64 {:>7.1} dBFS, f32 {:>7.1} dBFS",
                r.0,
                db(e64),
                db(e32)
            );
            assert!(e64 < 1e-12, "{} / {nom_s} : écart f64 {e64:e}", r.0);
            assert!(
                e32 > 1e-10,
                "{} / {nom_s} : le banc ne discrimine pas ({e32:e})",
                r.0
            );
        }
    }
}

/// INAUDIBLE, et le bruit de calcul baisse.
#[test]
fn l_ecart_avec_l_ancien_chemin_est_inaudible_et_le_bruit_baisse() {
    for r in reglages() {
        for (nom_s, x) in signaux() {
            let reference = reference(&r, &x);
            let y64 = par_f64(&r, &x, 1_024);
            let y32 = par_f32(&r, &x, 1_024);
            let ecart = ecart_max(&y64, &y32);
            let (b64, b32) = (ecart_rms(&y64, &reference), ecart_rms(&y32, &reference));
            println!(
                "T06 crossfeed {:<20} {nom_s:<12} f32↔f64 max {:>7.1} dBFS ; bruit f32 {:>7.1} dBFS, f64 {:>7.1} dBFS",
                r.0,
                db(ecart),
                db(b32),
                db(b64)
            );
            assert!(db(ecart) < -120.0, "{} / {nom_s} : {}", r.0, db(ecart));
            assert!(db(b64) < db(b32) - 60.0, "{} / {nom_s}", r.0);
        }
    }
}

/// La boucle `f32` d'AVANT T06, recopiée telle quelle (sans ombre) :
/// l'oracle du bit près.
fn ancienne_boucle_f32(
    amount: f32,
    delay_ms: f32,
    samples: &mut [f32],
    etat: &mut (Vec<f32>, Vec<f32>, usize),
) {
    let d = ((delay_ms.clamp(0.0, 5.0) / 1000.0) * SR as f32).round() as usize;
    if etat.0.len() != d {
        *etat = (vec![0.0; d], vec![0.0; d], 0);
    }
    if amount == 0.0 {
        return;
    }
    for f in 0..samples.len() / 2 {
        let (l, r) = (samples[2 * f], samples[2 * f + 1]);
        let (ld, rd) = if d == 0 {
            (l, r)
        } else {
            let v = (etat.0[etat.2], etat.1[etat.2]);
            etat.0[etat.2] = l;
            etat.1[etat.2] = r;
            etat.2 += 1;
            if etat.2 >= d {
                etat.2 = 0;
            }
            v
        };
        samples[2 * f] = (l + amount * (rd - ld)).clamp(-1.0, 1.0);
        samples[2 * f + 1] = (r + amount * (ld - rd)).clamp(-1.0, 1.0);
    }
}

/// Le chemin `f32` reste celui d'avant AU BIT PRÈS (boucle partagée, lignes
/// à retard en `f64` qui rendent un `f32` exact), signaux forts compris (le
/// garde-fou à ±1 mord).
#[test]
fn le_chemin_f32_reste_au_bit_pres() {
    for r in reglages().into_iter().filter(|r| r.3.is_none()) {
        for (_, x) in signaux() {
            let x: Vec<f32> = x.iter().map(|&v| (v * 3.0) as f32).collect();
            let mut a = x.clone();
            let mut m = moteur(&r);
            let mut b = x.clone();
            let mut etat = (Vec::new(), Vec::new(), 0);
            for (ma, mb) in a.chunks_mut(2 * 500).zip(b.chunks_mut(2 * 500)) {
                m.process_interleaved(ma);
                ancienne_boucle_f32(r.1, r.2, mb, &mut etat);
            }
            assert_eq!(
                a.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                b.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                "{}",
                r.0
            );
        }
    }
}

/// RÉGLAGE CHANGÉ EN LECTURE : force 0,3 → 0,45 et ombre allumée en plein
/// bloc (fondu de 10 ms, #5081). La transition `f64` est celle du `f32` sous
/// −120 dBFS, et sa différence seconde ne dépasse pas celle du signal établi.
#[test]
fn un_reglage_change_en_lecture_ne_claque_pas_en_f64() {
    let x: Vec<f64> = (0..2 * N)
        .map(|i| {
            let t = (i / 2) as f64 / f64::from(SR);
            if i % 2 == 0 {
                0.3 * (2.0 * PI * 440.0 * t).sin()
            } else {
                0.3 * (2.0 * PI * 445.0 * t + 0.5).sin()
            }
        })
        .collect();
    let coupe = 2 * 2_001;
    let neuf = || CrossfeedProcessor::avec_ombre(SR, 0.45, 0.3, Some(OmbreDeTete::default()));
    let mut a = CrossfeedProcessor::new(SR, 0.3, 0.3);
    let mut y64 = x.clone();
    a.process_interleaved_f64(&mut y64[..coupe]);
    let mut b = neuf();
    b.inherit_state_from(&a);
    b.process_interleaved_f64(&mut y64[coupe..]);

    let mut a = CrossfeedProcessor::new(SR, 0.3, 0.3);
    let mut y32: Vec<f32> = x.iter().map(|&v| v as f32).collect();
    a.process_interleaved(&mut y32[..coupe]);
    let mut b = neuf();
    b.inherit_state_from(&a);
    b.process_interleaved(&mut y32[coupe..]);
    let y32: Vec<f64> = y32.into_iter().map(f64::from).collect();

    let ecart = ecart_max(&y64, &y32);
    let d2 = |y: &[f64], de: usize, a: usize| {
        (de.max(2)..a)
            .map(|n| (y[2 * n] - 2.0 * y[2 * (n - 1)] + y[2 * (n - 2)]).abs())
            .fold(0.0, f64::max)
    };
    let etabli = d2(&y64, 500, 1_990);
    let transition = d2(&y64, 1_995, 2_600);
    let apres = d2(&y64, 2_700, N);
    println!(
        "T06 crossfeed réglage en vol : d² avant {etabli:.3e}, fondu {transition:.3e}, après {apres:.3e} ; f32↔f64 {:.1} dBFS",
        db(ecart)
    );
    assert!(db(ecart) < -120.0);
    assert!(
        transition <= etabli.max(apres) * 1.05,
        "pointe au changement : {transition:e}"
    );
}

/// Le greffon SDK accepte `F64` (manifeste `audio-f64`) et rend exactement le
/// moteur `f64`.
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
    let f = AudioFormat::new(SR, ChannelLayout::Stereo, SampleEncoding::F64).unwrap();
    let reglages = serde_json::json!({"enabled": true, "amount": 0.3, "delay_ms": 0.3, "head_shadow_enabled": true});
    let mut greffon = Crossfeed.prepare(f, 512, &reglages).unwrap();
    let (_, x) = signaux().remove(3);
    let mut attendu = x.clone();
    CrossfeedProcessor::avec_ombre(SR, 0.3, 0.3, Some(OmbreDeTete::default()))
        .process_interleaved_f64(&mut attendu);
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

/// Banc CPU (`--release -- --ignored --nocapture`) : crossfeed avec ombre,
/// 96 kHz stéréo, 60 s d'audio, `f32` contre `f64`.
#[test]
#[ignore = "banc CPU : cargo test --release -- --ignored --nocapture"]
fn banc_cpu_crossfeed_96_khz() {
    const SR96: u32 = 96_000;
    const SECONDES: usize = 60;
    const BLOC: usize = 1_024;
    let mut alea = Alea(11);
    let x: Vec<f64> = (0..2 * BLOC * 64).map(|_| 0.3 * alea.suivant()).collect();
    let blocs = SECONDES * SR96 as usize / BLOC;
    for (nom, ombre) in [
        ("sans ombre", None),
        ("avec ombre", Some(OmbreDeTete::default())),
    ] {
        let mesurer = |f: &mut dyn FnMut(usize)| {
            let mut meilleur = f64::MAX;
            for _ in 0..3 {
                let t = std::time::Instant::now();
                for b in 0..blocs {
                    f(b);
                }
                meilleur = meilleur.min(t.elapsed().as_secs_f64());
            }
            meilleur
        };
        let mut m32 = CrossfeedProcessor::avec_ombre(SR96, 0.3, 0.3, ombre);
        let mut t32: Vec<f32> = x.iter().map(|&v| v as f32).collect();
        let s32 = mesurer(&mut |b| {
            let i = (b % 64) * 2 * BLOC;
            m32.process_interleaved(&mut t32[i..i + 2 * BLOC]);
        });
        let mut m64 = CrossfeedProcessor::avec_ombre(SR96, 0.3, 0.3, ombre);
        let mut t64 = x.clone();
        let s64 = mesurer(&mut |b| {
            let i = (b % 64) * 2 * BLOC;
            m64.process_interleaved_f64(&mut t64[i..i + 2 * BLOC]);
        });
        println!(
            "T06 CPU crossfeed {nom} 96 kHz stéréo, {SECONDES} s : f32 {s32:.3} s ({:.3} % d'un cœur), f64 {s64:.3} s ({:.3} %), ratio {:.3}",
            100.0 * s32 / SECONDES as f64,
            100.0 * s64 / SECONDES as f64,
            s64 / s32
        );
    }
}
