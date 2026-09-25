//! #4971 — boîte noire du contact SOAP avec un renderer DLNA.
//!
//! Le Diretta Renderer du NUC (`DirettaRenderer/1.0`, 192.168.1.100:4005)
//! cesse d'écouter peu après une bascule PURE. Le journal INFO de Tune ne
//! disait ni QUAND le renderer avait cessé de répondre (le sondeur l'interroge
//! chaque seconde, mais au niveau DEBUG), ni ce que Tune lui avait envoyé
//! juste avant. Entre le dernier flux lâché (18:59:07) et le premier `Play`
//! refusé (19:01:00), deux minutes sans une ligne : impossible de trancher
//! entre « Tune fait tomber le renderer » et « le renderer tombe tout seul ».
//!
//! Ce module ne corrige rien. Il garde les derniers échanges SOAP de chaque
//! sortie et rend UNE bascule par changement d'état :
//!
//! * `Perdu` au premier échec de contact (refus, délai, coupure) qui suit un
//!   échange réussi — avec l'échange réussi précédent et l'historique ;
//! * `Retrouve` à la première réponse qui suit une perte — avec sa durée.
//!
//! Les échecs suivants d'une même perte ne rendent rien : un sondeur à 1 Hz
//! n'inonde pas le journal.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Nombre d'échanges gardés : une douzaine couvre le cycle complet d'une
/// relance (Stop, GetTransportInfo×n, SetAVTransportURI, Play, relecture de
/// l'URI) et les quelques sondages qui la suivent.
pub(crate) const ECHANGES_RETENUS: usize = 12;

/// Ce qu'un envoi SOAP a donné, du seul point de vue du CONTACT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IssueContact {
    /// Le renderer a répondu (quel que soit le statut HTTP : une faute SOAP
    /// est une réponse).
    Reponse,
    /// Le port n'écoute pas (`ECONNREFUSED`, `10061`).
    Refus,
    /// Aucune réponse dans le délai.
    Expire,
    /// Connexion impossible ou coupée avant la réponse, réessais épuisés.
    Coupure,
}

impl IssueContact {
    pub(crate) fn etiquette(self) -> &'static str {
        match self {
            Self::Reponse => "ok",
            Self::Refus => "refus",
            Self::Expire => "delai",
            Self::Coupure => "coupure",
        }
    }
}

#[derive(Debug, Clone)]
struct Echange {
    action: String,
    issue: IssueContact,
    duree_ms: u64,
    a: Instant,
}

/// Le changement d'état que l'appelant doit porter au journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Bascule {
    Aucune,
    Perdu {
        /// Dernier échange réussi : action et ancienneté (ms).
        dernier_ok: Option<(String, u64)>,
        /// Les échanges retenus, du plus ancien au plus récent :
        /// `Action/issue/durée_ms/-il_y_a_ms`, séparés par `;`.
        historique: String,
    },
    Retrouve {
        duree_perte_ms: u64,
        echecs: u32,
    },
}

#[derive(Debug, Default)]
pub(crate) struct JournalDeContact {
    echanges: VecDeque<Echange>,
    dernier_ok: Option<(String, Instant)>,
    perdu_depuis: Option<Instant>,
    echecs: u32,
}

impl JournalDeContact {
    /// Noter un envoi et rendre la bascule éventuelle.
    pub(crate) fn noter(
        &mut self,
        action: &str,
        issue: IssueContact,
        duree: Duration,
        maintenant: Instant,
    ) -> Bascule {
        if self.echanges.len() == ECHANGES_RETENUS {
            self.echanges.pop_front();
        }
        self.echanges.push_back(Echange {
            action: action.to_string(),
            issue,
            duree_ms: duree.as_millis() as u64,
            a: maintenant,
        });

        if issue == IssueContact::Reponse {
            self.dernier_ok = Some((action.to_string(), maintenant));
            return match self.perdu_depuis.take() {
                Some(depuis) => {
                    let echecs = std::mem::take(&mut self.echecs);
                    Bascule::Retrouve {
                        duree_perte_ms: maintenant.saturating_duration_since(depuis).as_millis()
                            as u64,
                        echecs,
                    }
                }
                None => Bascule::Aucune,
            };
        }

        self.echecs += 1;
        if self.perdu_depuis.is_some() {
            return Bascule::Aucune;
        }
        self.perdu_depuis = Some(maintenant);
        Bascule::Perdu {
            dernier_ok: self.dernier_ok.as_ref().map(|(a, t)| {
                (
                    a.clone(),
                    maintenant.saturating_duration_since(*t).as_millis() as u64,
                )
            }),
            historique: self.historique(maintenant),
        }
    }

    fn historique(&self, maintenant: Instant) -> String {
        self.echanges
            .iter()
            .map(|e| {
                format!(
                    "{}/{}/{}ms/-{}ms",
                    e.action,
                    e.issue.etiquette(),
                    e.duree_ms,
                    maintenant.saturating_duration_since(e.a).as_millis()
                )
            })
            .collect::<Vec<_>>()
            .join(";")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn une_perte_se_dit_une_fois_avec_ce_qui_la_precede() {
        let t0 = Instant::now();
        let mut j = JournalDeContact::default();
        assert_eq!(
            j.noter("SetAVTransportURI", IssueContact::Reponse, ms(3833), t0),
            Bascule::Aucune
        );
        assert_eq!(
            j.noter("Play", IssueContact::Reponse, ms(2), t0 + ms(200)),
            Bascule::Aucune
        );
        let b = j.noter(
            "GetTransportInfo",
            IssueContact::Refus,
            ms(0),
            t0 + ms(1200),
        );
        let Bascule::Perdu {
            dernier_ok,
            historique,
        } = b
        else {
            panic!("le premier échec doit rendre Perdu : {b:?}");
        };
        assert_eq!(dernier_ok, Some(("Play".to_string(), 1000)));
        assert_eq!(
            historique,
            "SetAVTransportURI/ok/3833ms/-1200ms;Play/ok/2ms/-1000ms;GetTransportInfo/refus/0ms/-0ms"
        );
        // Le sondeur à 1 Hz ne réécrit rien tant que la perte dure.
        for k in 2..5 {
            assert_eq!(
                j.noter(
                    "GetTransportInfo",
                    IssueContact::Refus,
                    ms(0),
                    t0 + ms(1000 * k)
                ),
                Bascule::Aucune
            );
        }
        assert_eq!(
            j.noter(
                "GetTransportInfo",
                IssueContact::Reponse,
                ms(3),
                t0 + ms(6200)
            ),
            Bascule::Retrouve {
                duree_perte_ms: 5000,
                echecs: 4
            }
        );
    }

    #[test]
    fn l_historique_est_borne() {
        let t0 = Instant::now();
        let mut j = JournalDeContact::default();
        for k in 0..(ECHANGES_RETENUS as u64 + 5) {
            j.noter("GetPositionInfo", IssueContact::Reponse, ms(1), t0 + ms(k));
        }
        let Bascule::Perdu { historique, .. } =
            j.noter("Play", IssueContact::Expire, ms(2001), t0 + ms(100))
        else {
            panic!("Perdu attendu");
        };
        assert_eq!(historique.split(';').count(), ECHANGES_RETENUS);
        assert!(
            historique.ends_with("Play/delai/2001ms/-0ms"),
            "{historique}"
        );
    }

    #[test]
    fn une_perte_des_le_premier_envoi_n_a_pas_de_dernier_ok() {
        let mut j = JournalDeContact::default();
        let b = j.noter("Play", IssueContact::Coupure, ms(5), Instant::now());
        assert!(
            matches!(
                b,
                Bascule::Perdu {
                    dernier_ok: None,
                    ..
                }
            ),
            "{b:?}"
        );
    }
}
