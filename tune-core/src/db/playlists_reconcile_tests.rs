//! #6179 — une piste supprimée ou ré-identifiée par un scan ne doit plus
//! sortir EN SILENCE des playlists locales.
//!
//! Les trois scénarios du ticket, sur une vraie base SQLite montée comme au
//! démarrage (schéma neuf + migrations, donc avec le déclencheur) :
//!
//! * un scan qui DÉPLACE le fichier (nouvelle ligne `tracks` au nouveau
//!   chemin, l'ancienne retirée) : la piste reste dans la playlist, à son rang ;
//! * un fichier SUPPRIMÉ : la ligne reste, « introuvable », avec de quoi la
//!   nommer — titre, artiste, album, durée, chemin ;
//! * le fichier qui REVIENT : la ligne est rétablie, au même rang.
//!
//! Les assertions lisent `playlist_tracks` en SQL brut : elles ne dépendent
//! d'aucune fonction nouvelle, si bien que sur le code d'avant le correctif
//! elles échouent sur le FOND (la ligne a disparu), pas sur la compilation.

use std::sync::Arc;

use super::PlaylistsReconciler;
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::migrations;
use crate::db::playlist_repo::PlaylistRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

/// Insère et rend l'id — par `execute_returning_id`, qui vaut aussi sous
/// PostgreSQL (`pg_6179_*`).
fn id(db: &Arc<dyn DbBackend>, sql: &str, params: &[&dyn ToSqlValue]) -> i64 {
    db.execute_returning_id(sql, params).unwrap()
}

struct Semis {
    playlist: i64,
    artiste: i64,
    album: i64,
    pistes: [i64; 3],
}

fn piste(db: &Arc<dyn DbBackend>, s: (&str, i64, i64, &str, Option<&str>, Option<&str>)) -> i64 {
    let (titre, artiste, album, chemin, empreinte, mbid) = s;
    id(
        db,
        "INSERT INTO tracks (title, artist_id, album_id, file_path, duration_ms, \
         audio_hash, musicbrainz_recording_id) VALUES (?, ?, ?, ?, 250000, ?, ?)",
        &[&titre, &artiste, &album, &chemin, &empreinte, &mbid],
    )
}

/// Trois pistes d'un album, rangées dans une playlist aux rangs 0, 1, 2.
fn semer(db: &Arc<dyn DbBackend>) -> Semis {
    let artiste = id(
        db,
        "INSERT INTO artists (name) VALUES (?)",
        &[&"Nina Simone"],
    );
    let album = id(
        db,
        "INSERT INTO albums (title, artist_id, track_count) VALUES (?, ?, 3)",
        &[&"Pastel Blues", &artiste],
    );
    let pistes = [
        piste(
            db,
            (
                "Be My Husband",
                artiste,
                album,
                "/m/pb/01.flac",
                Some("h-01"),
                None,
            ),
        ),
        piste(
            db,
            (
                "Nobody",
                artiste,
                album,
                "/m/pb/02.flac",
                Some("h-02"),
                Some("mbid-02"),
            ),
        ),
        piste(
            db,
            (
                "Sinnerman",
                artiste,
                album,
                "/m/pb/03.flac",
                Some("h-03"),
                None,
            ),
        ),
    ];
    let repo = PlaylistRepo::with_backend(db.clone());
    let playlist = repo.create("Du soir", None, 1).unwrap();
    repo.add_tracks(playlist, &pistes, None).unwrap();
    Semis {
        playlist,
        artiste,
        album,
        pistes,
    }
}

/// Les `track_id` de la playlist dans l'ordre (`None` = ligne sans piste).
fn rangs(db: &Arc<dyn DbBackend>, playlist: i64) -> Vec<Option<i64>> {
    db.query_many(
        "SELECT track_id FROM playlist_tracks WHERE playlist_id = ? ORDER BY position, id",
        &[&playlist],
    )
    .unwrap()
    .iter()
    .map(|r| r.first().and_then(|v| v.as_i64()))
    .collect()
}

fn reconcilier(db: &Arc<dyn DbBackend>) {
    PlaylistsReconciler::with_backend(db.clone()).run().unwrap();
}

#[test]
fn un_scan_qui_deplace_le_fichier_garde_la_piste_dans_la_playlist() {
    let db = base();
    let s = semer(&db);
    // Le scan voit le fichier sous son nouveau chemin (même contenu, donc
    // même `audio_hash`), l'insère sous un NOUVEL id, puis retire l'ancien
    // chemin disparu — l'ordre de la purge post-scan.
    let deplacee = piste(
        &db,
        (
            "Nobody",
            s.artiste,
            s.album,
            "/m/rangee/02.flac",
            Some("h-02"),
            Some("mbid-02"),
        ),
    );
    TrackRepo::with_backend(db.clone())
        .delete_by_path("/m/pb/02.flac")
        .unwrap();
    reconcilier(&db);
    assert_eq!(
        rangs(&db, s.playlist),
        vec![Some(s.pistes[0]), Some(deplacee), Some(s.pistes[2])],
        "la piste déplacée doit rester dans la playlist, au même rang"
    );
}

#[test]
fn un_fichier_supprime_laisse_une_ligne_introuvable_et_non_un_trou() {
    let db = base();
    let s = semer(&db);
    TrackRepo::with_backend(db.clone())
        .delete_by_path("/m/pb/02.flac")
        .unwrap();
    reconcilier(&db);
    assert_eq!(
        rangs(&db, s.playlist),
        vec![Some(s.pistes[0]), None, Some(s.pistes[2])],
        "la ligne de la piste disparue doit RESTER, à son rang, sans piste"
    );
    let ligne = db
        .query_one(
            "SELECT title, artist, album, duration_ms, file_path, audio_hash, mbid, \
             missing_since, source FROM playlist_tracks \
             WHERE playlist_id = ? AND track_id IS NULL",
            &[&s.playlist],
        )
        .unwrap()
        .expect("la ligne introuvable");
    let texte = |i: usize| ligne.get(i).and_then(|v| v.as_string());
    assert_eq!(texte(0).as_deref(), Some("Nobody"));
    assert_eq!(texte(1).as_deref(), Some("Nina Simone"));
    assert_eq!(texte(2).as_deref(), Some("Pastel Blues"));
    assert_eq!(ligne.get(3).and_then(|v| v.as_i64()), Some(250_000));
    assert_eq!(texte(4).as_deref(), Some("/m/pb/02.flac"));
    assert_eq!(texte(5).as_deref(), Some("h-02"));
    assert_eq!(texte(6).as_deref(), Some("mbid-02"));
    assert!(
        ligne.get(7).and_then(|v| v.as_i64()).is_some_and(|t| t > 0),
        "la date de disparition est posée"
    );
    assert!(
        texte(8).is_none(),
        "une ligne introuvable n'est pas un titre de service"
    );
}

#[test]
fn le_fichier_qui_revient_retablit_la_ligne_au_meme_rang() {
    let db = base();
    let s = semer(&db);
    TrackRepo::with_backend(db.clone())
        .delete_by_path("/m/pb/02.flac")
        .unwrap();
    reconcilier(&db);
    // Le disque est rebranché : le scan réinsère le fichier, sous un nouvel id.
    let revenue = piste(
        &db,
        (
            "Nobody",
            s.artiste,
            s.album,
            "/m/pb/02.flac",
            Some("h-02"),
            Some("mbid-02"),
        ),
    );
    assert_ne!(revenue, s.pistes[1], "l'id a bien changé");
    reconcilier(&db);
    assert_eq!(
        rangs(&db, s.playlist),
        vec![Some(s.pistes[0]), Some(revenue), Some(s.pistes[2])],
        "la ligne doit être rétablie, au même rang"
    );
}

#[test]
fn vider_la_bibliotheque_puis_rescanner_rend_la_playlist_entiere() {
    let db = base();
    let s = semer(&db);
    TrackRepo::with_backend(db.clone()).delete_all().unwrap();
    assert_eq!(
        rangs(&db, s.playlist),
        vec![None, None, None],
        "« Vider la bibliothèque » laisse les lignes, introuvables"
    );
    // Rescan : tout revient, dans l'ordre du parcours, sous de nouveaux ids.
    let artiste = id(
        &db,
        "INSERT INTO artists (name) VALUES (?)",
        &[&"Nina Simone"],
    );
    let album = id(
        &db,
        "INSERT INTO albums (title, artist_id, track_count) VALUES (?, ?, 3)",
        &[&"Pastel Blues", &artiste],
    );
    let c = piste(
        &db,
        (
            "Sinnerman",
            artiste,
            album,
            "/m/pb/03.flac",
            Some("h-03"),
            None,
        ),
    );
    let a = piste(
        &db,
        (
            "Be My Husband",
            artiste,
            album,
            "/m/pb/01.flac",
            Some("h-01"),
            None,
        ),
    );
    let b = piste(
        &db,
        (
            "Nobody",
            artiste,
            album,
            "/m/pb/02.flac",
            Some("h-02"),
            Some("mbid-02"),
        ),
    );
    reconcilier(&db);
    assert_eq!(rangs(&db, s.playlist), vec![Some(a), Some(b), Some(c)]);
}

/// Contre-épreuve de prudence : sans chemin, empreinte ni MBID qui
/// concordent, deux homonymes (même titre, même artiste, deux albums) ne
/// donnent AUCUN gagnant. La ligne reste introuvable plutôt que de pointer
/// la mauvaise version.
#[test]
fn deux_homonymes_ne_rattachent_pas_au_hasard() {
    let db = base();
    let artiste = id(
        &db,
        "INSERT INTO artists (name) VALUES (?)",
        &[&"Nina Simone"],
    );
    let live = id(
        &db,
        "INSERT INTO albums (title, artist_id) VALUES (?, ?)",
        &[&"Live at Montreux", &artiste],
    );
    let studio = id(
        &db,
        "INSERT INTO albums (title, artist_id) VALUES (?, ?)",
        &[&"Pastel Blues", &artiste],
    );
    let disparue = piste(
        &db,
        ("Sinnerman", artiste, studio, "/m/a.flac", Some("h-a"), None),
    );
    let repo = PlaylistRepo::with_backend(db.clone());
    let pl = repo.create("Une", None, 1).unwrap();
    repo.add_tracks(pl, &[disparue], None).unwrap();
    TrackRepo::with_backend(db.clone())
        .delete(disparue)
        .unwrap();
    // Deux candidates du même titre et du même artiste, sur deux AUTRES albums
    // que celui de la piste disparue, et rien d'autre en commun avec elle.
    let autre = id(
        &db,
        "INSERT INTO albums (title, artist_id) VALUES (?, ?)",
        &[&"Anthologie", &artiste],
    );
    piste(
        &db,
        ("Sinnerman", artiste, live, "/m/b.flac", Some("h-b"), None),
    );
    piste(
        &db,
        ("Sinnerman", artiste, autre, "/m/c.flac", Some("h-c"), None),
    );
    reconcilier(&db);
    assert_eq!(rangs(&db, pl), vec![None], "aucun rattachement au hasard");
}

/// La ligne introuvable se LIT (`get_entries`) à son rang, avec l'identité de
/// la piste disparue — c'est ce que `GET /playlists/{id}/tracks` rend.
#[test]
fn la_ligne_introuvable_se_lit_a_son_rang_avec_son_identite() {
    use crate::db::playlist_repo::EntryContent;
    let db = base();
    let s = semer(&db);
    TrackRepo::with_backend(db.clone())
        .delete_by_path("/m/pb/02.flac")
        .unwrap();
    let lignes = PlaylistRepo::with_backend(db.clone())
        .get_entries(s.playlist)
        .unwrap();
    assert_eq!(lignes.len(), 3);
    assert_eq!(lignes[0].content, EntryContent::Local(s.pistes[0]));
    let m = lignes[1]
        .missing()
        .expect("le rang 1 est la ligne introuvable");
    assert_eq!(m.title.as_deref(), Some("Nobody"));
    assert_eq!(m.artist.as_deref(), Some("Nina Simone"));
    assert_eq!(m.album.as_deref(), Some("Pastel Blues"));
    assert_eq!(m.duration_ms, Some(250_000));
    assert_eq!(m.file_path.as_deref(), Some("/m/pb/02.flac"));
    let json = m.to_json();
    assert_eq!(json["missing"], true);
    assert!(json["id"].is_null());
    assert_eq!(json["title"], "Nobody");
    assert_eq!(lignes[2].content, EntryContent::Local(s.pistes[2]));
}

/// Les gestes sur la playlist gardent la ligne introuvable : l'ancien
/// réordonnancement par `track_ids` (qui réécrit tout) la laisse à son rang,
/// le réordonnancement par rangs la déplace, la duplication la recopie. Seul
/// « Retirer » (`remove_track`) l'efface.
#[test]
fn les_gestes_gardent_la_ligne_introuvable_et_seul_retirer_l_efface() {
    let db = base();
    let s = semer(&db);
    TrackRepo::with_backend(db.clone())
        .delete_by_path("/m/pb/02.flac")
        .unwrap();
    let repo = PlaylistRepo::with_backend(db.clone());
    // Ancienne forme : les pistes locales inversées, la ligne introuvable reste.
    repo.reorder_tracks(s.playlist, &[s.pistes[2], s.pistes[0]])
        .unwrap();
    assert_eq!(
        rangs(&db, s.playlist),
        vec![Some(s.pistes[2]), None, Some(s.pistes[0])],
        "la réécriture par track_ids ne doit pas effacer la ligne introuvable"
    );
    // Par rangs : elle se déplace comme les autres, son identité la suit.
    assert!(repo.reorder_by_ranks(s.playlist, &[1, 0, 2]).unwrap());
    let lignes = repo.get_entries(s.playlist).unwrap();
    assert_eq!(
        lignes[0]
            .missing()
            .and_then(|m| m.file_path.clone())
            .as_deref(),
        Some("/m/pb/02.flac")
    );
    // Duplication : recopiée, identité comprise (empreinte incluse).
    let copie = crate::db::playlist_repo::PlaylistRepo::with_backend(db.clone())
        .create_with_entries(
            "Copie",
            None,
            1,
            &lignes.iter().map(|e| e.content.clone()).collect::<Vec<_>>(),
        )
        .unwrap()
        .0;
    assert_eq!(
        rangs(&db, copie),
        vec![None, Some(s.pistes[2]), Some(s.pistes[0])]
    );
    let empreinte = db
        .query_one(
            "SELECT audio_hash FROM playlist_tracks WHERE playlist_id = ? AND track_id IS NULL",
            &[&copie],
        )
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_string()));
    assert_eq!(empreinte.as_deref(), Some("h-02"));
    // La réconciliation n'efface rien de ce qu'elle ne retrouve pas…
    reconcilier(&db);
    assert_eq!(rangs(&db, s.playlist).len(), 3);
    // …et « Retirer » au rang 0 l'efface, elle seule.
    repo.remove_track(s.playlist, 0).unwrap();
    assert_eq!(
        rangs(&db, s.playlist),
        vec![Some(s.pistes[2]), Some(s.pistes[0])]
    );
}

/// La piste retrouvée était DÉJÀ dans la playlist (l'utilisateur l'y a remise
/// à la main) : la ligne introuvable s'y fond, pas de doublon.
#[test]
fn une_piste_deja_remise_absorbe_la_ligne_introuvable() {
    let db = base();
    let s = semer(&db);
    TrackRepo::with_backend(db.clone())
        .delete_by_path("/m/pb/02.flac")
        .unwrap();
    let revenue = piste(
        &db,
        (
            "Nobody",
            s.artiste,
            s.album,
            "/m/pb/02.flac",
            Some("h-02"),
            Some("mbid-02"),
        ),
    );
    PlaylistRepo::with_backend(db.clone())
        .add_tracks(s.playlist, &[revenue], None)
        .unwrap();
    let stats = PlaylistsReconciler::with_backend(db.clone()).run().unwrap();
    assert_eq!(stats.deduplicated, 1, "{stats:?}");
    assert_eq!(
        rangs(&db, s.playlist),
        vec![Some(s.pistes[0]), Some(s.pistes[2]), Some(revenue)]
    );
}

/// La passe RAPIDE du surveillant rattache par chemin et par empreinte, sans
/// la règle titre + artiste.
#[test]
fn la_passe_rapide_rattache_par_chemin_mais_pas_par_titre() {
    let db = base();
    let s = semer(&db);
    TrackRepo::with_backend(db.clone())
        .delete_by_path("/m/pb/02.flac")
        .unwrap();
    // Re-balisée ET déplacée : ni chemin, ni empreinte, ni MBID communs ;
    // seuls le titre et l'artiste la désignent.
    let rebalisee = piste(
        &db,
        (
            "Nobody",
            s.artiste,
            s.album,
            "/m/autre/02.flac",
            Some("h-zz"),
            None,
        ),
    );
    let rapide = PlaylistsReconciler::with_backend(db.clone())
        .run_rapide()
        .unwrap();
    assert_eq!((rapide.relinked, rapide.unresolved), (0, 1));
    let complete = PlaylistsReconciler::with_backend(db.clone()).run().unwrap();
    assert_eq!(complete.relinked, 1);
    assert_eq!(
        rangs(&db, s.playlist),
        vec![Some(s.pistes[0]), Some(rebalisee), Some(s.pistes[2])]
    );
}

/// #6179 sur PostgreSQL, les deux naissances (même découpe que `pg_4889`).
///
/// * native (001 → 072 → 089 + déclencheur) : la clé étrangère passe en
///   `SET NULL`, le déclencheur fige l'identité, la ligne reste à son rang ;
///   la piste qui revient au même chemin est rattachée par la réconciliation ;
/// * bascule (`PG_FULL_SCHEMA` tout-TEXT, une ligne copiée, 072, 089 deux
///   fois) : `missing_since` convertie en BIGINT, le nouveau CHECK admet la
///   ligne introuvable et refuse toujours « locale ET service ».
///
/// Sauté sans `TUNE_TEST_PG_URL`.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_6179_la_ligne_survit_a_sa_piste_sur_les_deux_naissances() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT: TUNE_TEST_PG_URL absent");
        return;
    };

    async fn base(url: &str, schema: &'static str) -> sqlx::PgPool {
        let maintenance = sqlx::PgPool::connect(url).await.unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema}"
        )))
        .execute(&maintenance)
        .await
        .unwrap();
        maintenance.close().await;
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .after_connect(move |c, _| {
                Box::pin(async move {
                    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET search_path TO {schema}")))
                        .execute(c)
                        .await
                        .map(|_| ())
                })
            })
            .connect(url)
            .await
            .unwrap()
    }

    // 1. Installation native.
    let pool = base(&url, "playlists_6179_natif").await;
    for sql in [
        include_str!("../../migrations/postgres/001_initial_schema.sql"),
        "ALTER TABLE playlists ADD COLUMN IF NOT EXISTS profile_id BIGINT NOT NULL DEFAULT 1;
         ALTER TABLE tracks ADD COLUMN IF NOT EXISTS audio_fingerprint TEXT;",
        include_str!("../../migrations/postgres/072_playlist_tracks_titres_de_service.sql"),
        include_str!("../../migrations/postgres/089_playlist_tracks_orphelines.sql"),
        include_str!("../../migrations/postgres/089_playlist_tracks_orphelines.sql"),
        include_str!("../../migrations/postgres/playlist_tracks_orphelines.sql"),
    ] {
        sqlx::raw_sql(sql).execute(&pool).await.unwrap();
    }
    let regle: String = sqlx::query_scalar(
        "SELECT confdeltype::text FROM pg_constraint \
         WHERE conrelid = 'playlist_tracks'::regclass AND confrelid = 'tracks'::regclass \
           AND contype = 'f'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(regle, "n", "la clé étrangère de la 001 passe en SET NULL");
    let db: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
    let s = semer(&db);
    db.execute("DELETE FROM tracks WHERE id = ?", &[&s.pistes[1]])
        .unwrap();
    assert_eq!(
        rangs(&db, s.playlist),
        vec![Some(s.pistes[0]), None, Some(s.pistes[2])],
        "PostgreSQL : la ligne reste, à son rang"
    );
    let titre: Option<String> = sqlx::query_scalar(
        "SELECT title FROM playlist_tracks WHERE track_id IS NULL AND file_path = '/m/pb/02.flac'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(titre.as_deref(), Some("Nobody"));
    let revenue = piste(
        &db,
        (
            "Nobody",
            s.artiste,
            s.album,
            "/m/pb/02.flac",
            Some("h-02"),
            Some("mbid-02"),
        ),
    );
    reconcilier(&db);
    assert_eq!(
        rangs(&db, s.playlist),
        vec![Some(s.pistes[0]), Some(revenue), Some(s.pistes[2])]
    );
    pool.close().await;

    // 2. Bascule depuis SQLite.
    let pool = base(&url, "playlists_6179_bascule").await;
    sqlx::raw_sql(crate::db::pg_migrate::PG_FULL_SCHEMA)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(
        "CREATE TABLE IF NOT EXISTS schema_version (
             version INTEGER PRIMARY KEY, name TEXT, applied_at TIMESTAMPTZ DEFAULT now());
         INSERT INTO playlist_tracks (id, playlist_id, track_id, position) VALUES ('8', '3', '41', '0');
         INSERT INTO playlist_tracks (id, playlist_id, track_id, position, title, file_path, missing_since)
             VALUES ('9', '3', NULL, '1', 'Copiée introuvable', '/m/x.flac', '1760000000');",
    )
    .execute(&pool)
    .await
    .unwrap();
    for sql in [
        include_str!("../../migrations/postgres/072_playlist_tracks_titres_de_service.sql"),
        include_str!("../../migrations/postgres/089_playlist_tracks_orphelines.sql"),
        include_str!("../../migrations/postgres/089_playlist_tracks_orphelines.sql"),
    ] {
        sqlx::raw_sql(sql).execute(&pool).await.unwrap();
    }
    let (typ, depuis): (String, Option<i64>) = sqlx::query_as(
        "SELECT (SELECT data_type::text FROM information_schema.columns \
                  WHERE table_schema = current_schema() AND table_name = 'playlist_tracks' \
                    AND column_name = 'missing_since'), \
                (SELECT missing_since FROM playlist_tracks WHERE id = '9')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((typ.as_str(), depuis), ("bigint", Some(1_760_000_000)));
    assert!(
        sqlx::raw_sql(
            "INSERT INTO playlist_tracks (id, playlist_id, track_id, position, source, source_id) \
             VALUES ('10', '3', '41', '2', 'qobuz', '1')"
        )
        .execute(&pool)
        .await
        .is_err(),
        "le CHECK refuse toujours une ligne locale ET de service"
    );
    pool.close().await;

    let maintenance = sqlx::PgPool::connect(&url).await.unwrap();
    sqlx::raw_sql(
        "DROP SCHEMA IF EXISTS playlists_6179_natif CASCADE; \
         DROP SCHEMA IF EXISTS playlists_6179_bascule CASCADE;",
    )
    .execute(&maintenance)
    .await
    .unwrap();
    maintenance.close().await;
}
