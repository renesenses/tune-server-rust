//! Rendre au système la mémoire que l'allocateur garde après usage, à froid.
//!
//! # Ce qui a été mesuré
//!
//! Sous Linux, le binaire publié (`x86_64-unknown-linux-gnu`) alloue avec le
//! `malloc` de la glibc. Chaque fil qui alloue reçoit son arène (jusqu'à huit
//! par cœur), et une arène ne rend ses pages au noyau que lorsque le HAUT de
//! son tas est libre. Un pic passager — un scan, une rafale de vignettes, un
//! lot d'analyse — laisse donc derrière lui des pages libres, mais résidentes.
//!
//! Relevé sur Shrek le 05/10/2026, bibliothèque synthétique de 50 000 pistes,
//! `mallinfo2()` lu dans le processus au repos, après un scan, une rafale de
//! 866 requêtes de l'interface et un second scan :
//!
//! ```text
//! RSS 683 Mo, dont anonyme 560 Mo
//! arènes 566 Mo  =  en usage 32 Mo  +  libre 533 Mo
//! ```
//!
//! 94 % du tas résident était de la mémoire LIBRE, gardée par l'allocateur.
//! Rien ne fuit : la mémoire vivante reste à quelques dizaines de mégaoctets.
//! Mais le relevé `memory_diagnostics` (RSS) monte et ne redescend pas, et le
//! système voit un processus de plusieurs centaines de mégaoctets au repos.
//!
//! # Le geste
//!
//! `malloc_trim(0)` parcourt les arènes et rend au noyau les pages libres
//! (`madvise(MADV_DONTNEED)`), sans rien déplacer : aucune allocation vivante
//! n'est touchée. Mesuré sur le même scénario, au rythme du relevé (cinq
//! minutes) : de 0 à 59 ms par purge ; RSS au repos 666 Mo sans la purge,
//! 255 à 293 Mo avec (anonyme : 537 Mo contre 132 à 170 Mo).
//!
//! Il n'est fait qu'À FROID — aucune zone en lecture — parce qu'il tient le
//! verrou de chaque arène pendant qu'il la parcourt : un fil de lecture qui
//! allouerait à cet instant attendrait. Au repos, personne n'attend.
//!
//! Hors glibc (macOS, Windows, musl), la fonction ne fait rien : leurs
//! allocateurs ont leur propre politique, et ce relevé ne les concerne pas.

/// RSS du processus, en mégaoctets — la même lecture que `memory_diagnostics`
/// (`/proc/self/statm`, pages de 4 Kio). `None` hors Linux.
pub fn rss_mb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        Some(pages * 4 / 1024)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Rend au noyau les pages libres que l'allocateur garde. `true` quand
/// l'allocateur dit en avoir rendu ; `false` s'il n'y avait rien à rendre, ou
/// hors glibc, où rien n'est fait.
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

/// La purge n'a lieu qu'à froid : aucune zone ne joue.
pub fn purge_permise(une_zone_joue: bool) -> bool {
    !une_zone_joue
}
