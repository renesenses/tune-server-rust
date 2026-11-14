//! #7769 — les règles « nombre d'écoutes » et « dernière écoute » comptent
//! comme la Bibliothèque : par titre + interprète, hors radio. Elles lisaient
//! `listen_history.track_id`, que la production laisse toujours vide :
//! « = 0 » rendait toute la bibliothèque et « < 1 » rien du tout.
//!
//! Les écoutes de `criteres::tests::bibliotheque` sont écrites comme en
//! production (`track_id` vide) : piste 1 « Blue in Green », 3 écoutes ;
//! piste 2 « So What », 1 écoute ; pistes 3 et 4, aucune. On y ajoute deux
//! écoutes qui ne doivent PAS compter : une écoute radio de « Giant Steps »
//! et un « Solo » d'un autre interprète.

use serde_json::{Value, json};
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;

use super::tests::{bibliotheque, pistes};
use crate::smart_collections::build_album_query;
use crate::smart_playlists::build_smart_query_rapport;
use crate::smart_refs::{EmptyResolver, RefCtx};

fn regle(champ: &str, op: &str, v: Value) -> String {
    json!([{"field": champ, "op": op, "value": v}]).to_string()
}

fn avec_ecoutes_a_ne_pas_compter() -> SqliteDb {
    let db = bibliotheque();
    db.execute_batch(
        "INSERT INTO listen_history (title, artist_name, source, listened_at) VALUES \
           ('Giant Steps','John Coltrane','radio','2024-07-01T10:00:00Z'), \
           ('Solo','Quelqu''un d''autre','local','2024-07-01T10:00:00Z');",
    )
    .unwrap();
    db
}

/// Le nombre d'écoutes que la Bibliothèque affiche pour chaque piste :
/// la requête même de `/library/tracks` (`HistoryRepo::plays_for_tracks`).
fn ecoutes_de_la_bibliotheque(db: &SqliteDb) -> Vec<(i64, i64)> {
    let sql = tune_core::db::history_repo::sql::plays_for_tracks("1,2,3,4");
    let mut v: Vec<(i64, i64)> = db
        .query_many(&sql, &[])
        .unwrap()
        .iter()
        .map(|r| (r[0].as_i64().unwrap(), r[1].as_i64().unwrap()))
        .collect();
    v.sort();
    v
}

/// 🔴 LE TÉMOIN — pistes à 0, 1 et 3 écoutes : chaque règle rend exactement
/// les bonnes pistes, avec la valeur en texte comme en nombre JSON.
#[test]
fn les_regles_d_ecoutes_rendent_exactement_les_bonnes_pistes_7769() {
    let db = avec_ecoutes_a_ne_pas_compter();
    assert_eq!(
        ecoutes_de_la_bibliotheque(&db),
        vec![(1, 3), (2, 1), (3, 0), (4, 0)],
        "le banc ne donne plus 3, 1, 0 et 0 écoutes"
    );
    let cas: [(&str, Value, Vec<i64>); 10] = [
        ("=", json!(0), vec![3, 4]),
        ("<", json!(1), vec![3, 4]),
        (">", json!(2), vec![1]),
        ("=", json!(3), vec![1]),
        ("=", json!("1"), vec![2]),
        ("!=", json!(0), vec![1, 2]),
        ("<=", json!(1), vec![2, 3, 4]),
        (">=", json!(1), vec![1, 2]),
        ("between", json!([1, 3]), vec![1, 2]),
        ("between", json!([0, 1]), vec![2, 3, 4]),
    ];
    for (op, valeur, attendu) in cas {
        assert_eq!(
            pistes(&db, &regle("play_count", op, valeur.clone())),
            attendu,
            "play_count {op} {valeur} : mauvaises pistes (écoutes comptées par track_id ?)"
        );
    }
}

/// Une valeur qui n'est pas un entier ne vaut plus 0 : la règle est refusée,
/// nommée dans le rapport, et ne retient rien.
#[test]
fn une_valeur_d_ecoutes_illisible_est_refusee_7769() {
    let ctx = RefCtx::root(&EmptyResolver, Some(1));
    for valeur in [json!("abc"), json!("1; DROP TABLE tracks"), json!(1.5)] {
        let (w, _o, _l, refusees) = build_smart_query_rapport(
            &regle("play_count", "=", valeur.clone()),
            "all",
            "title",
            "asc",
            None,
            &ctx,
        );
        assert_eq!(refusees, vec!["play_count =".to_string()], "{valeur}");
        assert_eq!(w, format!("WHERE {}", crate::regles_sql::FAUX), "{valeur}");
    }
}

/// « Dernière écoute » suit la même règle : les écoutes sans `track_id`
/// comptent, la radio et l'homonyme d'un autre interprète non.
#[test]
fn la_derniere_ecoute_compte_comme_la_bibliotheque_7769() {
    let db = avec_ecoutes_a_ne_pas_compter();
    assert_eq!(
        pistes(&db, &regle("last_played_at", ">", json!("2024-01-01"))),
        vec![1]
    );
    assert_eq!(
        pistes(&db, &regle("last_played_at", "<", json!("2024-01-01"))),
        vec![2]
    );
    assert_eq!(
        pistes(&db, &regle("last_played_at", "is_null", json!(null))),
        vec![3, 4]
    );
}

/// Le tri par nombre d'écoutes range les pistes par le compte de la
/// Bibliothèque : 3, puis 1, puis les jamais écoutées.
#[test]
fn le_tri_par_ecoutes_suit_le_compte_de_la_bibliotheque_7769() {
    let db = avec_ecoutes_a_ne_pas_compter();
    let etat = crate::SmartHttpState::new(std::sync::Arc::new(db));
    let ctx = RefCtx::root(&EmptyResolver, Some(1));
    let (w, o, l, _) = build_smart_query_rapport("[]", "all", "play_count", "desc", None, &ctx);
    let ids: Vec<i64> = crate::smart_playlists::execute_smart_track_query(&etat, 1, &w, &o, &l)
        .unwrap_or_else(|_| panic!("requête du tri refusée"))
        .iter()
        .filter_map(|p| p["id"].as_i64())
        .collect();
    assert_eq!(&ids[..2], &[1, 2], "tri par écoutes : {ids:?}");
}

/// Les collections par album comptent de même : Kind of Blue (pistes 1 et 2)
/// a 4 écoutes, Giant Steps et Seul aucune.
#[test]
fn les_collections_comptent_les_ecoutes_comme_la_bibliotheque_7769() {
    let db = avec_ecoutes_a_ne_pas_compter();
    let ctx = RefCtx::root(&EmptyResolver, Some(1));
    let albums = |regles: String| -> Vec<i64> {
        let (w, _o, _l) = build_album_query(&regles, "all", "title", "asc", None, &ctx);
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
    };
    assert_eq!(albums(regle("play_count", "=", json!(0))), vec![2, 3]);
    assert_eq!(albums(regle("play_count", "=", json!(4))), vec![1]);
    assert_eq!(albums(regle("play_count", ">", json!(2))), vec![1]);
    assert_eq!(albums(regle("play_count", "<", json!(1))), vec![2, 3]);
    assert_eq!(
        albums(regle("last_played_at", "is_null", json!(null))),
        vec![2, 3]
    );
    assert!(
        albums(regle("play_count", "=", json!("abc"))).is_empty(),
        "une valeur illisible ne vaut plus 0"
    );
}

/// Une écoute dont `track_id` désigne la piste lui revient, même sans
/// interprète enregistré : c'était la règle d'avant #7769, et la jointure
/// par titre + interprète seule perdait ces écoutes (`pg_date_relative_6122`
/// rougissait). « Solo » d'Autre (piste 4) reçoit une telle écoute ;
/// l'homonyme d'un autre interprète, sans `track_id`, ne compte toujours pas.
#[test]
fn une_ecoute_rattachee_par_track_id_compte_sans_interprete_7769() {
    let db = avec_ecoutes_a_ne_pas_compter();
    db.execute_batch(
        "INSERT INTO listen_history (track_id, title, listened_at) VALUES \
           (4,'Solo','2024-07-02T10:00:00Z');",
    )
    .unwrap();
    assert_eq!(
        pistes(&db, &regle("play_count", "=", json!(1))),
        vec![2, 4],
        "l'écoute rattachée par track_id n'est pas comptée"
    );
    assert_eq!(pistes(&db, &regle("play_count", "=", json!(0))), vec![3]);
    assert_eq!(
        pistes(&db, &regle("last_played_at", ">", json!("2024-07-01"))),
        vec![4],
        "la dernière écoute rattachée par track_id est perdue"
    );
    let ctx = RefCtx::root(&EmptyResolver, Some(1));
    let (w, _o, _l) = build_album_query(
        &regle("last_played_at", ">", json!("2024-07-01")),
        "all",
        "title",
        "asc",
        None,
        &ctx,
    );
    let sql = format!(
        "SELECT al.id FROM albums al \
         LEFT JOIN artists ar ON al.artist_id = ar.id \
         LEFT JOIN tracks t ON t.album_id = al.id \
         {w} GROUP BY al.id ORDER BY al.id"
    );
    let albums: Vec<i64> = db
        .query_many(&sql, &[])
        .unwrap_or_else(|e| panic!("{e}\n{sql}"))
        .iter()
        .map(|r| r[0].as_i64().unwrap())
        .collect();
    assert_eq!(
        albums,
        vec![3],
        "collections : la dernière écoute rattachée par track_id est perdue"
    );
}

/// #8714 — mêmes titre et interprète, deux pistes du même album et une
/// copie d'un autre album : les nouvelles écoutes ne se propagent pas.
#[test]
fn les_playlists_departagent_les_homonymes_par_identifiant_8714() {
    let db = bibliotheque();
    db.execute_batch(
        "DELETE FROM listen_history;
         INSERT INTO tracks (id,album_id,artist_id,title,file_path) VALUES
           (5,1,1,'Blue in Green','/m/reprise.flac'),
           (6,2,1,'Blue in Green','/m/compilation.flac');
         INSERT INTO listen_history (track_id,album_id,title,artist_name,source,listened_at)
           VALUES (1,1,'Blue in Green','Miles Davis','local','2024-07-02T10:00:00Z');",
    )
    .unwrap();
    assert_eq!(
        pistes(&db, &regle("play_count", "=", json!(1))),
        vec![1],
        "l'identifiant doit exclure même la reprise homonyme du même album"
    );
    assert_eq!(
        pistes(&db, &regle("last_played_at", ">", json!("2024-07-01"))),
        vec![1]
    );
    db.execute_batch(
        "INSERT INTO listen_history (album_id,title,artist_name,source,listened_at)
           VALUES (1,'Blue in Green','Miles Davis','local','2020-01-01T10:00:00Z');",
    )
    .unwrap();
    assert_eq!(pistes(&db, &regle("play_count", "=", json!(2))), vec![1]);
    assert_eq!(
        pistes(&db, &regle("play_count", "=", json!(1))),
        vec![5],
        "l'ancien historique sans identifiant reste borné à son album"
    );
    db.execute_batch(
        "INSERT INTO listen_history (title,artist_name,source,listened_at)
           VALUES ('Blue in Green','Miles Davis','local','2019-01-01T10:00:00Z');",
    )
    .unwrap();
    assert_eq!(pistes(&db, &regle("play_count", "=", json!(3))), vec![1]);
    assert_eq!(pistes(&db, &regle("play_count", "=", json!(2))), vec![5]);
    assert_eq!(pistes(&db, &regle("play_count", "=", json!(1))), vec![6]);
}
