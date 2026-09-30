//! Les dossiers « Collections » suivent leurs albums (#5527, #5528).

use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::db::album_repo::AlbumRepo;
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::migrations;
use crate::db::models::Track;
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn artiste(db: &Arc<dyn DbBackend>, nom: &str) -> i64 {
    db.execute(
        "INSERT INTO artists (name) VALUES (?)",
        &[&nom as &dyn ToSqlValue],
    )
    .unwrap();
    db.last_insert_rowid()
}

fn album(db: &Arc<dyn DbBackend>, titre: &str, artiste: i64) -> i64 {
    db.execute(
        "INSERT INTO albums (title, artist_id, source) VALUES (?, ?, 'local')",
        &[&titre as &dyn ToSqlValue, &artiste],
    )
    .unwrap();
    db.last_insert_rowid()
}

fn piste(db: &Arc<dyn DbBackend>, album: i64, chemin: &str) -> i64 {
    let mut t = Track::new(format!("piste {chemin}"));
    t.album_id = Some(album);
    t.file_path = Some(chemin.to_string());
    TrackRepo::with_backend(db.clone()).create(&t).unwrap()
}

/// Ce que fait le scan d'un fichier retagué : la MÊME ligne, relue, avec un
/// autre `album_id` — par `update_batch`, le chemin du scan et du
/// surveillant.
fn retaguer(db: &Arc<dyn DbBackend>, pistes: &[i64], vers: i64) {
    let repo = TrackRepo::with_backend(db.clone());
    let lot: Vec<Track> = pistes
        .iter()
        .map(|id| {
            let mut t = repo.get(*id).unwrap().unwrap();
            t.album_id = Some(vers);
            t
        })
        .collect();
    assert_eq!(repo.update_batch(&lot).unwrap(), pistes.len());
}

fn ranger(db: &Arc<dyn DbBackend>, dossiers: Value) {
    SettingsRepo::with_backend(db.clone())
        .set(REGLAGE, &dossiers.to_string())
        .unwrap();
}

fn dossiers(db: &Arc<dyn DbBackend>) -> Vec<Value> {
    serde_json::from_str(
        &SettingsRepo::with_backend(db.clone())
            .get(REGLAGE)
            .unwrap()
            .unwrap(),
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// La clé
// ---------------------------------------------------------------------------

#[test]
fn la_cle_ignore_casse_accents_ponctuation_et_numero_de_disque() {
    let k = |t: &str| cle_d_album(t, Some("Maria Callas"));
    let tosca = k("Tosca");
    assert!(tosca.is_some());
    for variante in [
        "TOSCA",
        "Tosca, CD1",
        "Tosca CD2",
        "Tosca (Disc 1)",
        "Tosca [Disc 2]",
        "Tosca - Disque 3",
        "CD1 - Tosca",
        "Tosca.",
    ] {
        assert_eq!(k(variante), tosca, "« {variante} » est Tosca");
    }
    assert_eq!(
        cle_d_album("Été indien", Some("Joe Dassin")),
        cle_d_album("ete   indien!", Some("JOE DASSIN")),
        "accents, casse, ponctuation, espaces"
    );
    assert_eq!(
        cle_d_album("Don Giovanni: K. 527", Some("Mozart")),
        cle_d_album("Don Giovanni K 527", Some("Mozart")),
    );
}

#[test]
fn la_cle_ne_confond_pas_ce_qui_differe() {
    assert_ne!(
        cle_d_album("Tosca", Some("Maria Callas")),
        cle_d_album("Tosca", Some("Renata Tebaldi")),
        "même titre, autre artiste"
    );
    assert_ne!(
        cle_d_album("Tosca", Some("Maria Callas")),
        cle_d_album("Tosca Highlights", Some("Maria Callas")),
    );
    // `vol` n'est pas un disque (règle des coffrets).
    assert_ne!(
        cle_d_album("Greatest Hits Vol. 2", Some("X")),
        cle_d_album("Greatest Hits", Some("X")),
    );
    assert_eq!(cle_d_album("Tosca", None), None, "sans artiste, pas de clé");
    assert_eq!(cle_d_album("", Some("X")), None);
}

// ---------------------------------------------------------------------------
// #5528 — le rescan réunit deux disques : le dossier suit
// ---------------------------------------------------------------------------

#[test]
fn deux_disques_reunis_par_un_rescan_sont_remplaces_dans_le_dossier() {
    let db = base();
    let callas = artiste(&db, "Maria Callas");
    let cd1 = album(&db, "Tosca, CD1", callas);
    let cd2 = album(&db, "Tosca, CD2", callas);
    let autre = album(&db, "Norma", callas);
    let p1 = piste(&db, cd1, "/m/Tosca/CD1/01.flac");
    let p2 = piste(&db, cd2, "/m/Tosca/CD2/01.flac");
    piste(&db, autre, "/m/Norma/01.flac");
    ranger(
        &db,
        json!([{ "id": 1, "name": "Opéras", "album_ids": [cd1, autre, cd2],
                 "album_labels": {
                     cd1.to_string(): {"title": "Tosca, CD1", "artist": "Maria Callas"},
                     cd2.to_string(): {"title": "Tosca, CD2", "artist": "Maria Callas"}
                 } }]),
    );

    // Le rescan : les fichiers retagués passent dans un album neuf.
    let reuni = album(&db, "Tosca", callas);
    retaguer(&db, &[p1], reuni);
    retaguer(&db, &[p2], reuni);

    let notes = dossiers(&db);
    assert_eq!(
        reunis_dans(&notes[0]["album_labels"][cd1.to_string()]),
        vec![reuni],
        "le déplacement est noté dans l'étiquette : {notes:?}"
    );

    // La purge des albums vidés établit la disparition : le dossier suit.
    let purges = AlbumRepo::with_backend(db.clone())
        .delete_orphans()
        .unwrap();
    assert_eq!(purges, 2);
    let apres = dossiers(&db);
    assert_eq!(
        apres[0]["album_ids"],
        json!([reuni, autre]),
        "les deux disques cèdent la place à l'album réuni, sans doublon"
    );
    let etiquettes = apres[0]["album_labels"].as_object().unwrap();
    assert!(!etiquettes.contains_key(&cd1.to_string()));
    assert!(!etiquettes.contains_key(&cd2.to_string()));
    assert_eq!(
        etiquettes[&reuni.to_string()],
        json!({"title": "Tosca", "artist": "Maria Callas"}),
        "l'album réuni prend son nom dans le dossier"
    );
}

#[test]
fn un_album_reparti_entre_deux_albums_n_est_pas_remplace() {
    let db = base();
    let a = artiste(&db, "A");
    let depart = album(&db, "Double", a);
    let p1 = piste(&db, depart, "/m/D/01.flac");
    let p2 = piste(&db, depart, "/m/D/02.flac");
    ranger(
        &db,
        json!([{ "id": 1, "name": "X", "album_ids": [depart] }]),
    );
    let un = album(&db, "Un", a);
    let deux = album(&db, "Deux", a);
    retaguer(&db, &[p1], un);
    retaguer(&db, &[p2], deux);
    AlbumRepo::with_backend(db.clone())
        .delete_orphans()
        .unwrap();

    let apres = dossiers(&db);
    assert_eq!(apres[0]["album_ids"], json!([depart]), "on ne choisit pas");
    assert_eq!(
        reunis_dans(&apres[0]["album_labels"][depart.to_string()]),
        vec![un, deux],
        "les deux arrivées restent notées"
    );
}

#[test]
fn un_album_encore_vivant_perd_sa_note_et_reste_range() {
    let db = base();
    let a = artiste(&db, "A");
    let depart = album(&db, "Gros", a);
    let p1 = piste(&db, depart, "/m/G/01.flac");
    piste(&db, depart, "/m/G/02.flac");
    let vide = album(&db, "Vide", a);
    ranger(
        &db,
        json!([{ "id": 1, "name": "X", "album_ids": [depart] }]),
    );
    let ailleurs = album(&db, "Ailleurs", a);
    retaguer(&db, &[p1], ailleurs);
    // Une purge a lieu (l'album `vide`), l'album de départ vit encore.
    let _ = vide;
    AlbumRepo::with_backend(db.clone())
        .delete_orphans()
        .unwrap();

    let apres = dossiers(&db);
    assert_eq!(apres[0]["album_ids"], json!([depart]));
    assert!(
        reunis_dans(&apres[0]["album_labels"][depart.to_string()]).is_empty(),
        "la note d'un album vivant est effacée : {apres:?}"
    );
}

#[test]
fn un_deplacement_hors_de_tout_dossier_n_ecrit_rien() {
    let db = base();
    let a = artiste(&db, "A");
    let depart = album(&db, "Libre", a);
    let p = piste(&db, depart, "/m/L/01.flac");
    let range = album(&db, "Rangé", a);
    piste(&db, range, "/m/R/01.flac");
    let avant = json!([{ "id": 1, "name": "X", "album_ids": [range] }]);
    ranger(&db, avant.clone());
    let ailleurs = album(&db, "Ailleurs", a);
    retaguer(&db, &[p], ailleurs);
    assert_eq!(json!(dossiers(&db)), avant);
}

// ---------------------------------------------------------------------------
// L'absorption réécrit aussi les étiquettes
// ---------------------------------------------------------------------------

#[test]
fn l_absorption_reecrit_les_etiquettes_du_dossier() {
    let db = base();
    let a = artiste(&db, "Pink Floyd");
    let cible = album(&db, "Ummagumma", a);
    let doublon = album(&db, "Ummagumma CD2", a);
    piste(&db, cible, "/m/U/CD1/01.flac");
    piste(&db, doublon, "/m/U/CD2/01.flac");
    ranger(
        &db,
        json!([{ "id": 1, "name": "X", "album_ids": [doublon],
                 "album_labels": { doublon.to_string(): {"title": "Ummagumma CD2", "artist": "Pink Floyd"} } }]),
    );
    let rapport = AlbumRepo::with_backend(db.clone())
        .absorber(cible, doublon)
        .unwrap();
    assert_eq!(rapport.collections_reecrites, 1);
    let apres = dossiers(&db);
    assert_eq!(apres[0]["album_ids"], json!([cible]));
    assert_eq!(
        apres[0]["album_labels"],
        json!({ cible.to_string(): {"title": "Ummagumma", "artist": "Pink Floyd"} }),
        "l'étiquette suit l'identifiant"
    );
}
