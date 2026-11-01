//! #7619, volet L1 — la liste des pistes par CURSEUR sur la clé de tri
//! indexée (`tracks.cle_tri`, `idx_tracks_cle_tri`).
//!
//! Témoins, sur le banc de #5138 (albums masqués, distants doublés, copies
//! de moindre qualité, pistes sans album, gros album sans numéros) :
//!
//! - le parcours par curseur rend EXACTEMENT les pistes de l'ancien chemin
//!   (socle corrélé, `list_visible_avec_total`), sans doublon ni trou ;
//! - dans l'ordre de la clé (artiste, album, disque, numéro pliés, puis id) ;
//! - l'ancien `offset` rend la même suite ;
//! - un tri par colonne se parcourt aussi par curseur, dans les deux sens ;
//! - une piste insérée entre deux pages ne fait PAS revenir une ligne déjà
//!   rendue (contre-épreuve : par `offset`, elle revient) ;
//! - un renommage est rattrapé par le rafraîchissement complet ;
//! - la passe de démarrage est idempotente.

use std::collections::HashSet;

use super::backend::DbBackend;
use super::lenteur_pistes_5138_tests::remplir_banc;
use super::sqlite::SqliteDb;
use super::track_repo::cle_de_tri::{Curseur, cle_de_tri};
use super::track_repo::{ColonneDeTri, DemandeDePistes, TrackRepo};

fn banc(dossier: &tempfile::TempDir, locaux: i64) -> (std::sync::Arc<SqliteDb>, TrackRepo) {
    let chemin = dossier.path().join("banc-7619.db");
    let db = SqliteDb::open(chemin.to_str().unwrap()).unwrap();
    db.init_schema().unwrap();
    super::migrations::run_migrations(&db).unwrap();
    remplir_banc(&db, locaux);
    // Un peu de texte accentué et des cellules vides : le pli et les NULL.
    db.execute_batch(
        "UPDATE artists SET name = 'Éla Ferré' WHERE id = 2;
         UPDATE artists SET name = 'abba' WHERE id = 3;
         UPDATE tracks SET disc_number = NULL WHERE id % 17 = 0;
         UPDATE tracks SET title = '' WHERE id % 23 = 0;",
    )
    .unwrap();
    let db = std::sync::Arc::new(db);
    let repo = TrackRepo::with_backend(db.clone());
    // Plus de pistes que le plafond du comblement en ligne : on comble ici,
    // comme le ferait la tâche de fond au premier démarrage.
    repo.rafraichir_cles_de_tri(false).unwrap();
    (db, repo)
}

/// Toutes les pages, curseur après curseur.
fn par_curseur(repo: &TrackRepo, base: &DemandeDePistes) -> (Vec<i64>, i64) {
    let mut ids = Vec::new();
    let mut apres: Option<Curseur> = None;
    for _ in 0..10_000 {
        let page = repo
            .page_de_pistes(&DemandeDePistes {
                apres: apres.clone(),
                ..base.clone()
            })
            .unwrap();
        ids.extend(page.pistes.iter().filter_map(|t| t.id));
        match page.curseur_suivant {
            Some(c) => apres = Some(Curseur::decoder(&c).unwrap()),
            None => return (ids, page.total),
        }
    }
    panic!("le parcours ne s'arrête pas");
}

/// L'ancien chemin : socle corrélé, ordre `LOWER(…)`, toute la liste.
fn liste_d_avant(repo: &TrackRepo) -> Vec<i64> {
    repo.list_visible_avec_total(1_000_000, 0)
        .unwrap()
        .0
        .iter()
        .filter_map(|t| t.id)
        .collect()
}

/// L'ordre attendu, recalculé en Rust depuis les colonnes.
fn ordre_attendu(db: &dyn DbBackend, ids: &[i64]) -> Vec<i64> {
    let voulus: HashSet<i64> = ids.iter().copied().collect();
    let mut lignes: Vec<(String, i64)> = db
        .query_many(
            "SELECT t.id, ar.name, al.title, CAST(t.disc_number AS INTEGER), \
             CAST(t.track_number AS INTEGER) FROM tracks t \
             LEFT JOIN albums al ON t.album_id = al.id \
             LEFT JOIN artists ar ON t.artist_id = ar.id",
            &[],
        )
        .unwrap()
        .iter()
        .filter_map(|l| {
            let id = l[0].as_i64()?;
            voulus.contains(&id).then(|| {
                (
                    cle_de_tri(l[1].as_str(), l[2].as_str(), l[3].as_i64(), l[4].as_i64()),
                    id,
                )
            })
        })
        .collect();
    lignes.sort();
    lignes.into_iter().map(|(_, id)| id).collect()
}

fn sans_doublon(ids: &[i64]) {
    let mut vus = HashSet::new();
    for id in ids {
        assert!(vus.insert(*id), "la piste {id} est rendue deux fois");
    }
}

#[test]
fn le_curseur_rend_la_vue_entiere_sans_doublon_ni_trou_dans_l_ordre_de_la_cle() {
    let dossier = tempfile::tempdir().unwrap();
    let (db, repo) = banc(&dossier, 200);
    let avant = liste_d_avant(&repo);
    let base = DemandeDePistes {
        limit: 97,
        ..Default::default()
    };
    let (ids, total) = par_curseur(&repo, &base);
    sans_doublon(&ids);
    assert_eq!(
        ids.iter().copied().collect::<HashSet<_>>(),
        avant.iter().copied().collect::<HashSet<_>>(),
        "le curseur ne rend pas les pistes de l'ancien chemin"
    );
    assert_eq!(ids.len(), avant.len());
    assert_eq!(
        total,
        repo.count_visible().unwrap(),
        "total ≠ compteur de la vue"
    );
    assert_eq!(
        ids,
        ordre_attendu(db.as_ref(), &ids),
        "ordre ≠ (clé pliée, id)"
    );

    // L'ancien `offset` : la même suite.
    let mut par_decalage = Vec::new();
    let mut offset = 0;
    loop {
        let page = repo
            .page_de_pistes(&DemandeDePistes {
                offset,
                ..base.clone()
            })
            .unwrap();
        if page.pistes.is_empty() {
            break;
        }
        offset += page.pistes.len() as i64;
        par_decalage.extend(page.pistes.iter().filter_map(|t| t.id));
    }
    assert_eq!(par_decalage, ids, "offset et curseur divergent");
}

#[test]
fn un_tri_par_colonne_se_parcourt_par_curseur_dans_les_deux_sens() {
    let dossier = tempfile::tempdir().unwrap();
    let (_db, repo) = banc(&dossier, 120);
    let avant: HashSet<i64> = liste_d_avant(&repo).into_iter().collect();
    for (tri, descendant) in [
        (ColonneDeTri::Titre, false),
        (ColonneDeTri::Titre, true),
        (ColonneDeTri::Disque, true),
        (ColonneDeTri::Duree, false),
    ] {
        let base = DemandeDePistes {
            tri: Some(tri),
            descendant,
            limit: 61,
            ..Default::default()
        };
        let (ids, _) = par_curseur(&repo, &base);
        sans_doublon(&ids);
        assert_eq!(
            ids.iter().copied().collect::<HashSet<_>>(),
            avant,
            "{tri:?} desc={descendant} : pas les pistes de l'ancien chemin"
        );
        let d_un_coup: Vec<i64> = repo
            .page_de_pistes(&DemandeDePistes {
                limit: 1_000_000,
                ..base.clone()
            })
            .unwrap()
            .pistes
            .iter()
            .filter_map(|t| t.id)
            .collect();
        assert_eq!(
            ids, d_un_coup,
            "{tri:?} desc={descendant} : pages ≠ liste d'un coup"
        );
    }
}

#[test]
fn une_piste_inseree_entre_deux_pages_ne_fait_pas_revenir_une_ligne_rendue() {
    let dossier = tempfile::tempdir().unwrap();
    let (db, repo) = banc(&dossier, 120);
    let base = DemandeDePistes {
        limit: 50,
        ..Default::default()
    };
    let p1 = repo.page_de_pistes(&base).unwrap();
    let curseur = Curseur::decoder(p1.curseur_suivant.as_deref().unwrap()).unwrap();
    // Une piste qui se range EN TÊTE de la liste (artiste « 0 »).
    db.execute_batch(
        "INSERT INTO artists (id, name) VALUES (99999, '0 en tête');
         INSERT INTO tracks (title, artist_id, file_path, format, source) \
         VALUES ('Nouvelle', 99999, '/banc/nouvelle.flac', 'flac', 'local');",
    )
    .unwrap();
    let rendus: HashSet<i64> = p1.pistes.iter().filter_map(|t| t.id).collect();
    let p2 = repo
        .page_de_pistes(&DemandeDePistes {
            apres: Some(curseur),
            ..base.clone()
        })
        .unwrap();
    for t in &p2.pistes {
        assert!(
            !rendus.contains(&t.id.unwrap()),
            "curseur : la piste {:?} revient après une insertion",
            t.id
        );
    }
    // Contre-épreuve : par `offset`, la ligne 50 d'avant revient en tête.
    let p2_decalage = repo
        .page_de_pistes(&DemandeDePistes {
            offset: 50,
            ..base.clone()
        })
        .unwrap();
    assert!(
        p2_decalage
            .pistes
            .iter()
            .any(|t| rendus.contains(&t.id.unwrap())),
        "contre-épreuve : l'offset aurait dû rendre une ligne déjà vue"
    );
}

#[test]
fn un_renommage_est_rattrape_par_le_rafraichissement_complet() {
    let dossier = tempfile::tempdir().unwrap();
    let (db, repo) = banc(&dossier, 100);
    assert!(repo.assurer_les_cles_de_tri());
    let n_artiste_1 = db
        .query_one("SELECT COUNT(*) FROM tracks WHERE artist_id = 1", &[])
        .unwrap()
        .unwrap()[0]
        .as_i64()
        .unwrap();
    db.execute_batch("UPDATE artists SET name = 'Zz renommé' WHERE id = 1;")
        .unwrap();
    assert_eq!(
        repo.rafraichir_cles_de_tri(true).unwrap(),
        0,
        "aucune clé n'est absente : le comblement rapide n'écrit rien"
    );
    assert_eq!(
        repo.rafraichir_cles_de_tri(false).unwrap() as i64,
        n_artiste_1,
        "le rafraîchissement complet réécrit les clés de l'artiste renommé"
    );
    assert_eq!(repo.rafraichir_cles_de_tri(false).unwrap(), 0, "idempotent");
}

#[test]
fn la_passe_de_demarrage_pose_la_colonne_et_l_index_une_fois() {
    let dossier = tempfile::tempdir().unwrap();
    let (db, _repo) = banc(&dossier, 100);
    super::migrations::run_migrations(&db).unwrap();
    let un = |sql: &str| {
        db.query_one(sql, &[]).unwrap().unwrap()[0]
            .as_i64()
            .unwrap()
    };
    assert_eq!(
        un("SELECT COUNT(*) FROM pragma_table_info('tracks') WHERE name = 'cle_tri'"),
        1
    );
    assert_eq!(
        un(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_tracks_cle_tri'"
        ),
        1
    );
}

#[test]
fn un_curseur_d_un_autre_tri_n_est_pas_suivi() {
    let dossier = tempfile::tempdir().unwrap();
    let (_db, repo) = banc(&dossier, 100);
    let p1 = repo
        .page_de_pistes(&DemandeDePistes {
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    let c = Curseur::decoder(p1.curseur_suivant.as_deref().unwrap()).unwrap();
    // Le même curseur sous un tri par titre : repart de la première page.
    let titre = DemandeDePistes {
        tri: Some(ColonneDeTri::Titre),
        limit: 20,
        ..Default::default()
    };
    let premiere = repo.page_de_pistes(&titre).unwrap();
    let avec_c = repo
        .page_de_pistes(&DemandeDePistes {
            apres: Some(c),
            ..titre.clone()
        })
        .unwrap();
    assert_eq!(
        premiere.pistes.iter().map(|t| t.id).collect::<Vec<_>>(),
        avec_c.pistes.iter().map(|t| t.id).collect::<Vec<_>>()
    );
}
