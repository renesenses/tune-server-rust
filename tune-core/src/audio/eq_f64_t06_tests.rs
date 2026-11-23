//! TESS T06 — la façade de l'égaliseur en `f64` : fondus de bascule (#5215),
//! extinction du préampli gardé (#6211, #6741) et compensation portée
//! (#5227) se jouent en `f64` exactement comme en `f32`, sans saut.
//! Aucun son.

use std::f64::consts::PI;

use super::*;

const SR: u32 = 48_000;

fn egaliseur(gain: f64) -> EqProcessor {
    let profil = EqProfile {
        enabled: true,
        bands: vec![
            EqBandSpec {
                freq: 120.0,
                gain,
                q: 0.9,
                band_type: "peak".into(),
                ..Default::default()
            },
            EqBandSpec {
                freq: 6_000.0,
                gain: -gain / 2.0,
                q: 0.7,
                band_type: "high_shelf".into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    EqProcessor::new(&profil, SR, 2)
}

/// Sinus stéréo en double précision (non représentable en `f32`).
fn sinus(trames: usize) -> Vec<f64> {
    (0..trames)
        .flat_map(|i| {
            let t = i as f64 / f64::from(SR);
            [
                0.3 * (2.0 * PI * 220.0 * t).sin() + 1e-10,
                0.3 * (2.0 * PI * 223.0 * t + 0.4).sin(),
            ]
        })
        .collect()
}

/// Joue `x` par blocs de `bloc` trames, avec un geste en vol à la trame
/// `coupe` : `geste` reçoit l'égaliseur courant et rend le suivant.
fn jouer<T: EchantillonHote>(
    x: &[T],
    bloc: usize,
    coupe: usize,
    depart: Option<EqProcessor>,
    geste: impl FnOnce(Option<EqProcessor>) -> Option<EqProcessor>,
) -> Vec<T> {
    let mut eq = depart;
    let mut geste = Some(geste);
    let mut y = x.to_vec();
    let mut fait = 0;
    for morceau in y.chunks_mut(2 * bloc) {
        if fait >= coupe
            && let Some(g) = geste.take()
        {
            eq = g(eq.take());
        }
        if let Some(e) = eq.as_mut() {
            e.traiter(morceau);
        }
        fait += morceau.len() / 2;
    }
    y
}

fn ecart_max(a: &[f64], b: &[f64]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0, f64::max)
}

fn d2_max(y: &[f64], de: usize, a: usize) -> f64 {
    (de.max(2)..a)
        .map(|n| (y[2 * n] - 2.0 * y[2 * (n - 1)] + y[2 * (n - 2)]).abs())
        .fold(0.0, f64::max)
}

/// Les gestes en vol de la façade : activation (+12 dB, fondu de 300 ms),
/// coupure (préampli gardé puis remontée, #6741 avec borne posée), gros
/// changement de réglage (fondu), et activation avec compensation portée
/// (#5227). Pour
/// chacun : le chemin `f64` rend la même chose que le chemin `f32` sous
/// −120 dBFS, et la différence seconde du fondu ne dépasse pas celle du
/// signal établi de part et d'autre (aucun clic).
#[test]
fn les_gestes_en_vol_se_jouent_en_f64_comme_en_f32_et_sans_clic() {
    const TRAMES: usize = 96_000;
    const COUPE: usize = 24_000;
    let x = sinus(TRAMES);
    let x32: Vec<f32> = x.iter().map(|&v| v as f32).collect();
    type Geste = fn(Option<EqProcessor>) -> Option<EqProcessor>;
    let gestes: [(&str, Option<fn() -> EqProcessor>, Geste); 4] = [
        ("activation +12 dB", None, |p| {
            EqProcessor::prendre_la_releve(Some(egaliseur(12.0)), p)
        }),
        ("coupure", Some(|| egaliseur(12.0)), |p| {
            let mut n = EqProcessor::prendre_la_releve(None, p);
            if let Some(n) = n.as_mut() {
                n.borner_la_remontee(1.0);
            }
            n
        }),
        ("+3 → +12 dB", Some(|| egaliseur(3.0)), |p| {
            EqProcessor::prendre_la_releve(Some(egaliseur(12.0)), p)
        }),
        ("activation compensée", None, |p| {
            let mut n = egaliseur(6.0);
            n.porter_la_compensation(1.6);
            EqProcessor::prendre_la_releve(Some(n), p)
        }),
    ];
    for (nom, depart, geste) in gestes {
        let y64 = jouer(&x, 1_000, COUPE, depart.map(|f| f()), geste);
        let y32: Vec<f64> = jouer(&x32, 1_000, COUPE, depart.map(|f| f()), geste)
            .into_iter()
            .map(f64::from)
            .collect();
        let ecart = ecart_max(&y64, &y32);
        let avant = d2_max(&y64, 1_000, COUPE - 10);
        let fondu = d2_max(&y64, COUPE - 10, COUPE + 14_400 + 10);
        let apres = d2_max(&y64, TRAMES - 20_000, TRAMES);
        println!(
            "T06 façade EQ {nom:<20} f32↔f64 {:.1} dBFS ; d² avant {avant:.3e}, fondu {fondu:.3e}, fin {apres:.3e}",
            20.0 * ecart.max(1e-300).log10()
        );
        assert!(ecart < 1e-6, "{nom} : f32↔f64 {ecart:e}");
        assert!(
            fondu <= avant.max(apres) * 1.05,
            "{nom} : pointe au geste ({fondu:e} contre {avant:e} / {apres:e})"
        );
    }
}

/// Le chemin `f64` de la façade ne passe par aucun `f32` : sur une source
/// non représentable en `f32`, la sortie n'est PAS sur la grille `f32`
/// (contre-épreuve : le chemin `f32` y est partout).
#[test]
fn la_facade_ne_repasse_jamais_par_f32() {
    let x = sinus(4_000);
    let mut e = EqProcessor::prendre_la_releve(Some(egaliseur(9.0)), None).unwrap();
    let mut y = x.clone();
    e.process_interleaved_f64(&mut y);
    let hors_grille = y.iter().filter(|&&v| f64::from(v as f32) != v).count();
    assert!(
        hors_grille > y.len() * 9 / 10,
        "{hors_grille} sur {}",
        y.len()
    );
}
