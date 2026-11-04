//! Témoins du spectrogramme (#7857).

use super::*;

const SR: u32 = 44_100;
const LARGEUR: u32 = 1200;
const HAUTEUR: u32 = 400;

fn sinus(freq: f64, amplitude: f64, secondes: f64, sr: u32) -> Vec<f32> {
    let n = (secondes * sr as f64) as usize;
    (0..n)
        .map(|i| {
            (amplitude * (2.0 * std::f64::consts::PI * freq * i as f64 / sr as f64).sin()) as f32
        })
        .collect()
}

fn image_de(mono: &[f32], sr: u32) -> RgbImage {
    let zone = disposition(LARGEUR, HAUTEUR);
    let mut acc = Accumulateur::new(sr, zone.hauteur as usize);
    // Par morceaux de taille quelconque : le découpage ne doit rien changer.
    for morceau in mono.chunks(7919) {
        acc.pousser(morceau);
    }
    rendre(&acc.terminer(), LARGEUR, HAUTEUR)
}

fn luminance(p: &Rgb<u8>) -> f64 {
    0.2126 * p[0] as f64 + 0.7152 * p[1] as f64 + 0.0722 * p[2] as f64
}

/// La luminance moyenne de chaque ligne de pixels de la zone du spectre, de
/// haut en bas.
fn luminance_des_lignes(image: &RgbImage) -> Vec<f64> {
    let zone = disposition(image.width(), image.height());
    (0..zone.hauteur)
        .map(|y| {
            (0..zone.largeur)
                .map(|x| luminance(image.get_pixel(zone.x0 + x, zone.y0 + y)))
                .sum::<f64>()
                / zone.largeur as f64
        })
        .collect()
}

/// La ligne de pixels (depuis le haut de la zone) où doit tomber `freq` :
/// 0 Hz en bas, Nyquist en haut, échelle linéaire.
fn ligne_attendue(freq: f64, sr: u32, hauteur_zone: u32) -> i64 {
    let rangee = (freq / (sr as f64 / 2.0) * hauteur_zone as f64).floor() as i64;
    hauteur_zone as i64 - 1 - rangee
}

fn ligne_la_plus_intense(lignes: &[f64]) -> i64 {
    lignes
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(i, _)| i as i64)
        .unwrap()
}

#[test]
fn un_sinus_donne_sa_ligne_la_plus_intense_a_la_bonne_hauteur_7857() {
    let zone = disposition(LARGEUR, HAUTEUR);
    for freq in [1_000.0, 10_000.0, 18_000.0] {
        let image = image_de(&sinus(freq, 0.5, 5.0, SR), SR);
        let lignes = luminance_des_lignes(&image);
        let trouvee = ligne_la_plus_intense(&lignes);
        let attendue = ligne_attendue(freq, SR, zone.hauteur);
        assert!(
            (trouvee - attendue).abs() <= 1,
            "sinus de {freq} Hz : ligne la plus intense {trouvee}, attendue {attendue} \
             (0 Hz en bas, Nyquist en haut)"
        );
    }
}

#[test]
fn une_coupure_a_16_khz_se_voit_7857() {
    // Un « bruit » fait de raies tous les 50 Hz jusqu'à 15 950 Hz, phases
    // pseudo-aléatoires : la signature d'un MP3 coupé à 16 kHz.
    let n = 5 * SR as usize;
    let mut graine: u64 = 0x7857;
    let raies: Vec<(f64, f64)> = (1..=319)
        .map(|k| {
            graine = graine
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let phase = (graine >> 11) as f64 / (1u64 << 53) as f64 * std::f64::consts::TAU;
            (k as f64 * 50.0, phase)
        })
        .collect();
    let mono: Vec<f32> = (0..n)
        .map(|i| {
            let t = i as f64 / SR as f64;
            raies
                .iter()
                .map(|&(f, p)| 0.003 * (std::f64::consts::TAU * f * t + p).sin())
                .sum::<f64>() as f32
        })
        .collect();
    let zone = disposition(LARGEUR, HAUTEUR);
    let lignes = luminance_des_lignes(&image_de(&mono, SR));
    let sous_15_5 = ligne_attendue(15_500.0, SR, zone.hauteur);
    let sur_16_5 = ligne_attendue(16_500.0, SR, zone.hauteur);
    let la_plus_sombre_dessous = lignes[sous_15_5 as usize..]
        .iter()
        .copied()
        .fold(f64::INFINITY, f64::min);
    let la_plus_claire_dessus = lignes[..=sur_16_5 as usize]
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    assert!(
        la_plus_sombre_dessous > 80.0,
        "sous 15,5 kHz, la bande pleine doit être claire : luminance minimale {la_plus_sombre_dessous:.1}"
    );
    assert!(
        la_plus_claire_dessus < 40.0,
        "au-dessus de 16,5 kHz, la bande coupée doit être sombre : luminance maximale {la_plus_claire_dessus:.1}"
    );
}

#[test]
fn la_palette_eclaircit_avec_le_niveau_7857() {
    let mut avant = -1.0;
    for i in 0..=240 {
        let db = PLANCHER_DB + i as f32 * 0.5;
        let l = luminance(&couleur(db));
        assert!(l >= avant - 0.5, "luminance non croissante à {db} dB");
        avant = l;
    }
    assert_eq!(couleur(PLANCHER_DB - 10.0), Rgb([0, 0, 0]));
    assert_eq!(couleur(0.0), Rgb([255, 255, 255]));
}

#[test]
fn la_memoire_reste_bornee_sur_une_longue_piste_7857() {
    let mut acc = Accumulateur::new(8_000, 50);
    // 3 × TRANCHES_MAX trames de 256 échantillons.
    let trame = vec![0.1f32; taille_fft(8_000)];
    for _ in 0..3 * TRANCHES_MAX {
        acc.pousser(&trame);
    }
    let spectre = acc.terminer();
    assert!(spectre.colonnes.len() <= TRANCHES_MAX);
    assert!(spectre.colonnes.len() >= TRANCHES_MAX / 4);
}

#[test]
fn la_taille_demandee_est_bornee_7857() {
    assert_eq!(borner(10, 10), (LARGEUR_MIN, HAUTEUR_MIN));
    assert_eq!(borner(100_000, 100_000), (LARGEUR_MAX, HAUTEUR_MAX));
    let image = image_de(&sinus(440.0, 0.5, 1.0, SR), SR);
    assert_eq!((image.width(), image.height()), (LARGEUR, HAUTEUR));
}

/// Écrit un WAV PCM 16 bits stéréo.
pub(crate) fn ecrire_wav(chemin: &std::path::Path, mono: &[f32], sr: u32) {
    let canaux: u16 = 2;
    let donnees = (mono.len() * canaux as usize * 2) as u32;
    let mut o = Vec::with_capacity(44 + donnees as usize);
    o.extend_from_slice(b"RIFF");
    o.extend_from_slice(&(36 + donnees).to_le_bytes());
    o.extend_from_slice(b"WAVEfmt ");
    o.extend_from_slice(&16u32.to_le_bytes());
    o.extend_from_slice(&1u16.to_le_bytes());
    o.extend_from_slice(&canaux.to_le_bytes());
    o.extend_from_slice(&sr.to_le_bytes());
    o.extend_from_slice(&(sr * canaux as u32 * 2).to_le_bytes());
    o.extend_from_slice(&(canaux * 2).to_le_bytes());
    o.extend_from_slice(&16u16.to_le_bytes());
    o.extend_from_slice(b"data");
    o.extend_from_slice(&donnees.to_le_bytes());
    for &s in mono {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        o.extend_from_slice(&v.to_le_bytes());
        o.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(chemin, o).unwrap();
}

#[test]
fn un_fichier_decode_par_fenetres_garde_son_sinus_a_la_bonne_hauteur_7857() {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("sinus.wav");
    // 25 s : trois fenêtres de décodage.
    ecrire_wav(&chemin, &sinus(5_000.0, 0.5, 25.0, SR), SR);
    let cedees = std::cell::Cell::new(0);
    let png = generer_png(
        chemin.to_str().unwrap(),
        LARGEUR,
        HAUTEUR,
        Some(25.0),
        &|| cedees.set(cedees.get() + 1),
    )
    .unwrap();
    assert!(png.starts_with(b"\x89PNG"));
    let image = image::load_from_memory(&png).unwrap().to_rgb8();
    assert_eq!((image.width(), image.height()), (LARGEUR, HAUTEUR));
    let zone = disposition(LARGEUR, HAUTEUR);
    let lignes = luminance_des_lignes(&image);
    let attendue = ligne_attendue(5_000.0, SR, zone.hauteur);
    assert!((ligne_la_plus_intense(&lignes) - attendue).abs() <= 1);
    // La ligne du sinus est allumée sur toute la durée : aucune fenêtre
    // perdue (la dernière colonne aussi).
    let y = zone.y0 + attendue as u32;
    for x in [0, zone.largeur / 2, zone.largeur - 2] {
        assert!(
            luminance(image.get_pixel(zone.x0 + x, y)) > 150.0,
            "colonne {x} éteinte : une fenêtre de décodage a été perdue"
        );
    }
    assert!(
        cedees.get() >= 2,
        "la génération doit céder entre deux fenêtres"
    );
}

/// Temps de génération d'une piste de 5 min (44,1 kHz, 16 bits, stéréo).
/// `cargo test -p tune-core --release --lib -- --ignored temps_de_generation --nocapture`
#[test]
#[ignore]
fn temps_de_generation_d_une_piste_de_5_min_7857() {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("cinq_minutes.wav");
    ecrire_wav(&chemin, &sinus(3_000.0, 0.3, 300.0, SR), SR);
    let debut = std::time::Instant::now();
    let png = generer_png(
        chemin.to_str().unwrap(),
        LARGEUR_DEFAUT,
        HAUTEUR_DEFAUT,
        Some(300.0),
        &|| {},
    )
    .unwrap();
    println!(
        "spectrogramme 5 min 44,1 kHz : {:?}, {} octets",
        debut.elapsed(),
        png.len()
    );
}
