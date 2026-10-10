//! Rendre au noyau les pages libres que l'allocateur garde (`malloc_trim`).
//!
//! Le geste et sa mesure sont décrits dans `tune-server/src/memoire_a_froid.rs`,
//! qui le joue toutes les cinq minutes à froid. Il vit ici, dans `tune-core`,
//! pour que la passe acoustique puisse le jouer AUSSITÔT après avoir relâché sa
//! session ONNX — sans attendre le prochain relevé (fuite CLAP du .18, 08/10).

/// Rend au noyau les pages libres que l'allocateur garde. `true` quand
/// l'allocateur dit en avoir rendu ; `false` s'il n'y avait rien à rendre, ou
/// hors glibc, où rien n'est fait.
///
/// ⚠️ Tient le verrou de chaque arène pendant qu'il la parcourt : ne pas
/// l'appeler pendant qu'une sortie LOCALE joue.
pub fn rendre_la_memoire_liberee() -> bool {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY : `malloc_trim` n'a pas de précondition ; il prend lui-même
        // le verrou de chaque arène et ne déplace aucune allocation vivante.
        unsafe { libc::malloc_trim(0) != 0 }
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        false
    }
}
