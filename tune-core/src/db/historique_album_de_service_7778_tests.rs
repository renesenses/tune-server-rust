//! #7778, point 6 — fil 2637 (rc10) : dans l'Historique, un titre de service
//! lancé hors de son album (titre phare, playlist, file) n'a plus de lien
//! vers l'album.
//!
//! L'écoute enregistre pourtant `album_ref` (l'album chez le service) et
//! `context_source` ; la liste de l'historique ne les relisait pas
//! (`row_to_listen` les posait à `None`).
//!
//! Contre-épreuve : remettre `album_ref: None` dans `row_to_listen` fait
//! tomber `l_historique_rend_l_album_du_service_d_un_titre_lance_hors_album_7778`.

use super::*;
use crate::db::migrations;

#[test]
fn l_historique_rend_l_album_du_service_d_un_titre_lance_hors_album_7778() {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    let repo = HistoryRepo::new(db);
    repo.record(&ListenRecord {
        title: "Titre phare".into(),
        source: "qobuz".into(),
        source_id: Some("453649358".into()),
        duration_ms: 240_000,
        context_type: Some("artist".into()),
        context_id: Some("artiste-7778".into()),
        context_source: Some("qobuz".into()),
        album_ref: Some("iol01ns87bl3d".into()),
        ..Default::default()
    })
    .unwrap();

    let recent = repo.recent(10).unwrap();
    let (page, _) = repo.recent_paginated(10, 0).unwrap();
    for ecoute in [&recent[0], &page[0]] {
        assert_eq!(
            ecoute.album_ref.as_deref(),
            Some("iol01ns87bl3d"),
            "#7778 : l'historique doit rendre l'album du service, sans quoi le client ne peut \
             pas proposer « Aller à l'album »"
        );
        assert_eq!(ecoute.context_source.as_deref(), Some("qobuz"));
    }
}
