//! La RÉVISION de la bibliothèque — chantier #7619, volet L5.
//!
//! Le client web garde les pages de la Bibliothèque dans le cache de son
//! navigateur (IndexedDB) : il les affiche aussitôt, puis il les redemande.
//! Pour savoir quand ce cache ne vaut plus rien, il lit cette révision : une
//! révision différente de celle sous laquelle il a rangé ses pages, et il les
//! jette.
//!
//! ## Ce qui la fait changer
//!
//! - tout événement `library.*` émis sur le bus (scan commencé, en cours,
//!   terminé, enrichissement, pochettes, `library.updated` du veilleur et des
//!   éditions) : voir [`crate::event_bus::EventBus::emit`] ;
//! - toute requête qui MODIFIE la bibliothèque par l'API (`POST`, `PUT`,
//!   `PATCH`, `DELETE` réussis sous `/library` et `/metadata`), posée par une
//!   couche du routeur côté serveur. Beaucoup d'éditions n'émettent aucun
//!   événement : la couche les rattrape toutes, sans liste à tenir.
//!
//! Une révision de trop ne coûte qu'un rechargement ; une révision manquée
//! laisserait le client sur des pages fausses. Le compteur penche donc
//! toujours du côté de « trop ».
//!
//! ## Pourquoi pas un simple entier
//!
//! Le compteur vit en mémoire et repart à zéro au redémarrage. Un client qui
//! aurait rangé ses pages sous « 5 » avant un redémarrage retrouverait « 5 »
//! plus tard, pour une tout autre bibliothèque. La révision porte donc
//! l'instant de démarrage du processus : `"<démarrage>-<compteur>"`. Deux
//! processus ne la partagent jamais, et le premier appel après un redémarrage
//! invalide le cache, ce qui est juste (un scan a pu passer entre-temps).

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

static COMPTEUR: AtomicU64 = AtomicU64::new(0);

/// L'instant de démarrage, en millisecondes depuis l'époque Unix, figé au
/// premier usage.
static DEMARRAGE: LazyLock<u64> = LazyLock::new(|| {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
});

/// La révision courante, telle que l'API la rend.
pub fn courante() -> String {
    format!("{}-{}", *DEMARRAGE, COMPTEUR.load(Ordering::SeqCst))
}

/// La bibliothèque a changé (ou a pu changer).
pub fn avancer() {
    // Fige le démarrage AVANT d'avancer : un premier `avancer()` suivi d'un
    // premier `courante()` doit donner la même base que l'inverse.
    let _ = *DEMARRAGE;
    COMPTEUR.fetch_add(1, Ordering::SeqCst);
}

/// Un événement du bus qui touche la bibliothèque.
///
/// `library.` en préfixe, sauf l'historique d'écoute (qui vit sous
/// `/library/history` côté API mais ne change aucune page de la
/// Bibliothèque) : aucun événement du bus ne le porte aujourd'hui, la garde
/// n'est qu'une précaution.
pub fn evenement_de_bibliotheque(event_type: &str) -> bool {
    event_type.starts_with("library.") && !event_type.starts_with("library.history")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avancer_change_la_revision() {
        let avant = courante();
        avancer();
        let apres = courante();
        assert_ne!(avant, apres);
        // Même démarrage, compteur différent.
        assert_eq!(avant.split('-').next(), apres.split('-').next());
    }

    #[test]
    fn la_revision_porte_le_demarrage() {
        let r = courante();
        let (demarrage, compteur) = r.split_once('-').expect("forme <démarrage>-<compteur>");
        assert!(demarrage.parse::<u64>().unwrap() > 0);
        assert!(compteur.parse::<u64>().is_ok());
    }

    #[test]
    fn evenements_reconnus() {
        assert!(evenement_de_bibliotheque("library.scan.completed"));
        assert!(evenement_de_bibliotheque("library.updated"));
        assert!(evenement_de_bibliotheque("library.scan.progress"));
        assert!(!evenement_de_bibliotheque("playback.started"));
        assert!(!evenement_de_bibliotheque("zone.updated"));
        assert!(!evenement_de_bibliotheque("library.history.added"));
    }
}
