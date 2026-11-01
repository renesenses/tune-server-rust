//! Égaliseur paramétrique automatique (prototype C2).
//!
//! Filtres biquad du « Cookbook » de R. Bristow-Johnson (mêmes formules que
//! `sdk/tune-plugin-equalizer/src/engine.rs`), évaluation analytique de la
//! réponse en z = e^{jω}, et ajustement glouton puis raffinement par
//! descente de coordonnées, dans l'esprit de la fonction « EQ → Match
//! response to target » de REW et d'AutoEq (Jaakko Pasanen) : on ne coupe
//! que les pics, on ne remonte pas les creux (gain ≤ 0 par défaut), avec un
//! gain de coupe maximal, des bornes de fréquence et une pondération
//! asymétrique qui pénalise plus un résidu sous la cible (sur-correction)
//! qu'au-dessus.
//!
//! Sortie : bandes au format de l'égaliseur natif de Tune
//! (`{"freq", "gain", "q", "type": "peak"}`), prêtes pour `POST /zones/{id}/eq`.

use crate::analysis::LogResponse;
use crate::target::TargetCurve;
use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

/// Type de filtre, limité à ce que l'égaliseur natif accepte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BandType {
    Peak,
    LowShelf,
    HighShelf,
}

/// Une bande au format natif (`sdk/tune-plugin-equalizer`, `EqBandSpec`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Band {
    pub freq: f64,
    pub gain: f64,
    pub q: f64,
    #[serde(rename = "type")]
    pub kind: BandType,
    /// 0 = gauche, 1 = droite, absent = tous les canaux.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub channel: Option<u16>,
}

/// Corps de `POST /zones/{id}/eq` (`EqSettings` dans `routes/playback.rs`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EqSettings {
    pub enabled: bool,
    pub bands: Vec<Band>,
    pub headroom_mode: String,
}

/// Coefficients biquad normalisés (a0 = 1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Biquad {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl Biquad {
    /// Peaking EQ (RBJ), Q en Q « classique ».
    pub fn peaking(fs: f64, f0: f64, gain_db: f64, q: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * f0 / fs;
        let alpha = w0.sin() / (2.0 * q);
        let a0 = 1.0 + alpha / a;
        Self {
            b0: (1.0 + alpha * a) / a0,
            b1: (-2.0 * w0.cos()) / a0,
            b2: (1.0 - alpha * a) / a0,
            a1: (-2.0 * w0.cos()) / a0,
            a2: (1.0 - alpha / a) / a0,
        }
    }

    /// Low shelf (RBJ, pente S = 1 comme l'égaliseur natif).
    pub fn low_shelf(fs: f64, f0: f64, gain_db: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * f0 / fs;
        let alpha = w0.sin() / 2.0 * 2f64.sqrt();
        let (c, sa) = (w0.cos(), 2.0 * a.sqrt() * alpha);
        let a0 = (a + 1.0) + (a - 1.0) * c + sa;
        Self {
            b0: a * ((a + 1.0) - (a - 1.0) * c + sa) / a0,
            b1: 2.0 * a * ((a - 1.0) - (a + 1.0) * c) / a0,
            b2: a * ((a + 1.0) - (a - 1.0) * c - sa) / a0,
            a1: -2.0 * ((a - 1.0) + (a + 1.0) * c) / a0,
            a2: ((a + 1.0) + (a - 1.0) * c - sa) / a0,
        }
    }

    /// High shelf (RBJ, S = 1).
    pub fn high_shelf(fs: f64, f0: f64, gain_db: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * f0 / fs;
        let alpha = w0.sin() / 2.0 * 2f64.sqrt();
        let (c, sa) = (w0.cos(), 2.0 * a.sqrt() * alpha);
        let a0 = (a + 1.0) - (a - 1.0) * c + sa;
        Self {
            b0: a * ((a + 1.0) + (a - 1.0) * c + sa) / a0,
            b1: -2.0 * a * ((a - 1.0) + (a + 1.0) * c) / a0,
            b2: a * ((a + 1.0) + (a - 1.0) * c - sa) / a0,
            a1: 2.0 * ((a - 1.0) - (a + 1.0) * c) / a0,
            a2: ((a + 1.0) - (a - 1.0) * c - sa) / a0,
        }
    }

    pub fn from_band(fs: f64, b: &Band) -> Self {
        match b.kind {
            BandType::Peak => Self::peaking(fs, b.freq, b.gain, b.q),
            BandType::LowShelf => Self::low_shelf(fs, b.freq, b.gain),
            BandType::HighShelf => Self::high_shelf(fs, b.freq, b.gain),
        }
    }

    /// Module de la réponse (dB) à la fréquence `f`.
    pub fn magnitude_db(&self, fs: f64, f: f64) -> f64 {
        let w = 2.0 * PI * f / fs;
        let (c1, s1) = (w.cos(), w.sin());
        let (c2, s2) = ((2.0 * w).cos(), (2.0 * w).sin());
        // H(e^{jw}) = (b0 + b1 e^{-jw} + b2 e^{-2jw}) / (1 + a1 e^{-jw} + a2 e^{-2jw})
        let num_re = self.b0 + self.b1 * c1 + self.b2 * c2;
        let num_im = -(self.b1 * s1 + self.b2 * s2);
        let den_re = 1.0 + self.a1 * c1 + self.a2 * c2;
        let den_im = -(self.a1 * s1 + self.a2 * s2);
        let num = num_re * num_re + num_im * num_im;
        let den = den_re * den_re + den_im * den_im;
        10.0 * (num / den.max(1e-300)).max(1e-30).log10()
    }
}

/// Réponse cumulée (dB) d'une liste de bandes sur une grille.
pub fn bands_response_db(fs: f64, bands: &[Band], freqs: &[f64]) -> Vec<f64> {
    let biquads: Vec<Biquad> = bands.iter().map(|b| Biquad::from_band(fs, b)).collect();
    freqs.iter().map(|f| biquads.iter().map(|b| b.magnitude_db(fs, *f)).sum()).collect()
}

/// Garde-fous et bornes de l'ajustement.
#[derive(Debug, Clone, PartialEq)]
pub struct AutoEqConfig {
    /// Taux d'échantillonnage pour le calcul des coefficients (la réponse en
    /// dessous de fs/4 n'en dépend presque pas).
    pub sample_rate: f64,
    /// Zone de correction. Au-delà de la fréquence de Schroeder (~200-300 Hz
    /// en pièce domestique) la correction d'une mesure en un point est
    /// discutable : on borne par défaut à 20-300 Hz pour un seul point et
    /// jusqu'à 500 Hz-1 kHz pour une moyenne spatiale.
    pub f_lo: f64,
    pub f_hi: f64,
    /// Bande servant à aligner la cible sur la mesure.
    pub align_lo: f64,
    pub align_hi: f64,
    pub max_bands: usize,
    /// Coupe maximale (dB, positif).
    pub max_cut_db: f64,
    /// Relance maximale (dB, positif). 0 = coupes seulement (défaut).
    pub max_boost_db: f64,
    /// Écart en dessous duquel on ne corrige pas.
    pub threshold_db: f64,
    pub q_min: f64,
    pub q_max: f64,
    /// Poids d'un résidu sous la cible (sur-correction) relatif à un résidu au-dessus.
    pub undershoot_weight: f64,
    /// Marge imposée entre le gain cumulé maximal et 0 dBFS : c'est le
    /// préamplificateur à appliquer si des relances existent.
    pub clipping_margin_db: f64,
    /// Lissage 1/N d'octave appliqué à la mesure avant l'ajustement (REW
    /// recommande 1/6 pour l'égalisation ; 0 = aucun).
    pub pre_smoothing_n: usize,
    /// Percentile des écarts mesure − cible par lequel passe la cible dans la
    /// bande d'alignement (0,5 = médiane ; 0,3 = près du plancher entre les
    /// modes, coupes plus profondes et plus larges).
    pub align_percentile: f64,
}

impl AutoEqConfig {
    pub fn single_position(sample_rate: f64) -> Self {
        Self {
            sample_rate,
            f_lo: 20.0,
            f_hi: 300.0,
            align_lo: 40.0,
            align_hi: 300.0,
            max_bands: 8,
            max_cut_db: 12.0,
            max_boost_db: 0.0,
            threshold_db: 1.0,
            q_min: 0.5,
            q_max: 10.0,
            undershoot_weight: 3.0,
            clipping_margin_db: 0.5,
            pre_smoothing_n: 6,
            align_percentile: 0.3,
        }
    }

    pub fn spatial_average(sample_rate: f64) -> Self {
        Self { f_hi: 500.0, align_hi: 500.0, max_bands: 10, ..Self::single_position(sample_rate) }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AutoEqResult {
    pub bands: Vec<Band>,
    /// Décalage ajouté à la cible pour l'aligner sur la mesure.
    pub target_offset_db: f64,
    /// Coût efficace pondéré (dB) avant/après : dépassement + sur-correction induite.
    pub rms_before_db: f64,
    pub rms_after_db: f64,
    /// Dépassement efficace (dB) : RMS de la seule partie AU-DESSUS de la
    /// cible, ce qu'un égaliseur qui ne fait que couper peut réduire.
    pub overshoot_rms_before_db: f64,
    pub overshoot_rms_after_db: f64,
    /// Écart positif maximal (pic résiduel) avant/après.
    pub max_peak_before_db: f64,
    pub max_peak_after_db: f64,
    /// Gain cumulé maximal du correcteur sur toute la grille ; > 0 seulement si
    /// des relances sont permises.
    pub max_total_gain_db: f64,
    /// Préampli (dB, ≤ 0) à appliquer pour garder `clipping_margin_db`.
    pub preamp_db: f64,
    pub warnings: Vec<String>,
}

impl AutoEqResult {
    /// Corps prêt pour `POST /zones/{id}/eq`.
    pub fn to_eq_settings(&self) -> EqSettings {
        EqSettings { enabled: true, bands: self.bands.clone(), headroom_mode: "safe".into() }
    }
}

struct Problem<'a> {
    cfg: &'a AutoEqConfig,
    freqs: Vec<f64>,
    /// mesure lissée − cible (dB), > 0 = pic à couper.
    error: Vec<f64>,
}

/// Lissage 1/N d'octave (moyenne des dB) d'une courbe sur une grille log.
fn smooth_on_grid(freqs: &[f64], values: &[f64], n: usize) -> Vec<f64> {
    if n == 0 {
        return values.to_vec();
    }
    let half = 2f64.powf(0.5 / n as f64);
    (0..freqs.len())
        .map(|i| {
            let (lo, hi) = (freqs[i] / half, freqs[i] * half);
            let mut acc = 0.0;
            let mut cnt = 0usize;
            // La grille est croissante : on balaie autour de i.
            let mut j = i;
            while j > 0 && freqs[j - 1] >= lo {
                j -= 1;
            }
            while j < freqs.len() && freqs[j] <= hi {
                acc += values[j];
                cnt += 1;
                j += 1;
            }
            acc / cnt.max(1) as f64
        })
        .collect()
}

impl Problem<'_> {
    fn overshoot_rms(residual: &[f64]) -> f64 {
        (residual.iter().map(|r| r.max(0.0).powi(2)).sum::<f64>() / residual.len().max(1) as f64).sqrt()
    }

    /// Coût : dépassement résiduel au-dessus de la cible, plus la seule
    /// SUR-CORRECTION induite par l'égaliseur (ce qu'il a poussé sous la
    /// cible au-delà de ce qui y était déjà), pondérée `undershoot_weight`.
    /// Les creux préexistants ne pèsent pas : l'égaliseur n'y peut rien et
    /// ils ne doivent pas l'empêcher de couper un pic voisin avec le bon Q.
    fn weighted_rms(&self, residual: &[f64]) -> f64 {
        let mut acc = 0.0;
        for (r, e) in residual.iter().zip(&self.error) {
            let over = r.max(0.0);
            let extra_under = ((-r).max(0.0) - (-e).max(0.0)).max(0.0);
            acc += over * over + self.cfg.undershoot_weight * extra_under * extra_under;
        }
        (acc / residual.len().max(1) as f64).sqrt()
    }

    /// Résidu = mesure lissée − cible + réponse de l'égaliseur LISSÉE DE MÊME :
    /// le lissage est linéaire en dB, donc lisser(mesure + EQ) = lisser(mesure)
    /// + lisser(EQ). Sans cela, un biquad étroit serait comparé à un pic
    /// élargi par le lissage et l'ajustement choisirait des Q trop grands et
    /// des coupes trop faibles (défaut vu sur le prototype).
    fn residual(&self, bands: &[Band]) -> Vec<f64> {
        let corr = bands_response_db(self.cfg.sample_rate, bands, &self.freqs);
        let corr = smooth_on_grid(&self.freqs, &corr, self.cfg.pre_smoothing_n);
        self.error.iter().zip(corr).map(|(e, c)| e + c).collect()
    }

    /// La profondeur de coupe CUMULÉE ne dépasse pas `max_cut_db` : deux
    /// bandes empilées ne contournent pas le plafond d'une seule.
    fn feasible(&self, bands: &[Band]) -> bool {
        let corr = bands_response_db(self.cfg.sample_rate, bands, &self.freqs);
        corr.iter().all(|c| *c >= -self.cfg.max_cut_db - 1e-9 && *c <= self.cfg.max_boost_db + 1e-9)
    }

    /// Coût d'un candidat : erreur pondérée, ou +∞ s'il viole le plafond cumulé.
    fn cost(&self, bands: &[Band]) -> f64 {
        if !self.feasible(bands) {
            return f64::INFINITY;
        }
        self.weighted_rms(&self.residual(bands))
    }

    fn clamp(&self, b: &mut Band) {
        b.freq = b.freq.clamp(self.cfg.f_lo, self.cfg.f_hi);
        b.q = b.q.clamp(self.cfg.q_min, self.cfg.q_max);
        b.gain = b.gain.clamp(-self.cfg.max_cut_db, self.cfg.max_boost_db);
    }

    /// Descente de coordonnées sur (f, Q, gain) de la bande `idx`.
    fn refine(&self, bands: &mut [Band], idx: usize, passes: usize) {
        let mut best = self.cost(bands);
        let mut f_step = 2f64.powf(1.0 / 6.0);
        let mut q_step: f64 = 1.3;
        let mut g_step: f64 = 1.0;
        for _ in 0..passes {
            let mut improved = false;
            for dir in [1.0, -1.0] {
                let mut cand = bands.to_vec();
                cand[idx].freq *= f_step.powf(dir);
                self.clamp(&mut cand[idx]);
                let e = self.cost(&cand);
                if e < best - 1e-6 {
                    best = e;
                    bands[idx] = cand[idx].clone();
                    improved = true;
                }
            }
            for dir in [1.0, -1.0] {
                let mut cand = bands.to_vec();
                cand[idx].q *= q_step.powf(dir);
                self.clamp(&mut cand[idx]);
                let e = self.cost(&cand);
                if e < best - 1e-6 {
                    best = e;
                    bands[idx] = cand[idx].clone();
                    improved = true;
                }
            }
            for dir in [1.0, -1.0] {
                let mut cand = bands.to_vec();
                cand[idx].gain += g_step * dir;
                self.clamp(&mut cand[idx]);
                let e = self.cost(&cand);
                if e < best - 1e-6 {
                    best = e;
                    bands[idx] = cand[idx].clone();
                    improved = true;
                }
            }
            if !improved {
                f_step = f_step.sqrt();
                q_step = q_step.sqrt();
                g_step *= 0.5;
                if g_step < 0.05 {
                    break;
                }
            }
        }
    }
}

/// Ajuste un égaliseur paramétrique pour ramener `measured` vers `target`.
pub fn fit(measured: &LogResponse, target: &TargetCurve, cfg: &AutoEqConfig) -> AutoEqResult {
    let smoothed;
    let measured = if cfg.pre_smoothing_n > 0 {
        smoothed = measured.smoothed(cfg.pre_smoothing_n);
        &smoothed
    } else {
        measured
    };
    let offset = target.align_offset_percentile(measured, cfg.align_lo, cfg.align_hi, cfg.align_percentile);
    let mut freqs = Vec::new();
    let mut error = Vec::new();
    for (f, m) in measured.freqs.iter().zip(&measured.magnitude_db) {
        if *f >= cfg.f_lo && *f <= cfg.f_hi {
            freqs.push(*f);
            error.push(m - (target.level_db(*f) + offset));
        }
    }
    let mut warnings = Vec::new();
    if freqs.len() < 4 {
        warnings.push("zone de correction trop étroite pour la grille fournie".into());
    }
    let problem = Problem { cfg, freqs, error };
    let rms_before = problem.weighted_rms(&problem.error);
    let max_before = problem.error.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

    let mut bands: Vec<Band> = Vec::new();
    // Candidats déjà essayés sans gain : exclus à ±1/12 d'octave, pour passer
    // au pic suivant au lieu d'abandonner (réponses hachées).
    let mut excluded: Vec<f64> = Vec::new();
    let is_excluded = |f: f64, excluded: &[f64]| excluded.iter().any(|x| (f / x).log2().abs() < 1.0 / 12.0);
    let mut attempts = 0usize;
    while bands.len() < cfg.max_bands && attempts < cfg.max_bands * 3 {
        attempts += 1;
        let residual = problem.residual(&bands);
        // Pic le plus haut au-dessus de la cible, hors zones exclues.
        let (i_peak, e_peak) = residual
            .iter()
            .cloned()
            .enumerate()
            .filter(|(i, _)| !is_excluded(problem.freqs[*i], &excluded))
            .fold((0usize, f64::NEG_INFINITY), |m, (i, v)| if v > m.1 { (i, v) } else { m });
        let mut wants_boost = false;
        let (i_sel, e_sel) = if e_peak >= cfg.threshold_db {
            (i_peak, e_peak)
        } else if cfg.max_boost_db > 0.0 {
            // Creux le plus profond, si les relances sont permises.
            let (i_dip, e_dip) = residual
                .iter()
                .cloned()
                .enumerate()
                .filter(|(i, _)| !is_excluded(problem.freqs[*i], &excluded))
                .fold((0usize, f64::INFINITY), |m, (i, v)| if v < m.1 { (i, v) } else { m });
            if -e_dip < cfg.threshold_db {
                break;
            }
            wants_boost = true;
            (i_dip, e_dip)
        } else {
            break;
        };
        // Largeur à mi-hauteur → Q initial.
        let half = e_sel / 2.0;
        let above = |v: f64| if wants_boost { v <= half } else { v >= half };
        let mut lo = i_sel;
        while lo > 0 && above(residual[lo - 1]) {
            lo -= 1;
        }
        let mut hi = i_sel;
        while hi + 1 < residual.len() && above(residual[hi + 1]) {
            hi += 1;
        }
        let f0 = problem.freqs[i_sel];
        let bw = (problem.freqs[hi] - problem.freqs[lo]).max(f0 * 0.05);
        let mut band = Band { freq: f0, gain: -e_sel, q: f0 / bw, kind: BandType::Peak, channel: None };
        problem.clamp(&mut band);
        let before = problem.weighted_rms(&residual);
        bands.push(band);
        let last = bands.len() - 1;
        // Respect du plafond cumulé dès l'insertion : on rapproche le gain de 0
        // jusqu'à ce que l'empilement soit admissible.
        let mut guard = 0;
        while !problem.feasible(&bands) && guard < 60 {
            bands[last].gain *= 0.8;
            guard += 1;
        }
        if !problem.feasible(&bands) {
            bands.pop();
            excluded.push(f0);
            continue;
        }
        problem.refine(&mut bands, last, 40);
        let after = problem.weighted_rms(&problem.residual(&bands));
        if before - after < 0.02 {
            bands.pop();
            excluded.push(f0);
        }
    }
    // Raffinement conjoint.
    for _ in 0..2 {
        for i in 0..bands.len() {
            problem.refine(&mut bands, i, 20);
        }
    }
    bands.retain(|b| b.gain.abs() >= 0.1);
    bands.sort_by(|a, b| a.freq.partial_cmp(&b.freq).unwrap());

    let residual = problem.residual(&bands);
    let rms_after = problem.weighted_rms(&residual);
    let overshoot_before = Problem::overshoot_rms(&problem.error);
    let overshoot_after = Problem::overshoot_rms(&residual);
    let max_after = residual.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

    // Gain cumulé sur une grille large (pas seulement la zone corrigée).
    let wide = LogResponse::grid(10.0, cfg.sample_rate * 0.45, 24);
    let total = bands_response_db(cfg.sample_rate, &bands, &wide);
    let max_total = total.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let preamp = if max_total > 0.0 { -(max_total + cfg.clipping_margin_db) } else { 0.0 };
    if max_total > cfg.max_boost_db + 1e-6 {
        warnings.push(format!("gain cumulé {max_total:.2} dB au-delà de la relance autorisée"));
    }
    if max_after > cfg.threshold_db + 1.0 {
        warnings.push(format!("pic résiduel de {max_after:.1} dB : plus de bandes ou coupe plus forte nécessaires"));
    }

    AutoEqResult {
        bands,
        target_offset_db: offset,
        rms_before_db: rms_before,
        rms_after_db: rms_after,
        overshoot_rms_before_db: overshoot_before,
        overshoot_rms_after_db: overshoot_after,
        max_peak_before_db: max_before,
        max_peak_after_db: max_after,
        max_total_gain_db: max_total,
        preamp_db: preamp,
        warnings,
    }
}

/// Export au format « Filter Settings file » de REW (importable par REW et
/// par les outils miniDSP/Equalizer APO qui lisent ce texte).
pub fn to_rew_filters(bands: &[Band], preamp_db: f64) -> String {
    let mut s = String::from("Filter Settings file\n\nRoom EQ V5.31\nNotes:Tune calibration (#7632)\nEqualiser: Generic\n");
    s.push_str(&format!("Preamp: {preamp_db:.1} dB\n"));
    for (i, b) in bands.iter().enumerate() {
        let kind = match b.kind {
            BandType::Peak => "PK",
            BandType::LowShelf => "LS",
            BandType::HighShelf => "HS",
        };
        s.push_str(&format!(
            "Filter {:2}: ON  {kind}       Fc {:8.2} Hz  Gain {:6.2} dB  Q {:6.3}\n",
            i + 1,
            b.freq,
            b.gain,
            b.q
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peaking_a_le_bon_gain_au_centre_et_zero_loin() {
        let b = Biquad::peaking(48_000.0, 100.0, -6.0, 4.0);
        assert!((b.magnitude_db(48_000.0, 100.0) + 6.0).abs() < 1e-6);
        assert!(b.magnitude_db(48_000.0, 5000.0).abs() < 0.05);
    }

    #[test]
    fn serialisation_au_format_natif() {
        let b = Band { freq: 55.5, gain: -4.25, q: 3.0, kind: BandType::Peak, channel: None };
        let j = serde_json::to_value(&b).unwrap();
        assert_eq!(j, serde_json::json!({"freq": 55.5, "gain": -4.25, "q": 3.0, "type": "peak"}));
        let s = EqSettings { enabled: true, bands: vec![b], headroom_mode: "safe".into() };
        let j = serde_json::to_string(&s).unwrap();
        assert!(j.contains("\"headroom_mode\":\"safe\""));
    }

    #[test]
    fn retrouve_un_pic_unique() {
        let fs = 48_000.0;
        let grid = LogResponse::grid(20.0, 1000.0, 24);
        let src = Band { freq: 60.0, gain: 8.0, q: 4.0, kind: BandType::Peak, channel: None };
        let mags = bands_response_db(fs, &[src], &grid);
        let measured = LogResponse { freqs: grid, magnitude_db: mags.iter().map(|m| m + 75.0).collect() };
        let r = fit(&measured, &TargetCurve::flat(), &AutoEqConfig::single_position(fs));
        assert_eq!(r.bands.len(), 1, "{:?}", r.bands);
        let b = &r.bands[0];
        assert!((b.freq / 60.0).log2().abs() < 0.08, "f {}", b.freq);
        assert!((b.gain + 8.0).abs() < 1.0, "gain {}", b.gain);
        assert!(r.max_peak_after_db < 1.0);
        assert_eq!(r.preamp_db, 0.0);
    }

    #[test]
    fn ne_remonte_jamais_un_creux() {
        let fs = 48_000.0;
        let grid = LogResponse::grid(20.0, 1000.0, 24);
        let dip = Band { freq: 80.0, gain: -10.0, q: 3.0, kind: BandType::Peak, channel: None };
        let mags = bands_response_db(fs, &[dip], &grid);
        let measured = LogResponse { freqs: grid, magnitude_db: mags };
        let r = fit(&measured, &TargetCurve::flat(), &AutoEqConfig::single_position(fs));
        assert!(r.bands.iter().all(|b| b.gain <= 0.0), "{:?}", r.bands);
        assert!(r.max_total_gain_db <= 1e-9);
    }

    #[test]
    fn coupe_bornee_au_maximum() {
        let fs = 48_000.0;
        let grid = LogResponse::grid(20.0, 1000.0, 24);
        let src = Band { freq: 45.0, gain: 20.0, q: 6.0, kind: BandType::Peak, channel: None };
        let mags = bands_response_db(fs, &[src], &grid);
        let measured = LogResponse { freqs: grid, magnitude_db: mags };
        let cfg = AutoEqConfig { max_cut_db: 10.0, ..AutoEqConfig::single_position(fs) };
        let r = fit(&measured, &TargetCurve::flat(), &cfg);
        assert!(r.bands.iter().all(|b| b.gain >= -10.0 - 1e-9), "{:?}", r.bands);
        assert!(r.warnings.iter().any(|w| w.contains("pic résiduel")), "{:?}", r.warnings);
    }
}
