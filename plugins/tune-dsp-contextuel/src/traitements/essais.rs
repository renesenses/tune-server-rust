//! Outils communs aux tests des traitements.

use std::f64::consts::PI;

use super::Traitement;

/// Sinusoïde entrelacée : `canaux` canaux identiques, `trames` trames.
pub fn sinus(f: f64, cadence: f64, amplitude: f64, trames: usize, canaux: usize) -> Vec<f64> {
    let mut v = Vec::with_capacity(trames * canaux);
    for n in 0..trames {
        let x = amplitude * (2.0 * PI * f * n as f64 / cadence).sin();
        v.extend(std::iter::repeat_n(x, canaux));
    }
    v
}

/// Bruit déterministe (générateur congruentiel), pleine échelle × `amplitude`.
pub fn bruit(trames: usize, canaux: usize, amplitude: f64, graine: u64) -> Vec<f64> {
    let mut s = graine
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1);
    (0..trames * canaux)
        .map(|_| {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            amplitude * (((s >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0)
        })
        .collect()
}

/// Amplitude de la composante à `f` Hz d'un canal, mesurée par détection
/// synchrone sur les trames `[debut, fin)` (nombre entier de périodes
/// conseillé).
pub fn amplitude_a(
    tampon: &[f64],
    canaux: usize,
    canal: usize,
    f: f64,
    cadence: f64,
    debut: usize,
    fin: usize,
) -> f64 {
    let (mut s, mut c) = (0.0, 0.0);
    for n in debut..fin {
        let x = tampon[n * canaux + canal];
        let w = 2.0 * PI * f * n as f64 / cadence;
        s += x * w.sin();
        c += x * w.cos();
    }
    let k = (fin - debut) as f64;
    2.0 * (s * s + c * c).sqrt() / k
}

/// Traite `entree` par blocs de `bloc` trames et rend la sortie.
pub fn par_blocs(t: &mut dyn Traitement, entree: &[f64], canaux: usize, bloc: usize) -> Vec<f64> {
    let mut sortie = entree.to_vec();
    for morceau in sortie.chunks_mut(bloc * canaux) {
        let trames = morceau.len() / canaux;
        t.traiter(morceau, trames);
    }
    sortie
}

/// Un traitement INACTIF rend son entrée au bit près, et se dit neutre.
pub fn assert_identite_inactif(t: &mut dyn Traitement, canaux: u16, cadence: u32) {
    t.preparer(canaux, cadence).expect("préparation");
    assert!(!t.actif());
    assert!(t.est_neutre(), "{:?} inactif doit être neutre", t.genre());
    let entree = bruit(4_096, usize::from(canaux), 0.9, 7);
    let sortie = par_blocs(t, &entree, usize::from(canaux), 333);
    assert!(
        entree
            .iter()
            .zip(&sortie)
            .all(|(a, b)| a.to_bits() == b.to_bits()),
        "{:?} inactif a touché le signal",
        t.genre()
    );
}

/// Active puis coupe le traitement SUR UNE CRÊTE d'une sinusoïde (là où un
/// saut de gain ou de filtre se verrait le plus) : aucun écart d'une trame à
/// la suivante ne dépasse le double de la pente maximale de la sinusoïde, et
/// après le fondu de coupure le traitement redevient neutre et rend son
/// entrée au bit près.
pub fn assert_fondus_sans_saut(t: &mut dyn Traitement, canaux: u16, cadence: u32) {
    let c = usize::from(canaux);
    let fs = f64::from(cadence);
    t.preparer(canaux, cadence).expect("préparation");
    let trames = cadence as usize * 2 / 5; // 400 ms
    let entree = sinus(220.0, fs, 0.5, trames, c);
    // Crêtes de la sinusoïde : n = fs/880 + k·fs/220.
    let crete = |k: f64| (fs / 880.0 + k * fs / 220.0).round() as usize;
    let (allumage, coupure) = (crete(3.0), crete(40.0));
    let mut sortie = entree.clone();
    let mut debut = 0;
    for (fin, bascule) in [
        (allumage, Some(true)),
        (coupure, Some(false)),
        (trames, None),
    ] {
        for morceau in sortie[debut * c..fin * c].chunks_mut(256 * c) {
            let n = morceau.len() / c;
            t.traiter(morceau, n);
        }
        if let Some(actif) = bascule {
            t.activer(actif);
        }
        debut = fin;
    }
    // Le fondu (20 ms) est fini : la fin est l'entrée exacte.
    assert!(t.est_neutre(), "{:?} doit redevenir neutre", t.genre());
    let fin = (trames - 1_000) * c;
    assert!(
        entree[fin..]
            .iter()
            .zip(&sortie[fin..])
            .all(|(a, b)| a.to_bits() == b.to_bits()),
        "{:?} coupé doit rendre l'entrée au bit près",
        t.genre()
    );
    let pente_max = 2.0 * 0.5 * 2.0 * std::f64::consts::PI * 220.0 / fs;
    for k in 0..c {
        for n in 1..trames {
            let d = (sortie[n * c + k] - sortie[(n - 1) * c + k]).abs();
            assert!(
                d <= pente_max,
                "{:?} : saut de {d} à la trame {n} (max {pente_max})",
                t.genre()
            );
        }
    }
}
