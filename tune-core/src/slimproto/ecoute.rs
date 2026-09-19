//! État des écoutes auxiliaires SlimProto (#3462).
//! Chaque tentative possède un jeton : une ancienne tâche annulée ne peut
//! effacer le résultat de sa remplaçante. Un échec reste visible jusqu'à la
//! prochaine tentative ou l'arrêt explicite ; une écoute arrêtée disparaît.

use std::io;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, serde::Serialize)]
pub struct EtatEcoute {
    pub port: u16,
    pub protocole: &'static str,
    pub ecoute: bool,
    pub cause: Option<&'static str>,
    pub message: Option<String>,
    pub erreur_systeme: Option<String>,
}

#[derive(Default)]
struct Observation {
    tentative: Option<Arc<()>>,
    etat: Option<EtatEcoute>,
}

pub(super) struct JournalEcoute(Mutex<Observation>);

impl JournalEcoute {
    pub const fn new() -> Self {
        Self(Mutex::new(Observation {
            tentative: None,
            etat: None,
        }))
    }

    pub fn lire(&self) -> Option<EtatEcoute> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .etat
            .clone()
    }

    pub fn effacer(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Observation::default();
    }

    pub fn commencer(&self) -> Tentative<'_> {
        let jeton = Arc::new(());
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Observation {
            tentative: Some(jeton.clone()),
            etat: None,
        };
        Tentative {
            journal: self,
            jeton,
        }
    }
}

pub(super) struct Tentative<'a> {
    journal: &'a JournalEcoute,
    jeton: Arc<()>,
}

impl Tentative<'_> {
    fn retenir(&self, etat: EtatEcoute) {
        let mut observation = self.journal.0.lock().unwrap_or_else(|e| e.into_inner());
        if observation
            .tentative
            .as_ref()
            .is_some_and(|t| Arc::ptr_eq(t, &self.jeton))
        {
            observation.etat = Some(etat);
        }
    }

    pub fn ecoute(&self, port: u16, protocole: &'static str) {
        self.retenir(EtatEcoute {
            port,
            protocole,
            ecoute: true,
            cause: None,
            message: None,
            erreur_systeme: None,
        });
    }

    /// Écoute obtenue sur un AUTRE port que celui demandé (#4361).
    ///
    /// Ce n'est pas une panne — le service est rendu, `ecoute` reste vrai et le
    /// composant de santé reste vert — mais c'est un dégradé : les
    /// télécommandes déjà configurées visent le mauvais numéro. La `cause` le
    /// nomme (`port_de_repli`) et le `message` porte les deux ports, pour que
    /// les écrans et le rapport de bogue puissent le DIRE au lieu de laisser
    /// l'utilisateur devant une télécommande muette.
    ///
    /// Pas d'`erreur_systeme` : l'erreur de bind qui a provoqué le repli est
    /// déjà dans le journal, et la retenir ici ferait passer une écoute vivante
    /// pour un échec aux yeux de tout lecteur pressé.
    pub fn ecoute_de_repli(&self, port: u16, protocole: &'static str, message: String) {
        self.retenir(EtatEcoute {
            port,
            protocole,
            ecoute: true,
            cause: Some("port_de_repli"),
            message: Some(message),
            erreur_systeme: None,
        });
    }

    pub fn echec(&self, port: u16, protocole: &'static str, erreur: &io::Error, consequence: &str) {
        // Un échec UDP ne permet pas d'identifier le détenteur, ni de conclure
        // quoi que ce soit sur le TCP du même numéro.
        let (cause, raison) = match erreur.kind() {
            io::ErrorKind::AddrInUse => ("adresse_deja_utilisee", "adresse déjà utilisée"),
            io::ErrorKind::PermissionDenied => ("permission_refusee", "permission refusée"),
            _ => ("echec_ecoute", "ouverture impossible"),
        };
        self.retenir(EtatEcoute {
            port,
            protocole,
            ecoute: false,
            cause: Some(cause),
            message: Some(format!("{protocole} {port} : {raison}. {consequence}")),
            erreur_systeme: Some(erreur.to_string()),
        });
    }
}

impl Drop for Tentative<'_> {
    fn drop(&mut self) {
        let mut observation = self.journal.0.lock().unwrap_or_else(|e| e.into_inner());
        if observation
            .tentative
            .as_ref()
            .is_some_and(|t| Arc::ptr_eq(t, &self.jeton))
            && observation.etat.as_ref().is_none_or(|e| e.ecoute)
        {
            *observation = Observation::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn une_ancienne_tache_nefface_pas_la_reprise() {
        let journal = JournalEcoute::new();
        let ancienne = journal.commencer();
        ancienne.ecoute(1, "UDP");
        let nouvelle = journal.commencer();
        nouvelle.ecoute(2, "UDP");
        ancienne.echec(1, "UDP", &io::ErrorKind::AddrInUse.into(), "ancien");
        drop(ancienne);
        assert_eq!(journal.lire().unwrap().port, 2);
        drop(nouvelle);
        assert!(journal.lire().is_none());
    }

    #[test]
    fn un_echec_persiste_jusqua_reprise_ou_arret() {
        let journal = JournalEcoute::new();
        {
            let tentative = journal.commencer();
            tentative.echec(
                9090,
                "TCP",
                &io::ErrorKind::PermissionDenied.into(),
                "CLI indisponible",
            );
        }
        let etat = journal.lire().unwrap();
        assert!(!etat.ecoute);
        assert_eq!(etat.cause, Some("permission_refusee"));
        assert!(etat.message.unwrap().contains("CLI indisponible"));
        journal.effacer();
        assert!(journal.lire().is_none());
    }
}
