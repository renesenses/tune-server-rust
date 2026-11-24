//! C3 (#7632, TESS T10b) : le FIR de correction calculé depuis la mesure.
//!
//! ```text
//! Resultat.reponse (1/12 oct.) ─┐
//! RI f64 de la mesure ──────────┼─ fir::concevoir_fir (caisse tune-calibration)
//! Cible (mêmes formes que C2) ──┘      module borné + phase minimale
//!                                       (+ phase excédentaire facultative)
//!                                            │
//!        PropositionFir : prises, cadence, latence exacte, courbes prévues
//!        + la RI `.wav` (flottant 64 ou 32 bits), à déposer par
//!        `POST /api/v1/room-correction/ir/upload/{zone}?per_rate=1`
//! ```
//!
//! Rien n'est installé ici : le dépôt dans la zone est un second geste,
//! par la route existante du serveur, qui valide le fichier, le range sous
//! sa cadence et rafraîchit la sortie qui joue. Le greffon n'a ni l'état du
//! serveur ni ses gardes : y écrire directement doublerait l'écrivain.
//!
//! Ce que l'utilisateur règle passe par [`DemandeFir::options`], qui REFUSE
//! ce qui sort des bornes au lieu de l'écrêter :
//!
//! * bande corrigée dans [`F_MIN_HZ`, `F_MAX_HZ`] (20 Hz–15 kHz par défaut) ;
//! * relance (remontée des creux) au plus [`fir::RELANCE_MAX_DB`], +6 dB par
//!   défaut, et au plus [`RELANCE_SANS_NORMALISATION_DB`] si la
//!   normalisation (crête du filtre ramenée à 0 dB) est désactivée ; coupes au plus [`fir::COUPE_MAX_DB`], 15 dB par défaut ;
//! * longueur entre [`fir::PRISES_MIN`] et [`fir::PRISES_MAX`] prises ;
//! * phase excédentaire : DÉSACTIVÉE par défaut (latence et pré-tintement
//!   ajoutés, à éprouver dans deux pièces réelles avant d'être proposée
//!   d'office, Q10) ; une moyenne de positions n'a pas de phase : elle est
//!   refusée sur une session.

use serde::{Deserialize, Serialize};
use tune_calibration::analysis::LogResponse;
use tune_calibration::deconv::ImpulseResponse;
use tune_calibration::fir::{self, FirCorrection, OptionsFir, OptionsPhase};
use tune_calibration::wav::{FormatEchantillon, ecrire_wav};

use crate::correction::Cible;

/// Bornes dures de la bande corrigée.
pub const F_MIN_HZ: f64 = 15.0;
pub const F_MAX_HZ: f64 = 20_000.0;
/// Au-delà, une mesure en un seul point décrit surtout la place du micro.
pub const F_SCHROEDER_HZ: f64 = 500.0;
/// Relance permise quand la normalisation est désactivée (dB) : le gain du
/// filtre s'ajoute alors tel quel au niveau joué.
pub const RELANCE_SANS_NORMALISATION_DB: f64 = 6.0;
/// Cadence des mesures, donc du filtre.
const CADENCE: u32 = crate::mesure::FREQUENCE_MESURE;

/// Ce que l'utilisateur règle. Tout est facultatif.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct DemandeFir {
    #[serde(default)]
    pub cible: Cible,
    pub f_min_hz: Option<f64>,
    pub f_max_hz: Option<f64>,
    pub relance_max_db: Option<f64>,
    pub coupe_max_db: Option<f64>,
    pub prises: Option<usize>,
    /// Ramener la crête du filtre à 0 dB (défaut : oui).
    pub normaliser: Option<bool>,
    /// Corriger la phase excédentaire sous `f_bascule_hz` (défaut : non).
    #[serde(default)]
    pub phase_excedentaire: bool,
    pub f_bascule_hz: Option<f64>,
    pub pre_tintement_ms: Option<f64>,
    /// `"f64"` (défaut) ou `"f32"`.
    pub format: Option<String>,
}

impl DemandeFir {
    /// Les options de conception, bornées, et le format du fichier ; ou le
    /// refus motivé. `positions` : nombre de places de la réponse (1 = une
    /// mesure, 3 et plus = une moyenne spatiale).
    pub fn options(
        &self,
        positions: usize,
    ) -> Result<(OptionsFir, FormatEchantillon, Vec<String>), String> {
        let defaut = OptionsFir::default();
        let mut avertissements = Vec::new();
        let f_lo = self.f_min_hz.unwrap_or(defaut.f_lo);
        let f_hi = self.f_max_hz.unwrap_or(defaut.f_hi);
        if !f_lo.is_finite() || !f_hi.is_finite() || f_lo < F_MIN_HZ || f_hi > F_MAX_HZ {
            return Err(format!(
                "bande corrigée {f_lo}–{f_hi} Hz : elle doit tenir dans {F_MIN_HZ}–{F_MAX_HZ} Hz"
            ));
        }
        if positions < tune_calibration::positions::POSITIONS_MIN && f_hi > F_SCHROEDER_HZ {
            avertissements.push(format!(
                "correction jusqu'à {f_hi} Hz sur une mesure en un point : au-delà de {F_SCHROEDER_HZ} Hz, elle corrige surtout la place du micro (une session à plusieurs positions est préférable)"
            ));
        }
        let format = match self.format.as_deref() {
            None => FormatEchantillon::Flottant64,
            Some(t) => FormatEchantillon::depuis(t)
                .ok_or_else(|| format!("format « {t} » : f32 ou f64"))?,
        };
        if self.phase_excedentaire && positions != 1 {
            return Err(
                "correction de phase : une moyenne de positions n'a pas de phase, elle se demande sur une mesure"
                    .into(),
            );
        }
        let phase = self.phase_excedentaire.then(|| {
            let d = OptionsPhase::default();
            OptionsPhase {
                f_bascule_hz: self.f_bascule_hz.unwrap_or(d.f_bascule_hz),
                pre_tintement_ms: self.pre_tintement_ms.unwrap_or(d.pre_tintement_ms),
            }
        });
        let o = OptionsFir {
            prises: self.prises.unwrap_or(defaut.prises),
            f_lo,
            f_hi,
            relance_max_db: self.relance_max_db.unwrap_or(defaut.relance_max_db),
            coupe_max_db: self.coupe_max_db.unwrap_or(defaut.coupe_max_db),
            normaliser: self.normaliser.unwrap_or(defaut.normaliser),
            phase,
            ..defaut
        };
        o.valider(CADENCE)?;
        // Sécurité des enceintes : sans normalisation, la relance s'ajoute
        // au niveau de lecture ; au-delà de +6 dB, elle est refusée.
        if !o.normaliser && o.relance_max_db > RELANCE_SANS_NORMALISATION_DB {
            return Err(format!(
                "relance de {} dB sans normalisation : {RELANCE_SANS_NORMALISATION_DB} dB au plus (ou garder la normalisation)",
                o.relance_max_db
            ));
        }
        Ok((o, format, avertissements))
    }
}

/// Les courbes sur la grille de la mesure.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CourbesFir {
    pub freqs_hz: Vec<f64>,
    /// La mesure lissée telle que la conception la voit (1/6 d'octave sous
    /// 300 Hz, 1/3 au-dessus).
    pub mesure_db: Vec<f64>,
    /// La cible effective (alignée, normalisation comprise).
    pub cible_db: Vec<f64>,
    /// Le module voulu du filtre.
    pub correction_db: Vec<f64>,
    /// Mesure + module réel du filtre : ce que la vérification devrait montrer.
    pub corrigee_db: Vec<f64>,
}

/// Ce que `POST …/correction/fir` rend ; `…/correction/fir.wav` rend le
/// fichier du même calcul.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PropositionFir {
    pub cadence: u32,
    pub prises: usize,
    pub canaux: usize,
    /// `"f64"` ou `"f32"`.
    pub format: &'static str,
    pub latence_trames: usize,
    pub latence_ms: f64,
    pub phase_excedentaire: bool,
    pub f_bascule_hz: Option<f64>,
    pub pre_tintement_ms: Option<f64>,
    /// Énergie avant la latence, rapportée au total (dB) ; absente en phase
    /// minimale.
    pub energie_pre_tintement_db: Option<f64>,
    pub zone_hz: (f64, f64),
    pub relance_max_db: f64,
    pub coupe_max_db: f64,
    pub decalage_cible_db: f64,
    pub normalisation_db: f64,
    pub gain_max_db: f64,
    pub gain_min_db: f64,
    pub courbes: CourbesFir,
    pub avertissements: Vec<String>,
    #[serde(skip)]
    pub coefficients: Vec<f64>,
    #[serde(skip)]
    pub format_wav: Option<FormatEchantillon>,
}

impl PropositionFir {
    /// La RI `.wav` (mono) du filtre.
    pub fn wav(&self) -> Result<Vec<u8>, String> {
        ecrire_wav(
            self.cadence,
            std::slice::from_ref(&self.coefficients),
            self.format_wav.unwrap_or(FormatEchantillon::Flottant64),
        )
    }

    /// Le nom de fichier proposé au téléchargement.
    pub fn nom_de_fichier(&self, id: &str) -> String {
        let id: String = id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .take(64)
            .collect();
        format!(
            "correction-{id}-{}k-{}.wav",
            self.cadence / 1000,
            self.format
        )
    }
}

/// Le FIR d'une réponse mesurée (une mesure : `ri` présente ; une moyenne de
/// `positions` places : `ri` absente).
pub fn proposer_fir(
    reponse: &LogResponse,
    ri: Option<&ImpulseResponse>,
    demande: &DemandeFir,
    positions: usize,
) -> Result<PropositionFir, String> {
    let cible = demande.cible.courbe()?;
    let (o, format, mut avertissements) = demande.options(positions)?;
    let f: FirCorrection = fir::concevoir_fir(CADENCE, reponse, ri, &cible, &o)?;
    avertissements.extend(f.avertissements.iter().cloned());
    Ok(PropositionFir {
        cadence: f.cadence,
        prises: f.coefficients.len(),
        canaux: 1,
        format: match format {
            FormatEchantillon::Flottant32 => "f32",
            FormatEchantillon::Flottant64 => "f64",
        },
        latence_trames: f.latence_trames,
        latence_ms: f.latence_ms(),
        phase_excedentaire: f.phase_excedentaire,
        f_bascule_hz: o.phase.as_ref().map(|p| p.f_bascule_hz),
        pre_tintement_ms: o.phase.as_ref().map(|p| p.pre_tintement_ms),
        energie_pre_tintement_db: f.energie_pre_db,
        zone_hz: (o.f_lo, o.f_hi),
        relance_max_db: o.relance_max_db,
        coupe_max_db: o.coupe_max_db,
        decalage_cible_db: f.decalage_cible_db,
        normalisation_db: f.normalisation_db,
        gain_max_db: f.gain_max_db,
        gain_min_db: f.gain_min_db,
        courbes: CourbesFir {
            freqs_hz: f.freqs,
            mesure_db: f.mesure_lissee_db,
            cible_db: f.cible_db,
            correction_db: f.correction_db,
            corrigee_db: f.prevue_db,
        },
        avertissements,
        coefficients: f.coefficients,
        format_wav: Some(format),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plate() -> LogResponse {
        let freqs = LogResponse::grid(20.0, 20_000.0, 48);
        LogResponse {
            magnitude_db: freqs
                .iter()
                .map(|f| -20.0 + 4.0 * (-(f / 50.0).ln().powi(2) * 8.0).exp())
                .collect(),
            freqs,
        }
    }

    #[test]
    fn les_bornes_sont_refusees_pas_ecretees() {
        let r = plate();
        let refus = |d: DemandeFir| proposer_fir(&r, None, &d, 1).unwrap_err();
        assert!(
            refus(DemandeFir {
                f_max_hz: Some(21_000.0),
                ..Default::default()
            })
            .contains("15–20000")
        );
        assert!(
            refus(DemandeFir {
                relance_max_db: Some(20.0),
                ..Default::default()
            })
            .contains("relance")
        );
        // Sécurité des enceintes : pas plus de +6 dB sans normalisation.
        assert!(
            refus(DemandeFir {
                normaliser: Some(false),
                relance_max_db: Some(12.0),
                ..Default::default()
            })
            .contains("sans normalisation")
        );
        // Contre-épreuve : +6 dB sans normalisation, et +12 dB normalisés,
        // sont acceptés ; le filtre non normalisé ne dépasse pas +6 dB (+1).
        let p = proposer_fir(
            &r,
            None,
            &DemandeFir {
                normaliser: Some(false),
                relance_max_db: Some(6.0),
                ..Default::default()
            },
            1,
        )
        .unwrap();
        assert!(p.gain_max_db <= 7.0, "{}", p.gain_max_db);
        assert!(
            proposer_fir(
                &r,
                None,
                &DemandeFir {
                    relance_max_db: Some(12.0),
                    ..Default::default()
                },
                1
            )
            .is_ok()
        );
        assert!(
            refus(DemandeFir {
                prises: Some(100),
                ..Default::default()
            })
            .contains("prises")
        );
        assert!(
            refus(DemandeFir {
                format: Some("i16".into()),
                ..Default::default()
            })
            .contains("f32 ou f64")
        );
        // Phase sans RI, puis sur une moyenne de positions.
        assert!(
            refus(DemandeFir {
                phase_excedentaire: true,
                ..Default::default()
            })
            .contains("sans réponse impulsionnelle")
        );
        let e = proposer_fir(
            &r,
            None,
            &DemandeFir {
                phase_excedentaire: true,
                ..Default::default()
            },
            5,
        )
        .unwrap_err();
        assert!(e.contains("moyenne de positions"), "{e}");
    }

    #[test]
    fn la_proposition_dit_sa_latence_et_son_fichier() {
        let p = proposer_fir(&plate(), None, &DemandeFir::default(), 1).unwrap();
        assert_eq!(p.cadence, 48_000);
        assert_eq!(p.prises, fir::PRISES_DEFAUT);
        assert_eq!(p.latence_trames, 0);
        assert_eq!(p.format, "f64");
        assert!(!p.phase_excedentaire);
        assert!(
            p.avertissements
                .iter()
                .any(|a| a.contains("place du micro"))
        );
        let w = p.wav().unwrap();
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(w.len(), 58 + 8 * p.prises);
        assert_eq!(p.nom_de_fichier("m1/../x"), "correction-m1x-48k-f64.wav");
        let v = serde_json::to_value(&p).unwrap();
        assert!(v.get("coefficients").is_none());
        assert_eq!(v["latence_ms"], 0.0);
        // Moyenne de positions : pas d'avertissement « place du micro ».
        let s = proposer_fir(&plate(), None, &DemandeFir::default(), 5).unwrap();
        assert!(
            !s.avertissements
                .iter()
                .any(|a| a.contains("place du micro"))
        );
        // f32 : 4 octets par prise.
        let p32 = proposer_fir(
            &plate(),
            None,
            &DemandeFir {
                format: Some("f32".into()),
                ..Default::default()
            },
            1,
        )
        .unwrap();
        assert_eq!(p32.wav().unwrap().len(), 58 + 4 * p32.prises);
    }
}
