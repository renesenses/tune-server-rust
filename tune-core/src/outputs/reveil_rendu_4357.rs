//! #4357 — le **réveil** de la boucle de rendu WASAPI exclusive.
//!
//! La boucle de rendu attendait l'événement du pilote et **rien d'autre** :
//! `WaitForSingleObject(event_handle, 2000)`. Or `stop()` pose `running =
//! false` puis appelle `IAudioClient::Stop` — après quoi le pilote ne signale
//! plus jamais son événement. Le fil de rendu passe la quasi-totalité de son
//! temps **dans cette attente** (une période de 10 ms, un rendu de quelques
//! microsecondes) : un arrêt demandé pendant l'attente n'a donc aucun moyen de
//! la réveiller, et le fil ne ressort qu'au **bout des 2 000 ms**.
//!
//! Mesuré dans les journaux de terrain de Didier, Tune **0.9.161 publiée**,
//! SMSL SU-8 en WASAPI exclusif (pièces jointes de #4357) :
//!
//! | journal | changements de piste | `local_audio_stop_thread_detached` | `output_ms` |
//! |---|---|---|---|
//! | 2026-09-22 13h49 | 27 | **27** | 25 fois ≈ 2 210 ms, 2 fois ≈ 210 ms |
//! | 2026-09-21 19h37 | 27 | **20** | 20 fois ≈ 2 215 ms |
//!
//! L'écart entre la demande d'arrêt et `wasapi_exclusive_render_thread_stopped`
//! vaut **2,00 à 2,06 s** sur les 28 arrêts du journal du 22/09 — la valeur du
//! délai d'attente, au millième près. Pendant ce temps `local.rs` épuise son
//! propre budget de 2 000 ms et **détache** le fil de lecture précédent, qui
//! tient encore le périphérique exclusif.
//!
//! Les compteurs, eux, se taisaient : à l'expiration, l'ancienne boucle ne
//! comptait une échéance manquée que **si `running` était encore vrai**. Un
//! arrêt donnait donc `deadline_misses=0` — et c'est bien ce qu'on lit sur les
//! 28 arrêts du journal. L'absence de compteur ne prouvait rien.
//!
//! Le correctif ajoute un **événement d'arrêt** (manuel, signalé par `stop()`
//! avant `IAudioClient::Stop`) et fait attendre la boucle sur les **deux**
//! poignées. Ce module porte la table de décision, hors de la couche COM, pour
//! être testée sur toutes les plateformes de CI.

/// Délai maximal de l'attente de rendu, en millisecondes.
/// Seul `wasapi_exclusive` l'attend ; les épreuves lisent la table de décision.
#[cfg(all(target_os = "windows", feature = "local-audio"))]
pub(crate) const ATTENTE_RENDU_MS: u32 = 2_000;

/// `WAIT_OBJECT_0` — poignée 0 du tableau : l'événement du pilote.
pub(crate) const REVEIL_EVENEMENT_PILOTE: u32 = 0;

/// `WAIT_OBJECT_0 + 1` — poignée 1 du tableau : l'événement d'arrêt.
pub(crate) const REVEIL_DEMANDE_ARRET: u32 = 1;

/// `WAIT_TIMEOUT` — l'attente est allée au bout de son délai.
pub(crate) const REVEIL_ECHEANCE: u32 = 0x0000_0102;

/// Ce que la boucle de rendu doit faire de son réveil.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReveilRendu {
    /// Servir un tampon au pilote.
    Rendre,
    /// Sortir de la boucle — tout de suite, sans attendre l'échéance.
    Arreter,
    /// Le pilote n'a rien réclamé dans le délai : compter, puis réattendre.
    EcheanceManquee,
}

/// Traduit le résultat de `WaitForMultipleObjects` en conduite à tenir.
///
/// `en_marche` est la lecture de `running` **après** le réveil : elle départage
/// un événement de pilote reçu alors que l'arrêt vient d'être demandé.
pub(crate) fn reveil_rendu(resultat_attente: u32, en_marche: bool) -> ReveilRendu {
    match resultat_attente {
        // L'événement d'arrêt prime : il n'est jamais signalé par erreur.
        REVEIL_DEMANDE_ARRET => ReveilRendu::Arreter,
        REVEIL_EVENEMENT_PILOTE if en_marche => ReveilRendu::Rendre,
        REVEIL_EVENEMENT_PILOTE => ReveilRendu::Arreter,
        REVEIL_ECHEANCE if en_marche => ReveilRendu::EcheanceManquee,
        REVEIL_ECHEANCE => ReveilRendu::Arreter,
        // `WAIT_FAILED` et tout code inattendu : une attente qui ne peut plus
        // bloquer ferait tourner la boucle à vide sur un cœur entier. On sort.
        _ => ReveilRendu::Arreter,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le cœur du défaut : l'arrêt demandé pendant l'attente doit sortir de la
    /// boucle **immédiatement**, et non au bout des 2 000 ms.
    #[test]
    fn l_evenement_d_arret_sort_de_la_boucle_sans_attendre_l_echeance() {
        assert_eq!(
            reveil_rendu(REVEIL_DEMANDE_ARRET, true),
            ReveilRendu::Arreter,
            "l'événement d'arrêt doit sortir de la boucle même si `running` \
             n'a pas encore été relu"
        );
        assert_eq!(
            reveil_rendu(REVEIL_DEMANDE_ARRET, false),
            ReveilRendu::Arreter
        );
    }

    #[test]
    fn l_evenement_du_pilote_fait_rendre_tant_que_la_sortie_tourne() {
        assert_eq!(
            reveil_rendu(REVEIL_EVENEMENT_PILOTE, true),
            ReveilRendu::Rendre
        );
        assert_eq!(
            reveil_rendu(REVEIL_EVENEMENT_PILOTE, false),
            ReveilRendu::Arreter
        );
    }

    /// L'échéance reste une échéance — le compteur `deadline_misses` garde son
    /// sens, et c'est lui qui a diagnostiqué #4184 (18 expirations en 38 s).
    #[test]
    fn l_echeance_est_comptee_tant_que_la_sortie_tourne() {
        assert_eq!(
            reveil_rendu(REVEIL_ECHEANCE, true),
            ReveilRendu::EcheanceManquee
        );
        assert_eq!(reveil_rendu(REVEIL_ECHEANCE, false), ReveilRendu::Arreter);
    }

    /// `WAIT_FAILED` : ne jamais réattendre en boucle sur une attente cassée.
    #[test]
    fn une_attente_cassee_ne_fait_pas_tourner_la_boucle_a_vide() {
        assert_eq!(reveil_rendu(u32::MAX, true), ReveilRendu::Arreter);
        assert_eq!(reveil_rendu(0x80, true), ReveilRendu::Arreter);
    }

    /// Le branchement — sans lui la table de décision ci-dessus ne garde rien.
    /// Comparé sans blancs : rustfmt coupe librement un appel long.
    #[test]
    fn la_boucle_de_rendu_attend_sur_les_deux_poignees() {
        let source: String = include_str!("wasapi_exclusive.rs")
            .split_whitespace()
            .collect();
        assert!(
            source.contains("WaitForMultipleObjects(2,poignees.as_ptr(),0,ATTENTE_RENDU_MS)"),
            "la boucle de rendu doit attendre sur les DEUX poignées"
        );
        assert!(
            !source.contains("WaitForSingleObject(event_handle,2000)"),
            "l'ancienne attente aveugle sur le seul événement du pilote est \
             encore présente : un arrêt y coûte 2 000 ms"
        );
    }

    /// `stop()` doit signaler l'événement **avant** `IAudioClient::Stop` :
    /// une fois le client arrêté, le pilote ne signale plus rien et la boucle
    /// irait de nouveau au bout de son délai.
    #[test]
    fn stop_reveille_la_boucle_avant_d_arreter_le_client() {
        let source: String = include_str!("wasapi_exclusive.rs")
            .split_whitespace()
            .collect();
        let reveil = source.find("SetEvent(self.stop_event)");
        let arret_client = source.find("letstop:StopFn=std::mem::transmute(*vtable.add(11));");
        assert!(
            reveil.is_some(),
            "stop() ne signale aucun événement d'arrêt"
        );
        assert!(
            arret_client.is_some(),
            "l'appel IAudioClient::Stop n'a pas été trouvé — garde à réécrire"
        );
        assert!(
            reveil < arret_client,
            "SetEvent doit précéder IAudioClient::Stop"
        );
    }
}
