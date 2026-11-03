//! #6474, point 2 — le nom de tri MusicBrainz (`sort-name`) d'un artiste dont
//! les fichiers n'ont pas de balise ARTISTSORT.
//!
//! Les réponses MusicBrainz sont FACTICES, mais passent par les vrais
//! analyseurs : `parse_release_detail` (identification, MetaRust) et
//! `credits_d_artiste` (passe d'enrichissement des pistes). Aucune requête :
//! c'est la propriété demandée — le nom de tri vient de réponses que ces
//! passes reçoivent déjà.

use std::sync::Arc;

use serde_json::{Value, json};

use super::artistes_du_pressage::{
    nom_de_tri_des_credits, poser_le_nom_de_tri, rattacher_les_artistes_de_l_album,
};
use super::musicbrainz_release::{credits_d_artiste, parse_release_detail};
use crate::db::artist_repo::{ArtistRepo, SOURCE_TRI_MUSICBRAINZ};
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::migrations;
use crate::db::sqlite::SqliteDb;

const MB_MILES: &str = "561d854a-6a28-4aa7-8c99-323e6ce46c2a";
const MB_COLTRANE: &str = "b625448e-bf4a-41c3-a421-72ad46cdb831";

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

/// Un crédit d'artiste tel que MusicBrainz le rend (`inc=artist-credits`).
fn credit_mb(mbid: &str, nom: &str, tri: &str) -> Value {
    json!({
        "name": nom,
        "joinphrase": "",
        "artist": { "id": mbid, "name": nom, "sort-name": tri },
    })
}

/// Une réponse `/release/{id}` factice : Kind of Blue, deux pistes, la
/// seconde avec un invité.
fn release_factice() -> Value {
    json!({
        "id": "rel-kind-of-blue",
        "title": "Kind of Blue",
        "date": "1959-08-17",
        "artist-credit": [credit_mb(MB_MILES, "Miles Davis", "Davis, Miles")],
        "media": [{
            "position": 1,
            "tracks": [
                {
                    "position": 1,
                    "title": "So What",
                    "recording": { "id": "rec-so-what", "title": "So What" },
                    "artist-credit": [credit_mb(MB_MILES, "Miles Davis", "Davis, Miles")],
                },
                {
                    "position": 2,
                    "title": "Freddie Freeloader",
                    "recording": { "id": "rec-freddie", "title": "Freddie Freeloader" },
                    "artist-credit": [
                        credit_mb(MB_MILES, "Miles Davis", "Davis, Miles"),
                        credit_mb(MB_COLTRANE, "John Coltrane", "Coltrane, John"),
                    ],
                },
            ],
        }],
    })
}

/// Miles Davis (artiste de l'album), John Coltrane (artiste de la piste 2),
/// sans MBID ni nom de tri : des fichiers sans ARTISTSORT.
fn bibliotheque() -> (Arc<dyn DbBackend>, i64, i64, i64) {
    let backend = base();
    let repo = ArtistRepo::with_backend(backend.clone());
    let miles = repo
        .get_or_create("Miles Davis", None, None)
        .unwrap()
        .id
        .unwrap();
    let coltrane = repo
        .get_or_create("John Coltrane", None, None)
        .unwrap()
        .id
        .unwrap();
    backend
        .execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Kind of Blue', ?)",
            &[&miles as &dyn ToSqlValue],
        )
        .unwrap();
    backend
        .execute(
            "INSERT INTO tracks (id, title, album_id, artist_id, track_number, disc_number) \
             VALUES (10, 'So What', 1, ?, 1, 1), (11, 'Freddie Freeloader', 1, ?, 2, 1)",
            &[&miles as &dyn ToSqlValue, &coltrane as &dyn ToSqlValue],
        )
        .unwrap();
    (backend, 1, miles, coltrane)
}

fn tri_et_source(backend: &Arc<dyn DbBackend>, id: i64) -> (Option<String>, Option<String>) {
    let ligne = backend
        .query_one(
            "SELECT sort_name, sort_name_source FROM artists WHERE id = ?",
            &[&id as &dyn ToSqlValue],
        )
        .unwrap()
        .unwrap();
    (
        ligne.first().and_then(|v| v.as_string()),
        ligne.get(1).and_then(|v| v.as_string()),
    )
}

fn identifier(backend: &Arc<dyn DbBackend>, album_id: i64) -> usize {
    let detail = parse_release_detail(&release_factice()).expect("release factice lisible");
    let recordings = vec![
        (10, "rec-so-what".to_string()),
        (11, "rec-freddie".to_string()),
    ];
    rattacher_les_artistes_de_l_album(backend, album_id, &detail, &recordings)
        .unwrap()
        .noms_de_tri
}

/// L'identification (MetaRust, une requête par release, gardée en base) pose
/// le nom de tri de l'artiste de l'album ET celui de l'artiste d'une piste,
/// marqués comme venant de MusicBrainz.
#[test]
fn l_identification_pose_le_nom_de_tri_musicbrainz_6474() {
    let (backend, album, miles, coltrane) = bibliotheque();

    assert_eq!(identifier(&backend, album), 2);

    assert_eq!(
        tri_et_source(&backend, miles),
        (
            Some("Davis, Miles".into()),
            Some(SOURCE_TRI_MUSICBRAINZ.into())
        )
    );
    assert_eq!(
        tri_et_source(&backend, coltrane),
        (
            Some("Coltrane, John".into()),
            Some(SOURCE_TRI_MUSICBRAINZ.into())
        )
    );
    // Le nom affiché ne bouge pas.
    let affiche = ArtistRepo::with_backend(backend.clone())
        .get(miles)
        .unwrap()
        .unwrap()
        .name;
    assert_eq!(affiche, "Miles Davis");
}

/// La passe d'enrichissement des pistes reçoit les crédits avec
/// l'enregistrement (`inc=artist-credits`) : le nom de tri de l'artiste de la
/// piste en vient, rattaché par son MBID.
#[test]
fn la_passe_d_enrichissement_pose_le_nom_de_tri_de_l_artiste_6474() {
    let (backend, _, miles, _) = bibliotheque();
    backend
        .execute(
            "UPDATE artists SET musicbrainz_id = ? WHERE id = ?",
            &[&MB_MILES as &dyn ToSqlValue, &miles as &dyn ToSqlValue],
        )
        .unwrap();
    // Réponse `/recording/{id}` factice, telle que l'analyseur la lit.
    let enregistrement = json!({
        "id": "rec-so-what",
        "title": "So What",
        "artist-credit": [credit_mb(MB_MILES, "Miles Davis", "Davis, Miles")],
    });
    let credits = credits_d_artiste(&enregistrement);

    assert!(poser_le_nom_de_tri(&backend, miles, &credits).unwrap());
    assert_eq!(
        tri_et_source(&backend, miles).0.as_deref(),
        Some("Davis, Miles")
    );
}

/// Les crédits ne donnent rien qui ne soit sûr : un MBID différent, un nom
/// qui ne nomme pas la fiche, deux noms de tri pour le même MBID, une fiche
/// sans MBID.
#[test]
fn rien_n_est_pose_sans_rattachement_sur_6474() {
    let credit = |mbid: &str, nom: &str, tri: &str| {
        credits_d_artiste(&json!({ "artist-credit": [credit_mb(mbid, nom, tri)] }))
    };
    // Autre MBID.
    assert_eq!(
        nom_de_tri_des_credits(
            "Miles Davis",
            MB_MILES,
            &credit(MB_COLTRANE, "Miles Davis", "x")
        ),
        None
    );
    // Le crédit ne nomme pas la fiche (un invité porteur du même MBID).
    assert_eq!(
        nom_de_tri_des_credits(
            "Miles Davis Quintet",
            MB_MILES,
            &credit(MB_MILES, "Miles Davis", "Davis, Miles")
        ),
        None
    );
    // Deux noms de tri pour le même artiste : ambigu.
    let mut deux = credit(MB_MILES, "Miles Davis", "Davis, Miles");
    deux.extend(credit(MB_MILES, "Miles Davis", "Davis, M."));
    assert_eq!(nom_de_tri_des_credits("Miles Davis", MB_MILES, &deux), None);
    // Le cas sûr, MBID en majuscules compris.
    assert_eq!(
        nom_de_tri_des_credits(
            "MILES DAVIS",
            &MB_MILES.to_uppercase(),
            &credit(MB_MILES, "Miles Davis", "Davis, Miles")
        )
        .as_deref(),
        Some("Davis, Miles")
    );

    // Une fiche sans MBID ne reçoit rien de la passe d'enrichissement.
    let (backend, _, miles, _) = bibliotheque();
    assert!(
        !poser_le_nom_de_tri(
            &backend,
            miles,
            &credit(MB_MILES, "Miles Davis", "Davis, Miles")
        )
        .unwrap()
    );
    assert_eq!(tri_et_source(&backend, miles), (None, None));
}

/// La balise garde la priorité, dans les deux ordres :
/// - un nom de tri lu dans ARTISTSORT n'est jamais remplacé par MusicBrainz ;
/// - un nom de tri posé par MusicBrainz est remplacé par la balise lue
///   ENSUITE (fichier retagué puis rescanné).
#[test]
fn la_balise_garde_la_priorite_sur_musicbrainz_6474() {
    // Balise d'abord.
    let (backend, album, miles, _) = bibliotheque();
    ArtistRepo::with_backend(backend.clone())
        .get_or_create("Miles Davis", None, Some("Davis, Miles (tp)"))
        .unwrap();
    identifier(&backend, album);
    assert_eq!(
        tri_et_source(&backend, miles),
        (Some("Davis, Miles (tp)".into()), None),
        "MusicBrainz ne remplace pas une balise"
    );

    // MusicBrainz d'abord, la balise ensuite.
    let (backend, album, miles, _) = bibliotheque();
    identifier(&backend, album);
    assert_eq!(
        tri_et_source(&backend, miles).1.as_deref(),
        Some(SOURCE_TRI_MUSICBRAINZ)
    );
    let relu = ArtistRepo::with_backend(backend.clone())
        .get_or_create("Miles Davis", None, Some("Davis, Miles (tp)"))
        .unwrap();
    assert_eq!(relu.sort_name.as_deref(), Some("Davis, Miles (tp)"));
    assert_eq!(
        tri_et_source(&backend, miles),
        (Some("Davis, Miles (tp)".into()), None),
        "la balise lue après MusicBrainz le remplace, et la provenance repasse à la balise"
    );
    // Une nouvelle identification ne la reprend pas.
    identifier(&backend, album);
    assert_eq!(
        tri_et_source(&backend, miles).0.as_deref(),
        Some("Davis, Miles (tp)")
    );
}

/// Une saisie dans « Modifier l'artiste » n'est remplacée ni par MusicBrainz
/// ni par une balise ; une écriture qui ne touche pas au nom de tri (image
/// d'artiste) lui garde sa provenance.
#[test]
fn une_saisie_n_est_jamais_remplacee_6474() {
    let (backend, album, miles, _) = bibliotheque();
    let repo = ArtistRepo::with_backend(backend.clone());
    identifier(&backend, album);

    // Une écriture d'une autre colonne : la provenance reste MusicBrainz.
    let mut fiche = repo.get(miles).unwrap().unwrap();
    fiche.image_source = Some("local".into());
    repo.update(&fiche).unwrap();
    assert_eq!(
        tri_et_source(&backend, miles).1.as_deref(),
        Some(SOURCE_TRI_MUSICBRAINZ)
    );

    // La saisie.
    let mut fiche = repo.get(miles).unwrap().unwrap();
    fiche.sort_name = Some("Davis".into());
    repo.update(&fiche).unwrap();
    assert_eq!(tri_et_source(&backend, miles), (Some("Davis".into()), None));

    identifier(&backend, album);
    repo.get_or_create("Miles Davis", None, Some("Davis, Miles"))
        .unwrap();
    assert_eq!(
        tri_et_source(&backend, miles),
        (Some("Davis".into()), None),
        "ni MusicBrainz ni la balise ne remplacent une saisie"
    );
}

/// L'ordre de la liste des artistes suit le nom de tri venu de MusicBrainz :
/// Miles Davis se range sous D, après John Coltrane (C).
#[test]
fn la_liste_des_artistes_suit_le_nom_de_tri_musicbrainz_6474() {
    let (backend, album, _, _) = bibliotheque();
    backend
        .execute(
            "INSERT INTO albums (id, title, artist_id) \
             SELECT 2, 'Giant Steps', id FROM artists WHERE name = 'John Coltrane'",
            &[],
        )
        .unwrap();
    let repo = ArtistRepo::with_backend(backend.clone());
    let ordre = |repo: &ArtistRepo| -> Vec<String> {
        repo.list(100, 0)
            .unwrap()
            .into_iter()
            .map(|a| a.name)
            .collect()
    };
    // Sans nom de tri : l'ordre du nom affiché (J… avant M…).
    assert_eq!(ordre(&repo), ["John Coltrane", "Miles Davis"]);
    identifier(&backend, album);
    // « Coltrane, John » < « Davis, Miles ».
    assert_eq!(ordre(&repo), ["John Coltrane", "Miles Davis"]);
    // Le témoin qui distingue : un nom de tri MusicBrainz qui inverse l'ordre.
    backend
        .execute(
            "UPDATE artists SET sort_name = NULL, sort_name_source = NULL WHERE name = 'John Coltrane'",
            &[],
        )
        .unwrap();
    assert!(
        poser_le_nom_de_tri(
            &backend,
            repo.get_or_create("John Coltrane", None, None)
                .unwrap()
                .id
                .unwrap(),
            &credits_d_artiste(&json!({
                "artist-credit": [credit_mb(MB_COLTRANE, "John Coltrane", "Trane")]
            })),
        )
        .unwrap()
    );
    assert_eq!(ordre(&repo), ["Miles Davis", "John Coltrane"]);
}
