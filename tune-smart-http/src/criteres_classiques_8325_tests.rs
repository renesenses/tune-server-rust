//! #8325 — les critères « Chef », « Ensemble / orchestre » et « Œuvre » dans
//! les deux moteurs (playlists : la piste ; collections : l'album dont une
//! piste passe), lus dans les mêmes magasins que les facettes : crédits,
//! balises du scan, fiche d'album.

use serde_json::{Value, json};
use tune_core::db::sqlite::SqliteDb;

use super::tests::{albums, bibliotheque, pistes};

/// La bibliothèque des gardes, et :
/// - piste 1 (album 1) : chef Karajan par un crédit MusicBrainz ;
/// - piste 2 (album 1) : ensemble « Wiener Singverein; Berliner
///   Philharmoniker » (balises ENSEMBLE + ORCHESTRA) ;
/// - album 2 (piste 3) : œuvre « Requiem » sur la fiche d'album ;
/// - piste 4 (album 3) : rien.
fn classique() -> SqliteDb {
    let db = bibliotheque();
    db.execute_batch(
        "INSERT INTO track_credits (track_id, artist_name, role) VALUES \
           (1, 'Herbert von Karajan', 'conductor'); \
         INSERT INTO track_metadata (track_id, key, value) VALUES \
           (2, 'ensemble', 'Wiener Singverein; Berliner Philharmoniker'); \
         INSERT INTO album_metadata (album_id, key, value) VALUES (2, 'work', 'Requiem');",
    )
    .unwrap();
    db
}

fn regle(champ: &str, op: &str, v: Value) -> String {
    json!([{"field": champ, "op": op, "value": v}]).to_string()
}

#[test]
fn les_playlists_lisent_chef_ensemble_et_oeuvre_8325() {
    let db = classique();
    let p = |c: &str, op: &str, v: Value| pistes(&db, &regle(c, op, v));
    assert_eq!(p("conductor", "=", json!("herbert von karajan")), [1]);
    assert_eq!(p("conductor", "contains", json!("Karajan")), [1]);
    assert_eq!(
        p("conductor", "!=", json!("Herbert von Karajan")),
        [2, 3, 4]
    );
    assert_eq!(
        p("ensemble", "=", json!("Berliner Philharmoniker")),
        [2],
        "une entrée au milieu de la liste « ; »"
    );
    assert!(
        p("ensemble", "=", json!("Berliner")).is_empty(),
        "« = » compare une entrée entière"
    );
    assert_eq!(p("ensemble", "starts_with", json!("Berliner")), [2]);
    assert_eq!(p("ensemble", "not_contains", json!("Wiener")), [1, 3, 4]);
    assert_eq!(p("work", "=", json!("Requiem")), [3], "fiche d'album");
    assert_eq!(p("work", "in", json!("Messe, Requiem")), [3]);
    assert_eq!(p("work", "is_null", Value::Null), [1, 2, 4]);
    assert_eq!(p("work", "is_not_null", Value::Null), [3]);
}

#[test]
fn les_collections_lisent_chef_ensemble_et_oeuvre_8325() {
    let db = classique();
    let a = |c: &str, op: &str, v: Value| albums(&db, &regle(c, op, v));
    assert_eq!(a("conductor", "=", json!("Herbert von Karajan")), [1]);
    assert_eq!(a("ensemble", "contains", json!("philharmoniker")), [1]);
    assert_eq!(a("work", "=", json!("Requiem")), [2]);
    // Album 1 : la piste 1 n'a pas d'ensemble, la piste 2 en a un. « Ne
    // contient pas » demande qu'AUCUNE piste ne le porte.
    assert_eq!(a("ensemble", "not_contains", json!("Wiener")), [2, 3]);
    assert_eq!(a("conductor", "is_null", Value::Null), [2, 3]);
}
