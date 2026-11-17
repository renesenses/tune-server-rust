//! Le descripteur d'un greffon TESS et le catalogue de référence.
//!
//! Un greffon TESS est un traitement DSP du SDK natif : référence compilée dans
//! le serveur ou paquet `.tuneplugin` signé (étude, § A4). Pas de troisième
//! système de greffons.
//!
//! Pour les greffons des dépôts PRIVÉS (Crossfeed Pro, Crosstalk), le public ne
//! connaît que l'id, le droit et les contraintes de place : leur schéma de
//! réglages vient du paquet signé ([`Origine::Package`]).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Le droit d'accès à un greffon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Droit {
    Free,
    Premium,
}

/// D'où vient le traitement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origine {
    /// Référence du SDK compilée dans le serveur (`sdk/tune-plugin-*`).
    Reference,
    /// Paquet `.tuneplugin` signé (minisign), installé depuis le catalogue.
    Package,
}

/// Ce qu'un greffon fait du nombre de canaux.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Canaux {
    /// Travaille sur n'importe quel nombre de canaux, sans le changer.
    Any,
    /// Stéréo seulement : refusé si le flux qui lui arrive n'a pas 2 canaux.
    /// Un greffon n'est « stéréo seulement » que s'il le DÉCLARE.
    StereoOnly,
    /// Change le nombre de canaux (réaffectation). Impose
    /// [`Position::LastPlugin`] : aucun greffon ne voit un flux réaffecté.
    Remaps,
}

/// Où un greffon peut se placer parmi les greffons de la chaîne.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Position {
    /// N'importe où (ordre libre, glisser-déposer).
    Free,
    /// Dernier des greffons, juste avant les étages fixes de sortie.
    LastPlugin,
}

/// Le groupe d'un greffon dans la fenêtre « Add plugin » (maquette A3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Categorie {
    Equalisation,
    Spatial,
    RoomAndFilters,
    RestorationAndRouting,
}

/// Les contraintes de place d'un greffon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContraintesPlace {
    pub channels: Canaux,
    pub position: Position,
    /// Deux greffons ACTIFS d'un même groupe ne coexistent jamais dans une
    /// chaîne (crossfeed, Crossfeed Pro et Crosstalk : #7616).
    pub exclusive_group: Option<String>,
}

/// Le descripteur public d'un greffon TESS.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DescripteurGreffon {
    /// Identifiant stable, minuscules ASCII, chiffres et tirets.
    pub id: String,
    /// Nom d'affichage (anglais, traduit par les clients).
    pub name: String,
    /// Groupe d'affichage du catalogue (maquette A3).
    pub category: Categorie,
    pub entitlement: Droit,
    pub origin: Origine,
    /// Schéma JSON (draft 2020-12) des réglages du greffon. Pour une
    /// référence, il vient de la caisse du SDK (fonction `schemas`) ; pour un
    /// paquet, de son manifeste. L'hôte en vérifie `type`, `required` et
    /// `additionalProperties: false` ; le reste est vérifié par le greffon.
    pub settings_schema: Value,
    pub placement: ContraintesPlace,
}

/// Pourquoi un descripteur ou un catalogue est refusé.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ErreurCatalogue {
    #[error("identifiant de greffon invalide : {0:?}")]
    IdInvalide(String),
    #[error("greffon {0} déclaré deux fois")]
    IdEnDouble(String),
    #[error("le greffon {0} change le nombre de canaux sans être placé en dernier")]
    ReaffectationPasEnDernier(String),
    #[error("le schéma de réglages du greffon {0} n'est pas un objet JSON")]
    SchemaInvalide(String),
}

fn id_valide(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('-')
        && !id.ends_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

impl DescripteurGreffon {
    /// Cohérence interne du descripteur.
    pub fn verifier(&self) -> Result<(), ErreurCatalogue> {
        if !id_valide(&self.id) {
            return Err(ErreurCatalogue::IdInvalide(self.id.clone()));
        }
        if self.placement.channels == Canaux::Remaps
            && self.placement.position != Position::LastPlugin
        {
            return Err(ErreurCatalogue::ReaffectationPasEnDernier(self.id.clone()));
        }
        if !self.settings_schema.is_object() {
            return Err(ErreurCatalogue::SchemaInvalide(self.id.clone()));
        }
        Ok(())
    }

    /// Vérifie des réglages contre le schéma, au niveau de l'hôte : objet (ou
    /// `null` = réglages par défaut), clés `required` présentes, aucune clé
    /// inconnue si le schéma dit `additionalProperties: false`.
    pub fn verifier_reglages(&self, reglages: &Value) -> Result<(), String> {
        let objet = match reglages {
            Value::Null => return Ok(()),
            Value::Object(o) => o,
            _ => return Err("settings must be a JSON object".into()),
        };
        if let Some(requis) = self
            .settings_schema
            .get("required")
            .and_then(Value::as_array)
        {
            for cle in requis.iter().filter_map(Value::as_str) {
                if !objet.contains_key(cle) {
                    return Err(format!("missing setting {cle:?}"));
                }
            }
        }
        let ferme = self.settings_schema.get("additionalProperties") == Some(&Value::Bool(false));
        if ferme {
            let connues = self
                .settings_schema
                .get("properties")
                .and_then(Value::as_object);
            for cle in objet.keys() {
                if !connues.is_some_and(|p| p.contains_key(cle)) {
                    return Err(format!("unknown setting {cle:?}"));
                }
            }
        }
        Ok(())
    }
}

/// Le catalogue des greffons TESS connus de l'hôte.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalogue {
    pub plugins: Vec<DescripteurGreffon>,
}

impl Catalogue {
    pub fn get(&self, id: &str) -> Option<&DescripteurGreffon> {
        self.plugins.iter().find(|d| d.id == id)
    }

    pub fn verifier(&self) -> Result<(), ErreurCatalogue> {
        for (i, d) in self.plugins.iter().enumerate() {
            d.verifier()?;
            if self.plugins[..i].iter().any(|p| p.id == d.id) {
                return Err(ErreurCatalogue::IdEnDouble(d.id.clone()));
            }
        }
        Ok(())
    }
}

/// Groupe exclusif de la famille « croisement des voies » (#7616).
pub const GROUPE_CROSSFEED_CROSSTALK: &str = "crossfeed_crosstalk";

#[allow(clippy::too_many_arguments)]
fn descripteur(
    id: &str,
    name: &str,
    entitlement: Droit,
    origin: Origine,
    channels: Canaux,
    position: Position,
    exclusive_group: Option<&str>,
) -> DescripteurGreffon {
    DescripteurGreffon {
        id: id.into(),
        name: name.into(),
        category: match id {
            "equalizer-graphic" | "equalizer-parametric" | "equalizer-headphone" => {
                Categorie::Equalisation
            }
            "crossfeed" | "crossfeed-pro" | "crosstalk" => Categorie::Spatial,
            "convolution" | "room-correction" => Categorie::RoomAndFilters,
            _ => Categorie::RestorationAndRouting,
        },
        entitlement,
        origin,
        // Schéma minimal : la caisse du SDK ou le paquet le complète au
        // branchement (lot T08).
        settings_schema: json!({ "type": "object" }),
        placement: ContraintesPlace {
            channels,
            position,
            exclusive_group: exclusive_group.map(str::to_owned),
        },
    }
}

/// Les dix greffons TESS de la v1.0 et leurs droits (décisions Q2, Q3, P1).
///
/// - Graphic EQ, Headphone EQ, Crossfeed (Q2 révisée le 17/11),
///   Channel remapping : Free ;
/// - Parametric EQ (Q3), Convolution, Room correction, Crossfeed Pro,
///   Crosstalk, Declick temps réel (P1) : Premium.
///
/// Le suréchantillonneur n'est PAS un greffon : c'est un étage fixe
/// ([`crate::tess::Upsampling`]). L'outil de fichiers `declick` reste dans
/// Studio : le greffon temps réel a un id distinct, `declick-live`.
pub fn catalogue_de_reference() -> Catalogue {
    use Canaux::{Any, Remaps, StereoOnly};
    use Droit::Premium;
    use Origine::{Package, Reference};
    let gratuit = Droit::Free;
    let libre = Position::Free;
    let g = Some(GROUPE_CROSSFEED_CROSSTALK);
    Catalogue {
        plugins: vec![
            descripteur(
                "equalizer-graphic",
                "Graphic EQ",
                gratuit,
                Reference,
                Any,
                libre,
                None,
            ),
            descripteur(
                "equalizer-parametric",
                "Parametric EQ",
                Premium,
                Reference,
                Any,
                libre,
                None,
            ),
            descripteur(
                "equalizer-headphone",
                "Headphone EQ",
                gratuit,
                Reference,
                Any,
                libre,
                None,
            ),
            descripteur(
                "convolution",
                "Convolution",
                Premium,
                Reference,
                Any,
                libre,
                None,
            ),
            descripteur(
                "room-correction",
                "Room correction",
                Premium,
                Reference,
                Any,
                libre,
                None,
            ),
            descripteur(
                "crossfeed",
                "Crossfeed",
                gratuit,
                Reference,
                StereoOnly,
                libre,
                g,
            ),
            descripteur(
                "crossfeed-pro",
                "Crossfeed Pro",
                Premium,
                Package,
                StereoOnly,
                libre,
                g,
            ),
            descripteur(
                "crosstalk",
                "Crosstalk",
                Premium,
                Package,
                StereoOnly,
                libre,
                g,
            ),
            descripteur(
                "channel-remap",
                "Channel remapping",
                gratuit,
                Reference,
                Remaps,
                Position::LastPlugin,
                None,
            ),
            descripteur(
                "declick-live",
                "Declick",
                Premium,
                Reference,
                Any,
                libre,
                None,
            ),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_catalogue_de_reference_est_coherent() {
        let c = catalogue_de_reference();
        assert_eq!(c.plugins.len(), 10);
        c.verifier().unwrap();
        let droit = |id: &str| c.get(id).unwrap().entitlement;
        assert_eq!(droit("equalizer-graphic"), Droit::Free);
        assert_eq!(droit("equalizer-headphone"), Droit::Free);
        assert_eq!(droit("channel-remap"), Droit::Free);
        // Q2 révisée : le crossfeed simple est gratuit, Crossfeed Pro payant.
        assert_eq!(droit("crossfeed"), Droit::Free);
        for id in [
            "equalizer-parametric",
            "convolution",
            "room-correction",
            "crossfeed-pro",
            "crosstalk",
            "declick-live",
        ] {
            assert_eq!(droit(id), Droit::Premium, "{id}");
        }
        // L'outil par lots garde son id ; le greffon temps réel en a un autre.
        assert!(c.get("declick").is_none());
        assert!(c.get("upsampler").is_none(), "étage fixe, pas un greffon");
        // Groupes de la maquette A3.
        let groupe = |id: &str| c.get(id).unwrap().category;
        assert_eq!(groupe("equalizer-headphone"), Categorie::Equalisation);
        assert_eq!(groupe("crosstalk"), Categorie::Spatial);
        assert_eq!(groupe("room-correction"), Categorie::RoomAndFilters);
        assert_eq!(groupe("declick-live"), Categorie::RestorationAndRouting);
        assert_eq!(groupe("channel-remap"), Categorie::RestorationAndRouting);
    }

    #[test]
    fn le_catalogue_fait_l_aller_retour_json() {
        let c = catalogue_de_reference();
        let texte = serde_json::to_string(&c).unwrap();
        let relu: Catalogue = serde_json::from_str(&texte).unwrap();
        assert_eq!(relu, c);
        let v = serde_json::to_value(&c.plugins[5]).unwrap();
        assert_eq!(
            v,
            json!({
                "id": "crossfeed",
                "name": "Crossfeed",
                "category": "spatial",
                "entitlement": "free",
                "origin": "reference",
                "settings_schema": { "type": "object" },
                "placement": {
                    "channels": "stereo_only",
                    "position": "free",
                    "exclusive_group": "crossfeed_crosstalk"
                }
            })
        );
    }

    #[test]
    fn un_descripteur_incoherent_est_refuse() {
        let mut d = catalogue_de_reference()
            .get("channel-remap")
            .unwrap()
            .clone();
        d.verifier().unwrap();
        d.placement.position = Position::Free;
        assert_eq!(
            d.verifier(),
            Err(ErreurCatalogue::ReaffectationPasEnDernier(
                "channel-remap".into()
            ))
        );
        let mut d = catalogue_de_reference().plugins[0].clone();
        for mauvais in ["", "Equalizer", "eq_graphic", "-eq", "eq/../x"] {
            d.id = mauvais.into();
            assert_eq!(
                d.verifier(),
                Err(ErreurCatalogue::IdInvalide(mauvais.into()))
            );
        }
        d.id = "ok".into();
        d.settings_schema = json!("object");
        assert_eq!(
            d.verifier(),
            Err(ErreurCatalogue::SchemaInvalide("ok".into()))
        );
    }

    #[test]
    fn un_catalogue_avec_un_id_en_double_est_refuse() {
        let mut c = catalogue_de_reference();
        let copie = c.plugins[3].clone();
        c.plugins.push(copie);
        assert_eq!(
            c.verifier(),
            Err(ErreurCatalogue::IdEnDouble("convolution".into()))
        );
    }

    #[test]
    fn les_reglages_sont_verifies_contre_le_schema() {
        let mut d = catalogue_de_reference().get("convolution").unwrap().clone();
        d.settings_schema = json!({
            "type": "object",
            "required": ["filter_set"],
            "properties": {
                "filter_set": { "type": "string" },
                "resample_nearest": { "type": "boolean" }
            },
            "additionalProperties": false
        });
        assert!(d.verifier_reglages(&Value::Null).is_ok());
        assert!(
            d.verifier_reglages(
                &json!({ "filter_set": "living-room.zip", "resample_nearest": true })
            )
            .is_ok()
        );
        // Contre-épreuves.
        assert!(d.verifier_reglages(&json!([1, 2])).is_err());
        assert!(
            d.verifier_reglages(&json!({ "resample_nearest": true }))
                .is_err()
        );
        assert!(
            d.verifier_reglages(&json!({ "filter_set": "a.wav", "gain": 3 }))
                .is_err()
        );
    }
}
