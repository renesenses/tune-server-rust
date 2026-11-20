//! La chaîne TESS d'une zone : greffons ordonnés et réglages des étages fixes.
//!
//! **Lecture et écriture n'ont pas la même rigueur** (compatibilité) :
//! - la désérialisation de [`ChaineTess`] est TOLÉRANTE (champs inconnus
//!   ignorés, valeurs inconnues lues `unknown`) : un client ou un serveur v1
//!   lit sans échouer une chaîne écrite par une version plus récente ; la
//!   résolution ([`ChaineTess::resoudre`]) éteint alors ce qu'elle ne comprend
//!   pas, avec un motif ;
//! - le corps d'un `PUT` passe par [`ChaineTess::depuis_corps`], STRICT : JSON
//!   illisible, champ ou valeur inconnus → 400 `invalid_body` ; puis
//!   [`ChaineTess::valider`] (409, ou 402 pour un droit manquant).
//!
//! Version : le serveur accepte toute `version` de 1 à [`VERSION_CHAINE`] et
//! la remonte ([`ChaineTess::mettre_a_niveau`]) ; une version plus récente que
//! la sienne est refusée explicitement (409 `unsupported_version`), jamais
//! tronquée en silence.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::greffon::{Catalogue, PolitiqueGroupe, Position};
use crate::audio::surechantillonnage::greffon::{DEFAULT_FILTER, OFFERED_FILTERS, OFFERED_RATIOS};

/// Version du format de la chaîne enregistrée (`zone_{id}_tess_chain`).
pub const VERSION_CHAINE: u32 = 1;

/// Headroom automatique dès qu'un greffon ou l'upsampler est actif (T07).
pub const HEADROOM_AUTO_DB: f64 = -3.0;
/// Bornes d'un headroom manuel, en dB.
pub const HEADROOM_MANUEL_MIN_DB: f64 = -24.0;
pub const HEADROOM_MANUEL_MAX_DB: f64 = 0.0;
/// Plafond de cadence de l'upsampler, comme `UpsamplerSettings`.
pub const CADENCE_MAX_HZ: u32 = 768_000;

/// Un greffon placé dans la chaîne.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaillonGreffon {
    pub id: String,
    /// La case à cocher de l'écran : un maillon décoché reste à sa place, ses
    /// réglages sont gardés, il ne traite rien.
    pub enabled: bool,
    /// Réglages propres au greffon (`null` = valeurs par défaut).
    #[serde(default)]
    pub settings: Value,
}

/// Étage fixe d'entrée. Il porte aussi le ReplayGain et le gain de zone, qui
/// restent réglés là où ils le sont aujourd'hui.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Headroom {
    /// [`HEADROOM_AUTO_DB`] dès qu'un greffon ou l'upsampler est EFFECTIF,
    /// 0 dB sinon (la chaîne vide reste bit-perfect).
    Auto {},
    /// Valeur fixe, dans [`HEADROOM_MANUEL_MIN_DB`, `HEADROOM_MANUEL_MAX_DB`].
    Manual { db: f64 },
    #[serde(other)]
    Unknown,
}

impl Default for Headroom {
    fn default() -> Self {
        Self::Auto {}
    }
}

/// Taux DSD (source ou sortie du modulateur).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TauxDsd {
    Dsd64,
    Dsd128,
    Dsd256,
    #[serde(other)]
    Unknown,
}

fn filtre_par_defaut() -> String {
    DEFAULT_FILTER.to_owned()
}

/// Étage fixe de suréchantillonnage (Premium), toujours après les greffons.
/// Les champs reprennent TOUS ceux d'`UpsamplerSettings` (#7614), pour que la
/// migration (T02) ne perde rien.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Upsampling {
    Off {},
    /// PCM, multiple entier de la fréquence source.
    Pcm {
        /// Un des filtres proposés (`OFFERED_FILTERS` du SDK).
        #[serde(default = "filtre_par_defaut")]
        filter: String,
        /// ×2, ×4, ×8 ou ×16 au plus ; `None` = le maximum du périphérique.
        #[serde(default)]
        max_ratio: Option<u32>,
        /// Plafond de la cadence de sortie en Hz ; `None` = le plus haut que
        /// le périphérique accepte.
        #[serde(default)]
        max_rate_hz: Option<u32>,
    },
    /// Vers DSD : le volume passe juste avant le modulateur (exception de Q9).
    Dsd {
        rate: TauxDsd,
        #[serde(default = "filtre_par_defaut")]
        filter: String,
    },
    #[serde(other)]
    Unknown,
}

impl Default for Upsampling {
    fn default() -> Self {
        Self::Off {}
    }
}

impl Upsampling {
    pub fn demande(&self) -> bool {
        !matches!(self, Self::Off {})
    }
}

/// Le dither de l'étape finale de quantification (PCM).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Dither {
    /// TPDF ±1 LSB, appliqué seulement s'il y a requantification.
    #[default]
    Tpdf,
    /// TPDF avec mise en forme du bruit.
    NoiseShaped,
    /// Aucun dither (sortie en entiers 32 bits, par exemple).
    Off,
    #[serde(other)]
    Unknown,
}

/// Étape finale de quantification, séparée du volume (Q9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Quantification {
    #[serde(default)]
    pub dither: Dither,
}

/// Les réglages des étages fixes. Le volume n'en fait pas partie : il reste un
/// réglage de zone ; seule sa PLACE est fixée par le contrat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct EtagesFixes {
    #[serde(default)]
    pub headroom: Headroom,
    #[serde(default)]
    pub upsampling: Upsampling,
    #[serde(default)]
    pub quantization: Quantification,
}

/// La chaîne TESS d'une zone, telle qu'elle est enregistrée et échangée.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChaineTess {
    pub version: u32,
    /// « Bypass all » : greffons, headroom et upsampling contournés ; restent
    /// le volume logiciel et la quantification qu'il exige.
    #[serde(default)]
    pub bypass_all: bool,
    /// Les greffons, dans l'ordre de traitement.
    #[serde(default)]
    pub plugins: Vec<MaillonGreffon>,
    #[serde(default)]
    pub fixed: EtagesFixes,
}

impl Default for ChaineTess {
    fn default() -> Self {
        Self {
            version: VERSION_CHAINE,
            bypass_all: false,
            plugins: Vec::new(),
            fixed: EtagesFixes::default(),
        }
    }
}

/// Ce que la validation d'un `PUT` doit savoir du compte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContexteValidation {
    /// Le compte a-t-il le Premium ? (Le lien fin avec les `Feature` de
    /// `license.rs`, via `DescripteurGreffon::feature`, est le travail de T19.)
    pub premium: bool,
}

/// Pourquoi un corps de `PUT` est refusé. Chaque variante a un code stable
/// ([`ErreurChaine::code`]) et un statut HTTP ([`ErreurChaine::statut_http`]).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ErreurChaine {
    #[error("corps illisible : {0}")]
    CorpsIllisible(String),
    #[error("version de chaîne {trouvee} non prise en charge (de 1 à {attendue})")]
    VersionNonPriseEnCharge { trouvee: u32, attendue: u32 },
    #[error("valeur inconnue de cette version : {0}")]
    ValeurInconnue(&'static str),
    #[error("greffon inconnu : {0}")]
    GreffonInconnu(String),
    #[error("greffon {0} présent deux fois")]
    GreffonEnDouble(String),
    #[error("réglages du greffon {id} refusés : {raison}")]
    ReglagesInvalides { id: String, raison: String },
    #[error("le greffon {0} doit être le dernier des greffons")]
    PasEnDernier(String),
    #[error("{premier} et {second} ne peuvent pas être actifs ensemble (groupe {groupe})")]
    Exclusifs {
        premier: String,
        second: String,
        groupe: String,
    },
    #[error("{0} demande le Premium")]
    PremiumRequis(String),
    #[error("headroom manuel hors bornes : {0} dB")]
    HeadroomHorsBornes(f64),
    #[error("réglage d'upsampling refusé : {0}")]
    UpsamplingInvalide(&'static str),
}

impl ErreurChaine {
    /// 400 corps illisible, 402 droit manquant, 409 chaîne invalide.
    pub fn statut_http(&self) -> u16 {
        match self {
            Self::CorpsIllisible(_) => 400,
            Self::PremiumRequis(_) => 402,
            Self::VersionNonPriseEnCharge { .. }
            | Self::ValeurInconnue(_)
            | Self::GreffonInconnu(_)
            | Self::GreffonEnDouble(_)
            | Self::ReglagesInvalides { .. }
            | Self::PasEnDernier(_)
            | Self::Exclusifs { .. }
            | Self::HeadroomHorsBornes(_)
            | Self::UpsamplingInvalide(_) => 409,
        }
    }

    /// Code stable du refus (terme du contrat, traduit par les clients).
    pub fn code(&self) -> &'static str {
        match self {
            Self::CorpsIllisible(_) => "invalid_body",
            Self::VersionNonPriseEnCharge { .. } => "unsupported_version",
            Self::ValeurInconnue(_) => "unknown_value",
            Self::GreffonInconnu(_) => "unknown_plugin",
            Self::GreffonEnDouble(_) => "duplicate_plugin",
            Self::ReglagesInvalides { .. } => "invalid_settings",
            Self::PasEnDernier(_) => "must_be_last_plugin",
            Self::Exclusifs { .. } => "mutually_exclusive",
            Self::PremiumRequis(_) => "premium_required",
            Self::HeadroomHorsBornes(_) => "headroom_out_of_range",
            Self::UpsamplingInvalide(_) => "invalid_upsampling",
        }
    }

    /// Le greffon en cause, s'il y en a un.
    pub fn greffon(&self) -> Option<&str> {
        match self {
            Self::GreffonInconnu(id)
            | Self::GreffonEnDouble(id)
            | Self::PasEnDernier(id)
            | Self::PremiumRequis(id) => Some(id),
            Self::ReglagesInvalides { id, .. } => Some(id),
            Self::Exclusifs { second, .. } => Some(second),
            Self::CorpsIllisible(_)
            | Self::VersionNonPriseEnCharge { .. }
            | Self::ValeurInconnue(_)
            | Self::HeadroomHorsBornes(_)
            | Self::UpsamplingInvalide(_) => None,
        }
    }

    /// Un exemple de chaque variante, pour les tests de couverture (schéma,
    /// traductions). [`Self::rang`] est un `match` exhaustif : une variante
    /// ajoutée sans exemple fait tomber le test qui compare les deux.
    pub fn exemples() -> Vec<Self> {
        vec![
            Self::CorpsIllisible("x".into()),
            Self::VersionNonPriseEnCharge {
                trouvee: 2,
                attendue: 1,
            },
            Self::ValeurInconnue("fixed.upsampling"),
            Self::GreffonInconnu("x".into()),
            Self::GreffonEnDouble("x".into()),
            Self::ReglagesInvalides {
                id: "x".into(),
                raison: "r".into(),
            },
            Self::PasEnDernier("x".into()),
            Self::Exclusifs {
                premier: "a".into(),
                second: "b".into(),
                groupe: "g".into(),
            },
            Self::PremiumRequis("x".into()),
            Self::HeadroomHorsBornes(3.0),
            Self::UpsamplingInvalide("filter"),
        ]
    }

    /// Rang de la variante, de 0 à [`Self::NOMBRE_DE_VARIANTES`] − 1.
    pub fn rang(&self) -> usize {
        match self {
            Self::CorpsIllisible(_) => 0,
            Self::VersionNonPriseEnCharge { .. } => 1,
            Self::ValeurInconnue(_) => 2,
            Self::GreffonInconnu(_) => 3,
            Self::GreffonEnDouble(_) => 4,
            Self::ReglagesInvalides { .. } => 5,
            Self::PasEnDernier(_) => 6,
            Self::Exclusifs { .. } => 7,
            Self::PremiumRequis(_) => 8,
            Self::HeadroomHorsBornes(_) => 9,
            Self::UpsamplingInvalide(_) => 10,
        }
    }

    pub const NOMBRE_DE_VARIANTES: usize = 11;
}

/// Le premier chemin présent dans `entree` et absent de `relu` : un champ que
/// la lecture tolérante a ignoré.
fn premier_champ_ignore(entree: &Value, relu: &Value, chemin: &str) -> Option<String> {
    match (entree, relu) {
        (Value::Object(a), Value::Object(b)) => a.iter().find_map(|(k, v)| {
            let sous = if chemin.is_empty() {
                k.clone()
            } else {
                format!("{chemin}.{k}")
            };
            match b.get(k) {
                None => Some(sous),
                Some(w) => premier_champ_ignore(v, w, &sous),
            }
        }),
        (Value::Array(a), Value::Array(b)) => a
            .iter()
            .zip(b)
            .enumerate()
            .find_map(|(i, (v, w))| premier_champ_ignore(v, w, &format!("{chemin}[{i}]"))),
        _ => None,
    }
}

impl ChaineTess {
    /// Lecture STRICTE du corps d'un `PUT` : JSON illisible, champ inconnu ou
    /// valeur inconnue → [`ErreurChaine::CorpsIllisible`] (400).
    pub fn depuis_corps(texte: &str) -> Result<Self, ErreurChaine> {
        let brut: Value =
            serde_json::from_str(texte).map_err(|e| ErreurChaine::CorpsIllisible(e.to_string()))?;
        let chaine: Self = serde_json::from_value(brut.clone())
            .map_err(|e| ErreurChaine::CorpsIllisible(e.to_string()))?;
        let relu = serde_json::to_value(&chaine)
            .map_err(|e| ErreurChaine::CorpsIllisible(e.to_string()))?;
        if let Some(champ) = premier_champ_ignore(&brut, &relu, "") {
            return Err(ErreurChaine::CorpsIllisible(format!(
                "unknown field {champ}"
            )));
        }
        if let Some(champ) = chaine.valeur_inconnue() {
            return Err(ErreurChaine::CorpsIllisible(format!(
                "unknown value in {champ}"
            )));
        }
        Ok(chaine)
    }

    /// Le premier réglage fixe lu `unknown`, s'il y en a un.
    pub fn valeur_inconnue(&self) -> Option<&'static str> {
        let f = &self.fixed;
        if matches!(f.headroom, Headroom::Unknown) {
            Some("fixed.headroom")
        } else if matches!(
            f.upsampling,
            Upsampling::Unknown
                | Upsampling::Dsd {
                    rate: TauxDsd::Unknown,
                    ..
                }
        ) {
            Some("fixed.upsampling")
        } else if matches!(f.quantization.dither, Dither::Unknown) {
            Some("fixed.quantization.dither")
        } else {
            None
        }
    }

    /// Remonte une chaîne d'une version antérieure acceptée au format courant.
    /// (La v1 est la première : rien à convertir aujourd'hui.)
    pub fn mettre_a_niveau(mut self) -> Self {
        self.version = VERSION_CHAINE;
        self
    }

    fn verifier_upsampling(&self) -> Result<(), ErreurChaine> {
        let filtre_ok = |f: &str| OFFERED_FILTERS.contains(&f);
        match &self.fixed.upsampling {
            Upsampling::Off {} => Ok(()),
            Upsampling::Unknown => Err(ErreurChaine::ValeurInconnue("fixed.upsampling")),
            Upsampling::Pcm {
                filter,
                max_ratio,
                max_rate_hz,
            } => {
                if !filtre_ok(filter) {
                    return Err(ErreurChaine::UpsamplingInvalide("filter"));
                }
                if max_ratio.is_some_and(|r| !OFFERED_RATIOS.contains(&r)) {
                    return Err(ErreurChaine::UpsamplingInvalide("max_ratio"));
                }
                if max_rate_hz.is_some_and(|r| !(1..=CADENCE_MAX_HZ).contains(&r)) {
                    return Err(ErreurChaine::UpsamplingInvalide("max_rate_hz"));
                }
                Ok(())
            }
            Upsampling::Dsd { rate, filter } => {
                if *rate == TauxDsd::Unknown {
                    return Err(ErreurChaine::ValeurInconnue("fixed.upsampling.rate"));
                }
                if !filtre_ok(filter) {
                    return Err(ErreurChaine::UpsamplingInvalide("filter"));
                }
                Ok(())
            }
        }
    }

    /// Valide une chaîne reçue par `PUT` contre le catalogue. La première faute
    /// trouvée est rendue. Ce qui dépend du flux (canaux, paquet désinstallé)
    /// n'est PAS refusé ici : la résolution l'éteint avec un motif.
    pub fn valider(
        &self,
        catalogue: &Catalogue,
        contexte: ContexteValidation,
    ) -> Result<(), ErreurChaine> {
        if !(1..=VERSION_CHAINE).contains(&self.version) {
            return Err(ErreurChaine::VersionNonPriseEnCharge {
                trouvee: self.version,
                attendue: VERSION_CHAINE,
            });
        }
        if let Some(champ) = self.valeur_inconnue() {
            return Err(ErreurChaine::ValeurInconnue(champ));
        }
        if let Headroom::Manual { db } = self.fixed.headroom
            && !(db.is_finite() && (HEADROOM_MANUEL_MIN_DB..=HEADROOM_MANUEL_MAX_DB).contains(&db))
        {
            return Err(ErreurChaine::HeadroomHorsBornes(db));
        }
        self.verifier_upsampling()?;

        let mut actifs_par_groupe: Vec<(String, &str)> = Vec::new();
        let dernier = self.plugins.len().saturating_sub(1);
        for (i, m) in self.plugins.iter().enumerate() {
            let d = catalogue
                .get(&m.id)
                .ok_or_else(|| ErreurChaine::GreffonInconnu(m.id.clone()))?;
            if self.plugins[..i].iter().any(|p| p.id == m.id) {
                return Err(ErreurChaine::GreffonEnDouble(m.id.clone()));
            }
            d.verifier_reglages(&m.settings)
                .map_err(|raison| ErreurChaine::ReglagesInvalides {
                    id: m.id.clone(),
                    raison,
                })?;
            if d.placement.position == Position::LastPlugin && i != dernier {
                return Err(ErreurChaine::PasEnDernier(m.id.clone()));
            }
            if !m.enabled || self.bypass_all {
                continue;
            }
            if d.entitlement.demande_le_premium() && !contexte.premium {
                return Err(ErreurChaine::PremiumRequis(m.id.clone()));
            }
            if let Some(g) = d.groupe_effectif()
                && catalogue.politique(&g.id) == PolitiqueGroupe::Refuse
            {
                if let Some((_, premier)) = actifs_par_groupe.iter().find(|(x, _)| *x == g.id) {
                    return Err(ErreurChaine::Exclusifs {
                        premier: (*premier).to_owned(),
                        second: m.id.clone(),
                        groupe: g.id,
                    });
                }
                actifs_par_groupe.push((g.id, m.id.as_str()));
            }
        }
        if self.fixed.upsampling.demande() && !self.bypass_all && !contexte.premium {
            return Err(ErreurChaine::PremiumRequis(ID_UPSAMPLING.into()));
        }
        Ok(())
    }
}

/// L'id réservé de l'étage fixe d'upsampling dans un refus 402.
pub const ID_UPSAMPLING: &str = "upsampling";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tess::greffon::{PolitiqueDeGroupe, catalogue_de_reference};
    use serde_json::json;

    fn maillon(id: &str, enabled: bool) -> MaillonGreffon {
        MaillonGreffon {
            id: id.into(),
            enabled,
            settings: Value::Null,
        }
    }

    fn chaine(plugins: Vec<MaillonGreffon>) -> ChaineTess {
        ChaineTess {
            plugins,
            ..ChaineTess::default()
        }
    }

    fn pcm(max_ratio: Option<u32>) -> Upsampling {
        Upsampling::Pcm {
            filter: DEFAULT_FILTER.into(),
            max_ratio,
            max_rate_hz: None,
        }
    }

    const PREMIUM: ContexteValidation = ContexteValidation { premium: true };
    const GRATUIT: ContexteValidation = ContexteValidation { premium: false };

    fn valider(c: &ChaineTess, ctx: ContexteValidation) -> Result<(), ErreurChaine> {
        c.valider(&catalogue_de_reference(), ctx)
    }

    #[test]
    fn la_chaine_fait_l_aller_retour_json() {
        let mut variantes = vec![ChaineTess::default()];
        let mut c = chaine(vec![
            MaillonGreffon {
                id: "convolution".into(),
                enabled: true,
                settings: json!({ "filter_set": "living-room.zip" }),
            },
            maillon("crossfeed", false),
        ]);
        c.bypass_all = true;
        for dither in [Dither::Tpdf, Dither::NoiseShaped, Dither::Off] {
            c.fixed.quantization.dither = dither;
            variantes.push(c.clone());
        }
        c.fixed.headroom = Headroom::Manual { db: -6.5 };
        for up in [
            pcm(None),
            Upsampling::Pcm {
                filter: "precision-pm".into(),
                max_ratio: Some(8),
                max_rate_hz: Some(384_000),
            },
            Upsampling::Dsd {
                rate: TauxDsd::Dsd128,
                filter: "resolution".into(),
            },
        ] {
            c.fixed.upsampling = up;
            variantes.push(c.clone());
        }
        for v in variantes {
            let texte = serde_json::to_string(&v).unwrap();
            assert_eq!(ChaineTess::depuis_corps(&texte).unwrap(), v, "{texte}");
        }
    }

    #[test]
    fn le_format_enregistre_est_fige() {
        let texte = r#"{
            "version": 1,
            "bypass_all": false,
            "plugins": [
                {"id": "convolution", "enabled": true, "settings": {"filter_set": "living-room.zip"}},
                {"id": "crossfeed", "enabled": false, "settings": null}
            ],
            "fixed": {
                "headroom": {"mode": "manual", "db": -3.0},
                "upsampling": {"mode": "pcm", "filter": "polyvalent", "max_ratio": 4, "max_rate_hz": null},
                "quantization": {"dither": "tpdf"}
            }
        }"#;
        let c = ChaineTess::depuis_corps(texte).unwrap();
        assert_eq!(c.fixed.upsampling, pcm(Some(4)));
        let relu: Value = serde_json::from_str(texte).unwrap();
        assert_eq!(serde_json::to_value(&c).unwrap(), relu);
        // Champs facultatifs absents : valeurs par défaut ; « maximum du
        // périphérique » quand max_ratio manque.
        assert_eq!(
            ChaineTess::depuis_corps(r#"{"version":1}"#).unwrap(),
            ChaineTess::default()
        );
        let c = ChaineTess::depuis_corps(r#"{"version":1,"fixed":{"upsampling":{"mode":"pcm"}}}"#)
            .unwrap();
        assert_eq!(c.fixed.upsampling, pcm(None));
    }

    #[test]
    fn un_corps_strict_refuse_l_inconnu_en_400() {
        for texte in [
            "{not json",
            r#"{"plugins":[]}"#,
            r#"{"version":1,"volume":-18}"#,
            r#"{"version":1,"plugins":[{"id":"crossfeed","enabled":true,"order":2}]}"#,
            r#"{"version":1,"fixed":{"headroom":{"mode":"auto","db":-3}}}"#,
            r#"{"version":1,"fixed":{"headroom":{"mode":"limiter"}}}"#,
            r#"{"version":1,"fixed":{"upsampling":{"mode":"pcm","factor":3}}}"#,
            r#"{"version":1,"fixed":{"upsampling":{"mode":"off","filter":"x"}}}"#,
            r#"{"version":1,"fixed":{"upsampling":{"mode":"dsd","rate":"dsd512"}}}"#,
            r#"{"version":1,"fixed":{"quantization":{"dither":"rectangular"}}}"#,
        ] {
            let e = ChaineTess::depuis_corps(texte).unwrap_err();
            assert_eq!(e.code(), "invalid_body", "{texte}");
            assert_eq!(e.statut_http(), 400);
        }
    }

    #[test]
    fn une_lecture_tolerante_garde_ce_qu_elle_comprend() {
        // Chaîne écrite par une version plus récente : lue, valeurs inconnues
        // marquées, champs inconnus ignorés.
        let texte = r#"{"version":2,"limiter":{"ceiling":-1},
            "plugins":[{"id":"convolution","enabled":true,"settings":null,"order":1}],
            "fixed":{"upsampling":{"mode":"dsd","rate":"dsd512"},"quantization":{"dither":"rectangular"}}}"#;
        let c: ChaineTess = serde_json::from_str(texte).unwrap();
        assert_eq!(c.version, 2);
        assert_eq!(c.plugins[0].id, "convolution");
        assert_eq!(c.valeur_inconnue(), Some("fixed.upsampling"));
        assert_eq!(c.fixed.quantization.dither, Dither::Unknown);
        // Et la validation d'un PUT la refuse explicitement.
        assert_eq!(
            valider(&c, PREMIUM),
            Err(ErreurChaine::VersionNonPriseEnCharge {
                trouvee: 2,
                attendue: VERSION_CHAINE
            })
        );
    }

    #[test]
    fn les_versions_acceptees_vont_de_1_a_la_courante() {
        let mut c = ChaineTess::default();
        valider(&c, GRATUIT).unwrap();
        assert_eq!(c.clone().mettre_a_niveau().version, VERSION_CHAINE);
        for v in [0, VERSION_CHAINE + 1] {
            c.version = v;
            assert_eq!(
                valider(&c, GRATUIT).unwrap_err().code(),
                "unsupported_version"
            );
        }
    }

    #[test]
    fn l_upsampling_porte_tous_les_reglages_actuels() {
        // Épreuve : chaque filtre et chaque rapport proposés par le SDK passent.
        for f in OFFERED_FILTERS {
            for r in OFFERED_RATIOS.iter().copied().map(Some).chain([None]) {
                let mut c = ChaineTess::default();
                c.fixed.upsampling = Upsampling::Pcm {
                    filter: f.into(),
                    max_ratio: r,
                    max_rate_hz: Some(CADENCE_MAX_HZ),
                };
                valider(&c, PREMIUM).unwrap();
            }
        }
        // Contre-épreuves : rapports non puissances de 2, filtre inconnu,
        // plafond hors bornes.
        for (up, champ) in [
            (pcm(Some(3)), "max_ratio"),
            (pcm(Some(32)), "max_ratio"),
            (pcm(Some(1)), "max_ratio"),
            (
                Upsampling::Pcm {
                    filter: "brickwall".into(),
                    max_ratio: None,
                    max_rate_hz: None,
                },
                "filter",
            ),
            (
                Upsampling::Pcm {
                    filter: DEFAULT_FILTER.into(),
                    max_ratio: None,
                    max_rate_hz: Some(CADENCE_MAX_HZ + 1),
                },
                "max_rate_hz",
            ),
            (
                Upsampling::Dsd {
                    rate: TauxDsd::Dsd64,
                    filter: "x".into(),
                },
                "filter",
            ),
        ] {
            let mut c = ChaineTess::default();
            c.fixed.upsampling = up;
            assert_eq!(
                valider(&c, PREMIUM),
                Err(ErreurChaine::UpsamplingInvalide(champ))
            );
        }
    }

    #[test]
    fn le_groupe_du_crossfeed_refuse_deux_membres_coches() {
        // Décision B (18/11) : deux membres cochés = 409 `mutually_exclusive`.
        let c = chaine(vec![
            maillon("crossfeed", true),
            maillon("crossfeed-pro", true),
            maillon("crosstalk", true),
        ]);
        let e = valider(&c, PREMIUM).unwrap_err();
        assert_eq!(
            e,
            ErreurChaine::Exclusifs {
                premier: "crossfeed".into(),
                second: "crossfeed-pro".into(),
                groupe: "crossfeed_crosstalk".into()
            }
        );
        assert_eq!(e.statut_http(), 409);
        assert_eq!(e.code(), "mutually_exclusive");
        // Contre-épreuve : un seul membre coché, les autres décochés, passe.
        let un_seul = chaine(vec![
            maillon("crossfeed", false),
            maillon("crossfeed-pro", false),
            maillon("crosstalk", true),
        ]);
        valider(&un_seul, PREMIUM).unwrap();
        // Et la politique « priority » reste disponible pour un groupe qui la
        // déclare.
        let mut cat = catalogue_de_reference();
        cat.groups = vec![PolitiqueDeGroupe {
            id: "crossfeed_crosstalk".into(),
            policy: PolitiqueGroupe::Priority,
        }];
        c.valider(&cat, PREMIUM).unwrap();
    }

    #[test]
    fn la_reaffectation_des_canaux_est_en_dernier() {
        let ok = chaine(vec![
            maillon("equalizer-graphic", true),
            maillon("channel-remap", true),
        ]);
        valider(&ok, PREMIUM).unwrap();
        for actif in [true, false] {
            let ko = chaine(vec![
                maillon("channel-remap", actif),
                maillon("equalizer-graphic", true),
            ]);
            assert_eq!(
                valider(&ko, PREMIUM),
                Err(ErreurChaine::PasEnDernier("channel-remap".into()))
            );
        }
    }

    #[test]
    fn un_greffon_premium_actif_demande_le_premium() {
        valider(
            &chaine(vec![
                maillon("equalizer-graphic", true),
                maillon("crossfeed", true),
                maillon("channel-remap", true),
            ]),
            GRATUIT,
        )
        .unwrap();
        // Décoché : gardé sans droit (P2).
        valider(
            &chaine(vec![maillon("equalizer-parametric", false)]),
            GRATUIT,
        )
        .unwrap();
        // Bypass all : rien ne traite, rien n'est refusé.
        let mut c = chaine(vec![maillon("equalizer-parametric", true)]);
        c.fixed.upsampling = pcm(None);
        c.bypass_all = true;
        valider(&c, GRATUIT).unwrap();
        // Contre-épreuves : 402.
        c.bypass_all = false;
        let e = valider(&c, GRATUIT).unwrap_err();
        assert_eq!(
            e,
            ErreurChaine::PremiumRequis("equalizer-parametric".into())
        );
        assert_eq!(e.statut_http(), 402);
        c.plugins.clear();
        assert_eq!(
            valider(&c, GRATUIT),
            Err(ErreurChaine::PremiumRequis("upsampling".into()))
        );
        valider(&c, PREMIUM).unwrap();
    }

    #[test]
    fn les_chaines_invalides_sont_refusees_en_409() {
        let manuel = |db| ChaineTess {
            fixed: EtagesFixes {
                headroom: Headroom::Manual { db },
                ..EtagesFixes::default()
            },
            ..ChaineTess::default()
        };
        let cas: Vec<(ChaineTess, &str)> = vec![
            (
                chaine(vec![maillon("hqplayer-dsp", true)]),
                "unknown_plugin",
            ),
            (
                chaine(vec![
                    maillon("convolution", true),
                    maillon("convolution", false),
                ]),
                "duplicate_plugin",
            ),
            (
                chaine(vec![MaillonGreffon {
                    id: "crossfeed".into(),
                    enabled: true,
                    settings: json!(0.5),
                }]),
                "invalid_settings",
            ),
            (manuel(6.0), "headroom_out_of_range"),
            (manuel(f64::NAN), "headroom_out_of_range"),
            (
                ChaineTess {
                    fixed: EtagesFixes {
                        upsampling: Upsampling::Unknown,
                        ..EtagesFixes::default()
                    },
                    ..ChaineTess::default()
                },
                "unknown_value",
            ),
        ];
        for (c, code) in cas {
            let e = valider(&c, PREMIUM).unwrap_err();
            assert_eq!(e.code(), code, "{e}");
            assert_eq!(e.statut_http(), 409);
        }
        valider(&manuel(-24.0), PREMIUM).unwrap();
    }

    #[test]
    fn chaque_variante_d_erreur_a_un_exemple() {
        let mut rangs: Vec<usize> = ErreurChaine::exemples()
            .iter()
            .map(ErreurChaine::rang)
            .collect();
        rangs.sort_unstable();
        assert_eq!(
            rangs,
            (0..ErreurChaine::NOMBRE_DE_VARIANTES).collect::<Vec<_>>()
        );
        let mut codes: Vec<_> = ErreurChaine::exemples()
            .iter()
            .map(ErreurChaine::code)
            .collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(
            codes.len(),
            ErreurChaine::NOMBRE_DE_VARIANTES,
            "codes distincts"
        );
    }
}
