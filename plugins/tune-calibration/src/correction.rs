//! C2 (#7632) : de la réponse mesurée à une proposition d'égaliseur.
//!
//! ```text
//! Resultat.reponse (1/12 oct.)  ─┐
//! Cible : plate + pente, points,  ├─ eq::fit (caisse tune-calibration, prototype C2)
//!         ou courbe importée     ─┘        coupes SEULEMENT, plafond cumulé
//!                                            │
//!                    Proposition : bandes au format natif, corps de
//!                    `POST /zones/{id}/eq`, courbe corrigée prévue,
//!                    export « Filter Settings » (REW / Equalizer APO),
//!                    RoomProfile de la zone (`room_profile_{zone}`)
//! ```
//!
//! Ce module ne réécrit pas l'égaliseur automatique : il le BORNE. Tout ce
//! que l'utilisateur peut régler passe par [`DemandeCorrection::config`], qui
//! refuse ce qui sort des bornes au lieu de l'écrêter en silence :
//!
//! * **coupes seulement** : la relance est fixée à 0 dB et n'est pas un
//!   paramètre — un creux de pièce est une annulation, le remonter coûte de la
//!   réserve et de l'excursion pour rien ;
//! * **zone de correction** dans [`F_MIN_HZ`, `F_MAX_HZ`] ; au-delà de
//!   [`F_SCHROEDER_HZ`] une mesure en un point est discutable (moyenne
//!   spatiale en C4) : c'est permis, mais dit ;
//! * **nombre de bandes** entre 1 et [`BANDES_MAX`] ;
//! * **coupe cumulée** au plus [`COUPE_MAX_DB`] : c'est la réponse de TOUTES
//!   les bandes empilées qui est bornée, pas chaque bande seule (`eq::fit`
//!   rejette tout candidat qui la dépasse).
//!
//! L'écriture dans l'égaliseur de la zone ne se fait PAS ici : le greffon
//! enregistre le profil de pièce de la zone, et l'application passe par la
//! route existante `POST /api/v1/room-correction/profiles/{zone}/apply`, qui
//! écrit `zone_{id}_eq_profile` (profil complet, #7642) et rafraîchit la
//! sortie qui joue (#1725). Un seul écrivain de cette clé, avec ses gardes.

use serde::{Deserialize, Serialize};
use tune_calibration::analysis::LogResponse;
use tune_calibration::eq::{
    AutoEqConfig, Band, BandType, EqSettings, bands_response_db, fit, to_rew_filters,
};
use tune_calibration::target::TargetCurve;
use tune_core::room_correction::{CorrectionFilter, FilterType, FrequencyPoint, RoomProfile};

/// Bornes dures de la zone de correction.
pub const F_MIN_HZ: f64 = 20.0;
pub const F_MAX_HZ: f64 = 500.0;
/// Au-delà, une mesure en un seul point ne décrit plus la pièce.
pub const F_SCHROEDER_HZ: f64 = 300.0;
pub const BANDES_MAX: usize = 10;
/// Coupe cumulée maximale (dB, positif) de l'égaliseur proposé.
pub const COUPE_MAX_DB: f64 = 12.0;
/// Pente de cible admise (dB/octave, en valeur absolue).
pub const PENTE_MAX_DB_OCT: f64 = 3.0;
/// Écart cible − cible maximal d'une courbe importée (dB).
pub const ECART_CIBLE_MAX_DB: f64 = 20.0;
/// Cadence de calcul des coefficients : celle de la mesure.
const CADENCE: f64 = crate::mesure::FREQUENCE_MESURE as f64;

/// La courbe cible demandée.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Cible {
    /// Plate, avec une pente en dB/octave autour d'un pivot (0 = plate).
    Plate {
        #[serde(default)]
        pente_db_oct: f64,
        #[serde(default = "pivot_defaut")]
        pivot_hz: f64,
    },
    /// Points (Hz, dB) croissants, interpolés en log(f).
    Points { points: Vec<(f64, f64)> },
    /// Courbe importée en texte « fréquence dB » : « house curve » de REW,
    /// export de mesure REW, ou tout fichier à deux colonnes (`*`, `#`, `;`
    /// commentent une ligne).
    Texte { texte: String },
    /// Approximation de la cible Harman en pièce (Olive 2013).
    Harman,
}

fn pivot_defaut() -> f64 {
    1000.0
}

impl Default for Cible {
    fn default() -> Self {
        Self::Plate {
            pente_db_oct: 0.0,
            pivot_hz: pivot_defaut(),
        }
    }
}

impl Cible {
    /// La courbe de la caisse, ou le refus motivé.
    pub fn courbe(&self) -> Result<TargetCurve, String> {
        match self {
            Cible::Plate {
                pente_db_oct,
                pivot_hz,
            } => {
                if !pente_db_oct.is_finite() || pente_db_oct.abs() > PENTE_MAX_DB_OCT {
                    return Err(format!(
                        "pente de cible {pente_db_oct} dB/octave : au plus ±{PENTE_MAX_DB_OCT}"
                    ));
                }
                if !pivot_hz.is_finite() || !(20.0..=20_000.0).contains(pivot_hz) {
                    return Err(format!("pivot {pivot_hz} Hz : entre 20 Hz et 20 kHz"));
                }
                Ok(TargetCurve::Tilt {
                    db_per_octave: *pente_db_oct,
                    pivot_hz: *pivot_hz,
                })
            }
            Cible::Points { points } => points_valides(points.clone()),
            Cible::Texte { texte } => {
                if texte.len() > 256 * 1024 {
                    return Err("cible importée : 256 Ko au plus".into());
                }
                let r = tune_calibration::rew::import_measurement_txt(texte)
                    .ok_or("cible importée illisible : lignes « fréquence dB » attendues")?;
                points_valides(r.freqs.into_iter().zip(r.magnitude_db).collect())
            }
            Cible::Harman => Ok(TargetCurve::harman_in_room()),
        }
    }
}

fn points_valides(points: Vec<(f64, f64)>) -> Result<TargetCurve, String> {
    if points.len() < 2 {
        return Err("cible : deux points au moins".into());
    }
    if points.len() > 2048 {
        return Err("cible : 2048 points au plus".into());
    }
    if points
        .iter()
        .any(|(f, d)| !f.is_finite() || !d.is_finite() || *f <= 0.0)
    {
        return Err("cible : fréquences positives et niveaux finis attendus".into());
    }
    if points.windows(2).any(|w| w[1].0 <= w[0].0) {
        return Err("cible : fréquences strictement croissantes attendues".into());
    }
    let (lo, hi) = points
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), (_, d)| {
            (lo.min(*d), hi.max(*d))
        });
    if hi - lo > ECART_CIBLE_MAX_DB {
        return Err(format!(
            "cible : {:.1} dB entre son point le plus bas et le plus haut, {ECART_CIBLE_MAX_DB} au plus",
            hi - lo
        ));
    }
    Ok(TargetCurve::Points(points))
}

/// Ce que l'utilisateur règle. Tout est facultatif ; les défauts sont ceux
/// d'une mesure en un point (20–300 Hz, 8 bandes, 12 dB).
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct DemandeCorrection {
    #[serde(default)]
    pub cible: Cible,
    pub f_min_hz: Option<f64>,
    pub f_max_hz: Option<f64>,
    pub bandes_max: Option<usize>,
    pub coupe_max_db: Option<f64>,
}

impl DemandeCorrection {
    /// La configuration de l'ajustement, bornée, ou le refus motivé.
    pub fn config(&self) -> Result<(AutoEqConfig, Vec<String>), String> {
        let mut cfg = AutoEqConfig::single_position(CADENCE);
        let mut avertissements = Vec::new();
        let f_lo = self.f_min_hz.unwrap_or(cfg.f_lo);
        let f_hi = self.f_max_hz.unwrap_or(cfg.f_hi);
        if !f_lo.is_finite() || !f_hi.is_finite() || f_lo < F_MIN_HZ || f_hi > F_MAX_HZ {
            return Err(format!(
                "zone de correction {f_lo}–{f_hi} Hz : elle doit tenir dans {F_MIN_HZ}–{F_MAX_HZ} Hz"
            ));
        }
        if f_hi / f_lo < 2.0 {
            return Err(format!(
                "zone de correction {f_lo}–{f_hi} Hz : une octave au moins"
            ));
        }
        if f_hi > F_SCHROEDER_HZ {
            avertissements.push(format!(
                "correction jusqu'à {f_hi} Hz sur une mesure en un point : au-delà de {F_SCHROEDER_HZ} Hz, elle corrige surtout la position du micro"
            ));
        }
        let bandes = self.bandes_max.unwrap_or(cfg.max_bands);
        if !(1..=BANDES_MAX).contains(&bandes) {
            return Err(format!("{bandes} bandes : entre 1 et {BANDES_MAX}"));
        }
        let coupe = self.coupe_max_db.unwrap_or(cfg.max_cut_db);
        if !coupe.is_finite() || coupe <= 0.0 || coupe > COUPE_MAX_DB {
            return Err(format!(
                "coupe maximale {coupe} dB : entre 0 et {COUPE_MAX_DB} dB"
            ));
        }
        cfg.f_lo = f_lo;
        cfg.f_hi = f_hi;
        cfg.align_lo = f_lo.max(40.0).min(f_hi / 2.0);
        cfg.align_hi = f_hi;
        cfg.max_bands = bandes;
        cfg.max_cut_db = coupe;
        // Coupes seulement : ce n'est pas un réglage.
        cfg.max_boost_db = 0.0;
        Ok((cfg, avertissements))
    }
}

/// Une courbe sur la grille de la mesure.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Courbes {
    pub freqs_hz: Vec<f64>,
    /// La mesure telle que l'ajustement la voit (lissée 1/6 d'octave).
    pub mesure_db: Vec<f64>,
    /// La cible, alignée sur la mesure.
    pub cible_db: Vec<f64>,
    /// La mesure plus la réponse de l'égaliseur proposé : ce que la mesure
    /// de vérification devrait montrer.
    pub corrigee_db: Vec<f64>,
}

/// Ce que `POST /mesures/{id}/correction` rend.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Proposition {
    pub bandes: Vec<Band>,
    /// Corps prêt pour `POST /zones/{id}/eq`.
    pub egaliseur: EqSettings,
    pub zone_hz: (f64, f64),
    pub bandes_max: usize,
    pub coupe_max_db: f64,
    /// Coupe la plus profonde de l'empilement (dB, ≤ 0), sur 10 Hz–21 kHz.
    pub coupe_cumulee_db: f64,
    /// Gain le plus haut de l'empilement (dB) : 0 quand rien n'est relevé.
    pub gain_max_db: f64,
    pub decalage_cible_db: f64,
    pub depassement_avant_db: f64,
    pub depassement_apres_db: f64,
    pub pic_avant_db: f64,
    pub pic_apres_db: f64,
    pub courbes: Courbes,
    /// Texte « Filter Settings file » : lu par REW et par Equalizer APO.
    pub export_filtres: String,
    pub avertissements: Vec<String>,
}

impl Proposition {
    /// Le profil de pièce de la zone, tel que
    /// `POST /room-correction/profiles/{zone}/apply` l'applique.
    pub fn profil_de_zone(&self, zone_id: i64, nom: &str, maintenant: &str) -> RoomProfile {
        let mesure: Vec<FrequencyPoint> = self
            .courbes
            .freqs_hz
            .iter()
            .zip(&self.courbes.mesure_db)
            .map(|(f, d)| FrequencyPoint {
                frequency_hz: *f,
                amplitude_db: *d,
            })
            .collect();
        RoomProfile {
            name: nom.to_string(),
            zone_id: zone_id.to_string(),
            filters: self
                .bandes
                .iter()
                .map(|b| CorrectionFilter {
                    frequency_hz: b.freq,
                    gain_db: b.gain,
                    q_factor: b.q,
                    filter_type: match b.kind {
                        BandType::Peak => FilterType::Peaking,
                        BandType::LowShelf => FilterType::LowShelf,
                        BandType::HighShelf => FilterType::HighShelf,
                    },
                })
                .collect(),
            created_at: maintenant.to_string(),
            measurement_data: serde_json::to_string(&mesure).ok(),
        }
    }
}

/// Calcule la proposition pour une réponse mesurée (grille log, dB).
pub fn proposer(reponse: &LogResponse, demande: &DemandeCorrection) -> Result<Proposition, String> {
    let cible = demande.cible.courbe()?;
    let (cfg, mut avertissements) = demande.config()?;
    if reponse.freqs.len() < 8
        || reponse.freqs.len() != reponse.magnitude_db.len()
        || reponse.magnitude_db.iter().any(|d| !d.is_finite())
    {
        return Err("réponse mesurée vide ou invalide".into());
    }
    let (r_lo, r_hi) = (reponse.freqs[0], *reponse.freqs.last().unwrap());
    if r_lo > cfg.f_lo * 1.01 || r_hi < cfg.f_hi * 0.99 {
        return Err(format!(
            "la mesure couvre {r_lo:.0}–{r_hi:.0} Hz, la zone de correction {}–{} Hz en sort",
            cfg.f_lo, cfg.f_hi
        ));
    }
    let r = fit(reponse, &cible, &cfg);
    avertissements.extend(r.warnings.iter().cloned());

    let lisse = reponse.smoothed(cfg.pre_smoothing_n);
    let corr = bands_response_db(cfg.sample_rate, &r.bands, &lisse.freqs);
    let courbes = Courbes {
        cible_db: lisse
            .freqs
            .iter()
            .map(|f| cible.level_db(*f) + r.target_offset_db)
            .collect(),
        corrigee_db: lisse
            .magnitude_db
            .iter()
            .zip(&corr)
            .map(|(m, c)| m + c)
            .collect(),
        freqs_hz: lisse.freqs,
        mesure_db: lisse.magnitude_db,
    };
    let large = LogResponse::grid(10.0, cfg.sample_rate * 0.44, 48);
    let total = bands_response_db(cfg.sample_rate, &r.bands, &large);
    let coupe_cumulee = total.iter().cloned().fold(0.0, f64::min);
    let gain_max = total.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    if r.bands.is_empty() {
        avertissements.push("aucun pic à couper au-dessus de la cible dans la zone".into());
    }
    Ok(Proposition {
        egaliseur: r.to_eq_settings(),
        export_filtres: to_rew_filters(&r.bands, r.preamp_db),
        bandes: r.bands,
        zone_hz: (cfg.f_lo, cfg.f_hi),
        bandes_max: cfg.max_bands,
        coupe_max_db: cfg.max_cut_db,
        coupe_cumulee_db: coupe_cumulee,
        gain_max_db: gain_max,
        decalage_cible_db: r.target_offset_db,
        depassement_avant_db: r.overshoot_rms_before_db,
        depassement_apres_db: r.overshoot_rms_after_db,
        pic_avant_db: r.max_peak_before_db,
        pic_apres_db: r.max_peak_after_db,
        courbes,
        avertissements,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Une pièce sur grille : 75 dB, trois modes et un creux.
    fn piece(modes: &[(f64, f64, f64)]) -> LogResponse {
        let freqs = LogResponse::grid(15.0, 2000.0, 48);
        let bandes: Vec<Band> = modes
            .iter()
            .map(|(f, g, q)| Band {
                freq: *f,
                gain: *g,
                q: *q,
                kind: BandType::Peak,
                channel: None,
            })
            .collect();
        let db = bands_response_db(CADENCE, &bandes, &freqs);
        LogResponse {
            magnitude_db: db.iter().map(|d| d + 75.0).collect(),
            freqs,
        }
    }

    fn salon() -> LogResponse {
        // Modes à 42, 68 et 115 Hz, creux d'annulation à 90 Hz.
        piece(&[
            (42.0, 9.0, 6.0),
            (68.0, 6.0, 5.0),
            (90.0, -10.0, 4.0),
            (115.0, 5.0, 7.0),
        ])
    }

    #[test]
    fn les_modes_sont_coupes_et_le_creux_reste_un_creux() {
        let m = salon();
        let p = proposer(&m, &DemandeCorrection::default()).expect("proposition");
        assert!(!p.bandes.is_empty());
        assert!(
            p.bandes.iter().all(|b| b.gain <= 0.0),
            "relance proposée : {:?}",
            p.bandes
        );
        assert!(p.gain_max_db <= 1e-9, "gain max {}", p.gain_max_db);
        for f in [42.0, 68.0, 115.0] {
            assert!(
                p.bandes.iter().any(|b| (b.freq / f).log2().abs() < 0.15),
                "mode {f} Hz non coupé : {:?}",
                p.bandes
            );
        }
        // Le creux de 90 Hz : la courbe corrigée n'y est pas plus haute que
        // la mesure (rien n'est relevé).
        let i = p.courbes.freqs_hz.iter().position(|f| *f >= 90.0).unwrap();
        assert!(p.courbes.corrigee_db[i] <= p.courbes.mesure_db[i] + 1e-9);
        assert!(p.pic_apres_db < p.pic_avant_db - 3.0, "{p:?}");
        assert!(p.depassement_apres_db < p.depassement_avant_db / 2.0);
        assert_eq!(p.egaliseur.headroom_mode, "safe");
        assert!(p.export_filtres.contains("Filter  1: ON  PK"));
    }

    #[test]
    fn la_coupe_cumulee_et_le_nombre_de_bandes_sont_bornes() {
        // Un mode énorme et large, et beaucoup de petits : ni l'empilement ni
        // le nombre de bandes ne dépassent ce qui est demandé.
        let mut modes = vec![(50.0, 22.0, 2.0)];
        modes.extend((0..9).map(|k| (80.0 + 22.0 * k as f64, 4.0, 8.0)));
        let m = piece(&modes);
        let d = DemandeCorrection {
            bandes_max: Some(4),
            coupe_max_db: Some(8.0),
            ..Default::default()
        };
        let p = proposer(&m, &d).unwrap();
        assert!(p.bandes.len() <= 4, "{} bandes", p.bandes.len());
        assert!(
            p.coupe_cumulee_db >= -8.0 - 1e-6,
            "coupe cumulée {} dB",
            p.coupe_cumulee_db
        );
        assert!(p.avertissements.iter().any(|a| a.contains("pic résiduel")));
    }

    #[test]
    fn les_bornes_sont_refusees_pas_ecretees() {
        let m = salon();
        let refus = |d: DemandeCorrection| proposer(&m, &d).unwrap_err();
        assert!(
            refus(DemandeCorrection {
                f_max_hz: Some(2000.0),
                ..Default::default()
            })
            .contains("20–500")
        );
        assert!(
            refus(DemandeCorrection {
                f_min_hz: Some(10.0),
                ..Default::default()
            })
            .contains("20–500")
        );
        assert!(
            refus(DemandeCorrection {
                bandes_max: Some(11),
                ..Default::default()
            })
            .contains("entre 1 et 10")
        );
        assert!(
            refus(DemandeCorrection {
                bandes_max: Some(0),
                ..Default::default()
            })
            .contains("entre 1 et 10")
        );
        assert!(
            refus(DemandeCorrection {
                coupe_max_db: Some(18.0),
                ..Default::default()
            })
            .contains("entre 0 et 12")
        );
        assert!(
            refus(DemandeCorrection {
                f_min_hz: Some(100.0),
                f_max_hz: Some(150.0),
                ..Default::default()
            })
            .contains("une octave")
        );
        assert!(
            refus(DemandeCorrection {
                cible: Cible::Plate {
                    pente_db_oct: -6.0,
                    pivot_hz: 1000.0
                },
                ..Default::default()
            })
            .contains("pente")
        );
        // Au-delà de 300 Hz : permis, mais dit.
        let p = proposer(
            &m,
            &DemandeCorrection {
                f_max_hz: Some(500.0),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            p.avertissements
                .iter()
                .any(|a| a.contains("position du micro"))
        );
    }

    #[test]
    fn une_cible_importee_en_texte_est_suivie() {
        // Une « house curve » REW : +6 dB sous 60 Hz, plate au-dessus. Sur
        // une pièce plate, le seul « pic » est le haut de la zone, au-dessus
        // de la cible relevée en bas — l'égaliseur ne remonte pas le bas, il
        // ne coupe rien sous 60 Hz.
        let plate = piece(&[]);
        let texte = "* house curve\n20 6\n60 6\n100 0\n20000 0\n";
        let d = DemandeCorrection {
            cible: Cible::Texte {
                texte: texte.into(),
            },
            ..Default::default()
        };
        let p = proposer(&plate, &d).unwrap();
        assert!(p.bandes.iter().all(|b| b.gain <= 0.0));
        assert!(p.bandes.iter().all(|b| b.freq > 60.0), "{:?}", p.bandes);
        let i = p.courbes.freqs_hz.iter().position(|f| *f >= 30.0).unwrap();
        let j = p.courbes.freqs_hz.iter().position(|f| *f >= 200.0).unwrap();
        assert!(
            (p.courbes.cible_db[i] - p.courbes.cible_db[j] - 6.0).abs() < 0.1,
            "la cible importée n'est pas celle qui est suivie"
        );
        let illisible = DemandeCorrection {
            cible: Cible::Texte {
                texte: "pas une courbe".into(),
            },
            ..Default::default()
        };
        assert!(
            proposer(&plate, &illisible)
                .unwrap_err()
                .contains("illisible")
        );
    }

    #[test]
    fn le_profil_de_zone_est_celui_que_l_egaliseur_natif_relit() {
        let p = proposer(&salon(), &DemandeCorrection::default()).unwrap();
        let profil = p.profil_de_zone(7, "Salon", "2026-11-04T10:00:00Z");
        assert_eq!(profil.zone_id, "7");
        assert_eq!(profil.filters.len(), p.bandes.len());
        let eq = tune_core::room_correction::profil_eq_de_correction(&profil.filters);
        assert!(eq.enabled);
        for (spec, b) in eq.bands.iter().zip(&p.bandes) {
            assert_eq!(spec.band_type, "peak");
            assert_eq!(spec.gain, b.gain);
            assert_eq!(spec.freq, b.freq);
        }
        // Et ce profil d'égaliseur se relit tel quel (le défaut de #7642).
        let texte = serde_json::to_string(&eq).unwrap();
        let relu: tune_core::audio::eq::EqProfile = serde_json::from_str(&texte).unwrap();
        assert_eq!(relu.bands.len(), p.bandes.len());
        assert!(profil.measurement_data.unwrap().contains("frequency_hz"));
    }
}
