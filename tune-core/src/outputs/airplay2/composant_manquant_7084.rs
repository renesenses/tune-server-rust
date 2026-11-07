//! #7084 — le composant AirPlay 2 (`airplay-daemon`) manque : le DIRE.
//!
//! Sous Windows, un antivirus peut mettre `airplay-daemon.exe` en quarantaine.
//! Tune servait alors l'enceinte AirPlay 2 par sa sortie AirPlay 1 sans rien
//! en dire hors du journal ; un appareil qui refuse AirPlay 1 (Devialet
//! Mania) répondait 403, et l'erreur parlait d'appairage HomeKit.
//!
//! Ce module tient la liste des appareils servis sans leur composant, que la
//! fiche de zone publie, et le message unique que la lecture rend.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

/// Code stable : l'appareil exige AirPlay 2 et `airplay-daemon` est absent.
pub const CODE_AIRPLAY2_COMPONENT_MISSING: &str = "airplay2_component_missing";

/// Message rendu par la lecture et publié sur la zone.
pub fn message_composant_manquant() -> String {
    format!(
        "{CODE_AIRPLAY2_COMPONENT_MISSING}: AirPlay 2 component missing: airplay-daemon was not \
         found next to Tune, an antivirus may have quarantined or deleted it. This speaker needs \
         AirPlay 2, so Tune cannot play on it until the component is back: restore the file or \
         add an antivirus exclusion for the Tune folder, then reinstall Tune and restart it"
    )
}

/// Appareils (identifiant de sortie) servis sans leur composant AirPlay 2.
static APPAREILS_SANS_COMPOSANT: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Note que `device_id` est servi sans son composant AirPlay 2.
pub fn signaler(device_id: &str) {
    if let Ok(mut appareils) = APPAREILS_SANS_COMPOSANT.lock() {
        appareils.insert(device_id.to_string());
    }
}

/// Le composant est de retour pour `device_id`.
pub fn retablir(device_id: &str) {
    if let Ok(mut appareils) = APPAREILS_SANS_COMPOSANT.lock() {
        appareils.remove(device_id);
    }
}

/// `device_id` est-il servi sans son composant AirPlay 2 ?
pub fn manque_pour(device_id: &str) -> bool {
    APPAREILS_SANS_COMPOSANT
        .lock()
        .map(|appareils| appareils.contains(device_id))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_message_nomme_le_composant_et_l_antivirus() {
        let message = message_composant_manquant();
        assert!(message.starts_with(CODE_AIRPLAY2_COMPONENT_MISSING));
        assert!(message.contains("AirPlay 2 component missing"));
        assert!(message.contains("airplay-daemon"));
        assert!(message.contains("antivirus"));
    }

    #[test]
    fn un_appareil_signale_puis_retabli() {
        let id = "airplay-composant-manquant-7084-test";
        assert!(!manque_pour(id));
        signaler(id);
        assert!(manque_pour(id));
        retablir(id);
        assert!(!manque_pour(id));
    }
}
