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
/// ⚠️ Tient le verrou de chaque arène pendant qu'il la parcourt : un fil qui
/// ALLOUE à cet instant attend (de 0 à quelques dizaines de millisecondes).
/// Jamais depuis le fil async, ni depuis un rappel de rendu audio.
///
/// # Pendant une lecture LOCALE (#8609)
///
/// Il n'attend que les fils qui allouent ou libèrent. Le rappel de rendu
/// local n'en fait aucun : il lit l'anneau dans le tampon que lui tend le
/// pilote (`render_local_shared_*_callback`, plus aucune allocation depuis
/// #2218, gardé par `outputs::local::ringbuf_tests` qui compte les appels à
/// l'allocateur), et le fil ALSA de cpal travaille dans des tampons faits à
/// l'ouverture. Le producteur, lui, alloue, mais il a deux secondes d'anneau
/// devant lui. Mesuré sur Shrek (banc `banc_trim_pendant_lecture_8609`) :
/// voir `tune-server/src/memoire_a_froid.rs`.
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

/// Ce que dit `mallinfo2()` du tas, en mégaoctets (#8609).
///
/// De quoi trancher, dans le journal, entre « libéré mais gardé par les
/// arènes » (`libre_mb` monte : `malloc_trim` le rend) et « vraiment en
/// usage » (`en_usage_mb` monte : une structure vivante grossit, et aucune
/// purge n'y peut rien).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EtatDuTas {
    /// Blocs vivants, tas des arènes ET blocs `mmap` (`uordblks + hblkhd`).
    pub en_usage_mb: u64,
    /// Blocs libres que les arènes gardent (`fordblks`) : ce que la purge vise.
    /// Résidents ou non : une page déjà rendue au noyau y reste comptée.
    pub libre_mb: u64,
    /// Dont blocs servis par `mmap` (`hblkhd`), rendus seuls à leur libération.
    pub mmap_mb: u64,
}

/// Lit l'état du tas. `None` hors glibc.
///
/// ⚠️ `mallinfo2` prend, lui aussi, le verrou de chaque arène le temps de
/// compter ses listes libres : hors du fil async, comme la purge.
pub fn etat_du_tas() -> Option<EtatDuTas> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY : `mallinfo2` n'a pas de précondition et rend une structure
        // par valeur.
        let m = unsafe { libc::mallinfo2() };
        const MO: usize = 1024 * 1024;
        Some(EtatDuTas {
            en_usage_mb: ((m.uordblks + m.hblkhd) / MO) as u64,
            libre_mb: (m.fordblks / MO) as u64,
            mmap_mb: (m.hblkhd / MO) as u64,
        })
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        None
    }
}

/// RSS du processus en mégaoctets (`/proc/self/statm`). `None` hors Linux.
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

/// Une purge faite, et ce qu'elle a rendu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PurgeFaite {
    pub rss_avant_mb: Option<u64>,
    pub rss_apres_mb: Option<u64>,
    /// L'allocateur dit avoir rendu des pages.
    pub rendu: bool,
    pub duree_ms: u64,
}

/// Purge HORS du fil async, juste après une tâche lourde qui vient de rendre
/// sa mémoire à l'allocateur (fin d'un lot ReplayGain, session ONNX relâchée),
/// QUE QUELQUE CHOSE JOUE OU NON (#8609). `None` hors glibc.
///
/// Avant #8609, ces purges n'avaient lieu que si rien ne jouait : un serveur
/// qui écoute sur sa carte son toute la journée ne rendait jamais rien
/// (505 Mo → 5 Go en dix heures, fil 2167). Voir [`rendre_la_memoire_liberee`]
/// pour ce que la lecture locale risque — rien de mesurable.
pub async fn rendre_hors_du_fil_async() -> Option<PurgeFaite> {
    if !cfg!(all(target_os = "linux", target_env = "gnu")) {
        return None;
    }
    tokio::task::spawn_blocking(|| {
        let rss_avant_mb = rss_mb();
        let debut = std::time::Instant::now();
        let rendu = rendre_la_memoire_liberee();
        PurgeFaite {
            rss_avant_mb,
            rss_apres_mb: rss_mb(),
            rendu,
            duree_ms: debut.elapsed().as_millis() as u64,
        }
    })
    .await
    .ok()
}

// ---------------------------------------------------------------------------
// #6168 — le seuil `mmap` de la glibc, fixé au démarrage.
// ---------------------------------------------------------------------------
//
// # Ce qui a été mesuré
//
// La glibc sert une allocation par `mmap` au-delà d'un seuil (128 Kio au
// départ), et la rend au noyau dès sa libération. Mais ce seuil est DYNAMIQUE :
// libérer un bloc `mmap` de taille T relève le seuil à T (jusqu'à 32 Mio).
// Les tampons suivants de cette taille partent alors dans le tas des arènes,
// et un tas d'arène ne rend ses pages que lorsque son SOMMET est libre. Les
// tampons de décodage de l'analyse ReplayGain (24/96 : 23 Mio d'`i32` par
// segment de 30 s) finissent ainsi résidents, une arène après l'autre :
// c'est la mémoire « libre mais gardée » que `malloc_trim` rend toutes les
// cinq minutes (fil 2231 : 8,6 Go → 4,1 Go, puis +4,5 Go en cinq minutes).
//
// # Le geste
//
// `mallopt(M_MMAP_THRESHOLD, 4 Mio)`. FIXER le seuil DÉSACTIVE son ajustement
// dynamique (et celui de `M_TRIM_THRESHOLD`, qui le suit) : toute allocation
// de 4 Mio ou plus passe par `mmap` et retourne au noyau à sa libération,
// quel que soit l'historique du processus. Les petites allocations ne
// changent pas de chemin.
//
// Linux + glibc uniquement : musl, macOS et Windows ont leur propre politique,
// et rien n'y est fait.
//
// # Le couper
//
// `TUNE_GLIBC_MMAP_THRESHOLD=0` rend le comportement d'origine (seuil
// dynamique). Une valeur positive, en octets, remplace 4 Mio. Une variable
// glibc déjà posée par l'exploitant (`MALLOC_MMAP_THRESHOLD_`) est respectée.

/// Variable d'environnement qui règle (ou coupe, avec `0`) le seuil.
pub const VARIABLE_SEUIL_MMAP: &str = "TUNE_GLIBC_MMAP_THRESHOLD";

/// Seuil fixé par défaut : 4 Mio.
pub const SEUIL_MMAP_PAR_DEFAUT: usize = 4 * 1024 * 1024;

/// Ce que demande la configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeuilDemande {
    /// Fixer le seuil à ce nombre d'octets.
    Fixe(usize),
    /// `TUNE_GLIBC_MMAP_THRESHOLD=0` : ne rien toucher.
    Coupe,
}

/// Lecture PURE de `TUNE_GLIBC_MMAP_THRESHOLD`.
///
/// Absente, vide ou illisible : le défaut (4 Mio). `0` : coupé. Un entier
/// positif : ce seuil, en octets.
pub fn seuil_demande(valeur: Option<&str>) -> SeuilDemande {
    match valeur.map(str::trim) {
        Some("0") => SeuilDemande::Coupe,
        Some(v) if !v.is_empty() => match v.parse::<usize>() {
            Ok(n) if n > 0 && n <= i32::MAX as usize => SeuilDemande::Fixe(n),
            _ => SeuilDemande::Fixe(SEUIL_MMAP_PAR_DEFAUT),
        },
        _ => SeuilDemande::Fixe(SEUIL_MMAP_PAR_DEFAUT),
    }
}

/// Ce qui a été fait au démarrage, pour la ligne de journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueSeuilMmap {
    /// Seuil fixé à ce nombre d'octets.
    Applique(usize),
    /// Coupé par `TUNE_GLIBC_MMAP_THRESHOLD=0` : seuil dynamique d'origine.
    Coupe,
    /// `MALLOC_MMAP_THRESHOLD_` déjà posé par l'exploitant : laissé tel quel.
    LaisseALExploitant,
    /// La glibc a refusé la valeur (au-delà de son maximum).
    Refuse(usize),
    /// Pas la glibc (musl, macOS, Windows) : rien n'est fait.
    HorsGlibc,
}

impl IssueSeuilMmap {
    /// Le libellé de la ligne de journal.
    pub fn libelle(self) -> String {
        match self {
            Self::Applique(n) => format!("fixe a {n} octets (ajustement dynamique desactive)"),
            Self::Coupe => format!("coupe par {VARIABLE_SEUIL_MMAP}=0 (seuil dynamique glibc)"),
            Self::LaisseALExploitant => "MALLOC_MMAP_THRESHOLD_ deja pose : laisse tel quel".into(),
            Self::Refuse(n) => format!("refuse par la glibc ({n} octets) : seuil dynamique"),
            Self::HorsGlibc => "hors glibc : rien a faire".into(),
        }
    }
}

/// Applique la demande (Linux + glibc), sans lire l'environnement.
pub fn appliquer_le_seuil_mmap(demande: SeuilDemande) -> IssueSeuilMmap {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        match demande {
            SeuilDemande::Coupe => IssueSeuilMmap::Coupe,
            SeuilDemande::Fixe(n) => {
                // SAFETY : `mallopt` n'a pas de précondition ; il prend le
                // verrou de l'arène principale et ne touche aucune
                // allocation vivante. `n <= i32::MAX` (voir `seuil_demande`).
                if unsafe { libc::mallopt(libc::M_MMAP_THRESHOLD, n as libc::c_int) } == 1 {
                    IssueSeuilMmap::Applique(n)
                } else {
                    IssueSeuilMmap::Refuse(n)
                }
            }
        }
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        let _ = demande;
        IssueSeuilMmap::HorsGlibc
    }
}

/// Fixe le seuil UNE fois par processus, d'après l'environnement, et rend ce
/// qui a été fait. Les appels suivants rendent la même issue sans rien
/// refaire : le moteur l'appelle avant de démarrer ses fils, `run` le relit
/// pour le journal (et l'applique si un binaire composeur n'est pas passé par
/// le moteur).
pub fn fixer_le_seuil_mmap() -> IssueSeuilMmap {
    static ISSUE: std::sync::OnceLock<IssueSeuilMmap> = std::sync::OnceLock::new();
    *ISSUE.get_or_init(|| {
        if !cfg!(all(target_os = "linux", target_env = "gnu")) {
            return IssueSeuilMmap::HorsGlibc;
        }
        if std::env::var_os("MALLOC_MMAP_THRESHOLD_").is_some() {
            return IssueSeuilMmap::LaisseALExploitant;
        }
        appliquer_le_seuil_mmap(seuil_demande(
            std::env::var(VARIABLE_SEUIL_MMAP).ok().as_deref(),
        ))
    })
}

#[cfg(test)]
mod tests_seuil_mmap {
    use super::*;

    #[test]
    fn la_variable_absente_ou_vide_donne_le_defaut() {
        assert_eq!(
            seuil_demande(None),
            SeuilDemande::Fixe(SEUIL_MMAP_PAR_DEFAUT)
        );
        assert_eq!(
            seuil_demande(Some("")),
            SeuilDemande::Fixe(SEUIL_MMAP_PAR_DEFAUT)
        );
        assert_eq!(
            seuil_demande(Some("  ")),
            SeuilDemande::Fixe(SEUIL_MMAP_PAR_DEFAUT)
        );
    }

    #[test]
    fn zero_coupe() {
        assert_eq!(seuil_demande(Some("0")), SeuilDemande::Coupe);
        assert_eq!(seuil_demande(Some(" 0 ")), SeuilDemande::Coupe);
    }

    #[test]
    fn un_entier_positif_remplace_le_defaut() {
        assert_eq!(
            seuil_demande(Some("1048576")),
            SeuilDemande::Fixe(1_048_576)
        );
    }

    #[test]
    fn une_valeur_illisible_garde_le_defaut() {
        for v in ["abc", "-5", "4Mo", "99999999999999999999"] {
            assert_eq!(
                seuil_demande(Some(v)),
                SeuilDemande::Fixe(SEUIL_MMAP_PAR_DEFAUT),
                "{v}"
            );
        }
    }

    #[test]
    fn coupe_ne_touche_a_rien() {
        let attendu = if cfg!(all(target_os = "linux", target_env = "gnu")) {
            IssueSeuilMmap::Coupe
        } else {
            IssueSeuilMmap::HorsGlibc
        };
        assert_eq!(appliquer_le_seuil_mmap(SeuilDemande::Coupe), attendu);
    }

    /// Hors glibc, rien n'est fait, quelle que soit la demande.
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    #[test]
    fn hors_glibc_rien_n_est_fait() {
        assert_eq!(
            appliquer_le_seuil_mmap(SeuilDemande::Fixe(SEUIL_MMAP_PAR_DEFAUT)),
            IssueSeuilMmap::HorsGlibc
        );
        assert_eq!(fixer_le_seuil_mmap(), IssueSeuilMmap::HorsGlibc);
    }
}
