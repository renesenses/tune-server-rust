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
//! quand le volume est à 0 dB. Charger un préréglage REMPLACE la chaîne de la
//! zone : l'écran (W4) propose d'enregistrer la chaîne courante avant.
//!
//! Le préréglage actif est propre à chaque ZONE (maquette A1 : sélecteurs
//! Zone et Preset) : il est publié dans la réponse de la zone
//! (`active_preset`), pas dans cette liste globale.

use serde::{Deserialize, Serialize};

use super::chaine::ChaineTess;
use super::greffon::id_valide;

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
    /// Valeur de repli à la lecture : traitée comme `at_next_playback`
    /// (jamais d'interruption non voulue).
    #[serde(other)]
    Unknown,
}

/// Un préréglage de chaîne.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
pub struct PreReglages {
    #[serde(default)]
    pub apply_mode: ModeApplication,
    pub presets: Vec<PreReglage>,
}

impl Default for PreReglages {
    fn default() -> Self {
        Self {
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
            if !id_valide(&p.id) {
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tess::chaine::MaillonGreffon;
    use crate::tess::greffon::catalogue_de_reference;
    use crate::tess::resolution::{
        Badge, ContexteExecution, ControleVolume, SourceFlux, VolumeZone,
    };
    use crate::tess::{ContexteValidation, TransportZone};
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

    fn tout(_: &str) -> bool {
        true
    }

    #[test]
    fn pure_est_bit_perfect_meme_sans_droit() {
        let p = PreReglage::pure();
        p.chain
            .valider(
                &catalogue_de_reference(),
                ContexteValidation { premium: false },
            )
            .unwrap();
        let ctx = ContexteExecution {
            transport: TransportZone::Local,
            source: Some(SourceFlux::Pcm {
                sample_rate_hz: 44_100,
                bits: 16,
                channels: 2,
            }),
            premium: false,
            greffon_disponible: &tout,
            pure: false,
            volume: VolumeZone {
                control: ControleVolume::Software,
                db: 0.0,
            },
        };
        let r = p.chain.resoudre(&catalogue_de_reference(), &ctx);
        assert_eq!(r.badge, Badge::BitPerfect);
        assert_eq!(r.headroom_db, 0.0);
        // Contre-épreuve : le préréglage « soirée » traite.
        let r = soiree().chain.resoudre(&catalogue_de_reference(), &ctx);
        assert_eq!(r.badge, Badge::ProcessedByTess);
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

        // Même règle d'identifiant que les greffons (et que le schéma).
        for mauvais in ["Evening listening", "-x", "x-", ""] {
            let mut l = base();
            l.presets[1].id = mauvais.into();
            assert_eq!(
                l.verifier(),
                Err(ErreurPreReglage::IdInvalide(mauvais.into()))
            );
        }

        let mut l = base();
        l.presets[0].chain.bypass_all = false;
        l.presets[0].chain.plugins = soiree().chain.plugins;
        assert_eq!(l.verifier(), Err(ErreurPreReglage::PureModifie));

        // Pure peut être supprimé (la croix de la maquette A2).
        let mut l = base();
        l.presets.remove(0);
        l.verifier().unwrap();
        assert!(l.get(ID_PREREGLAGE_PURE).is_none());
    }

    #[test]
    fn un_client_v1_lit_une_liste_plus_recente() {
        let texte = r#"{"apply_mode":"crossfade","shared":true,
            "presets":[{"id":"x","name":"X","chain":{"version":1},"color":"blue"}]}"#;
        let l: PreReglages = serde_json::from_str(texte).unwrap();
        assert_eq!(l.apply_mode, ModeApplication::Unknown);
        assert!(!l.presets[0].builtin);
        let l: PreReglages = serde_json::from_str(r#"{"presets":[]}"#).unwrap();
        assert_eq!(l.apply_mode, ModeApplication::Immediately);
    }
}
