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
//! Il n'est fait qu'À FROID — aucune SORTIE LOCALE en lecture — parce qu'il
//! tient le verrou de chaque arène pendant qu'il la parcourt : un fil de
//! lecture qui allouerait à cet instant attendrait.
//!
//! # Une zone réseau qui joue n'empêche plus la purge (fil 2167)
//!
//! La première version refusait la purge dès qu'UNE zone jouait. Or une
//! écoute continue sur une zone réseau (DLNA, OpenHome, AirPlay…) est
//! justement le cas où le tas monte le plus : chaque piste y est décodée,
//! traitée et ré-encodée en entier, et ses fenêtres de niveaux sont gardées
//! le temps de la piste. Relevé de terrain : un serveur qui joue sans arrêt
//! vers une zone DLNA avec égaliseur passe de 449 Mo à 4,1 Go de RSS en
//! 2 h 40, et la purge ne s'est jamais déclenchée.
//!
//! Le renderer réseau tire un FICHIER ou un flux HTTP tamponné de plusieurs
//! secondes : quelques dizaines de millisecondes d'attente d'un fil du
//! serveur ne s'entendent pas. Seule une sortie LOCALE (la carte son de la
//! machine, son rappel temps réel) mérite qu'on ne la fasse pas attendre ;
//! elle seule bloque désormais la purge — y compris quand elle suit, dans un
//! groupe, une zone réseau qui joue. Banc du fil 2167 (zone réseau avec
//! égaliseur, pistes de 9 à 20 min) : jusqu'à 375 Mo d'anonyme entre deux
//! pistes sans la purge, 100 à 165 Mo avec ; la mémoire réellement en usage
//! reste de 95 à 156 Mo dans les deux cas.
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
///
/// Le geste vit dans `tune_core::memoire_rendue`, pour que la passe acoustique
/// puisse aussi le jouer juste après avoir relâché sa session ONNX.
pub fn rendre_la_memoire_liberee() -> bool {
    tune_core::memoire_rendue::rendre_la_memoire_liberee()
}

/// La purge n'a lieu qu'à froid : aucune sortie LOCALE ne joue (fil 2167).
pub fn purge_permise(une_sortie_locale_joue: bool) -> bool {
    !une_sortie_locale_joue
}

/// Une zone qui joue retient-elle la purge ? Oui quand elle est locale, quand
/// son type est inconnu, ou quand elle partage son groupe avec une zone
/// locale (une suiveuse locale peut ne pas porter elle-même `playing`).
/// SQL standard : la même requête sert SQLite et PostgreSQL.
pub const REQUETE_SORTIE_LOCALE_EN_LECTURE: &str = "SELECT p.id FROM zones p \
     WHERE p.last_play_state = 'playing' AND ( \
       p.output_type IS NULL OR p.output_type = '' OR p.output_type = 'local' \
       OR (p.group_id IS NOT NULL AND EXISTS ( \
         SELECT 1 FROM zones l WHERE l.group_id = p.group_id \
           AND (l.output_type IS NULL OR l.output_type = '' OR l.output_type = 'local'))) \
     ) LIMIT 1";

/// Vrai quand une sortie locale joue (voir [`REQUETE_SORTIE_LOCALE_EN_LECTURE`]).
/// Une requête qui échoue répond `true` : dans le doute, on ne purge pas.
pub fn une_sortie_locale_joue(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
) -> bool {
    match backend.query_one(REQUETE_SORTIE_LOCALE_EN_LECTURE, &[]) {
        Ok(ligne) => ligne.is_some(),
        Err(_) => true,
    }
}
