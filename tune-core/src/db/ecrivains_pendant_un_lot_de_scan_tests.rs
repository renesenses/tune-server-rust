//! Les écrivains qui ne prenaient pas la porte du scan écrivaient DANS la
//! transaction d'un lot de scan.
//!
//! Un lot de scan tient `BEGIN IMMEDIATE` sur l'unique connexion d'écriture,
//! à travers des centaines d'appels. Entre deux appels, le verrou de la
//! connexion est libre : un favori, une note, une édition manuelle, une
//! playlist ou une écriture de l'enrichissement passait alors dans SA
//! transaction. Un `ROLLBACK` du lot l'emportait ; la relecture par le pool de
//! lecture ne le voyait pas ; un `write_tx` échouait net.
//!
//! #5072 avait prouvé et corrigé ce défaut pour le seul surveillant. Ces
//! épreuves le prouvent pour les autres écrivains, par les dépôts réels, et
//! le corrigé est générique : c'est la connexion d'écriture qui fait attendre
//! tout fil qui n'est pas le propriétaire de la transaction ouverte
//! (`transaction_du_lot.rs`).
//!
//! ⚠️ Base de FICHIER obligatoire : sur `:memory:`, le pool de lecture est la
//! connexion d'écriture elle-même, qui voit la transaction en cours.
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::db::album_metadata_repo::AlbumMetadataRepo;
use crate::db::backend::DbBackend;
use crate::db::models::Track;
use crate::db::playlist_repo::PlaylistRepo;
use crate::db::profile_repo::ProfileRepo;
use crate::db::rating_repo::RatingRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;

/// Durée pendant laquelle le lot simulé garde sa transaction ouverte.
const LOT: Duration = Duration::from_millis(400);

fn base_fichier(epreuve: &str) -> (crate::test_scratch::ScratchDir, Arc<dyn DbBackend>) {
    let (dossier, db) = base_fichier_sqlite(epreuve);
    (dossier, db)
}

fn base_fichier_sqlite(epreuve: &str) -> (crate::test_scratch::ScratchDir, Arc<SqliteDb>) {
    let dossier = crate::test_scratch::scratch_dir(&format!("ecrivains-lot-{epreuve}"));
    let chemin = dossier.join("tune-epreuve.db");
    let db = SqliteDb::open(&chemin.to_string_lossy()).expect("base de fichier");
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    db.execute(
        "INSERT INTO albums (id, title) VALUES (1, 'Album témoin')",
        &[],
    )
    .unwrap();
    (dossier, Arc::new(db))
}

#[derive(Clone, Copy)]
enum Issue {
    Commit,
    Rollback,
}

/// Un lot de scan en vol, sur un autre fil : `BEGIN IMMEDIATE`, une écriture
/// à lui, sa transaction tenue `LOT`, puis `COMMIT` ou `ROLLBACK`.
/// `ecrivain` agit pendant ce temps. Rend ce que l'écrivain rend et le temps
/// qu'il a mis.
fn pendant_un_lot<R>(
    db: &Arc<dyn DbBackend>,
    issue: Issue,
    ecrivain: impl FnOnce() -> R,
) -> (R, Duration) {
    let (ouvert, lot_ouvert) = mpsc::channel::<()>();
    let db_du_lot = db.clone();
    let lot = std::thread::spawn(move || {
        db_du_lot
            .execute_batch("BEGIN IMMEDIATE")
            .expect("BEGIN du lot");
        db_du_lot
            .execute(
                "INSERT INTO tracks (title, file_path) VALUES ('du lot', '/lot/une.flac')",
                &[],
            )
            .expect("écriture du lot");
        ouvert.send(()).unwrap();
        std::thread::sleep(LOT);
        match issue {
            Issue::Commit => db_du_lot.execute_batch("COMMIT").expect("COMMIT du lot"),
            Issue::Rollback => db_du_lot
                .execute_batch("ROLLBACK")
                .expect("ROLLBACK du lot"),
        }
    });
    lot_ouvert.recv().expect("le lot a ouvert sa transaction");
    let debut = Instant::now();
    let r = ecrivain();
    let duree = debut.elapsed();
    lot.join().expect("fil du lot");
    (r, duree)
}

fn piste(db: &Arc<dyn DbBackend>, titre: &str) -> i64 {
    let mut t = Track::new(titre.into());
    t.file_path = Some(format!("/musique/{titre}.flac"));
    TrackRepo::with_backend(db.clone()).create(&t).unwrap()
}

#[test]
fn temoin_hors_lot_un_favori_pose_reste_en_base() {
    let (_d, db) = base_fichier("temoin");
    let profils = ProfileRepo::with_backend(db.clone());
    profils.add_favorite(1, "track", 42).unwrap();
    assert!(profils.is_favorite(1, "track", 42).unwrap());
}

#[test]
fn un_favori_pose_pendant_un_lot_annule_reste_en_base() {
    let (_d, db) = base_fichier("favori");
    let profils = ProfileRepo::with_backend(db.clone());
    pendant_un_lot(&db, Issue::Rollback, || {
        profils.add_favorite(1, "track", 42).unwrap()
    });
    assert!(
        profils.is_favorite(1, "track", 42).unwrap(),
        "le favori posé pendant le lot a été emporté par son ROLLBACK"
    );
}

#[test]
fn une_note_d_album_posee_pendant_un_lot_annule_reste_en_base() {
    let (_d, db) = base_fichier("note");
    let notes = RatingRepo::with_backend(db.clone());
    pendant_un_lot(&db, Issue::Rollback, || {
        notes.rate_album(1, 1, 4, Some("belle prise")).unwrap()
    });
    let note = notes.get_rating(1, 1).unwrap();
    assert!(
        note.is_some(),
        "la note posée pendant le lot a été emportée par son ROLLBACK"
    );
}

#[test]
fn une_metadonnee_d_enrichissement_ecrite_pendant_un_lot_annule_reste_en_base() {
    let (_d, db) = base_fichier("enrichissement");
    let meta = AlbumMetadataRepo::with_backend(db.clone());
    pendant_un_lot(&db, Issue::Rollback, || {
        meta.set(1, "wikipedia_summary", "Résumé").unwrap()
    });
    assert_eq!(
        meta.get_all(1)
            .unwrap()
            .get("wikipedia_summary")
            .map(String::as_str),
        Some("Résumé"),
        "l'écriture de l'enrichissement a été emportée par le ROLLBACK du lot"
    );
}

#[test]
fn une_edition_manuelle_pendant_un_lot_se_relit_aussitot() {
    let (_d, db) = base_fichier("edition");
    let id = piste(&db, "Avant");
    let pistes = TrackRepo::with_backend(db.clone());
    let (relu, _) = pendant_un_lot(&db, Issue::Commit, || {
        let mut t = pistes.get(id).unwrap().unwrap();
        t.title = "Après".into();
        pistes.update(&t).unwrap();
        // La route d'édition rend la ligne relue : par le pool de lecture.
        pistes.get(id).unwrap().unwrap().title
    });
    assert_eq!(
        relu, "Après",
        "la relecture de l'édition ne voyait pas l'édition"
    );
}

#[test]
fn une_playlist_creee_pendant_un_lot_se_relit_aussitot() {
    let (_d, db) = base_fichier("playlist-creee");
    let playlists = PlaylistRepo::with_backend(db.clone());
    let (relue, _) = pendant_un_lot(&db, Issue::Commit, || {
        let id = playlists.create("Nouvelle", None, 1).unwrap();
        playlists.get(id).unwrap()
    });
    assert!(
        relue.is_some(),
        "la playlist créée pendant le lot était introuvable à la relecture"
    );
}

#[test]
fn un_ajout_a_une_playlist_pendant_un_lot_n_echoue_plus() {
    let (_d, db) = base_fichier("playlist-ajout");
    let tid = piste(&db, "Morceau");
    let playlists = PlaylistRepo::with_backend(db.clone());
    let pid = playlists.create("Existante", None, 1).unwrap();
    let (ajout, _) = pendant_un_lot(&db, Issue::Commit, || {
        playlists.add_tracks(pid, &[tid], None)
    });
    assert!(ajout.is_ok(), "ajout refusé pendant le lot : {ajout:?}");
    assert_eq!(playlists.get_track_ids(pid).unwrap(), vec![tid]);
}

#[test]
fn une_lecture_forte_n_attend_pas_la_fin_du_lot() {
    // La lecture par la connexion d'écriture sert la lecture audio (file,
    // zones) : elle ne doit pas attendre qu'un lot se termine.
    let (_d, db) = base_fichier("lecture-forte");
    let (_, duree) = pendant_un_lot(&db, Issue::Commit, || {
        db.query_one_strong("SELECT COUNT(*) FROM tracks", &[])
            .unwrap()
    });
    assert!(
        duree < LOT / 2,
        "la lecture forte a attendu le lot : {duree:?}"
    );
}

/// Un long lot, fichier par fichier, comme le scan : `fichiers` unités de
/// `par_fichier` chacune, une ligne écrite par unité, et — si `cede` — le
/// point de cession du scan entre deux unités. `ecrivain` démarre une fois
/// le lot bien entamé. Rend le temps d'attente de l'écrivain.
fn pendant_un_long_lot(
    db: &Arc<dyn DbBackend>,
    fichiers: usize,
    par_fichier: Duration,
    cede: bool,
    ecrivain: impl FnOnce(),
) -> Duration {
    let (ouvert, lot_ouvert) = mpsc::channel::<()>();
    let db_du_lot = db.clone();
    let lot = std::thread::spawn(move || {
        db_du_lot
            .execute_batch("BEGIN IMMEDIATE")
            .expect("BEGIN du lot");
        for i in 0..fichiers {
            if cede {
                db_du_lot.ceder_aux_ecrivains();
            }
            let chemin = format!("/lot/{i}.flac");
            let p: [&dyn crate::db::backend::ToSqlValue; 1] = [&chemin];
            db_du_lot
                .execute(
                    "INSERT INTO tracks (title, file_path) VALUES ('du lot', ?)",
                    &p,
                )
                .expect("écriture du lot");
            if i == 2 {
                ouvert.send(()).unwrap();
            }
            std::thread::sleep(par_fichier);
        }
        db_du_lot.execute_batch("COMMIT").expect("COMMIT du lot");
    });
    lot_ouvert.recv().expect("le lot est entamé");
    let debut = Instant::now();
    ecrivain();
    let attente = debut.elapsed();
    lot.join().expect("fil du lot");
    attente
}

#[test]
fn un_long_lot_cede_la_place_a_un_favori_entre_deux_fichiers() {
    let (_d, sqlite) = base_fichier_sqlite("cession");
    let db: Arc<dyn DbBackend> = sqlite.clone();
    let profils = ProfileRepo::with_backend(db.clone());
    let (ouvert_tx, ouvert_rx) = mpsc::channel();
    let (ceder_tx, ceder_rx) = mpsc::channel();
    let (continuer_tx, continuer_rx) = mpsc::channel();
    let db_lot = db.clone();
    let lot = std::thread::spawn(move || {
        db_lot.execute_batch("BEGIN IMMEDIATE").unwrap();
        for i in 0..40 {
            let chemin = format!("/lot/{i}.flac");
            db_lot
                .execute(
                    "INSERT INTO tracks (title, file_path) VALUES ('du lot', ?)",
                    &[&chemin],
                )
                .unwrap();
            if i == 2 {
                ouvert_tx.send(()).unwrap();
                ceder_rx.recv().unwrap();
                db_lot.ceder_aux_ecrivains();
                // Ne pas finir le lot avant que le test ait observé le favori.
                continuer_rx.recv().unwrap();
            }
        }
        db_lot.execute_batch("COMMIT").unwrap();
    });
    ouvert_rx.recv_timeout(Duration::from_secs(15)).unwrap();
    let (favori_tx, favori_rx) = mpsc::channel();
    let db_favori = db.clone();
    let favori = std::thread::spawn(move || {
        ProfileRepo::with_backend(db_favori)
            .add_favorite(1, "album", 7)
            .unwrap();
        favori_tx.send(()).unwrap();
    });
    let limite = Instant::now() + Duration::from_secs(15);
    while sqlite.connection().ecrivains_du_lot_en_attente_pour_essai() == 0
        && Instant::now() < limite
    {
        std::thread::yield_now();
    }
    let en_attente = sqlite.connection().ecrivains_du_lot_en_attente_pour_essai();
    ceder_tx.send(()).unwrap();
    let avant_fin = favori_rx.recv_timeout(Duration::from_secs(15)).is_ok();
    // Toujours libérer les fils, même lors de la contre-épreuve.
    continuer_tx.send(()).unwrap();
    lot.join().unwrap();
    favori.join().unwrap();
    assert!(
        en_attente > 0,
        "le favori doit attendre hors de la transaction du lot"
    );
    assert!(
        avant_fin,
        "le lot ne cède pas : le favori ne finit qu'après le COMMIT final"
    );
    assert!(profils.is_favorite(1, "album", 7).unwrap());
    let lignes = db
        .query_one("SELECT COUNT(*) FROM tracks WHERE title = 'du lot'", &[])
        .unwrap()
        .and_then(|l| l[0].as_i64());
    assert_eq!(lignes, Some(40), "le lot cédant a perdu des lignes");
}

#[test]
fn sans_cession_un_favori_attend_la_fin_du_lot_sans_y_entrer() {
    // La même chose sans point de cession : l'écrivain n'entre plus dans la
    // transaction du lot, mais il en attend la fin. C'est la mesure de ce que
    // coûterait la seule attente.
    let (_d, db) = base_fichier("sans-cession");
    let profils = ProfileRepo::with_backend(db.clone());
    let attente = pendant_un_long_lot(&db, 40, Duration::from_millis(50), false, || {
        profils.add_favorite(1, "album", 7).unwrap();
    });
    eprintln!(
        "attente du favori, lot qui ne cède pas : {} ms",
        attente.as_millis()
    );
    assert!(
        attente >= Duration::from_millis(1000),
        "le favori n'a pas attendu la fin du lot ({attente:?}) : il est entré dedans"
    );
    assert!(profils.is_favorite(1, "album", 7).unwrap());
}

#[test]
fn une_transaction_orpheline_ne_fait_attendre_qu_une_fois() {
    // Un fil ouvre une transaction et meurt sans la fermer. Le premier
    // écrivain attend la limite, puis écrit ; les suivants ne doivent plus
    // l'attendre, sinon chaque écriture du serveur coûterait la limite
    // entière, indéfiniment (voir l'en-tête de `transaction_du_lot.rs`).
    let dossier = crate::test_scratch::scratch_dir("ecrivains-lot-orpheline");
    let chemin = dossier.join("tune-epreuve.db");
    let sqlite = SqliteDb::open(&chemin.to_string_lossy()).expect("base de fichier");
    sqlite.init_schema().unwrap();
    crate::db::migrations::run_migrations(&sqlite).unwrap();
    // Une limite longue devant le temps d'une écriture, même sur une machine
    // chargée : un suivant qui l'attendrait encore se voit sans ambiguïté.
    let limite = Duration::from_secs(2);
    sqlite.connection().fixer_attente_max_pour_essai(limite);
    let sqlite = Arc::new(sqlite);
    let db: Arc<dyn DbBackend> = sqlite.clone();
    let profils = ProfileRepo::with_backend(db.clone());

    let db_du_fil = db.clone();
    std::thread::spawn(move || {
        db_du_fil
            .execute_batch("BEGIN IMMEDIATE")
            .expect("BEGIN orphelin");
    })
    .join()
    .expect("fil orphelin");

    let debut = Instant::now();
    profils.add_favorite(1, "track", 1).unwrap();
    let premier = debut.elapsed();
    eprintln!("orpheline : premier écrivain {} ms", premier.as_millis());
    assert!(
        premier >= limite,
        "le premier écrivain n'a pas attendu : {premier:?}"
    );
    for id in 2..=3 {
        let debut = Instant::now();
        profils.add_favorite(1, "track", id).unwrap();
        let suivant = debut.elapsed();
        eprintln!("orpheline : écrivain suivant {} ms", suivant.as_millis());
        assert!(
            suivant < limite,
            "un écrivain suivant attend encore la transaction orpheline : {suivant:?}"
        );
    }

    // Une fois l'orpheline fermée, un nouveau lot fait de nouveau attendre :
    // un favori posé pendant qu'il est annulé survit.
    db.execute_batch("COMMIT").expect("fermer l'orpheline");
    sqlite
        .connection()
        .fixer_attente_max_pour_essai(crate::db::transaction_du_lot::ATTENTE_MAX_FIN_DU_LOT);
    pendant_un_lot(&db, Issue::Rollback, || {
        profils.add_favorite(1, "track", 4).unwrap()
    });
    for id in 1..=4 {
        assert!(
            profils.is_favorite(1, "track", id).unwrap(),
            "favori {id} perdu"
        );
    }
}
