//! #3067 — une zone navigateur ne porte plus l'étiquette générique de la zone
//! locale du même poste (Fuccaro, fil 1634 : « Cet ordinateur (Browser) » à
//! côté de « This Computer »).
use std::sync::Arc;

use super::*;
use crate::db::backend::DbBackend;
use crate::db::sqlite::SqliteDb;

fn repo() -> ZoneRepo {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    ZoneRepo::with_backend(backend)
}

fn nom(repo: &ZoneRepo, id: i64) -> String {
    repo.get(id).unwrap().unwrap().name
}

#[test]
fn la_zone_navigateur_heritee_ne_porte_plus_le_nom_de_la_zone_locale() {
    let repo = repo();
    let locale = repo
        .create("This Computer", Some("local"), Some("local:Haut-parleurs"))
        .unwrap();
    let navigateur_fr = repo
        .create("Cet ordinateur", Some("browser"), None)
        .unwrap();
    let navigateur_en = repo.create("This computer", Some("browser"), None).unwrap();

    let renommees = repo.distinguer_zones_navigateur_generiques().unwrap();

    assert_eq!(renommees.len(), 2, "renommées : {renommees:?}");
    assert_eq!(nom(&repo, navigateur_fr), "Ce navigateur");
    assert_eq!(nom(&repo, navigateur_en), "This browser");
    // La zone LOCALE garde son étiquette : c'est elle, « cet ordinateur ».
    assert_eq!(nom(&repo, locale), "This Computer");
    // Idempotent : un second démarrage ne renomme plus rien.
    assert!(
        repo.distinguer_zones_navigateur_generiques()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn une_zone_navigateur_suffixee_ou_renommee_n_est_jamais_touchee() {
    let repo = repo();
    let suffixee = repo
        .create("Cet ordinateur (192.168.1.20)", Some("browser"), None)
        .unwrap();
    let perso = repo.create("Casque bureau", Some("browser"), None).unwrap();

    assert!(
        repo.distinguer_zones_navigateur_generiques()
            .unwrap()
            .is_empty()
    );
    assert_eq!(nom(&repo, suffixee), "Cet ordinateur (192.168.1.20)");
    assert_eq!(nom(&repo, perso), "Casque bureau");
}
