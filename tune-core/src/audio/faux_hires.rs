//! Détection des FAUX hi-res (#6285, go de Bertrand du 20/10/2026).
//!
//! Un fichier se dit 24 bits / 96 kHz ; ce qu'il porte peut être un CD
//! (16 bits, bande coupée vers 20-22 kHz) gonflé par un convertisseur, ou un
//! MP3/AAC décodé puis réencodé en FLAC (bande coupée à 16-20 kHz). Deux
//! signatures se mesurent sur le PCM décodé, en Rust natif (FFT `realfft`,
//! jamais ffmpeg) :
//!
//! * **les bits effectifs** : si les bits de poids faible de TOUS les
//!   échantillons sont nuls, le fichier n'en porte que `déclarés - nuls` ;
//! * **la coupure spectrale** : une FALAISE dans le spectre moyen — le niveau
//!   chute d'au moins [`FALAISE_DB`] et ne remonte plus jusqu'à Nyquist. Une
//!   pente NATURELLE (l'énergie d'un enregistrement décroît doucement avec la
//!   fréquence) n'est pas une falaise : c'est ce qui sépare un vrai 24/96 au
//!   contenu aigu faible d'un CD suréchantillonné.
//!
//! Idée reprise d'Aède / FlacCompagnon
//! (<https://craft-and-code.github.io/FlacCompagnon/>, licence MIT) ; aucune
//! ligne de code n'en est reprise, seulement le principe des deux signatures.
//!
//! # Ce que le verdict ne prétend pas
//!
//! Un hi-res « vérifié » porte du contenu au-dessus de 24 kHz et plus de
//! 16 bits effectifs : cela ne prouve pas l'origine du master. Un « douteux »
//! dit la raison mesurée (bits, coupure) et sa fréquence ; le taux de faux
//! positifs sur une vraie bibliothèque reste à mesurer (#6285, « À vérifier
//! d'abord ») avant de montrer ce badge à l'écran.

use realfft::RealFftPlanner;
use serde::Serialize;

/// Taille de la FFT. À 96 kHz, une raie tous les 11,7 Hz.
pub const TAILLE_FFT: usize = 8192;

/// La chute minimale, en dB, qui fait une falaise.
pub const FALAISE_DB: f64 = 30.0;

/// Le bord d'une falaise : là où le niveau est descendu de tant de dB sous la
/// médiane de la bande passante.
const BORD_DB: f64 = 20.0;

/// Largeur de la marche de part et d'autre d'une falaise candidate, en Hz.
const MARCHE_HZ: f64 = 500.0;

/// Largeur du lissage du spectre, en Hz.
const LISSAGE_HZ: f64 = 200.0;

/// Sous ce niveau moyen (dBFS par raie), le passage est trop silencieux pour
/// conclure.
const SILENCE_DB: f64 = -150.0;

/// Un 44,1/48 kHz coupé sous cette fréquence porte la signature d'un codec
/// avec perte (MP3 à 128 kbit/s coupe vers 16 kHz, à 320 vers 20 kHz).
pub const COUPURE_LOSSY_MAX_HZ: f64 = 20_500.0;

/// Un « hi-res » (plus de 48 kHz) coupé sous cette fréquence n'a jamais eu
/// plus que la bande d'un CD ou d'un 48 kHz.
pub const COUPURE_HIRES_MAX_HZ: f64 = 24_000.0;

/// Ce qui a été mesuré sur un fichier.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Mesure {
    pub sample_rate: u32,
    pub bits_declares: u32,
    /// Bits portant de l'information. `None` : signal entièrement nul.
    pub bits_effectifs: Option<u32>,
    /// Fréquence de la falaise spectrale, `None` quand le spectre n'en a pas.
    pub coupure_hz: Option<f64>,
    /// Le passage analysé était trop silencieux pour juger le spectre.
    pub silencieux: bool,
}

/// La raison d'un « douteux ».
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(tag = "raison", rename_all = "snake_case")]
pub enum Raison {
    /// Les bits de poids faible sont toujours nuls : un 24 bits qui n'en porte
    /// que `effectifs`.
    BitsDeRemplissage { effectifs: u32 },
    /// Un hi-res dont la bande s'arrête à celle d'un CD : suréchantillonné.
    BandeDeCd { coupure_hz: f64 },
    /// Une coupure de codec avec perte (MP3, AAC) dans un fichier sans perte.
    CoupureDeCodec { coupure_hz: f64 },
}

/// Le verdict d'une piste.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    /// Se dit hi-res, et l'est : contenu au-dessus de 24 kHz ou bits au-delà
    /// de 16, sans signature contraire.
    Verifie,
    /// Qualité CD déclarée et rien de suspect : pas de badge.
    Conforme,
    /// Une signature contredit ce que le fichier déclare.
    Douteux(Raison),
    /// Rien à juger (silence, signal nul).
    NonConcluant,
}

impl Verdict {
    /// Le mot stocké en base et publié par les routes.
    pub fn code(&self) -> &'static str {
        match self {
            Verdict::Verifie => "verifie",
            Verdict::Conforme => "conforme",
            Verdict::Douteux(_) => "douteux",
            Verdict::NonConcluant => "non_concluant",
        }
    }

    /// La raison d'un douteux, en un mot.
    pub fn code_raison(&self) -> Option<&'static str> {
        match self {
            Verdict::Douteux(Raison::BitsDeRemplissage { .. }) => Some("bits_de_remplissage"),
            Verdict::Douteux(Raison::BandeDeCd { .. }) => Some("bande_de_cd"),
            Verdict::Douteux(Raison::CoupureDeCodec { .. }) => Some("coupure_de_codec"),
            _ => None,
        }
    }
}

/// Bits effectifs d'échantillons entiers déclarés sur `bits_declares` bits :
/// les bits de poids faible nuls dans TOUS les échantillons ne portent rien.
pub fn bits_effectifs(echantillons: &[i32], bits_declares: u32) -> Option<u32> {
    let ou = echantillons.iter().fold(0u32, |acc, &s| acc | s as u32);
    if ou == 0 {
        return None;
    }
    Some(bits_declares.saturating_sub(ou.trailing_zeros()))
}

/// Spectre de puissance moyen (dB par raie, `TAILLE_FFT / 2 + 1` raies) d'un
/// signal mono normalisé à ±1, fenêtre de Hann, trames jointives.
fn spectre_moyen_db(mono: &[f32]) -> Option<Vec<f64>> {
    let trames = mono.len() / TAILLE_FFT;
    if trames == 0 {
        return None;
    }
    let mut planificateur = RealFftPlanner::<f32>::new();
    let fft = planificateur.plan_fft_forward(TAILLE_FFT);
    let hann: Vec<f32> = (0..TAILLE_FFT)
        .map(|i| {
            let x = std::f32::consts::PI * 2.0 * i as f32 / TAILLE_FFT as f32;
            0.5 - 0.5 * x.cos()
        })
        .collect();
    let mut entree = fft.make_input_vec();
    let mut sortie = fft.make_output_vec();
    let mut puissance = vec![0f64; sortie.len()];
    for t in 0..trames {
        let trame = &mono[t * TAILLE_FFT..(t + 1) * TAILLE_FFT];
        for ((e, &s), &w) in entree.iter_mut().zip(trame).zip(&hann) {
            *e = s * w;
        }
        fft.process(&mut entree, &mut sortie).ok()?;
        for (p, c) in puissance.iter_mut().zip(&sortie) {
            *p += (c.re as f64).powi(2) + (c.im as f64).powi(2);
        }
    }
    let norme = (trames as f64) * (TAILLE_FFT as f64).powi(2);
    Some(
        puissance
            .iter()
            .map(|p| 10.0 * (p / norme).max(1e-30).log10())
            .collect(),
    )
}

/// Lissage par moyenne glissante de la puissance (pas des dB), `demi` raies
/// de part et d'autre.
fn lisser(db: &[f64], demi: usize) -> Vec<f64> {
    let p: Vec<f64> = db.iter().map(|d| 10f64.powf(d / 10.0)).collect();
    let mut cumul = vec![0f64; p.len() + 1];
    for (i, v) in p.iter().enumerate() {
        cumul[i + 1] = cumul[i] + v;
    }
    (0..p.len())
        .map(|i| {
            let a = i.saturating_sub(demi);
            let b = (i + demi + 1).min(p.len());
            let m = (cumul[b] - cumul[a]) / (b - a) as f64;
            10.0 * m.max(1e-30).log10()
        })
        .collect()
}

/// La falaise la plus HAUTE du spectre : la fréquence où le niveau, mesuré
/// juste en dessous, dépasse d'au moins [`FALAISE_DB`] TOUT ce qui suit
/// jusqu'à Nyquist. `None` sans falaise (pente naturelle, bande pleine).
fn falaise(db_lisse: &[f64], sample_rate: u32) -> Option<f64> {
    let n = db_lisse.len();
    let hz_par_raie = sample_rate as f64 / TAILLE_FFT as f64;
    let marche = ((MARCHE_HZ / hz_par_raie).round() as usize).max(1);
    // Le maximum de tout ce qui suit chaque raie, calculé une fois.
    let mut max_apres = vec![f64::NEG_INFINITY; n + 1];
    for i in (0..n).rev() {
        max_apres[i] = max_apres[i + 1].max(db_lisse[i]);
    }
    // On ignore le très grave (sous 1 kHz) : une falaise y serait un signal
    // pathologique, pas une coupure de bande.
    let debut = ((1000.0 / hz_par_raie) as usize).max(2 * marche);
    let mut trouvee = None;
    for i in debut..n.saturating_sub(marche) {
        let dessous = db_lisse[i - 2 * marche..i - marche]
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max);
        if dessous - max_apres[i + marche] >= FALAISE_DB {
            trouvee = Some(i);
        }
    }
    let i = trouvee?;
    // Le bord. `i` est la falaise la plus HAUTE, mais sous un plancher très
    // bas (24 bits) la jupe de fuite de la fenêtre de Hann reste plus de
    // `FALAISE_DB` au-dessus du plancher bien au-delà du bord : `i` peut être
    // loin dans la jupe. La référence est donc la bande PASSANTE (médiane des
    // niveaux entre 1 kHz et la falaise), et le bord la plus haute raie qui
    // reste à moins de `BORD_DB` d'elle.
    let mut bande: Vec<f64> = db_lisse[debut..i.saturating_sub(2 * marche).max(debut + 1)].to_vec();
    bande.sort_by(|a, b| a.total_cmp(b));
    let reference = bande[bande.len() / 2];
    let bord = (debut..=i)
        .rev()
        .find(|&k| db_lisse[k] >= reference - BORD_DB)
        .map_or(i, |k| k + 1);
    Some(bord as f64 * hz_par_raie)
}

/// Mesure un passage décodé : échantillons entrelacés, à leur profondeur
/// d'origine (`bits_declares`), comme les rend `decode::decode_to_pcm`.
pub fn mesurer(echantillons: &[i32], canaux: u32, sample_rate: u32, bits_declares: u32) -> Mesure {
    let canaux = canaux.max(1) as usize;
    let echelle = 1.0 / (1u64 << bits_declares.clamp(1, 32).saturating_sub(1)) as f32;
    let mono: Vec<f32> = echantillons
        .chunks_exact(canaux)
        .map(|trame| trame.iter().map(|&s| s as f32).sum::<f32>() * echelle / canaux as f32)
        .collect();
    let bits = bits_effectifs(echantillons, bits_declares);
    let spectre = spectre_moyen_db(&mono);
    let (coupure_hz, silencieux) = match spectre {
        None => (None, true),
        Some(db) => {
            let hz_par_raie = sample_rate as f64 / TAILLE_FFT as f64;
            let demi = ((LISSAGE_HZ / hz_par_raie / 2.0).round() as usize).max(1);
            let lisse = lisser(&db, demi);
            let niveau = lisse.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            if niveau < SILENCE_DB {
                (None, true)
            } else {
                (falaise(&lisse, sample_rate), false)
            }
        }
    };
    Mesure {
        sample_rate,
        bits_declares,
        bits_effectifs: bits,
        coupure_hz,
        silencieux,
    }
}

/// Le verdict à partir de la mesure.
pub fn juger(m: &Mesure) -> Verdict {
    let Some(bits) = m.bits_effectifs else {
        return Verdict::NonConcluant;
    };
    let se_dit_hires = m.sample_rate > 48_000 || m.bits_declares > 16;
    if m.bits_declares > 16 && bits <= 16 {
        return Verdict::Douteux(Raison::BitsDeRemplissage { effectifs: bits });
    }
    if m.silencieux {
        return Verdict::NonConcluant;
    }
    match m.coupure_hz {
        Some(f) if m.sample_rate > 48_000 && f <= COUPURE_HIRES_MAX_HZ => {
            // Sous la bande d'un codec : la cause la plus probable est une
            // source avec perte, quelle que soit la fréquence du conteneur.
            if f < COUPURE_LOSSY_MAX_HZ - 500.0 {
                Verdict::Douteux(Raison::CoupureDeCodec { coupure_hz: f })
            } else {
                Verdict::Douteux(Raison::BandeDeCd { coupure_hz: f })
            }
        }
        Some(f) if m.sample_rate <= 48_000 && f < COUPURE_LOSSY_MAX_HZ => {
            Verdict::Douteux(Raison::CoupureDeCodec { coupure_hz: f })
        }
        _ if se_dit_hires => Verdict::Verifie,
        _ => Verdict::Conforme,
    }
}

#[cfg(test)]
#[path = "faux_hires_tests_6285.rs"]
pub(crate) mod tests;
