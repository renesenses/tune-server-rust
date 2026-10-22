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
