//! Le descripteur d'un greffon TESS et le catalogue de référence.
//!
//! Un greffon TESS est un traitement DSP du SDK natif : référence compilée dans
//! le serveur ou paquet `.tuneplugin` signé (étude, § A4). Pas de troisième
//! système de greffons.
//!
//! Pour les greffons des dépôts PRIVÉS (Crossfeed Pro, Crosstalk), le public ne
//! connaît que l'id, le droit et les contraintes de place : leur schéma de
//! réglages vient du paquet signé ([`Origine::Package`]).
//!
//! Compatibilité : ces types sont des types de RÉPONSE. Ils tolèrent les champs
//! inconnus, et chaque énumération a une valeur de repli `unknown` : un client
//! v1 lit un catalogue plus récent sans échouer.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Le droit d'accès à un greffon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Droit {
    Free,
    Premium,
    /// Valeur inconnue de cette version : traitée comme Premium (prudence).
    #[serde(other)]
    Unknown,
}

impl Droit {
    pub fn demande_le_premium(self) -> bool {
        !matches!(self, Self::Free)
    }
}

/// D'où vient le traitement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origine {
    /// Référence du SDK compilée dans le serveur (`sdk/tune-plugin-*`).
    Reference,
    /// Paquet `.tuneplugin` signé (minisign), installé depuis le catalogue.
    Package,
    #[serde(other)]
    Unknown,
}

/// Le groupe d'un greffon dans la fenêtre « Add plugin » (maquette A3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Categorie {
    Equalisation,
    Spatial,
    RoomAndFilters,
    RestorationAndRouting,
    #[serde(other)]
    Unknown,
}

/// Ce qu'un greffon fait du nombre de canaux.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Canaux {
    /// Travaille sur n'importe quel nombre de canaux, sans le changer.
    Any,
    /// Stéréo seulement : éteint (avec motif) si le flux qui lui arrive n'a
    /// pas 2 canaux. Un greffon n'est « stéréo seulement » que s'il le DÉCLARE.
    StereoOnly,
    /// Change le nombre de canaux (réaffectation). Impose
    /// [`Position::LastPlugin`] : aucun greffon ne voit un flux réaffecté.
    Remaps,
    #[serde(other)]
    Unknown,
}

/// Où un greffon peut se placer parmi les greffons de la chaîne.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Position {
    /// N'importe où (ordre libre, glisser-déposer).
    Free,
    /// Dernier des greffons, juste avant les étages fixes de sortie.
    LastPlugin,
    #[serde(other)]
    Unknown,
}

/// Que faire quand plusieurs greffons actifs d'un même groupe exclusif
/// coexistent dans une chaîne.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PolitiqueGroupe {
    /// Comportement actuel (#7616) : le plus prioritaire traite, les autres
    /// sont écartés avec un motif dans le chemin du signal. Jamais de refus.
    #[default]
    Priority,
    /// Refus de la chaîne en 409 (`mutually_exclusive`). Réservé à une
    /// décision ultérieure de Bertrand.
    Refuse,
    #[serde(other)]
    Unknown,
}

/// Appartenance à un groupe exclusif.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupeExclusif {
    pub id: String,
    /// Plus grand = l'emporte. À égalité, le premier dans la chaîne.
    pub priority: u8,
}

/// Les contraintes de place d'un greffon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContraintesPlace {
    pub channels: Canaux,
    pub position: Position,
    pub exclusive_group: Option<GroupeExclusif>,
}

/// Le descripteur public d'un greffon TESS.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DescripteurGreffon {
    /// Identifiant stable, minuscules ASCII, chiffres et tirets.
    pub id: String,
    /// Nom d'affichage (anglais, traduit par les clients).
    pub name: String,
    /// Groupe d'affichage du catalogue (maquette A3).
    pub category: Categorie,
    pub entitlement: Droit,
    /// Le code stable de la `Feature` de `license.rs` qui ouvre ce greffon
    /// (`Feature::code`), quand il y en a une. `None` : le droit Premium
    /// général. Le lien fin droit ↔ Feature est le travail de T19.
    #[serde(default)]
    pub feature: Option<String>,
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
    #[error("le greffon {0} porte une valeur inconnue de cette version")]
    ValeurInconnue(String),
}

/// Règle commune des identifiants (greffons, préréglages) : minuscules ASCII,
/// chiffres et tirets, ni tiret en tête ni en queue, 64 caractères au plus.
pub fn id_valide(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('-')
        && !id.ends_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Préfixe de la famille du crossfeed, reconnu tel qu'aujourd'hui
/// (`crossfeed.rs`, `est_de_la_famille_du_crossfeed`) pour les paquets déjà
/// publiés dont le manifeste ne déclare pas encore de groupe (T05 l'ajoute).
pub const PREFIXE_FAMILLE_CROSSFEED: &str = "crossfeed-";

/// Groupe exclusif de la famille « croisement des voies » (#7616).
pub const GROUPE_CROSSFEED_CROSSTALK: &str = "crossfeed_crosstalk";

/// Priorités dans le groupe, celles du code actuel : crosstalk l'emporte sur
/// toute la famille ; un crossfeed tiers (`crossfeed-*`) remplace le crossfeed
/// intégré.
pub const PRIORITE_CROSSTALK: u8 = 3;
pub const PRIORITE_CROSSFEED_TIERS: u8 = 2;
pub const PRIORITE_CROSSFEED_INTEGRE: u8 = 1;

impl DescripteurGreffon {
    /// Cohérence interne du descripteur (côté serveur, à l'enregistrement).
    pub fn verifier(&self) -> Result<(), ErreurCatalogue> {
        if !id_valide(&self.id) {
            return Err(ErreurCatalogue::IdInvalide(self.id.clone()));
        }
        let inconnue = matches!(self.entitlement, Droit::Unknown)
            || matches!(self.origin, Origine::Unknown)
            || matches!(self.category, Categorie::Unknown)
            || matches!(self.placement.channels, Canaux::Unknown)
            || matches!(self.placement.position, Position::Unknown);
        if inconnue {
            return Err(ErreurCatalogue::ValeurInconnue(self.id.clone()));
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

    /// Le groupe exclusif effectif : celui que le descripteur déclare, sinon
    /// la famille du crossfeed reconnue par le préfixe `crossfeed-`.
    pub fn groupe_effectif(&self) -> Option<GroupeExclusif> {
        if let Some(g) = &self.placement.exclusive_group {
            return Some(g.clone());
        }
        let famille = self.id.len() > PREFIXE_FAMILLE_CROSSFEED.len()
            && self.id.starts_with(PREFIXE_FAMILLE_CROSSFEED);
        famille.then(|| GroupeExclusif {
            id: GROUPE_CROSSFEED_CROSSTALK.into(),
            priority: PRIORITE_CROSSFEED_TIERS,
        })
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

/// Un groupe exclusif et sa politique.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolitiqueDeGroupe {
    pub id: String,
    pub policy: PolitiqueGroupe,
}

/// Le catalogue des greffons TESS connus de l'hôte.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Catalogue {
    pub plugins: Vec<DescripteurGreffon>,
    /// Politique de chaque groupe exclusif ; un groupe absent suit
    /// [`PolitiqueGroupe::Priority`].
    #[serde(default)]
    pub groups: Vec<PolitiqueDeGroupe>,
}

impl Catalogue {
    pub fn get(&self, id: &str) -> Option<&DescripteurGreffon> {
        self.plugins.iter().find(|d| d.id == id)
    }

    pub fn politique(&self, groupe: &str) -> PolitiqueGroupe {
        self.groups
            .iter()
            .find(|g| g.id == groupe)
            .map(|g| g.policy)
            .unwrap_or_default()
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

struct Ligne {
    id: &'static str,
    name: &'static str,
    category: Categorie,
    entitlement: Droit,
    feature: Option<&'static str>,
    origin: Origine,
    channels: Canaux,
    position: Position,
    group_priority: Option<u8>,
}

impl Ligne {
    fn descripteur(&self) -> DescripteurGreffon {
        DescripteurGreffon {
            id: self.id.into(),
            name: self.name.into(),
            category: self.category,
            entitlement: self.entitlement,
            feature: self.feature.map(str::to_owned),
            origin: self.origin,
            // Schéma minimal : la caisse du SDK ou le paquet le complète au
            // branchement (lot T08).
            settings_schema: json!({ "type": "object" }),
            placement: ContraintesPlace {
                channels: self.channels,
                position: self.position,
                exclusive_group: self.group_priority.map(|priority| GroupeExclusif {
                    id: GROUPE_CROSSFEED_CROSSTALK.into(),
                    priority,
                }),
            },
        }
    }
}

/// Les onze greffons TESS de la v1.0 (dix au plan, plus `mono-downmix`,
/// choix provisoire de T02 en attente de la décision D2) et leurs droits (décisions Q2 révisée,
/// Q3, P1).
///
/// - Graphic EQ, Headphone EQ, Crossfeed, Mono downmix (T02), Channel
///   remapping : Free ;
/// - Parametric EQ, Convolution, Room correction, Crossfeed Pro, Crosstalk,
///   Declick temps réel : Premium.
///
/// Le suréchantillonneur n'est PAS un greffon : c'est un étage fixe
/// ([`crate::tess::Upsampling`]). L'outil de fichiers `declick` reste dans
/// Studio : le greffon temps réel a un id distinct, `declick-live`.
pub fn catalogue_de_reference() -> Catalogue {
    use Canaux::{Any, Remaps, StereoOnly};
    use Categorie::*;
    use Origine::{Package, Reference};
    let (gratuit, payant) = (Droit::Free, Droit::Premium);
    let libre = Position::Free;
    let lignes = [
        Ligne {
            id: "equalizer-graphic",
            name: "Graphic EQ",
            category: Equalisation,
            entitlement: gratuit,
            feature: None,
            origin: Reference,
            channels: Any,
            position: libre,
            group_priority: None,
        },
        Ligne {
            id: "equalizer-parametric",
            name: "Parametric EQ",
            category: Equalisation,
            entitlement: payant,
            feature: None,
            origin: Reference,
            channels: Any,
            position: libre,
            group_priority: None,
        },
        Ligne {
            id: "equalizer-headphone",
            name: "Headphone EQ",
            category: Equalisation,
            entitlement: gratuit,
            feature: None,
            origin: Reference,
            channels: Any,
            position: libre,
            group_priority: None,
        },
        Ligne {
            id: "crossfeed",
            name: "Crossfeed",
            category: Spatial,
            entitlement: gratuit,
            feature: None,
            origin: Reference,
            channels: StereoOnly,
            position: libre,
            group_priority: Some(PRIORITE_CROSSFEED_INTEGRE),
        },
        Ligne {
            id: "crossfeed-pro",
            name: "Crossfeed Pro",
            category: Spatial,
            entitlement: payant,
            feature: None,
            origin: Package,
            channels: StereoOnly,
            position: libre,
            group_priority: Some(PRIORITE_CROSSFEED_TIERS),
        },
        Ligne {
            id: "crosstalk",
            name: "Crosstalk",
            category: Spatial,
            entitlement: payant,
            feature: None,
            origin: Package,
            channels: StereoOnly,
            position: libre,
            group_priority: Some(PRIORITE_CROSSTALK),
        },
        Ligne {
            id: "convolution",
            name: "Convolution",
            category: RoomAndFilters,
            entitlement: payant,
            feature: Some("room_correction"),
            origin: Reference,
            channels: Any,
            position: libre,
            group_priority: None,
        },
        Ligne {
            id: "room-correction",
            name: "Room correction",
            category: RoomAndFilters,
            entitlement: payant,
            feature: Some("room_correction"),
            origin: Reference,
            channels: Any,
            position: libre,
            group_priority: None,
        },
        Ligne {
            id: "declick-live",
            name: "Declick",
            category: RestorationAndRouting,
            entitlement: payant,
            feature: None,
            origin: Reference,
            channels: Any,
            position: libre,
            group_priority: None,
        },
        Ligne {
            id: "mono-downmix",
            name: "Mono downmix",
            category: RestorationAndRouting,
            entitlement: gratuit,
            feature: None,
            origin: Reference,
            channels: StereoOnly,
            position: libre,
            group_priority: None,
        },
        Ligne {
            id: "channel-remap",
            name: "Channel remapping",
            category: RestorationAndRouting,
            entitlement: gratuit,
            feature: None,
            origin: Reference,
            channels: Remaps,
            position: Position::LastPlugin,
            group_priority: None,
        },
    ];
    Catalogue {
        plugins: lignes.iter().map(Ligne::descripteur).collect(),
        groups: vec![PolitiqueDeGroupe {
            id: GROUPE_CROSSFEED_CROSSTALK.into(),
            policy: PolitiqueGroupe::Priority,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::crossfeed::{ID_CROSSTALK, est_de_la_famille_du_crossfeed};
    use crate::license::Feature;

    #[test]
    fn le_catalogue_de_reference_suit_les_decisions() {
        let c = catalogue_de_reference();
        assert_eq!(c.plugins.len(), 11);
        c.verifier().unwrap();
        let droit = |id: &str| c.get(id).unwrap().entitlement;
        for id in [
            "equalizer-graphic",
            "equalizer-headphone",
            "crossfeed",
            "mono-downmix",
            "channel-remap",
        ] {
            assert_eq!(droit(id), Droit::Free, "{id}");
        }
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
        assert!(c.get("declick").is_none(), "l'outil par lots garde son id");
        assert!(c.get("upsampler").is_none(), "étage fixe, pas un greffon");
        let groupe = |id: &str| c.get(id).unwrap().category;
        assert_eq!(groupe("equalizer-headphone"), Categorie::Equalisation);
        assert_eq!(groupe("crosstalk"), Categorie::Spatial);
        assert_eq!(groupe("room-correction"), Categorie::RoomAndFilters);
        assert_eq!(groupe("declick-live"), Categorie::RestorationAndRouting);
    }

    #[test]
    fn les_features_citees_existent_dans_license_rs() {
        // Contre-épreuve sur le code réel : chaque `feature` du catalogue est
        // le code d'une `Feature` Premium existante.
        let codes: Vec<&str> = Feature::all_premium().iter().map(Feature::code).collect();
        for d in catalogue_de_reference().plugins {
            if let Some(f) = &d.feature {
                assert!(codes.contains(&f.as_str()), "{} : {f}", d.id);
                assert_eq!(d.entitlement, Droit::Premium, "{}", d.id);
            }
        }
        assert!(!codes.contains(&"no_such_feature"));
    }

    #[test]
    fn le_groupe_du_crossfeed_suit_le_code_actuel() {
        let c = catalogue_de_reference();
        // Mêmes appartenances que `est_de_la_famille_du_crossfeed` + crosstalk.
        for d in &c.plugins {
            let dans_le_groupe = d.groupe_effectif().is_some();
            let selon_le_code = d.id == "crossfeed"
                || d.id == ID_CROSSTALK
                || est_de_la_famille_du_crossfeed(&d.id);
            assert_eq!(dans_le_groupe, selon_le_code, "{}", d.id);
        }
        // Un paquet tiers `crossfeed-xyz` sans groupe déclaré est reconnu par
        // le préfixe, comme aujourd'hui.
        let mut tiers = c.get("equalizer-graphic").unwrap().clone();
        tiers.id = "crossfeed-xyz".into();
        assert_eq!(
            tiers.groupe_effectif(),
            Some(GroupeExclusif {
                id: GROUPE_CROSSFEED_CROSSTALK.into(),
                priority: PRIORITE_CROSSFEED_TIERS
            })
        );
        // Contre-épreuve : le préfixe seul (`crossfeed-`) n'est pas un membre.
        tiers.id = "crossfeed-".into();
        assert_eq!(tiers.groupe_effectif(), None);
        let prio = |id: &str| c.get(id).unwrap().groupe_effectif().unwrap().priority;
        assert!(prio("crosstalk") > prio("crossfeed-pro"));
        assert!(prio("crossfeed-pro") > prio("crossfeed"));
        assert_eq!(
            c.politique(GROUPE_CROSSFEED_CROSSTALK),
            PolitiqueGroupe::Priority
        );
        assert_eq!(c.politique("autre"), PolitiqueGroupe::Priority);
    }

    #[test]
    fn le_catalogue_fait_l_aller_retour_json() {
        let c = catalogue_de_reference();
        let texte = serde_json::to_string(&c).unwrap();
        let relu: Catalogue = serde_json::from_str(&texte).unwrap();
        assert_eq!(relu, c);
        let v = serde_json::to_value(c.get("crossfeed").unwrap()).unwrap();
        assert_eq!(
            v,
            json!({
                "id": "crossfeed",
                "name": "Crossfeed",
                "category": "spatial",
                "entitlement": "free",
                "feature": null,
                "origin": "reference",
                "settings_schema": { "type": "object" },
                "placement": {
                    "channels": "stereo_only",
                    "position": "free",
                    "exclusive_group": { "id": "crossfeed_crosstalk", "priority": 1 }
                }
            })
        );
    }

    #[test]
    fn un_client_v1_lit_un_descripteur_plus_recent() {
        let mut v =
            serde_json::to_value(catalogue_de_reference().get("crosstalk").unwrap()).unwrap();
        v["category"] = json!("dynamics");
        v["entitlement"] = json!("studio");
        v["placement"]["channels"] = json!("surround_only");
        v["latency_frames"] = json!(128);
        let d: DescripteurGreffon = serde_json::from_value(v).unwrap();
        assert_eq!(d.category, Categorie::Unknown);
        assert_eq!(d.entitlement, Droit::Unknown);
        assert!(d.entitlement.demande_le_premium(), "inconnu = prudence");
        assert_eq!(d.placement.channels, Canaux::Unknown);
        // Le serveur, lui, ne publie jamais de valeur inconnue.
        assert_eq!(
            d.verifier(),
            Err(ErreurCatalogue::ValeurInconnue("crosstalk".into()))
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
        for mauvais in ["", "Equalizer", "eq_graphic", "-eq", "eq-", "eq/../x"] {
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
        let copie = c.plugins[6].clone();
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
