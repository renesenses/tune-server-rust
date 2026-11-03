//! Reprise bornée d'une ouverture ALSA matérielle refusée.
//!
//! Le rapport de https://github.com/renesenses/tune-server-rust/issues/7207
//! montre un refus à 176,4 kHz puis une ouverture réussie trois secondes plus
//! tard. L'appelant conserve le périphérique, les configurations et les données
//! de la piste ; seule la tentative d'ouverture est rejouée. Aucun rendu ici.

use std::time::Duration;

/// Seuls les PCM matériels ALSA sont concernés par le refus observé.
/// On regarde l'endpoint réellement résolu, pas le nom demandé à la zone.
pub(super) fn supports_open_retry(host: &str, endpoint: &str) -> bool {
    host == "Alsa" && endpoint.starts_with("alsa:hw:")
}

/// `None` signifie un arrêt demandé pendant l'attente ; il ne faut alors
/// publier aucune panne. Le budget de sommeil est de trois secondes, hors
/// durée des appels au pilote. Une sortie définitivement refusée reste une erreur.
/// `wait(0)` vérifie aussi l'annulation avant la première ouverture.
pub(super) fn retry_open<T, E>(
    retry_enabled: bool,
    mut open: impl FnMut() -> Result<T, E>,
    mut wait: impl FnMut(Duration) -> bool,
) -> Option<Result<T, E>> {
    let mut delays = [100, 200, 400, 800, 1500].into_iter();
    if !wait(Duration::ZERO) {
        return None;
    }
    loop {
        match open() {
            Ok(output) => return Some(Ok(output)),
            Err(error) => {
                let next = if retry_enabled { delays.next() } else { None };
                let Some(ms) = next else {
                    return Some(Err(error));
                };
                if !wait(Duration::from_millis(ms)) {
                    return None;
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "open_retry_7207_tests.rs"]
mod tests;
