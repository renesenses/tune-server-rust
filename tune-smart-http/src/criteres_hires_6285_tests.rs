//! #6285, deuxième étape — le critère « Verdict hi-res » (`hires_verdict`)
//! dans les deux moteurs : le verdict PUBLIÉ de l'album dans une collection,
//! celui de la piste dans une playlist, « douteux » masqué par défaut.

use std::sync::Arc;

use serde_json::{Value, json};
use tune_core::audio::faux_hires_publie::montrer_douteux;
use tune_core::db::backend::DbBackend;

use super::tests::bibliotheque;
use crate::smart_collections::build_album_query;
use crate::smart_playlists::build_smart_query_rapport;
use crate::smart_refs::{DbRefResolver, EmptyResolver, RefCtx, RefKind, RefResolver, SmartEntity};

/// La bibliothèque des gardes (`criteres::tests::bibliotheque`), mesurée :
/// piste 1 vérifiée, piste 2 douteuse, piste 3 conforme, piste 4 jamais
/// mesurée. Album 1 (pistes 1 et 2) : douteux ; album 2 (piste 3) :
/// conforme ; album 3 (piste 4) : non analysé.
fn mesuree() -> Arc<dyn DbBackend> {
    let db = bibliotheque();
    db.execute_batch(
        "INSERT INTO hires_verifications (track_id, verdict, raison) VALUES \
           (1, 'verifie', NULL), (2, 'douteux', 'bande_de_cd'), (3, 'conforme', NULL);",
    )
    .unwrap();
    Arc::new(db)
}

/// Un résolveur sans entité qui MONTRE « douteux ».
struct Visible;

impl RefResolver for Visible {
    fn smart_entity(&self, _kind: RefKind, _id: i64) -> Option<SmartEntity> {
        None
    }
    fn collection_album_ids(&self, _id: i64) -> Option<Vec<i64>> {
        None
    }
    fn hires_douteux_visible(&self) -> bool {
        true
    }
}

fn regle(op: &str, v: &str) -> String {
    json!([{"field": "hires_verdict", "op": op, "value": v}]).to_string()
}

fn albums_avec(db: &Arc<dyn DbBackend>, regles: &str, resolveur: &dyn RefResolver) -> Vec<i64> {
    let ctx = RefCtx::root(resolveur, Some(1));
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

fn pistes_avec(db: &Arc<dyn DbBackend>, regles: &str, resolveur: &dyn RefResolver) -> Vec<i64> {
    let ctx = RefCtx::root(resolveur, Some(1));
    let (w, _o, _l, refusees) =
        build_smart_query_rapport(regles, "all", "title", "asc", None, &ctx);
    assert!(refusees.is_empty(), "règle refusée {refusees:?} : {regles}");
    let sql = format!(
        "SELECT t.id FROM tracks t \
         LEFT JOIN albums al ON t.album_id = al.id \
         LEFT JOIN artists ar ON t.artist_id = ar.id \
         {w} ORDER BY t.id"
    );
    db.query_many(&sql, &[])
        .unwrap_or_else(|e| panic!("{e}\n{sql}"))
        .iter()
        .map(|r| r[0].as_i64().unwrap())
        .collect()
}

/// 🔴 Playlists : le verdict de la piste, « douteux » masqué par défaut.
#[test]
fn une_playlist_choisit_ses_pistes_sur_le_verdict_hires_6285() {
    let db = mesuree();
    let p = |op, v| pistes_avec(&db, &regle(op, v), &EmptyResolver);
    assert_eq!(p("=", "verifie"), vec![1]);
    assert_eq!(p("=", "conforme"), vec![3]);
    assert_eq!(p("=", "douteux"), Vec::<i64>::new(), "masqué par défaut");
    assert_eq!(
        p("=", "non_analyse"),
        vec![2, 4],
        "masquée, la douteuse est non analysée"
    );
    assert_eq!(p("!=", "verifie"), vec![2, 3, 4]);
    let v = |op, v| pistes_avec(&db, &regle(op, v), &Visible);
    assert_eq!(v("=", "douteux"), vec![2]);
    assert_eq!(v("=", "non_analyse"), vec![4]);
    assert_eq!(v("!=", "douteux"), vec![1, 3, 4]);
}

/// 🔴 Collections : le verdict publié de l'album, celui de son badge dans la
/// liste des albums.
#[test]
fn une_collection_choisit_ses_albums_sur_le_verdict_hires_6285() {
    let db = mesuree();
    let a = |op, v| albums_avec(&db, &regle(op, v), &EmptyResolver);
    assert_eq!(
        a("=", "verifie"),
        Vec::<i64>::new(),
        "l'album 1 a une piste douteuse"
    );
    assert_eq!(a("=", "conforme"), vec![2]);
    assert_eq!(a("=", "douteux"), Vec::<i64>::new(), "masqué par défaut");
    assert_eq!(a("=", "non_analyse"), vec![1, 3]);
    assert_eq!(a("!=", "conforme"), vec![1, 3]);
    let v = |op, v| albums_avec(&db, &regle(op, v), &Visible);
    assert_eq!(v("=", "douteux"), vec![1]);
    assert_eq!(v("=", "non_analyse"), vec![3]);
}

/// Le résolveur branché sur la base lit le réglage de la liste des albums.
#[test]
fn le_resolveur_de_la_base_suit_le_reglage_6285() {
    let db = mesuree();
    let r = regle("=", "douteux");
    assert_eq!(
        pistes_avec(&db, &r, &DbRefResolver::new(&db)),
        Vec::<i64>::new()
    );
    montrer_douteux(&db, true).unwrap();
    assert_eq!(pistes_avec(&db, &r, &DbRefResolver::new(&db)), vec![2]);
    assert_eq!(albums_avec(&db, &r, &DbRefResolver::new(&db)), vec![1]);
}

/// Un verdict inconnu rend FAUX, jamais toute la bibliothèque, et la
/// playlist le dit dans son rapport.
#[test]
fn un_verdict_inconnu_ne_rend_rien_6285() {
    let db = mesuree();
    let r = regle("=", "hires");
    let ctx = RefCtx::root(&EmptyResolver, Some(1));
    let (w, _o, _l, refusees) = build_smart_query_rapport(&r, "all", "title", "asc", None, &ctx);
    assert_eq!(refusees, vec!["hires_verdict =".to_string()]);
    assert!(w.contains("1 = 0"), "{w}");
    assert_eq!(albums_avec(&db, &r, &EmptyResolver), Vec::<i64>::new());
    let r = json!([{"field": "hires_verdict", "op": "contains", "value": "verifie"}]).to_string();
    assert_eq!(albums_avec(&db, &r, &EmptyResolver), Vec::<i64>::new());
}

/// La requête texte : `hires:verifie`, `hires!=conforme`.
#[test]
fn la_requete_texte_connait_hires_6285() {
    let db = mesuree();
    let lire = |texte: &str| {
        let arbre = crate::requete_texte::analyser(texte).unwrap_or_else(|e| panic!("{e:?}"));
        let (regles, mode) = crate::requete_texte::regles_editeur(&arbre).expect(texte);
        assert_eq!(mode, "all");
        Value::Array(regles).to_string()
    };
    assert_eq!(
        pistes_avec(&db, &lire("hires:verifie"), &EmptyResolver),
        vec![1]
    );
    assert_eq!(
        pistes_avec(&db, &lire("hires!=conforme"), &EmptyResolver),
        vec![1, 2, 4]
    );
}
