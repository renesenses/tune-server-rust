//! #8611 — témoins des lectures ciblées des artistes bloqués : piste locale
//! (artiste de la piste, de l'album, balise `ALBUMARTIST`), favoris de la
//! bibliothèque, et le filtre des résultats des services (par nom seul).

use super::*;
use crate::db::backend::{DbBackend, ToSqlValue};
use serde_json::json;
use std::sync::Arc;

const BLOQUE: i64 = 7;
const LIBRE: i64 = 8;

/// Deux artistes, un album chacun, une piste chacun ; une piste du libre
/// dont seule la balise `ALBUMARTIST` nomme l'artiste bloqué ; une piste du
/// libre sur un album de l'artiste bloqué.
fn base() -> Arc<dyn DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let sql = |s: &str, p: &[&dyn ToSqlValue]| {
        db.execute(s, p).unwrap();
    };
    for (id, nom) in [(BLOQUE, "Groupe Interdit"), (LIBRE, "Groupe Libre")] {
        sql("INSERT INTO artists (id, name) VALUES (?, ?)", &[&id, &nom]);
        sql(
            "INSERT INTO albums (id, title, artist_id) VALUES (?, ?, ?)",
            &[&(id * 10), &format!("Album de {nom}"), &id],
        );
        sql(
            "INSERT INTO tracks (id, title, artist_id, album_id, file_path) VALUES (?, ?, ?, ?, ?)",
            &[
                &(id * 100),
                &"Titre",
                &id,
                &(id * 10),
                &format!("/m/{id}.flac"),
            ],
        );
    }
    // Piste du libre, balise ALBUMARTIST du bloqué (graphie différente).
    sql(
        "INSERT INTO tracks (id, title, artist_id, album_id, album_artist, file_path) \
         VALUES (801, 'Invité', 8, 80, 'groupe  INTERDIT', '/m/801.flac')",
        &[],
    );
    // Piste du libre sur l'album du bloqué.
    sql(
        "INSERT INTO tracks (id, title, artist_id, album_id, file_path) \
         VALUES (802, 'Featuring', 8, 70, '/m/802.flac')",
        &[],
    );
    db
}

fn filtre(artistes: Vec<ArtisteBloque>) -> Filtre {
    Filtre::depuis(&PolitiqueParentale {
        artistes_bloques: artistes,
        ..Default::default()
    })
}

#[test]
fn la_piste_locale_d_un_artiste_bloque_8611() {
    let db = base();
    for f in [
        filtre(vec![ArtisteBloque {
            id: Some(BLOQUE),
            nom: None,
        }]),
        filtre(vec![ArtisteBloque {
            id: None,
            nom: Some("Groupe Interdit".into()),
        }]),
    ] {
        assert!(
            piste_locale_bloquee(&db, &f, 700).unwrap(),
            "piste du bloqué"
        );
        assert!(
            piste_locale_bloquee(&db, &f, 802).unwrap(),
            "album du bloqué"
        );
        assert!(!piste_locale_bloquee(&db, &f, 800).unwrap(), "piste libre");
        assert!(!piste_locale_bloquee(&db, &f, 999_999).unwrap(), "inconnue");
    }
    // La balise ALBUMARTIST ne se reconnaît que par le nom.
    let par_nom = filtre(vec![ArtisteBloque {
        id: None,
        nom: Some("Groupe Interdit".into()),
    }]);
    assert!(piste_locale_bloquee(&db, &par_nom, 801).unwrap());
    // Sans artiste bloqué, rien n'est lu ni bloqué.
    assert!(!piste_locale_bloquee(&db, &filtre(vec![]), 700).unwrap());
}

#[test]
fn les_favoris_d_un_artiste_bloque_8611() {
    let db = base();
    let f = filtre(vec![ArtisteBloque {
        id: Some(BLOQUE),
        nom: None,
    }]);
    let favoris: Vec<(String, i64)> = [
        ("artist", BLOQUE),
        ("artist", LIBRE),
        ("album", 70),
        ("album", 80),
        ("track", 700),
        ("track", 800),
        ("track", 802),
        ("genre", BLOQUE),
    ]
    .iter()
    .map(|(t, i)| (t.to_string(), *i))
    .collect();
    let bloques = favoris_bloques(&db, &f, &favoris).unwrap();
    let attendus: HashSet<(String, i64)> = [
        ("artist", BLOQUE),
        ("album", 70),
        ("track", 700),
        ("track", 802),
    ]
    .iter()
    .map(|(t, i)| (t.to_string(), *i))
    .collect();
    assert_eq!(bloques, attendus);
    assert!(
        favoris_bloques(&db, &filtre(vec![]), &favoris)
            .unwrap()
            .is_empty()
    );
}

/// Les résultats d'un service : le nom seul compte. Un identifiant de
/// service égal à l'identifiant d'un artiste bloqué ne retire rien.
#[test]
fn les_resultats_des_services_par_le_nom_8611() {
    let f = filtre(vec![ArtisteBloque {
        id: Some(BLOQUE),
        nom: Some("Groupe Interdit".into()),
    }]);
    let mut corps = json!({
        "albums": [
            {"id": 1, "title": "A", "artist": "Groupe Interdit"},
            {"id": 2, "title": "B", "artist": "Groupe Libre", "artist_id": BLOQUE},
        ],
        "artists": [
            {"id": BLOQUE, "name": "Un autre"},
            {"id": 3, "name": "groupe interdit"},
        ],
        "tracks": [{"id": 4, "title": "T", "artist": "Groupe Interdit"}],
    });
    assert_eq!(f.filtrer(&mut corps, Liste::Services), 3);
    assert_eq!(corps["albums"].as_array().unwrap().len(), 1);
    assert_eq!(corps["artists"][0]["name"], "Un autre");
    assert!(corps["tracks"].as_array().unwrap().is_empty());
    // Favoris de service : l'artiste favori porte son nom dans `title`.
    let mut favoris = json!([
        {"item_type": "artist", "service": "qobuz", "service_id": "9", "title": "Groupe Interdit"},
        {"item_type": "album", "service": "qobuz", "service_id": "10", "title": "X", "artist": "Groupe Interdit"},
        {"item_type": "album", "service": "qobuz", "service_id": "11", "title": "Y", "artist": "Groupe Libre"},
    ]);
    assert_eq!(f.filtrer(&mut favoris, Liste::Services), 2);
    assert_eq!(favoris[0]["service_id"], "11");
}
