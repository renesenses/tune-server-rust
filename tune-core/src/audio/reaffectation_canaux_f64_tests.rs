//! TESS T06 — la réaffectation des canaux comme étage TESS, dans
//! l'exécuteur réel, en `f64` de bout en bout. Aucun son.

use std::sync::Arc;

use super::*;
use crate::tess::{EtageTess, Executeur, FormatFlux};

fn signal(canaux: usize, trames: usize) -> Vec<f64> {
    (0..trames * canaux)
        .map(|k| {
            let (t, c) = (k / canaux, k % canaux);
            0.05 * ((t as f64 * (0.011 + 0.003 * c as f64)).sin()) + 1e-10 * (c as f64 + 1.0)
        })
        .collect()
}

fn matrice(id: &str) -> Arc<Matrice> {
    Arc::new(Matrice::depuis_reglage(&prereglage(id).unwrap()).unwrap())
}

/// 4.0 → stéréo (N → M, tampon de sortie préalloué par l'exécuteur) et
/// échange G/D (N → N, sur place) : la sortie de l'exécuteur est EXACTEMENT
/// la matrice appliquée en `f64` (aucun arrondi `f32`), en blocs successifs,
/// et l'ancien chemin `f32` en diffère (contre-épreuve).
#[test]
fn l_etage_de_reaffectation_rend_la_matrice_en_f64_dans_l_executeur() {
    for (id, n) in [("quad_to_stereo", 4u16), ("swap_lr", 2), ("mono", 2)] {
        let m = matrice(id);
        let x = signal(usize::from(n), 3_000);
        let mut attendu = vec![0.0; 3_000 * usize::from(m.sorties())];
        m.appliquer_f64_dans(&x, &mut attendu);

        let mut etage = EtageReaffectation::new(m.clone());
        let mut ex = Executeur::new();
        let format = FormatFlux {
            channels: n,
            sample_rate_hz: 48_000,
        };
        let mut sortie = Vec::new();
        for bloc in x.chunks(usize::from(n) * 512) {
            let mut etages: [&mut dyn EtageTess; 1] = [&mut etage];
            ex.preparer(&mut etages, format, 512, false).unwrap();
            let mut t = bloc.to_vec();
            let trames = t.len() / usize::from(n);
            let s = ex.traiter(&mut etages, &mut t, trames).unwrap();
            assert_eq!(s.format.channels, m.sorties());
            sortie.extend_from_slice(s.echantillons);
        }
        assert_eq!(sortie, attendu, "{id} : l'étage n'est pas la matrice en f64");

        let x32: Vec<f32> = x.iter().map(|&v| v as f32).collect();
        let ancien: Vec<f64> = m.appliquer_f32(&x32).into_iter().map(f64::from).collect();
        if !m.est_recopie() {
            assert_ne!(ancien, attendu, "{id} : le banc ne discrimine pas");
        }
        let ecart = ancien
            .iter()
            .zip(&attendu)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0_f64, f64::max);
        assert!(ecart < 1e-6, "{id} : écart f32↔f64 {ecart:e}");
    }
}

/// Un flux dont le nombre de canaux n'est pas celui de la matrice est
/// refusé à la préparation (pas de réaffectation fausse en silence).
#[test]
fn un_format_qui_ne_correspond_pas_est_refuse() {
    let mut etage = EtageReaffectation::new(matrice("quad_to_stereo"));
    let r = etage.preparer(
        FormatFlux {
            channels: 2,
            sample_rate_hz: 44_100,
        },
        256,
    );
    assert!(matches!(
        r,
        Err(crate::tess::ErreurEtage::FormatNonPrisEnCharge { channels: 2, .. })
    ));
}

/// Contournement : possible en N → N (l'entrée ressort à l'identique),
/// ignoré en N → M (le nombre de canaux ne change pas en vol).
#[test]
fn le_contournement_ne_change_pas_le_nombre_de_canaux_en_vol() {
    let mut nn = EtageReaffectation::new(matrice("swap_lr"));
    nn.definir_bypass(true);
    assert!(nn.bypass());
    nn.preparer(
        FormatFlux {
            channels: 2,
            sample_rate_hz: 48_000,
        },
        64,
    )
    .unwrap();
    let x = signal(2, 64);
    let mut t = x.clone();
    nn.traiter_sur_place(&mut t, 64).unwrap();
    assert_eq!(t, x);

    let mut nm = EtageReaffectation::new(matrice("quad_to_stereo"));
    nm.definir_bypass(true);
    assert!(!nm.bypass());
}
