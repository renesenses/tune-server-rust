//! #8609 (banc rc12) — le rétablissement du verrou mémoire REFUSÉ par le
//! noyau après une purge : l'échec est rendu, la mémoire reste déverrouillée
//! le temps qu'il faut, et le relevé suivant (`reverrouiller_si_en_attente`)
//! repose le verrou avec les MÊMES drapeaux dès que le noyau le permet.
//!
//! `purge_sous_mlockall_8609.rs` prouve le chemin heureux (levé, rendu,
//! rétabli). Ce banc joue le chemin d'échec, que rien ne couvrait : le refus
//! est provoqué sans matériel en abaissant la limite DOUCE `RLIMIT_MEMLOCK`
//! sous la taille du processus (le noyau répond alors `ENOMEM` à
//! `mlockall(MCL_CURRENT…)`), puis en la remettant.
//!
//! Binaire à lui seul, un seul test : `mlockall` et `RLIMIT_MEMLOCK` valent
//! pour tout le processus. Il demande une `RLIMIT_MEMLOCK` d'au moins 2 Gio et
//! un compte sans `CAP_IPC_LOCK` (root passe outre la limite) ; sinon il le
//! dit et s'arrête sans rien prouver.

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[test]
fn un_reverrouillage_refuse_est_dit_puis_retente_au_releve_suivant() {
    use tune_core::audio::ordonnancement_rt::{
        VerrouillageMemoire, reverrouiller_si_en_attente, verrouiller_sans_decision_pour_un_banc,
    };
    use tune_core::memoire_rendue::{memoire_verrouillee_kb, purger};

    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY : lecture dans une structure locale.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut lim) },
        0
    );
    if lim.rlim_cur != libc::RLIM_INFINITY && lim.rlim_cur < (2u64 << 30) as libc::rlim_t {
        eprintln!(
            "RLIMIT_MEMLOCK = {} octets : verrouillage non simulable ici, rien n'est prouvé",
            lim.rlim_cur
        );
        return;
    }
    // SAFETY : lecture sans effet de bord.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("root : la limite ne s'applique pas, refus non simulable, rien n'est prouvé");
        return;
    }
    let limite_d_origine = lim;
    let poser_la_limite = |douce: libc::rlim_t| {
        let l = libc::rlimit {
            rlim_cur: douce,
            rlim_max: limite_d_origine.rlim_max,
        };
        // SAFETY : structure locale valide ; une limite douce <= dure est permise.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &l) }, 0);
    };

    assert!(
        matches!(
            verrouiller_sans_decision_pour_un_banc(),
            VerrouillageMemoire::Verrouille { .. }
        ),
        "mlockall refusé"
    );
    let vmlck = || memoire_verrouillee_kb().expect("VmLck lisible");
    // De la mémoire touchée, donc verrouillée.
    let mut gardee = vec![1u8; 32 << 20];
    gardee[0] = 2;
    assert!(vmlck() > 0, "le verrou de production doit être posé");

    // Le noyau refusera le rétablissement : 1 Mio, bien sous la taille du
    // processus.
    poser_la_limite(1 << 20);
    let p = purger().expect("une purge");
    assert!(p.verrou_leve, "le verrou de Tune devait être levé : {p:?}");
    assert!(
        p.echec_du_reverrouillage.is_some(),
        "le refus du rétablissement doit être rendu, pas tu : {p:?}"
    );
    assert_eq!(
        p.verrouillee_apres_kb,
        Some(0),
        "refusé, le verrou n'est PAS revenu : VmLck doit être nul : {p:?}"
    );
    // Toujours refusé : le relevé retente et échoue encore, sans mentir.
    assert!(
        !reverrouiller_si_en_attente(),
        "sous la limite, le nouvel essai ne peut pas réussir"
    );
    assert_eq!(vmlck(), 0);

    // La limite revient : le relevé suivant rétablit le verrou.
    poser_la_limite(limite_d_origine.rlim_cur);
    assert!(
        reverrouiller_si_en_attente(),
        "le relevé suivant doit reposer le verrou en attente"
    );
    assert!(vmlck() > 0, "le verrou doit être revenu");
    assert!(
        !reverrouiller_si_en_attente(),
        "plus rien n'est en attente une fois le verrou rétabli"
    );

    // Les MÊMES drapeaux : `MCL_FUTURE` est revenu, une projection neuve est
    // comptée d'office (avec `MCL_ONFAULT`, VmLck compte la projection).
    let avant_projection = vmlck();
    let mut neuve = vec![0u8; 64 << 20];
    neuve[0] = 1;
    let apres_projection = vmlck();
    drop(neuve);
    drop(gardee);
    assert!(
        apres_projection >= avant_projection + 60 * 1024,
        "MCL_FUTURE doit être rétabli : VmLck {avant_projection} -> {apres_projection} Ko"
    );

    // La purge suivante, limite revenue, se passe normalement.
    let p = purger().expect("une purge");
    assert!(p.verrou_leve, "{p:?}");
    assert!(p.echec_du_reverrouillage.is_none(), "{p:?}");

    // SAFETY : appel sans pointeur.
    unsafe { libc::munlockall() };
}
