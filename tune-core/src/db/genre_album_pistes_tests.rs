//! Témoins de #5314 — le genre posé sur un album vaut pour ses pistes.
//!
//! Chaque scénario est écrit UNE fois, sur `Arc<dyn DbBackend>` : SQLite le
//! joue ici, PostgreSQL le rejoue dans `postgres_e2e.rs`
//! (`pg_genre_album_pistes_5314`).
use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::db::album_metadata_repo::AlbumMetadataRepo;
use crate::db::album_repo::AlbumRepo;
use crate::db::artist_repo::ArtistRepo;
use crate::db::edition_album::{self, Modification, Tenues};
use crate::db::models::{Album, Artist, Track};
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;

fn sqlite() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

/// Le magasin que `postgres_e2e::reset_schema` ne vide pas.
fn nettoyer(db: &Arc<dyn DbBackend>) {
    let _ = db.execute("DELETE FROM album_metadata", &[]);
}

fn artiste(db: &Arc<dyn DbBackend>, nom: &str) -> i64 {
    let repo = ArtistRepo::with_backend(db.clone());
    if let Some(id) = repo.get_by_name(nom).unwrap().and_then(|a| a.id) {
        return id;
    }
    repo.create(&Artist::new(nom.into())).unwrap()
}

/// Un album et ses pistes, chacune avec les genres de SES balises
/// (`(tracks.genre, tracks.genres)`).
fn album(db: &Arc<dyn DbBackend>, titre: &str, genres: &[(&str, &str)]) -> (i64, Vec<i64>) {
    let ar = artiste(db, "Artiste");
    let mut a = Album::new(titre.into());
    a.artist_id = Some(ar);
    let id = AlbumRepo::with_backend(db.clone()).create(&a).unwrap();
    let pistes = TrackRepo::with_backend(db.clone());
    let mut ids = Vec::new();
    for (k, (g, gs)) in genres.iter().enumerate() {
        let mut t = Track::new(format!("{titre} {k}"));
        t.album_id = Some(id);
        t.artist_id = Some(ar);
        t.track_number = k as i32 + 1;
        t.disc_number = 1;
        t.file_path = Some(format!("/m/{titre}/{k:02}.flac"));
        t.genre = Some(g.to_string());
        t.genres = Some(gs.to_string());
        ids.push(pistes.create(&t).unwrap());
    }
    (id, ids)
}

fn modifier(db: &Arc<dyn DbBackend>, id: i64, corps: serde_json::Value) {
    let m: Modification = serde_json::from_value(corps).expect("corps conforme au contrat");
    edition_album::appliquer(db, id, &m).unwrap();
}

/// `(tracks.genre, tracks.genres)` de chaque piste de l'album, par id.
fn colonnes(db: &Arc<dyn DbBackend>, album_id: i64) -> Vec<(Option<String>, Option<String>)> {
    let p = match db.engine() {
        Engine::Sqlite => SqliteDialect.placeholder(1),
        Engine::Postgres => PostgresDialect.placeholder(1),
    };
    db.query_many_strong(
        &format!("SELECT genre, genres FROM tracks WHERE album_id = {p} ORDER BY id"),
        &[&album_id as &dyn ToSqlValue],
    )
    .unwrap()
    .into_iter()
    .map(|r| {
        (
            r.first().and_then(|v| v.as_string()),
            r.get(1).and_then(|v| v.as_string()),
        )
    })
    .collect()
}

/// Ce que la facette Genre d'Oxygen propose pour cet album : l'UNION de la
/// colonne et du tableau, exactement comme `genre_facet` (`facets.rs`).
fn facette(db: &Arc<dyn DbBackend>, album_id: i64) -> BTreeSet<String> {
    let mut v = BTreeSet::new();
    for (g, gs) in colonnes(db, album_id) {
        if let Some(json) = gs.as_deref()
            && let Ok(arr) = serde_json::from_str::<Vec<String>>(json)
        {
            v.extend(arr.into_iter().filter(|x| !x.trim().is_empty()));
        }
        if let Some(g) = g.filter(|g| !g.trim().is_empty()) {
            v.insert(g);
        }
    }
    v
}

fn marqueur(db: &Arc<dyn DbBackend>, album_id: i64) -> Option<String> {
    AlbumMetadataRepo::with_backend(db.clone())
        .get_all(album_id)
        .unwrap()
        .get(CLE_GENRE_PISTES)
        .cloned()
}

fn albums_genres(db: &Arc<dyn DbBackend>, album_id: i64) -> Option<String> {
    AlbumRepo::with_backend(db.clone())
        .get(album_id)
        .unwrap()
        .unwrap()
        .genres
}

/// 🔴 L'épreuve de la décision : genre d'album modifié par la fiche
/// « Modifier » ⇒ la facette Genre d'Oxygen ne propose QUE ce genre pour cet
/// album. Et un scan qui relit les balises ne le défait pas.
pub(crate) fn scenario_edition_recopie_et_tient(db: &Arc<dyn DbBackend>) {
    nettoyer(db);
    let (id, pistes) = album(
        db,
        "Kind of Blue",
        &[("Rock", r#"["Rock","Pop"]"#), ("Blues", r#"["Blues"]"#)],
    );
    assert_eq!(
        facette(db, id),
        ["Blues", "Pop", "Rock"].map(String::from).into(),
        "avant : les genres des balises"
    );

    modifier(db, id, json!({ "genre": "Jazz" }));
    assert_eq!(
        facette(db, id),
        ["Jazz"].map(String::from).into(),
        "#5314 : la facette Genre d'Oxygen doit ne proposer que le genre posé sur l'album"
    );
    assert_eq!(
        colonnes(db, id),
        vec![(Some("Jazz".into()), Some(r#"["Jazz"]"#.into())); 2]
    );
    assert_eq!(albums_genres(db, id).as_deref(), Some(r#"["Jazz"]"#));
    assert_eq!(marqueur(db, id).as_deref(), Some("Jazz"));

    // Le scan reconstruit la ligne depuis les BALISES (encore « Rock ») :
    // la tenue lui repose le genre de l'album avant l'écriture.
    let mut relue = TrackRepo::with_backend(db.clone())
        .get(pistes[0])
        .unwrap()
        .unwrap();
    relue.genre = Some("Rock".into());
    relue.genres = Some(r#"["Rock","Pop"]"#.into());
    Tenues::charger(db).appliquer(&mut relue);
    assert_eq!(
        (relue.genre.as_deref(), relue.genres.as_deref()),
        (Some("Jazz"), Some(r#"["Jazz"]"#)),
        "#5314 : un scan qui relit les balises doit reposer le genre recopié de l'album"
    );
}

/// Plusieurs genres posés d'un coup : découpés comme au scan, le premier en
/// colonne.
pub(crate) fn scenario_genres_multiples(db: &Arc<dyn DbBackend>) {
    nettoyer(db);
    let (id, _) = album(db, "Bitches Brew", &[("Rock", r#"["Rock"]"#)]);
    modifier(db, id, json!({ "genre": "Jazz; Fusion" }));
    assert_eq!(
        colonnes(db, id),
        vec![(Some("Jazz".into()), Some(r#"["Jazz","Fusion"]"#.into()))]
    );
    assert_eq!(facette(db, id), ["Fusion", "Jazz"].map(String::from).into());
}

/// L'exception : une compilation aux pistes de genres DIFFÉRENTS les garde ;
/// « appliquer aussi aux pistes » les recopie. Une compilation aux pistes
/// d'un même genre, elle, suit l'album.
pub(crate) fn scenario_compilation(db: &Arc<dyn DbBackend>) {
    nettoyer(db);
    let (id, _) = album(
        db,
        "Hits 1975",
        &[("Rock", r#"["Rock"]"#), ("Disco", r#"["Disco"]"#)],
    );
    // Le mode est posé par la MÊME édition : l'exception juge l'album tel
    // qu'elle vient de le poser.
    modifier(db, id, json!({ "genre": "Pop", "compilation_mode": "oui" }));
    assert_eq!(
        facette(db, id),
        ["Disco", "Rock"].map(String::from).into(),
        "une compilation aux genres différents garde ceux de ses pistes"
    );
    assert_eq!(marqueur(db, id).as_deref(), Some(""), "examinée, épargnée");
    assert_eq!(
        AlbumRepo::with_backend(db.clone())
            .get(id)
            .unwrap()
            .unwrap()
            .genre
            .as_deref(),
        Some("Pop"),
        "le genre de l'album change quand même"
    );
    // Épargnée : un scan ne repose rien.
    let mut t = Track::new("x".into());
    t.album_id = Some(id);
    t.genre = Some("Rock".into());
    Tenues::charger(db).appliquer(&mut t);
    assert_eq!(t.genre.as_deref(), Some("Rock"));

    // Demande explicite, sans changer le genre.
    modifier(db, id, json!({ "apply_genre_to_tracks": true }));
    assert_eq!(facette(db, id), ["Pop"].map(String::from).into());
    assert_eq!(marqueur(db, id).as_deref(), Some("Pop"));

    // Compilation dont les pistes s'accordent : elle suit l'album.
    let (id2, _) = album(
        db,
        "Jazz à Juan",
        &[("Jazz", r#"["Jazz"]"#), ("jazz", r#"["jazz"]"#)],
    );
    modifier(
        db,
        id2,
        json!({ "genre": "Bebop", "compilation_mode": "oui" }),
    );
    assert_eq!(facette(db, id2), ["Bebop"].map(String::from).into());
}

/// Un formulaire renvoyé avec le MÊME genre ne réécrit pas les pistes ; un
/// genre effacé ne les efface pas non plus et lève le marqueur.
pub(crate) fn scenario_sans_changement_et_effacement(db: &Arc<dyn DbBackend>) {
    nettoyer(db);
    let (id, pistes) = album(db, "Blue Train", &[("Rock", r#"["Rock"]"#)]);
    modifier(db, id, json!({ "genre": "Jazz" }));
    // Une édition de piste isolée, après coup.
    db.execute(
        &format!(
            "UPDATE tracks SET genre = 'Hard Bop', genres = '[\"Hard Bop\"]' WHERE id = {}",
            pistes[0]
        ),
        &[],
    )
    .unwrap();
    modifier(
        db,
        id,
        json!({ "title": "Blue Train (RVG)", "genre": " jazz " }),
    );
    assert_eq!(
        facette(db, id),
        ["Hard Bop"].map(String::from).into(),
        "le même genre renvoyé ne réécrit pas les pistes"
    );
    modifier(db, id, json!({ "genre": null }));
    assert_eq!(facette(db, id), ["Hard Bop"].map(String::from).into());
    assert_eq!(
        marqueur(db, id),
        None,
        "plus de genre d'album, plus de tenue"
    );
}

/// Le rattrapage des genres posés à la main avant #5314 : une fois, puis plus.
pub(crate) fn scenario_rattrapage(db: &Arc<dyn DbBackend>) {
    nettoyer(db);
    let (id, _) = album(db, "Moanin'", &[("Rock", r#"["Rock"]"#)]);
    let (autre, _) = album(db, "Sans main", &[("Rock", r#"["Rock"]"#)]);
    for a in [id, autre] {
        db.execute(
            &format!("UPDATE albums SET genre = 'Jazz' WHERE id = {a}"),
            &[],
        )
        .unwrap();
    }
    // Seul `id` a été tenu À LA MAIN (C3).
    AlbumMetadataRepo::with_backend(db.clone())
        .marquer_edition_manuelle(id, &["genre"])
        .unwrap();

    assert_eq!(rattraper_les_genres_tenus(db).unwrap(), 1);
    assert_eq!(facette(db, id), ["Jazz"].map(String::from).into());
    assert_eq!(
        facette(db, autre),
        ["Rock"].map(String::from).into(),
        "un genre d'album qui n'a pas été posé à la main n'est pas rattrapé"
    );
    assert_eq!(
        rattraper_les_genres_tenus(db).unwrap(),
        0,
        "idempotent : un album rattrapé porte son marqueur"
    );
}

#[test]
fn edition_recopie_et_tient_sur_sqlite() {
    scenario_edition_recopie_et_tient(&sqlite());
}

#[test]
fn genres_multiples_sur_sqlite() {
    scenario_genres_multiples(&sqlite());
}

#[test]
fn compilation_sur_sqlite() {
    scenario_compilation(&sqlite());
}

#[test]
fn sans_changement_et_effacement_sur_sqlite() {
    scenario_sans_changement_et_effacement(&sqlite());
}

#[test]
fn rattrapage_sur_sqlite() {
    scenario_rattrapage(&sqlite());
}

#[test]
fn colonnes_de_piste_suit_le_scan() {
    assert_eq!(colonnes_de_piste("  "), None);
    assert_eq!(
        colonnes_de_piste("Jazz / Fusion"),
        Some(("Jazz".into(), r#"["Jazz","Fusion"]"#.into()))
    );
}

#[test]
fn genre_change_ignore_casse_et_espaces() {
    assert!(!genre_change(Some("Jazz"), Some(" jazz ")));
    assert!(genre_change(None, Some("Jazz")));
    assert!(genre_change(Some("Jazz"), None));
    assert!(!genre_change(None, Some("")));
}
