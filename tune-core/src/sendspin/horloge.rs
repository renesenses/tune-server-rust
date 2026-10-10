//! L'horloge du SERVEUR Sendspin : monotone, en microsecondes (#3326).
//!
//! La spécification place tous les horodatages audio dans le domaine d'horloge
//! du serveur (`messaging.md` § *Clock Synchronization*) : `server/time`, les
//! `server_transmitted` de `stream/start` et `stream/clear`, et l'horodatage de
//! chaque morceau audio. Ils doivent donc venir de la MÊME horloge. Le pilote
//! de connexion et la sortie qui calcule les horodatages vivent dans deux
//! caisses : l'horloge est ici, partagée par les deux.
//!
//! Monotone (`Instant`), jamais l'heure murale : un réglage NTP en pleine
//! lecture déplacerait sinon toute la ligne de temps des enceintes.

use std::sync::OnceLock;
use std::time::Instant;

/// Microsecondes écoulées depuis la première lecture de l'horloge dans ce
/// processus. Ne revient jamais en arrière.
#[must_use]
pub fn maintenant_us() -> i64 {
    static ORIGINE: OnceLock<Instant> = OnceLock::new();
    let ecoule = ORIGINE.get_or_init(Instant::now).elapsed().as_micros();
    i64::try_from(ecoule).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i3326_horloge_monotone_et_en_microsecondes() {
        let a = maintenant_us();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = maintenant_us();
        assert!(a >= 0);
        // 5 ms dormies : au moins 5000 µs, et pas 5 (ce qui trahirait des ms).
        assert!(b - a >= 5_000, "écart {} µs", b - a);
        assert!(b - a < 5_000_000, "écart {} µs", b - a);
    }
}
