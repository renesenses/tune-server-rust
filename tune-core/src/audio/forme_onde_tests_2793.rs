//! Témoins de la forme d'onde (fil du forum 2793) : chiffres d'un sinus,
//! écrêtage, deux canaux, colonnes, et un vrai fichier décodé.

use super::*;

const SR: u32 = 44_100;

/// `secondes` de trames entrelacées : un sinus de 1 kHz par canal, à
/// l'amplitude (linéaire) donnée pour chacun.
fn sinus(amplitudes: &[f64], secondes: f64) -> Vec<f32> {
    let n = (secondes * f64::from(SR)) as usize;
    let mut o = Vec::with_capacity(n * amplitudes.len());
    for i in 0..n {
        let s = (std::f64::consts::TAU * 1000.0 * i as f64 / f64::from(SR)).sin();
        for a in amplitudes {
            o.push((a * s).clamp(-1.0, 1.0) as f32);
        }
    }
    o
}

fn amplitude(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

fn forme(entrelaces: &[f32], canaux: u32, colonnes: usize, estimees: Option<u64>) -> FormeOnde {
    let mut acc = Accumulateur::new(SR, canaux, colonnes, estimees);
    // Par morceaux de taille quelconque : le découpage ne doit rien changer.
    for morceau in entrelaces.chunks(7919 * canaux as usize) {
        acc.pousser(morceau);
    }
    acc.terminer()
}

#[test]
fn sinus_a_moins_6_dbfs_crete_rms_et_facteur_de_crete() {
    let f = forme(
        &sinus(&[amplitude(-6.0)], 3.0),
        1,
        300,
        Some(3 * u64::from(SR)),
    );
    assert_eq!(f.voies.len(), 1);
    let crete = f.crete_dbfs().unwrap();
    let rms = f.rms_dbfs().unwrap();
    assert!((crete - -6.0).abs() < 0.05, "crête {crete}");
    assert!((rms - -9.01).abs() < 0.05, "RMS {rms}");
    let plr = f.facteur_de_crete_db().unwrap();
    assert!((plr - 3.01).abs() < 0.05, "PLR {plr}");
    assert_eq!(f.ecretes, 0, "aucun écrêtage à −6 dBFS");
    assert_eq!(f.colonnes, 300);
    // Chaque colonne voit le sinus entier : crêtes ±0,5, RMS ≈ 0,354.
    let v = &f.voies[0];
    assert_eq!(v.min.len(), 300);
    for j in 0..300 {
        assert!((v.max[j] - 0.501).abs() < 0.01, "max[{j}] = {}", v.max[j]);
        assert!((v.min[j] + 0.501).abs() < 0.01, "min[{j}] = {}", v.min[j]);
        assert!((v.rms[j] - 0.354).abs() < 0.01, "rms[{j}] = {}", v.rms[j]);
    }
    assert!((f.duree_s - 3.0).abs() < 1e-6);
}

#[test]
fn signal_ecrete_compte_les_echantillons_a_pleine_echelle() {
    // Un sinus poussé à +6 dB, coupé à ±1 : un bon tiers des échantillons.
    let f = forme(&sinus(&[2.0], 1.0), 1, 100, None);
    assert!(f.ecretes > 0);
    assert!(f.ecretes > u64::from(SR) / 4, "{} écrêtés", f.ecretes);
    assert!(f.crete_dbfs().unwrap().abs() < 1e-3);
    // Un facteur de crête écrasé : moins de 3 dB.
    assert!(f.facteur_de_crete_db().unwrap() < 3.0);
}

#[test]
fn stereo_deux_canaux_distincts() {
    let f = forme(
        &sinus(&[amplitude(-6.0), amplitude(-12.0)], 2.0),
        2,
        200,
        None,
    );
    assert_eq!(f.voies.len(), 2);
    let g = f.voies[0].crete_dbfs().unwrap();
    let d = f.voies[1].crete_dbfs().unwrap();
    assert!((g - -6.0).abs() < 0.05, "gauche {g}");
    assert!((d - -12.0).abs() < 0.05, "droite {d}");
    assert!((f.voies[1].rms_dbfs().unwrap() - -15.01).abs() < 0.05);
    // La crête globale est celle du canal le plus fort.
    assert!((f.crete_dbfs().unwrap() - -6.0).abs() < 0.05);
    assert_eq!(f.voies[0].max.len(), 200);
    assert_eq!(f.voies[1].max.len(), 200);
}

#[test]
fn memoire_bornee_sans_duree_connue_et_colonnes_exactes() {
    // Sans estimation, les cases fusionnent : le nombre de colonnes reste
    // celui demandé, et la RMS ne bouge pas.
    let f = forme(&sinus(&[0.5], 30.0), 1, 1000, None);
    assert_eq!(f.colonnes, 1000);
    assert_eq!(f.voies[0].rms.len(), 1000);
    assert!((f.rms_dbfs().unwrap() - dbfs(0.5 / 2f64.sqrt()).unwrap()).abs() < 0.01);
}

#[test]
fn silence_sans_dbfs_et_signal_court() {
    let f = forme(&vec![0.0; 100], 1, 1000, None);
    assert_eq!(f.crete_dbfs(), None);
    assert_eq!(f.facteur_de_crete_db(), None);
    // Moins de cases que de colonnes demandées : autant de colonnes que de cases.
    assert!(f.colonnes >= 1 && f.colonnes < 1000);
}

#[test]
fn normaliser_pleine_echelle() {
    let n = normaliser(&[32767, -32768, 0, 16384], 16);
    assert!((n[0] - 1.0).abs() < 1e-4);
    assert_eq!(n[1], -1.0);
    assert_eq!(n[2], 0.0);
    assert!((n[3] - 0.5).abs() < 1e-6);
}

/// Un WAV PCM 16 bits stéréo : sinus de 1 kHz, gauche à −6 dBFS, droite écrêtée.
fn ecrire_wav_stereo(chemin: &std::path::Path, secondes: f64) {
    let n = (secondes * f64::from(SR)) as usize;
    let donnees = (n * 4) as u32;
    let mut o = Vec::with_capacity(44 + donnees as usize);
    o.extend_from_slice(b"RIFF");
    o.extend_from_slice(&(36 + donnees).to_le_bytes());
    o.extend_from_slice(b"WAVEfmt ");
    o.extend_from_slice(&16u32.to_le_bytes());
    o.extend_from_slice(&1u16.to_le_bytes());
    o.extend_from_slice(&2u16.to_le_bytes());
    o.extend_from_slice(&SR.to_le_bytes());
    o.extend_from_slice(&(SR * 4).to_le_bytes());
    o.extend_from_slice(&4u16.to_le_bytes());
    o.extend_from_slice(&16u16.to_le_bytes());
    o.extend_from_slice(b"data");
    o.extend_from_slice(&donnees.to_le_bytes());
    for i in 0..n {
        let s = (std::f64::consts::TAU * 1000.0 * i as f64 / f64::from(SR)).sin();
        let g = (amplitude(-6.0) * s * 32767.0) as i16;
        let d = ((2.0 * s).clamp(-1.0, 1.0) * 32767.0) as i16;
        o.extend_from_slice(&g.to_le_bytes());
        o.extend_from_slice(&d.to_le_bytes());
    }
    std::fs::write(chemin, o).unwrap();
}

#[test]
fn fichier_decode_par_fenetres_et_plage() {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("stereo.wav");
    ecrire_wav_stereo(&chemin, 12.0);
    let chemin = chemin.to_string_lossy().into_owned();
    let cessions = std::cell::Cell::new(0);
    let f = calculer(&chemin, 400, 0.0, None, Some(12.0), &|| {
        cessions.set(cessions.get() + 1)
    })
    .unwrap();
    assert_eq!(f.voies.len(), 2);
    assert_eq!(f.colonnes, 400);
    assert!((f.duree_s - 12.0).abs() < 0.05, "durée {}", f.duree_s);
    assert!((f.voies[0].crete_dbfs().unwrap() - -6.0).abs() < 0.1);
    assert_eq!(f.voies[0].ecretes, 0);
    assert!(f.voies[1].ecretes > 0);
    assert!(cessions.get() >= 1, "deux fenêtres de 10 s : une cession");

    // Une plage de 2 s au milieu.
    let p = calculer(&chemin, 100, 5.0, Some(2.0), Some(12.0), &|| {}).unwrap();
    assert!((p.duree_s - 2.0).abs() < 0.05, "plage {}", p.duree_s);
    assert_eq!(p.colonnes, 100);

    assert!(calculer("/nulle/part.wav", 100, 0.0, None, None, &|| {}).is_err());
}
