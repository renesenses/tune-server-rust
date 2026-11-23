//! Diagnostic rc14 (#8963, #7398, #9174) : ce que le relais a reçu du pont,
//! et quand.
//!
//! Une lecture d'une zone navigateur jouée à distance (« Ce téléphone » par le
//! pont) est abandonnée au bout de 12 s quand personne ne tire le flux
//! (`lecture_sans_destination_abandonnee`). Le journal ne disait pas si le
//! téléphone parlait encore au serveur à ce moment-là. Trois histoires
//! restent possibles, et elles ne laissent pas les mêmes traces :
//!
//! - le téléphone ne dit plus rien par le pont (ni état, ni position) :
//!   aucune requête relayée depuis la lecture ;
//! - le téléphone parle (états, positions) mais ne demande pas le flux :
//!   requêtes relayées récentes, aucune demande de flux ;
//! - le pont demande un flux, mais pas celui de la lecture en cours.
//!
//! Ce module garde l'instant de la dernière requête d'API relayée et de la
//! dernière demande de flux relayée. Rien d'autre : ni chemin, ni appareil.

use std::sync::Mutex;
use std::time::Instant;

static DERNIERE_REQUETE: Mutex<Option<Instant>> = Mutex::new(None);
static DERNIERE_DEMANDE_DE_FLUX: Mutex<Option<(Instant, String)>> = Mutex::new(None);

/// Le pont vient de relayer une requête d'API (`relay.request`).
pub fn noter_requete_relayee() {
    if let Ok(mut d) = DERNIERE_REQUETE.lock() {
        *d = Some(Instant::now());
    }
}

/// Le pont vient de relayer une demande de flux (`relay.stream_request`).
pub fn noter_demande_de_flux_relayee(stream_id: &str) {
    if let Ok(mut d) = DERNIERE_DEMANDE_DE_FLUX.lock() {
        *d = Some((Instant::now(), identifiant_court(stream_id)));
    }
}

/// Millisecondes depuis la dernière requête d'API relayée, `None` si aucune
/// depuis le démarrage.
pub fn depuis_la_derniere_requete_relayee_ms() -> Option<u64> {
    DERNIERE_REQUETE
        .lock()
        .ok()
        .and_then(|d| *d)
        .map(|t| t.elapsed().as_millis() as u64)
}

/// Millisecondes depuis la dernière demande de flux relayée, avec
/// l'identifiant court du flux demandé.
pub fn derniere_demande_de_flux_relayee() -> Option<(u64, String)> {
    DERNIERE_DEMANDE_DE_FLUX
        .lock()
        .ok()
        .and_then(|d| d.clone())
        .map(|(t, id)| (t.elapsed().as_millis() as u64, id))
}

/// Les 8 premiers caractères d'un identifiant de flux, extension retirée :
/// assez pour rapprocher deux lignes du même flux, sans recopier
/// l'identifiant entier.
pub fn identifiant_court(stream_id: &str) -> String {
    let sans_extension = stream_id.split('.').next().unwrap_or(stream_id);
    sans_extension.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l_identifiant_court_garde_huit_caracteres_sans_extension() {
        assert_eq!(identifiant_court("038e9337abcdef.flac"), "038e9337");
        assert_eq!(identifiant_court("abc"), "abc");
        assert_eq!(identifiant_court(""), "");
    }

    #[test]
    fn les_instants_notes_se_relisent() {
        noter_requete_relayee();
        noter_demande_de_flux_relayee("0123456789.wav");
        assert!(depuis_la_derniere_requete_relayee_ms().is_some_and(|ms| ms < 60_000));
        let (ms, id) = derniere_demande_de_flux_relayee().expect("demande notée");
        assert!(ms < 60_000);
        assert_eq!(id, "01234567");
    }
}
