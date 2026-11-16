//! #8416 (forum 2626) — les règles « nombre d'écoutes » et « dernière écoute »
//! des listes ET des collections intelligentes comptent les écoutes
//! IMPORTÉES (`imported_plays`, #7763), comme la Bibliothèque. Elles ne
//! lisaient que `listen_history` : une piste écoutée seulement dans Roon ou
//! Plex restait « jamais écoutée ».
//!
//! Banc `criteres::tests::bibliotheque` (historique : piste 1 « Blue in
//! Green » 3 écoutes, piste 2 « So What » 1 écoute en 2019, pistes 3 et 4
//! aucune), auquel on ajoute par le vrai chemin d'import :
//!
//! - piste 3 « Giant Steps » : 5 écoutes importées, la dernière le 01/08/2024 ;
//! - piste 2 « So What » : 2 écoutes importées sans date (liste m3u).
//!
//! Comptes de la Bibliothèque : 1 → 3, 2 → 3, 3 → 5, 4 → 0. Albums : Kind of
//! Blue (1, 2) → 6, Giant Steps (3) → 5, Seul (4) → 0.
//!
//! Contre-épreuve : sans `imported_plays` dans les fragments partagés, la
//! piste 3 vaut 0 et Giant Steps n'a aucune écoute (rouge).

use std::sync::Arc;

use serde_json::{Value, json};
use tune_core::db::backend::DbBackend;
use tune_core::db::ecoutes_importees::EcoutesImporteesRepo;
use tune_core::db::history_repo::HistoryRepo;
use tune_core::db::sqlite::SqliteDb;

use super::tests::{bibliotheque, pistes};
use crate::smart_collections::build_album_query;
use crate::smart_playlists::build_smart_query_rapport;
use crate::smart_refs::{EmptyResolver, RefCtx};

fn regle(champ: &str, op: &str, v: Value) -> String {
    json!([{"field": champ, "op": op, "value": v}]).to_string()
}

fn avec_ecoutes_importees() -> Arc<SqliteDb> {
    let db = Arc::new(bibliotheque());
    let dyn_db: Arc<dyn DbBackend> = db.clone();
    let import = EcoutesImporteesRepo::with_backend(dyn_db);
    import.ajouter(3, 5, Some("2024-08-01T10:00:00Z")).unwrap();
    import.ajouter(2, 2, None).unwrap();
    db
}

fn albums(db: &SqliteDb, regles: &str) -> Vec<i64> {
    let ctx = RefCtx::root(&EmptyResolver, Some(1));
    let (w, _o, _l) = build_album_query(regles, "all", "title", "asc", None, &ctx);
    let sql = format!(
        "SELECT al.id FROM albums al \
         LEFT JOIN artists ar ON al.artist_id = ar.id \
         LEFT JOIN tracks t ON t.album_id = al.id \
         {w} GROUP BY al.id ORDER BY al.id"
    );
    db.query_many(&sql, &[])
        .unwrap_or_else(|e| panic!("{e}\n{sql}"))
        .iter()
        .map(|r| r[0].as_i64().unwrap())
        .collect()
}

/// 🔴 LE TÉMOIN — chaque règle « nombre d'écoutes » d'une liste rend les
/// pistes que donne le compte de la Bibliothèque, écoutes importées comprises.
#[test]
fn les_listes_comptent_les_ecoutes_importees_8416() {
    let db = avec_ecoutes_importees();
    let dyn_db: Arc<dyn DbBackend> = db.clone();
    let bib = HistoryRepo::with_backend(dyn_db)
        .plays_for_tracks(&[1, 2, 3, 4])
        .unwrap();
    let mut comptes: Vec<(i64, i64)> = bib.iter().map(|(id, (n, _))| (*id, *n)).collect();
    comptes.sort();
    assert_eq!(
        comptes,
        vec![(1, 3), (2, 3), (3, 5), (4, 0)],
        "le banc ne donne plus 3, 3, 5 et 0 écoutes"
    );
    let cas: [(&str, Value, Vec<i64>); 9] = [
        ("=", json!(0), vec![4]),
        ("<", json!(1), vec![4]),
        (">", json!(2), vec![1, 2, 3]),
        ("=", json!(3), vec![1, 2]),
        ("=", json!(5), vec![3]),
        ("!=", json!(0), vec![1, 2, 3]),
        (">=", json!(4), vec![3]),
        ("between", json!([4, 5]), vec![3]),
        ("between", json!([0, 1]), vec![4]),
    ];
    for (op, valeur, attendu) in cas {
        assert_eq!(
            pistes(&db, &regle("play_count", op, valeur.clone())),
            attendu,
            "play_count {op} {valeur} : les écoutes importées ne sont pas comptées"
        );
    }
}

/// « Dernière écoute » : la plus récente de l'historique et de l'import ;
/// « jamais » ne garde que la piste sans aucune écoute.
#[test]
fn la_derniere_ecoute_compte_les_ecoutes_importees_8416() {
    let db = avec_ecoutes_importees();
    assert_eq!(
        pistes(&db, &regle("last_played_at", ">", json!("2024-07-01"))),
        vec![3],
        "la date importée n'est pas lue"
    );
    assert_eq!(
        pistes(&db, &regle("last_played_at", "<", json!("2024-01-01"))),
        vec![2]
    );
    assert_eq!(
        pistes(
            &db,
            &regle(
                "last_played_at",
                "between",
                json!(["2024-07-15", "2024-08-15"])
            )
        ),
        vec![3]
    );
    assert_eq!(
        pistes(&db, &regle("last_played_at", "is_null", json!(null))),
        vec![4],
        "une piste aux écoutes importées n'est pas « jamais écoutée »"
    );
    // Des écoutes importées SANS date (liste m3u) : la piste a été écoutée.
    let dyn_db: Arc<dyn DbBackend> = db.clone();
    EcoutesImporteesRepo::with_backend(dyn_db)
        .ajouter(4, 2, None)
        .unwrap();
    assert!(
        pistes(&db, &regle("last_played_at", "is_null", json!(null))).is_empty(),
        "des écoutes importées sans date restent des écoutes"
    );
    assert!(pistes(&db, &regle("play_count", "=", json!(0))).is_empty());
}

/// Le tri par nombre d'écoutes suit le compte de la Bibliothèque : Giant
/// Steps (5, toutes importées) en tête, la piste jamais écoutée en dernier.
#[test]
fn le_tri_par_ecoutes_compte_les_ecoutes_importees_8416() {
    let db = avec_ecoutes_importees();
    let dyn_db: Arc<dyn DbBackend> = db.clone();
    let etat = crate::SmartHttpState::new(dyn_db);
    let ctx = RefCtx::root(&EmptyResolver, Some(1));
    let (w, o, l, _) = build_smart_query_rapport("[]", "all", "play_count", "desc", None, &ctx);
    let ids: Vec<i64> = crate::smart_playlists::execute_smart_track_query(&etat, 1, &w, &o, &l)
        .unwrap_or_else(|_| panic!("requête du tri refusée"))
        .iter()
        .filter_map(|p| p["id"].as_i64())
        .collect();
    assert_eq!(ids.first(), Some(&3), "tri par écoutes : {ids:?}");
    assert_eq!(ids.last(), Some(&4), "tri par écoutes : {ids:?}");
}

/// Les collections par album comptent de même.
#[test]
fn les_collections_comptent_les_ecoutes_importees_8416() {
    let db = avec_ecoutes_importees();
    assert_eq!(albums(&db, &regle("play_count", "=", json!(0))), vec![3]);
    assert_eq!(albums(&db, &regle("play_count", "=", json!(5))), vec![2]);
    assert_eq!(albums(&db, &regle("play_count", "=", json!(6))), vec![1]);
    assert_eq!(albums(&db, &regle("play_count", ">", json!(4))), vec![1, 2]);
    assert_eq!(albums(&db, &regle("play_count", "<", json!(6))), vec![2, 3]);
    assert_eq!(
        albums(&db, &regle("play_count", "between", json!([0, 5]))),
        vec![2, 3]
    );
    assert_eq!(
        albums(&db, &regle("last_played_at", ">", json!("2024-07-01"))),
        vec![2]
    );
    assert_eq!(
        albums(&db, &regle("last_played_at", "<", json!("2024-07-01"))),
        vec![1]
    );
    assert_eq!(
        albums(&db, &regle("last_played_at", "is_null", json!(null))),
        vec![3]
    );
}
