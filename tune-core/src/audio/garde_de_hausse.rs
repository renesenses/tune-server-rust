//! #6721 — garde générale contre les montées brusques du volume.
//!
//! Fil 2366 : un Marantz NR1200 est parti à 100 % sur un geste, et un tweeter
//! n'y a pas survécu. Les causes établies sont corrigées (#6723, #6749,
//! #6797). Cette garde ferme la porte pour toutes les autres, connues ou non :
//! **aucune consigne ne monte le son de plus de [`SEUIL_DE_HAUSSE_DB`] en une
//! fois**, quel que soit son chemin (curseur, API, assistant, alarme, groupe,
//! UPnP, pilotage d'un appareil Sonos…). Au-delà, la consigne est PLAFONNÉE
//! au seuil ; aller plus haut d'un coup exige une action explicite
//! ([`Hausse::Confirmee`], champ `confirm_increase` des routes).
//!
//! # L'unité : le dB perçu du curseur de la zone
//!
//! `volume` reste le GAIN partout (décision du 23/10, #6812). La hausse se
//! mesure sur le CURSEUR de la zone (`volume_curve`), dont chaque point vaut
//! `PLAGE_PERCEPTIVE_DB / 100` = 0,6 dB perçu :
//!
//! * sortie logicielle (`db60`) : le curseur suit la loi en dB du gain, donc
//!   10 dB de hausse = 10 dB de gain en plus, exactement (au-dessus du genou) ;
//! * appareil réseau (`linear`) : le curseur EST l'échelle 0..100 de
//!   l'appareil, qui applique sa propre courbe, de forme perceptive. Le même
//!   écart de curseur (16,7 points) y est retenu comme 10 dB perçus.
//!
//! Une seule règle, donc, et le même geste de curseur donne le même plafond
//! sur toutes les zones.
//!
//! # Ce que la garde ne fait pas
//!
//! * Elle ne touche jamais une BAISSE, ni une consigne égale.
//! * Elle ne s'applique ni au 100 % de PURE verrouillé (armé par une
//!   confirmation, #5695), ni au volume fixe (armé par `confirm_full_volume`),
//!   qui sont déjà des actions explicites.
//! * Elle ne réécrit aucune valeur enregistrée.
use crate::audio::volume_scale::{EchelleDeVolume, PLAGE_PERCEPTIVE_DB};

/// La plus forte hausse permise en une consigne, en dB perçus du curseur de
/// la zone : 10 dB, soit 16,7 points de curseur (une perception « deux fois
/// plus fort »). Depuis le silence, une consigne s'arrête donc à 16,7 % sur
/// un appareil réseau et à −50 dB de gain sur une sortie logicielle.
pub const SEUIL_DE_HAUSSE_DB: f64 = 10.0;

/// Nom du champ (corps JSON) qui porte la confirmation sur les routes
/// `PUT /zones/{id}/volume`, `POST /playback/{id}/volume` et
/// `PATCH /zones/{id}`. Publié dans la réponse d'un plafonnement.
pub const CHAMP_DE_CONFIRMATION: &str = "confirm_increase";

/// Une consigne a-t-elle été confirmée par l'utilisateur ?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Hausse {
    /// Le cas de toute consigne : plafonnée à [`SEUIL_DE_HAUSSE_DB`].
    #[default]
    Gardee,
    /// L'utilisateur a confirmé cette hausse (champ `confirm_increase`) :
    /// elle passe en entier. Les limites #6541 et PURE tiennent toujours.
    Confirmee,
}

impl Hausse {
    /// `confirm_increase` d'un corps de requête (absent = non confirmé).
    pub fn depuis_confirmation(confirmee: bool) -> Self {
        if confirmee {
            Self::Confirmee
        } else {
            Self::Gardee
        }
    }
}

/// Le plus haut gain qu'une consigne non confirmée peut atteindre depuis
/// `actuel`, sur le curseur `echelle`. Un `actuel` illisible (NaN, infini)
/// vaut le silence : la prudence.
pub fn plafond(actuel: f64, echelle: EchelleDeVolume) -> f64 {
    let actuel = if actuel.is_finite() {
        actuel.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let position = echelle.position(actuel) + SEUIL_DE_HAUSSE_DB / PLAGE_PERCEPTIVE_DB;
    // Le plafond n'est jamais sous l'actuel : `position` arrondit vers le bas.
    echelle.gain(position.min(1.0)).max(actuel)
}

/// La consigne retenue : `demande` si elle ne monte pas de plus de
/// [`SEUIL_DE_HAUSSE_DB`] au-dessus d'`actuel` (ou si elle est confirmée),
/// sinon le plafond. Une baisse passe toujours, telle quelle.
pub fn retenir(actuel: f64, demande: f64, echelle: EchelleDeVolume, hausse: Hausse) -> f64 {
    let demande = if demande.is_finite() {
        demande.clamp(0.0, 1.0)
    } else {
        0.0
    };
    if hausse == Hausse::Confirmee {
        return demande;
    }
    demande.min(plafond(actuel, echelle))
}

/// Ce qu'une consigne est devenue : ce que les routes rendent au client.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VolumeRetenu {
    /// La consigne reçue (0..1, gain).
    pub demande: f64,
    /// Le gain réellement retenu, après limites #6541, PURE, volume fixe et
    /// cette garde.
    pub retenu: f64,
    /// Vrai quand CETTE garde a retenu la consigne sous la demande : le
    /// client peut alors proposer de confirmer.
    pub hausse_plafonnee: bool,
}

impl VolumeRetenu {
    /// Les champs ajoutés à la réponse d'une route quand la garde a mordu.
    /// Rien n'est ajouté sinon : la réponse d'une consigne ordinaire ne change
    /// pas.
    pub fn inserer(&self, obj: &mut serde_json::Map<String, serde_json::Value>) {
        if !self.hausse_plafonnee {
            return;
        }
        obj.insert("increase_capped".into(), serde_json::json!(true));
        obj.insert("requested_volume".into(), serde_json::json!(self.demande));
        obj.insert("volume".into(), serde_json::json!(self.retenu));
        obj.insert(
            "max_increase_db".into(),
            serde_json::json!(SEUIL_DE_HAUSSE_DB),
        );
        obj.insert(
            "confirm_field".into(),
            serde_json::json!(CHAMP_DE_CONFIRMATION),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::volume_scale::linear_to_db;

    const DB60: EchelleDeVolume = EchelleDeVolume::Perceptive;
    const LIN: EchelleDeVolume = EchelleDeVolume::Lineaire;

    #[test]
    fn une_baisse_ou_une_consigne_egale_passe_telle_quelle() {
        for echelle in [DB60, LIN] {
            assert_eq!(retenir(0.8, 0.1, echelle, Hausse::Gardee), 0.1);
            assert_eq!(retenir(0.8, 0.0, echelle, Hausse::Gardee), 0.0);
            assert_eq!(retenir(0.4, 0.4, echelle, Hausse::Gardee), 0.4);
        }
    }

    /// Le cas du fil 2366 : de 10 % à 100 % d'un coup sur un appareil réseau.
    #[test]
    fn de_10_a_100_sur_un_appareil_reseau_s_arrete_a_10_db_au_dessus() {
        let retenu = retenir(0.10, 1.0, LIN, Hausse::Gardee);
        assert!((retenu - (0.10 + 10.0 / 60.0)).abs() < 1e-12, "{retenu}");
    }

    #[test]
    fn sur_une_sortie_logicielle_la_hausse_est_de_10_db_de_gain() {
        // 50 % du curseur = −30 dB ; le plafond est −20 dB.
        let actuel = crate::audio::volume_scale::gain_perceptif(0.5);
        let retenu = retenir(actuel, 1.0, DB60, Hausse::Gardee);
        let ecart = linear_to_db(retenu).unwrap() - linear_to_db(actuel).unwrap();
        assert!(ecart <= SEUIL_DE_HAUSSE_DB + 1e-9, "{ecart}");
        assert!(ecart > SEUIL_DE_HAUSSE_DB - 1e-6, "{ecart}");
    }

    /// Sur toute la course, des deux échelles : jamais plus de 10 dB perçus
    /// (16,7 points de curseur) en une consigne.
    #[test]
    fn aucune_consigne_ne_monte_de_plus_du_seuil_sur_le_curseur() {
        for echelle in [DB60, LIN] {
            for a in 0..=1000 {
                let actuel = f64::from(a) / 1000.0;
                for d in [0.0, 0.25, 0.5, 0.75, 0.99, 1.0] {
                    let retenu = retenir(actuel, d, echelle, Hausse::Gardee);
                    let points = echelle.position(retenu) - echelle.position(actuel);
                    assert!(
                        points * PLAGE_PERCEPTIVE_DB <= SEUIL_DE_HAUSSE_DB + 1e-6,
                        "{echelle:?} {actuel} → {d} : {retenu} ({points})"
                    );
                    assert!(retenu <= d.max(actuel) + 1e-15);
                    assert!(retenu >= actuel.min(d) - 1e-15);
                }
            }
        }
    }

    #[test]
    fn une_hausse_confirmee_passe_en_entier() {
        assert_eq!(retenir(0.1, 1.0, LIN, Hausse::Confirmee), 1.0);
        assert_eq!(retenir(0.0, 1.0, DB60, Hausse::Confirmee), 1.0);
    }

    #[test]
    fn un_actuel_illisible_vaut_le_silence() {
        assert!(retenir(f64::NAN, 1.0, LIN, Hausse::Gardee) <= 10.0 / 60.0 + 1e-12);
        assert_eq!(retenir(0.5, f64::NAN, LIN, Hausse::Gardee), 0.0);
    }

    #[test]
    fn la_reponse_ne_change_que_si_la_garde_a_mordu() {
        let mut obj = serde_json::Map::new();
        VolumeRetenu {
            demande: 0.5,
            retenu: 0.5,
            hausse_plafonnee: false,
        }
        .inserer(&mut obj);
        assert!(obj.is_empty());
        VolumeRetenu {
            demande: 1.0,
            retenu: 0.2667,
            hausse_plafonnee: true,
        }
        .inserer(&mut obj);
        assert_eq!(obj["increase_capped"], true);
        assert_eq!(obj["volume"], 0.2667);
        assert_eq!(obj["requested_volume"], 1.0);
        assert_eq!(obj["confirm_field"], "confirm_increase");
    }
}
