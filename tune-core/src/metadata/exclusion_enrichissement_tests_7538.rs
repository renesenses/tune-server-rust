//! #7538 — « l'enrichissement communautaire détruit mes genres » (Jean-François
//! Paquet, 30/10 : « Jazz » devient « Latin Jazz »), et les dossiers exclus de
//! l'enrichissement.
//!
//! Témoins sur une base SQLite en mémoire, schéma et migrations complets : les
//! requêtes jouées sont celles de la production (`write_track_enrichment`,
//! `community_sync::appliquer_un_lot`, `genre_album_pistes::combler_les_pistes`).
use std::sync::Arc;

use crate::cloud::community_sync::{Enrichissement, appliquer_un_lot};
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;
use crate::metadata::enrichment::{RecordingDetails, write_track_enrichment};
use crate::taches_de_fond::perimetre::{
    CLE_DOSSIERS_EXCLUS_ENRICHISSEMENT, album_touche_l_exclusion, exclu_de_l_enrichissement,
};

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    db.execute(
        "INSERT INTO artists (id, name) VALUES (1, 'Tito Puente')",
        &[],
    )
    .unwrap();
    db
}

fn album(db: &Arc<dyn DbBackend>, id: i64, genre: Option<&str>) {
    db.execute(
        "INSERT INTO albums (id, title, artist_id, genre) VALUES (?, ?, 1, ?)",
        &[
            &id as &dyn ToSqlValue,
            &format!("album {id}"),
            &genre.map(str::to_string),
        ],
    )
    .unwrap();
}

#[allow(clippy::too_many_arguments)]
fn piste(
    db: &Arc<dyn DbBackend>,
    id: i64,
    album_id: i64,
    chemin: &str,
    genre: Option<&str>,
    genres: Option<&str>,
    mbid: Option<&str>,
) {
    db.execute(
        "INSERT INTO tracks (id, title, album_id, artist_id, file_path, genre, genres, \
         musicbrainz_recording_id) VALUES (?, ?, ?, 1, ?, ?, ?, ?)",
        &[
            &id as &dyn ToSqlValue,
            &format!("piste {id}"),
            &album_id,
            &chemin.to_string(),
            &genre.map(str::to_string),
            &genres.map(str::to_string),
            &mbid.map(str::to_string),
        ],
    )
    .unwrap();
}

fn genre_de(db: &Arc<dyn DbBackend>, id: i64) -> (Option<String>, Option<String>) {
    let r = db
        .query_one(
            "SELECT genre, genres FROM tracks WHERE id = ?",
            &[&id as &dyn ToSqlValue],
        )
        .unwrap()
        .unwrap();
    (
        r.first().and_then(|v| v.as_string()),
        r.get(1).and_then(|v| v.as_string()),
    )
}

fn annee_de(db: &Arc<dyn DbBackend>, id: i64) -> Option<i64> {
    db.query_one(
        "SELECT year FROM tracks WHERE id = ?",
        &[&id as &dyn ToSqlValue],
    )
    .unwrap()
    .and_then(|r| r.first().and_then(|v| v.as_i64()))
}

fn exclure(db: &Arc<dyn DbBackend>, dossiers: &[&str]) {
    SettingsRepo::with_backend(db.clone())
        .set(
            CLE_DOSSIERS_EXCLUS_ENRICHISSEMENT,
            &serde_json::to_string(dossiers).unwrap(),
        )
        .unwrap();
}

fn latin_jazz_1959() -> RecordingDetails {
    RecordingDetails {
        genre: Some("Latin Jazz".into()),
        year: Some(1959),
        ..Default::default()
    }
}

fn nuage(mbid: &str) -> Enrichissement {
    Enrichissement {
        mb_id: mbid.into(),
        genre: Some("Latin Jazz".into()),
        year: Some(1959),
        composer: None,
        label: None,
        isrc: None,
    }
}

/// MusicBrainz : la piste sans genre d'un album « Jazz » prend « Jazz », pas
/// le genre plus fin que propose l'enregistrement. L'année, elle, est comblée.
#[test]
fn musicbrainz_ne_pose_pas_latin_jazz_dans_un_album_jazz_7538() {
    let db = base();
    album(&db, 1, Some("Jazz"));
    piste(&db, 1, 1, "/music/Jazz/1.flac", None, None, None);
    write_track_enrichment(&db, 1, Some(1), "mb-1", None, &latin_jazz_1959()).unwrap();
    assert_eq!(
        genre_de(&db, 1).0.as_deref(),
        Some("Jazz"),
        "#7538 : la piste d'un album « Jazz » ne doit pas recevoir « Latin Jazz »"
    );
    assert_eq!(
        annee_de(&db, 1),
        Some(1959),
        "le reste est comblé comme avant"
    );
}

/// Le tableau multivaleur `genres` rempli, la colonne vide : la piste a des
/// genres, l'enrichissement n'en ajoute pas un autre.
#[test]
fn un_tableau_de_genres_n_est_pas_un_trou_7538() {
    let db = base();
    album(&db, 1, None);
    piste(
        &db,
        1,
        1,
        "/music/1.flac",
        None,
        Some(r#"["Jazz","Bop"]"#),
        None,
    );
    write_track_enrichment(&db, 1, Some(1), "mb-1", None, &latin_jazz_1959()).unwrap();
    assert_eq!(genre_de(&db, 1).0, None);
}

/// Témoin inverse : sans aucun genre nulle part, MusicBrainz comble comme
/// avant — une garde qui refuserait tout serait verte ailleurs.
#[test]
fn sans_genre_nulle_part_musicbrainz_comble_7538() {
    let db = base();
    album(&db, 1, None);
    piste(&db, 1, 1, "/music/1.flac", None, None, None);
    write_track_enrichment(&db, 1, Some(1), "mb-1", None, &latin_jazz_1959()).unwrap();
    assert_eq!(genre_de(&db, 1).0.as_deref(), Some("Latin Jazz"));
    // Et la remontée vers l'album a lieu.
    let g = db
        .query_one("SELECT genre FROM albums WHERE id = 1", &[])
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_string()));
    assert_eq!(g.as_deref(), Some("Latin Jazz"));
}

/// Le partage communautaire suit la même règle.
#[test]
fn le_nuage_ne_pose_pas_latin_jazz_dans_un_album_jazz_7538() {
    let db = base();
    album(&db, 1, Some("Jazz"));
    album(&db, 2, None);
    piste(&db, 1, 1, "/music/Jazz/1.flac", None, None, Some("mb-1"));
    piste(
        &db,
        2,
        1,
        "/music/Jazz/2.flac",
        Some("Jazz"),
        Some(r#"["Jazz"]"#),
        Some("mb-2"),
    );
    piste(&db, 3, 2, "/music/Autre/3.flac", None, None, Some("mb-3"));
    appliquer_un_lot(&db, &[nuage("mb-1"), nuage("mb-2"), nuage("mb-3")]);
    assert_eq!(genre_de(&db, 1).0.as_deref(), Some("Jazz"));
    assert_eq!(genre_de(&db, 2).0.as_deref(), Some("Jazz"));
    assert_eq!(
        genre_de(&db, 3).0.as_deref(),
        Some("Latin Jazz"),
        "un album sans genre reste comblé par le nuage"
    );
    assert_eq!(annee_de(&db, 1), Some(1959));
}

/// Un dossier exclu : ni MusicBrainz, ni le nuage n'écrivent rien sur ses
/// pistes. Un dossier voisin au nom prolongé (`Jazz2`) n'est pas exclu.
#[test]
fn un_dossier_exclu_n_est_jamais_enrichi_7538() {
    let db = base();
    album(&db, 1, None);
    piste(&db, 1, 1, "/music/Jazz/1.flac", None, None, Some("mb-1"));
    piste(&db, 2, 1, "/music/Jazz2/2.flac", None, None, Some("mb-2"));
    piste(&db, 3, 1, "/music/Jazz/3.flac", None, None, None);
    exclure(&db, &["/music/Jazz/"]);

    appliquer_un_lot(&db, &[nuage("mb-1"), nuage("mb-2")]);
    assert_eq!(genre_de(&db, 1), (None, None), "nuage sur un dossier exclu");
    assert_eq!(annee_de(&db, 1), None);
    assert_eq!(genre_de(&db, 2).0.as_deref(), Some("Latin Jazz"));

    let n = write_track_enrichment(&db, 3, Some(1), "mb-3", None, &latin_jazz_1959()).unwrap();
    assert_eq!(n, 0);
    assert_eq!(
        genre_de(&db, 3),
        (None, None),
        "MusicBrainz sur un dossier exclu"
    );
    assert_eq!(annee_de(&db, 3), None);
    let mbid = db
        .query_one(
            "SELECT musicbrainz_recording_id FROM tracks WHERE id = 3",
            &[],
        )
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_string()));
    assert_eq!(mbid, None);
}

/// Les réparations de genre (Last.fm, Discogs, par artiste, par famille)
/// COMBLENT : la piste qui porte « Jazz » le garde, la piste nue reçoit le
/// genre réparé, la piste d'un dossier exclu n'est pas touchée.
#[test]
fn une_reparation_comble_sans_ecraser_7538() {
    let db = base();
    album(&db, 1, Some("Latin Jazz"));
    piste(
        &db,
        1,
        1,
        "/music/A/1.flac",
        Some("Jazz"),
        Some(r#"["Jazz"]"#),
        None,
    );
    piste(&db, 2, 1, "/music/A/2.flac", None, None, None);
    piste(&db, 3, 1, "/music/X/3.flac", None, None, None);
    exclure(&db, &["/music/X"]);
    let n = crate::db::genre_album_pistes::combler_les_pistes(&db, 1).unwrap();
    assert_eq!(n, 1);
    assert_eq!(genre_de(&db, 1).0.as_deref(), Some("Jazz"));
    assert_eq!(
        genre_de(&db, 2),
        (Some("Latin Jazz".into()), Some(r#"["Latin Jazz"]"#.into()))
    );
    assert_eq!(genre_de(&db, 3), (None, None));
    // Aucun marqueur de recopie : le scan ne reposera rien par-dessus les balises.
    let marqueur = db
        .query_one(
            "SELECT 1 FROM album_metadata WHERE album_id = 1 AND key = ?",
            &[&crate::db::genre_album_pistes::CLE_GENRE_PISTES as &dyn ToSqlValue],
        )
        .unwrap();
    assert!(marqueur.is_none());
    assert!(album_touche_l_exclusion(&db, 1, &["/music/X".into()]));
    assert!(!album_touche_l_exclusion(&db, 1, &["/music/Y".into()]));
    assert!(!album_touche_l_exclusion(&db, 1, &[]));
}

/// Le test en Rust compte exactement comme la clause SQL : préfixe borné,
/// séparateurs normalisés, la racine elle-même incluse.
#[test]
fn exclu_de_l_enrichissement_borne_le_prefixe_7538() {
    let d = vec![
        "/music/Jazz/".to_string(),
        r"D:\Musique\Classique".to_string(),
    ];
    assert!(exclu_de_l_enrichissement("/music/Jazz", &d));
    assert!(exclu_de_l_enrichissement("/music/Jazz/a/b.flac", &d));
    assert!(!exclu_de_l_enrichissement("/music/Jazz2/b.flac", &d));
    assert!(exclu_de_l_enrichissement(
        r"D:\Musique\Classique\x.flac",
        &d
    ));
    assert!(exclu_de_l_enrichissement("D:/Musique/Classique/x.flac", &d));
    assert!(!exclu_de_l_enrichissement("/autre/x.flac", &d));
    assert!(!exclu_de_l_enrichissement("/music/Jazz/x.flac", &[]));
}
