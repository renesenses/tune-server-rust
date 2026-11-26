//! Fichiers d'étalonnage de microphone au format texte miniDSP UMIK-1 /
//! UMIK-2 / REW (`.txt` ou `.cal`).
//!
//! Format constaté (miniDSP, « UMIK-1 calibration file ») :
//! ```text
//! "Sens Factor =-1.021dB, SERNO: 7001234"
//! 10.058	-5.2
//! 10.308	-4.9
//! ...
//! ```
//! Chaque ligne de données porte la fréquence (Hz) et l'écart du micro par
//! rapport à une réponse plate (dB), parfois une troisième colonne de phase
//! (degrés). Lignes de commentaire : `*`, `#`, `;`, `"` ou texte sans
//! nombre. Les fichiers REW `.cal` suivent la même convention. La réponse
//! corrigée vaut `mesure − écart`.
//!
//! Le « Sens Factor » est la déviation de sensibilité par rapport au nominal
//! de l'UMIK-1 ; REW l'emploie pour afficher des dB SPL absolus. Convention
//! retenue ici, À CONFIRMER sur un micro réel (question au relecteur) :
//! `SPL = dBFS + 94 + 18 − sens_factor`, le nominal UMIK-1 étant −18 dBFS
//! à 94 dB SPL (1 kHz).

use crate::analysis::interp_log;

#[derive(Debug, Clone, PartialEq)]
pub struct MicCalibration {
    pub freqs: Vec<f64>,
    pub deviation_db: Vec<f64>,
    pub phase_deg: Option<Vec<f64>>,
    pub sens_factor_db: Option<f64>,
    pub serial: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CalError {
    NoData,
    NotMonotonic(usize),
}

impl std::fmt::Display for CalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CalError::NoData => write!(f, "fichier d'étalonnage sans donnée"),
            CalError::NotMonotonic(i) => write!(f, "fréquences non croissantes à la ligne {i}"),
        }
    }
}

impl MicCalibration {
    /// Analyse le contenu texte d'un fichier d'étalonnage.
    pub fn parse(text: &str) -> Result<Self, CalError> {
        let mut freqs = Vec::new();
        let mut dev = Vec::new();
        let mut phase = Vec::new();
        let mut sens = None;
        let mut serial = None;
        for (lineno, raw) in text.lines().enumerate() {
            let line = raw.trim().trim_start_matches('\u{feff}');
            if line.is_empty() {
                continue;
            }
            let lower = line.to_ascii_lowercase();
            if lower.contains("sens factor") {
                sens = extract_number_after(&lower, "sens factor");
                if let Some(pos) = lower.find("serno") {
                    let rest: String = line[pos..]
                        .chars()
                        .skip_while(|c| !c.is_ascii_digit())
                        .take_while(|c| c.is_ascii_alphanumeric())
                        .collect();
                    if !rest.is_empty() {
                        serial = Some(rest);
                    }
                }
                continue;
            }
            if line.starts_with(['*', '#', ';', '"', '\'']) {
                continue;
            }
            let nums: Vec<f64> = line
                .split([' ', '\t', ',', ';'])
                .filter(|s| !s.is_empty())
                .map(|s| s.parse::<f64>())
                .take_while(|r| r.is_ok())
                .map(|r| r.unwrap())
                .collect();
            if nums.len() < 2 {
                continue; // ligne de texte libre
            }
            if let Some(last) = freqs.last() {
                if nums[0] <= *last {
                    return Err(CalError::NotMonotonic(lineno + 1));
                }
            }
            freqs.push(nums[0]);
            dev.push(nums[1]);
            if nums.len() >= 3 {
                phase.push(nums[2]);
            }
        }
        if freqs.is_empty() {
            return Err(CalError::NoData);
        }
        let phase = if phase.len() == freqs.len() {
            Some(phase)
        } else {
            None
        };
        Ok(Self {
            freqs,
            deviation_db: dev,
            phase_deg: phase,
            sens_factor_db: sens,
            serial,
        })
    }

    /// Écart du micro (dB) à la fréquence `f`, interpolé en log(f), borné hors plage.
    pub fn deviation_at(&self, f: f64) -> f64 {
        interp_log(&self.freqs, &self.deviation_db, f)
    }

    /// Applique la correction à une réponse mesurée (dB) donnée sur `freqs`.
    pub fn correct(&self, freqs: &[f64], measured_db: &[f64]) -> Vec<f64> {
        freqs
            .iter()
            .zip(measured_db)
            .map(|(f, m)| m - self.deviation_at(*f))
            .collect()
    }

    /// Niveau SPL (dB) estimé pour un niveau crête ou efficace `dbfs`, si le
    /// fichier porte un facteur de sensibilité. Voir la convention en tête de module.
    pub fn spl_from_dbfs(&self, dbfs: f64) -> Option<f64> {
        self.sens_factor_db.map(|s| dbfs + 94.0 + 18.0 - s)
    }

    /// Sens Factor est le niveau RMS numérique à 94 dB SPL, au gain
    /// d'étalonnage du micro. Une atténuation de capture est négative.
    /// Le carré moyen est référencé au pic numérique pleine échelle.
    pub fn reference_spl(&self, gain_relatif_db: Option<f64>) -> Option<f64> {
        let gain = gain_relatif_db.filter(|g| g.is_finite() && *g <= 0.0)?;
        let sens = self.sens_factor_db.filter(|s| s.is_finite())?;
        Some(94.0 - sens - gain)
    }

    /// La veille existante reste disponible aux clients sans gain de référence.
    pub fn crete_spl(&self, dbfs: f64, gain_relatif_db: Option<f64>) -> Option<f64> {
        self.reference_spl(gain_relatif_db)
            .map(|reference| dbfs + reference)
            .or_else(|| self.spl_from_dbfs(dbfs))
    }

    /// Niveau continu équivalent pondéré C sur le palier capté, à sa cadence native.
    pub fn niveau_c(&self, palier: &[f64], fs: u32, gain_relatif_db: Option<f64>) -> Option<f64> {
        let reference = self.reference_spl(gain_relatif_db)?;
        if palier.len() < 2
            || fs == 0
            || self.freqs.is_empty()
            || self.freqs.iter().any(|f| !f.is_finite() || *f <= 0.0)
            || self.deviation_db.iter().any(|v| !v.is_finite())
        {
            return None;
        }
        let nfft = crate::fft::next_pow2(palier.len());
        let moyenne = palier.iter().sum::<f64>() / palier.len() as f64;
        let centre: Vec<_> = palier.iter().map(|s| s - moyenne).collect();
        let spectre = crate::fft::rfft(&centre, nfft);
        let dev_ref = self.deviation_at(1000.0);
        let c_ref = reponse_c(1000.0);
        // Parseval, spectre réel unilatéral. Nyquist n'a pas de bin miroir.
        let energie: f64 = spectre
            .iter()
            .enumerate()
            .skip(1)
            .map(|(k, bin)| {
                let f = k as f64 * f64::from(fs) / nfft as f64;
                let gain =
                    reponse_c(f) / c_ref * crate::fft::from_db(dev_ref - self.deviation_at(f));
                let multiplicite = if k == nfft / 2 { 1.0 } else { 2.0 };
                multiplicite * bin.norm_sqr() * gain * gain
            })
            .sum();
        let puissance = energie / (palier.len() as f64 * nfft as f64);
        let niveau = reference + 10.0 * puissance.log10();
        niveau.is_finite().then_some(niveau)
    }

    /// Plage de fréquences couverte par le fichier.
    pub fn range(&self) -> (f64, f64) {
        (self.freqs[0], *self.freqs.last().unwrap())
    }
}

/// Réponse en amplitude C de la CEI 61672, normalisée à 1 kHz par l'appelant.
fn reponse_c(f: f64) -> f64 {
    let f2 = f * f;
    let haut = 12_194.217_f64.powi(2);
    haut * f2 / ((f2 + 20.598_997_f64.powi(2)) * (f2 + haut))
}

fn extract_number_after(lower: &str, key: &str) -> Option<f64> {
    let pos = lower.find(key)? + key.len();
    let rest = &lower[pos..];
    let start = rest.find(|c: char| c.is_ascii_digit() || c == '-' || c == '+')?;
    let num: String = rest[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
        .collect();
    num.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pression_c_du_signal_a_63_1000_et_8000_hz() {
        // À 94 dB SPL, -12 dBFS RMS dans le fichier et -6 dB de gain
        // relatif donnent -18 dBFS dans la capture. À 75 dB : -37 dBFS.
        let cal = MicCalibration::parse("Sens Factor =-12dB\n63 -3\n1000 0\n8000 2").unwrap();
        for fs in [44_100, 48_000] {
            for (hz, deviation, poids_c) in
                [(63.0, -3.0, -0.8), (1000.0, 0.0, 0.0), (8000.0, 2.0, -3.0)]
            {
                let amplitude = 2.0_f64.sqrt() * crate::fft::from_db(-37.0 + deviation);
                let signal: Vec<_> = (0..fs)
                    .map(|i| {
                        0.04 + amplitude
                            * (std::f64::consts::TAU * hz * f64::from(i) / f64::from(fs)).sin()
                    })
                    .collect();
                let dbc = cal.niveau_c(&signal, fs, Some(-6.0)).unwrap();
                assert!(
                    (dbc - (75.0 + poids_c)).abs() < 0.1,
                    "{fs} Hz, {hz} Hz: {dbc}"
                );
            }
        }
    }

    #[test]
    fn sans_sensibilite_gain_ou_signal_aucune_pression_absolue() {
        let courbe = MicCalibration::parse("63 0\n1000 0").unwrap();
        let cal = MicCalibration::parse(UMIK).unwrap();
        assert_eq!(courbe.niveau_c(&[0.1, -0.1], 48000, Some(0.0)), None);
        assert_eq!(cal.niveau_c(&[0.1, -0.1], 48000, None), None);
        assert_eq!(cal.niveau_c(&[0.0; 8], 48000, Some(0.0)), None);
        assert_eq!(cal.reference_spl(Some(1.0)), None);
    }

    const UMIK: &str = "\"Sens Factor =-1.021dB, SERNO: 7001234\"\n10.058\t-5.2\n20.0\t-1.0\n1000\t0.0\n10000\t1.5\n20000\t3.0\n";

    #[test]
    fn lit_un_fichier_umik() {
        let c = MicCalibration::parse(UMIK).unwrap();
        assert_eq!(c.freqs.len(), 5);
        assert_eq!(c.sens_factor_db, Some(-1.021));
        assert_eq!(c.serial.as_deref(), Some("7001234"));
        assert!((c.deviation_at(1000.0)).abs() < 1e-12);
        // Interpolation en log(f) : t = ln(100/20)/ln(1000/20) = 0,4114 → −1 + 0,4114.
        assert!(
            (c.deviation_at(100.0) + 0.5886).abs() < 1e-3,
            "interp log 20→1000 à 100 Hz : {}",
            c.deviation_at(100.0)
        );
        assert_eq!(c.deviation_at(5.0), -5.2);
    }

    #[test]
    fn accepte_le_format_rew_avec_phase_et_commentaires() {
        let txt = "* REW calibration\n# freq dB phase\n20, -2.0, 10\n200, 0.0, 0\n2000, 0.5, -5\n";
        let c = MicCalibration::parse(txt).unwrap();
        assert!(c.phase_deg.is_some());
        assert_eq!(c.sens_factor_db, None);
        let corr = c.correct(&[20.0, 2000.0], &[70.0, 70.0]);
        assert!((corr[0] - 72.0).abs() < 1e-9 && (corr[1] - 69.5).abs() < 1e-9);
    }

    #[test]
    fn refuse_le_vide_et_le_non_monotone() {
        assert_eq!(MicCalibration::parse("# rien\n"), Err(CalError::NoData));
        assert_eq!(
            MicCalibration::parse("100 0\n50 1\n"),
            Err(CalError::NotMonotonic(2))
        );
    }
}
