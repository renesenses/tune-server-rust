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

use tune_server::memoire_a_froid::{
    purge_permise, rendre_la_memoire_liberee, rss_mb, une_sortie_locale_joue,
};

#[test]
fn la_purge_n_a_lieu_qu_a_froid() {
    assert!(purge_permise(false));
    assert!(!purge_permise(true));
}

/// Fil 2167 — TÉMOIN : une zone RÉSEAU qui joue ne retient plus la purge ;
/// une sortie LOCALE, oui, même suiveuse d'un groupe mené par une zone réseau.
///
/// Avant le correctif, la garde était « une zone joue » : le premier cas
/// (une zone DLNA seule en lecture) refusait la purge, et un serveur qui
/// joue sans arrêt vers un renderer réseau ne rendait jamais rien.
#[test]
fn seule_une_sortie_locale_en_lecture_retient_la_purge() {
    use std::sync::Arc;
    use tune_core::db::backend::DbBackend;
    use tune_core::db::sqlite::SqliteDb;

    let db = SqliteDb::open_in_memory().unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db.clone());

    // Table absente : la requête échoue, dans le doute on ne purge pas.
    assert!(
        une_sortie_locale_joue(&backend),
        "une requête en échec doit retenir la purge"
    );

    db.execute_batch(
        "CREATE TABLE zones (id INTEGER PRIMARY KEY, name TEXT, output_type TEXT,
                             group_id INTEGER, last_play_state TEXT);
         INSERT INTO zones (id, name, output_type, group_id, last_play_state) VALUES
             (1, 'Salon', 'dlna', NULL, 'playing'),
             (2, 'Carte son', 'local', NULL, 'stopped'),
             (3, 'Cuisine', 'airplay', NULL, 'paused');",
    )
    .unwrap();
    assert!(
        !une_sortie_locale_joue(&backend),
        "une zone DLNA seule en lecture retient la purge : un serveur qui joue sans arrêt \
         vers un renderer réseau ne rend jamais sa mémoire (fil 2167)"
    );
    assert!(purge_permise(une_sortie_locale_joue(&backend)));

    // La sortie locale joue : la purge attend.
    db.execute_batch("UPDATE zones SET last_play_state = 'playing' WHERE id = 2;")
        .unwrap();
    assert!(
        une_sortie_locale_joue(&backend),
        "une sortie locale joue : pas de purge"
    );

    // Suiveuse locale d'un groupe mené par la zone DLNA : la purge attend aussi.
    db.execute_batch(
        "UPDATE zones SET last_play_state = 'stopped' WHERE id = 2;
         UPDATE zones SET group_id = 7 WHERE id IN (1, 2);",
    )
    .unwrap();
    assert!(
        une_sortie_locale_joue(&backend),
        "une sortie locale groupée avec une zone qui joue doit retenir la purge"
    );

    // Type inconnu en lecture : dans le doute, la purge attend.
    db.execute_batch(
        "UPDATE zones SET group_id = NULL, last_play_state = 'stopped';
         INSERT INTO zones (id, name, output_type, last_play_state) VALUES (4, '?', NULL, 'playing');",
    )
    .unwrap();
    assert!(
        une_sortie_locale_joue(&backend),
        "un type de sortie inconnu doit retenir la purge"
    );
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
