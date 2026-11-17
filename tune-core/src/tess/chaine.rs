//! La chaîne TESS d'une zone : greffons ordonnés, étages fixes, ordre réel.
//!
//! Ordre réel des étapes (décision Q9) :
//!
//! ```text
//! Source → Headroom (fixe, entrée) → greffons, dans l'ordre de la liste
//!        → Upsampling (fixe, s'il est actif) → Volume (fixe, en flottant)
//!        → Quantization (fixe, étape finale séparée : dither TPDF ou mise en forme du bruit)
//!        → Output
//! ```
//!
//! Exception de Q9, upsampling vers DSD : le volume est appliqué juste avant le
//! modulateur sigma-delta, qui tient lieu d'étape finale de quantification
//! ([`CodeEtape::DsdModulator`]). La réponse du volume y est plus lente
//! ([`PlacementVolume::BeforeDsdModulator`]) ; partout ailleurs il est appliqué
//! au rendu, réactif en moins de 50 ms (#7264).
//!
//! L'écran TESS montre CET ordre (Q9). La maquette A1 dessine le volume avant
//! l'upsampling : c'est l'ordre réel ci-dessus qui fait foi.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::greffon::{Canaux, Catalogue, Droit, Position};

/// Version du format de la chaîne enregistrée (`zone_{id}_tess_chain`).
pub const VERSION_CHAINE: u32 = 1;

/// Headroom automatique dès qu'un greffon ou l'upsampler est actif (T07).
pub const HEADROOM_AUTO_DB: f64 = -3.0;
/// Bornes d'un headroom manuel, en dB.
pub const HEADROOM_MANUEL_MIN_DB: f64 = -24.0;
pub const HEADROOM_MANUEL_MAX_DB: f64 = 0.0;
/// Bornes d'un facteur d'upsampling PCM (multiple entier de la fréquence source).
pub const FACTEUR_UPSAMPLING_MIN: u32 = 2;
pub const FACTEUR_UPSAMPLING_MAX: u32 = 16;

/// Un greffon placé dans la chaîne.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Headroom {
    /// [`HEADROOM_AUTO_DB`] dès qu'un greffon ou l'upsampler est actif, 0 dB
    /// sinon (la chaîne vide reste bit-perfect).
    Auto {},
    /// Valeur fixe, dans [`HEADROOM_MANUEL_MIN_DB`, `HEADROOM_MANUEL_MAX_DB`].
    Manual { db: f64 },
}

impl Default for Headroom {
    fn default() -> Self {
        Self::Auto {}
    }
}

/// Taux DSD de sortie du modulateur.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TauxDsd {
    Dsd64,
    Dsd128,
    Dsd256,
}

/// Étage fixe de suréchantillonnage (Premium), toujours après les greffons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Upsampling {
    Off {},
    /// PCM, multiple entier de la fréquence source.
    Pcm {
        factor: u32,
    },
    /// Vers DSD : le volume passe juste avant le modulateur (exception de Q9).
    Dsd {
        rate: TauxDsd,
    },
}

impl Default for Upsampling {
    fn default() -> Self {
        Self::Off {}
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
}

/// Étape finale de quantification, séparée du volume (Q9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Quantification {
    pub dither: Dither,
}

/// Les réglages des étages fixes. Le volume n'en fait pas partie : il reste un
/// réglage de zone ; seule sa PLACE est fixée par le contrat.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct ChaineTess {
    pub version: u32,
    /// « Bypass all » : greffons, headroom et upsampling contournés ; restent
    /// le volume et la quantification. Volume à 0 dB ⇒ bit-perfect.
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

/// Code stable d'une étape du chemin du signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeEtape {
    Source,
    Headroom,
    Plugin,
    Upsampling,
    Volume,
    Quantization,
    DsdModulator,
    Output,
}

/// Une étape de l'ordre réel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Etape {
    pub code: CodeEtape,
    /// L'id du greffon pour [`CodeEtape::Plugin`], `null` sinon.
    pub plugin_id: Option<String>,
    /// Étape fixe (non déplaçable) : « FIXED » à l'écran.
    pub fixed: bool,
    /// Traite-t-elle le signal dans cette configuration ?
    pub active: bool,
}

/// Où le volume est appliqué.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementVolume {
    /// En flottant au rendu, après l'anneau : réactif en moins de 50 ms (#7264).
    AtRender,
    /// Juste avant le modulateur DSD : réponse plus lente acceptée (Q9).
    BeforeDsdModulator,
}

/// Ce que la validation doit savoir de la zone et du compte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContexteValidation {
    /// Canaux du flux à l'entrée de la chaîne, s'ils sont connus. `None` à
    /// l'enregistrement : la contrainte « stéréo seulement » est alors
    /// vérifiée à l'exécution.
    pub channels: Option<u16>,
    /// Le compte a-t-il le Premium ?
    pub premium: bool,
}

/// Pourquoi une chaîne est refusée. Chaque variante a un code stable
/// ([`ErreurChaine::code`]) et un statut HTTP ([`ErreurChaine::statut_http`]).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ErreurChaine {
    #[error("version de chaîne {trouvee} non prise en charge (attendue : {attendue})")]
    VersionNonPriseEnCharge { trouvee: u32, attendue: u32 },
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
    #[error("le greffon {id} est stéréo seulement, le flux a {canaux} canaux")]
    StereoSeulement { id: String, canaux: u16 },
    #[error("{0} demande le Premium")]
    PremiumRequis(String),
    #[error("headroom manuel hors bornes : {0} dB")]
    HeadroomHorsBornes(f64),
    #[error("facteur d'upsampling hors bornes : {0}")]
    FacteurHorsBornes(u32),
}

impl ErreurChaine {
    /// 402 pour un droit manquant, 409 pour une chaîne invalide.
    pub fn statut_http(&self) -> u16 {
        match self {
            Self::PremiumRequis(_) => 402,
            _ => 409,
        }
    }

    /// Code stable du refus (terme du contrat, traduit par les clients).
    pub fn code(&self) -> &'static str {
        match self {
            Self::VersionNonPriseEnCharge { .. } => "unsupported_version",
            Self::GreffonInconnu(_) => "unknown_plugin",
            Self::GreffonEnDouble(_) => "duplicate_plugin",
            Self::ReglagesInvalides { .. } => "invalid_settings",
            Self::PasEnDernier(_) => "must_be_last_plugin",
            Self::Exclusifs { .. } => "mutually_exclusive",
            Self::StereoSeulement { .. } => "stereo_only",
            Self::PremiumRequis(_) => "premium_required",
            Self::HeadroomHorsBornes(_) => "headroom_out_of_range",
            Self::FacteurHorsBornes(_) => "upsampling_factor_out_of_range",
        }
    }

    /// Le greffon en cause, s'il y en a un.
    pub fn greffon(&self) -> Option<&str> {
        match self {
            Self::GreffonInconnu(id)
            | Self::GreffonEnDouble(id)
            | Self::PasEnDernier(id)
            | Self::PremiumRequis(id) => Some(id),
            Self::ReglagesInvalides { id, .. } | Self::StereoSeulement { id, .. } => Some(id),
            Self::Exclusifs { second, .. } => Some(second),
            _ => None,
        }
    }
}

/// L'id réservé de l'étage fixe d'upsampling dans un refus 402.
pub const ID_UPSAMPLING: &str = "upsampling";

impl ChaineTess {
    /// Un greffon est-il réellement en service ?
    fn greffons_actifs(&self) -> impl Iterator<Item = &MaillonGreffon> {
        self.plugins
            .iter()
            .filter(move |m| m.enabled && !self.bypass_all)
    }

    fn upsampling_actif(&self) -> bool {
        !self.bypass_all && !matches!(self.fixed.upsampling, Upsampling::Off {})
    }

    /// Le headroom réellement appliqué, en dB.
    pub fn headroom_effectif_db(&self) -> f64 {
        if self.bypass_all {
            return 0.0;
        }
        match self.fixed.headroom {
            Headroom::Manual { db } => db,
            Headroom::Auto {} => {
                if self.greffons_actifs().next().is_some() || self.upsampling_actif() {
                    HEADROOM_AUTO_DB
                } else {
                    0.0
                }
            }
        }
    }

    pub fn placement_volume(&self) -> PlacementVolume {
        match self.fixed.upsampling {
            Upsampling::Dsd { .. } if !self.bypass_all => PlacementVolume::BeforeDsdModulator,
            _ => PlacementVolume::AtRender,
        }
    }

    /// L'ordre réel des étapes (Q9), source et sortie comprises.
    pub fn ordre_des_etapes(&self) -> Vec<Etape> {
        let fixe = |code, active| Etape {
            code,
            plugin_id: None,
            fixed: true,
            active,
        };
        let mut etapes = vec![
            fixe(CodeEtape::Source, true),
            fixe(CodeEtape::Headroom, self.headroom_effectif_db() != 0.0),
        ];
        etapes.extend(self.plugins.iter().map(|m| Etape {
            code: CodeEtape::Plugin,
            plugin_id: Some(m.id.clone()),
            fixed: false,
            active: m.enabled && !self.bypass_all,
        }));
        etapes.push(fixe(CodeEtape::Upsampling, self.upsampling_actif()));
        etapes.push(fixe(CodeEtape::Volume, true));
        etapes.push(match self.placement_volume() {
            PlacementVolume::BeforeDsdModulator => fixe(CodeEtape::DsdModulator, true),
            PlacementVolume::AtRender => fixe(CodeEtape::Quantization, true),
        });
        etapes.push(fixe(CodeEtape::Output, true));
        etapes
    }

    /// Valide la chaîne contre le catalogue. La première faute trouvée est
    /// rendue ; l'ordre des contrôles est celui du code ci-dessous.
    pub fn valider(
        &self,
        catalogue: &Catalogue,
        contexte: ContexteValidation,
    ) -> Result<(), ErreurChaine> {
        if self.version != VERSION_CHAINE {
            return Err(ErreurChaine::VersionNonPriseEnCharge {
                trouvee: self.version,
                attendue: VERSION_CHAINE,
            });
        }
        if let Headroom::Manual { db } = self.fixed.headroom
            && !(db.is_finite() && (HEADROOM_MANUEL_MIN_DB..=HEADROOM_MANUEL_MAX_DB).contains(&db))
        {
            return Err(ErreurChaine::HeadroomHorsBornes(db));
        }
        if let Upsampling::Pcm { factor } = self.fixed.upsampling
            && !(FACTEUR_UPSAMPLING_MIN..=FACTEUR_UPSAMPLING_MAX).contains(&factor)
        {
            return Err(ErreurChaine::FacteurHorsBornes(factor));
        }

        let mut canaux = contexte.channels;
        let mut actifs_par_groupe: Vec<(&str, &str)> = Vec::new();
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
            if !m.enabled {
                continue;
            }
            if d.entitlement == Droit::Premium && !contexte.premium {
                return Err(ErreurChaine::PremiumRequis(m.id.clone()));
            }
            if let Some(groupe) = d.placement.exclusive_group.as_deref() {
                if let Some((_, premier)) = actifs_par_groupe.iter().find(|(g, _)| *g == groupe) {
                    return Err(ErreurChaine::Exclusifs {
                        premier: (*premier).to_owned(),
                        second: m.id.clone(),
                        groupe: groupe.to_owned(),
                    });
                }
                actifs_par_groupe.push((groupe, m.id.as_str()));
            }
            match d.placement.channels {
                Canaux::StereoOnly => {
                    if let Some(n) = canaux
                        && n != 2
                    {
                        return Err(ErreurChaine::StereoSeulement {
                            id: m.id.clone(),
                            canaux: n,
                        });
                    }
                }
                // Le nombre de canaux de sortie dépend des réglages ; il n'y a
                // de toute façon plus aucun greffon après (LastPlugin).
                Canaux::Remaps => canaux = None,
                Canaux::Any => {}
            }
        }
        if !matches!(self.fixed.upsampling, Upsampling::Off {}) && !contexte.premium {
            return Err(ErreurChaine::PremiumRequis(ID_UPSAMPLING.into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tess::greffon::catalogue_de_reference;
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

    const PREMIUM_STEREO: ContexteValidation = ContexteValidation {
        channels: Some(2),
        premium: true,
    };

    fn valider(c: &ChaineTess, ctx: ContexteValidation) -> Result<(), ErreurChaine> {
        c.valider(&catalogue_de_reference(), ctx)
    }

    /// La chaîne de la maquette A1 (zone « atelier », OAAT).
    fn chaine_maquette_a1() -> ChaineTess {
        ChaineTess {
            version: VERSION_CHAINE,
            bypass_all: false,
            plugins: vec![
                MaillonGreffon {
                    id: "convolution".into(),
                    enabled: true,
                    settings: json!({ "filter_set": "living-room.zip", "resample_nearest": true }),
                },
                MaillonGreffon {
                    id: "equalizer-graphic".into(),
                    enabled: true,
                    settings: json!({ "bands": 31, "preset": "Jazz" }),
                },
                maillon("crossfeed", false),
            ],
            fixed: EtagesFixes::default(),
        }
    }

    #[test]
    fn la_chaine_fait_l_aller_retour_json() {
        let mut variantes = vec![ChaineTess::default(), chaine_maquette_a1()];
        let mut c = chaine_maquette_a1();
        c.bypass_all = true;
        c.fixed = EtagesFixes {
            headroom: Headroom::Manual { db: -6.5 },
            upsampling: Upsampling::Pcm { factor: 4 },
            quantization: Quantification {
                dither: Dither::NoiseShaped,
            },
        };
        variantes.push(c.clone());
        for rate in [TauxDsd::Dsd64, TauxDsd::Dsd128, TauxDsd::Dsd256] {
            c.fixed.upsampling = Upsampling::Dsd { rate };
            variantes.push(c.clone());
        }
        for v in variantes {
            let texte = serde_json::to_string(&v).unwrap();
            let relue: ChaineTess = serde_json::from_str(&texte).unwrap();
            assert_eq!(relue, v, "{texte}");
        }
    }

    #[test]
    fn le_format_enregistre_est_fige() {
        // Le texte exact du contrat : un renommage de champ fait tomber ce test.
        let texte = r#"{
            "version": 1,
            "bypass_all": false,
            "plugins": [
                {"id": "convolution", "enabled": true, "settings": {"filter_set": "living-room.zip"}},
                {"id": "crossfeed", "enabled": false, "settings": null}
            ],
            "fixed": {
                "headroom": {"mode": "manual", "db": -3.0},
                "upsampling": {"mode": "dsd", "rate": "dsd128"},
                "quantization": {"dither": "tpdf"}
            }
        }"#;
        let c: ChaineTess = serde_json::from_str(texte).unwrap();
        assert_eq!(c.plugins[0].settings["filter_set"], "living-room.zip");
        assert_eq!(c.fixed.headroom, Headroom::Manual { db: -3.0 });
        assert_eq!(
            c.fixed.upsampling,
            Upsampling::Dsd {
                rate: TauxDsd::Dsd128
            }
        );
        let relu: Value = serde_json::from_str(texte).unwrap();
        assert_eq!(serde_json::to_value(&c).unwrap(), relu);
        // Champs facultatifs absents : valeurs par défaut.
        let minimale: ChaineTess = serde_json::from_str(r#"{"version":1}"#).unwrap();
        assert_eq!(minimale, ChaineTess::default());
    }

    #[test]
    fn un_champ_inconnu_est_refuse_a_la_lecture() {
        for texte in [
            r#"{"version":1,"volume":-18}"#,
            r#"{"version":1,"plugins":[{"id":"crossfeed","enabled":true,"order":2}]}"#,
            r#"{"version":1,"fixed":{"headroom":{"mode":"auto","db":-3}}}"#,
            r#"{"version":1,"fixed":{"upsampling":{"mode":"pcm"}}}"#,
            r#"{"version":1,"fixed":{"upsampling":{"mode":"off","factor":2}}}"#,
            r#"{"version":1,"fixed":{"quantization":{"dither":"rectangular"}}}"#,
        ] {
            assert!(
                serde_json::from_str::<ChaineTess>(texte).is_err(),
                "{texte}"
            );
        }
    }

    #[test]
    fn la_chaine_de_la_maquette_est_valide() {
        valider(&chaine_maquette_a1(), PREMIUM_STEREO).unwrap();
        valider(
            &ChaineTess::default(),
            ContexteValidation {
                channels: None,
                premium: false,
            },
        )
        .unwrap();
    }

    #[test]
    fn crossfeed_et_crosstalk_sont_exclusifs() {
        // Épreuve : un seul actif de la famille, l'autre décoché, passe.
        let ok = chaine(vec![
            maillon("crossfeed", true),
            maillon("crosstalk", false),
        ]);
        valider(&ok, PREMIUM_STEREO).unwrap();
        // Contre-épreuve : deux actifs, refusé en 409.
        for (a, b) in [
            ("crossfeed", "crosstalk"),
            ("crosstalk", "crossfeed-pro"),
            ("crossfeed", "crossfeed-pro"),
        ] {
            let e = valider(
                &chaine(vec![maillon(a, true), maillon(b, true)]),
                PREMIUM_STEREO,
            )
            .unwrap_err();
            assert_eq!(
                e,
                ErreurChaine::Exclusifs {
                    premier: a.into(),
                    second: b.into(),
                    groupe: "crossfeed_crosstalk".into()
                }
            );
            assert_eq!(e.statut_http(), 409);
            assert_eq!(e.code(), "mutually_exclusive");
        }
    }

    #[test]
    fn la_reaffectation_des_canaux_est_en_dernier() {
        let ok = chaine(vec![
            maillon("equalizer-graphic", true),
            maillon("crossfeed", true),
            maillon("channel-remap", true),
        ]);
        valider(&ok, PREMIUM_STEREO).unwrap();
        // Contre-épreuve : un greffon après la réaffectation, même décochée.
        for actif in [true, false] {
            let ko = chaine(vec![
                maillon("channel-remap", actif),
                maillon("equalizer-graphic", true),
            ]);
            assert_eq!(
                valider(&ko, PREMIUM_STEREO),
                Err(ErreurChaine::PasEnDernier("channel-remap".into()))
            );
        }
    }

    #[test]
    fn un_greffon_stereo_seulement_refuse_un_flux_multicanal() {
        let c = chaine(vec![
            maillon("equalizer-graphic", true),
            maillon("crossfeed", true),
        ]);
        valider(&c, PREMIUM_STEREO).unwrap();
        // Canaux inconnus à l'enregistrement : vérifié à l'exécution.
        valider(
            &c,
            ContexteValidation {
                channels: None,
                premium: true,
            },
        )
        .unwrap();
        // Contre-épreuve : 5.1.
        assert_eq!(
            valider(
                &c,
                ContexteValidation {
                    channels: Some(6),
                    premium: true
                }
            ),
            Err(ErreurChaine::StereoSeulement {
                id: "crossfeed".into(),
                canaux: 6
            })
        );
        // Un greffon qui ne DÉCLARE pas « stéréo seulement » accepte le multicanal.
        valider(
            &chaine(vec![maillon("convolution", true)]),
            ContexteValidation {
                channels: Some(6),
                premium: true,
            },
        )
        .unwrap();
    }

    #[test]
    fn un_greffon_premium_actif_demande_le_premium() {
        let free = ContexteValidation {
            channels: Some(2),
            premium: false,
        };
        // Free : graphique et casque passent.
        valider(
            &chaine(vec![
                maillon("equalizer-graphic", true),
                maillon("equalizer-headphone", true),
                maillon("channel-remap", true),
            ]),
            free,
        )
        .unwrap();
        // Un greffon Premium décoché est gardé sans droit (P2 : profil conservé).
        valider(&chaine(vec![maillon("equalizer-parametric", false)]), free).unwrap();
        // Contre-épreuve : actif, refusé en 402.
        let e = valider(&chaine(vec![maillon("equalizer-parametric", true)]), free).unwrap_err();
        assert_eq!(
            e,
            ErreurChaine::PremiumRequis("equalizer-parametric".into())
        );
        assert_eq!(e.statut_http(), 402);
        let mut c = ChaineTess::default();
        c.fixed.upsampling = Upsampling::Pcm { factor: 2 };
        assert_eq!(
            valider(&c, free),
            Err(ErreurChaine::PremiumRequis("upsampling".into()))
        );
        valider(&c, PREMIUM_STEREO).unwrap();
    }

    #[test]
    fn les_chaines_invalides_sont_refusees() {
        let cas: Vec<(ChaineTess, &str)> = vec![
            (
                ChaineTess {
                    version: 2,
                    ..ChaineTess::default()
                },
                "unsupported_version",
            ),
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
            (
                ChaineTess {
                    fixed: EtagesFixes {
                        headroom: Headroom::Manual { db: 6.0 },
                        ..EtagesFixes::default()
                    },
                    ..ChaineTess::default()
                },
                "headroom_out_of_range",
            ),
            (
                ChaineTess {
                    fixed: EtagesFixes {
                        headroom: Headroom::Manual { db: f64::NAN },
                        ..EtagesFixes::default()
                    },
                    ..ChaineTess::default()
                },
                "headroom_out_of_range",
            ),
            (
                ChaineTess {
                    fixed: EtagesFixes {
                        upsampling: Upsampling::Pcm { factor: 1 },
                        ..EtagesFixes::default()
                    },
                    ..ChaineTess::default()
                },
                "upsampling_factor_out_of_range",
            ),
        ];
        for (c, code) in cas {
            let e = valider(&c, PREMIUM_STEREO).unwrap_err();
            assert_eq!(e.code(), code, "{e}");
            assert_eq!(e.statut_http(), 409);
        }
    }

    #[test]
    fn l_ordre_reel_suit_q9() {
        use CodeEtape::*;
        let codes = |c: &ChaineTess| {
            c.ordre_des_etapes()
                .iter()
                .map(|e| e.code)
                .collect::<Vec<_>>()
        };
        let mut c = chaine_maquette_a1();
        // Upsampling éteint : volume puis quantification, en dernier.
        assert_eq!(
            codes(&c),
            [
                Source,
                Headroom,
                Plugin,
                Plugin,
                Plugin,
                Upsampling,
                Volume,
                Quantization,
                Output
            ]
        );
        assert_eq!(c.placement_volume(), PlacementVolume::AtRender);
        // Upsampling PCM : avant le volume, quantification séparée.
        c.fixed.upsampling = super::Upsampling::Pcm { factor: 2 };
        assert_eq!(
            codes(&c),
            [
                Source,
                Headroom,
                Plugin,
                Plugin,
                Plugin,
                Upsampling,
                Volume,
                Quantization,
                Output
            ]
        );
        assert_eq!(c.placement_volume(), PlacementVolume::AtRender);
        // Exception DSD : volume juste avant le modulateur.
        c.fixed.upsampling = super::Upsampling::Dsd {
            rate: TauxDsd::Dsd64,
        };
        let o = codes(&c);
        let v = o.iter().position(|e| *e == Volume).unwrap();
        assert_eq!(o[v + 1], DsdModulator);
        assert!(!o.contains(&Quantization));
        assert_eq!(c.placement_volume(), PlacementVolume::BeforeDsdModulator);
        // Ordre des greffons = ordre de la liste.
        let ids: Vec<_> = c
            .ordre_des_etapes()
            .into_iter()
            .filter_map(|e| e.plugin_id)
            .collect();
        assert_eq!(ids, ["convolution", "equalizer-graphic", "crossfeed"]);
    }

    #[test]
    fn headroom_automatique_et_bypass_all() {
        let mut c = ChaineTess::default();
        // Chaîne vide : 0 dB, rien d'actif sauf volume et quantification.
        assert_eq!(c.headroom_effectif_db(), 0.0);
        let actives: Vec<_> = c
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
        // Un greffon décoché ne déclenche pas le headroom.
        c.plugins.push(maillon("crossfeed", false));
        assert_eq!(c.headroom_effectif_db(), 0.0);
        // Contre-épreuve : un greffon actif, −3 dB.
        c.plugins[0].enabled = true;
        assert_eq!(c.headroom_effectif_db(), HEADROOM_AUTO_DB);
        // L'upsampler seul aussi.
        c.plugins[0].enabled = false;
        c.fixed.upsampling = Upsampling::Pcm { factor: 2 };
        assert_eq!(c.headroom_effectif_db(), HEADROOM_AUTO_DB);
        // Bypass all : tout contourné, volume au rendu, 0 dB.
        c.plugins[0].enabled = true;
        c.fixed.upsampling = Upsampling::Dsd {
            rate: TauxDsd::Dsd128,
        };
        c.bypass_all = true;
        assert_eq!(c.headroom_effectif_db(), 0.0);
        assert_eq!(c.placement_volume(), PlacementVolume::AtRender);
        assert!(
            c.ordre_des_etapes()
                .iter()
                .filter(|e| matches!(
                    e.code,
                    CodeEtape::Plugin | CodeEtape::Upsampling | CodeEtape::Headroom
                ))
                .all(|e| !e.active)
        );
    }
}
