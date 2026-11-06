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
//! * **la coupure spectrale** : une FALAISE dans le spectre de chaque fenêtre
//!   du morceau (#7859, voir [`mesurer_des_tranches`]) — le niveau
//!   chute d'au moins [`FALAISE_DB`] et ne remonte plus jusqu'à Nyquist. Une
//!   pente NATURELLE (l'énergie d'un enregistrement décroît doucement avec la
//!   fréquence) n'est pas une falaise : c'est ce qui sépare un vrai 24/96 au
//!   contenu aigu faible d'un CD suréchantillonné.
//!
//! Idée reprise d'Aède / FlacCompagnon
//! (<https://craft-and-code.github.io/FlacCompagnon/> ; la bibliothèque
//! `flaccompagnon-core` est sous licence MPL-2.0, le dépôt et l'application
//! sous MIT) ; aucune ligne de code n'en est reprise, seulement le principe
//! des deux signatures.
//!
//! # Ce que le verdict ne prétend pas
//!
//! Un hi-res « vérifié » porte du contenu au-dessus de 24 kHz et plus de
//! 16 bits effectifs : cela ne prouve pas l'origine du master. Un « douteux »
//! dit la raison mesurée (bits, coupure) et sa fréquence ; le taux de faux
//! positifs sur une vraie bibliothèque reste à mesurer (#6285, « À vérifier
//! d'abord ») avant de montrer ce badge à l'écran.

use std::ops::Range;
use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};
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

/// #7859 — le nombre de trames FFT jointives moyennées dans UNE fenêtre :
/// assez pour un spectre stable (0,34 s à 96 kHz, 0,74 s à 44,1 kHz), assez
/// peu pour qu'une coupure limitée à un passage ne soit pas diluée.
pub const TRAMES_PAR_FENETRE: usize = 4;

/// #7859 — au plus tant de fenêtres par morceau, réparties sur toute la
/// durée analysée.
pub const FENETRES_MAX: usize = 220;

/// #7859 — la part de fenêtres jugées dont la coupure est suspecte à partir
/// de laquelle le morceau est « douteux ». Un quart : une coupure sur un
/// tiers du morceau suffit, quelques fenêtres isolées (un passage sans aigu,
/// un fondu) ne suffisent pas. À revoir avec le banc de faux positifs sur une
/// vraie bibliothèque que demande l'issue.
pub const PART_COUPEE_MIN: f64 = 0.25;

/// #7859 — une fenêtre dont la coupure est suspecte, « en cause » dans le
/// verdict : sa position dans le morceau et sa coupure.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct FenetreCoupee {
    pub debut_s: f64,
    pub fin_s: f64,
    pub coupure_hz: f64,
}

/// Ce qui a été mesuré sur un fichier.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Mesure {
    pub sample_rate: u32,
    pub bits_declares: u32,
    /// Bits portant de l'information. `None` : signal entièrement nul.
    pub bits_effectifs: Option<u32>,
    /// La coupure RETENUE (#7859) : la médiane des fenêtres en cause quand
    /// elles sont au moins [`PART_COUPEE_MIN`] des fenêtres jugées ; sinon la
    /// médiane des falaises non suspectes (le filtre d'un vrai hi-res), ou
    /// `None` quand aucune fenêtre n'a de falaise. C'est la valeur rangée
    /// dans `hires_verifications.coupure_hz`, et celle que lit [`juger`].
    pub coupure_hz: Option<f64>,
    /// Aucune fenêtre n'était assez forte pour juger le spectre.
    pub silencieux: bool,
    /// #7859 — combien de fenêtres ont été jugées (hors fenêtres silencieuses).
    pub fenetres_jugees: u32,
    /// #7859 — les fenêtres en cause, dans l'ordre du morceau, qu'elles
    /// suffisent ou non à rendre le morceau douteux.
    pub fenetres_coupees: Vec<FenetreCoupee>,
}

impl Mesure {
    /// #7859 — la part des fenêtres jugées qui sont en cause (0 sans fenêtre).
    pub fn part_coupee(&self) -> f64 {
        if self.fenetres_jugees == 0 {
            return 0.0;
        }
        self.fenetres_coupees.len() as f64 / self.fenetres_jugees as f64
    }
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

/// Le calcul des spectres, préparé une fois par mesure : plan FFT, fenêtre de
/// Hann et tampons.
struct Analyseur {
    fft: Arc<dyn RealToComplex<f32>>,
    hann: Vec<f32>,
    entree: Vec<f32>,
    sortie: Vec<Complex<f32>>,
}

impl Analyseur {
    fn new() -> Self {
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(TAILLE_FFT);
        let hann = (0..TAILLE_FFT)
            .map(|i| {
                let x = std::f32::consts::PI * 2.0 * i as f32 / TAILLE_FFT as f32;
                0.5 - 0.5 * x.cos()
            })
            .collect();
        let entree = fft.make_input_vec();
        let sortie = fft.make_output_vec();
        Self {
            fft,
            hann,
            entree,
            sortie,
        }
    }

    /// Spectre de puissance moyen (dB par raie, `TAILLE_FFT / 2 + 1` raies)
    /// des trames jointives `trames` d'un signal mono normalisé à ±1.
    fn spectre_db(&mut self, mono: &[f32], trames: Range<usize>) -> Option<Vec<f64>> {
        if trames.is_empty() || trames.end * TAILLE_FFT > mono.len() {
            return None;
        }
        let mut puissance = vec![0f64; self.sortie.len()];
        for t in trames.clone() {
            let trame = &mono[t * TAILLE_FFT..(t + 1) * TAILLE_FFT];
            for ((e, &s), &w) in self.entree.iter_mut().zip(trame).zip(&self.hann) {
                *e = s * w;
            }
            self.fft.process(&mut self.entree, &mut self.sortie).ok()?;
            for (p, c) in puissance.iter_mut().zip(&self.sortie) {
                *p += (c.re as f64).powi(2) + (c.im as f64).powi(2);
            }
        }
        let norme = (trames.len() as f64) * (TAILLE_FFT as f64).powi(2);
        Some(
            puissance
                .iter()
                .map(|p| 10.0 * (p / norme).max(1e-30).log10())
                .collect(),
        )
    }
}

/// #7859 — les fenêtres d'une tranche de `trames` trames FFT : au plus `max`,
/// chacune de [`TRAMES_PAR_FENETRE`] trames jointives (toutes, si la tranche
/// est plus courte), réparties régulièrement du début à la fin de la tranche.
fn fenetres(trames: usize, max: usize) -> Vec<Range<usize>> {
    if trames == 0 || max == 0 {
        return Vec::new();
    }
    let largeur = TRAMES_PAR_FENETRE.min(trames);
    let n = (trames / largeur).clamp(1, max);
    (0..n)
        .map(|w| {
            let debut = if n == 1 {
                0
            } else {
                w * (trames - largeur) / (n - 1)
            };
            debut..debut + largeur
        })
        .collect()
}

/// Une coupure à `f` Hz est-elle suspecte dans un fichier à `sample_rate` ?
/// Les mêmes seuils que [`juger`] : la bande d'un CD ou d'un 48 kHz dans un
/// hi-res, la coupure d'un codec avec perte dans un 44,1/48 kHz.
pub fn coupure_suspecte(sample_rate: u32, f: f64) -> bool {
    if sample_rate > 48_000 {
        f <= COUPURE_HIRES_MAX_HZ
    } else {
        f < COUPURE_LOSSY_MAX_HZ
    }
}

/// La médiane d'une liste non vide.
fn mediane(mut valeurs: Vec<f64>) -> Option<f64> {
    if valeurs.is_empty() {
        return None;
    }
    valeurs.sort_by(|a, b| a.total_cmp(b));
    Some(valeurs[valeurs.len() / 2])
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
    mesurer_des_tranches(&[(0.0, echantillons)], canaux, sample_rate, bits_declares)
}

/// #7859 — mesure des TRANCHES décodées d'un même morceau, chacune donnée
/// avec sa position de départ en secondes.
///
/// Le spectre n'est plus moyenné sur tout le passage : un spectre moyen
/// masque une coupure limitée à une partie du morceau (un tiers coupé à
/// 20 kHz n'abaisse la moyenne au-dessus de 20 kHz que de quelques dB, loin
/// des [`FALAISE_DB`] d'une falaise). Chaque tranche est découpée en
/// fenêtres de [`TRAMES_PAR_FENETRE`] trames, au plus [`FENETRES_MAX`] pour
/// tout le morceau ; chaque fenêtre a son spectre et sa falaise, avec les
/// mêmes critères qu'avant (une pente naturelle n'est toujours pas une
/// falaise). Les fenêtres trop silencieuses ne sont pas jugées. Le morceau
/// est douteux quand au moins [`PART_COUPEE_MIN`] des fenêtres jugées sont
/// en cause ; [`Mesure::fenetres_coupees`] dit lesquelles.
pub fn mesurer_des_tranches(
    tranches: &[(f64, &[i32])],
    canaux: u32,
    sample_rate: u32,
    bits_declares: u32,
) -> Mesure {
    mesurer_selon(tranches, canaux, sample_rate, bits_declares, fenetres)
}

/// [`mesurer_des_tranches`], avec le découpage en fenêtres donné : les
/// témoins s'en servent pour mesurer comme l'ancien spectre moyen.
fn mesurer_selon(
    tranches: &[(f64, &[i32])],
    canaux: u32,
    sample_rate: u32,
    bits_declares: u32,
    decouper: fn(usize, usize) -> Vec<Range<usize>>,
) -> Mesure {
    let canaux = canaux.max(1) as usize;
    let echelle = 1.0 / (1u64 << bits_declares.clamp(1, 32).saturating_sub(1)) as f32;
    let hz_par_raie = sample_rate as f64 / TAILLE_FFT as f64;
    let demi = ((LISSAGE_HZ / hz_par_raie / 2.0).round() as usize).max(1);
    let s_par_trame = TAILLE_FFT as f64 / sample_rate.max(1) as f64;
    let par_tranche = (FENETRES_MAX / tranches.len().max(1)).max(1);
    let mut analyseur = Analyseur::new();
    let mut bits = None;
    let mut jugees = 0u32;
    let mut coupees = Vec::new();
    let mut falaises_saines = Vec::new();
    for &(depart_s, echantillons) in tranches {
        // Le OU de tous les échantillons de toutes les tranches : le plus
        // grand nombre de bits effectifs d'une tranche est celui du morceau.
        bits = bits.max(bits_effectifs(echantillons, bits_declares));
        let mono: Vec<f32> = echantillons
            .chunks_exact(canaux)
            .map(|trame| trame.iter().map(|&s| s as f32).sum::<f32>() * echelle / canaux as f32)
            .collect();
        for fenetre in decouper(mono.len() / TAILLE_FFT, par_tranche) {
            let Some(db) = analyseur.spectre_db(&mono, fenetre.clone()) else {
                continue;
            };
            let lisse = lisser(&db, demi);
            let niveau = lisse.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            if niveau < SILENCE_DB {
                continue;
            }
            jugees += 1;
            match falaise(&lisse, sample_rate) {
                Some(f) if coupure_suspecte(sample_rate, f) => coupees.push(FenetreCoupee {
                    debut_s: depart_s + fenetre.start as f64 * s_par_trame,
                    fin_s: depart_s + fenetre.end as f64 * s_par_trame,
                    coupure_hz: f,
                }),
                Some(f) => falaises_saines.push(f),
                None => {}
            }
        }
    }
    let mut m = Mesure {
        sample_rate,
        bits_declares,
        bits_effectifs: bits,
        coupure_hz: None,
        silencieux: jugees == 0,
        fenetres_jugees: jugees,
        fenetres_coupees: coupees,
    };
    m.coupure_hz = if !m.fenetres_coupees.is_empty() && m.part_coupee() >= PART_COUPEE_MIN {
        mediane(m.fenetres_coupees.iter().map(|c| c.coupure_hz).collect())
    } else {
        mediane(falaises_saines)
    };
    m
}

/// La coupure qu'aurait vue l'ancien SPECTRE MOYEN (#6285) : une seule
/// fenêtre qui couvre tout le passage. Pour les témoins de #7859.
#[cfg(test)]
pub(crate) fn coupure_du_spectre_moyen(
    echantillons: &[i32],
    canaux: u32,
    sample_rate: u32,
    bits_declares: u32,
) -> Option<f64> {
    let un_seul = |trames: usize, _max: usize| {
        if trames == 0 {
            Vec::new()
        } else {
            std::iter::once(0..trames).collect()
        }
    };
    let m = mesurer_selon(
        &[(0.0, echantillons)],
        canaux,
        sample_rate,
        bits_declares,
        un_seul,
    );
    m.fenetres_coupees
        .first()
        .map(|c| c.coupure_hz)
        .or(m.coupure_hz)
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

#[cfg(test)]
#[path = "faux_hires_tests_7859.rs"]
mod tests_7859;
