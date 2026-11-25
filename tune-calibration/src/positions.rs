//! C4 (#7632, lot TESS T10a) : mesure à plusieurs positions.
//!
//! ```text
//! N réponses (3 à 9, même grille log)
//!    │ courbe médiane, point à point
//!    │ écart efficace de chaque position à la médiane (bande de rejet)
//!    │ rejet : écart > max(seuil_db, facteur × médiane des écarts)
//!    ▼
//! positions retenues ── poids (centrale prépondérante, ou poids donnés)
//!    │ moyenne pondérée en puissance (défaut, comme REW) ou en dB,
//!    │ calculée comme ÉCART à la position de référence
//!    ▼
//! réponse moyenne ── égaliseur (C2) ── comparaison A/B avant/après
//! ```
//!
//! Pourquoi plusieurs positions : sous la fréquence de Schroeder, la réponse
//! en un point est faite de modes ET d'annulations (réflexions, nœuds) qui
//! dépendent de la place exacte du micro. Un égaliseur calé sur un seul point
//! corrige des creux qui n'existent qu'à cet endroit. Les creux bougent d'une
//! position à l'autre, les modes de la pièce restent : la moyenne garde les
//! seconds et comble les premiers.
//!
//! **Exactitude par construction** : la moyenne est calculée comme la
//! référence plus la moyenne pondérée des ÉCARTS à la référence. Des
//! positions identiques donnent des écarts nuls, donc exactement la courbe de
//! référence, au bit près — pas « à 1e-12 près ».
//!
//! Tout est pur : aucune capture, aucun son.

use serde::{Deserialize, Serialize};

use crate::analysis::LogResponse;

/// Bornes du nombre de positions d'une session multi-positions.
pub const POSITIONS_MIN: usize = 3;
pub const POSITIONS_MAX: usize = 9;
/// Poids de la position centrale (place d'écoute) par défaut, les autres
/// valant 1.
pub const POIDS_CENTRAL_DEFAUT: f64 = 2.0;
/// Poids maximal d'une position (relatif) : au-delà, la « moyenne » n'est
/// plus qu'une mesure en un point déguisée.
pub const POIDS_MAX: f64 = 10.0;

/// Comment combiner les positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Moyenne {
    /// Moyenne des puissances (REW, « RMS average ») : un creux, qui porte
    /// peu d'énergie, pèse peu. Défaut.
    #[default]
    Puissance,
    /// Moyenne des dB (« dB average ») : creux et bosses pèsent pareil.
    Db,
}

/// La pondération des positions.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Ponderation {
    /// Poids de la position centrale ; les autres valent 1.
    #[serde(default = "poids_central_defaut")]
    pub poids_central: f64,
    /// Poids explicites, un par position (remplace `poids_central`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poids: Option<Vec<f64>>,
}

fn poids_central_defaut() -> f64 {
    POIDS_CENTRAL_DEFAUT
}

impl Default for Ponderation {
    fn default() -> Self {
        Self {
            poids_central: POIDS_CENTRAL_DEFAUT,
            poids: None,
        }
    }
}

impl Ponderation {
    /// Les poids des `n` positions, ou le refus motivé.
    pub fn poids(&self, n: usize, centrale: usize) -> Result<Vec<f64>, String> {
        let ok = |w: f64| w.is_finite() && w > 0.0 && w <= POIDS_MAX;
        match &self.poids {
            Some(p) => {
                if p.len() != n {
                    return Err(format!("{} poids pour {n} positions", p.len()));
                }
                if let Some(w) = p.iter().find(|w| !ok(**w)) {
                    return Err(format!(
                        "poids {w} : strictement positif et {POIDS_MAX} au plus"
                    ));
                }
                Ok(p.clone())
            }
            None => {
                if !ok(self.poids_central) || self.poids_central < 1.0 {
                    return Err(format!(
                        "poids central {} : entre 1 et {POIDS_MAX}",
                        self.poids_central
                    ));
                }
                Ok((0..n)
                    .map(|i| {
                        if i == centrale {
                            self.poids_central
                        } else {
                            1.0
                        }
                    })
                    .collect())
            }
        }
    }
}

/// Le rejet des mesures aberrantes.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Rejet {
    /// Écart efficace à la médiane (dB) toléré quoi qu'il arrive.
    #[serde(default = "seuil_defaut")]
    pub seuil_db: f64,
    /// Écart toléré relatif : `facteur ×` l'écart médian des positions.
    #[serde(default = "facteur_defaut")]
    pub facteur: f64,
    /// Décalage de niveau toléré (dB) : médiane, sur la bande, de l'écart à
    /// la courbe médiane. Insensible aux creux (qui sont étroits), il
    /// attrape le micro couvert, tourné ou dont le gain a bougé.
    #[serde(default = "niveau_defaut")]
    pub niveau_db: f64,
    /// Bande où l'écart est mesuré : celle que la correction touche.
    #[serde(default = "f_lo_defaut")]
    pub f_lo_hz: f64,
    #[serde(default = "f_hi_defaut")]
    pub f_hi_hz: f64,
}

fn seuil_defaut() -> f64 {
    6.0
}
fn niveau_defaut() -> f64 {
    6.0
}
fn facteur_defaut() -> f64 {
    3.0
}
fn f_lo_defaut() -> f64 {
    20.0
}
fn f_hi_defaut() -> f64 {
    500.0
}

impl Default for Rejet {
    fn default() -> Self {
        Self {
            seuil_db: seuil_defaut(),
            facteur: facteur_defaut(),
            niveau_db: niveau_defaut(),
            f_lo_hz: f_lo_defaut(),
            f_hi_hz: f_hi_defaut(),
        }
    }
}

/// Réglages d'une moyenne multi-positions. Tout a un défaut.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct ConfigPositions {
    #[serde(default)]
    pub ponderation: Ponderation,
    #[serde(default)]
    pub moyenne: Moyenne,
    #[serde(default)]
    pub rejet: Rejet,
}

/// Ce qu'il advient d'une position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerdictPosition {
    pub index: usize,
    /// Poids demandé ; 0 dans la moyenne si la position est rejetée.
    pub poids: f64,
    /// Écart efficace à la médiane des positions, dans la bande de rejet (dB).
    pub ecart_median_db: f64,
    /// Décalage de niveau médian par rapport aux autres positions (dB).
    #[serde(default)]
    pub decalage_niveau_db: f64,
    pub rejetee: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub motif: Option<String>,
}

/// Le résultat d'une moyenne multi-positions.
#[derive(Debug, Clone, PartialEq)]
pub struct MoyennePositions {
    pub reponse: LogResponse,
    pub verdicts: Vec<VerdictPosition>,
    /// Seuil de rejet effectivement appliqué (dB).
    pub seuil_applique_db: f64,
    pub avertissements: Vec<String>,
}

impl MoyennePositions {
    pub fn retenues(&self) -> usize {
        self.verdicts.iter().filter(|v| !v.rejetee).count()
    }
}

fn dans_bande(freqs: &[f64], f_lo: f64, f_hi: f64) -> Vec<usize> {
    freqs
        .iter()
        .enumerate()
        .filter(|(_, f)| **f >= f_lo && **f <= f_hi)
        .map(|(i, _)| i)
        .collect()
}

fn mediane(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    if n == 0 {
        return f64::NAN;
    }
    if n % 2 == 1 {
        v[n / 2]
    } else {
        0.5 * (v[n / 2 - 1] + v[n / 2])
    }
}

/// Courbe médiane point à point (dB).
pub fn courbe_mediane(reponses: &[LogResponse]) -> LogResponse {
    let n = reponses[0].freqs.len();
    let mut tampon = Vec::with_capacity(reponses.len());
    let magnitude_db = (0..n)
        .map(|i| {
            tampon.clear();
            tampon.extend(reponses.iter().map(|r| r.magnitude_db[i]));
            mediane(&mut tampon)
        })
        .collect();
    LogResponse {
        freqs: reponses[0].freqs.clone(),
        magnitude_db,
    }
}

/// Écart efficace (dB) entre deux réponses de même grille, dans une bande.
pub fn ecart_rms_db(a: &LogResponse, b: &LogResponse, f_lo: f64, f_hi: f64) -> f64 {
    let idx = dans_bande(&a.freqs, f_lo, f_hi);
    if idx.is_empty() {
        return 0.0;
    }
    let s: f64 = idx
        .iter()
        .map(|&i| (a.magnitude_db[i] - b.magnitude_db[i]).powi(2))
        .sum();
    (s / idx.len() as f64).sqrt()
}

/// Décalage de niveau (dB) de `a` par rapport à `b` : médiane, dans la
/// bande, de l'écart point à point.
pub fn decalage_median_db(a: &LogResponse, b: &LogResponse, f_lo: f64, f_hi: f64) -> f64 {
    let mut d: Vec<f64> = dans_bande(&a.freqs, f_lo, f_hi)
        .into_iter()
        .map(|i| a.magnitude_db[i] - b.magnitude_db[i])
        .collect();
    if d.is_empty() {
        return 0.0;
    }
    mediane(&mut d)
}

/// Écart-type (dB) d'une réponse dans une bande : sa « rugosité ».
pub fn ecart_type_db(r: &LogResponse, f_lo: f64, f_hi: f64) -> f64 {
    let idx = dans_bande(&r.freqs, f_lo, f_hi);
    if idx.len() < 2 {
        return 0.0;
    }
    let m = idx.iter().map(|&i| r.magnitude_db[i]).sum::<f64>() / idx.len() as f64;
    let v = idx
        .iter()
        .map(|&i| (r.magnitude_db[i] - m).powi(2))
        .sum::<f64>()
        / idx.len() as f64;
    v.sqrt()
}

/// Écart-type des CREUX (dB) : écart quadratique de la seule partie de la
/// réponse qui passe SOUS sa médiane dans la bande. Les bosses (modes, que
/// l'égaliseur coupe) n'y entrent pas ; les annulations, si.
pub fn ecart_type_creux_db(r: &LogResponse, f_lo: f64, f_hi: f64) -> f64 {
    let idx = dans_bande(&r.freqs, f_lo, f_hi);
    if idx.is_empty() {
        return 0.0;
    }
    let mut v: Vec<f64> = idx.iter().map(|&i| r.magnitude_db[i]).collect();
    let med = mediane(&mut v);
    let s: f64 = idx
        .iter()
        .map(|&i| (r.magnitude_db[i] - med).min(0.0).powi(2))
        .sum();
    (s / idx.len() as f64).sqrt()
}

/// Creux le plus profond sous la médiane de la bande (dB, ≥ 0).
pub fn creux_max_db(r: &LogResponse, f_lo: f64, f_hi: f64) -> f64 {
    let idx = dans_bande(&r.freqs, f_lo, f_hi);
    if idx.is_empty() {
        return 0.0;
    }
    let mut v: Vec<f64> = idx.iter().map(|&i| r.magnitude_db[i]).collect();
    let med = mediane(&mut v);
    idx.iter()
        .map(|&i| med - r.magnitude_db[i])
        .fold(0.0, f64::max)
}

/// Pic le plus haut au-dessus de la médiane de la bande (dB, ≥ 0).
pub fn pic_max_db(r: &LogResponse, f_lo: f64, f_hi: f64) -> f64 {
    let idx = dans_bande(&r.freqs, f_lo, f_hi);
    if idx.is_empty() {
        return 0.0;
    }
    let mut v: Vec<f64> = idx.iter().map(|&i| r.magnitude_db[i]).collect();
    let med = mediane(&mut v);
    idx.iter()
        .map(|&i| r.magnitude_db[i] - med)
        .fold(0.0, f64::max)
}

fn verifier_grilles(reponses: &[LogResponse]) -> Result<(), String> {
    let g = &reponses[0].freqs;
    if g.len() < 2 {
        return Err("réponse vide".into());
    }
    for (k, r) in reponses.iter().enumerate() {
        if r.freqs != *g || r.magnitude_db.len() != g.len() {
            return Err(format!(
                "position {k} : grille de fréquences différente de la première"
            ));
        }
        if r.magnitude_db.iter().any(|d| !d.is_finite()) {
            return Err(format!("position {k} : réponse non finie"));
        }
    }
    Ok(())
}

/// Moyenne pondérée de positions retenues, calculée comme référence plus
/// moyenne pondérée des écarts à la référence (voir l'en-tête du module).
pub fn moyenne_ponderee(
    reponses: &[&LogResponse],
    poids: &[f64],
    reference: usize,
    moyenne: Moyenne,
) -> LogResponse {
    assert!(!reponses.is_empty() && reponses.len() == poids.len());
    let r0 = reponses[reference];
    let n = r0.freqs.len();
    let wsum: f64 = poids.iter().sum();
    let magnitude_db = (0..n)
        .map(|i| {
            let m0 = r0.magnitude_db[i];
            let mut acc = 0.0;
            for (r, w) in reponses.iter().zip(poids) {
                let d = r.magnitude_db[i] - m0;
                acc += w * match moyenne {
                    Moyenne::Puissance => 10f64.powf(d / 10.0),
                    Moyenne::Db => d,
                };
            }
            m0 + match moyenne {
                Moyenne::Puissance => 10.0 * (acc / wsum).log10(),
                Moyenne::Db => acc / wsum,
            }
        })
        .collect();
    LogResponse {
        freqs: r0.freqs.clone(),
        magnitude_db,
    }
}

/// Moyenne multi-positions : rejet des aberrantes, puis moyenne pondérée.
///
/// `centrale` : l'index de la place d'écoute. Elle sert de référence de la
/// moyenne si elle est retenue (sinon la première retenue).
///
/// Une seule position est acceptée (session mono-position ancienne) : la
/// moyenne est alors cette position, sans rejet possible. De 2 positions, on
/// ne peut pas dire laquelle est aberrante : rejet désactivé, avertissement.
pub fn moyenne_positions(
    reponses: &[LogResponse],
    centrale: usize,
    cfg: &ConfigPositions,
) -> Result<MoyennePositions, String> {
    if reponses.is_empty() {
        return Err("aucune position mesurée".into());
    }
    if reponses.len() > POSITIONS_MAX {
        return Err(format!(
            "{} positions : {POSITIONS_MAX} au plus",
            reponses.len()
        ));
    }
    if centrale >= reponses.len() {
        return Err(format!(
            "position centrale {centrale} : {} positions seulement",
            reponses.len()
        ));
    }
    verifier_grilles(reponses)?;
    let r = &cfg.rejet;
    if !r.seuil_db.is_finite() || r.seuil_db < 1.0 {
        return Err(format!("seuil de rejet {} dB : 1 dB au moins", r.seuil_db));
    }
    if !r.niveau_db.is_finite() || r.niveau_db < 1.0 {
        return Err(format!(
            "décalage de niveau toléré {} dB : 1 dB au moins",
            r.niveau_db
        ));
    }
    if !r.facteur.is_finite() || r.facteur < 1.5 {
        return Err(format!("facteur de rejet {} : 1,5 au moins", r.facteur));
    }
    if !(r.f_lo_hz.is_finite() && r.f_hi_hz.is_finite() && r.f_hi_hz > r.f_lo_hz) {
        return Err("bande de rejet invalide".into());
    }
    let poids = cfg.ponderation.poids(reponses.len(), centrale)?;
    let mut avertissements = Vec::new();

    let mediane_c = courbe_mediane(reponses);
    let ecarts: Vec<f64> = reponses
        .iter()
        .map(|x| ecart_rms_db(x, &mediane_c, r.f_lo_hz, r.f_hi_hz))
        .collect();
    let decalages: Vec<f64> = reponses
        .iter()
        .map(|x| decalage_median_db(x, &mediane_c, r.f_lo_hz, r.f_hi_hz))
        .collect();
    let rejet_possible = reponses.len() >= POSITIONS_MIN;
    if reponses.len() == 2 {
        avertissements.push(
            "deux positions : impossible de dire laquelle serait aberrante, aucune n'est rejetée"
                .into(),
        );
    }
    let seuil = r.seuil_db.max(r.facteur * mediane(&mut ecarts.clone()));
    let verdicts: Vec<VerdictPosition> = ecarts
        .iter()
        .enumerate()
        .map(|(k, e)| {
            let niveau = decalages[k];
            let par_niveau = rejet_possible && niveau.abs() > r.niveau_db;
            let par_forme = rejet_possible && *e > seuil;
            let motif = if par_niveau {
                Some(format!(
                    "niveau décalé de {niveau:+.1} dB par rapport aux autres positions (tolérance ±{:.1} dB) : micro couvert, tourné, ou gain changé ? Refaire cette position",
                    r.niveau_db
                ))
            } else if par_forme {
                Some(format!(
                    "écart de {e:.1} dB à la médiane des positions (seuil {seuil:.1} dB) : bruit, mauvaise zone, micro hors de la zone d'écoute ? Refaire cette position"
                ))
            } else {
                None
            };
            VerdictPosition {
                index: k,
                poids: poids[k],
                ecart_median_db: *e,
                decalage_niveau_db: niveau,
                rejetee: motif.is_some(),
                motif,
            }
        })
        .collect();
    let retenues: Vec<usize> = verdicts
        .iter()
        .filter(|v| !v.rejetee)
        .map(|v| v.index)
        .collect();
    // La médiane borne le nombre de rejets à moins de la moitié ; la garde
    // reste, au cas où.
    if retenues.is_empty() {
        return Err("toutes les positions sont rejetées".into());
    }
    if verdicts[centrale].rejetee {
        avertissements.push(format!(
            "la position centrale ({centrale}) est rejetée : la moyenne se fait sans elle, la refaire"
        ));
    }
    if rejet_possible && retenues.len() < POSITIONS_MIN && reponses.len() >= POSITIONS_MIN {
        avertissements.push(format!(
            "{} positions retenues sur {} : moyenne peu représentative",
            retenues.len(),
            reponses.len()
        ));
    }
    let refs: Vec<&LogResponse> = retenues.iter().map(|&k| &reponses[k]).collect();
    let w: Vec<f64> = retenues.iter().map(|&k| poids[k]).collect();
    let reference = retenues.iter().position(|&k| k == centrale).unwrap_or(0);
    let reponse = moyenne_ponderee(&refs, &w, reference, cfg.moyenne);
    Ok(MoyennePositions {
        reponse,
        verdicts,
        seuil_applique_db: seuil,
        avertissements,
    })
}

/// Indicateurs d'une réponse dans la bande comparée.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Indicateurs {
    pub ecart_type_db: f64,
    pub ecart_type_creux_db: f64,
    pub creux_max_db: f64,
    pub pic_max_db: f64,
    /// Écart efficace à la cible alignée (dB), si une cible est donnée.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ecart_cible_db: Option<f64>,
}

impl Indicateurs {
    pub fn de(r: &LogResponse, f_lo: f64, f_hi: f64, cible: Option<&[f64]>) -> Self {
        Self {
            ecart_type_db: ecart_type_db(r, f_lo, f_hi),
            ecart_type_creux_db: ecart_type_creux_db(r, f_lo, f_hi),
            creux_max_db: creux_max_db(r, f_lo, f_hi),
            pic_max_db: pic_max_db(r, f_lo, f_hi),
            ecart_cible_db: cible.map(|c| ecart_cible_aligne(r, c, f_lo, f_hi)),
        }
    }
}

/// Écart efficace à une cible (même grille), la cible étant d'abord alignée
/// sur la moyenne de la réponse dans la bande (le niveau absolu ne compte pas).
fn ecart_cible_aligne(r: &LogResponse, cible: &[f64], f_lo: f64, f_hi: f64) -> f64 {
    let idx = dans_bande(&r.freqs, f_lo, f_hi);
    if idx.is_empty() {
        return 0.0;
    }
    let d: Vec<f64> = idx.iter().map(|&i| r.magnitude_db[i] - cible[i]).collect();
    let m = d.iter().sum::<f64>() / d.len() as f64;
    (d.iter().map(|x| (x - m).powi(2)).sum::<f64>() / d.len() as f64).sqrt()
}

/// Comparaison A/B : la même pièce avant et après correction (deux mesures,
/// ou deux moyennes multi-positions), sur la même grille.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComparaisonAB {
    pub bande_hz: (f64, f64),
    pub avant: Indicateurs,
    pub apres: Indicateurs,
    pub freqs_hz: Vec<f64>,
    pub avant_db: Vec<f64>,
    pub apres_db: Vec<f64>,
    /// Après − avant, point à point (dB).
    pub difference_db: Vec<f64>,
    /// Vrai si l'écart-type dans la bande a baissé.
    pub amelioree: bool,
}

/// Compare deux réponses de même grille dans `[f_lo, f_hi]`. `cible` : la
/// courbe cible sur la même grille (facultative).
pub fn comparer_ab(
    avant: &LogResponse,
    apres: &LogResponse,
    f_lo: f64,
    f_hi: f64,
    cible: Option<&[f64]>,
) -> Result<ComparaisonAB, String> {
    verifier_grilles(&[avant.clone(), apres.clone()])
        .map_err(|e| format!("A/B : {e} (mesures faites avec des réglages différents)"))?;
    if !(f_lo.is_finite() && f_hi.is_finite() && f_hi > f_lo) {
        return Err("A/B : bande invalide".into());
    }
    if cible.is_some_and(|c| c.len() != avant.freqs.len()) {
        return Err("A/B : cible sur une autre grille".into());
    }
    let a = Indicateurs::de(avant, f_lo, f_hi, cible);
    let b = Indicateurs::de(apres, f_lo, f_hi, cible);
    Ok(ComparaisonAB {
        bande_hz: (f_lo, f_hi),
        amelioree: b.ecart_type_db < a.ecart_type_db,
        avant: a,
        apres: b,
        freqs_hz: avant.freqs.clone(),
        avant_db: avant.magnitude_db.clone(),
        apres_db: apres.magnitude_db.clone(),
        difference_db: avant
            .magnitude_db
            .iter()
            .zip(&apres.magnitude_db)
            .map(|(x, y)| y - x)
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn courbe(f: impl Fn(f64) -> f64) -> LogResponse {
        let freqs = LogResponse::grid(20.0, 1000.0, 24);
        let magnitude_db = freqs.iter().map(|x| f(*x)).collect();
        LogResponse {
            freqs,
            magnitude_db,
        }
    }

    #[test]
    fn des_positions_identiques_rendent_la_courbe_au_bit_pres() {
        let r = courbe(|f| 3.0 * (f / 40.0).ln().sin() - 0.37 * (f / 7.0).sqrt());
        for moyenne in [Moyenne::Puissance, Moyenne::Db] {
            for n in 1..=POSITIONS_MAX {
                let cfg = ConfigPositions {
                    moyenne,
                    ..Default::default()
                };
                let m = moyenne_positions(&vec![r.clone(); n], 0, &cfg).unwrap();
                assert_eq!(m.reponse, r, "{moyenne:?}, {n} positions");
                assert!(m.verdicts.iter().all(|v| !v.rejetee));
            }
        }
    }

    #[test]
    fn la_ponderation_favorise_la_centrale() {
        let a = courbe(|_| 0.0);
        let b = courbe(|_| -6.0);
        let cfg = ConfigPositions {
            moyenne: Moyenne::Db,
            ..Default::default()
        };
        // 0 dB (poids 2) et deux fois −6 dB (poids 1) : −3 dB.
        let m = moyenne_positions(&[a.clone(), b.clone(), b.clone()], 0, &cfg).unwrap();
        assert!(m
            .reponse
            .magnitude_db
            .iter()
            .all(|d| (d + 3.0).abs() < 1e-12));
        // Poids explicites.
        let cfg = ConfigPositions {
            moyenne: Moyenne::Db,
            ponderation: Ponderation {
                poids_central: 1.0,
                poids: Some(vec![4.0, 1.0, 1.0]),
            },
            ..Default::default()
        };
        let m = moyenne_positions(&[a, b.clone(), b], 0, &cfg).unwrap();
        assert!(m
            .reponse
            .magnitude_db
            .iter()
            .all(|d| (d + 2.0).abs() < 1e-12));
    }

    #[test]
    fn une_position_aberrante_est_rejetee_les_autres_non() {
        let base = |k: f64| courbe(move |f| 2.0 * ((f / 30.0).ln() * (3.0 + k)).sin());
        let mut v: Vec<LogResponse> = (0..4).map(|k| base(k as f64 * 0.2)).collect();
        v.push(courbe(|_| -18.0));
        let m = moyenne_positions(&v, 0, &ConfigPositions::default()).unwrap();
        let rej: Vec<usize> = m
            .verdicts
            .iter()
            .filter(|x| x.rejetee)
            .map(|x| x.index)
            .collect();
        assert_eq!(rej, vec![4], "{:?}", m.verdicts);
        assert!(m.verdicts[4].motif.is_some());
        assert_eq!(m.retenues(), 4);
    }

    #[test]
    fn les_bornes_sont_refusees() {
        let r = courbe(|_| 0.0);
        assert!(moyenne_positions(&vec![r.clone(); 10], 0, &Default::default()).is_err());
        assert!(
            moyenne_positions(&[r.clone(), r.clone(), r.clone()], 3, &Default::default()).is_err()
        );
        let cfg = ConfigPositions {
            ponderation: Ponderation {
                poids_central: 50.0,
                poids: None,
            },
            ..Default::default()
        };
        assert!(moyenne_positions(&[r.clone(), r.clone(), r.clone()], 0, &cfg).is_err());
        let cfg = ConfigPositions {
            ponderation: Ponderation {
                poids_central: 2.0,
                poids: Some(vec![1.0, 1.0]),
            },
            ..Default::default()
        };
        assert!(moyenne_positions(&[r.clone(), r.clone(), r], 0, &cfg).is_err());
    }

    #[test]
    fn l_ab_dit_si_la_correction_aplatit() {
        let avant = courbe(|f| 6.0 * (-(((f / 50.0).ln()).powi(2)) * 20.0).exp());
        let apres = courbe(|f| 1.0 * (-(((f / 50.0).ln()).powi(2)) * 20.0).exp());
        let ab = comparer_ab(&avant, &apres, 20.0, 300.0, None).unwrap();
        assert!(ab.amelioree);
        assert!(ab.apres.pic_max_db < ab.avant.pic_max_db);
        assert_eq!(ab.difference_db.len(), avant.freqs.len());
    }
}
