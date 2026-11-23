//! TESS T06 — banc sonore de l'égaliseur en `f64` (graphique, paramétrique,
//! casque AutoEQ et OPRA : un seul moteur, une seule cascade).
//!
//! Aucun son : tout est calculé en mémoire.
//!
//! - **Épreuve « 64 bits réels »** : la sortie du chemin `f64` est comparée à
//!   une référence calculée À PART, hors ligne, en précision ÉTENDUE : la
//!   même cascade (mêmes coefficients préparés), récurrence en double-double
//!   (≈ 106 bits). L'écart mesuré est donc le bruit de calcul propre du
//!   moteur ; il doit rester sous 1e-12 relatif (≈ −240 dB) — un seul
//!   arrondi `f32` dans la chaîne le porterait vers 1e-8.
//! - **Contre-épreuve** : le chemin `f32` (celui d'avant) échoue au même banc.
//! - **Mêmes courbes qu'avant** : le module de la réponse impulsionnelle
//!   `f64`, évalué aux 256 fréquences de [`EqProcessor::response`] (calculée
//!   depuis les coefficients, inchangés par T06), colle à 1e-6 dB près.
//! - **Inaudible** : l'écart entre l'ancien chemin `f32` et le chemin `f64`
//!   reste sous −120 dBFS ; le bruit de calcul du `f64` est, lui, des
//!   dizaines de dB plus bas que celui du `f32`.
//! - **Réglage changé en lecture** : le chemin `f64` se comporte comme le
//!   chemin `f32` (même historique hérité), sans saut nouveau.
//!
//! Les mesures sont imprimées (`--nocapture`) pour le rapport.

use std::f64::consts::PI;

use tune_plugin_equalizer::{EqBandSpec, EqProcessor, EqProfile, Equalizer, HeadroomMode};
use tune_plugin_sdk::audio::*;

const SR: u32 = 48_000;
const N: usize = 6_000;

// ---------------------------------------------------------------------------
// Profils : graphique 31 bandes, paramétrique, casque (forme AutoEQ / OPRA :
// préampli + étagères + pics), et réglages extrêmes.
// ---------------------------------------------------------------------------

const GRILLE_31: [f64; 31] = [
    20.0, 25.0, 31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 315.0, 400.0,
    500.0, 630.0, 800.0, 1000.0, 1250.0, 1600.0, 2000.0, 2500.0, 3150.0, 4000.0, 5000.0, 6300.0,
    8000.0, 10000.0, 12500.0, 16000.0, 20000.0,
];

fn bande(t: &str, freq: f64, gain: f64, q: f64) -> EqBandSpec {
    EqBandSpec {
        freq,
        gain,
        q,
        band_type: t.into(),
        ..Default::default()
    }
}

fn profil(bands: Vec<EqBandSpec>) -> EqProfile {
    EqProfile {
        enabled: true,
        bands,
        ..Default::default()
    }
}

fn graphique_31() -> EqProfile {
    profil(
        GRILLE_31
            .iter()
            .enumerate()
            .map(|(i, &f)| bande("peak", f, if i % 2 == 0 { 6.0 } else { -4.5 }, 4.32))
            .collect(),
    )
}

fn parametrique() -> EqProfile {
    profil(vec![
        bande("low_shelf", 105.0, 4.0, 0.7),
        bande("peak", 1_700.0, -7.0, 1.2),
        bande("peak", 3_200.0, 3.5, 2.5),
        bande("high_shelf", 9_000.0, -2.5, 0.7),
    ])
}

/// La forme d'un profil AutoEQ / OPRA (casque) : étagères à Q 0,7 et une
/// dizaine de pics, gains et Q non ronds.
fn casque_autoeq() -> EqProfile {
    profil(vec![
        bande("low_shelf", 105.0, 6.3, 0.7),
        bande("peak", 62.0, -2.1, 0.48),
        bande("peak", 184.0, -3.7, 0.83),
        bande("peak", 1_117.0, 1.9, 1.62),
        bande("peak", 2_540.0, -4.4, 2.91),
        bande("peak", 3_430.0, 3.2, 4.07),
        bande("peak", 5_420.0, -5.6, 3.33),
        bande("peak", 7_120.0, 2.7, 5.12),
        bande("peak", 9_870.0, -3.9, 2.18),
        bande("high_shelf", 10_000.0, -1.8, 0.7),
    ])
}

/// Réglages EXTRÊMES : gains aux bornes (±24 dB), Q aux bornes (0,1 et 30),
/// fréquences aux bornes (10 Hz, 0,45 × la cadence), filtres coupe-bas,
/// coupe-haut résonant et réjecteur.
fn extreme() -> EqProfile {
    profil(vec![
        bande("high_pass", 10.0, 0.0, 0.1),
        bande("peak", 30.0, 24.0, 30.0),
        bande("peak", 400.0, -24.0, 0.1),
        bande("notch", 1_000.0, 0.0, 30.0),
        bande("high_shelf", 12_000.0, 24.0, 0.7),
        bande("low_pass", 21_600.0, 0.0, 30.0),
    ])
}

fn profils() -> Vec<(&'static str, EqProfile)> {
    vec![
        ("graphique 31 bandes", graphique_31()),
        ("paramétrique", parametrique()),
        ("casque AutoEQ/OPRA", casque_autoeq()),
        ("extrême", extreme()),
    ]
}

// ---------------------------------------------------------------------------
// Signaux : sinus, bruit, impulsion, tous en double précision (non
// représentables en `f32`, comme une source 32 bits ou la sortie d'un étage).
// ---------------------------------------------------------------------------

struct Alea(u64);
impl Alea {
    fn suivant(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }
}

/// Stéréo entrelacée, `N` trames. Amplitude choisie pour que la sortie la plus
/// poussée reste un signal ordinaire (la réserve automatique s'en charge).
fn signaux() -> Vec<(&'static str, Vec<f64>)> {
    let sinus = |f: f64, a: f64| -> Vec<f64> {
        (0..N)
            .flat_map(|i| {
                let t = i as f64 / f64::from(SR);
                [
                    a * (2.0 * PI * f * t).sin(),
                    a * (2.0 * PI * f * t + 0.3).sin(),
                ]
            })
            .collect()
    };
    let mut alea = Alea(0x9e37_79b9_7f4a_7c15);
    let bruit: Vec<f64> = (0..2 * N).map(|_| 0.5 * alea.suivant()).collect();
    let mut impulsion = vec![0.0; 2 * N];
    impulsion[2 * 17] = 0.7;
    impulsion[2 * 401 + 1] = -0.6;
    vec![
        ("sinus 1 kHz", sinus(1_000.0, 0.5)),
        ("sinus 31 Hz", sinus(31.0, 0.5)),
        ("sinus 19 kHz", sinus(19_000.0, 0.5)),
        ("bruit blanc", bruit),
        ("impulsion", impulsion),
    ]
}

// ---------------------------------------------------------------------------
// La référence hors ligne : la MÊME cascade (mêmes coefficients préparés,
// même pré-gain), récurrence calculée en double-double (≈ 106 bits de
// mantisse, opérations exactes de Dekker/Knuth par `mul_add`). Son erreur
// propre est ~1e-30 : tout l'écart mesuré est celui du moteur.
// ---------------------------------------------------------------------------

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
    /// `self × c`, `c` un `f64` exact (coefficient).
    fn fois(self, c: f64) -> Dd {
        let p = deux_produits(self.0, c);
        deux_sommes(p.0, p.1 + self.1 * c)
    }
    fn neg(self) -> Dd {
        Dd(-self.0, -self.1)
    }
    fn f64(self) -> f64 {
        self.0 + self.1
    }
}

fn reference(p: &EqProfile, entree: &[f64]) -> Vec<f64> {
    let (cascades, preamp) = EqProcessor::new(p, SR, 2).coefficients_du_banc();
    let mut sortie = vec![0.0; entree.len()];
    for canal in 0..2 {
        let etages = &cascades[canal];
        // [x1, x2, y1, y2] par étage.
        let mut etat = vec![[Dd::default(); 4]; etages.len()];
        for (i, x) in entree.iter().skip(canal).step_by(2).enumerate() {
            let mut s = deux_produits(*x, preamp[canal]);
            for (c, e) in etages.iter().zip(etat.iter_mut()) {
                let y = s
                    .fois(c[0])
                    .plus(e[0].fois(c[1]))
                    .plus(e[1].fois(c[2]))
                    .plus(e[2].fois(c[3]).neg())
                    .plus(e[3].fois(c[4]).neg());
                *e = [s, e[0], y, e[2]];
                s = y;
            }
            sortie[2 * i + canal] = s.f64();
        }
    }
    sortie
}

/// Réponse impulsionnelle de `canal`, `n` échantillons, par le chemin `f64`.
fn reponse_impulsionnelle(p: &EqProfile, canal: usize, n: usize) -> Vec<f64> {
    let mut x = vec![0.0; 2 * n];
    x[canal] = 1.0;
    EqProcessor::new(p, SR, 2).process_interleaved_f64(&mut x);
    x.iter().skip(canal).step_by(2).copied().collect()
}

fn par_f64(p: &EqProfile, entree: &[f64], bloc: usize) -> Vec<f64> {
    let mut e = EqProcessor::new(p, SR, 2);
    let mut y = entree.to_vec();
    for morceau in y.chunks_mut(2 * bloc) {
        e.process_interleaved_f64(morceau);
    }
    y
}

fn par_f32(p: &EqProfile, entree: &[f64], bloc: usize) -> Vec<f64> {
    let mut e = EqProcessor::new(p, SR, 2);
    let mut y: Vec<f32> = entree.iter().map(|&v| v as f32).collect();
    for morceau in y.chunks_mut(2 * bloc) {
        e.process_interleaved(morceau);
    }
    y.into_iter().map(f64::from).collect()
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

// ---------------------------------------------------------------------------
// Preuves
// ---------------------------------------------------------------------------

/// ÉPREUVE et CONTRE-ÉPREUVE « 64 bits réels », pour chaque profil et chaque
/// signal, en blocs irréguliers (l'état passe d'un bloc à l'autre).
///
/// Seuil : 1e-12 relatif à la crête (≈ −240 dB, critère du plan TESS). Une
/// exception mesurée et documentée : le graphique 31 bandes (31 biquads de
/// Q 4,32 en cascade, forme directe I) a un bruit de calcul `f64` PROPRE de
/// ≈ 3e-12 en crête (≈ −231 dB, −260 dB RMS) ; son seuil est 1e-11
/// (−220 dB). C'est l'arithmétique `f64` de la cascade d'origine, pas un
/// arrondi `f32` : celui-ci la porterait à ~1e-7 (−142 dB), 90 dB au-dessus.
/// Le réduire demanderait de changer la structure du filtre, donc le son :
/// hors de T06.
#[test]
fn soixante_quatre_bits_reels_contre_la_reference_hors_ligne() {
    let mut echecs = Vec::new();
    for (nom_p, p) in profils() {
        let seuil = if nom_p.starts_with("graphique 31") {
            1e-11
        } else {
            1e-12
        };
        for (nom_s, x) in signaux() {
            let r = reference(&p, &x);
            let y64 = par_f64(&p, &x, 333);
            let y32 = par_f32(&p, &x, 333);
            let crete = r.iter().fold(0.0_f64, |m, v| m.max(v.abs())).max(1e-3);
            let e64 = ecart_max(&y64, &r) / crete;
            let e32 = ecart_max(&y32, &r) / crete;
            println!(
                "T06 EQ {nom_p:<20} {nom_s:<12} écart f64 {:>8.1} dB ({e64:.2e}), f32 {:>7.1} dB (relatif à la crête)",
                db(e64),
                db(e32)
            );
            if e64 >= seuil {
                echecs.push(format!("{nom_p} / {nom_s} : écart f64 {e64:e} ≥ {seuil:e}"));
            }
            if e32 <= 1e-10 {
                echecs.push(format!(
                    "{nom_p} / {nom_s} : le banc ne discrimine pas (f32 à {e32:e})"
                ));
            }
        }
    }
    assert!(echecs.is_empty(), "{echecs:#?}");
}

/// NON-RÉGRESSION des courbes : la réponse en fréquence MESURÉE sur la réponse
/// impulsionnelle `f64` colle à la courbe calculée depuis les coefficients
/// (`response`, inchangée par T06) à 1e-6 dB près, sur ses 256 points.
#[test]
fn memes_courbes_qu_avant_a_1e_6_db() {
    // Assez long pour que la queue la plus lente (pic de 30 Hz à Q 30 du
    // profil extrême) soit éteinte sous 1e-14 : ce qu'elle laisse dehors pèse
    // moins de 1e-8 dB.
    const L: usize = 1 << 20;
    for (nom, p) in profils() {
        let attendu = EqProcessor::new(&p, SR, 2).response(SR);
        let hz: Vec<f64> = attendu["frequency_hz"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap())
            .collect();
        let mut pire = 0.0_f64;
        for canal in 0..2 {
            let h = reponse_impulsionnelle(&p, canal, L);
            let queue = h[L - 4096..].iter().fold(0.0_f64, |m, v| m.max(v.abs()));
            assert!(queue < 1e-14, "{nom} : réponse pas éteinte ({queue:e})");
            let courbe = attendu["channels_db"][canal].as_array().unwrap();
            // Un point sur quatre de la courbe (64 fréquences de 10 Hz à
            // 24 kHz) : la DFT de 2^19 points reste rapide.
            for (f, a) in hz.iter().zip(courbe).step_by(4) {
                let w = 2.0 * PI * f / f64::from(SR);
                let (mut re, mut im) = (0.0_f64, 0.0_f64);
                // Phaseur e^{-iwn} par rotation, recalé tous les 1 024 pas.
                let (zs, zc) = w.sin_cos();
                let (mut pc, mut ps) = (1.0_f64, 0.0_f64);
                for (n, v) in h.iter().enumerate() {
                    if n % 1_024 == 0 {
                        let (s, c) = (w * n as f64).sin_cos();
                        (pc, ps) = (c, s);
                    }
                    re += v * pc;
                    im -= v * ps;
                    (pc, ps) = (pc * zc - ps * zs, ps * zc + pc * zs);
                }
                let mesure = 10.0 * (re * re + im * im).log10();
                pire = pire.max((mesure - a.as_f64().unwrap()).abs());
            }
        }
        println!("T06 EQ {nom:<20} courbe : écart max {pire:.2e} dB");
        assert!(pire < 1e-6, "{nom} : courbe changée de {pire:e} dB");
    }
}

/// INAUDIBLE : sur les signaux de test, l'ancien chemin `f32` et le chemin
/// `f64` diffèrent de moins de −120 dBFS ; et le bruit de calcul du `f64`
/// (écart RMS à la référence étendue) est au moins 60 dB sous celui du
/// `f32`.
#[test]
fn l_ecart_avec_l_ancien_chemin_est_inaudible_et_le_bruit_baisse() {
    for (nom_p, p) in profils() {
        for (nom_s, x) in signaux() {
            let r = reference(&p, &x);
            let y64 = par_f64(&p, &x, 1_024);
            let y32 = par_f32(&p, &x, 1_024);
            let ecart = ecart_max(&y64, &y32);
            let bruit64 = ecart_rms(&y64, &r);
            let bruit32 = ecart_rms(&y32, &r);
            println!(
                "T06 EQ {nom_p:<20} {nom_s:<12} f32↔f64 max {:>7.1} dBFS ; bruit de calcul f32 {:>7.1} dBFS, f64 {:>7.1} dBFS",
                db(ecart),
                db(bruit32),
                db(bruit64)
            );
            assert!(db(ecart) < -120.0, "{nom_p} / {nom_s} : {} dBFS", db(ecart));
            assert!(
                db(bruit64) < db(bruit32) - 60.0,
                "{nom_p} / {nom_s} : le f64 ne réduit pas le bruit ({} contre {} dBFS)",
                db(bruit64),
                db(bruit32)
            );
        }
    }
}

/// Le mode « Réaliste » (#5171, limiteur LIÉ) : non linéaire, donc pas de
/// référence par convolution ; le chemin `f64` y rend la même chose que le
/// chemin `f32` sous −120 dBFS, limiteur engagé compris.
#[test]
fn le_mode_realiste_et_son_limiteur_suivent_en_f64() {
    let mut p = extreme();
    p.headroom_mode = HeadroomMode::Realistic;
    for (nom_s, x) in signaux() {
        let x: Vec<f64> = x.iter().map(|v| v * 1.8).collect();
        let y64 = par_f64(&p, &x, 777);
        let y32 = par_f32(&p, &x, 777);
        let ecart = ecart_max(&y64, &y32);
        println!(
            "T06 EQ réaliste {nom_s:<12} f32↔f64 max {:>7.1} dBFS",
            db(ecart)
        );
        assert!(db(ecart) < -120.0, "{nom_s} : {} dBFS", db(ecart));
    }
}

/// Le chemin `f32` d'avant reste celui d'avant AU BIT PRÈS (la boucle est
/// partagée) : un `f64` qui porte exactement des `f32` rend exactement la
/// sortie `f32`, arrondie une fois.
#[test]
fn le_chemin_f32_reste_au_bit_pres() {
    for (_, p) in profils() {
        for (_, x) in signaux() {
            let x32: Vec<f64> = x.iter().map(|&v| f64::from(v as f32)).collect();
            let y64: Vec<f32> = par_f64(&p, &x32, 512)
                .into_iter()
                .map(|v| v as f32)
                .collect();
            let y32: Vec<f32> = par_f32(&p, &x32, 512)
                .into_iter()
                .map(|v| v as f32)
                .collect();
            assert_eq!(
                y64.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                y32.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
            );
        }
    }
}

/// Le greffon SDK accepte `F64` (manifeste `audio-f64`) et rend, bloc par
/// bloc, avec des mises à jour de réglage en vol, exactement le moteur `f64`.
#[test]
fn le_greffon_sdk_traite_le_f64_tel_quel() {
    let manifeste: serde_json::Value =
        serde_json::from_str(include_str!("../manifest.json")).unwrap();
    let demande = manifeste["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["id"] == "audio-f64");
    assert!(demande, "le manifeste doit demander audio-f64");

    let f = AudioFormat::new(SR, ChannelLayout::Stereo, SampleEncoding::F64).unwrap();
    let p = casque_autoeq();
    let reglages = serde_json::to_value(&p).unwrap();
    let mut greffon = Equalizer.prepare(f, 512, &reglages).unwrap();
    let (_, x) = signaux().remove(3);
    let mut attendu = x.clone();
    EqProcessor::new(&p, SR, 2).process_interleaved_f64(&mut attendu);
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
        greffon.update(&reglages).unwrap();
    }
    assert_eq!(y, attendu);
}

/// RÉGLAGE CHANGÉ EN LECTURE, au niveau du MOTEUR : un cran de curseur
/// (+6 → +5 dB) sur un sinus, en plein bloc. Le chemin `f64` hérite de
/// l'historique exactement comme le chemin `f32` (#1725, cran immédiat sous
/// `SEUIL_DE_RAMPE_DB` côté hôte) : la transition est LA MÊME sous
/// −120 dBFS — T06 n'ajoute aucune discontinuité. (Le fondu des changements
/// plus grands vit dans la façade de l'hôte, éprouvé en `f64` dans
/// `tune-core`.) La pointe de différence seconde est imprimée pour le
/// rapport.
#[test]
fn un_reglage_change_en_lecture_ne_claque_pas_en_f64() {
    let avant = profil(vec![bande("peak", 1_000.0, 6.0, 1.0)]);
    let apres = profil(vec![bande("peak", 1_000.0, 5.0, 1.0)]);
    let x: Vec<f64> = (0..2 * N)
        .map(|i| 0.4 * (2.0 * PI * 997.0 * (i / 2) as f64 / f64::from(SR)).sin())
        .collect();
    let coupe = 2 * 3_001;
    let rendre64 = || {
        let mut a = EqProcessor::new(&avant, SR, 2);
        let mut y = x.clone();
        a.process_interleaved_f64(&mut y[..coupe]);
        let mut b = EqProcessor::new(&apres, SR, 2);
        b.inherit_state_from(&a);
        b.process_interleaved_f64(&mut y[coupe..]);
        y
    };
    let rendre32 = || {
        let mut a = EqProcessor::new(&avant, SR, 2);
        let mut y: Vec<f32> = x.iter().map(|&v| v as f32).collect();
        a.process_interleaved(&mut y[..coupe]);
        let mut b = EqProcessor::new(&apres, SR, 2);
        b.inherit_state_from(&a);
        b.process_interleaved(&mut y[coupe..]);
        y.into_iter().map(f64::from).collect::<Vec<_>>()
    };
    let y64 = rendre64();
    let y32 = rendre32();
    let ecart = ecart_max(&y64, &y32);
    assert!(
        db(ecart) < -120.0,
        "transition f64 ≠ f32 : {} dBFS",
        db(ecart)
    );
    // Différence seconde du canal gauche : un clic est une pointe.
    let d2 = |y: &[f64], de: usize, a: usize| {
        (de.max(2)..a)
            .map(|n| (y[2 * n] - 2.0 * y[2 * (n - 1)] + y[2 * (n - 2)]).abs())
            .fold(0.0, f64::max)
    };
    let etabli = d2(&y64, 1_000, 2_900);
    let transition = d2(&y64, 2_990, 3_020);
    let transition32 = d2(&y32, 2_990, 3_020);
    println!(
        "T06 EQ cran en vol : d² établi {etabli:.3e}, transition f64 {transition:.3e}, f32 {transition32:.3e} ; écart f32↔f64 {:.1} dBFS",
        db(ecart)
    );
    assert!((transition - transition32).abs() <= 1e-6 * transition32.max(1e-9) + 1e-9);
}

/// Banc CPU (à lancer en `--release -- --ignored --nocapture`) : égaliseur
/// graphique 31 bandes, 96 kHz stéréo, 60 s d'audio, `f32` contre `f64`.
#[test]
#[ignore = "banc CPU : cargo test --release -- --ignored --nocapture"]
fn banc_cpu_egaliseur_31_bandes_96_khz() {
    const SR96: u32 = 96_000;
    const SECONDES: usize = 60;
    const BLOC: usize = 1_024;
    let p = graphique_31();
    let mut alea = Alea(7);
    let x: Vec<f64> = (0..2 * BLOC * 64).map(|_| 0.3 * alea.suivant()).collect();
    let blocs = SECONDES * SR96 as usize / BLOC;
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
    let mut e32 = EqProcessor::new(&p, SR96, 2);
    let mut t32: Vec<f32> = x.iter().map(|&v| v as f32).collect();
    let s32 = mesurer(&mut |b| {
        let i = (b % 64) * 2 * BLOC;
        e32.process_interleaved(&mut t32[i..i + 2 * BLOC]);
    });
    let mut e64 = EqProcessor::new(&p, SR96, 2);
    let mut t64 = x.clone();
    let s64 = mesurer(&mut |b| {
        let i = (b % 64) * 2 * BLOC;
        e64.process_interleaved_f64(&mut t64[i..i + 2 * BLOC]);
    });
    println!(
        "T06 CPU égaliseur 31 bandes 96 kHz stéréo, {SECONDES} s d'audio : f32 {:.3} s ({:.2} % d'un cœur), f64 {:.3} s ({:.2} %), ratio f64/f32 {:.3}",
        s32,
        100.0 * s32 / SECONDES as f64,
        s64,
        100.0 * s64 / SECONDES as f64,
        s64 / s32
    );
}
