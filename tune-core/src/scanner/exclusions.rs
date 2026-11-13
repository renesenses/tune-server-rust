//! #7478 — ce que le scan et la surveillance des dossiers laissent de côté.
//!
//! Deux sources, cumulées :
//!
//! - les **exclusions globales** (`scan_exclude_paths`), inchangées : des
//!   sous-chaînes de chemin, sans tenir compte de la casse ;
//! - les **exclusions par emplacement** (`scan_exclusions_par_emplacement`) :
//!   pour une racine de musique, des motifs glob lus RELATIVEMENT à cette
//!   racine.
//!
//! Lecture d'un motif d'emplacement :
//!
//! - sans `/` (`*.bak`, `Incoming`) : il vise un NOM, à n'importe quelle
//!   profondeur — un fichier ou un dossier dont le nom correspond est écarté,
//!   avec tout ce qu'il contient ;
//! - avec `/` (`**/Incoming/**`, `Rips/*.wav`) : il vise le chemin relatif
//!   entier. `*` ne franchit pas `/`, `**` franchit les dossiers. Un motif
//!   terminé par `/**` écarte aussi le dossier lui-même.
//!
//! Insensible à la casse par défaut ; `case_sensitive: true` par emplacement.

use glob::{MatchOptions, Pattern};
use serde::{Deserialize, Serialize};
use tracing::warn;

/// La clé de réglage des exclusions par emplacement (JSON : liste de
/// [`ExclusionsDEmplacement`]).
pub const CLE_EXCLUSIONS_PAR_EMPLACEMENT: &str = "scan_exclusions_par_emplacement";

/// La clé de réglage des exclusions globales (JSON : liste de sous-chaînes).
pub const CLE_EXCLUSIONS_GLOBALES: &str = "scan_exclude_paths";

/// Les motifs d'un emplacement de stockage, tels que l'écran les écrit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExclusionsDEmplacement {
    /// La racine de musique, telle qu'elle figure dans `music_dirs`.
    pub root: String,
    #[serde(default)]
    pub patterns: Vec<String>,
    #[serde(default)]
    pub case_sensitive: bool,
}

/// Vérifier des règles avant de les enregistrer : le premier motif illisible
/// est nommé.
pub fn valider(regles: &[ExclusionsDEmplacement]) -> Result<(), String> {
    for r in regles {
        if r.root.trim().is_empty() {
            return Err("emplacement sans racine".into());
        }
        for m in &r.patterns {
            let m = m.trim();
            if m.is_empty() {
                return Err(format!("motif vide pour {}", r.root));
            }
            Pattern::new(m).map_err(|e| format!("motif invalide « {m} » : {e}"))?;
        }
    }
    Ok(())
}

/// Des chemins comparés avec `/` pour séparateur, quel que soit le système.
fn unifier(chemin: &str) -> String {
    chemin.replace('\\', "/")
}

#[derive(Debug)]
struct Motif {
    motif: Pattern,
    /// Le motif sans son `/**` final, pour écarter le dossier lui-même.
    dossier: Option<Pattern>,
    vise_un_nom: bool,
}

#[derive(Debug)]
struct RacineCompilee {
    racine: String,
    motifs: Vec<Motif>,
    options: MatchOptions,
}

impl RacineCompilee {
    fn relatif<'c>(&self, chemin: &'c str) -> Option<&'c str> {
        let reste = chemin.strip_prefix(self.racine.as_str())?;
        let reste = reste.strip_prefix('/')?;
        (!reste.is_empty()).then_some(reste)
    }

    fn exclut(&self, relatif: &str) -> bool {
        self.motifs.iter().any(|m| {
            if m.vise_un_nom {
                relatif
                    .split('/')
                    .any(|nom| m.motif.matches_with(nom, self.options))
            } else {
                m.motif.matches_with(relatif, self.options)
                    || m.dossier
                        .as_ref()
                        .is_some_and(|d| d.matches_with(relatif, self.options))
            }
        })
    }
}

/// Les exclusions compilées, prêtes pour le parcours.
#[derive(Debug, Default)]
pub struct Exclusions {
    globales: Vec<String>,
    par_racine: Vec<RacineCompilee>,
}

impl Exclusions {
    /// Les seules exclusions globales (le comportement d'avant #7478).
    pub fn globales_seules(globales: &[String]) -> Self {
        Self::new(globales, &[])
    }

    /// Un motif illisible est écarté (journalisé) : il a été refusé à
    /// l'enregistrement, il ne peut venir que d'une écriture à la main.
    pub fn new(globales: &[String], regles: &[ExclusionsDEmplacement]) -> Self {
        let globales = globales
            .iter()
            .map(|p| p.trim().to_lowercase())
            .filter(|p| !p.is_empty())
            .collect();
        let par_racine = regles
            .iter()
            .filter_map(|r| {
                let racine = unifier(&crate::scanner::walker::normalize_path(&r.root));
                let racine = racine.trim_end_matches('/').to_string();
                if racine.is_empty() {
                    return None;
                }
                let motifs: Vec<Motif> = r
                    .patterns
                    .iter()
                    .map(|m| m.trim())
                    .filter(|m| !m.is_empty())
                    .filter_map(|m| match Pattern::new(m) {
                        Ok(motif) => Some(Motif {
                            dossier: m
                                .strip_suffix("/**")
                                .filter(|d| !d.is_empty())
                                .and_then(|d| Pattern::new(d).ok()),
                            vise_un_nom: !m.contains('/'),
                            motif,
                        }),
                        Err(e) => {
                            warn!(motif = %m, erreur = %e, "scan_exclusion_motif_illisible_ignore");
                            None
                        }
                    })
                    .collect();
                (!motifs.is_empty()).then(|| RacineCompilee {
                    racine,
                    motifs,
                    options: MatchOptions {
                        case_sensitive: r.case_sensitive,
                        require_literal_separator: true,
                        require_literal_leading_dot: false,
                    },
                })
            })
            .collect();
        Self {
            globales,
            par_racine,
        }
    }

    /// Les sous-chaînes globales, en minuscules.
    pub fn globales(&self) -> &[String] {
        &self.globales
    }

    pub fn est_vide(&self) -> bool {
        self.globales.is_empty() && self.par_racine.is_empty()
    }

    pub fn a_des_exclusions_par_emplacement(&self) -> bool {
        !self.par_racine.is_empty()
    }

    /// Le chemin est-il écarté par une sous-chaîne globale ?
    pub fn exclut_globalement(&self, chemin: &str) -> bool {
        if self.globales.is_empty() {
            return false;
        }
        let l = chemin.to_lowercase();
        self.globales.iter().any(|x| l.contains(x.as_str()))
    }

    /// Le chemin est-il écarté par un motif de SON emplacement ?
    pub fn exclut_par_emplacement(&self, chemin: &str) -> bool {
        if self.par_racine.is_empty() {
            return false;
        }
        let chemin = unifier(chemin);
        self.par_racine
            .iter()
            .any(|r| r.relatif(&chemin).is_some_and(|rel| r.exclut(rel)))
    }

    /// Écarté par l'une ou l'autre source.
    pub fn exclut(&self, chemin: &str) -> bool {
        self.exclut_globalement(chemin) || self.exclut_par_emplacement(chemin)
    }
}

#[cfg(test)]
#[path = "exclusions_7478_tests.rs"]
mod tests;
