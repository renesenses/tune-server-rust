//! Préréglages de chaîne et mode d'application (maquette A2, lot T17).
//!
//! « A preset keeps every plugin, their order and all their settings. » Un
//! préréglage est donc une [`ChaineTess`] complète, nommée. Les préréglages
//! sont GLOBAUX (une liste pour le serveur) ; chaque zone garde sa propre
//! chaîne. Les préréglages propres à un greffon (`eq_presets`,
//! `crossfeed_presets`…) restent dans ses réglages.
//!
//! « Pure — no processing » est un préréglage fourni par le serveur
//! ([`PreReglage::pure`]) : chaîne vide et `bypass_all`, soit le bit-perfect
//! quand le volume est à 0 dB.

use serde::{Deserialize, Serialize};

use super::chaine::ChaineTess;

/// Id réservé du préréglage « Pure — no processing ».
pub const ID_PREREGLAGE_PURE: &str = "pure";

/// Quand un changement de chaîne prend effet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ModeApplication {
    /// La lecture se met en pause puis reprend avec les nouveaux réglages
    /// (relance à la position, #8422).
    #[default]
    Immediately,
    /// Le changement attend la prochaine lecture.
    AtNextPlayback,
}

/// Un préréglage de chaîne.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreReglage {
    /// Identifiant stable, minuscules ASCII, chiffres et tirets.
    pub id: String,
    /// Nom choisi par l'utilisateur (« Name the current chain »).
    pub name: String,
    /// Fourni par le serveur (« Pure ») plutôt que créé par l'utilisateur.
    #[serde(default)]
    pub builtin: bool,
    pub chain: ChaineTess,
}

impl PreReglage {
    /// « Pure — no processing ».
    pub fn pure() -> Self {
        Self {
            id: ID_PREREGLAGE_PURE.into(),
            name: "Pure — no processing".into(),
            builtin: true,
            chain: ChaineTess {
                bypass_all: true,
                ..ChaineTess::default()
            },
        }
    }
}

/// La liste des préréglages et le mode d'application.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreReglages {
    /// Le préréglage chargé (« ACTIVE »), s'il y en a un.
    pub active: Option<String>,
    #[serde(default)]
    pub apply_mode: ModeApplication,
    pub presets: Vec<PreReglage>,
}

impl Default for PreReglages {
    fn default() -> Self {
        Self {
            active: None,
            apply_mode: ModeApplication::default(),
            presets: vec![PreReglage::pure()],
        }
    }
}

/// Pourquoi une liste de préréglages est refusée (409).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ErreurPreReglage {
    #[error("identifiant de préréglage invalide : {0:?}")]
    IdInvalide(String),
    #[error("préréglage {0} présent deux fois")]
    IdEnDouble(String),
    #[error("le préréglage {0} n'a pas de nom")]
    NomVide(String),
    #[error("le préréglage actif {0} n'existe pas")]
    ActifInconnu(String),
    #[error("le préréglage Pure doit rester sans traitement")]
    PureModifie,
}

impl PreReglages {
    pub fn get(&self, id: &str) -> Option<&PreReglage> {
        self.presets.iter().find(|p| p.id == id)
    }

    /// Cohérence de la liste. La validité de chaque chaîne contre le
    /// catalogue et les droits se vérifie au CHARGEMENT
    /// ([`ChaineTess::valider`]) : un préréglage Premium reste enregistré sur
    /// un compte redevenu gratuit.
    pub fn verifier(&self) -> Result<(), ErreurPreReglage> {
        for (i, p) in self.presets.iter().enumerate() {
            let id_ok = !p.id.is_empty()
                && p.id.len() <= 64
                && p.id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !id_ok {
                return Err(ErreurPreReglage::IdInvalide(p.id.clone()));
            }
            if self.presets[..i].iter().any(|q| q.id == p.id) {
                return Err(ErreurPreReglage::IdEnDouble(p.id.clone()));
            }
            if p.name.trim().is_empty() {
                return Err(ErreurPreReglage::NomVide(p.id.clone()));
            }
            if p.id == ID_PREREGLAGE_PURE && p.chain != PreReglage::pure().chain {
                return Err(ErreurPreReglage::PureModifie);
            }
        }
        if let Some(a) = &self.active
            && self.get(a).is_none()
        {
            return Err(ErreurPreReglage::ActifInconnu(a.clone()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tess::chaine::MaillonGreffon;
    use crate::tess::greffon::catalogue_de_reference;
    use crate::tess::{CodeEtape, ContexteValidation};
    use serde_json::{Value, json};

    fn soiree() -> PreReglage {
        PreReglage {
            id: "evening-listening".into(),
            name: "Evening listening".into(),
            builtin: false,
            chain: ChaineTess {
                plugins: vec![MaillonGreffon {
                    id: "equalizer-graphic".into(),
                    enabled: true,
                    settings: Value::Null,
                }],
                ..ChaineTess::default()
            },
        }
    }

    #[test]
    fn les_prereglages_font_l_aller_retour_json() {
        let mut l = PreReglages::default();
        l.presets.push(soiree());
        l.active = Some("evening-listening".into());
        l.apply_mode = ModeApplication::AtNextPlayback;
        l.verifier().unwrap();
        let texte = serde_json::to_string(&l).unwrap();
        assert_eq!(serde_json::from_str::<PreReglages>(&texte).unwrap(), l);
        assert_eq!(
            serde_json::to_value(l.apply_mode).unwrap(),
            json!("at_next_playback")
        );
        assert_eq!(
            serde_json::to_value(ModeApplication::Immediately).unwrap(),
            json!("immediately")
        );
    }

    #[test]
    fn pure_ne_traite_rien() {
        let p = PreReglage::pure();
        p.chain
            .valider(
                &catalogue_de_reference(),
                ContexteValidation {
                    channels: Some(2),
                    premium: false,
                },
            )
            .unwrap();
        assert_eq!(p.chain.headroom_effectif_db(), 0.0);
        let actives: Vec<_> = p
            .chain
            .ordre_des_etapes()
            .into_iter()
            .filter(|e| e.active)
            .map(|e| e.code)
            .collect();
        assert_eq!(
            actives,
            [
                CodeEtape::Source,
                CodeEtape::Volume,
                CodeEtape::Quantization,
                CodeEtape::Output
            ]
        );
    }

    #[test]
    fn une_liste_de_prereglages_incoherente_est_refusee() {
        let base = || {
            let mut l = PreReglages::default();
            l.presets.push(soiree());
            l
        };
        base().verifier().unwrap();

        let mut l = base();
        l.active = Some("headphones".into());
        assert_eq!(
            l.verifier(),
            Err(ErreurPreReglage::ActifInconnu("headphones".into()))
        );

        let mut l = base();
        l.presets.push(soiree());
        assert_eq!(
            l.verifier(),
            Err(ErreurPreReglage::IdEnDouble("evening-listening".into()))
        );

        let mut l = base();
        l.presets[1].name = "  ".into();
        assert_eq!(
            l.verifier(),
            Err(ErreurPreReglage::NomVide("evening-listening".into()))
        );

        let mut l = base();
        l.presets[1].id = "Evening listening".into();
        assert!(matches!(l.verifier(), Err(ErreurPreReglage::IdInvalide(_))));

        let mut l = base();
        l.presets[0].chain.bypass_all = false;
        l.presets[0].chain.plugins = soiree().chain.plugins;
        assert_eq!(l.verifier(), Err(ErreurPreReglage::PureModifie));

        // Pure peut être supprimé (la croix de la maquette A2).
        let mut l = base();
        l.presets.remove(0);
        l.verifier().unwrap();
    }

    #[test]
    fn un_champ_inconnu_dans_un_prereglage_est_refuse() {
        let texte =
            r#"{"active":null,"presets":[{"id":"x","name":"X","chain":{"version":1},"order":3}]}"#;
        assert!(serde_json::from_str::<PreReglages>(texte).is_err());
        let ok = r#"{"active":null,"presets":[{"id":"x","name":"X","chain":{"version":1}}]}"#;
        let l: PreReglages = serde_json::from_str(ok).unwrap();
        assert_eq!(l.apply_mode, ModeApplication::Immediately);
        assert!(!l.presets[0].builtin);
    }
}
