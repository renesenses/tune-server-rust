//! #8645 — une émission de podcast classée parmi les « Artistes les plus
//! écoutés » de l'accueil (Bertrand, 13/11/2026, .18 en rc11 : une émission
//! de FIP, six écoutes).
//!
//! Depuis #7965, un épisode s'écrit dans l'historique avec le NOM DE
//! L'ÉMISSION en `artist_name` et en `album_title` (`source = 'podcast'`,
//! `enregistrer_ecoute_podcast`). Les classements par artiste et par album
//! n'excluaient que `source = 'radio'`.
//!
//! Témoin, sur SQLite ET PostgreSQL (`pg_8645_…`, saute sans
//! `TUNE_TEST_PG_URL`) : un interprète écouté 3 fois, une émission écoutée
//! 6 fois (plus que l'interprète, comme sur le .18), une ancienne ligne de
//! radio à titre ICY, une ancienne écoute SANS source (NULL), qui reste de la
//! musique. L'émission, ses épisodes et l'interprète ICY sortent des
//! classements (artistes, albums, titres — décision de Bertrand du 13/11) et
//! du compte d'artistes ; les épisodes restent dans le nombre d'écoutes et le
//! temps d'écoute.
//!
//! Contre-épreuve : retirer `'podcast'` de [`super::ECOUTE_DE_MUSIQUE`] /
//! [`super::ecoute_de_musique`] fait rougir le témoin (l'émission arrive en
//! tête des classements).

use super::*;
use crate::db::migrations;

const INTERPRETE: &str = "Interprète 8645";
const EMISSION: &str = "Émission 8645";
const ICY: &str = "Interprète ICY 8645";
const ANCIEN: &str = "Interprète ancien 8645";
const MS_PISTE: i64 = 200_000;
const MS_EPISODE: i64 = 1_800_000;
const MS_ANCIEN: i64 = 100_000;

fn ecoute(titre: &str, artiste: &str, album: Option<&str>, source: &str, ms: i64) -> ListenRecord {
    ListenRecord {
        title: titre.into(),
        artist_name: Some(artiste.into()),
        album_title: album.map(Into::into),
        source: source.into(),
        duration_ms: ms,
        ..Default::default()
    }
}

/// Les écoutes du témoin, écrites par le vrai chemin (`record`), comme
/// `enregistrer_ecoute_podcast` les écrit pour un épisode.
fn poser_le_temoin(repo: &HistoryRepo) {
    for _ in 0..3 {
        repo.record(&ecoute(
            "Titre 8645",
            INTERPRETE,
            Some("Album 8645"),
            "local",
            MS_PISTE,
        ))
        .unwrap();
    }
    for i in 0..6 {
        let mut episode = ecoute(
            &format!("Épisode {i} 8645"),
            EMISSION,
            Some(EMISSION),
            "podcast",
            MS_EPISODE,
        );
        episode.source_id = Some(format!("https://exemple.invalid/8645/{i}.mp3"));
        episode.context_type = Some("podcast".into());
        episode.context_id = Some(EMISSION.into());
        repo.record(&episode).unwrap();
    }
    // Ligne de radio d'avant la v0.8.258 : l'interprète d'un titre ICY.
    repo.record(&ecoute("Titre ICY 8645", ICY, None, "radio", 0))
        .unwrap();
    // Écoute d'avant la colonne `source` renseignée : NULL, donc musique.
    repo.db
        .execute(
            &format!(
                "INSERT INTO listen_history (title, artist_name, album_title, source, duration_ms) \
                 VALUES ('Titre ancien 8645', '{ANCIEN}', 'Album ancien 8645', NULL, {MS_ANCIEN})"
            ),
            &[],
        )
        .unwrap();
}

fn noms<'a>(it: impl Iterator<Item = &'a str>) -> Vec<String> {
    it.filter(|n| n.contains("8645"))
        .map(str::to_string)
        .collect()
}

/// Les classements, sur n'importe quel moteur (base éventuellement partagée :
/// seuls les noms du témoin sont regardés).
fn verifier_les_classements(repo: &HistoryRepo) {
    let artistes = repo.top_artists(100_000).unwrap();
    assert_eq!(
        noms(artistes.iter().map(|(n, _)| n.as_str())),
        vec![INTERPRETE.to_string(), ANCIEN.to_string()],
        "top_artists (Tune Circle, statistiques) : ni l'émission, ni l'interprète ICY : {artistes:?}"
    );
    let albums = repo.top_albums(100_000).unwrap();
    assert_eq!(
        noms(albums.iter().map(|(t, _, _)| t.as_str())),
        vec!["Album 8645".to_string(), "Album ancien 8645".to_string()],
        "top_albums : l'émission n'est pas un album : {albums:?}"
    );

    let titres = repo.top_tracks(100_000).unwrap();
    assert_eq!(
        noms(titres.iter().filter_map(|t| t["title"].as_str())),
        vec!["Titre 8645".to_string(), "Titre ancien 8645".to_string()],
        "top_tracks : aucun épisode de podcast, ni titre ICY : {titres:?}"
    );

    // L'accueil : « Artistes les plus écoutés » et les tops du tableau de bord.
    let d = repo.full_dashboard("all", None, None, 100_000).unwrap();
    assert_eq!(
        noms(d.top_artists.iter().map(|a| a.artist_name.as_str())),
        vec![INTERPRETE.to_string(), ANCIEN.to_string()],
        "tableau de bord, artistes les plus écoutés : {:?}",
        d.top_artists
            .iter()
            .filter(|a| a.artist_name.contains("8645"))
            .map(|a| (&a.artist_name, a.plays))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        d.top_artists
            .iter()
            .find(|a| a.artist_name == INTERPRETE)
            .map(|a| a.plays),
        Some(3)
    );
    assert_eq!(
        noms(d.top_albums.iter().map(|a| a.album_title.as_str())),
        vec!["Album 8645".to_string(), "Album ancien 8645".to_string()],
        "tableau de bord, albums les plus écoutés"
    );
    assert_eq!(
        noms(d.top_tracks.iter().map(|t| t.title.as_str())),
        vec!["Titre 8645".to_string(), "Titre ancien 8645".to_string()],
        "tableau de bord, titres les plus écoutés : aucun épisode"
    );
}

#[test]
fn sqlite_une_emission_de_podcast_n_est_pas_un_artiste_8645() {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    let repo = HistoryRepo::new(db);
    poser_le_temoin(&repo);
    verifier_les_classements(&repo);

    // Base vierge : les totaux se vérifient exactement.
    let d = repo.full_dashboard("all", None, None, 50).unwrap();
    assert_eq!(d.totals.plays, 10, "les épisodes restent des écoutes");
    assert_eq!(
        d.totals.listening_ms,
        3 * MS_PISTE + 6 * MS_EPISODE + MS_ANCIEN,
        "le temps d'écoute garde les épisodes"
    );
    assert_eq!(
        d.totals.unique_artists, 2,
        "l'émission n'est pas un artiste de plus, l'écoute sans source en est un"
    );
    assert_eq!(repo.dashboard().unwrap().unique_artists, 2);
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_8645_une_emission_de_podcast_n_est_pas_un_artiste() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT : TUNE_TEST_PG_URL non posée");
        return;
    };
    let pool = sqlx::PgPool::connect(&url).await.expect("connexion PG");
    for sql in crate::db::postgres::ENSURE_TABLES
        .iter()
        .chain(crate::db::postgres::ENSURE_COLUMNS.iter())
    {
        let _ = sqlx::raw_sql(*sql).execute(&pool).await;
    }
    crate::db::migrations::run_pg_migrations(&pool)
        .await
        .unwrap_or_else(|e| panic!("run_pg_migrations : {e}"));
    let db: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
    let nettoyer = |db: &Arc<dyn DbBackend>| {
        db.execute(
            "DELETE FROM listen_history WHERE artist_name LIKE '%8645'",
            &[],
        )
        .unwrap();
    };
    nettoyer(&db);
    let repo = HistoryRepo::with_backend(db.clone());
    poser_le_temoin(&repo);
    let resultat = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        verifier_les_classements(&repo)
    }));
    nettoyer(&db);
    if let Err(e) = resultat {
        std::panic::resume_unwind(e);
    }
}
