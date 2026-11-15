//! #8647 (fil 2794) — la sortie OAAT sert-elle EN CE MOMENT un DSD natif ?
//!
//! La voie directe d'OAAT (`oaat/output.rs`) lit elle-même le `.dsf` et
//! l'expédie en `DSD_U8` quand le récepteur annonce `dsd_max_rate` et que le
//! multiple DSD du fichier ne le dépasse pas. L'orchestrateur, lui, a déjà
//! préparé un WAV (`oaat_needs_wav`) qui n'est alors jamais lu. Le chemin du
//! signal, qui ne voyait que cette préparation, affichait « DSD256 → WAV
//! 352 kHz/24 bits » pendant que le DAC recevait du DSD256 natif.
//!
//! Ce registre publie, par `device_id`, le drapeau `native_dsd_active` que
//! la sortie pose ELLE-MÊME au moment où elle part en DSD natif, et efface à
//! `stop()`. Le chemin du signal le lit au moment de se construire : c'est
//! un constat de la sortie, pas une déduction depuis le réglage `dsd_mode`.
//!
//! Hors `cfg(feature = "oaat")` : sans sortie OAAT, rien n'est inscrit et
//! [`est_actif`] rend `false` — le comportement d'avant. Une sortie détruite
//! ne laisse qu'une référence faible, qui ne répond plus.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

fn table() -> &'static Mutex<HashMap<String, Weak<AtomicBool>>> {
    static TABLE: OnceLock<Mutex<HashMap<String, Weak<AtomicBool>>>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// La sortie `device_id` publie son drapeau « DSD natif en cours ». Une
/// nouvelle inscription du même appareil remplace la précédente : c'est la
/// dernière sortie construite qui est enregistrée.
pub fn inscrire(device_id: &str, drapeau: &Arc<AtomicBool>) {
    if let Ok(mut t) = table().lock() {
        t.retain(|_, w| w.strong_count() > 0);
        t.insert(device_id.to_string(), Arc::downgrade(drapeau));
    }
}

/// Vrai si la sortie `device_id` sert en ce moment un DSD natif.
#[must_use]
pub fn est_actif(device_id: &str) -> bool {
    table()
        .lock()
        .ok()
        .and_then(|t| t.get(device_id).and_then(Weak::upgrade))
        .is_some_and(|d| d.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_appareil_inconnu_n_est_jamais_en_dsd_natif() {
        assert!(!est_actif("oaat:inconnu-8647"));
    }

    #[test]
    fn le_drapeau_de_la_sortie_est_lu_en_direct_8647() {
        let drapeau = Arc::new(AtomicBool::new(false));
        inscrire("oaat:direct-8647", &drapeau);
        assert!(!est_actif("oaat:direct-8647"));
        drapeau.store(true, Ordering::SeqCst);
        assert!(est_actif("oaat:direct-8647"));
        drapeau.store(false, Ordering::SeqCst);
        assert!(!est_actif("oaat:direct-8647"));
    }

    #[test]
    fn une_sortie_detruite_ne_repond_plus_8647() {
        let drapeau = Arc::new(AtomicBool::new(true));
        inscrire("oaat:detruite-8647", &drapeau);
        assert!(est_actif("oaat:detruite-8647"));
        drop(drapeau);
        assert!(!est_actif("oaat:detruite-8647"));
    }
}
