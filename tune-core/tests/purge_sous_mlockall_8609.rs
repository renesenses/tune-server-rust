//! #8609 — sous le verrou mémoire de Tune (`mlockall(MCL_CURRENT|MCL_FUTURE|
//! MCL_ONFAULT)`, Tune OS et `.deb`), la purge rend vraiment la mémoire, et
//! le verrou est rétabli à l'identique.
//!
//! Décision de Bertrand du 12/11 : le verrou est levé le temps de la purge,
//! et seulement pendant. Sans cela, le noyau refuse `MADV_DONTNEED` sur les
//! pages verrouillées : `malloc_trim` ne rend rien (banc Shrek : 524 → 524 Mo).
//!
//! Binaire à lui seul, un seul test : `mlockall` vaut pour tout le processus.
//! Il demande une `RLIMIT_MEMLOCK` d'au moins 2 Gio (Shrek : 31 Gio) ; en
//! deçà — runner de CI ordinaire —, il le dit et s'arrête sans rien prouver.

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[test]
fn la_purge_leve_le_verrou_rend_la_memoire_et_le_retablit() {
    use tune_core::audio::ordonnancement_rt::{
        VerrouillageMemoire, verrouiller_sans_decision_pour_un_banc,
    };
    use tune_core::memoire_rendue::{memoire_verrouillee_kb, purger, rss_kb};

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

    // Le verrou de PRODUCTION, posé par `ordonnancement_rt`.
    assert!(
        matches!(
            verrouiller_sans_decision_pour_un_banc(),
            VerrouillageMemoire::Verrouille { .. }
        ),
        "mlockall refusé"
    );
    let vmlck = || memoire_verrouillee_kb().expect("VmLck lisible");
    assert!(vmlck() > 0);

    // ~200 Mo libres gardés par les arènes, et touchés : donc verrouillés.
    fn laisser_de_la_memoire_libre() -> Vec<Vec<u8>> {
        std::thread::scope(|s| {
            let fils: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| {
                        let mut blocs: Vec<Vec<u8>> =
                            (0..400).map(|_| vec![1u8; 64 * 1024]).collect();
                        let bouchon = vec![2u8; 512];
                        blocs.clear();
                        drop(blocs);
                        bouchon
                    })
                })
                .collect();
            fils.into_iter().map(|f| f.join().unwrap()).collect()
        })
    }

    // CONTRE-ÉPREUVE : `malloc_trim` seul, sous le verrou — rien ne revient.
    let bouchons = laisser_de_la_memoire_libre();
    let avant = rss_kb().unwrap();
    // SAFETY : aucune précondition.
    unsafe { libc::malloc_trim(0) };
    let apres = rss_kb().unwrap();
    assert!(
        avant.saturating_sub(apres) < 50 * 1024,
        "sous le verrou, malloc_trim seul ne devait (presque) rien rendre : {avant} -> {apres} Ko"
    );

    // La purge de production : elle lève le verrou, rend, le rétablit.
    let vmlck_avant = vmlck();
    let p = purger().expect("une purge");
    drop(bouchons);
    assert!(p.verrou_leve, "le verrou de Tune devait être levé : {p:?}");
    assert!(p.echec_du_reverrouillage.is_none(), "{p:?}");
    assert!(
        p.rendu_mb() >= 100,
        "levé le temps de la purge, le verrou ne retient plus rien : {p:?}"
    );
    let vmlck_apres = p.verrouillee_apres_kb.expect("VmLck lisible");
    assert!(
        vmlck_apres * 2 >= vmlck_avant,
        "le verrou doit être revenu : VmLck {vmlck_avant} -> {vmlck_apres} Ko"
    );

    // Les MÊMES drapeaux : `MCL_FUTURE` est revenu, une projection neuve est
    // verrouillée d'office (avec `MCL_ONFAULT`, VmLck compte la projection).
    let avant_projection = vmlck();
    let mut neuve = vec![0u8; 64 << 20];
    neuve[0] = 1;
    let apres_projection = vmlck();
    drop(neuve);
    assert!(
        apres_projection >= avant_projection + 60 * 1024,
        "MCL_FUTURE doit être rétabli : VmLck {avant_projection} -> {apres_projection} Ko"
    );

    // SAFETY : appel sans pointeur.
    unsafe { libc::munlockall() };
}
