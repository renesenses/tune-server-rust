//! #5341 — une retouche de balises (Mp3tag, foobar2000, Picard…) relue par le
//! surveillant de fichiers SUPPRIMAIT la ligne de la piste puis la recréait :
//! nouvel identifiant à chaque sauvegarde dans l'éditeur.
//!
//! Ces épreuves jouent la VRAIE voie du surveillant
//! ([`reimporter_fichier_surveillant`], appelée telle quelle par la boucle de
//! `spawn_file_watcher`) sur de vrais FLAC indexés par le scan, sur une base
//! SQLite de FICHIER — jamais `:memory:`, dont le pool de lecture clone la
//! connexion d'écriture et rend aveugles les épreuves de scan (#5043) — puis,
//! si `TUNE_TEST_PG_URL` est posée, sur une vraie base PostgreSQL.
//!
//! Avant la retouche, la piste reçoit tout ce qu'un auditeur y rattache :
//! favori, note de son album, écoute, place dans une playlist, exemplaire,
//! étiquette, signet et métadonnée étendue. Après la retouche, l'épreuve fait
//! l'inventaire de ce qui pointe encore sur une piste vivante.
//!
//! Constat sur le code d'avant (#5341) : l'identifiant changeait ; la place en
//! playlist, l'exemplaire, le signet et la métadonnée étendue partaient par
//! `ON DELETE CASCADE` ; l'écoute perdait sa piste (`ON DELETE SET NULL`) ; le
//! favori et l'étiquette restaient orphelins (aucune clé étrangère sur
//! `item_id`). Seule la note, portée par l'ALBUM, survivait.
use super::reimporter_fichier_surveillant;
use super::surveillant_retouche_tests_4896::{baliser, coffret_indexe};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tune_core::db::backend::{DbBackend, SqlValue, ToSqlValue};
use tune_core::db::track_repo::TrackRepo;
use tune_core::scanner::watcher::{ChangeType, FileChange};

/// Préfixe des noms de playlist et d'étiquette de l'épreuve (suffixés par
/// épreuve) : sur PostgreSQL, les tables sont partagées avec les autres bancs,
/// et `tags.name` est unique.
const MARQUE: &str = "retouche-5341";

/// Une base SQLite de FICHIER, schéma et migrations posés. Rend le dossier
/// (à garder vivant) et la base.
fn base_fichier(epreuve: &str) -> (tune_core::test_scratch::ScratchDir, Arc<dyn DbBackend>) {
    let dossier = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        &format!("base-5341-{epreuve}"),
    );
    let chemin = dossier.join("tune.db");
    let db = tune_core::db::sqlite::SqliteDb::open(&chemin.to_string_lossy()).unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    (dossier, Arc::new(db))
}

fn entier(db: &Arc<dyn DbBackend>, sql: &str, params: &[&dyn ToSqlValue]) -> i64 {
    match db.query_one(sql, params).expect(sql) {
        Some(ligne) => match ligne.first() {
            Some(SqlValue::Int(n)) => *n,
            autre => panic!("{sql} : pas un entier ({autre:?})"),
        },
        None => 0,
    }
}

fn identifiant(db: &Arc<dyn DbBackend>, piste: &Path) -> i64 {
    TrackRepo::with_backend(db.clone())
        .get_by_path(&piste.to_string_lossy())
        .unwrap()
        .expect("la piste est en base")
        .id
        .expect("identifiant")
}

/// Ce que l'auditeur a rattaché à la piste, posé par les tables mêmes que les
/// routes écrivent.
fn rattacher_tout(db: &Arc<dyn DbBackend>, piste: &Path, racine: &Path, marque: &str) {
    let id = identifiant(db, piste);
    let album_id = TrackRepo::with_backend(db.clone())
        .get(id)
        .unwrap()
        .unwrap()
        .album_id
        .expect("album");
    let exemplaire = racine
        .join("autre-racine")
        .join("01.flac")
        .to_string_lossy()
        .into_owned();
    let exec = |sql: &str, params: &[&dyn ToSqlValue]| {
        db.execute(sql, params)
            .unwrap_or_else(|e| panic!("{sql} : {e}"));
    };
    exec(
        "INSERT INTO favorites (profile_id, item_type, item_id) VALUES (1, 'track', ?)",
        &[&id],
    );
    exec(
        "INSERT INTO album_ratings (album_id, profile_id, rating) VALUES (?, 1, 5)",
        &[&album_id],
    );
    exec(
        "INSERT INTO listen_history (track_id, title) VALUES (?, 'Speak To Me')",
        &[&id],
    );
    let playlist = db
        .execute_returning_id("INSERT INTO playlists (name) VALUES (?)", &[&marque])
        .expect("playlist");
    exec(
        "INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES (?, ?, 0)",
        &[&playlist, &id],
    );
    exec(
        "INSERT INTO track_copies (track_id, file_path) VALUES (?, ?)",
        &[&id, &exemplaire],
    );
    let tag = db
        .execute_returning_id("INSERT INTO tags (name) VALUES (?)", &[&marque])
        .expect("étiquette");
    exec(
        "INSERT INTO item_tags (tag_id, item_type, item_id) VALUES (?, 'track', ?)",
        &[&tag, &id],
    );
    exec(
        "INSERT INTO bookmarks (track_id, position_ms) VALUES (?, 60000)",
        &[&id],
    );
    exec(
        "INSERT INTO track_metadata (track_id, key, value) VALUES (?, 'rg_track_gain', '-7.1 dB')",
        &[&id],
    );
}

/// L'inventaire après la retouche : une ligne par rattachement PERDU.
fn inventaire(db: &Arc<dyn DbBackend>, piste: &Path, id_avant: i64, marque: &str) -> Vec<String> {
    let id = identifiant(db, piste);
    let mut pertes = Vec::new();
    if id != id_avant {
        pertes.push(format!(
            "identifiant : {id_avant} avant la retouche, {id} après (ligne supprimée puis recréée)"
        ));
    }
    let album_id = TrackRepo::with_backend(db.clone())
        .get(id)
        .unwrap()
        .unwrap()
        .album_id
        .expect("album");
    let mut compte = |quoi: &str, sql: &str, params: &[&dyn ToSqlValue]| {
        let n = entier(db, sql, params);
        if n != 1 {
            pertes.push(format!(
                "{quoi} : {n} ligne(s) rattachée(s) à la piste vivante au lieu de 1"
            ));
        }
    };
    compte(
        "favori",
        "SELECT COUNT(*) FROM favorites WHERE item_type = 'track' AND item_id = ?",
        &[&id],
    );
    compte(
        "note de l'album",
        "SELECT COUNT(*) FROM album_ratings WHERE album_id = ? AND rating = 5",
        &[&album_id],
    );
    compte(
        "écoute",
        "SELECT COUNT(*) FROM listen_history WHERE track_id = ?",
        &[&id],
    );
    compte(
        "place en playlist",
        "SELECT COUNT(*) FROM playlist_tracks pt JOIN playlists p ON p.id = pt.playlist_id \
         WHERE p.name = ? AND pt.track_id = ?",
        &[&marque, &id],
    );
    compte(
        "exemplaire",
        "SELECT COUNT(*) FROM track_copies WHERE track_id = ?",
        &[&id],
    );
    compte(
        "étiquette",
        "SELECT COUNT(*) FROM item_tags it JOIN tags t ON t.id = it.tag_id \
         WHERE t.name = ? AND it.item_type = 'track' AND it.item_id = ?",
        &[&marque, &id],
    );
    compte(
        "signet",
        "SELECT COUNT(*) FROM bookmarks WHERE track_id = ?",
        &[&id],
    );
    compte(
        "métadonnée étendue",
        "SELECT COUNT(*) FROM track_metadata WHERE track_id = ? AND key = 'rg_track_gain'",
        &[&id],
    );
    // Les orphelins que la suppression laissait derrière elle, sur l'ancien
    // identifiant seulement (la base PostgreSQL est partagée).
    let orphelins = entier(
        db,
        "SELECT COUNT(*) FROM favorites f WHERE f.item_type = 'track' AND f.item_id = ? \
         AND NOT EXISTS (SELECT 1 FROM tracks t WHERE t.id = f.item_id)",
        &[&id_avant],
    ) + entier(
        db,
        "SELECT COUNT(*) FROM item_tags it JOIN tags t ON t.id = it.tag_id \
         WHERE t.name = ? AND NOT EXISTS (SELECT 1 FROM tracks tr WHERE tr.id = it.item_id)",
        &[&marque],
    );
    if orphelins > 0 {
        pertes.push(format!(
            "{orphelins} favori(s)/étiquette(s) orphelin(s) pointant sur l'ancien identifiant"
        ));
    }
    pertes
}

/// Mp3tag réenregistre la piste : titre corrigé, ALBUM posé, date avancée.
fn retoucher(piste: &Path) {
    baliser(
        piste,
        &[
            ("TITLE", "Speak To Me (2023 Remix)"),
            ("ARTIST", "Pink Floyd"),
            ("ALBUM", "The Dark Side Of The Moon"),
            ("TRACKNUMBER", "1"),
        ],
        std::time::SystemTime::now(),
    );
}

/// L'épreuve entière, pour un moteur et un type d'événement. Rend la racine
/// du coffret (pour le ménage PostgreSQL).
fn eprouver(db: &Arc<dyn DbBackend>, epreuve: &str, genre: ChangeType) -> PathBuf {
    let marque = format!("{MARQUE}-{epreuve}");
    let (racine, pistes) = coffret_indexe(db, epreuve);
    let piste = &pistes[0];
    rattacher_tout(db, piste, &racine, &marque);
    let id_avant = identifiant(db, piste);
    assert!(
        inventaire(db, piste, id_avant, &marque).is_empty(),
        "montage : tout doit être rattaché avant la retouche"
    );

    retoucher(piste);
    let change = FileChange {
        change_type: genre,
        path: piste.to_string_lossy().into_owned(),
    };
    reimporter_fichier_surveillant(db, &change, true);

    let ligne = TrackRepo::with_backend(db.clone())
        .get_by_path(&piste.to_string_lossy())
        .unwrap()
        .expect("la piste est toujours en base");
    assert_eq!(
        ligne.title, "Speak To Me (2023 Remix)",
        "la retouche est bien relue (sinon l'épreuve ne prouve rien)"
    );
    let pertes = inventaire(db, piste, id_avant, &marque);
    eprintln!("#5341 [{epreuve}] inventaire après la retouche : {pertes:#?}");
    assert!(
        pertes.is_empty(),
        "#5341 — une retouche de balises relue par le surveillant doit METTRE À JOUR la \
         ligne de la piste (même identifiant), pas la supprimer puis la recréer. Perdu : \
         {pertes:#?}"
    );
    racine.to_path_buf()
}

/// Windows et macOS, écriture EN PLACE : `Modified`.
#[test]
fn une_retouche_en_place_garde_l_identifiant_et_tout_ce_qui_s_y_rattache_5341() {
    let (_base, db) = base_fichier("en-place");
    eprouver(&db, "5341-en-place", ChangeType::Modified);
}

/// Windows, fichier REMPLACÉ par déplacement : la fusion du lot garde `Added`
/// sur un chemin déjà indexé (#4896).
#[test]
fn un_fichier_remplace_en_ajout_garde_aussi_son_identifiant_5341() {
    let (_base, db) = base_fichier("remplace");
    eprouver(&db, "5341-remplace", ChangeType::Added);
}

/// Garde : la ligne existante ne vote pas pour le dossier avec ses balises
/// d'AVANT, et le fichier relu ne devient pas son propre exemplaire.
#[test]
fn la_piste_relue_n_est_pas_son_propre_exemplaire_5341() {
    let (_base, db) = base_fichier("exemplaire");
    let (_racine, pistes) = coffret_indexe(&db, "5341-exemplaire");
    let piste = &pistes[0];
    retoucher(piste);
    let change = FileChange {
        change_type: ChangeType::Modified,
        path: piste.to_string_lossy().into_owned(),
    };
    reimporter_fichier_surveillant(&db, &change, true);
    let chemin = piste.to_string_lossy().into_owned();
    assert_eq!(
        entier(
            &db,
            "SELECT COUNT(*) FROM track_copies WHERE file_path = ?",
            &[&chemin]
        ),
        0,
        "le fichier relu est une piste, pas un exemplaire d'elle-même"
    );
    assert_eq!(
        entier(
            &db,
            "SELECT COUNT(*) FROM tracks WHERE file_path = ?",
            &[&chemin]
        ),
        1,
        "une seule ligne pour le fichier"
    );
}

/// La même épreuve sur une VRAIE base PostgreSQL. Variable ABSENTE ⇒ saut
/// annoncé ; POSÉE ⇒ l'épreuve tourne (étape dédiée de `test-postgres.yml`).
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_5341_une_retouche_garde_l_identifiant_sur_postgresql() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT: TUNE_TEST_PG_URL absent — épreuve PostgreSQL #5341 sautée");
        return;
    };
    let pg = tune_core::db::postgres::PostgresDb::connect(&url)
        .await
        .expect("connexion PostgreSQL");
    tune_core::db::migrations::run_pg_migrations(pg.pool())
        .await
        .expect("migrations PostgreSQL");
    pg.ensure_schema().await;
    let db: Arc<dyn DbBackend> = Arc::new(tune_core::db::backend::PostgresBackend::new(
        pg.pool().clone(),
    ));
    let menage_des_marques = || {
        let motif = format!("{MARQUE}%");
        let _ = db.execute("DELETE FROM playlists WHERE name LIKE ?", &[&motif]);
        let _ = db.execute("DELETE FROM tags WHERE name LIKE ?", &[&motif]);
    };
    // Une exécution interrompue a pu laisser ses marques (noms uniques).
    tokio::task::block_in_place(menage_des_marques);
    let racines = tokio::task::block_in_place(|| {
        vec![
            eprouver(&db, "5341-pg-en-place", ChangeType::Modified),
            eprouver(&db, "5341-pg-remplace", ChangeType::Added),
        ]
    });
    // Ménage : nos seules lignes (cascades pour le reste).
    tokio::task::block_in_place(|| {
        for racine in &racines {
            let motif = format!("{}%", racine.to_string_lossy());
            let _ = db.execute(
                "DELETE FROM favorites WHERE item_type = 'track' AND item_id IN \
                 (SELECT id FROM tracks WHERE file_path LIKE ?)",
                &[&motif],
            );
            let _ = db.execute(
                "DELETE FROM listen_history WHERE track_id IN \
                 (SELECT id FROM tracks WHERE file_path LIKE ?)",
                &[&motif],
            );
            let _ = db.execute("DELETE FROM tracks WHERE file_path LIKE ?", &[&motif]);
        }
        menage_des_marques();
        let _ = tune_core::db::album_repo::AlbumRepo::with_backend(db.clone()).delete_orphans();
    });
    pg.pool().close().await;
}
