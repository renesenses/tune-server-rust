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
//! Il n'était fait qu'À FROID (jusqu'à #8609, voir plus bas) parce qu'il
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
//! # Une sortie LOCALE qui joue ne l'empêche plus non plus (#8609, fil 2167)
//!
//! Le même testeur écoute désormais toute la journée sur sa carte son (ALSA,
//! DAC USB) : la purge ne tournait plus JAMAIS, et le serveur est passé de
//! 505 Mo à 5 Go en dix heures, par marches de +1,1 Go en cinq minutes
//! (passe ReplayGain, modèle texte CLAP chargé), sans une ligne
//! `memoire_rendue_a_froid`.
//!
//! La garde reposait sur une précaution, jamais mesurée : « un fil de lecture
//! qui allouerait attendrait ». Or `malloc_trim` n'attend que les fils qui
//! ALLOUENT ou LIBÈRENT, et le rappel de rendu local n'en fait aucun :
//! il copie l'anneau dans le tampon du pilote (`render_local_shared_*_callback`,
//! sans allocation depuis #2218), et le fil ALSA de cpal écrit dans des
//! tampons faits à l'ouverture. Le producteur (décodage) alloue, mais il a
//! deux secondes d'anneau devant lui, contre quelques dizaines de
//! millisecondes de purge.
//!
//! Mesuré sur Shrek (banc `banc_trim_pendant_lecture_8609`, rappel de rendu
//! RÉEL à la cadence d'une période de 11,6 ms, passe ReplayGain réelle sur
//! des WAV 32 bits / 96 kHz et rafales d'allocations type ONNX en parallèle,
//! purge toutes les 500 ms) : chiffres dans la PR #8609.
//!
//! Pendant une lecture locale, la purge ne se fait plus que si la mémoire
//! libre gardée en vaut la peine ([`SEUIL_LIBRE_PENDANT_LECTURE_MB`]) : à
//! froid, rien ne change.
//!
//! Hors glibc (macOS, Windows, musl), la fonction ne fait rien : leurs
//! allocateurs ont leur propre politique, et ce relevé ne les concerne pas.

/// RSS du processus, en mégaoctets — la même lecture que `memory_diagnostics`
/// (`/proc/self/statm`, pages de 4 Kio). `None` hors Linux.
pub fn rss_mb() -> Option<u64> {
    tune_core::memoire_rendue::rss_mb()
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

/// Mémoire libre gardée par les arènes (`mallinfo2().fordblks`) au-delà de
/// laquelle la purge a lieu MÊME pendant une lecture locale (#8609). En deçà,
/// le jeu n'en vaut pas la chandelle : on attend le prochain relevé.
pub const SEUIL_LIBRE_PENDANT_LECTURE_MB: u64 = 64;

/// Ce que décide le relevé de cinq minutes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionDePurge {
    /// Aucune sortie locale ne joue : purge, comme toujours.
    AFroid,
    /// Une sortie locale joue, et la mémoire libre gardée dépasse le seuil.
    PendantLectureLocale,
    /// Une sortie locale joue, peu de mémoire libre gardée : rien à gagner.
    Retenue,
}

impl DecisionDePurge {
    pub fn purge(self) -> bool {
        !matches!(self, Self::Retenue)
    }
}

/// Décision PURE. `libre_mb` : `mallinfo2().fordblks` ; inconnu (`None`),
/// la purge a lieu — elle ne coûte que quelques dizaines de millisecondes à
/// des fils qui ne sont pas le rendu.
pub fn decider_la_purge(une_sortie_locale_joue: bool, libre_mb: Option<u64>) -> DecisionDePurge {
    if !une_sortie_locale_joue {
        return DecisionDePurge::AFroid;
    }
    match libre_mb {
        Some(l) if l < SEUIL_LIBRE_PENDANT_LECTURE_MB => DecisionDePurge::Retenue,
        _ => DecisionDePurge::PendantLectureLocale,
    }
}

/// Le relevé de cinq minutes rend au système la mémoire libre gardée par
/// l'allocateur ([`decider_la_purge`]) et journalise `memoire_rendue_a_froid`.
/// Hors du fil async : `mallinfo2` et `malloc_trim` parcourent les arènes.
/// `None` hors glibc, ou quand la purge est retenue.
pub async fn purger_a_froid(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
) -> Option<(DecisionDePurge, tune_core::memoire_rendue::PurgeFaite)> {
    // Hors glibc, rien à rendre : ne pas payer la requête des zones.
    if !cfg!(all(target_os = "linux", target_env = "gnu")) {
        return None;
    }
    let backend = backend.clone();
    let mesure = tokio::task::spawn_blocking(move || {
        let locale = une_sortie_locale_joue(&backend);
        let tas = tune_core::memoire_rendue::etat_du_tas();
        let decision = decider_la_purge(locale, tas.map(|t| t.libre_mb));
        if !decision.purge() {
            return None;
        }
        let rss_avant_mb = rss_mb();
        let debut = std::time::Instant::now();
        let rendu = rendre_la_memoire_liberee();
        let duree_ms = debut.elapsed().as_millis() as u64;
        Some((
            decision,
            tas.map(|t| t.libre_mb),
            tune_core::memoire_rendue::PurgeFaite {
                rss_avant_mb,
                rss_apres_mb: rss_mb(),
                rendu,
                duree_ms,
            },
        ))
    })
    .await
    .ok()
    .flatten()?;
    let (decision, libre_avant_mb, purge) = mesure;
    if purge.rendu {
        tracing::info!(
            rss_avant_mb = purge.rss_avant_mb,
            rss_apres_mb = purge.rss_apres_mb,
            tas_libre_avant_mb = libre_avant_mb,
            duree_ms = purge.duree_ms,
            pendant_lecture_locale = decision == DecisionDePurge::PendantLectureLocale,
            "memoire_rendue_a_froid"
        );
    }
    Some((decision, purge))
}

/// Une sortie locale joue-t-elle ? Oui quand la zone est locale, quand
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
