//! La purge à froid rend vraiment au système les pages libres que l'allocateur
//! garde (`tune_server::memoire_a_froid`).
//!
//! Binaire de test À PART : le RSS est une mesure de tout le processus, et des
//! tests voisins qui allouent en parallèle la brouilleraient.
//!
//! Le scénario reproduit ce qui a été mesuré sur Shrek : plusieurs fils
//! allouent puis libèrent beaucoup de petits blocs (sous le seuil `mmap`, donc
//! dans leurs arènes), et chacun garde une petite allocation faite APRÈS, au
//! haut de son tas. Le tas n'a alors pas de haut libre : la glibc ne rend rien
//! d'elle-même, et le RSS reste haut alors que presque tout est libre.

use tune_server::memoire_a_froid::{purge_permise, rendre_la_memoire_liberee, rss_mb};

#[test]
fn la_purge_n_a_lieu_qu_a_froid() {
    assert!(purge_permise(false));
    assert!(!purge_permise(true));
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[test]
fn la_purge_rend_les_pages_libres_gardees_par_les_arenes() {
    const FILS: usize = 8;
    const BLOCS: usize = 400;
    const TAILLE: usize = 64 * 1024; // sous le seuil mmap de la glibc
    let total_mb = (FILS * BLOCS * TAILLE / 1_048_576) as u64; // 200 Mo

    let bouchons: Vec<Vec<u8>> = std::thread::scope(|s| {
        let fils: Vec<_> = (0..FILS)
            .map(|_| {
                s.spawn(|| {
                    let mut blocs: Vec<Vec<u8>> = (0..BLOCS).map(|_| vec![1u8; TAILLE]).collect();
                    // Le bouchon, alloué APRÈS : il tient le haut du tas.
                    let bouchon = vec![2u8; 512];
                    blocs.clear();
                    drop(blocs);
                    bouchon
                })
            })
            .collect();
        fils.into_iter().map(|f| f.join().unwrap()).collect()
    });

    let avant = rss_mb().expect("RSS lisible sous Linux");
    rendre_la_memoire_liberee();
    let apres = rss_mb().expect("RSS lisible sous Linux");
    drop(bouchons);

    assert!(
        avant >= apres + total_mb / 2,
        "la purge devait rendre au moins la moitié des {total_mb} Mo libérés : RSS {avant} -> {apres} Mo"
    );
}
