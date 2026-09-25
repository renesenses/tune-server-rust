use super::rows_to_json;
use crate::db::backend::DbBackend;
use crate::db::zone_repo::AutoplayMode;
use serde_json::Value;
use std::sync::Arc;

/// Continuation locale sans graine. Un album est joue entier dans son ordre ;
/// artiste, annee et pistes remplissent la file par lots de dix au plus.
/// Chaque appel tire un nouveau groupe, sans ponderer celui-ci par sa taille.
/// Les pistes CUE sont eligibles via leur fichier media. Les entrees de
/// service et les pistes sans chemin local sont exclues. RandomYear ignore
/// les annees inconnues ; il utilise l'annee d'album si celle de la piste manque.
/// Aucun appel reseau et aucun repli vers la strategie Similar.
pub fn generate_random_queue(
    db: &Arc<dyn DbBackend>,
    mode: AutoplayMode,
) -> Result<Vec<Value>, String> {
    let selection = match mode {
        AutoplayMode::RandomAlbum => {
            "WHERE t.album_id = (SELECT album_id FROM eligible WHERE album_id IS NOT NULL \
             GROUP BY album_id ORDER BY RANDOM() LIMIT 1) \
             ORDER BY COALESCE(t.disc_number, 1), COALESCE(t.track_number, 0), t.id"
        }
        AutoplayMode::RandomArtist => {
            "WHERE t.artist_id = (SELECT artist_id FROM eligible WHERE artist_id IS NOT NULL \
             GROUP BY artist_id ORDER BY RANDOM() LIMIT 1) ORDER BY RANDOM() LIMIT 10"
        }
        AutoplayMode::RandomYear => {
            "WHERE t.autoplay_year = (SELECT autoplay_year FROM eligible WHERE autoplay_year > 0 \
             GROUP BY autoplay_year ORDER BY RANDOM() LIMIT 1) ORDER BY RANDOM() LIMIT 10"
        }
        AutoplayMode::RandomTracks => "ORDER BY RANDOM() LIMIT 10",
        AutoplayMode::Off | AutoplayMode::Similar => return Ok(Vec::new()),
    };
    // #4806 — les titres bannis par le profil actif ne sont pas éligibles :
    // un seul prédicat dans le CTE couvre les quatre modes.
    let profil = crate::db::hidden_repo::profil_de_selection_automatique(db);
    let sans_bannis = crate::db::facet_filter::banned_tracks_excluded(profil);
    let sql = format!(
        "WITH eligible AS (SELECT t.*, COALESCE(NULLIF(t.year, 0), al.year) AS autoplay_year \
         FROM tracks t LEFT JOIN albums al ON al.id = t.album_id \
         WHERE COALESCE(NULLIF(t.source, ''), 'local') = 'local' \
         AND (NULLIF(TRIM(t.file_path), '') IS NOT NULL \
              OR NULLIF(TRIM(t.cue_media_path), '') IS NOT NULL) \
         AND {sans_bannis}) \
         SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.genre, t.autoplay_year, t.bpm \
         FROM eligible t LEFT JOIN artists ar ON ar.id = t.artist_id \
         LEFT JOIN albums al ON al.id = t.album_id {selection}"
    );
    db.query_many(&sql, &[]).map(|rows| rows_to_json(&rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::SqliteDb;
    use std::collections::HashSet;

    fn db() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    fn library(db: &Arc<dyn DbBackend>) {
        for group in 1i64..=2 {
            db.execute(
                "INSERT INTO artists (id, name) VALUES (?, ?)",
                &[&group, &format!("Artist {group}")],
            )
            .unwrap();
            db.execute(
                "INSERT INTO albums (id, title, artist_id, year) VALUES (?, ?, ?, ?)",
                &[&group, &format!("Album {group}"), &group, &(2000 + group)],
            )
            .unwrap();
            for number in 1i64..=12 {
                let id = group * 100 + number;
                db.execute("INSERT INTO tracks (id, title, album_id, artist_id, file_path, disc_number, track_number) \
                            VALUES (?, ?, ?, ?, ?, ?, ?)",
                    &[&id, &format!("Track {id}"), &group, &group, &format!("/{id}.flac"),
                      &((number - 1) / 6 + 1), &(7 - (number - 1) % 6)]).unwrap();
            }
        }
    }

    fn check_groups(db: &Arc<dyn DbBackend>) {
        for mode in [
            AutoplayMode::RandomAlbum,
            AutoplayMode::RandomArtist,
            AutoplayMode::RandomYear,
            AutoplayMode::RandomTracks,
        ] {
            let tracks = generate_random_queue(db, mode).unwrap();
            assert_eq!(
                tracks.len(),
                if mode == AutoplayMode::RandomAlbum {
                    12
                } else {
                    10
                },
                "{mode:?}"
            );
            let ids: Vec<i64> = tracks
                .iter()
                .map(|t| t["track_id"].as_i64().unwrap())
                .collect();
            assert_eq!(ids.iter().collect::<HashSet<_>>().len(), ids.len());
            assert!(
                ids.iter()
                    .all(|id| (101..=112).contains(id) || (201..=212).contains(id))
            );
            if mode != AutoplayMode::RandomTracks {
                let group = ids[0] / 100;
                assert!(
                    ids.iter().all(|id| id / 100 == group),
                    "un seul groupe : {tracks:?}"
                );
                assert!(tracks.iter().all(|t| t["year"] == 2000 + group));
                if mode == AutoplayMode::RandomAlbum {
                    let expected: Vec<i64> = [6, 5, 4, 3, 2, 1, 12, 11, 10, 9, 8, 7]
                        .iter()
                        .map(|n| group * 100 + n)
                        .collect();
                    assert_eq!(ids, expected, "album entier, disque puis numero de piste");
                }
            }
        }

        // Croiser les axes : un artiste et une annee traversent deux albums.
        // Sinon un generateur qui confondrait artiste, album et annee passerait.
        db.execute_batch(
            "UPDATE tracks SET artist_id = (id % 2) + 1,
            year = CASE WHEN id % 4 IN (0, 1) THEN 2010 ELSE 2020 END;",
        )
        .unwrap();
        for mode in [AutoplayMode::RandomArtist, AutoplayMode::RandomYear] {
            let tracks = generate_random_queue(db, mode).unwrap();
            assert_eq!(tracks.len(), 10);
            let artists: HashSet<_> = tracks
                .iter()
                .map(|t| t["artist"].as_str().unwrap())
                .collect();
            let years: HashSet<_> = tracks.iter().map(|t| t["year"].as_i64().unwrap()).collect();
            let albums: HashSet<_> = tracks
                .iter()
                .map(|t| t["album"].as_str().unwrap())
                .collect();
            assert_eq!(albums.len(), 2, "{mode:?} traverse les albums");
            match mode {
                AutoplayMode::RandomArtist => {
                    assert_eq!(artists.len(), 1, "un seul artiste");
                    assert_eq!(years.len(), 2, "ses pistes de plusieurs annees");
                }
                AutoplayMode::RandomYear => {
                    assert_eq!(years.len(), 1, "une seule annee");
                    assert_eq!(artists.len(), 2, "des artistes de cette annee");
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn autoplay_2271_selects_whole_album_or_ten_tracks_from_one_group() {
        let db = db();
        library(&db);
        check_groups(&db);
    }

    #[test]
    fn autoplay_2271_cue_is_local_but_remote_and_pathless_rows_are_not() {
        let db = db();
        db.execute_batch(
            "INSERT INTO tracks (id, title, source, file_path, cue_media_path) VALUES \
            (1, 'CUE', 'local', NULL, '/disc.flac'), (2, 'absent', 'local', NULL, NULL), \
            (3, 'Qobuz', 'qobuz', '/cache.flac', NULL), (4, 'empty', 'local', '  ', NULL);",
        )
        .unwrap();
        let tracks = generate_random_queue(&db, AutoplayMode::RandomTracks).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0]["track_id"], 1);
        for mode in [
            AutoplayMode::RandomAlbum,
            AutoplayMode::RandomArtist,
            AutoplayMode::RandomYear,
            AutoplayMode::Off,
            AutoplayMode::Similar,
        ] {
            assert!(
                generate_random_queue(&db, mode).unwrap().is_empty(),
                "{mode:?} has no matching group"
            );
        }
    }

    #[test]
    fn autoplay_2271_query_failure_is_not_an_empty_library() {
        let db: Arc<dyn DbBackend> = Arc::new(SqliteDb::open_in_memory().unwrap());
        assert!(generate_random_queue(&db, AutoplayMode::RandomTracks).is_err());
    }

    // Le filtre postgres_e2e de test-postgres.yml execute ce temoin avec
    // TUNE_TEST_PG_URL : il ne doit pas rester compile mais toujours saute.
    #[cfg(feature = "postgres")]
    #[tokio::test(flavor = "multi_thread")]
    async fn postgres_e2e_autoplay_2271_text_modes_and_random_queries() {
        let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
            eprintln!("SAUT : TUNE_TEST_PG_URL non posee — PostgreSQL non exerce");
            return;
        };
        for initial_type in ["INTEGER", "TEXT"] {
            // Tables TEMP sur une connexion unique : aucun schema ni donnees des
            // autres tests ne sont touches, meme avec une URL de CI partagee.
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect(&url)
                .await
                .unwrap();
            let db: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(pool));
            db.execute_batch(&format!("CREATE TEMP TABLE zones (id BIGINT PRIMARY KEY, autoplay_enabled {initial_type} DEFAULT '0');
            CREATE TEMP TABLE schema_version (version INTEGER PRIMARY KEY, name TEXT);
            CREATE TEMP TABLE artists (id BIGINT PRIMARY KEY, name TEXT);
            CREATE TEMP TABLE albums (id BIGINT PRIMARY KEY, title TEXT, artist_id BIGINT, year INTEGER);
            CREATE TEMP TABLE tracks (id BIGINT PRIMARY KEY, title TEXT, album_id BIGINT, artist_id BIGINT,
                file_path TEXT, cue_media_path TEXT, disc_number INTEGER, track_number INTEGER,
                duration_ms BIGINT DEFAULT 0, genre TEXT, year INTEGER, bpm DOUBLE PRECISION, source TEXT DEFAULT 'local');
            INSERT INTO zones (id, autoplay_enabled) VALUES (1, 0), (2, 1), (3, NULL);")).unwrap();
            if initial_type == "TEXT" {
                db.execute_batch(
                    "INSERT INTO zones VALUES (4, 'random_artist'), (5, 'future_mode');",
                )
                .unwrap();
            }
            let migration =
                include_str!("../../../migrations/postgres/064_zone_autoplay_mode_text.sql");
            for _ in 0..2 {
                db.execute_batch(migration).unwrap();
                let rows = db
                    .query_many("SELECT id, autoplay_enabled FROM zones ORDER BY id", &[])
                    .unwrap();
                assert_eq!(rows[0][1].as_str(), Some("0"));
                assert_eq!(rows[1][1].as_str(), Some("1"));
                assert!(rows[2][1].is_null());
                if initial_type == "TEXT" {
                    assert_eq!(rows[3][1].as_str(), Some("random_artist"));
                    assert_eq!(rows[4][1].as_str(), Some("future_mode"));
                }
            }
            db.execute_batch("INSERT INTO zones (id) VALUES (6);")
                .unwrap();
            let repo = crate::db::zone_repo::ZoneRepo::with_backend(db.clone());
            for name in AutoplayMode::NOMS {
                let mode = AutoplayMode::from_str_stocke(name).unwrap();
                repo.update_autoplay_mode(1, mode).unwrap();
                assert_eq!(repo.get_autoplay_mode(1), mode);
                assert_eq!(repo.get_autoplay_enabled(1), mode != AutoplayMode::Off);
            }
            for (enabled, mode) in [(true, AutoplayMode::Similar), (false, AutoplayMode::Off)] {
                repo.update_autoplay_enabled(1, enabled).unwrap();
                assert_eq!(repo.get_autoplay_mode(1), mode);
            }
            db.execute(
                "UPDATE zones SET autoplay_enabled = ? WHERE id = 1",
                &[&"future_mode"],
            )
            .unwrap();
            assert_eq!(repo.get_autoplay_mode(1), AutoplayMode::Similar);
            db.execute("UPDATE zones SET autoplay_enabled = NULL WHERE id = 1", &[])
                .unwrap();
            assert_eq!(repo.get_autoplay_mode(1), AutoplayMode::Off);
            assert_eq!(repo.get_autoplay_mode(6), AutoplayMode::Off);
            library(&db);
            check_groups(&db);
        }
    }

    /// #4806 — un titre banni ne sort d'AUCUN générateur de sélection
    /// automatique de tune-core : les quatre modes d'autoplay, l'enchaînement
    /// (`generate_queue`), l'ambiance, la radio d'artiste, la radio
    /// intelligente et les recommandations. Cent passes chacun sur une base
    /// de 24 pistes. Témoin : avant le bannissement, la piste sort ; après
    /// débannissement, elle ressort. Le profil est celui du serveur (1 par
    /// défaut), comme en production sans requête HTTP.
    #[test]
    fn un_titre_banni_ne_sort_d_aucun_generateur() {
        use crate::ai::recommendations::{get_recommendations, smart_radio};
        use crate::playback::auto_dj::{
            Mood, generate_mood_queue, generate_queue, tracks_for_artist_names,
        };

        let db = db();
        library(&db);
        let bannie = 105i64;
        let noms = vec!["Artist 1".to_string()];

        type Generateur<'a> = (&'a str, Box<dyn Fn() -> Vec<i64> + 'a>);
        let generateurs: Vec<Generateur> = vec![
            (
                "autoplay:tracks",
                Box::new(|| {
                    ids_de(&generate_random_queue(&db, AutoplayMode::RandomTracks).unwrap())
                }),
            ),
            (
                "autoplay:album",
                Box::new(|| {
                    ids_de(&generate_random_queue(&db, AutoplayMode::RandomAlbum).unwrap())
                }),
            ),
            (
                "autoplay:artist",
                Box::new(|| {
                    ids_de(&generate_random_queue(&db, AutoplayMode::RandomArtist).unwrap())
                }),
            ),
            (
                "autoplay:year",
                Box::new(|| ids_de(&generate_random_queue(&db, AutoplayMode::RandomYear).unwrap())),
            ),
            (
                "enchainement",
                Box::new(|| ids_de(&generate_queue(&db, 101, 20))),
            ),
            (
                "ambiance",
                Box::new(|| ids_de(&generate_mood_queue(&db, Mood::Party, 20))),
            ),
            (
                "radio_artiste",
                Box::new(|| ids_de(&tracks_for_artist_names(&db, &noms, 12, 12))),
            ),
            (
                "radio_intelligente",
                Box::new(|| {
                    smart_radio(&db, Some(101), None, None, 20)
                        .iter()
                        .map(|t| t.track_id)
                        .collect()
                }),
            ),
            (
                "recommandations",
                Box::new(|| {
                    get_recommendations(&db, None, 20)
                        .iter()
                        .map(|t| t.track_id)
                        .collect()
                }),
            ),
        ];

        // Témoin : chaque générateur ramène la piste au moins une fois en
        // cent passes (24 pistes, tirages de 10 à 20).
        for (nom, g) in &generateurs {
            let vue = (0..100).any(|_| g().contains(&bannie));
            assert!(
                vue,
                "témoin {nom} : la piste doit sortir avant le bannissement"
            );
        }

        let bans = crate::db::hidden_repo::HiddenRepo::with_backend(db.clone());
        assert!(bans.ban_track(1, bannie).unwrap());
        for (nom, g) in &generateurs {
            for _ in 0..100 {
                let tirage = g();
                assert!(
                    !tirage.contains(&bannie),
                    "{nom} : bannie et pourtant sélectionnée : {tirage:?}"
                );
            }
        }

        // Débannir rend tout.
        assert!(bans.unban_track(1, bannie).unwrap());
        for (nom, g) in &generateurs {
            let revue = (0..100).any(|_| g().contains(&bannie));
            assert!(revue, "{nom} : débannie, la piste doit ressortir");
        }
    }

    fn ids_de(tracks: &[Value]) -> Vec<i64> {
        tracks
            .iter()
            .filter_map(|t| t["track_id"].as_i64())
            .collect()
    }
}
