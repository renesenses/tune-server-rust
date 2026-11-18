//! Esquisse C3 : filtre FIR de correction en phase mixte et coût de la
//! convolution dans `tune-core/src/audio/convolver.rs`.
//!
//! - `minimum_phase_from_magnitude` : réponse impulsionnelle à phase minimale
//!   d'une magnitude donnée, par le cepstre réel (Oppenheim & Schafer,
//!   « Discrete-Time Signal Processing », § 13.5 ; même méthode que
//!   `scipy.signal.minimum_phase(method="homomorphic")`).
//! - `mixed_phase_design` : en dessous de `split_hz`, correction en phase
//!   minimale de la magnitude de l'égaliseur (les modes de pièce sont
//!   eux-mêmes à phase minimale, d'où l'absence de pré-écho) ; au-dessus,
//!   identité à phase linéaire (le FIR n'intervient pas, la correction HF
//!   restant à l'égaliseur IIR). Le résultat est un FIR prêt pour
//!   `ConvolverConfig::new` (f32, un vecteur par canal).
//! - `convolution_cost` : ordre de grandeur du coût CPU du convolueur
//!   partitionné uniforme de Tune (bloc 1024, FFT 2048) par canal.

use crate::fft;
use realfft::num_complex::Complex;

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
    // log|H| sur le spectre complet (symétrie hermitienne implicite via rfft/irfft).
    let log_mag: Vec<Complex<f64>> = (0..bins)
        .map(|i| {
            Complex::new(
                fft::from_db(magnitude_db(i as f64 * df)).max(1e-9).ln(),
                0.0,
            )
        })
        .collect();
    let cep = fft::irfft(&log_mag, nfft); // cepstre réel
                                          // Repliement : c[0], 2c[n] pour 0<n<N/2, c[N/2], 0 ensuite.
    let mut folded = vec![0.0; nfft];
    folded[0] = cep[0];
    for n in 1..nfft / 2 {
        folded[n] = 2.0 * cep[n];
    }
    folded[nfft / 2] = cep[nfft / 2];
    let spec = fft::rfft(&folded, nfft);
    let h_spec: Vec<Complex<f64>> = spec.iter().map(|c| c.exp()).collect();
    fft::irfft(&h_spec, nfft)
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

// ─────────────────────────────────────────────────────────────────────────
// C3 (lot TESS T10b) : FIR de correction calculé depuis la MESURE.
// ─────────────────────────────────────────────────────────────────────────
//
// ```text
// réponse mesurée (moyenne multi-positions de T10a, grille log 1/12 oct.)
//    │ lissage variable : résolution complète sous `transition_lo_hz`,
//    │ 1/`lissage_hf` d'octave au-dessus de `transition_hi_hz` (fondu entre)
//    ▼
// tendance (médiane glissante d'une octave) : niveau « vrai » de la pièce
//    │ large  = cible − tendance (cible alignée sur la tendance)
//    │ étroit = tendance − mesure : pics (modes) coupés ; CREUX remontés
//    │          seulement s'ils sont peu profonds (rien au-delà de 2R)
//    │ bornes : coupe ≤ `coupe_max_db`, remontée totale ≤ `remontee_max_db`
//    │ hors [f_min, f_max] : fondu vers 0 dB (rien n'est poussé hors bande)
//    ▼
// correction C(f) = C_bas(f) + C_haut(f)   (partage à `coupure_mixte_hz`)
//    │ C_bas → phase minimale (cepstre) : les modes sont à phase minimale,
//    │         la correction ne crée pas de pré-écho
//    │ C_haut → phase linéaire (symétrique, fenêtrée) : aucune rotation de
//    │          phase dans le médium-aigu, latence = moitié de sa longueur
//    ▼
// h = h_bas ⊛ h_haut, tronqué en douceur, normalisé (gain max ≤ 0 dB)
//    ▼
// RI `.wav` flottant (fmt + data), relue par `Convolver::read_wav_ir`
// ```
//
// La correction est en MAGNITUDE seule : l'excès de phase d'une pièce dépend
// de la place et ne se corrige pas sans risque hors du point mesuré.

use crate::analysis::LogResponse;
use crate::target::TargetCurve;
use serde::{Deserialize, Serialize};

/// Bornes et réglages de la conception (défauts prudents).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfigFir {
    /// Fréquence d'échantillonnage du filtre produit.
    pub sample_rate: u32,
    /// Longueur du FIR (coefficients). 0 = automatique (≈ 0,68 s).
    pub taps: usize,
    /// Longueur de la partie à phase linéaire (coefficients, paire).
    /// 0 = automatique (≈ 85 ms, latence ≈ 43 ms).
    pub taps_lineaire: usize,
    /// Bande corrigée ; hors bande, la correction s'éteint sur une demi-octave.
    pub f_min_hz: f64,
    pub f_max_hz: f64,
    /// Partage phase minimale (dessous) / phase linéaire (dessus).
    pub coupure_mixte_hz: f64,
    /// Coupe maximale (dB, > 0).
    pub coupe_max_db: f64,
    /// Remontée maximale (dB, ≥ 0), au total. Un creux étroit n'est remonté
    /// que s'il est peu profond : voir [`correction_bornee`].
    pub remontee_max_db: f64,
    /// Résolution complète sous ce seuil (zone modale).
    pub transition_lo_hz: f64,
    /// Lissage 1/`lissage_hf` d'octave au-dessus de ce seuil.
    pub transition_hi_hz: f64,
    pub lissage_hf: usize,
    /// Bande où la cible est alignée sur la mesure (médiane des écarts).
    pub alignement_lo_hz: f64,
    pub alignement_hi_hz: f64,
    /// Ramène le gain maximal du filtre à 0 dB (aucun écrêtage possible).
    pub normaliser: bool,
}

impl Default for ConfigFir {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            taps: 0,
            taps_lineaire: 0,
            f_min_hz: 25.0,
            f_max_hz: 16_000.0,
            coupure_mixte_hz: 300.0,
            coupe_max_db: 15.0,
            remontee_max_db: 4.0,
            transition_lo_hz: 300.0,
            transition_hi_hz: 1000.0,
            lissage_hf: 3,
            alignement_lo_hz: 200.0,
            alignement_hi_hz: 2000.0,
            normaliser: true,
        }
    }
}

/// Bornes dures qu'aucun réglage ne dépasse (même en mode expert).
pub const COUPE_MAX_ABSOLUE_DB: f64 = 24.0;
pub const REMONTEE_MAX_ABSOLUE_DB: f64 = 6.0;
pub const TAPS_MAX: usize = 262_144;

impl ConfigFir {
    pub fn pour(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            ..Self::default()
        }
    }

    pub fn taps_effectifs(&self) -> usize {
        if self.taps > 0 {
            self.taps
        } else {
            fft::next_pow2((self.sample_rate as f64 * 0.68) as usize)
        }
    }

    pub fn taps_lineaire_effectifs(&self) -> usize {
        let t = if self.taps_lineaire > 0 {
            self.taps_lineaire
        } else {
            fft::next_pow2((self.sample_rate as f64 * 0.085) as usize)
        };
        t & !1
    }

    /// Refus motivé d'un réglage hors des bornes.
    pub fn valider(&self) -> Result<(), String> {
        let fs = self.sample_rate as f64;
        if !(8_000..=768_000).contains(&self.sample_rate) {
            return Err(format!(
                "fréquence {} Hz hors de 8–768 kHz",
                self.sample_rate
            ));
        }
        if !(self.coupe_max_db > 0.0 && self.coupe_max_db <= COUPE_MAX_ABSOLUE_DB) {
            return Err(format!(
                "coupe maximale {} dB : entre 0 et {COUPE_MAX_ABSOLUE_DB} dB",
                self.coupe_max_db
            ));
        }
        if !(self.remontee_max_db >= 0.0 && self.remontee_max_db <= REMONTEE_MAX_ABSOLUE_DB) {
            return Err(format!(
                "remontée maximale {} dB : entre 0 et {REMONTEE_MAX_ABSOLUE_DB} dB",
                self.remontee_max_db
            ));
        }
        if !(self.f_min_hz >= 10.0 && self.f_min_hz < self.f_max_hz && self.f_max_hz < fs * 0.45) {
            return Err(format!(
                "bande {}–{} Hz invalide à {} Hz",
                self.f_min_hz, self.f_max_hz, self.sample_rate
            ));
        }
        if !(self.coupure_mixte_hz > self.f_min_hz && self.coupure_mixte_hz < self.f_max_hz) {
            return Err("coupure phase minimale / linéaire hors de la bande".into());
        }
        if !(self.transition_lo_hz > 0.0 && self.transition_lo_hz < self.transition_hi_hz) {
            return Err("transition de lissage invalide".into());
        }
        if self.lissage_hf == 0 || self.lissage_hf > 48 {
            return Err("lissage 1/N d'octave : N entre 1 et 48".into());
        }
        let (t, tl) = (self.taps_effectifs(), self.taps_lineaire_effectifs());
        if t > TAPS_MAX || tl < 64 || tl * 2 > t {
            return Err(format!(
                "longueurs {t} / {tl} : FIR ≤ {TAPS_MAX}, partie linéaire ≥ 64 et ≤ moitié du FIR"
            ));
        }
        Ok(())
    }
}

/// Le filtre conçu et ce qu'il a fallu borner.
#[derive(Debug, Clone, PartialEq)]
pub struct FirMesure {
    pub sample_rate: u32,
    pub taps: Vec<f64>,
    /// Correction visée après bornes (dB, avant normalisation), sur la grille
    /// de la mesure : c'est la courbe que l'écran affiche.
    pub correction: LogResponse,
    /// Décalage de cible appliqué (dB) : la cible passe par la médiane.
    pub decalage_cible_db: f64,
    /// Gain retiré pour que le filtre ne dépasse pas 0 dB.
    pub normalisation_db: f64,
    /// Latence (échantillons) : moitié de la partie à phase linéaire.
    pub latence_echantillons: usize,
    /// Points de la bande où une borne a agi (coupe, remontée, creux).
    pub points_bornes: usize,
    pub points_creux_non_combles: usize,
    pub avertissements: Vec<String>,
}

impl FirMesure {
    pub fn latence_ms(&self) -> f64 {
        self.latence_echantillons as f64 * 1000.0 / self.sample_rate as f64
    }
}

fn fondu_cos(t: f64) -> f64 {
    // 1 → 0 quand t va de 0 à 1.
    0.5 + 0.5 * (std::f64::consts::PI * t.clamp(0.0, 1.0)).cos()
}

/// Poids (0..1) du lissage HF à la fréquence `f`.
fn poids_hf(cfg: &ConfigFir, f: f64) -> f64 {
    if f <= cfg.transition_lo_hz {
        0.0
    } else if f >= cfg.transition_hi_hz {
        1.0
    } else {
        let t =
            (f / cfg.transition_lo_hz).ln() / (cfg.transition_hi_hz / cfg.transition_lo_hz).ln();
        1.0 - fondu_cos(t)
    }
}

/// Enveloppe de la bande (0..1) : 1 dans [f_min, f_max], fondu sur une
/// demi-octave au-delà.
fn poids_bande(cfg: &ConfigFir, f: f64) -> f64 {
    let demi = 2f64.sqrt();
    if f < cfg.f_min_hz {
        let t = (cfg.f_min_hz / f.max(1e-9)).ln() / demi.ln();
        fondu_cos(t)
    } else if f > cfg.f_max_hz {
        let t = (f / cfg.f_max_hz).ln() / demi.ln();
        fondu_cos(t)
    } else {
        1.0
    }
}

/// Lissage variable de la mesure : résolution complète dans la zone modale,
/// 1/`lissage_hf` d'octave au-dessus (fondu entre les deux transitions). La
/// conception ET le verdict s'y lisent : au-dessus de Schroeder, ce qui
/// reste plus fin que le tiers d'octave dépend de la place (peignes de
/// réflexions) et ne doit pas être corrigé.
pub fn lissage_variable(mesure: &LogResponse, cfg: &ConfigFir) -> LogResponse {
    let hf = mesure.smoothed(cfg.lissage_hf);
    LogResponse {
        freqs: mesure.freqs.clone(),
        magnitude_db: (0..mesure.freqs.len())
            .map(|i| {
                let w = poids_hf(cfg, mesure.freqs[i]);
                (1.0 - w) * mesure.magnitude_db[i] + w * hf.magnitude_db[i]
            })
            .collect(),
    }
}

/// Tendance large (dB) : MÉDIANE glissante sur une octave. Les modes (pics
/// étroits) et les annulations (creux étroits) n'occupent qu'une petite part
/// de l'octave : la médiane passe entre eux, au niveau « vrai » de la pièce.
pub fn tendance(r: &LogResponse) -> LogResponse {
    let demi = 2f64.sqrt();
    let mut tampon = Vec::new();
    LogResponse {
        freqs: r.freqs.clone(),
        magnitude_db: r
            .freqs
            .iter()
            .map(|&f| {
                tampon.clear();
                tampon.extend(
                    r.freqs
                        .iter()
                        .zip(&r.magnitude_db)
                        .filter(|(g, _)| **g >= f / demi && **g <= f * demi)
                        .map(|(_, m)| *m),
                );
                tampon.sort_by(|a, b| a.total_cmp(b));
                tampon[tampon.len() / 2]
            })
            .collect(),
    }
}

/// Correction bornée (dB) sur la grille de la mesure.
#[derive(Debug, Clone, PartialEq)]
pub struct CorrectionBornee {
    pub correction: LogResponse,
    pub decalage_cible_db: f64,
    pub points_bornes: usize,
    pub points_creux_non_combles: usize,
}

/// Deux étages, bornés séparément :
///
/// * **large** : cible − tendance (médiane d'octave), dans
///   [−`coupe_max_db`, +`remontee_max_db`] — c'est la forme de la cible
///   (pente, bas du spectre), qui ne touche à aucun creux ;
/// * **étroit** : tendance − mesure lissée. Un pic (mode) est coupé en
///   entier, jusqu'à `coupe_max_db` au total ; un CREUX n'est remonté que
///   s'il est peu profond : remontée permise = clamp(2R − profondeur, 0, R)
///   avec R = `remontee_max_db`. Un creux de 2R ou plus n'est pas remonté
///   du tout : c'est une annulation, la remplir coûte de la puissance et ne
///   la comble pas.
pub fn correction_bornee(
    mesure: &LogResponse,
    cible: &TargetCurve,
    cfg: &ConfigFir,
) -> Result<CorrectionBornee, String> {
    cfg.valider()?;
    if mesure.freqs.len() < 16 || mesure.freqs.len() != mesure.magnitude_db.len() {
        return Err("mesure vide ou incohérente".into());
    }
    if mesure
        .freqs
        .iter()
        .chain(&mesure.magnitude_db)
        .any(|m| !m.is_finite())
        || mesure.freqs.windows(2).any(|w| w[1] <= w[0])
    {
        return Err("mesure non finie ou grille non croissante".into());
    }
    if mesure.freqs[0] > cfg.f_min_hz * 1.2 || *mesure.freqs.last().unwrap() < cfg.f_max_hz / 1.2 {
        return Err(format!(
            "la mesure ({:.0}–{:.0} Hz) ne couvre pas la bande corrigée {}–{} Hz",
            mesure.freqs[0],
            mesure.freqs.last().unwrap(),
            cfg.f_min_hz,
            cfg.f_max_hz
        ));
    }
    let lisse = lissage_variable(mesure, cfg);
    let large = tendance(&lisse);
    let decalage = cible.align_offset(&large, cfg.alignement_lo_hz, cfg.alignement_hi_hz);
    let r = cfg.remontee_max_db;
    let (mut bornes, mut creux) = (0usize, 0usize);
    let corr: Vec<f64> = (0..mesure.freqs.len())
        .map(|i| {
            let f = mesure.freqs[i];
            let voulu_large = cible.level_db(f) + decalage - large.magnitude_db[i];
            let c_large = voulu_large.clamp(-cfg.coupe_max_db, r);
            let profondeur = large.magnitude_db[i] - lisse.magnitude_db[i]; // > 0 : creux
            let c_etroit = if profondeur > 0.0 {
                profondeur.min((2.0 * r - profondeur).clamp(0.0, r))
            } else {
                profondeur
            };
            let voulu = voulu_large + profondeur;
            let c = (c_large + c_etroit).clamp(-cfg.coupe_max_db, r);
            let dans_bande = f >= cfg.f_min_hz && f <= cfg.f_max_hz;
            if dans_bande && (c - voulu).abs() > 0.05 {
                bornes += 1;
                if profondeur > 0.0 && c_etroit < profondeur - 0.05 {
                    creux += 1;
                }
            }
            c * poids_bande(cfg, f)
        })
        .collect();
    Ok(CorrectionBornee {
        correction: LogResponse {
            freqs: mesure.freqs.clone(),
            magnitude_db: corr,
        },
        decalage_cible_db: decalage,
        points_bornes: bornes,
        points_creux_non_combles: creux,
    })
}

/// FIR à phase linéaire (symétrique, fenêtre de Hann) de longueur `n` paire
/// moins un (impaire, centre exact) et de module `mag_db(f)`.
fn phase_lineaire(sample_rate: u32, n: usize, mag_db: impl Fn(f64) -> f64) -> Vec<f64> {
    let n = n | 1; // impaire : centre exact
    let nfft = fft::next_pow2(n * 4);
    let bins = nfft / 2 + 1;
    let df = sample_rate as f64 / nfft as f64;
    let spec: Vec<Complex<f64>> = (0..bins)
        .map(|i| Complex::new(fft::from_db(mag_db(i as f64 * df)), 0.0))
        .collect();
    let zero = fft::irfft(&spec, nfft); // phase nulle, centrée en 0 (circulaire)
    let m = n / 2;
    (0..n)
        .map(|k| {
            let idx = (k as isize - m as isize).rem_euclid(nfft as isize) as usize;
            let w = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * k as f64 / (n - 1) as f64).cos();
            zero[idx] * w
        })
        .collect()
}

/// Conçoit le FIR de correction depuis une réponse mesurée (de préférence la
/// moyenne multi-positions de [`crate::positions::moyenne_positions`]).
pub fn concevoir_fir(
    mesure: &LogResponse,
    cible: &TargetCurve,
    cfg: &ConfigFir,
) -> Result<FirMesure, String> {
    let CorrectionBornee {
        correction,
        decalage_cible_db: decalage,
        points_bornes: bornes,
        points_creux_non_combles: creux,
    } = correction_bornee(mesure, cible, cfg)?;
    let fs = cfg.sample_rate;
    let taps = cfg.taps_effectifs();
    let tl = cfg.taps_lineaire_effectifs();
    let split = cfg.coupure_mixte_hz;
    let haut = split * 2f64.sqrt();
    let w_bas = |f: f64| -> f64 {
        if f <= split {
            1.0
        } else if f >= haut {
            0.0
        } else {
            fondu_cos((f / split).ln() / (haut / split).ln())
        }
    };
    // `correction.at` prolonge la grille par sa valeur d'extrémité : on y
    // réapplique l'enveloppe de bande pour que DC et Nyquist restent à 0 dB.
    let c = |f: f64| correction.at(f) * poids_bande(cfg, f);
    let n_min = taps - tl;
    let mut h_bas =
        minimum_phase_from_magnitude(fs, fft::next_pow2(n_min * 2), |f| w_bas(f) * c(f));
    h_bas.truncate(n_min);
    let fade = n_min / 4;
    for i in 0..fade {
        h_bas[n_min - fade + i] *= fondu_cos(i as f64 / fade as f64);
    }
    let h_haut = phase_lineaire(fs, tl - 1, |f| (1.0 - w_bas(f)) * c(f));
    let mut h = fft::convolve(&h_bas, &h_haut);
    h.truncate(taps);
    h.resize(taps, 0.0);

    // Normalisation : gain maximal du filtre (grille fine) ramené à 0 dB.
    let mut normalisation = 0.0;
    if cfg.normaliser {
        let nfft = fft::next_pow2(taps * 2);
        let spec = fft::rfft(&h, nfft);
        let max = spec.iter().map(|z| z.norm()).fold(0.0, f64::max);
        let max_db = fft::to_db(max);
        if max_db > 0.0 {
            let g = fft::from_db(-max_db);
            h.iter_mut().for_each(|v| *v *= g);
            normalisation = max_db;
        }
    }
    let mut avertissements = Vec::new();
    if creux > 0 {
        avertissements.push(format!(
            "{creux} points en creux non comblés : remontée bornée à {} dB, nulle au-delà d'un creux de {} dB",
            cfg.remontee_max_db,
            2.0 * cfg.remontee_max_db
        ));
    }
    Ok(FirMesure {
        sample_rate: fs,
        taps: h,
        correction,
        decalage_cible_db: decalage,
        normalisation_db: normalisation,
        latence_echantillons: (tl - 1) / 2,
        points_bornes: bornes,
        points_creux_non_combles: creux,
        avertissements,
    })
}

/// Format d'échantillon de la RI produite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormatRi {
    /// IEEE float 32 bits : relu par Tune, REW, Convolver, foobar2000.
    #[default]
    Float32,
    /// IEEE float 64 bits : relu par Tune (`read_wav_ir`, moteur F64 T09b).
    Float64,
}

/// RI `.wav` (RIFF, `fmt ` + `data`, IEEE float) : un vecteur de
/// coefficients par canal, entrelacés. Même forme que
/// `convolver::ecrire_wav_float32`, relue par `Convolver::read_wav_ir`
/// (et l'import T09a).
pub fn wav_ri(canaux: &[&[f64]], sample_rate: u32, format: FormatRi) -> Result<Vec<u8>, String> {
    if canaux.is_empty() || canaux.len() > 32 {
        return Err("entre 1 et 32 canaux".into());
    }
    let n = canaux[0].len();
    if n == 0 || canaux.iter().any(|c| c.len() != n) {
        return Err("canaux vides ou de longueurs différentes".into());
    }
    if canaux.iter().flat_map(|c| c.iter()).any(|v| !v.is_finite()) {
        return Err("coefficient non fini".into());
    }
    let octets: u16 = match format {
        FormatRi::Float32 => 4,
        FormatRi::Float64 => 8,
    };
    let nc = canaux.len() as u16;
    let bloc = nc * octets;
    let taille = n as u64 * bloc as u64;
    if taille + 36 > u32::MAX as u64 {
        return Err("RI trop longue pour un WAV".into());
    }
    let taille = taille as u32;
    let mut w = Vec::with_capacity(44 + taille as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + taille).to_le_bytes());
    w.extend_from_slice(b"WAVE");
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    w.extend_from_slice(&nc.to_le_bytes());
    w.extend_from_slice(&sample_rate.to_le_bytes());
    w.extend_from_slice(&(sample_rate * bloc as u32).to_le_bytes());
    w.extend_from_slice(&bloc.to_le_bytes());
    w.extend_from_slice(&(octets * 8).to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&taille.to_le_bytes());
    for i in 0..n {
        for c in canaux {
            match format {
                FormatRi::Float32 => w.extend_from_slice(&(c[i] as f32).to_le_bytes()),
                FormatRi::Float64 => w.extend_from_slice(&c[i].to_le_bytes()),
            }
        }
    }
    Ok(w)
}

/// Module (dB) d'un FIR f64 sur une grille, par FFT zéro-paddée puis
/// interpolation : rapide pour 30 000 coefficients et 500 fréquences.
pub fn module_fir_db(h: &[f64], sample_rate: u32, freqs: &[f64]) -> Vec<f64> {
    let nfft = fft::next_pow2(h.len().max(sample_rate as usize) * 2);
    let spec = fft::rfft(h, nfft);
    let df = sample_rate as f64 / nfft as f64;
    let db: Vec<f64> = spec.iter().map(|z| fft::to_db(z.norm())).collect();
    freqs
        .iter()
        .map(|f| {
            let x = f / df;
            let i = (x.floor() as usize).min(db.len() - 2);
            let t = x - i as f64;
            db[i] * (1.0 - t) + db[i + 1] * t
        })
        .collect()
}

/// Réponse corrigée PRÉVUE : la mesure plus le module du filtre. La pièce
/// et la convolution sont linéaires : c'est exactement ce qu'une remesure
/// correction active donnerait, aux erreurs de mesure près.
pub fn reponse_prevue(mesure: &LogResponse, f: &FirMesure) -> LogResponse {
    let g = module_fir_db(&f.taps, f.sample_rate, &mesure.freqs);
    LogResponse {
        freqs: mesure.freqs.clone(),
        magnitude_db: mesure
            .magnitude_db
            .iter()
            .zip(g)
            .map(|(m, g)| m + g)
            .collect(),
    }
}

/// Pire écart (fréquence, dB) d'une réponse à la cible dans `[f_lo, f_hi]`,
/// lue avec le lissage variable de `cfg` ; la cible est réalignée sur la
/// médiane (bande d'alignement de `cfg`) : le niveau absolu ne compte pas.
/// C'est le verdict de T10b (±1,5 dB sur 30 Hz–15 kHz).
pub fn pire_ecart_a_la_cible(
    reponse: &LogResponse,
    cible: &TargetCurve,
    cfg: &ConfigFir,
    f_lo: f64,
    f_hi: f64,
) -> (f64, f64) {
    let r = lissage_variable(reponse, cfg);
    let off = cible.align_offset(&r, cfg.alignement_lo_hz, cfg.alignement_hi_hz);
    r.freqs
        .iter()
        .zip(&r.magnitude_db)
        .filter(|(f, _)| **f >= f_lo && **f <= f_hi)
        .map(|(f, m)| (*f, m - cible.level_db(*f) - off))
        .fold((0.0, 0.0), |a, b| if b.1.abs() > a.1.abs() { b } else { a })
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
}
