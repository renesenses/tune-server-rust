//! Rendre au noyau les pages libres que l'allocateur garde (`malloc_trim`).
//!
//! Le geste et sa mesure sont décrits dans `tune-server/src/memoire_a_froid.rs`,
//! qui le joue toutes les cinq minutes. Il vit ici, dans `tune-core`, pour que
//! la passe ReplayGain et la passe acoustique puissent le jouer AUSSITÔT après
//! une tâche lourde — sans attendre le prochain relevé (fuite CLAP du .18,
//! 08/10 ; #8609).

use std::sync::atomic::{AtomicBool, Ordering};

/// Une purge à la fois (#8609) : le relevé de cinq minutes, la fin d'un lot
/// ReplayGain et le relâchement d'une session ONNX peuvent tomber ensemble.
/// Deux `malloc_trim` simultanés se disputeraient les mêmes verrous d'arène
/// pour rien : le second passe son tour.
static PURGE_EN_COURS: AtomicBool = AtomicBool::new(false);

/// Rend au noyau les pages libres que l'allocateur garde. `true` quand
/// l'allocateur DIT en avoir rendu — ce qui ne prouve rien sous `mlockall`
/// (voir [`purger`]) ; `false` s'il n'y avait rien à rendre, si une autre
/// purge tourne déjà, ou hors glibc, où rien n'est fait.
///
/// ⚠️ Tient le verrou de chaque arène pendant qu'il la parcourt : un fil qui
/// ALLOUE à cet instant attend (de 0 à quelques dizaines de millisecondes).
/// Jamais depuis le fil async, ni depuis un rappel de rendu audio.
///
/// # Pendant une lecture LOCALE (#8609)
///
/// Il n'attend que les fils qui allouent ou libèrent. Les rappels de rendu
/// local n'en font aucun : ils lisent l'anneau dans le tampon que leur tend le
/// pilote, et des gardes comptent leurs appels à l'allocateur
/// (`outputs::local::ringbuf_tests` : rappels partagés `f32` et entier, rappel
/// natif S32 ; `sortie_double_locale` : la recopie vers la seconde sortie).
/// Le fil ALSA de cpal travaille dans des tampons faits à l'ouverture. Le
/// producteur, lui, alloue, mais il a deux secondes d'anneau devant lui.
/// Mesuré sur Shrek (banc `banc_trim_pendant_lecture_8609`) : voir
/// `tune-server/src/memoire_a_froid.rs` et la PR #8614.
pub fn rendre_la_memoire_liberee() -> bool {
    let Some(_tour) = TourDePurge::prendre() else {
        return false;
    };
    malloc_trim_brut()
}

/// Le tour de purge, rendu à sa sortie de portée (même sur panique).
struct TourDePurge;

impl TourDePurge {
    fn prendre() -> Option<Self> {
        (!PURGE_EN_COURS.swap(true, Ordering::AcqRel)).then_some(Self)
    }
}

impl Drop for TourDePurge {
    fn drop(&mut self) {
        PURGE_EN_COURS.store(false, Ordering::Release);
    }
}

fn malloc_trim_brut() -> bool {
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

/// `mallinfo2`, résolu À L'EXÉCUTION (`dlsym`) et non à l'édition des liens :
/// il n'existe que depuis la glibc 2.33, et la cible
/// `aarch64-unknown-linux-gnu` est construite par `cross` dans un conteneur
/// plus ancien. Un appel direct ferait échouer l'édition des liens (ou le
/// chargement du binaire sur une glibc < 2.33). Absent : `None`.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn mallinfo2_resolu() -> Option<unsafe extern "C" fn() -> libc::mallinfo2> {
    static FN: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let adresse = *FN.get_or_init(|| {
        // SAFETY : `dlsym` sur `RTLD_DEFAULT` avec un nom C terminé par NUL ;
        // il rend NULL si le symbole n'existe pas.
        unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"mallinfo2".as_ptr()) as usize }
    });
    if adresse == 0 {
        return None;
    }
    // SAFETY : l'adresse est celle de `mallinfo2`, dont la signature glibc
    // est `struct mallinfo2 mallinfo2(void)`.
    Some(unsafe {
        std::mem::transmute::<usize, unsafe extern "C" fn() -> libc::mallinfo2>(adresse)
    })
}

/// Lit l'état du tas. `None` hors glibc, ou glibc < 2.33 (pas de `mallinfo2`).
///
/// ⚠️ `mallinfo2` prend, lui aussi, le verrou de chaque arène le temps de
/// compter ses listes libres : hors du fil async, comme la purge, et une
/// fois par relevé.
pub fn etat_du_tas() -> Option<EtatDuTas> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        let f = mallinfo2_resolu()?;
        // SAFETY : voir `mallinfo2_resolu` ; aucune précondition.
        let m = unsafe { f() };
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

/// RSS du processus en kilooctets (`/proc/self/statm`, pages de 4 Kio).
/// `None` hors Linux.
pub fn rss_kb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        Some(pages * 4)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// RSS du processus en mégaoctets. `None` hors Linux.
pub fn rss_mb() -> Option<u64> {
    rss_kb().map(|k| k / 1024)
}

/// Mémoire verrouillée du processus (`VmLck` de `/proc/self/status`), en
/// kilooctets. `None` hors Linux ou champ absent.
///
/// Sous `mlockall(MCL_FUTURE)` — Tune OS et le paquet `.deb` posent
/// `LimitMEMLOCK=infinity`, et le premier rendu local verrouille alors toute
/// la mémoire (#3206, `audio::ordonnancement_rt`) — le noyau REFUSE
/// `MADV_DONTNEED` sur les pages verrouillées : `malloc_trim` ne rend
/// presque rien, mais la glibc répond quand même « rendu ». D'où ce champ
/// dans chaque ligne de purge, et la levée du verrou autour d'elle
/// ([`purger`]).
pub fn memoire_verrouillee_kb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        status
            .lines()
            .find_map(|l| l.strip_prefix("VmLck:"))
            .and_then(|v| v.split_whitespace().next()?.parse().ok())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Une purge faite, et ce qu'elle a VRAIMENT rendu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeFaite {
    pub rss_avant_kb: Option<u64>,
    pub rss_apres_kb: Option<u64>,
    /// Ce que la glibc a répondu (`malloc_trim != 0`). Ne prouve rien : sous
    /// `mlockall`, elle répond « rendu » sans que le RSS bouge.
    pub glibc_dit_rendu: bool,
    /// Mémoire verrouillée (`VmLck`) au moment de la purge, en Ko.
    pub verrouillee_kb: Option<u64>,
    /// Mémoire verrouillée APRÈS la purge (verrou rétabli), en Ko.
    pub verrouillee_apres_kb: Option<u64>,
    /// Le verrou de Tune a été levé le temps de la purge (#8609).
    pub verrou_leve: bool,
    /// Le verrou n'a pas pu être rétabli (cause) : journalisé en erreur et
    /// retenté au prochain relevé.
    pub echec_du_reverrouillage: Option<String>,
    pub duree_ms: u64,
}

impl PurgeFaite {
    /// Ce que le RSS a perdu, en Mo (0 s'il n'a pas baissé ou est inconnu).
    pub fn rendu_mb(&self) -> u64 {
        match (self.rss_avant_kb, self.rss_apres_kb) {
            (Some(a), Some(b)) => a.saturating_sub(b) / 1024,
            _ => 0,
        }
    }

    /// `true` seulement si le RSS a BAISSÉ d'au moins 1 Mo.
    pub fn rendu(&self) -> bool {
        self.rendu_mb() > 0
    }

    /// Une partie de la mémoire est verrouillée (`mlockall` actif).
    pub fn sous_mlock(&self) -> bool {
        self.verrouillee_kb.is_some_and(|k| k > 0)
    }

    pub fn rss_avant_mb(&self) -> Option<u64> {
        self.rss_avant_kb.map(|k| k / 1024)
    }

    pub fn rss_apres_mb(&self) -> Option<u64> {
        self.rss_apres_kb.map(|k| k / 1024)
    }
}

/// Purge MESURÉE, sur le fil courant (jamais le fil async ni un rappel
/// audio). `None` hors glibc, ou si une autre purge tourne déjà.
///
/// Sous le verrou mémoire de Tune (`mlockall`, Tune OS et `.deb`), le verrou
/// est levé le temps de la purge puis rétabli avec les mêmes drapeaux
/// (`audio::ordonnancement_rt::purger_en_levant_le_verrou`) : sans cela, le
/// noyau refuse de reprendre la moindre page.
pub fn purger() -> Option<PurgeFaite> {
    if !cfg!(all(target_os = "linux", target_env = "gnu")) {
        return None;
    }
    let _tour = TourDePurge::prendre()?;
    crate::audio::ordonnancement_rt::reverrouiller_si_en_attente();
    let verrouillee_kb = memoire_verrouillee_kb();
    let rss_avant_kb = rss_kb();
    let debut = std::time::Instant::now();
    let (glibc_dit_rendu, verrou) = crate::audio::ordonnancement_rt::purger_en_levant_le_verrou(
        verrouillee_kb.is_some_and(|k| k > 0),
        malloc_trim_brut,
    );
    let duree_ms = debut.elapsed().as_millis() as u64;
    Some(PurgeFaite {
        rss_avant_kb,
        rss_apres_kb: rss_kb(),
        glibc_dit_rendu,
        verrouillee_kb,
        verrouillee_apres_kb: memoire_verrouillee_kb(),
        verrou_leve: verrou.leve,
        echec_du_reverrouillage: verrou.echec_du_retablissement,
        duree_ms,
    })
}

/// [`purger`] HORS du fil async, juste après une tâche lourde qui vient de
/// rendre sa mémoire à l'allocateur (fin d'un lot ReplayGain, session ONNX
/// relâchée), QUE QUELQUE CHOSE JOUE OU NON (#8609).
///
/// Avant #8609, ces purges n'avaient lieu que si rien ne jouait : un serveur
/// qui écoute sur sa carte son toute la journée ne rendait jamais rien
/// (505 Mo → 5 Go en dix heures, fil 2167). Voir [`rendre_la_memoire_liberee`]
/// pour ce que la lecture locale risque — rien de mesurable.
pub async fn rendre_hors_du_fil_async() -> Option<PurgeFaite> {
    if !cfg!(all(target_os = "linux", target_env = "gnu")) {
        return None;
    }
    tokio::task::spawn_blocking(purger).await.ok().flatten()
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
