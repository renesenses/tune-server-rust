//! Une commande MPRIS → une requête à l'API de la zone.
//!
//! Mêmes routes que le mini-lecteur (`tune-widget/src/app.js`) et que les
//! clients web et remote : `POST /api/v1/zones/{id}/…`
//! (`tune-server/src/routes/playback.rs`).

use serde_json::{Value, json};

/// Ce que le bureau demande à la zone.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// `PlayPause` : la bascule en un appel (#4566), pour une touche unique.
    LecturePause,
    /// `Play` : reprendre.
    Lecture,
    Pause,
    Arret,
    Suivante,
    Precedente,
    /// Aller à cette position, en millisecondes.
    Position(i64),
    /// Gain linéaire 0..1, déjà passé par la loi du curseur.
    Volume(f64),
}

/// Une requête `POST` relative à la racine du serveur.
#[derive(Debug, Clone, PartialEq)]
pub struct Requete {
    pub chemin: String,
    pub corps: Option<Value>,
}

pub fn requete(zone_id: i64, action: &Action) -> Requete {
    let base = format!("/api/v1/zones/{zone_id}");
    let (suffixe, corps) = match action {
        Action::LecturePause => ("playpause", None),
        Action::Lecture => ("resume", None),
        Action::Pause => ("pause", None),
        Action::Arret => ("stop", None),
        Action::Suivante => ("next", None),
        Action::Precedente => ("previous", None),
        // Le serveur refuse une position négative (#3966) : bornée ici.
        Action::Position(ms) => ("seek", Some(json!({ "position_ms": (*ms).max(0) }))),
        // Jamais `confirm_increase` : la garde de hausse du serveur (#6721)
        // plafonne un saut de plus de 10 dB, ce qu'une molette ne doit pas
        // pouvoir contourner.
        Action::Volume(gain) => ("volume", Some(json!({ "volume": gain.clamp(0.0, 1.0) }))),
    };
    Requete {
        chemin: format!("{base}/{suffixe}"),
        corps,
    }
}

/// Qui exécute les commandes : le client HTTP en vrai, un faux en test.
// `async_trait` pose un `#[must_use]` sur un futur qui l'est déjà.
#[allow(clippy::double_must_use)]
#[async_trait::async_trait]
pub trait Commandes: Send + Sync {
    async fn envoyer(&self, action: Action) -> Result<(), String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chaque_commande_vise_la_route_de_la_zone() {
        let cas = [
            (Action::LecturePause, "/api/v1/zones/3/playpause"),
            (Action::Lecture, "/api/v1/zones/3/resume"),
            (Action::Pause, "/api/v1/zones/3/pause"),
            (Action::Arret, "/api/v1/zones/3/stop"),
            (Action::Suivante, "/api/v1/zones/3/next"),
            (Action::Precedente, "/api/v1/zones/3/previous"),
        ];
        for (action, chemin) in cas {
            let r = requete(3, &action);
            assert_eq!(r.chemin, chemin);
            assert_eq!(r.corps, None, "{action:?} n'a pas de corps");
        }
    }

    #[test]
    fn seek_porte_des_millisecondes_jamais_negatives() {
        let r = requete(1, &Action::Position(12_345));
        assert_eq!(r.chemin, "/api/v1/zones/1/seek");
        assert_eq!(r.corps, Some(json!({ "position_ms": 12_345 })));
        assert_eq!(
            requete(1, &Action::Position(-5)).corps,
            Some(json!({ "position_ms": 0 }))
        );
    }

    #[test]
    fn volume_porte_un_gain_borne_sans_confirmation_de_hausse() {
        let r = requete(2, &Action::Volume(0.25));
        assert_eq!(r.chemin, "/api/v1/zones/2/volume");
        assert_eq!(r.corps, Some(json!({ "volume": 0.25 })));
        assert_eq!(
            requete(2, &Action::Volume(3.0)).corps,
            Some(json!({ "volume": 1.0 }))
        );
    }
}
