//! Les champs tenus d'une piste : tenir, recharger, reposer, rétablir.
use super::*;
use crate::db::artist_repo::ArtistRepo;
use crate::db::models::Artist;
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

/// Une piste en base, telle que l'utilisateur l'a corrigée.
fn piste_corrigee(db: &Arc<dyn DbBackend>) -> Track {
    let mut t = Track::new("Original".into());
    t.file_path = Some("/musique/a/01.flac".into());
    t.genre = Some("Jazz".into());
    t.year = Some(1999);
    t.composer = Some("Corrigé à la main".into());
    let id = TrackRepo::with_backend(db.clone()).create(&t).unwrap();
    t.id = Some(id);
    t
}

/// La même piste, reconstruite depuis les balises du fichier par un scan.
fn relue_du_fichier() -> Track {
    let mut t = Track::new("Titre du fichier".into());
    t.file_path = Some("/musique/a/01.flac".into());
    t.genre = Some("Rock".into());
    t.year = Some(1988);
    t.composer = Some("Balise".into());
    t
}

#[test]
fn les_champs_tenus_survivent_a_la_relecture_des_balises() {
    let db = base();
    let t = piste_corrigee(&db);
    tenir(&db, &t, &[Champ::Genre, Champ::Annee, Champ::Compositeur]).unwrap();

    let mut relue = relue_du_fichier();
    assert!(Registre::charger(&db).appliquer(&mut relue));
    assert_eq!(relue.genre.as_deref(), Some("Jazz"));
    assert_eq!(relue.year, Some(1999));
    assert_eq!(relue.composer.as_deref(), Some("Corrigé à la main"));
    // Non tenu : la balise décide.
    assert_eq!(relue.title, "Titre du fichier");
}

#[test]
fn les_tenues_s_ajoutent_d_une_edition_a_l_autre() {
    let db = base();
    let t = piste_corrigee(&db);
    tenir(&db, &t, &[Champ::Genre]).unwrap();
    tenir(&db, &t, &[Champ::Titre]).unwrap();
    let noms = de_la_piste(&db, t.id.unwrap()).unwrap().noms();
    assert_eq!(noms, vec!["title", "genre"]);
}

#[test]
fn retablir_rend_la_main_au_fichier() {
    let db = base();
    let t = piste_corrigee(&db);
    tenir(&db, &t, &[Champ::Genre]).unwrap();
    assert!(retablir(&db, t.id.unwrap()).unwrap());
    assert!(de_la_piste(&db, t.id.unwrap()).is_none());
    let mut relue = relue_du_fichier();
    assert!(!Registre::charger(&db).appliquer(&mut relue));
    assert_eq!(relue.genre.as_deref(), Some("Rock"));
}

#[test]
fn un_artiste_supprime_ne_se_repose_pas() {
    let db = base();
    let artiste = ArtistRepo::with_backend(db.clone())
        .create(&Artist::new("Éphémère".into()))
        .unwrap();
    let mut t = piste_corrigee(&db);
    t.artist_id = Some(artiste);
    tenir(&db, &t, &[Champ::Artiste]).unwrap();
    db.execute(
        "DELETE FROM artists WHERE id = ?",
        &[&artiste as &dyn ToSqlValue],
    )
    .unwrap();
    let mut relue = relue_du_fichier();
    relue.artist_id = None;
    Registre::charger(&db).appliquer(&mut relue);
    assert_eq!(
        relue.artist_id, None,
        "clé étrangère vers un artiste disparu"
    );
}

/// Les tenues passent par le crochet unique des analyses :
/// `edition_album::Tenues::appliquer`.
#[test]
fn les_tenues_de_la_fiche_album_portent_les_champs_tenus() {
    let db = base();
    let t = piste_corrigee(&db);
    tenir(&db, &t, &[Champ::Annee]).unwrap();
    let tenues = crate::db::edition_album::Tenues::charger(&db);
    assert!(!tenues.is_empty());
    let mut relue = relue_du_fichier();
    assert!(tenues.appliquer(&mut relue));
    assert_eq!(relue.year, Some(1999));
}
