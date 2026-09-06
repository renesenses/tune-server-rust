//! DUP-1 (phase 2) : la PRÉSENCE d'une zone, à côté de `online` et jamais à sa
//! place.
//!
//! `online` répond « l'appareil répond-il maintenant ? ». Il ne dit pas depuis
//! quand il ne répond plus : une zone éteinte depuis deux minutes et une zone
//! abandonnée depuis trois semaines se ressemblaient. `presence` qualifie les
//! zones que le calcul vivant dit hors ligne, à partir de la dernière réponse
//! datée (`zones.last_seen_at`). Même patron que `output_reach` : un champ
//! ajouté, absent des serveurs anciens, que le client traite comme neutre.
//!
//! « Remplacée » n'est JAMAIS déduit de l'âge seul : c'est le rapport des
//! doublons (`zones_doublons`, phase 0) qui le propose, quand un autre
//! identifiant du même appareil est en ligne. Une zone seule, si vieille
//! soit-elle, est absente — pas remplacée.

use serde_json::{Value, json};

/// En deçà, une zone hors ligne est « éteinte récemment ».
pub(super) const RECENTE_SECS: i64 = 24 * 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Presence {
    EnLigne,
    EteinteRecemment,
    AbsenteDepuis { jours: i64 },
    JamaisVue,
}

impl Presence {
    pub(super) fn qualifier(en_ligne: bool, age_secs: Option<i64>) -> Self {
        if en_ligne {
            return Self::EnLigne;
        }
        match age_secs {
            None => Self::JamaisVue,
            Some(age) if age < RECENTE_SECS => Self::EteinteRecemment,
            Some(age) => Self::AbsenteDepuis {
                jours: age / 86_400,
            },
        }
    }

    pub(super) fn code(self) -> &'static str {
        match self {
            Self::EnLigne => "en_ligne",
            Self::EteinteRecemment => "eteinte_recemment",
            Self::AbsenteDepuis { .. } => "absente_depuis",
            Self::JamaisVue => "jamais_vue",
        }
    }

    /// Les champs à poser dans le JSON d'une zone.
    pub(super) fn champs(self) -> Vec<(&'static str, Value)> {
        let mut champs = vec![("presence", json!(self.code()))];
        if let Self::AbsenteDepuis { jours } = self {
            champs.push(("jours_absente", json!(jours)));
        }
        champs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_presence_se_qualifie_sans_toucher_a_online() {
        assert_eq!(Presence::qualifier(true, None), Presence::EnLigne);
        assert_eq!(
            Presence::qualifier(true, Some(30 * 86_400)),
            Presence::EnLigne,
            "en ligne prime sur l'age"
        );
        assert_eq!(Presence::qualifier(false, None), Presence::JamaisVue);
        assert_eq!(
            Presence::qualifier(false, Some(3_600)),
            Presence::EteinteRecemment
        );
        assert_eq!(
            Presence::qualifier(false, Some(RECENTE_SECS - 1)),
            Presence::EteinteRecemment
        );
        assert_eq!(
            Presence::qualifier(false, Some(RECENTE_SECS)),
            Presence::AbsenteDepuis { jours: 1 }
        );
        assert_eq!(
            Presence::qualifier(false, Some(21 * 86_400 + 5)),
            Presence::AbsenteDepuis { jours: 21 }
        );
    }

    #[test]
    fn les_champs_json_ne_portent_les_jours_que_pour_une_absence() {
        let absente = Presence::AbsenteDepuis { jours: 21 }.champs();
        assert_eq!(
            absente,
            vec![
                ("presence", json!("absente_depuis")),
                ("jours_absente", json!(21))
            ]
        );
        assert_eq!(
            Presence::EteinteRecemment.champs(),
            vec![("presence", json!("eteinte_recemment"))]
        );
        assert_eq!(
            Presence::JamaisVue.champs(),
            vec![("presence", json!("jamais_vue"))]
        );
    }
}
