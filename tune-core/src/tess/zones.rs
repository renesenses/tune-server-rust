//! Où TESS s'applique (décision Q8).
//!
//! TESS s'applique sur toutes les zones où Tune FABRIQUE le flux : sortie
//! locale, OAAT, famille UPnP (DLNA, OpenHome, BluOS, Squeezebox), Chromecast,
//! AirPlay, Devialet AIR. HQPlayer et Diretta sont HORS TESS, et l'écran TESS
//! affiche le motif. Un transport que ce contrat ne classe pas est hors TESS
//! tant qu'un lot (T12) ne l'a pas classé : on ne promet jamais un traitement
//! qui ne serait pas appliqué.
//!
//! Être « dans TESS » ne veut pas dire que tout y est déjà livré : l'état
//! effectif étape par étape (demandé, effectif, motif) est le travail des lots
//! T08, T12a, T12b et T12c.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TransportZone {
    #[serde(rename = "local")]
    Local,
    #[serde(rename = "oaat")]
    Oaat,
    #[serde(rename = "dlna")]
    Dlna,
    #[serde(rename = "openhome")]
    OpenHome,
    #[serde(rename = "bluos")]
    BluOs,
    #[serde(rename = "squeezebox")]
    Squeezebox,
    #[serde(rename = "chromecast")]
    Chromecast,
    #[serde(rename = "airplay")]
    AirPlay,
    #[serde(rename = "devialet_air")]
    DevialetAir,
    #[serde(rename = "hqplayer")]
    HqPlayer,
    #[serde(rename = "diretta")]
    Diretta,
    #[serde(rename = "other")]
    Other,
}

/// Pourquoi une zone est hors TESS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MotifHorsTess {
    /// HQPlayer applique son propre traitement au flux qu'il reçoit.
    HqplayerOwnProcessing,
    /// Diretta transporte le flux tel quel jusqu'à son hôte.
    DirettaOutsideTess,
    /// Transport pas encore classé par ce contrat.
    UnclassifiedTransport,
}

impl MotifHorsTess {
    /// Le motif en clair, pour un écran sans table de traduction.
    pub fn detail(self) -> &'static str {
        match self {
            Self::HqplayerOwnProcessing => {
                "HQPlayer applies its own processing: TESS is not applied to this zone."
            }
            Self::DirettaOutsideTess => {
                "Diretta carries the stream unchanged: TESS is not applied to this zone."
            }
            Self::UnclassifiedTransport => {
                "This output is not covered by TESS yet: no processing is applied."
            }
        }
    }
}

/// TESS s'applique-t-il à une zone ?
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Applicabilite {
    Applied,
    OutsideTess {
        reason: MotifHorsTess,
        detail: String,
    },
}

impl TransportZone {
    pub const ALL: [Self; 12] = [
        Self::Local,
        Self::Oaat,
        Self::Dlna,
        Self::OpenHome,
        Self::BluOs,
        Self::Squeezebox,
        Self::Chromecast,
        Self::AirPlay,
        Self::DevialetAir,
        Self::HqPlayer,
        Self::Diretta,
        Self::Other,
    ];

    /// Depuis le `output_type` d'une zone (`zone_repo::Zone::output_type`).
    pub fn depuis_type_de_sortie(output_type: Option<&str>) -> Self {
        match output_type.map(str::to_ascii_lowercase).as_deref() {
            Some("local") => Self::Local,
            Some("oaat") => Self::Oaat,
            Some("dlna") => Self::Dlna,
            Some("openhome") => Self::OpenHome,
            Some("bluos") => Self::BluOs,
            Some("squeezebox" | "slimproto") => Self::Squeezebox,
            Some("chromecast") => Self::Chromecast,
            Some("airplay" | "airplay2") => Self::AirPlay,
            Some("devialet_air") => Self::DevialetAir,
            Some("hqplayer") => Self::HqPlayer,
            Some("diretta") => Self::Diretta,
            _ => Self::Other,
        }
    }

    pub fn applicabilite(self) -> Applicabilite {
        let hors = |reason: MotifHorsTess| Applicabilite::OutsideTess {
            reason,
            detail: reason.detail().to_owned(),
        };
        match self {
            Self::HqPlayer => hors(MotifHorsTess::HqplayerOwnProcessing),
            Self::Diretta => hors(MotifHorsTess::DirettaOutsideTess),
            Self::Other => hors(MotifHorsTess::UnclassifiedTransport),
            Self::Local
            | Self::Oaat
            | Self::Dlna
            | Self::OpenHome
            | Self::BluOs
            | Self::Squeezebox
            | Self::Chromecast
            | Self::AirPlay
            | Self::DevialetAir => Applicabilite::Applied,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hqplayer_et_diretta_sont_hors_tess_avec_un_motif() {
        for (t, motif) in [
            (
                TransportZone::HqPlayer,
                MotifHorsTess::HqplayerOwnProcessing,
            ),
            (TransportZone::Diretta, MotifHorsTess::DirettaOutsideTess),
        ] {
            match t.applicabilite() {
                Applicabilite::OutsideTess { reason, detail } => {
                    assert_eq!(reason, motif);
                    assert!(!detail.is_empty());
                }
                Applicabilite::Applied => panic!("{t:?} doit être hors TESS"),
            }
        }
    }

    #[test]
    fn les_zones_ou_tune_fabrique_le_flux_sont_dans_tess() {
        for t in [
            "local",
            "oaat",
            "dlna",
            "openhome",
            "bluos",
            "squeezebox",
            "slimproto",
            "chromecast",
            "airplay2",
            "devialet_air",
        ] {
            assert_eq!(
                TransportZone::depuis_type_de_sortie(Some(t)).applicabilite(),
                Applicabilite::Applied,
                "{t}"
            );
        }
        // Contre-épreuve : inconnu ou absent = hors TESS, jamais appliqué en silence.
        for t in [None, Some("aes67"), Some("")] {
            assert!(matches!(
                TransportZone::depuis_type_de_sortie(t).applicabilite(),
                Applicabilite::OutsideTess {
                    reason: MotifHorsTess::UnclassifiedTransport,
                    ..
                }
            ));
        }
    }

    #[test]
    fn transports_et_applicabilite_font_l_aller_retour_json() {
        for t in TransportZone::ALL {
            let texte = serde_json::to_string(&t).unwrap();
            assert_eq!(serde_json::from_str::<TransportZone>(&texte).unwrap(), t);
            let a = t.applicabilite();
            let texte = serde_json::to_string(&a).unwrap();
            assert_eq!(serde_json::from_str::<Applicabilite>(&texte).unwrap(), a);
        }
        assert_eq!(
            serde_json::to_value(TransportZone::HqPlayer.applicabilite()).unwrap()["reason"],
            "hqplayer_own_processing"
        );
    }
}
