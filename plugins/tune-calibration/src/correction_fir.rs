//! C3 (lot TESS T10b) : le FIR de correction calculé depuis la MESURE, par
//! les routes du greffon.
//!
//! * `POST /sessions/{id}/fir` et `POST /mesures/{id}/fir` — le rapport :
//!   correction prévue (courbe), latence, bornes qui ont agi, avertissements ;
//! * `POST /sessions/{id}/fir.wav` et `POST /mesures/{id}/fir.wav` — la RI
//!   `.wav` (IEEE float, `fmt ` + `data`), au format que la convolution relit
//!   (`Convolver::read_wav_ir`, import T09a).
//!
//! Une session à plusieurs positions est la source recommandée (moyenne de
//! T10a) ; une mesure en un point est acceptée avec un avertissement.
//!
//! **Premium (décision D1 du 18/11/2026)** : la correction MESURÉE, dont ce
//! FIR, est Premium. Ces routes vivent sous la garde du routeur entier
//! (`routes::garde_premium`) et l'épreuve
//! `le_fir_mesure_est_premium` le vérifie route par route. La correction
//! calculée SANS micro (`/simulations`) n'est pas concernée ici.
//!
//! Rien n'est joué, rien n'est écrit dans la zone : le fichier est rendu à
//! l'utilisateur, qui le charge en convolution.

use serde::{Deserialize, Serialize};
use tune_calibration::analysis::LogResponse;
use tune_calibration::fir::{self, ConfigFir, FirMesure, FormatRi};
use tune_calibration::positions::POSITIONS_MIN;

use crate::correction::Cible;

/// Fréquences d'échantillonnage pour lesquelles un FIR est conçu.
pub const FREQUENCES_FIR: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

/// Corps des routes FIR. Tout est facultatif ; les bornes dures de la
/// caisse (`fir::COUPE_MAX_ABSOLUE_DB`, `fir::REMONTEE_MAX_ABSOLUE_DB`) ne
/// bougent pas.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct DemandeFir {
    #[serde(default)]
    pub cible: Cible,
    pub frequence: Option<u32>,
    pub coupe_max_db: Option<f64>,
    pub remontee_max_db: Option<f64>,
    pub f_min_hz: Option<f64>,
    pub f_max_hz: Option<f64>,
    #[serde(default)]
    pub format: FormatRi,
}

impl DemandeFir {
    pub fn config(&self) -> Result<ConfigFir, String> {
        let fs = self.frequence.unwrap_or(crate::mesure::FREQUENCE_MESURE);
        if !FREQUENCES_FIR.contains(&fs) {
            return Err(format!("fréquence {fs} Hz : une de {FREQUENCES_FIR:?}"));
        }
        let mut cfg = ConfigFir::pour(fs);
        if let Some(v) = self.coupe_max_db {
            cfg.coupe_max_db = v;
        }
        if let Some(v) = self.remontee_max_db {
            cfg.remontee_max_db = v;
        }
        if let Some(v) = self.f_min_hz {
            cfg.f_min_hz = v;
        }
        if let Some(v) = self.f_max_hz {
            cfg.f_max_hz = v;
        }
        cfg.valider()?;
        Ok(cfg)
    }
}

/// Le rapport rendu par `POST …/fir` (et calculé pour `…/fir.wav`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RapportFir {
    pub frequence: u32,
    pub coefficients: usize,
    pub latence_echantillons: usize,
    pub latence_ms: f64,
    pub normalisation_db: f64,
    pub decalage_cible_db: f64,
    pub positions: usize,
    pub points_bornes: usize,
    pub points_creux_non_combles: usize,
    pub coupe_max_db: f64,
    pub remontee_max_db: f64,
    /// Correction prévue (dB, avant normalisation) sur la grille de la mesure.
    pub freqs_hz: Vec<f64>,
    pub correction_db: Vec<f64>,
    /// Réponse corrigée prévue (mesure + module du filtre, même grille).
    /// La mesure du greffon contourne la convolution (#8513) : la remesure
    /// « correction active » se fait hors greffon (procédure terrain).
    pub reponse_prevue_db: Vec<f64>,
    /// Pire écart prévu à la cible, 30 Hz–15 kHz, lissage variable :
    /// `[fréquence, dB]`.
    pub ecart_cible_prevu: [f64; 2],
    pub avertissements: Vec<String>,
}

/// Conçoit le FIR d'une réponse portant `positions` places.
pub fn concevoir(
    reponse: &LogResponse,
    positions: usize,
    demande: &DemandeFir,
) -> Result<(FirMesure, RapportFir), String> {
    let cfg = demande.config()?;
    let cible = demande.cible.courbe()?;
    let f = fir::concevoir_fir(reponse, &cible, &cfg)?;
    let prevue = fir::reponse_prevue(reponse, &f);
    let (f_pire, e_pire) = fir::pire_ecart_a_la_cible(&prevue, &cible, &cfg, 30.0, 15_000.0);
    let mut avertissements = Vec::new();
    if positions < POSITIONS_MIN {
        avertissements.push(format!(
            "mesure en un point : au-dessus de la zone modale, la correction suit la place du micro ; \
             une session de {POSITIONS_MIN} positions ou plus est recommandée"
        ));
    }
    avertissements.extend(f.avertissements.iter().cloned());
    let rapport = RapportFir {
        frequence: f.sample_rate,
        coefficients: f.taps.len(),
        latence_echantillons: f.latence_echantillons,
        latence_ms: f.latence_ms(),
        normalisation_db: f.normalisation_db,
        decalage_cible_db: f.decalage_cible_db,
        positions,
        points_bornes: f.points_bornes,
        points_creux_non_combles: f.points_creux_non_combles,
        coupe_max_db: cfg.coupe_max_db,
        remontee_max_db: cfg.remontee_max_db,
        freqs_hz: f.correction.freqs.clone(),
        correction_db: f.correction.magnitude_db.clone(),
        reponse_prevue_db: prevue.magnitude_db,
        ecart_cible_prevu: [f_pire, e_pire],
        avertissements,
    };
    Ok((f, rapport))
}

/// La RI `.wav` mono du filtre (la convolution l'applique à chaque canal).
pub fn wav(f: &FirMesure, format: FormatRi) -> Result<Vec<u8>, String> {
    fir::wav_ri(&[&f.taps], f.sample_rate, format)
}
