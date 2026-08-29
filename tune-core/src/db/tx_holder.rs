//! Qui tient la transaction ouverte sur la connexion SQLite partagée.
//!
//! SQLite n'a **qu'une connexion** pour tout le serveur, et deux façons d'y
//! ouvrir une transaction :
//!
//! - `DbBackend::write_tx`, qui prend le verrou et rend la main tout de suite ;
//! - un `BEGIN IMMEDIATE` **brut**, tenu à travers un lot de travail entier.
//!
//! Les seuls `BEGIN IMMEDIATE` bruts du dépôt sont les chemins de scan
//! (`auto_scan.rs`, `routes/system/scan.rs`). Quand l'un d'eux tient sa
//! transaction, tout `write_tx` échoue sur *cannot start a transaction within
//! a transaction*.
//!
//! C'est ce qui est arrivé à Bilou (#1997) : douze essais espacés de 200 ms
//! épuisés — **2,4 secondes** — puis sa file d'attente vidée, sans que rien à
//! l'écran ne relie cela au scan en cours.
//!
//! ## Pourquoi ce module existe, et ce qu'il ne fait pas
//!
//! Il ne règle **pas** la contention. Le ticket le demande explicitement :
//!
//! > *« Journaliser qui tient la transaction est le préalable — sans quoi on
//! > optimisera à l'aveugle. »*
//!
//! Porter les essais de 12 à 24 déplacerait le seuil sans rien apprendre. Ici
//! on se contente de rendre le détenteur **nommable** : son étiquette et depuis
//! combien de temps il tient. Le jour où quelqu'un décidera de l'arbitrage,
//! il le fera sur des mesures et non sur une intuition.
//!
//! ## Coût
//!
//! Un `Mutex<Option<_>>` touché deux fois par lot de scan (déclaration,
//! libération) et une fois par échec de `write_tx`. Rien sur le chemin nominal
//! d'une transaction qui réussit.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Le détenteur courant : son étiquette et l'instant où il a pris la main.
///
/// `OnceLock` serait inutile ici — un `Mutex` const-initialisé suffit et évite
/// une indirection sur un chemin qui doit rester trivial.
static DETENTEUR: Mutex<Option<(&'static str, Instant)>> = Mutex::new(None);

/// Déclarer qu'un `BEGIN IMMEDIATE` brut vient d'aboutir.
///
/// L'étiquette est un `&'static str` à dessein : elle doit être une constante
/// du code appelant, pas une chaîne construite à l'exécution. On veut pouvoir
/// la chercher dans les sources depuis un journal.
pub fn declarer(etiquette: &'static str) {
    if let Ok(mut d) = DETENTEUR.lock() {
        *d = Some((etiquette, Instant::now()));
    }
}

/// Libérer après `COMMIT` ou `ROLLBACK`.
///
/// Idempotent : appeler `liberer` sans détenteur n'est pas une erreur. Un
/// chemin d'échec qui libère deux fois vaut mieux qu'un chemin qui oublie —
/// une étiquette périmée accuserait un innocent au prochain incident.
pub fn liberer() {
    if let Ok(mut d) = DETENTEUR.lock() {
        *d = None;
    }
}

/// Le détenteur courant et depuis combien de temps il tient, s'il y en a un.
pub fn courant() -> Option<(&'static str, Duration)> {
    DETENTEUR
        .lock()
        .ok()
        .and_then(|d| d.map(|(e, t)| (e, t.elapsed())))
}

/// Phrase à accoler à une erreur de transaction, ou chaîne vide.
///
/// Vide et non `None` quand personne n'est déclaré : l'appelant l'ajoute sans
/// condition, et **l'absence de détenteur est elle-même une information** — si
/// un `write_tx` échoue sur « within a transaction » sans qu'aucun scan ne soit
/// déclaré, c'est qu'une autre voie ouvre des transactions sans le dire, et
/// c'est cela qu'il faudra chercher.
pub fn mention() -> String {
    match courant() {
        Some((etiquette, age)) => {
            format!(
                " [transaction tenue par « {etiquette} » depuis {} ms]",
                age.as_millis()
            )
        }
        None => " [aucun détenteur déclaré — la transaction vient d'ailleurs]".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Les tests partagent un état global : ils prennent un verrou pour ne pas
    /// se marcher dessus. Sans lui, `cargo test` les exécute en parallèle et
    /// l'un remet à zéro ce que l'autre vient de poser — un test instable qu'on
    /// finirait par ignorer.
    static SERIE: Mutex<()> = Mutex::new(());

    #[test]
    fn sans_detenteur_la_mention_le_dit_au_lieu_de_se_taire() {
        let _s = SERIE.lock().unwrap_or_else(|e| e.into_inner());
        liberer();
        assert!(courant().is_none());
        let m = mention();
        assert!(
            m.contains("aucun détenteur"),
            "l'absence de détenteur doit être DITE : c'est le cas qui révélerait \
             une voie qui ouvre des transactions sans se déclarer. Obtenu : {m}"
        );
    }

    #[test]
    fn un_detenteur_declare_est_nomme_avec_son_age() {
        let _s = SERIE.lock().unwrap_or_else(|e| e.into_inner());
        declarer("scan:essai");
        let (etiquette, _) = courant().expect("un détenteur venait d'être déclaré");
        assert_eq!(etiquette, "scan:essai");
        let m = mention();
        assert!(m.contains("scan:essai"), "obtenu : {m}");
        assert!(m.contains(" ms]"), "l'âge doit figurer : {m}");
        liberer();
    }

    #[test]
    fn liberer_deux_fois_n_est_pas_une_erreur() {
        let _s = SERIE.lock().unwrap_or_else(|e| e.into_inner());
        declarer("scan:essai");
        liberer();
        liberer();
        assert!(courant().is_none());
    }

    /// Une étiquette périmée accuserait un innocent : la libération doit
    /// vraiment effacer, pas seulement marquer.
    #[test]
    fn une_etiquette_ne_survit_pas_a_sa_liberation() {
        let _s = SERIE.lock().unwrap_or_else(|e| e.into_inner());
        declarer("scan:premier");
        liberer();
        assert!(!mention().contains("scan:premier"), "{}", mention());
    }
}
