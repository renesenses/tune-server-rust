//! Le spectrogramme d'une piste, en image PNG (#7857, go de Bertrand du
//! 04/11/2026).
//!
//! Calculé en Rust natif sur le PCM que Tune décode lui-même (jamais ffmpeg) :
//! FFT `realfft`, fenêtre de Hann, trames jointives. L'axe des fréquences est
//! LINÉAIRE de 0 Hz (en bas) à Nyquist (en haut) ; la couleur dit le niveau en
//! dBFS, de [`PLANCHER_DB`] (noir) à 0 dBFS (blanc). Les marges portent les
//! graduations du temps et des fréquences, écrites avec une petite police
//! bitmap intégrée : aucune dépendance de police.
//!
//! La mémoire reste bornée quelle que soit la durée : la piste est décodée par
//! fenêtres de [`FENETRE_S`] secondes, et chaque trame n'est gardée que
//! réduite à une colonne de `hauteur` niveaux. Au-delà de [`TRANCHES_MAX`]
//! colonnes, deux voisines fusionnent (maximum par rangée).
//!
//! Réécriture : Aède, Phémios et FlacCompagnon servent de référence de
//! comportement seulement ; aucune ligne de leur code n'est reprise, et la
//! crate `flaccompagnon-spectrogram` n'est pas une dépendance.

use image::{ImageBuffer, Rgb, RgbImage};
use realfft::{RealFftPlanner, RealToComplex};
use std::sync::Arc;

/// Le niveau le plus bas représenté, en dBFS : noir en dessous.
pub const PLANCHER_DB: f32 = -120.0;

/// Durée de chaque fenêtre décodée, en secondes.
pub const FENETRE_S: f64 = 10.0;

/// Nombre de colonnes gardées avant de fusionner deux voisines.
pub const TRANCHES_MAX: usize = 8192;

/// Bornes et défauts de la taille de l'image, marges comprises.
pub const LARGEUR_MIN: u32 = 200;
pub const LARGEUR_MAX: u32 = 4000;
pub const HAUTEUR_MIN: u32 = 100;
pub const HAUTEUR_MAX: u32 = 2000;
pub const LARGEUR_DEFAUT: u32 = 1200;
pub const HAUTEUR_DEFAUT: u32 = 400;

const MARGE_GAUCHE: u32 = 44;
const MARGE_DROITE: u32 = 8;
const MARGE_HAUT: u32 = 8;
const MARGE_BAS: u32 = 22;

const FOND: Rgb<u8> = Rgb([24, 24, 24]);
const ENCRE: Rgb<u8> = Rgb([200, 200, 200]);

/// Où se trouve la zone du spectre dans l'image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Disposition {
    pub x0: u32,
    pub y0: u32,
    pub largeur: u32,
    pub hauteur: u32,
}

/// La zone du spectre d'une image de `largeur` × `hauteur` (bornées).
pub fn disposition(largeur: u32, hauteur: u32) -> Disposition {
    let (largeur, hauteur) = borner(largeur, hauteur);
    Disposition {
        x0: MARGE_GAUCHE,
        y0: MARGE_HAUT,
        largeur: largeur - MARGE_GAUCHE - MARGE_DROITE,
        hauteur: hauteur - MARGE_HAUT - MARGE_BAS,
    }
}

/// Ramène une taille demandée dans les bornes.
pub fn borner(largeur: u32, hauteur: u32) -> (u32, u32) {
    (
        largeur.clamp(LARGEUR_MIN, LARGEUR_MAX),
        hauteur.clamp(HAUTEUR_MIN, HAUTEUR_MAX),
    )
}

/// Taille de la FFT pour une fréquence d'échantillonnage : une trentaine de
/// trames par seconde, quelle que soit la fréquence (2048 à 44,1/48 kHz).
pub fn taille_fft(sample_rate: u32) -> usize {
    ((sample_rate / 32) as usize).next_power_of_two().max(256)
}

/// Les puissances (linéaires, 1,0 = sinus pleine échelle) d'un signal, une
/// colonne par tranche de temps, `rangees` niveaux par colonne (rangée 0 =
/// 0 Hz).
pub struct Spectre {
    pub sample_rate: u32,
    pub rangees: usize,
    pub colonnes: Vec<Vec<f32>>,
    pub duree_s: f64,
}

/// Accumule le spectre d'un signal mono poussé morceau par morceau.
pub struct Accumulateur {
    sample_rate: u32,
    rangees: usize,
    fft: Arc<dyn RealToComplex<f32>>,
    hann: Vec<f32>,
    /// Pour chaque rangée, les raies `[début, fin)` qu'elle couvre.
    raies_par_rangee: Vec<(usize, usize)>,
    norme: f32,
    reste: Vec<f32>,
    entree: Vec<f32>,
    sortie: Vec<realfft::num_complex::Complex<f32>>,
    tranches: Vec<Vec<f32>>,
    /// Trames fusionnées par tranche (puissance de 2).
    trames_par_tranche: usize,
    /// La tranche en cours de remplissage et son nombre de trames.
    courante: Option<(Vec<f32>, usize)>,
    echantillons: u64,
}

impl Accumulateur {
    pub fn new(sample_rate: u32, rangees: usize) -> Self {
        let sample_rate = sample_rate.max(1);
        let rangees = rangees.max(1);
        let n = taille_fft(sample_rate);
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(n);
        let hann: Vec<f32> = (0..n)
            .map(|i| {
                let x = std::f32::consts::PI * 2.0 * i as f32 / n as f32;
                0.5 - 0.5 * x.cos()
            })
            .collect();
        // Un sinus d'amplitude A donne une raie de module A·Σw/2 : la norme
        // ramène ce sinus à une puissance A².
        let somme: f32 = hann.iter().sum();
        let norme = (2.0 / somme).powi(2);
        let raies = n / 2 + 1;
        let hz_par_raie = sample_rate as f64 / n as f64;
        let nyquist = sample_rate as f64 / 2.0;
        let raies_par_rangee = (0..rangees)
            .map(|r| {
                let bas = r as f64 * nyquist / rangees as f64;
                let haut = (r + 1) as f64 * nyquist / rangees as f64;
                let debut = ((bas / hz_par_raie).ceil() as usize).min(raies - 1);
                let fin = ((haut / hz_par_raie).ceil() as usize).min(raies);
                if fin > debut {
                    (debut, fin)
                } else {
                    // Rangée plus fine qu'une raie : la raie la plus proche
                    // de son centre.
                    let centre =
                        (((bas + haut) / 2.0 / hz_par_raie).round() as usize).min(raies - 1);
                    (centre, centre + 1)
                }
            })
            .collect();
        Self {
            sample_rate,
            rangees,
            entree: fft.make_input_vec(),
            sortie: fft.make_output_vec(),
            fft,
            hann,
            raies_par_rangee,
            norme,
            reste: Vec::with_capacity(n),
            tranches: Vec::new(),
            trames_par_tranche: 1,
            courante: None,
            echantillons: 0,
        }
    }

    /// Ajoute des échantillons mono normalisés à ±1.
    pub fn pousser(&mut self, mono: &[f32]) {
        self.echantillons += mono.len() as u64;
        let n = self.hann.len();
        let mut reste = std::mem::take(&mut self.reste);
        let mut source = mono;
        if !reste.is_empty() {
            let manque = n - reste.len();
            let pris = manque.min(source.len());
            reste.extend_from_slice(&source[..pris]);
            source = &source[pris..];
            if reste.len() == n {
                self.trame(&reste);
                reste.clear();
            }
        }
        let mut trames = source.chunks_exact(n);
        for trame in trames.by_ref() {
            self.trame(trame);
        }
        reste.extend_from_slice(trames.remainder());
        self.reste = reste;
    }

    fn trame(&mut self, trame: &[f32]) {
        for ((e, &s), &w) in self.entree.iter_mut().zip(trame).zip(&self.hann) {
            *e = s * w;
        }
        if self
            .fft
            .process(&mut self.entree, &mut self.sortie)
            .is_err()
        {
            return;
        }
        let colonne: Vec<f32> = self
            .raies_par_rangee
            .iter()
            .map(|&(debut, fin)| {
                self.sortie[debut..fin]
                    .iter()
                    .map(|c| c.norm_sqr() * self.norme)
                    .fold(0.0, f32::max)
            })
            .collect();
        self.ajouter_colonne(colonne);
    }

    fn ajouter_colonne(&mut self, colonne: Vec<f32>) {
        let (cumul, trames) = match self.courante.take() {
            None => (colonne, 1),
            Some((mut cumul, trames)) => {
                max_en_place(&mut cumul, &colonne);
                (cumul, trames + 1)
            }
        };
        if trames < self.trames_par_tranche {
            self.courante = Some((cumul, trames));
            return;
        }
        self.tranches.push(cumul);
        if self.tranches.len() >= TRANCHES_MAX {
            // Deux voisines n'en font plus qu'une : la mémoire reste bornée,
            // la résolution en temps est divisée par deux.
            let anciennes = std::mem::take(&mut self.tranches);
            let mut paires = anciennes.chunks_exact(2);
            for paire in paires.by_ref() {
                let mut a = paire[0].clone();
                max_en_place(&mut a, &paire[1]);
                self.tranches.push(a);
            }
            self.tranches.extend(paires.remainder().iter().cloned());
            self.trames_par_tranche *= 2;
        }
    }

    /// Le spectre accumulé. Une trame incomplète en fin de signal est
    /// complétée de silence.
    pub fn terminer(mut self) -> Spectre {
        if !self.reste.is_empty() {
            let mut derniere = std::mem::take(&mut self.reste);
            derniere.resize(self.hann.len(), 0.0);
            self.trame(&derniere);
        }
        if let Some((cumul, _)) = self.courante.take() {
            self.tranches.push(cumul);
        }
        Spectre {
            sample_rate: self.sample_rate,
            rangees: self.rangees,
            colonnes: self.tranches,
            duree_s: self.echantillons as f64 / self.sample_rate as f64,
        }
    }
}

fn max_en_place(a: &mut [f32], b: &[f32]) {
    for (x, &y) in a.iter_mut().zip(b) {
        *x = x.max(y);
    }
}

/// Les échantillons entrelacés de `canaux` voies, sur `bits` bits, ramenés à
/// un signal mono normalisé à ±1.
pub fn en_mono(echantillons: &[i32], canaux: u32, bits: u32) -> Vec<f32> {
    let canaux = canaux.max(1) as usize;
    let echelle = 1.0 / (1u64 << bits.clamp(1, 32).saturating_sub(1)) as f32;
    echantillons
        .chunks_exact(canaux)
        .map(|trame| trame.iter().map(|&s| s as f32).sum::<f32>() * echelle / canaux as f32)
        .collect()
}

/// La couleur d'un niveau en dBFS. La luminance croît avec le niveau : du
/// noir (plancher) au blanc (0 dBFS), par le bleu, le violet, le rouge,
/// l'orange et le jaune.
pub fn couleur(db: f32) -> Rgb<u8> {
    const PALETTE: [[f32; 3]; 7] = [
        [0.0, 0.0, 0.0],
        [20.0, 10.0, 80.0],
        [110.0, 20.0, 140.0],
        [210.0, 50.0, 70.0],
        [250.0, 140.0, 20.0],
        [255.0, 230.0, 80.0],
        [255.0, 255.0, 255.0],
    ];
    let t = ((db - PLANCHER_DB) / -PLANCHER_DB).clamp(0.0, 1.0) * (PALETTE.len() - 1) as f32;
    let i = (t.floor() as usize).min(PALETTE.len() - 2);
    let f = t - i as f32;
    let a = PALETTE[i];
    let b = PALETTE[i + 1];
    Rgb([
        (a[0] + (b[0] - a[0]) * f).round() as u8,
        (a[1] + (b[1] - a[1]) * f).round() as u8,
        (a[2] + (b[2] - a[2]) * f).round() as u8,
    ])
}

/// Dessine le spectre dans une image de `largeur` × `hauteur` (bornées),
/// marges et graduations comprises. Le spectre doit avoir été accumulé avec
/// autant de rangées que la hauteur de [`disposition`].
pub fn rendre(spectre: &Spectre, largeur: u32, hauteur: u32) -> RgbImage {
    let (largeur, hauteur) = borner(largeur, hauteur);
    let zone = disposition(largeur, hauteur);
    let mut image: RgbImage = ImageBuffer::from_pixel(largeur, hauteur, FOND);
    let colonnes = spectre.colonnes.len();
    if colonnes > 0 {
        for x in 0..zone.largeur {
            // Les tranches qui tombent dans cette colonne de pixels ; au moins
            // une, quand il y a moins de tranches que de pixels.
            let debut = x as usize * colonnes / zone.largeur as usize;
            let fin = ((x as usize + 1) * colonnes / zone.largeur as usize)
                .max(debut + 1)
                .min(colonnes);
            for y in 0..zone.hauteur {
                // Rangée 0 (0 Hz) en bas.
                let r = ((zone.hauteur - 1 - y) as usize * spectre.rangees / zone.hauteur as usize)
                    .min(spectre.rangees - 1);
                let p = spectre.colonnes[debut..fin]
                    .iter()
                    .map(|c| c[r])
                    .fold(0.0, f32::max);
                let db = 10.0 * p.max(1e-30).log10();
                image.put_pixel(zone.x0 + x, zone.y0 + y, couleur(db));
            }
        }
    }
    graduer_les_frequences(&mut image, zone, spectre.sample_rate);
    graduer_le_temps(&mut image, zone, spectre.duree_s);
    image
}

/// Le pas de graduation le plus fin de `pas` qui laisse au moins `ecart`
/// pixels entre deux graduations.
fn choisir_le_pas(etendue: f64, pixels: u32, ecart: u32, pas: &[f64]) -> f64 {
    let max_graduations = (pixels / ecart).max(1) as f64;
    pas.iter()
        .copied()
        .find(|p| etendue / p <= max_graduations)
        .unwrap_or(*pas.last().unwrap_or(&etendue))
}

fn graduer_les_frequences(image: &mut RgbImage, zone: Disposition, sample_rate: u32) {
    let nyquist_khz = sample_rate as f64 / 2000.0;
    let pas = choisir_le_pas(
        nyquist_khz,
        zone.hauteur,
        24,
        &[1.0, 2.0, 5.0, 10.0, 20.0, 25.0, 50.0, 100.0],
    );
    let mut khz = 0.0;
    while khz <= nyquist_khz + 1e-9 {
        let y = zone.y0 + zone.hauteur
            - 1
            - ((khz / nyquist_khz) * (zone.hauteur - 1) as f64).round() as u32;
        for x in zone.x0 - 4..zone.x0 {
            image.put_pixel(x, y, ENCRE);
        }
        let texte = if khz == 0.0 {
            "0".to_string()
        } else {
            format!("{}k", khz as u32)
        };
        let l = largeur_du_texte(&texte);
        let ty = (y as i64 - 5).clamp(0, image.height() as i64 - 10) as u32;
        ecrire(image, zone.x0.saturating_sub(6 + l), ty, &texte);
        khz += pas;
    }
    ecrire(image, 2, zone.y0 + zone.hauteur + 10, "Hz");
}

fn graduer_le_temps(image: &mut RgbImage, zone: Disposition, duree_s: f64) {
    if duree_s <= 0.0 {
        return;
    }
    let pas = choisir_le_pas(
        duree_s,
        zone.largeur,
        70,
        &[
            1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0,
        ],
    );
    let mut t = 0.0;
    let y = zone.y0 + zone.hauteur;
    while t <= duree_s + 1e-9 {
        let x = zone.x0 + ((t / duree_s) * (zone.largeur - 1) as f64).round() as u32;
        for dy in 0..4 {
            image.put_pixel(x, y + dy, ENCRE);
        }
        let texte = if duree_s < 60.0 {
            format!("{}s", t as u32)
        } else {
            format!("{}:{:02}", t as u32 / 60, t as u32 % 60)
        };
        let l = largeur_du_texte(&texte);
        let tx = (x as i64 - l as i64 / 2).clamp(0, (image.width() - l) as i64) as u32;
        ecrire(image, tx, y + 6, &texte);
        t += pas;
    }
}

/// Les glyphes de la police : 3 × 5 points, une ligne par quintet de bits.
fn glyphe(c: char) -> [u8; 5] {
    match c {
        '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        '6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        '7' => [0b111, 0b001, 0b001, 0b001, 0b001],
        '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        'k' => [0b100, 0b101, 0b110, 0b101, 0b101],
        'H' => [0b101, 0b101, 0b111, 0b101, 0b101],
        'z' => [0b111, 0b001, 0b010, 0b100, 0b111],
        's' => [0b011, 0b100, 0b010, 0b001, 0b110],
        ':' => [0b000, 0b010, 0b000, 0b010, 0b000],
        _ => [0; 5],
    }
}

/// Points de 2 × 2 pixels, un point d'écart entre deux glyphes.
const POINT: u32 = 2;

fn largeur_du_texte(texte: &str) -> u32 {
    let n = texte.chars().count() as u32;
    if n == 0 { 0 } else { n * 4 * POINT - POINT }
}

fn ecrire(image: &mut RgbImage, x: u32, y: u32, texte: &str) {
    for (i, c) in texte.chars().enumerate() {
        let gx = x + i as u32 * 4 * POINT;
        for (ligne, bits) in glyphe(c).iter().enumerate() {
            for col in 0..3u32 {
                if bits & (0b100 >> col) == 0 {
                    continue;
                }
                for dy in 0..POINT {
                    for dx in 0..POINT {
                        let px = gx + col * POINT + dx;
                        let py = y + ligne as u32 * POINT + dy;
                        if px < image.width() && py < image.height() {
                            image.put_pixel(px, py, ENCRE);
                        }
                    }
                }
            }
        }
    }
}

/// Encode une image en PNG.
pub fn en_png(image: &RgbImage) -> Result<Vec<u8>, String> {
    let mut octets = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut octets, image::ImageFormat::Png)
        .map_err(|e| format!("png: {e}"))?;
    Ok(octets.into_inner())
}

/// Décode un fichier par fenêtres et rend son spectrogramme en PNG.
///
/// Bloquant : à appeler hors de l'exécuteur asynchrone. `ceder` est appelé
/// entre deux fenêtres : c'est là que la génération laisse passer la lecture.
/// `duree_s`, quand elle est connue, borne le nombre de fenêtres (garde
/// contre un décodeur qui ignorerait la position demandée).
pub fn generer_png(
    chemin: &str,
    largeur: u32,
    hauteur: u32,
    duree_s: Option<f64>,
    ceder: &dyn Fn(),
) -> Result<Vec<u8>, String> {
    let zone = disposition(largeur, hauteur);
    let fenetres_max = match duree_s {
        Some(d) if d > 0.0 => (d / FENETRE_S).ceil() as usize + 1,
        // Six heures au plus sans durée connue.
        _ => (6.0 * 3600.0 / FENETRE_S) as usize,
    };
    let mut accumulateur: Option<Accumulateur> = None;
    for i in 0..fenetres_max {
        if i > 0 {
            ceder();
        }
        let depart = i as f64 * FENETRE_S;
        let pcm = match crate::audio::decode::decode_to_pcm(chemin, None, None, depart, FENETRE_S) {
            Ok(pcm) => pcm,
            // Après la première fenêtre, une erreur est la fin du fichier.
            Err(_) if accumulateur.is_some() => break,
            Err(e) => return Err(e),
        };
        let mono = en_mono(&pcm.samples_i32, pcm.channels, pcm.bit_depth as u32);
        if mono.is_empty() {
            break;
        }
        let acc = accumulateur
            .get_or_insert_with(|| Accumulateur::new(pcm.sample_rate, zone.hauteur as usize));
        acc.pousser(&mono);
        // Une fenêtre incomplète est la dernière.
        if (mono.len() as f64) < FENETRE_S * pcm.sample_rate as f64 * 0.99 {
            break;
        }
    }
    let Some(accumulateur) = accumulateur else {
        return Err("aucun échantillon décodé".to_string());
    };
    en_png(&rendre(&accumulateur.terminer(), largeur, hauteur))
}

#[cfg(test)]
#[path = "spectrogramme_tests_7857.rs"]
mod tests;
