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
pub fn priorite_bornee(limite_douce: Option<u32>, max_de_la_politique: u32) -> u32 {
    let plafond = match limite_douce {
        Some(0) | None => max_de_la_politique,
        Some(l) => l.min(max_de_la_politique),
    };
    PRIORITE_VISEE.min(plafond).max(1)
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{OrdonnancementTempsReel, POLITIQUE, priorite_bornee};

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
        let mut param = libc::sched_param { sched_priority: 0 };
        // SAFETY : les deux sorties sont des valeurs locales valides ;
        // pthread_self() est toujours un fil vivant.
        unsafe {
            libc::pthread_getschedparam(libc::pthread_self(), &mut politique, &mut param);
        }
        (politique, param.sched_priority)
    }

    /// Demande `SCHED_FIFO` pour le fil courant, à la priorité bornée.
    pub fn demander() -> OrdonnancementTempsReel {
        let rlimit_rtprio = limite_rtprio();
        // SAFETY : appel sans argument ni effet de bord.
        let max = unsafe { libc::sched_get_priority_max(libc::SCHED_FIFO) };
        let priority = priorite_bornee(rlimit_rtprio, u32::try_from(max).unwrap_or(1));
        let param = libc::sched_param {
            sched_priority: priority as libc::c_int,
        };
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
    #[cfg(target_os = "linux")]
    {
        linux::demander()
    }
    #[cfg(not(target_os = "linux"))]
    {
        OrdonnancementTempsReel::SansObjet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// La borne : une limite plus basse que la cible l'emporte.
    #[test]
    fn la_limite_douce_borne_la_priorite() {
        assert_eq!(priorite_bornee(Some(95), 99), PRIORITE_VISEE);
        assert_eq!(priorite_bornee(Some(20), 99), 20);
        assert_eq!(priorite_bornee(Some(1), 99), 1);
        assert_eq!(priorite_bornee(None, 99), PRIORITE_VISEE);
    }

    /// Une limite à zéro ne fait pas renoncer : root n'en a pas besoin, et
    /// c'est le noyau qui refuse les autres.
    #[test]
    fn une_limite_a_zero_laisse_le_noyau_trancher() {
        assert_eq!(priorite_bornee(Some(0), 99), PRIORITE_VISEE);
    }

    /// Le maximum de la politique borne aussi, même sans limite.
    #[test]
    fn le_maximum_de_la_politique_borne_aussi() {
        assert_eq!(priorite_bornee(None, 32), 32);
        assert_eq!(priorite_bornee(Some(95), 0), 1);
    }

    /// La cible reste sous le plafond de Tune OS et hors de la bande du noyau.
    #[test]
    fn la_cible_tient_sous_le_plafond_de_tune_os() {
        assert!(PRIORITE_VISEE < 95, "au-dessus de LimitRTPRIO=95");
        assert!(PRIORITE_VISEE > 50, "sous les IRQ filées du noyau");
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
            let issue = demander_pour_le_fil_courant();
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
            OrdonnancementTempsReel::SansObjet => panic!("Linux n'est jamais sans objet"),
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
            let issue = demander_pour_le_fil_courant();
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
}
