//! C3 (#7632, TESS T10b) : FIR de correction de pièce calculé depuis la
//! MESURE, et coût de la convolution dans `tune-core/src/audio/convolver.rs`.
//!
//! - [`concevoir_fir`] : le FIR de correction d'une réponse mesurée (module
//!   lissé, et réponse impulsionnelle pour la phase), vers une cible
//!   ([`TargetCurve`]). Tout est calculé en `f64`, de la mesure au fichier
//!   ([`crate::wav`]) :
//!   * **module** : `cible + décalage − mesure lissée` (lissage 1/6 d'octave
//!     sous 300 Hz, 1/3 au-dessus, fondu d'une octave entre les deux), coupes
//!     bornées à `coupe_max_db`, relance bornée à `relance_max_db` (+6 dB par
//!     défaut : un creux d'annulation ne se comble pas, il coûte de la
//!     réserve), dans la bande `[f_lo, f_hi]` (20 Hz–15 kHz par défaut),
//!     retour à 0 dB en dehors par un fondu en cosinus d'une demi-octave ;
//!     normalisé par défaut pour que le gain le plus haut du filtre soit 0 dB
//!     (aucun écrêtage ajouté par le filtre) ; deux passes de raffinement
//!     ([`RAFFINEMENTS`]) reprennent l'écart entre la réponse prévue
//!     (mesure + filtre, même lissage) et la cible ;
//!   * **phase minimale** pour ce module (cepstre réel, ci-dessous) : aucune
//!     énergie avant le pic, latence nulle ;
//!   * **phase excédentaire** (facultative, [`OptionsPhase`]) : sous la
//!     fréquence de bascule (400 Hz par défaut), la phase de la mesure qui
//!     n'est PAS expliquée par son module (passe-tout d'un filtre de
//!     raccordement, retard d'un caisson) est retournée, lissée au 1/6
//!     d'octave. Sa réponse est anti-causale : elle est fenêtrée sur
//!     ±`pre_tintement_ms` (20 ms par défaut ; plateau sur la moitié, demi-Hann
//!     ensuite) puis retardée d'autant. **Le pré-tintement est donc borné par
//!     construction à `pre_tintement_ms`, et la latence du filtre vaut
//!     exactement ce retard** (`latence_trames`). Le module perdu par le
//!     fenêtrage est rendu par la partie à phase minimale, qui est calculée
//!     APRÈS (module voulu ÷ module de la partie fenêtrée) : le module final
//!     reste celui qui est demandé.
//! - [`minimum_phase_from_magnitude`] : réponse impulsionnelle à phase
//!   minimale d'une magnitude donnée, par le cepstre réel (Oppenheim &
//!   Schafer, « Discrete-Time Signal Processing », § 13.5 ; même méthode que
//!   `scipy.signal.minimum_phase(method="homomorphic")`).
//! - [`mixed_phase_design`] (esquisse d'avant T10b, gardée) : en dessous de
//!   `split_hz`, correction en phase minimale de la magnitude d'un égaliseur ;
//!   au-dessus, identité.
//! - [`convolution_cost`] : ordre de grandeur du coût CPU du convolueur
//!   partitionné uniforme de Tune (bloc 1024, FFT 2048) par canal.
//!
//! Une seule cadence est produite : celle de la mesure (48 kHz). Le
//! convolueur rééchantillonne lui-même la RI vers la cadence jouée.

use crate::analysis::{self, LogResponse};
use crate::deconv::ImpulseResponse;
use crate::fft;
use crate::target::TargetCurve;
use realfft::num_complex::Complex;
use std::f64::consts::PI;

/// Réponse impulsionnelle à phase minimale de longueur `nfft` dont le module
/// vaut `magnitude_db(f)` (fonction continue de la fréquence en Hz).
pub fn minimum_phase_from_magnitude(
    sample_rate: u32,
    nfft: usize,
    magnitude_db: impl Fn(f64) -> f64,
) -> Vec<f64> {
    let nfft = fft::next_pow2(nfft);
    let bins = nfft / 2 + 1;
    let df = sample_rate as f64 / nfft as f64;
    let ln_mag: Vec<f64> = (0..bins)
        .map(|i| fft::from_db(magnitude_db(i as f64 * df)).max(1e-9).ln())
        .collect();
    fft::irfft(&spectre_phase_minimale(&ln_mag, nfft), nfft)
}

/// Spectre (`nfft/2 + 1` bins) à phase minimale dont le logarithme népérien
/// du module vaut `ln_mag` : cepstre réel, repliement c[0], 2c[n] pour
/// 0 < n < N/2, c[N/2], 0 ensuite, puis exponentielle complexe.
pub fn spectre_phase_minimale(ln_mag: &[f64], nfft: usize) -> Vec<Complex<f64>> {
    let spec: Vec<Complex<f64>> = ln_mag.iter().map(|v| Complex::new(*v, 0.0)).collect();
    let cep = fft::irfft(&spec, nfft);
    let mut folded = vec![0.0; nfft];
    folded[0] = cep[0];
    for n in 1..nfft / 2 {
        folded[n] = 2.0 * cep[n];
    }
    folded[nfft / 2] = cep[nfft / 2];
    fft::rfft(&folded, nfft).iter().map(|c| c.exp()).collect()
}

/// FIR en phase mixte de `taps` coefficients (voir en-tête du module).
/// `correction_db(f)` est la réponse souhaitée (par exemple celle de
/// l'égaliseur automatique, `eq::bands_response_db`). Au-dessus de
/// `split_hz` la correction est fondue vers 0 dB sur une demi-octave.
pub fn mixed_phase_design(
    sample_rate: u32,
    taps: usize,
    split_hz: f64,
    correction_db: impl Fn(f64) -> f64,
) -> Vec<f32> {
    let nfft = fft::next_pow2(taps * 4);
    let hi = split_hz * 2f64.sqrt();
    let mag = |f: f64| -> f64 {
        if f <= split_hz {
            correction_db(f)
        } else if f < hi {
            let t = (f / split_hz).ln() / (hi / split_hz).ln();
            correction_db(f) * (0.5 + 0.5 * (std::f64::consts::PI * t).cos())
        } else {
            0.0
        }
    };
    let mut h = minimum_phase_from_magnitude(sample_rate, nfft, mag);
    h.truncate(taps);
    // Fondu de sortie sur le dernier quart pour éviter une troncature brutale.
    let fade = taps / 4;
    for i in 0..fade {
        let w = 0.5 + 0.5 * (std::f64::consts::PI * i as f64 / fade as f64).cos();
        h[taps - fade + i] *= w;
    }
    h.iter().map(|v| *v as f32).collect()
}

/// Module (dB) d'un FIR à la fréquence `f` (somme directe, comme
/// `convolver::fir_frequency_response`).
pub fn fir_magnitude_db(h: &[f32], sample_rate: u32, f: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * f / sample_rate as f64;
    let (mut re, mut im) = (0.0, 0.0);
    for (n, v) in h.iter().enumerate() {
        re += *v as f64 * (w * n as f64).cos();
        im -= *v as f64 * (w * n as f64).sin();
    }
    fft::to_db((re * re + im * im).sqrt())
}

/// Coût estimé de la convolution partitionnée uniforme de Tune.
#[derive(Debug, Clone, PartialEq)]
pub struct ConvolutionCost {
    pub sample_rate: u32,
    pub taps: usize,
    pub block: usize,
    pub partitions: usize,
    /// Latence de bloc (ms) = `block / fs` (`Convolver::latency_frames`).
    pub block_latency_ms: f64,
    /// Multiplications-accumulations complexes par seconde et par canal
    /// (produit spectral des partitions), hors FFT.
    pub cmacs_per_s: f64,
    /// Opérations flottantes approximatives par seconde et par canal, FFT
    /// comprises (5·N·log2 N par FFT, 2 FFT par bloc).
    pub flops_per_s: f64,
    /// Mémoire de la ligne à retard fréquentielle par canal (octets, f32 complexes).
    pub fdl_bytes: usize,
}

pub fn convolution_cost(sample_rate: u32, taps: usize, block: usize) -> ConvolutionCost {
    let fft_size = (block * 2).next_power_of_two();
    let bins = fft_size / 2 + 1;
    let partitions = taps.div_ceil(block);
    let blocks_per_s = sample_rate as f64 / block as f64;
    let cmacs = partitions as f64 * bins as f64 * blocks_per_s;
    let fft_flops = 2.0 * 5.0 * fft_size as f64 * (fft_size as f64).log2() * blocks_per_s;
    ConvolutionCost {
        sample_rate,
        taps,
        block,
        partitions,
        block_latency_ms: block as f64 * 1000.0 / sample_rate as f64,
        cmacs_per_s: cmacs,
        flops_per_s: cmacs * 8.0 + fft_flops,
        fdl_bytes: partitions * bins * 8,
    }
}

// ── C3 : le FIR de correction calculé depuis la mesure (T10b) ──────────────

/// Bornes des réglages de [`OptionsFir`] (un réglage hors bornes est REFUSÉ,
/// pas écrêté).
pub const PRISES_MIN: usize = 4_096;
/// 2^17 : bien sous la borne du convolueur (2^20 prises).
pub const PRISES_MAX: usize = 131_072;
pub const PRISES_DEFAUT: usize = 32_768;
pub const RELANCE_MAX_DB: f64 = 12.0;
pub const COUPE_MAX_DB: f64 = 30.0;
pub const PRE_TINTEMENT_MAX_MS: f64 = 50.0;
/// Largeur (octaves) du fondu de retour à 0 dB hors de la bande corrigée, et
/// des fondus de la correction de phase.
pub const FONDU_OCTAVES: f64 = 0.5;
/// Passes de raffinement : la réponse prévue (mesure + filtre, lissée comme
/// la conception) est comparée à la cible, l'écart restant est ajouté à la
/// correction voulue (toujours bornée), et le filtre recalculé. Le lissage
/// d'une somme n'est pas la somme des lissages : sans ces passes, un mode
/// étroit au milieu d'une queue diffuse laisse ±1,7 dB.
pub const RAFFINEMENTS: usize = 2;

/// Correction de la phase excédentaire (voir l'en-tête du module).
#[derive(Debug, Clone, PartialEq)]
pub struct OptionsPhase {
    /// Au-dessus (fondu d'une demi-octave), la phase n'est plus touchée.
    pub f_bascule_hz: f64,
    /// Demi-largeur de la fenêtre de la correction de phase : borne du
    /// pré-tintement ET latence ajoutée (ms).
    pub pre_tintement_ms: f64,
}

impl Default for OptionsPhase {
    fn default() -> Self {
        Self {
            f_bascule_hz: 400.0,
            pre_tintement_ms: 20.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OptionsFir {
    /// Longueur du filtre (prises).
    pub prises: usize,
    /// Bande corrigée ; 0 dB en dehors, après un fondu d'une demi-octave.
    pub f_lo: f64,
    pub f_hi: f64,
    /// Relance maximale (dB, ≥ 0) et coupe maximale (dB, ≥ 0) de la correction.
    pub relance_max_db: f64,
    pub coupe_max_db: f64,
    /// Lissage 1/N d'octave de la mesure : `lissage_bas_n` sous
    /// `f_lissage_hz`, `lissage_haut_n` au-dessus.
    pub lissage_bas_n: usize,
    pub lissage_haut_n: usize,
    pub f_lissage_hz: f64,
    /// Ramène le gain le plus haut du filtre à 0 dB.
    pub normaliser: bool,
    /// `None` : phase minimale seule (latence nulle).
    pub phase: Option<OptionsPhase>,
}

impl Default for OptionsFir {
    fn default() -> Self {
        Self {
            prises: PRISES_DEFAUT,
            f_lo: 20.0,
            f_hi: 15_000.0,
            relance_max_db: 6.0,
            coupe_max_db: 15.0,
            lissage_bas_n: 6,
            lissage_haut_n: 3,
            f_lissage_hz: 300.0,
            normaliser: true,
            phase: None,
        }
    }
}

impl OptionsFir {
    /// Le refus motivé d'un réglage hors bornes, pour la cadence `cadence`.
    pub fn valider(&self, cadence: u32) -> Result<(), String> {
        let fs = cadence as f64;
        if !(8_000..=768_000).contains(&cadence) {
            return Err(format!("cadence {cadence} Hz : entre 8 et 768 kHz"));
        }
        if !(PRISES_MIN..=PRISES_MAX).contains(&self.prises) {
            return Err(format!(
                "{} prises : entre {PRISES_MIN} et {PRISES_MAX}",
                self.prises
            ));
        }
        if !self.f_lo.is_finite() || !self.f_hi.is_finite() || self.f_lo < 10.0 {
            return Err(format!(
                "bande {}–{} Hz : 10 Hz au moins",
                self.f_lo, self.f_hi
            ));
        }
        if self.f_hi > fs * 0.45 {
            return Err(format!(
                "bande {}–{} Hz : au plus {:.0} Hz à {cadence} Hz",
                self.f_lo,
                self.f_hi,
                fs * 0.45
            ));
        }
        if self.f_hi / self.f_lo < 2.0 {
            return Err(format!(
                "bande {}–{} Hz : une octave au moins",
                self.f_lo, self.f_hi
            ));
        }
        if !(0.0..=RELANCE_MAX_DB).contains(&self.relance_max_db) {
            return Err(format!(
                "relance maximale {} dB : entre 0 et {RELANCE_MAX_DB} dB",
                self.relance_max_db
            ));
        }
        if !(0.0..=COUPE_MAX_DB).contains(&self.coupe_max_db) {
            return Err(format!(
                "coupe maximale {} dB : entre 0 et {COUPE_MAX_DB} dB",
                self.coupe_max_db
            ));
        }
        if !(1..=48).contains(&self.lissage_bas_n) || !(1..=48).contains(&self.lissage_haut_n) {
            return Err("lissage : 1/1 à 1/48 d'octave".into());
        }
        if !self.f_lissage_hz.is_finite() || self.f_lissage_hz <= 0.0 {
            return Err("fréquence de changement de lissage invalide".into());
        }
        if let Some(p) = &self.phase {
            if !p.f_bascule_hz.is_finite()
                || p.f_bascule_hz < 2.0 * self.f_lo
                || p.f_bascule_hz > 1_000.0
            {
                return Err(format!(
                    "fréquence de bascule de la phase {} Hz : entre {} Hz et 1 kHz",
                    p.f_bascule_hz,
                    2.0 * self.f_lo
                ));
            }
            if !p.pre_tintement_ms.is_finite()
                || p.pre_tintement_ms < 1.0
                || p.pre_tintement_ms > PRE_TINTEMENT_MAX_MS
            {
                return Err(format!(
                    "pré-tintement {} ms : entre 1 et {PRE_TINTEMENT_MAX_MS} ms",
                    p.pre_tintement_ms
                ));
            }
        }
        Ok(())
    }
}

/// Le FIR conçu et ce qu'il promet.
#[derive(Debug, Clone, PartialEq)]
pub struct FirCorrection {
    pub cadence: u32,
    /// Les coefficients (`f64`), un seul canal.
    pub coefficients: Vec<f64>,
    /// Retard de modélisation du filtre (trames) : 0 en phase minimale, la
    /// demi-fenêtre de la correction de phase sinon.
    pub latence_trames: usize,
    /// Rang du maximum absolu des coefficients (mesuré, pour contrôle).
    pub pic_trame: usize,
    /// Décalage à ajouter à la cible pour obtenir ce que la réponse corrigée
    /// doit suivre : alignement sur la mesure (médiane) + normalisation.
    pub decalage_cible_db: f64,
    /// Gain global appliqué pour ramener la crête du filtre à 0 dB (≤ 0).
    pub normalisation_db: f64,
    /// Gains extrêmes du filtre dans la bande corrigée (dB).
    pub gain_max_db: f64,
    pub gain_min_db: f64,
    pub phase_excedentaire: bool,
    /// Énergie avant `latence_trames` rapportée à l'énergie totale (dB) ;
    /// `None` en phase minimale (aucune énergie avant le pic).
    pub energie_pre_db: Option<f64>,
    /// Courbes sur la grille de la mesure.
    pub freqs: Vec<f64>,
    pub mesure_lissee_db: Vec<f64>,
    /// La cible effective : cible + `decalage_cible_db`.
    pub cible_db: Vec<f64>,
    /// Module voulu du filtre.
    pub correction_db: Vec<f64>,
    /// Mesure lissée + module réel du filtre : ce que la mesure de
    /// vérification devrait montrer.
    pub prevue_db: Vec<f64>,
    pub avertissements: Vec<String>,
}

impl FirCorrection {
    pub fn latence_ms(&self) -> f64 {
        self.latence_trames as f64 * 1000.0 / self.cadence as f64
    }
}

/// Poids de la bande corrigée : 1 dans `[f_lo, f_hi]`, fondu en cosinus sur
/// [`FONDU_OCTAVES`] de part et d'autre, 0 au-delà.
pub fn poids_bande(f: f64, f_lo: f64, f_hi: f64) -> f64 {
    if f <= 0.0 {
        return 0.0;
    }
    let t = if f < f_lo {
        (f_lo / f).log2() / FONDU_OCTAVES
    } else if f > f_hi {
        (f / f_hi).log2() / FONDU_OCTAVES
    } else {
        return 1.0;
    };
    if t >= 1.0 {
        0.0
    } else {
        0.5 + 0.5 * (PI * t).cos()
    }
}

/// Lissage 1/`n_bas` d'octave sous `f_bascule`, 1/`n_haut` au-dessus, fondu
/// linéaire (en log f) sur une octave centrée sur `f_bascule`.
pub fn lissage_variable(
    r: &LogResponse,
    n_bas: usize,
    n_haut: usize,
    f_bascule: f64,
) -> LogResponse {
    let bas = r.smoothed(n_bas);
    let haut = r.smoothed(n_haut);
    let magnitude_db = r
        .freqs
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let t = ((f / f_bascule).log2() + 0.5).clamp(0.0, 1.0);
            (1.0 - t) * bas.magnitude_db[i] + t * haut.magnitude_db[i]
        })
        .collect();
    LogResponse {
        freqs: r.freqs.clone(),
        magnitude_db,
    }
}

/// Module (dB) d'un FIR `f64` aux fréquences `freqs` : FFT fine (8 fois la
/// longueur au moins), interpolation linéaire du module entre deux bins.
pub fn reponse_fir_db(h: &[f64], cadence: u32, freqs: &[f64]) -> Vec<f64> {
    let nfft = fft::next_pow2((h.len() * 8).max(65_536));
    let spec = fft::rfft(h, nfft);
    let df = cadence as f64 / nfft as f64;
    freqs
        .iter()
        .map(|f| {
            let x = (f / df).clamp(0.0, (spec.len() - 1) as f64);
            let k = (x.floor() as usize).min(spec.len() - 2);
            let t = x - k as f64;
            fft::to_db((1.0 - t) * spec[k].norm() + t * spec[k + 1].norm())
        })
        .collect()
}

fn replier(x: f64) -> f64 {
    (x + PI).rem_euclid(2.0 * PI) - PI
}

/// Réponse (centrée au rang `P`, longueur `2P + 1`) de la correction de la
/// phase excédentaire de `ri` sous `p.f_bascule_hz`, fenêtrée sur ±P,
/// P = `p.pre_tintement_ms`. Voir l'en-tête du module.
pub fn correction_phase_excedentaire(
    ri: &ImpulseResponse,
    f_lo: f64,
    p: &OptionsPhase,
) -> Result<Vec<f64>, String> {
    if ri.samples.is_empty() || ri.peak_index >= ri.samples.len() {
        return Err("réponse impulsionnelle vide".into());
    }
    let fs = ri.sample_rate as f64;
    // Même fenêtre que l'analyse du module (100 ms avant le pic, 500 ms après).
    let seg = analysis::window_ir(ri, 100.0, 500.0, 250.0);
    let pre = ri.peak_index.min((0.1 * fs).round() as usize);
    let np = fft::next_pow2((seg.len() * 2).max(131_072));
    // Rotation : le pic au rang 0, ce qui le précède à la fin (retard pur ôté).
    let mut x = vec![0.0; np];
    for (j, v) in seg.iter().enumerate() {
        x[(j + np - pre) % np] += v;
    }
    let h = fft::rfft(&x, np);
    let crete = h.iter().fold(0.0f64, |m, z| m.max(z.norm()));
    if crete <= 0.0 || !crete.is_finite() {
        return Err("réponse impulsionnelle nulle".into());
    }
    let plancher = crete * 1e-5;
    let ln_mag: Vec<f64> = h.iter().map(|z| z.norm().max(plancher).ln()).collect();
    let hmin = spectre_phase_minimale(&ln_mag, np);
    let df = fs / np as f64;
    let haut = p.f_bascule_hz * 2f64.powf(FONDU_OCTAVES + 1.0 / 12.0);
    let k_max = ((haut / df).ceil() as usize + 1).min(h.len() - 1);
    let polarite = if ri.samples[ri.peak_index] < 0.0 {
        PI
    } else {
        0.0
    };
    let ex: Vec<f64> = (0..=k_max)
        .map(|k| replier((h[k] * hmin[k].conj()).arg() - polarite))
        .collect();
    // Déroulement depuis f_lo, vers le haut puis vers le bas.
    let k_ref = ((f_lo / df).round() as usize).clamp(1, k_max);
    let mut u = ex.clone();
    for k in k_ref + 1..=k_max {
        u[k] = u[k - 1] + replier(ex[k] - ex[k - 1]);
    }
    for k in (0..k_ref).rev() {
        u[k] = u[k + 1] + replier(ex[k] - ex[k + 1]);
    }
    // Lissage 1/6 d'octave (moyenne sur ±1/12 d'octave), par sommes cumulées.
    let mut cumul = vec![0.0; u.len() + 1];
    for (k, v) in u.iter().enumerate() {
        cumul[k + 1] = cumul[k] + v;
    }
    let demi = 2f64.powf(1.0 / 12.0);
    let low = f_lo / 2f64.powf(FONDU_OCTAVES);
    let mut phi = vec![0.0; h.len()];
    for (k, ph) in phi.iter_mut().enumerate().take(k_max + 1).skip(1) {
        let f = k as f64 * df;
        let w = if f < low {
            0.0
        } else {
            poids_bande(f, f_lo, p.f_bascule_hz)
        };
        if w == 0.0 {
            continue;
        }
        let a = ((f / demi / df).floor() as usize).max(1);
        let b = ((f * demi / df).ceil() as usize).min(k_max);
        let moyenne = (cumul[b + 1] - cumul[a]) / (b + 1 - a) as f64;
        *ph = -w * moyenne;
    }
    let spec: Vec<Complex<f64>> = phi.iter().map(|ph| Complex::from_polar(1.0, *ph)).collect();
    let e = fft::irfft(&spec, np);
    let pn = (p.pre_tintement_ms * fs / 1000.0).round() as usize;
    let plateau = pn / 2;
    let mut out = vec![0.0; 2 * pn + 1];
    for (i, o) in out.iter_mut().enumerate() {
        let n = i as isize - pn as isize;
        let a = n.unsigned_abs();
        let w = if a <= plateau {
            1.0
        } else {
            0.5 + 0.5 * (PI * (a - plateau) as f64 / (pn - plateau) as f64).cos()
        };
        *o = e[n.rem_euclid(np as isize) as usize] * w;
    }
    Ok(out)
}

/// Le FIR de correction de la réponse `mesure` (grille log, dB, lissée ou
/// non) vers `cible`. `ri` (la réponse impulsionnelle de la même mesure)
/// n'est lue que pour la phase excédentaire, qui l'exige.
pub fn concevoir_fir(
    cadence: u32,
    mesure: &LogResponse,
    ri: Option<&ImpulseResponse>,
    cible: &TargetCurve,
    o: &OptionsFir,
) -> Result<FirCorrection, String> {
    o.valider(cadence)?;
    if mesure.freqs.len() < 8
        || mesure.freqs.len() != mesure.magnitude_db.len()
        || mesure.magnitude_db.iter().any(|d| !d.is_finite())
    {
        return Err("réponse mesurée vide ou invalide".into());
    }
    let (r_lo, r_hi) = (mesure.freqs[0], *mesure.freqs.last().unwrap());
    if r_lo > o.f_lo * 1.01 || r_hi < o.f_hi * 0.99 {
        return Err(format!(
            "la mesure couvre {r_lo:.0}–{r_hi:.0} Hz, la bande corrigée {}–{} Hz en sort",
            o.f_lo, o.f_hi
        ));
    }
    if o.phase.is_some() {
        match ri {
            Some(r) if r.sample_rate == cadence => {}
            Some(r) => {
                return Err(format!(
                    "réponse impulsionnelle à {} Hz, filtre à {cadence} Hz",
                    r.sample_rate
                ));
            }
            None => {
                return Err(
                    "correction de phase demandée sans réponse impulsionnelle (moyenne de positions ?)"
                        .into(),
                );
            }
        }
    }
    let fs = cadence as f64;
    let mut avertissements = Vec::new();
    let lisse = lissage_variable(mesure, o.lissage_bas_n, o.lissage_haut_n, o.f_lissage_hz);
    let offset = cible.align_offset(&lisse, o.f_lo, o.f_hi);
    let brute = |f: f64| cible.level_db(f) + offset - lisse.at(f);
    // Correction voulue (avant normalisation), `d` = raffinement accumulé.
    let voulue = |f: f64, d: &LogResponse| {
        let w = poids_bande(f, o.f_lo, o.f_hi);
        if w == 0.0 {
            0.0
        } else {
            w * (brute(f) + d.at(f)).clamp(-o.coupe_max_db, o.relance_max_db)
        }
    };
    let relances_bornees = mesure
        .freqs
        .iter()
        .filter(|f| **f >= o.f_lo && **f <= o.f_hi && brute(**f) > o.relance_max_db + 0.5)
        .count();
    if relances_bornees > 0 {
        avertissements.push(format!(
            "creux plus profonds que la relance permise (+{} dB) : la réponse corrigée y reste sous la cible",
            o.relance_max_db
        ));
    }

    let nfft = fft::next_pow2(o.prises * 4);
    let bins = nfft / 2 + 1;
    let df = fs / nfft as f64;
    let phase = match &o.phase {
        Some(p) => Some(correction_phase_excedentaire(ri.unwrap(), o.f_lo, p)?),
        None => None,
    };
    let latence = phase.as_ref().map(|e| e.len() / 2).unwrap_or(0);
    let ez = phase.as_ref().map(|e| fft::rfft(e, nfft));
    let freqs = lisse.freqs.clone();
    let mut delta = LogResponse {
        freqs: freqs.clone(),
        magnitude_db: vec![0.0; freqs.len()],
    };
    let mut h = Vec::new();
    let mut reel = Vec::new();
    let mut prevue = Vec::new();
    let mut normalisation = 0.0;
    for passe in 0..=RAFFINEMENTS {
        let mut corr: Vec<f64> = (0..bins).map(|k| voulue(k as f64 * df, &delta)).collect();
        let crete = corr.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        normalisation = if o.normaliser && crete > 0.0 {
            -crete
        } else {
            0.0
        };
        corr.iter_mut().for_each(|c| *c += normalisation);
        // Module voulu ÷ module de la partie de phase fenêtrée.
        let ln_mag: Vec<f64> = match &ez {
            Some(ez) => corr
                .iter()
                .zip(ez)
                .map(|(c, z)| (fft::from_db(*c) / z.norm().max(1e-6)).ln())
                .collect(),
            None => corr.iter().map(|c| fft::from_db(*c).ln()).collect(),
        };
        let hmin = fft::irfft(&spectre_phase_minimale(&ln_mag, nfft), nfft);
        h = match &phase {
            Some(e) => fft::convolve(&hmin[..o.prises], e),
            None => hmin[..o.prises].to_vec(),
        };
        h.truncate(o.prises);
        // Fondu de sortie sur le dernier huitième : pas de troncature brutale.
        let fondu = o.prises / 8;
        for i in 0..fondu {
            let w = 0.5 + 0.5 * (PI * i as f64 / fondu as f64).cos();
            h[o.prises - fondu + i] *= w;
        }
        if h.iter().any(|v| !v.is_finite()) {
            return Err("filtre non fini (mesure dégénérée)".into());
        }
        // Ce que la vérification verra : (mesure + filtre), lissée comme la
        // conception ; l'écart restant à la cible raffine la passe suivante.
        reel = reponse_fir_db(&h, cadence, &freqs);
        let somme = LogResponse {
            freqs: freqs.clone(),
            magnitude_db: mesure
                .magnitude_db
                .iter()
                .zip(&reel)
                .map(|(m, g)| m + g)
                .collect(),
        };
        prevue = lissage_variable(&somme, o.lissage_bas_n, o.lissage_haut_n, o.f_lissage_hz)
            .magnitude_db;
        if passe < RAFFINEMENTS {
            for (i, f) in freqs.iter().enumerate() {
                if *f >= o.f_lo && *f <= o.f_hi {
                    delta.magnitude_db[i] +=
                        cible.level_db(*f) + offset + normalisation - prevue[i];
                }
            }
        }
    }

    let dans_bande = |i: &usize| freqs[*i] >= o.f_lo && freqs[*i] <= o.f_hi;
    let gain_max_db = (0..freqs.len())
        .filter(dans_bande)
        .map(|i| reel[i])
        .fold(f64::NEG_INFINITY, f64::max);
    let gain_min_db = (0..freqs.len())
        .filter(dans_bande)
        .map(|i| reel[i])
        .fold(f64::INFINITY, f64::min);
    let energie = h.iter().map(|v| v * v).sum::<f64>();
    let energie_pre_db = (latence > 0).then(|| {
        10.0 * (h[..latence].iter().map(|v| v * v).sum::<f64>() / energie)
            .max(1e-30)
            .log10()
    });
    let decalage = offset + normalisation;
    Ok(FirCorrection {
        cadence,
        pic_trame: fft::argmax_abs(&h),
        latence_trames: latence,
        decalage_cible_db: decalage,
        normalisation_db: normalisation,
        gain_max_db,
        gain_min_db,
        phase_excedentaire: phase.is_some(),
        energie_pre_db,
        cible_db: freqs
            .iter()
            .map(|f| cible.level_db(*f) + decalage)
            .collect(),
        correction_db: freqs
            .iter()
            .map(|f| voulue(*f, &delta) + normalisation)
            .collect(),
        prevue_db: prevue,
        mesure_lissee_db: lisse.magnitude_db,
        freqs,
        coefficients: h,
        avertissements,
    })
}

/// Réponse, sur `grille`, de la réponse impulsionnelle `ri` passée par le
/// filtre `h` de latence `latence` : convolution `f64`, puis la même analyse
/// que la mesure (fenêtre de 500 ms, lissage 1/12 d'octave).
pub fn reponse_corrigee(
    ri: &ImpulseResponse,
    h: &[f64],
    latence: usize,
    grille: &[f64],
) -> LogResponse {
    let corrigee = ImpulseResponse {
        sample_rate: ri.sample_rate,
        samples: fft::convolve(&ri.samples, h),
        peak_index: ri.peak_index + latence,
        delay_samples: ri.delay_samples + latence,
    };
    analysis::measured_response(&corrigee, 500.0, 12, grille)
}

/// Écart maximal (dB, en valeur absolue) entre `r` et `cible + decalage_db`
/// sur `[f_lo, f_hi]`.
pub fn ecart_max_a_la_cible_db(
    r: &LogResponse,
    cible: &TargetCurve,
    decalage_db: f64,
    f_lo: f64,
    f_hi: f64,
) -> f64 {
    r.freqs
        .iter()
        .zip(&r.magnitude_db)
        .filter(|(f, _)| **f >= f_lo && **f <= f_hi)
        .map(|(f, m)| (m - cible.level_db(*f) - decalage_db).abs())
        .fold(0.0, f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eq::{bands_response_db, Band, BandType};

    #[test]
    fn phase_minimale_restitue_la_magnitude() {
        let fs = 48_000;
        let bands = [Band {
            freq: 50.0,
            gain: -8.0,
            q: 4.0,
            kind: BandType::Peak,
            channel: None,
        }];
        let corr = |f: f64| bands_response_db(fs as f64, &bands, &[f])[0];
        let h = minimum_phase_from_magnitude(fs, 65_536, corr);
        let hf: Vec<f32> = h.iter().map(|v| *v as f32).collect();
        for f in [30.0, 50.0, 80.0, 200.0, 2000.0] {
            let got = fir_magnitude_db(&hf, fs, f);
            assert!((got - corr(f)).abs() < 0.3, "{f} Hz : {got} ≠ {}", corr(f));
        }
        // Phase minimale : l'énergie est concentrée au début.
        let e_total: f64 = h.iter().map(|v| v * v).sum();
        let e_head: f64 = h[..4096].iter().map(|v| v * v).sum();
        assert!(e_head / e_total > 0.99);
    }

    #[test]
    fn fir_mixte_colle_a_l_egaliseur_sous_la_coupure() {
        let fs = 48_000;
        let bands = [
            Band {
                freq: 42.0,
                gain: -9.0,
                q: 6.0,
                kind: BandType::Peak,
                channel: None,
            },
            Band {
                freq: 115.0,
                gain: -5.0,
                q: 7.0,
                kind: BandType::Peak,
                channel: None,
            },
        ];
        let corr = |f: f64| bands_response_db(fs as f64, &bands, &[f])[0];
        let h = mixed_phase_design(fs, 16_384, 300.0, corr);
        for f in [42.0, 60.0, 115.0, 200.0] {
            let got = fir_magnitude_db(&h, fs, f);
            assert!((got - corr(f)).abs() < 0.5, "{f} Hz : {got} ≠ {}", corr(f));
        }
        assert!(fir_magnitude_db(&h, fs, 2000.0).abs() < 0.2);
    }

    #[test]
    fn cout_convolution() {
        let c = convolution_cost(48_000, 16_384, 1024);
        assert_eq!(c.partitions, 16);
        assert!((c.block_latency_ms - 21.333).abs() < 0.01);
        assert!(c.flops_per_s > 1e7 && c.flops_per_s < 1e9);
    }

    // ── C3 (T10b) ───────────────────────────────────────────────────────

    use crate::analysis::LogResponse;
    use crate::sim::RoomSim;
    use crate::target::TargetCurve;

    fn ri_de(h: Vec<f64>, fs: u32, avance: usize) -> ImpulseResponse {
        let mut samples = vec![0.0; avance];
        samples.extend(h);
        ImpulseResponse {
            sample_rate: fs,
            samples,
            peak_index: avance,
            delay_samples: avance,
        }
    }

    fn reponse(ri: &ImpulseResponse) -> LogResponse {
        crate::analysis::measured_response(ri, 500.0, 12, &LogResponse::grid(20.0, 20_000.0, 48))
    }

    #[test]
    fn poids_de_bande_et_lissage_variable() {
        assert_eq!(poids_bande(100.0, 20.0, 15_000.0), 1.0);
        assert_eq!(poids_bande(20.0 / 2f64.sqrt() * 0.999, 20.0, 15_000.0), 0.0);
        assert!((poids_bande(20.0 / 2f64.powf(0.25), 20.0, 15_000.0) - 0.5).abs() < 1e-9);
        assert_eq!(poids_bande(22_000.0, 20.0, 15_000.0), 0.0);
        let freqs = LogResponse::grid(20.0, 20_000.0, 48);
        let r = LogResponse {
            magnitude_db: freqs.iter().map(|f| (f / 3.0).ln().sin() * 3.0).collect(),
            freqs,
        };
        let l = lissage_variable(&r, 6, 3, 300.0);
        let (b, h) = (r.smoothed(6), r.smoothed(3));
        let i = r.freqs.iter().position(|f| *f >= 100.0).unwrap();
        let j = r.freqs.iter().position(|f| *f >= 2_000.0).unwrap();
        assert_eq!(l.magnitude_db[i], b.magnitude_db[i]);
        assert_eq!(l.magnitude_db[j], h.magnitude_db[j]);
    }

    /// Phase minimale : le pic est au rang 0, la latence annoncée est nulle,
    /// rien ne précède le pic ; le module suit la cible à ±1,5 dB.
    #[test]
    fn le_fir_a_phase_minimale_n_a_rien_avant_son_pic() {
        let fs = 48_000;
        let ri = ri_de(RoomSim::dry_living_room(fs).impulse_response(), fs, 4_800);
        let m = reponse(&ri);
        let fir = concevoir_fir(
            fs,
            &m,
            Some(&ri),
            &TargetCurve::flat(),
            &OptionsFir::default(),
        )
        .unwrap();
        assert_eq!(fir.coefficients.len(), PRISES_DEFAUT);
        assert_eq!(fir.latence_trames, 0);
        assert_eq!(fir.pic_trame, 0, "pic au rang {}", fir.pic_trame);
        assert!(fir.energie_pre_db.is_none());
        assert!(!fir.phase_excedentaire);
        assert!(fir.gain_max_db <= 1e-6, "crête {} dB", fir.gain_max_db);
        let c = reponse_corrigee(&ri, &fir.coefficients, 0, &m.freqs);
        let c = lissage_variable(&c, 6, 3, 300.0);
        let e = ecart_max_a_la_cible_db(
            &c,
            &TargetCurve::flat(),
            fir.decalage_cible_db,
            30.0,
            15_000.0,
        );
        assert!(e < 1.5, "écart {e:.2} dB");
    }

    /// Relance bornée, coupe bornée, 0 dB hors de la bande (sans normalisation).
    #[test]
    fn relance_bornee_et_retour_a_zero_hors_bande() {
        let fs = 48_000;
        let freqs = LogResponse::grid(20.0, 20_000.0, 48);
        // Creux de 15 dB à 80 Hz, pic de 25 dB à 2 kHz.
        let bandes = [
            Band {
                freq: 80.0,
                gain: -15.0,
                q: 3.0,
                kind: BandType::Peak,
                channel: None,
            },
            Band {
                freq: 2_000.0,
                gain: 25.0,
                q: 3.0,
                kind: BandType::Peak,
                channel: None,
            },
        ];
        let m = LogResponse {
            magnitude_db: bands_response_db(fs as f64, &bandes, &freqs),
            freqs,
        };
        let o = OptionsFir {
            normaliser: false,
            f_hi: 10_000.0,
            ..OptionsFir::default()
        };
        let fir = concevoir_fir(fs, &m, None, &TargetCurve::flat(), &o).unwrap();
        let g = |f: f64| reponse_fir_db(&fir.coefficients, fs, &[f])[0];
        assert!(
            g(80.0) <= 6.0 + 0.3 && g(80.0) > 5.0,
            "80 Hz : {:.2}",
            g(80.0)
        );
        assert!(
            g(2_000.0) >= -15.0 - 0.3 && g(2_000.0) < -14.0,
            "2 kHz : {:.2}",
            g(2_000.0)
        );
        assert!(g(12.0).abs() < 0.3, "12 Hz : {:.2}", g(12.0));
        assert!(g(16_000.0).abs() < 0.3, "16 kHz : {:.2}", g(16_000.0));
        assert!(fir.avertissements.iter().any(|a| a.contains("relance")));
        // Normalisé : la crête du filtre est ramenée à 0 dB.
        let n = concevoir_fir(
            fs,
            &m,
            None,
            &TargetCurve::flat(),
            &OptionsFir {
                normaliser: true,
                ..o
            },
        )
        .unwrap();
        assert!(n.normalisation_db < -5.0, "{}", n.normalisation_db);
        assert!(n.gain_max_db <= 0.05, "{}", n.gain_max_db);
    }

    #[test]
    fn les_reglages_hors_bornes_sont_refuses() {
        let fs = 48_000;
        let freqs = LogResponse::grid(20.0, 20_000.0, 48);
        let m = LogResponse {
            magnitude_db: vec![0.0; freqs.len()],
            freqs,
        };
        let refus =
            |o: OptionsFir| concevoir_fir(fs, &m, None, &TargetCurve::flat(), &o).unwrap_err();
        let d = OptionsFir::default;
        assert!(refus(OptionsFir {
            prises: 1_000,
            ..d()
        })
        .contains("prises"));
        assert!(refus(OptionsFir {
            prises: 1 << 18,
            ..d()
        })
        .contains("prises"));
        assert!(refus(OptionsFir {
            relance_max_db: 13.0,
            ..d()
        })
        .contains("relance"));
        assert!(refus(OptionsFir {
            coupe_max_db: -1.0,
            ..d()
        })
        .contains("coupe"));
        assert!(refus(OptionsFir {
            f_hi: 22_000.0,
            ..d()
        })
        .contains("au plus"));
        assert!(refus(OptionsFir { f_lo: 5.0, ..d() }).contains("10 Hz"));
        assert!(refus(OptionsFir {
            phase: Some(OptionsPhase {
                pre_tintement_ms: 80.0,
                ..Default::default()
            }),
            ..d()
        })
        .contains("pré-tintement"));
        // Phase demandée sans réponse impulsionnelle.
        assert!(refus(OptionsFir {
            phase: Some(OptionsPhase::default()),
            ..d()
        })
        .contains("sans réponse impulsionnelle"));
        // Mesure trop courte pour la bande.
        let courte = LogResponse {
            freqs: LogResponse::grid(100.0, 20_000.0, 12),
            magnitude_db: vec![0.0; LogResponse::grid(100.0, 20_000.0, 12).len()],
        };
        assert!(concevoir_fir(fs, &courte, None, &TargetCurve::flat(), &d())
            .unwrap_err()
            .contains("couvre"));
    }
}
