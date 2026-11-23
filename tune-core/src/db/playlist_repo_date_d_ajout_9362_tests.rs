//! #9362 (fil forum 3000, accord de Bertrand du 22/11/2026, rc15) — la date
//! d'ajout d'une ligne À SA PLAYLIST (`playlist_tracks.added_at`).
//!
//! Le client trie une playlist « par date d'ajout » : la date doit donc être
//! celle de l'AJOUT, pas celle de la dernière écriture. Or trois chemins
//! effacent puis réinsèrent toute la playlist : l'ancien réordonnancement par
//! `track_ids` (avec ou sans titres de service), et la synchronisation d'une
//! playlist de dossier (`set_tracks`, à chaque scan). Sans reprise, un simple
//! déplacement rajeunissait toute la playlist, et une ligne d'avant la
//! migration (date inconnue) recevait une date inventée.
//!
//! Contre-épreuve : `DatesDAjout::reprendre` qui rend toujours l'heure (le
//! comportement d'une réinsertion naïve) fait rougir les témoins de reprise.
use super::*;
use crate::db::migrations::run_migrations;

fn base() -> SqliteDb {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    db
}

/// Trois pistes locales 10, 11, 12.
fn avec_pistes(db: &SqliteDb) {
    db.execute_batch(
        "INSERT INTO tracks (id, title, file_path) VALUES
             (10, 'A', '/m/a.flac'), (11, 'B', '/m/b.flac'), (12, 'C', '/m/c.flac');",
    )
    .unwrap();
}

fn service(id: &str) -> ServiceEntry {
    ServiceEntry {
        source: "qobuz".into(),
        source_id: id.into(),
        title: format!("Titre {id}"),
        artist: None,
        album: None,
        album_source_id: None,
        duration_ms: Some(1000),
        cover_url: None,
    }
}

/// Pose à la main les dates des lignes, dans l'ordre d'affichage.
fn dater(db: &SqliteDb, repo: &PlaylistRepo, plid: i64, dates: &[Option<i64>]) {
    let lignes = repo.get_entries(plid).unwrap();
    assert_eq!(lignes.len(), dates.len());
    let conn = db.connection().lock().unwrap();
    for (l, d) in lignes.iter().zip(dates) {
        conn.execute(
            "UPDATE playlist_tracks SET added_at = ?1 WHERE id = ?2",
            rusqlite::params![d, l.id],
        )
        .unwrap();
    }
}

/// `(contenu court, date)` dans l'ordre d'affichage.
fn vues(repo: &PlaylistRepo, plid: i64) -> Vec<(String, Option<i64>)> {
    repo.get_entries(plid)
        .unwrap()
        .into_iter()
        .map(|l| {
            let nom = match &l.content {
                EntryContent::Local(t) => format!("L{t}"),
                EntryContent::Service(s) => format!("S{}", s.source_id),
                EntryContent::Missing(m) => format!("?{}", m.title.clone().unwrap_or_default()),
            };
            (nom, l.added_at)
        })
        .collect()
}

fn entre(date: Option<i64>, avant: i64, apres: i64) -> bool {
    date.is_some_and(|d| d >= avant && d <= apres)
}

#[test]
fn un_ajout_date_la_ligne_a_l_heure_de_l_ajout_9362() {
    let db = base();
    avec_pistes(&db);
    let repo = PlaylistRepo::new(db.clone());
    let plid = repo.create("Tests audio", None, 1).unwrap();
    let avant = maintenant_unix();
    repo.add_tracks_deduped(plid, &[10], None).unwrap();
    repo.add_entries_deduped(plid, &[EntryContent::Service(service("q1"))], None)
        .unwrap();
    let (copie, _) = repo
        .create_with_tracks("Copie", None, 1, &[11, 12])
        .unwrap();
    let apres = maintenant_unix();
    for (nom, date) in vues(&repo, plid).into_iter().chain(vues(&repo, copie)) {
        assert!(
            entre(date, avant, apres),
            "{nom} : date d'ajout {date:?} hors de [{avant}, {apres}]"
        );
    }
}

#[test]
fn deplacer_des_pistes_locales_garde_leur_date_null_compris_9362() {
    let db = base();
    avec_pistes(&db);
    let repo = PlaylistRepo::new(db.clone());
    let plid = repo.create("Classique", None, 1).unwrap();
    repo.add_tracks(plid, &[10, 11, 12], None).unwrap();
    dater(&db, &repo, plid, &[Some(100), None, Some(300)]);

    // L'ancienne forme (`track_ids`), sans titre de service : effacement
    // puis réinsertion.
    repo.reorder_tracks(plid, &[12, 10, 11]).unwrap();
    assert_eq!(
        vues(&repo, plid),
        vec![
            ("L12".into(), Some(300)),
            ("L10".into(), Some(100)),
            ("L11".into(), None),
        ],
        "chaque piste garde SA date ; l'inconnue reste inconnue"
    );

    // La forme par rangs ne réécrit que les positions.
    assert!(repo.reorder_by_ranks(plid, &[1, 2, 0]).unwrap());
    assert_eq!(
        vues(&repo, plid),
        vec![
            ("L10".into(), Some(100)),
            ("L11".into(), None),
            ("L12".into(), Some(300)),
        ]
    );
}

#[test]
fn deplacer_dans_une_playlist_mixte_garde_toutes_les_dates_9362() {
    let db = base();
    avec_pistes(&db);
    let repo = PlaylistRepo::new(db.clone());
    let plid = repo.create("Mixte", None, 1).unwrap();
    repo.add_entries_deduped(
        plid,
        &[
            EntryContent::Local(10),
            EntryContent::Service(service("q1")),
            EntryContent::Local(11),
        ],
        None,
    )
    .unwrap();
    dater(&db, &repo, plid, &[Some(100), Some(200), None]);

    // Un titre de service présent : `reorder_tracks` passe par
    // `rewrite_entries`. Une piste en plus (12) est un AJOUT.
    let avant = maintenant_unix();
    repo.reorder_tracks(plid, &[11, 10, 12]).unwrap();
    let apres = maintenant_unix();
    let v = vues(&repo, plid);
    assert_eq!(
        v[..3],
        [
            ("L11".to_string(), None),
            ("Sq1".to_string(), Some(200)),
            ("L10".to_string(), Some(100)),
        ],
        "déplacer n'est pas ajouter : {v:?}"
    );
    assert_eq!(v[3].0, "L12");
    assert!(
        entre(v[3].1, avant, apres),
        "la piste ajoutée par la réécriture est datée de l'heure : {v:?}"
    );
}

#[test]
fn une_piste_en_double_garde_ses_deux_dates_9362() {
    let db = base();
    avec_pistes(&db);
    let repo = PlaylistRepo::new(db.clone());
    let plid = repo.create("Doublons", None, 1).unwrap();
    // `add_tracks` (fusion sans dédoublonnage) garde les doublons.
    repo.add_tracks(plid, &[10, 11, 10], None).unwrap();
    dater(&db, &repo, plid, &[Some(100), Some(200), Some(300)]);
    repo.reorder_tracks(plid, &[11, 10, 10]).unwrap();
    assert_eq!(
        vues(&repo, plid),
        vec![
            ("L11".into(), Some(200)),
            ("L10".into(), Some(100)),
            ("L10".into(), Some(300)),
        ]
    );
}

#[test]
fn la_synchronisation_d_un_dossier_ne_rajeunit_pas_la_playlist_9362() {
    let db = base();
    avec_pistes(&db);
    let repo = PlaylistRepo::new(db.clone());
    let plid = repo.create("Dossier", None, 1).unwrap();
    repo.set_tracks(plid, &[10, 11]).unwrap();
    dater(&db, &repo, plid, &[Some(100), None]);

    // Le scan suivant : 11 a quitté le dossier, 12 y est entrée.
    let avant = maintenant_unix();
    repo.set_tracks(plid, &[12, 10]).unwrap();
    let apres = maintenant_unix();
    let v = vues(&repo, plid);
    assert_eq!(v.len(), 2);
    assert_eq!(v[0].0, "L12");
    assert!(entre(v[0].1, avant, apres), "12 est un ajout : {v:?}");
    assert_eq!(
        v[1],
        ("L10".to_string(), Some(100)),
        "10 était là : elle garde sa date"
    );
}

#[test]
fn la_migration_158_laisse_les_lignes_existantes_sans_date_9362() {
    let db = base();
    avec_pistes(&db);
    {
        let conn = db.connection().lock().unwrap();
        // Une base d'avant la 158 : la colonne n'existe pas.
        conn.execute_batch(
            "ALTER TABLE playlist_tracks DROP COLUMN added_at;
             INSERT INTO playlists (id, name) VALUES (1, 'Ancienne');
             INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES (1, 10, 0);
             DELETE FROM _migrations WHERE version >= 158;",
        )
        .unwrap();
    }
    run_migrations(&db).unwrap();
    assert_eq!(
        crate::db::migrations::current_version(&db).unwrap(),
        crate::db::migrations::latest_version()
    );
    let repo = PlaylistRepo::new(db.clone());
    assert_eq!(
        vues(&repo, 1),
        vec![("L10".into(), None)],
        "aucune date inventée pour une ligne d'avant la migration"
    );
    // Et la table accepte de nouveau un ajout daté.
    repo.add_tracks(1, &[11], None).unwrap();
    assert!(repo.get_entries(1).unwrap()[1].added_at.is_some());
}
