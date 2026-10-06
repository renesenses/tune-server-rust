//! #3206 — l'ordonnancement temps réel du fil de rendu de la sortie locale.
//!
//! ## Le fait
//!
//! Le fil que cpal consacre au rendu ALSA tournait en `SCHED_OTHER`, comme le
//! scanner ou le serveur HTTP : `sched_setscheduler`, `SCHED_FIFO` et
//! `pthread_setschedparam` n'apparaissaient nulle part dans le dépôt. Tune OS
//! pose pourtant `LimitRTPRIO=95` sur l'unité et `rtprio 95` dans
//! `limits.d` — une couche que personne ne consommait.
//!
//! ## Ce que ce module fait, et ce qu'il refuse de faire
//!
//! [`demander_pour_le_fil_courant`] demande `SCHED_FIFO` pour le fil qui
//! l'appelle, à une priorité **bornée** par la limite douce `RLIMIT_RTPRIO`
//! ([`priorite_bornee`]). Quand le noyau refuse — limite à zéro sans
//! `CAP_SYS_NICE`, conteneur bridé — le fil reste en `SCHED_OTHER` et la
//! fonction rend la cause : **jamais une panique, jamais un refus de lecture**.
//! Le son sort exactement comme avant ; seule la ligne de journal change.
//!
//! La décision de priorité est pure et vit hors de `local-audio`, comme la
//! période de #3208 : elle se juge dans la porte `test` de la CI, qui ne
//! compile pas cpal. Seul l'appel au noyau est Linux.
//!
//! Hors Linux, la fonction rend [`OrdonnancementTempsReel::SansObjet`] :
//! CoreAudio sert déjà son rappel depuis un fil à contrainte temporelle, et
//! WASAPI n'est pas l'objet de ce ticket.
//!
//! ## Lot audio-rt (b209) : réglable, dit une fois, mémoire verrouillée
//!
//! - La priorité se règle par `TUNE_AUDIO_RT_PRIORITY` ([`lire_reglage`]) :
//!   1 à 99, ou `0`/`off` pour ne rien demander. Défaut : [`PRIORITE_VISEE`].
//! - Le verdict se journalise **une fois par processus** ([`premiere_fois`]) ;
//!   un fil de rendu neuf (changement de format, réouverture) qui obtient le
//!   même verdict ne répète plus la ligne.
//! - `mlockall(MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT)` n'est tenté que si
//!   `RLIMIT_MEMLOCK` est illimitée ([`decider_le_verrouillage`]) : sous une
//!   limite finie, `MCL_FUTURE` ferait échouer les allocations suivantes — un
//!   `abort` du serveur. `MCL_ONFAULT` ne verrouille que les pages touchées :
//!   rien n'est préchargé. `TUNE_AUDIO_MLOCK=0` le désactive.
//! - [`fiche`] rend le tout pour `/system/diagnostics` (`audio_realtime`),
//!   même avant toute lecture et même sans `local-audio`.
//!
//! ⚠️ Ce qu'aucune épreuve ne peut établir ici : l'effet sur les xruns d'un
//! DAC réel. La machine de compilation n'a ni carte son ni droit temps réel
//! (`ulimit -r` = 0) ; le chemin « obtenu » n'y est prouvé que par la décision
//! pure et par la lecture de la politique quand l'environnement l'accorde.

use serde::Serialize;

/// Priorité `SCHED_FIFO` visée quand la limite le permet.
///
/// Pourquoi 70 : au-dessus des gestionnaires d'interruption filés du noyau
/// (50 par défaut sous `threadirqs`) et de ce que demandent PulseAudio ou
/// JACK, pour que la période audio ne soit pas coupée par une interruption
/// réseau ou disque ; en dessous des 88 de PipeWire et surtout du plafond 95
/// posé par Tune OS, qui laisse à un `rtirq` la place de hisser le fil
/// d'interruption USB du DAC **au-dessus** du rendu — c'est l'ordre attendu :
/// le pilote avant le producteur. Jamais 99 : c'est la bande des fils de
/// migration et de chien de garde du noyau.
pub const PRIORITE_VISEE: u32 = 70;
// La cible reste sous le plafond de Tune OS (`LimitRTPRIO=95`) et au-dessus des
// IRQ filées du noyau (50) : jugé à la compilation.
const _: () = assert!(PRIORITE_VISEE < 95 && PRIORITE_VISEE > 50);

/// Nom de la politique demandée, tel qu'il paraît dans le journal et l'état.
pub const POLITIQUE: &str = "SCHED_FIFO";

/// Ce qu'a rendu la demande d'ordonnancement temps réel du fil de rendu.
///
/// Sérialisé tel quel dans `LocalBackendStatus.realtime` : un écran ou un
/// rapport de bogue lit `state`, puis la priorité ou la cause.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum OrdonnancementTempsReel {
    /// Le noyau a accepté : le fil tourne sous `policy` à `priority`.
    Obtenu {
        policy: &'static str,
        priority: u32,
        /// La limite douce `RLIMIT_RTPRIO` lue au moment de la demande
        /// (`None` = illimitée).
        rlimit_rtprio: Option<u32>,
    },
    /// Le noyau a refusé ; le fil est resté en `SCHED_OTHER`.
    Refuse {
        /// La priorité qui avait été demandée.
        priority: u32,
        rlimit_rtprio: Option<u32>,
        /// L'erreur du noyau, en clair.
        cause: String,
    },
    /// Rien n'a été demandé : `TUNE_AUDIO_RT_PRIORITY=0`.
    Desactive,
    /// Plateforme où la question ne se pose pas (ni Linux ni ALSA).
    SansObjet,
}

impl OrdonnancementTempsReel {
    /// `true` quand le fil tourne réellement en temps réel.
    pub fn obtenu(&self) -> bool {
        matches!(self, Self::Obtenu { .. })
    }
}

/// La priorité à demander, bornée par la limite douce `RLIMIT_RTPRIO`
/// (`None` = `RLIM_INFINITY`) et par le maximum de la politique.
///
/// Une limite à **zéro** ne fait pas renoncer : c'est le cas de `root`, à qui
/// le noyau accorde `SCHED_FIFO` sans regarder la limite — l'unité des images
/// Tune OS tourne ainsi. Sans ce droit, le noyau répondra `EPERM` et la
/// cause le dira ; c'est lui qui tranche, pas nous.
pub fn priorite_bornee(visee: u32, limite_douce: Option<u32>, max_de_la_politique: u32) -> u32 {
    let plafond = match limite_douce {
        Some(0) | None => max_de_la_politique,
        Some(l) => l.min(max_de_la_politique),
    };
    visee.min(plafond).max(1)
}

/// Variable d'environnement qui règle la priorité demandée.
pub const VARIABLE_PRIORITE: &str = "TUNE_AUDIO_RT_PRIORITY";
/// Variable d'environnement qui désactive le verrouillage mémoire (`0`/`off`).
pub const VARIABLE_MLOCK: &str = "TUNE_AUDIO_MLOCK";

/// Le réglage lu dans [`VARIABLE_PRIORITE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Reglage {
    /// Demander `SCHED_FIFO` à cette priorité (avant bornage par la limite).
    Priorite {
        priority: u32,
        /// `default`, `env`, ou `invalid` quand la valeur lue a été écartée.
        source: &'static str,
    },
    /// `0` ou `off` : rien n'est demandé, le fil reste en `SCHED_OTHER`.
    Desactive,
}

fn vaut_non(v: &str) -> bool {
    matches!(
        v.to_ascii_lowercase().as_str(),
        "0" | "off" | "false" | "no" | "non"
    )
}

/// Lit le réglage de priorité. Une valeur illisible ou hors de 1..=99 ne fait
/// jamais échouer : elle retombe sur [`PRIORITE_VISEE`], marquée `invalid`.
pub fn lire_reglage(valeur: Option<&str>) -> Reglage {
    let defaut = |source| Reglage::Priorite {
        priority: PRIORITE_VISEE,
        source,
    };
    let Some(v) = valeur.map(str::trim).filter(|v| !v.is_empty()) else {
        return defaut("default");
    };
    if vaut_non(v) {
        return Reglage::Desactive;
    }
    match v.parse::<u32>() {
        Ok(p @ 1..=99) => Reglage::Priorite {
            priority: p,
            source: "env",
        },
        _ => defaut("invalid"),
    }
}

/// La décision, pure : quelle priorité demander au noyau, ou `None` pour ne
/// rien demander. Entrées : le réglage, la limite douce `RLIMIT_RTPRIO`
/// (`None` = illimitée) et le maximum de `SCHED_FIFO`.
pub fn decider(
    reglage: Reglage,
    limite_douce: Option<u32>,
    max_de_la_politique: u32,
) -> Option<u32> {
    match reglage {
        Reglage::Desactive => None,
        Reglage::Priorite { priority, .. } => {
            Some(priorite_bornee(priority, limite_douce, max_de_la_politique))
        }
    }
}

/// La décision du verrouillage mémoire, pure : `Ok(())` pour tenter
/// `mlockall`, sinon la raison de l'écarter.
///
/// `limite_memlock` est la limite douce `RLIMIT_MEMLOCK` en octets (`None` =
/// illimitée). Seule une limite illimitée autorise : même root, que le noyau
/// laisserait dépasser la limite, n'est verrouillé que si l'administrateur
/// l'a voulu (Tune OS pose `LimitMEMLOCK=infinity`).
pub fn decider_le_verrouillage(
    reglage_mlock: Option<&str>,
    limite_memlock: Option<u64>,
) -> Result<(), String> {
    if reglage_mlock.map(str::trim).is_some_and(vaut_non) {
        return Err(format!("désactivé par {VARIABLE_MLOCK}"));
    }
    match limite_memlock {
        None => Ok(()),
        Some(n) => Err(format!(
            "RLIMIT_MEMLOCK limitée à {n} octets : MCL_FUTURE ferait échouer les allocations suivantes"
        )),
    }
}

/// Ce qu'a rendu le verrouillage mémoire du processus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum VerrouillageMemoire {
    /// `mlockall` a réussi avec ces drapeaux.
    Verrouille { flags: &'static str },
    /// Non tenté, et pourquoi.
    Ecarte { reason: String },
    /// Tenté, refusé par le noyau.
    Refuse { cause: String },
    /// Plateforme où la question ne se pose pas.
    SansObjet,
}

/// Le verdict ne se journalise qu'à sa première occurrence dans le processus :
/// `true` si `issue` diffère du dernier verdict journalisé, qui devient alors
/// `issue`. Un verdict qui CHANGE (obtenu puis refusé) se journalise donc
/// encore ; la même ligne répétée à chaque réouverture du flux, non.
pub fn premiere_fois(issue: &OrdonnancementTempsReel) -> bool {
    premiere_fois_dans(&DEJA_JOURNALISE, issue)
}

static DEJA_JOURNALISE: std::sync::Mutex<Option<OrdonnancementTempsReel>> =
    std::sync::Mutex::new(None);

fn premiere_fois_dans(
    memoire: &std::sync::Mutex<Option<OrdonnancementTempsReel>>,
    issue: &OrdonnancementTempsReel,
) -> bool {
    let mut dernier = memoire.lock().unwrap_or_else(|e| e.into_inner());
    if dernier.as_ref() == Some(issue) {
        return false;
    }
    *dernier = Some(issue.clone());
    true
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{OrdonnancementTempsReel, POLITIQUE, Reglage, VerrouillageMemoire, decider};

    /// `sched_param` à la priorité donnée, les autres champs à zéro. Jamais
    /// par littéral : la structure de musl porte quatre champs de plus
    /// (`sched_ss_*`) que celle de glibc, et le littéral ne compile que sur
    /// glibc — la v0.9.157 a échoué sur `aarch64-unknown-linux-musl`.
    fn sched_param(priority: libc::c_int) -> libc::sched_param {
        // SAFETY : `sched_param` est une structure C de simples entiers, pour
        // laquelle le tout-zéro est une valeur valide.
        let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
        param.sched_priority = priority;
        param
    }

    /// Limite douce `RLIMIT_RTPRIO` du processus ; `None` = illimitée.
    pub fn limite_rtprio() -> Option<u32> {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY : `lim` est un `rlimit` valide et initialisé ; getrlimit ne
        // fait qu'y écrire.
        let rc = unsafe { libc::getrlimit(libc::RLIMIT_RTPRIO, &mut lim) };
        if rc != 0 || lim.rlim_cur == libc::RLIM_INFINITY {
            return None;
        }
        Some(lim.rlim_cur.min(u32::MAX as libc::rlim_t) as u32)
    }

    /// Politique et priorité du fil courant, telles que le noyau les tient —
    /// la lecture qui fait le témoin, jamais appelée hors des épreuves.
    #[cfg(test)]
    pub fn politique_du_fil_courant() -> (libc::c_int, libc::c_int) {
        let mut politique: libc::c_int = 0;
        let mut param = sched_param(0);
        // SAFETY : les deux sorties sont des valeurs locales valides ;
        // pthread_self() est toujours un fil vivant.
        unsafe {
            libc::pthread_getschedparam(libc::pthread_self(), &mut politique, &mut param);
        }
        (politique, param.sched_priority)
    }

    /// Limite douce `RLIMIT_MEMLOCK` du processus en octets ; `None` = illimitée.
    pub fn limite_memlock() -> Option<u64> {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY : comme `limite_rtprio`.
        let rc = unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut lim) };
        if rc != 0 {
            return Some(0);
        }
        if lim.rlim_cur == libc::RLIM_INFINITY {
            return None;
        }
        Some(u64::from(lim.rlim_cur))
    }

    /// `true` si le processus tourne en root (euid 0).
    pub fn est_root() -> bool {
        // SAFETY : lecture sans effet de bord.
        unsafe { libc::geteuid() == 0 }
    }

    /// `mlockall(MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT)`.
    pub fn verrouiller() -> VerrouillageMemoire {
        // SAFETY : appel sans pointeur ; au pire le noyau refuse.
        let rc =
            unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE | libc::MCL_ONFAULT) };
        if rc == 0 {
            VerrouillageMemoire::Verrouille {
                flags: "MCL_CURRENT|MCL_FUTURE|MCL_ONFAULT",
            }
        } else {
            VerrouillageMemoire::Refuse {
                cause: std::io::Error::last_os_error().to_string(),
            }
        }
    }

    /// Demande `SCHED_FIFO` pour le fil courant, à la priorité décidée.
    pub fn demander(reglage: Reglage) -> OrdonnancementTempsReel {
        let rlimit_rtprio = limite_rtprio();
        // SAFETY : appel sans argument ni effet de bord.
        let max = unsafe { libc::sched_get_priority_max(libc::SCHED_FIFO) };
        let Some(priority) = decider(reglage, rlimit_rtprio, u32::try_from(max).unwrap_or(1))
        else {
            return OrdonnancementTempsReel::Desactive;
        };
        let param = sched_param(priority as libc::c_int);
        // SAFETY : `param` est valide le temps de l'appel ; pthread_self() est
        // le fil courant, qui ne peut pas disparaître pendant qu'il s'exécute.
        let rc =
            unsafe { libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param) };
        if rc == 0 {
            OrdonnancementTempsReel::Obtenu {
                policy: POLITIQUE,
                priority,
                rlimit_rtprio,
            }
        } else {
            // pthread_setschedparam rend l'errno directement, pas -1.
            OrdonnancementTempsReel::Refuse {
                priority,
                rlimit_rtprio,
                cause: std::io::Error::from_raw_os_error(rc).to_string(),
            }
        }
    }
}

/// Demande l'ordonnancement temps réel pour **le fil qui appelle**.
///
/// À appeler depuis le fil de rendu — celui du rappel cpal —, une fois, à sa
/// première période : sous ALSA, cpal crée ce fil lui-même et n'offre aucun
/// autre point d'entrée avant le premier rappel.
pub fn demander_pour_le_fil_courant() -> OrdonnancementTempsReel {
    let issue = demander_selon(reglage_courant());
    if let Ok(mut slot) = DERNIER_VERDICT.write() {
        *slot = Some(issue.clone());
    }
    issue
}

fn demander_selon(reglage: Reglage) -> OrdonnancementTempsReel {
    #[cfg(target_os = "linux")]
    {
        linux::demander(reglage)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = reglage;
        OrdonnancementTempsReel::SansObjet
    }
}

/// Le réglage en vigueur, lu dans l'environnement.
pub fn reglage_courant() -> Reglage {
    lire_reglage(std::env::var(VARIABLE_PRIORITE).ok().as_deref())
}

/// Dernier verdict rendu au fil de rendu ; `None` = aucune lecture locale
/// depuis le démarrage.
static DERNIER_VERDICT: std::sync::RwLock<Option<OrdonnancementTempsReel>> =
    std::sync::RwLock::new(None);

static VERROUILLAGE: std::sync::OnceLock<VerrouillageMemoire> = std::sync::OnceLock::new();

/// Verrouille la mémoire du processus, une seule fois, si la décision le
/// permet, et rend le verdict. Hors Linux : [`VerrouillageMemoire::SansObjet`].
pub fn verrouiller_la_memoire_une_fois() -> &'static VerrouillageMemoire {
    VERROUILLAGE.get_or_init(|| {
        #[cfg(target_os = "linux")]
        {
            let reglage = std::env::var(VARIABLE_MLOCK).ok();
            match decider_le_verrouillage(reglage.as_deref(), linux::limite_memlock()) {
                Ok(()) => linux::verrouiller(),
                Err(reason) => VerrouillageMemoire::Ecarte { reason },
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            VerrouillageMemoire::SansObjet
        }
    })
}

/// Lance [`verrouiller_la_memoire_une_fois`] sur un fil à part, une seule fois
/// par processus : `mlockall` parcourt toutes les projections, et ce coût n'a
/// rien à faire dans la première période du rappel audio.
pub fn verrouiller_la_memoire_en_arriere_plan() {
    static LANCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if LANCE.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let lance = std::thread::Builder::new()
        .name("tune-mlock".into())
        .spawn(|| {
            let issue = verrouiller_la_memoire_une_fois();
            match issue {
                VerrouillageMemoire::Verrouille { flags } => {
                    tracing::info!(
                        flags,
                        "local_audio_memory_lock — mémoire verrouillée (#3206)"
                    )
                }
                VerrouillageMemoire::Ecarte { reason } => tracing::info!(
                    reason = %reason,
                    "local_audio_memory_lock — mémoire non verrouillée (#3206)"
                ),
                VerrouillageMemoire::Refuse { cause } => tracing::warn!(
                    cause = %cause,
                    "local_audio_memory_lock — mlockall refusé, la mémoire reste paginable (#3206)"
                ),
                VerrouillageMemoire::SansObjet => {}
            }
        });
    if let Err(e) = lance {
        tracing::warn!(error = %e, "local_audio_memory_lock — fil non lancé (#3206)");
    }
}

/// Une limite telle que la fiche l'affiche : un nombre, ou `"unlimited"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Limite {
    Valeur(u64),
    Illimitee(&'static str),
}

impl Limite {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn de(v: Option<u64>) -> Self {
        v.map_or(Limite::Illimitee("unlimited"), Limite::Valeur)
    }
}

/// Le bloc `audio_realtime` de `/system/diagnostics`.
#[derive(Debug, Clone, Serialize)]
pub struct FicheTempsReel {
    /// `false` hors Linux : rien n'est demandé, la fiche s'arrête là.
    pub applicable: bool,
    pub setting: Reglage,
    /// Limites douces du processus (Linux) ; `null` ailleurs.
    pub rlimit_rtprio: Option<Limite>,
    pub rlimit_memlock_bytes: Option<Limite>,
    /// euid 0 : le noyau accorde `SCHED_FIFO` sans regarder `RLIMIT_RTPRIO`.
    pub root: Option<bool>,
    /// Politique et priorité obtenues par le fil de rendu ; `null` tant
    /// qu'aucune lecture locale n'a tourné.
    pub render_thread: Option<OrdonnancementTempsReel>,
    /// Verdict du verrouillage mémoire ; `null` tant qu'il n'a pas été tenté.
    pub memory_lock: Option<VerrouillageMemoire>,
}

/// Construit la fiche, sans rien demander au noyau.
pub fn fiche() -> FicheTempsReel {
    let render_thread = DERNIER_VERDICT.read().ok().and_then(|g| g.clone());
    let memory_lock = VERROUILLAGE.get().cloned();
    #[cfg(target_os = "linux")]
    {
        FicheTempsReel {
            applicable: true,
            setting: reglage_courant(),
            rlimit_rtprio: Some(Limite::de(linux::limite_rtprio().map(u64::from))),
            rlimit_memlock_bytes: Some(Limite::de(linux::limite_memlock())),
            root: Some(linux::est_root()),
            render_thread,
            memory_lock,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        FicheTempsReel {
            applicable: false,
            setting: reglage_courant(),
            rlimit_rtprio: None,
            rlimit_memlock_bytes: None,
            root: None,
            render_thread,
            memory_lock,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// La borne : une limite plus basse que la cible l'emporte.
    #[test]
    fn la_limite_douce_borne_la_priorite() {
        let v = PRIORITE_VISEE;
        assert_eq!(priorite_bornee(v, Some(95), 99), PRIORITE_VISEE);
        assert_eq!(priorite_bornee(v, Some(20), 99), 20);
        assert_eq!(priorite_bornee(v, Some(1), 99), 1);
        assert_eq!(priorite_bornee(v, None, 99), PRIORITE_VISEE);
    }

    /// Une limite à zéro ne fait pas renoncer : root n'en a pas besoin, et
    /// c'est le noyau qui refuse les autres.
    #[test]
    fn une_limite_a_zero_laisse_le_noyau_trancher() {
        assert_eq!(priorite_bornee(PRIORITE_VISEE, Some(0), 99), PRIORITE_VISEE);
    }

    /// Le maximum de la politique borne aussi, même sans limite.
    #[test]
    fn le_maximum_de_la_politique_borne_aussi() {
        assert_eq!(priorite_bornee(PRIORITE_VISEE, None, 32), 32);
        assert_eq!(priorite_bornee(PRIORITE_VISEE, Some(95), 0), 1);
    }

    /// Le réglage : défaut, valeur valide, désactivation, valeur écartée.
    #[test]
    fn le_reglage_se_lit_sans_jamais_echouer() {
        let defaut = Reglage::Priorite {
            priority: PRIORITE_VISEE,
            source: "default",
        };
        assert_eq!(lire_reglage(None), defaut);
        assert_eq!(lire_reglage(Some("  ")), defaut);
        assert_eq!(
            lire_reglage(Some("80")),
            Reglage::Priorite {
                priority: 80,
                source: "env"
            }
        );
        for non in ["0", "off", "OFF", "false", "no"] {
            assert_eq!(lire_reglage(Some(non)), Reglage::Desactive, "{non}");
        }
        for mauvais in ["100", "-3", "haute", "70.5"] {
            assert_eq!(
                lire_reglage(Some(mauvais)),
                Reglage::Priorite {
                    priority: PRIORITE_VISEE,
                    source: "invalid"
                },
                "{mauvais}"
            );
        }
    }

    /// La fonction de décision, entrées simulées : Tune OS (95), hors Tune OS
    /// (limite 0, la décision demande quand même et laisse le noyau trancher),
    /// réglage plus haut que la limite, réglage désactivé.
    #[test]
    fn la_decision_suit_reglage_et_limite() {
        let p = |priority| Reglage::Priorite {
            priority,
            source: "env",
        };
        // Tune OS : LimitRTPRIO=95, SCHED_FIFO max 99.
        assert_eq!(decider(lire_reglage(None), Some(95), 99), Some(70));
        // Distribution ordinaire, utilisateur sans droit : limite 0.
        assert_eq!(decider(lire_reglage(None), Some(0), 99), Some(70));
        // Le réglage dépasse la limite : la limite l'emporte.
        assert_eq!(decider(p(90), Some(80), 99), Some(80));
        assert_eq!(decider(p(90), None, 99), Some(90));
        // Désactivé : rien n'est demandé, quelle que soit la limite.
        assert_eq!(decider(Reglage::Desactive, Some(95), 99), None);
    }

    /// Le verrouillage mémoire : seule une limite illimitée l'autorise.
    #[test]
    fn le_verrouillage_n_est_tente_que_sous_limite_illimitee() {
        assert_eq!(decider_le_verrouillage(None, None), Ok(()));
        assert_eq!(decider_le_verrouillage(Some("1"), None), Ok(()));
        // 8 Mio : la limite par défaut d'une distribution.
        let refus = decider_le_verrouillage(None, Some(8 << 20)).unwrap_err();
        assert!(refus.contains("8388608"), "{refus}");
        assert!(decider_le_verrouillage(None, Some(0)).is_err());
        let off = decider_le_verrouillage(Some("off"), None).unwrap_err();
        assert!(off.contains(VARIABLE_MLOCK), "{off}");
    }

    /// Le journal dit le verdict une fois ; un verdict qui change se dit encore.
    #[test]
    fn le_verdict_ne_se_journalise_qu_une_fois() {
        let memoire = std::sync::Mutex::new(None);
        let refuse = OrdonnancementTempsReel::Refuse {
            priority: 70,
            rlimit_rtprio: Some(0),
            cause: "Operation not permitted (os error 1)".into(),
        };
        assert!(premiere_fois_dans(&memoire, &refuse));
        assert!(!premiere_fois_dans(&memoire, &refuse));
        assert!(!premiere_fois_dans(&memoire, &refuse.clone()));
        let obtenu = OrdonnancementTempsReel::Obtenu {
            policy: POLITIQUE,
            priority: 70,
            rlimit_rtprio: Some(95),
        };
        assert!(premiere_fois_dans(&memoire, &obtenu));
        assert!(!premiere_fois_dans(&memoire, &obtenu));
    }

    /// La fiche se construit sans lecture et se sérialise avec ses limites.
    #[test]
    fn la_fiche_se_serialise() {
        let json = serde_json::to_value(fiche()).unwrap();
        assert!(json.get("setting").is_some(), "{json}");
        #[cfg(target_os = "linux")]
        {
            assert_eq!(json["applicable"], true);
            assert!(
                json["rlimit_rtprio"].is_u64() || json["rlimit_rtprio"] == "unlimited",
                "{json}"
            );
            assert!(json["root"].is_boolean(), "{json}");
        }
        #[cfg(not(target_os = "linux"))]
        assert_eq!(json["applicable"], false);
    }

    /// L'état se sérialise avec un discriminant lisible par un écran.
    #[test]
    fn l_etat_se_serialise_avec_un_discriminant() {
        let obtenu = OrdonnancementTempsReel::Obtenu {
            policy: POLITIQUE,
            priority: 70,
            rlimit_rtprio: Some(95),
        };
        let json = serde_json::to_value(&obtenu).unwrap();
        assert_eq!(json["state"], "obtenu");
        assert_eq!(json["policy"], "SCHED_FIFO");
        assert_eq!(json["priority"], 70);
        let refuse = OrdonnancementTempsReel::Refuse {
            priority: 70,
            rlimit_rtprio: Some(0),
            cause: "EPERM".into(),
        };
        let json = serde_json::to_value(&refuse).unwrap();
        assert_eq!(json["state"], "refuse");
        assert_eq!(json["rlimit_rtprio"], 0);
        assert!(!refuse.obtenu());
        assert!(obtenu.obtenu());
    }

    /// Le témoin du ticket, sur le vrai noyau : la demande ne panique jamais
    /// et son verdict est CELUI que le noyau tient pour le fil.
    ///
    /// - Sans droit (`ulimit -r 0`, pas root — le cas de la machine de
    ///   compilation) : `Refuse`, cause non vide, le fil est resté en
    ///   `SCHED_OTHER`.
    /// - Avec droit : `Obtenu`, le fil est en `SCHED_FIFO` à la priorité
    ///   annoncée, qui ne dépasse pas la limite.
    #[cfg(target_os = "linux")]
    #[test]
    fn la_demande_ne_panique_jamais_et_dit_vrai() {
        let fil = std::thread::spawn(|| {
            let avant = linux::politique_du_fil_courant();
            let issue = demander_selon(lire_reglage(None));
            let apres = linux::politique_du_fil_courant();
            (avant, issue, apres)
        });
        let (avant, issue, apres) = fil.join().expect("le fil ne doit pas paniquer");
        assert_eq!(
            avant.0,
            libc::SCHED_OTHER,
            "un fil neuf naît en SCHED_OTHER"
        );
        match &issue {
            OrdonnancementTempsReel::Obtenu {
                priority,
                rlimit_rtprio,
                ..
            } => {
                assert_eq!(apres, (libc::SCHED_FIFO, *priority as libc::c_int));
                if let Some(l) = rlimit_rtprio {
                    assert!(
                        *l == 0 || *priority <= *l,
                        "priorité {priority} > limite {l}"
                    );
                }
            }
            OrdonnancementTempsReel::Refuse { cause, .. } => {
                assert!(!cause.is_empty(), "un refus sans cause ne dit rien");
                assert_eq!(apres, avant, "un refus ne doit rien changer au fil");
            }
            OrdonnancementTempsReel::SansObjet | OrdonnancementTempsReel::Desactive => {
                panic!("Linux, réglage par défaut : la demande est posée, lu {issue:?}")
            }
        }
        eprintln!("verdict du noyau : {issue:?} ; politique après : {apres:?}");
    }

    /// Contre-épreuve du repli : sous `RLIMIT_RTPRIO = 0` et sans
    /// `CAP_SYS_NICE`, la demande est REFUSÉE, sans panique, et le fil reste
    /// en `SCHED_OTHER`. Jouée dans un processus enfant pour abaisser la
    /// limite sans toucher aux autres tests.
    #[cfg(target_os = "linux")]
    #[test]
    fn sans_droit_le_fil_reste_en_sched_other() {
        const MARQUEUR: &str = "TUNE_TEST_ORDONNANCEMENT_RT_ENFANT";
        if std::env::var_os(MARQUEUR).is_some() {
            let zero = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY : abaisser sa propre limite est toujours permis.
            let rc = unsafe { libc::setrlimit(libc::RLIMIT_RTPRIO, &zero) };
            assert_eq!(rc, 0, "setrlimit(RLIMIT_RTPRIO, 0)");
            let issue = demander_selon(lire_reglage(None));
            let apres = linux::politique_du_fil_courant();
            println!("ISSUE={issue:?}");
            println!("APRES={apres:?}");
            return;
        }
        // SAFETY : lecture sans effet de bord.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!(
                "root : le noyau accorde SCHED_FIFO quelle que soit la limite, contre-épreuve sans objet"
            );
            return;
        }
        let sortie = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("audio::ordonnancement_rt::tests::sans_droit_le_fil_reste_en_sched_other")
            .arg("--nocapture")
            .env(MARQUEUR, "1")
            .output()
            .expect("lancer l'enfant");
        let stdout = String::from_utf8_lossy(&sortie.stdout);
        assert!(
            sortie.status.success(),
            "l'enfant a échoué :\n{stdout}\n{}",
            String::from_utf8_lossy(&sortie.stderr)
        );
        assert!(
            stdout.contains("ISSUE=Refuse {"),
            "attendu un refus, lu :\n{stdout}"
        );
        assert!(
            stdout.contains("rlimit_rtprio: Some(0)"),
            "la cause doit porter la limite :\n{stdout}"
        );
        assert!(
            stdout.contains(&format!("APRES=({}, 0)", libc::SCHED_OTHER)),
            "le fil doit rester en SCHED_OTHER :\n{stdout}"
        );
    }

    /// Réglage désactivé : rien n'est demandé, le fil reste tel qu'il est né.
    #[cfg(target_os = "linux")]
    #[test]
    fn desactive_ne_demande_rien() {
        let (avant, issue, apres) = std::thread::spawn(|| {
            let avant = linux::politique_du_fil_courant();
            let issue = demander_selon(Reglage::Desactive);
            (avant, issue, linux::politique_du_fil_courant())
        })
        .join()
        .unwrap();
        assert_eq!(issue, OrdonnancementTempsReel::Desactive);
        assert_eq!(apres, avant);
        assert_eq!(apres.0, libc::SCHED_OTHER);
    }

    /// Repli du verrouillage mémoire : sous une limite finie (64 Kio, posée
    /// dans un processus enfant), `mlockall` n'est PAS tenté, sans panique,
    /// et la raison porte la limite.
    #[cfg(target_os = "linux")]
    #[test]
    fn sous_limite_finie_la_memoire_n_est_pas_verrouillee() {
        const MARQUEUR: &str = "TUNE_TEST_MLOCK_ENFANT";
        if std::env::var_os(MARQUEUR).is_some() {
            let lim = libc::rlimit {
                rlim_cur: 65536,
                rlim_max: 65536,
            };
            // SAFETY : abaisser sa propre limite est toujours permis.
            let rc = unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &lim) };
            assert_eq!(rc, 0, "setrlimit(RLIMIT_MEMLOCK)");
            println!("VERDICT={:?}", verrouiller_la_memoire_une_fois());
            println!("FICHE={}", serde_json::to_string(&fiche()).unwrap());
            return;
        }
        let sortie = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("audio::ordonnancement_rt::tests::sous_limite_finie_la_memoire_n_est_pas_verrouillee")
            .arg("--nocapture")
            .env(MARQUEUR, "1")
            .env_remove(VARIABLE_MLOCK)
            .output()
            .expect("lancer l'enfant");
        let stdout = String::from_utf8_lossy(&sortie.stdout);
        assert!(sortie.status.success(), "l'enfant a échoué :\n{stdout}");
        assert!(
            stdout.contains("VERDICT=Ecarte {") && stdout.contains("65536 octets"),
            "attendu un verrouillage écarté sur la limite, lu :\n{stdout}"
        );
        assert!(
            stdout.contains("\"rlimit_memlock_bytes\":65536"),
            "la fiche doit porter la limite :\n{stdout}"
        );
    }
}
