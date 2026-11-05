//! Témoins de #7763 côté base : les écoutes importées s'ajoutent à celles de
//! l'historique, sans y écrire une ligne, et une date plus ancienne ne change
//! pas la dernière écoute.

use super::*;
use crate::db::history_repo::HistoryRepo;
use crate::db::models::Track;
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;

fn base() -> (Arc<dyn DbBackend>, i64) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let mut t = Track::new("So What".to_string());
    t.file_path = Some("/m/so-what.flac".into());
    let id = TrackRepo::with_backend(backend.clone()).create(&t).unwrap();
    (backend, id)
}

fn ecoute(db: &dyn DbBackend, titre: &str, quand: &str) {
    db.execute(
        "INSERT INTO listen_history (title, source, listened_at) VALUES (?, 'local', ?)",
        &[&titre, &quand],
    )
    .unwrap();
}

fn lignes_d_historique(db: &dyn DbBackend) -> i64 {
    db.query_one("SELECT COUNT(*) FROM listen_history", &[])
        .unwrap()
        .and_then(|c| c.first().and_then(|v| v.as_i64()))
        .unwrap()
}

#[test]
fn les_ecoutes_importees_s_ajoutent_a_l_historique_sans_y_ecrire_7763() {
    let (db, id) = base();
    ecoute(db.as_ref(), "So What", "2026-10-01T10:00:00Z");
    ecoute(db.as_ref(), "So What", "2026-10-02T10:00:00Z");
    let avant = lignes_d_historique(db.as_ref());

    let repo = EcoutesImporteesRepo::with_backend(db.clone());
    repo.ajouter(id, 5, None).unwrap();
    repo.ajouter(id, 3, None).unwrap();

    let comptes = HistoryRepo::with_backend(db.clone())
        .plays_for_tracks(&[id])
        .unwrap();
    assert_eq!(
        comptes[&id],
        (10, Some("2026-10-02T10:00:00Z".to_string())),
        "2 écoutes d'historique + 5 + 3 importées, la date de l'historique reste"
    );
    assert_eq!(
        lignes_d_historique(db.as_ref()),
        avant,
        "un import ne doit écrire AUCUNE ligne dans listen_history"
    );
}

#[test]
fn une_date_importee_plus_ancienne_ne_change_pas_la_derniere_ecoute_7763() {
    let (db, id) = base();
    ecoute(db.as_ref(), "So What", "2026-10-02T10:00:00Z");
    let repo = EcoutesImporteesRepo::with_backend(db.clone());
    let historique = HistoryRepo::with_backend(db.clone());

    repo.ajouter(id, 1, Some("2025-01-01T00:00:00Z")).unwrap();
    assert_eq!(
        historique.plays_for_tracks(&[id]).unwrap()[&id]
            .1
            .as_deref(),
        Some("2026-10-02T10:00:00Z"),
        "une date importée plus ancienne que l'historique ne doit rien changer"
    );

    repo.ajouter(id, 1, Some("2026-11-01T08:00:00Z")).unwrap();
    // Un second import, plus ancien, ne ramène pas la date en arrière.
    repo.ajouter(id, 1, Some("2026-03-01T08:00:00Z")).unwrap();
    assert_eq!(
        historique.plays_for_tracks(&[id]).unwrap()[&id],
        (4, Some("2026-11-01T08:00:00Z".to_string())),
        "la date la plus récente gagne, d'où qu'elle vienne"
    );
}

#[test]
fn les_dates_d_export_sont_ramenees_au_format_de_l_historique_7763() {
    assert_eq!(
        date_normalisee("1700000000").as_deref(),
        Some("2023-11-14T22:13:20Z")
    );
    assert_eq!(
        date_normalisee("2026-10-02T12:00:00+02:00").as_deref(),
        Some("2026-10-02T10:00:00Z")
    );
    assert_eq!(
        date_normalisee("2026-10-02 10:00:00").as_deref(),
        Some("2026-10-02T10:00:00Z")
    );
    assert_eq!(
        date_normalisee("2026-10-02").as_deref(),
        Some("2026-10-02T00:00:00Z")
    );
    assert_eq!(date_normalisee("0"), None, "0 veut dire « jamais »");
    assert_eq!(date_normalisee("hier"), None);
    assert_eq!(date_normalisee(""), None);
}
