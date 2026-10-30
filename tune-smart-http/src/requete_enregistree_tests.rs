//! #6287, étape 4 — témoins de la règle `requete` : sa forme, son évaluation
//! par le moteur des playlists sur une vraie base SQLite, ses refus et la
//! détection de cycle à travers son texte.
//!
//! Bibliothèque partagée des critères (#5547) : 1 `Blue in Green` (Jazz,
//! 1959), 2 `So What` (Jazz, 1959), 3 `Giant Steps` (Jazz, 1960), 4 `Solo`
//! (Rock, 2001).

use super::*;
use crate::criteres::tests::{bibliotheque, pistes};
use crate::smart_playlists::build_smart_query_rapport;
use crate::smart_refs::{EmptyResolver, RefKind, RefResolver, SmartEntity, check_no_cycle};
use tune_core::db::backend::DbBackend;

const IMBRIQUEE: &str = "non genre:rock (annee:1959 ou annee:2001)";

fn enregistree(texte: &str) -> String {
    let arbre = requete_texte::analyser(texte).unwrap();
    let (regles, mode) = enregistrement(texte, &arbre);
    assert_eq!(mode, "all");
    Value::Array(regles).to_string()
}

/// Les règles refusées d'un jeu de règles, par le moteur des playlists.
fn refusees(regles: &str) -> Vec<String> {
    let ctx = RefCtx::root(&EmptyResolver, Some(1));
    build_smart_query_rapport(regles, "all", "title", "asc", None, &ctx).3
}

#[test]
fn une_requete_plate_garde_ses_regles_d_editeur_6287() {
    let texte = "genre:jazz annee:1959";
    let arbre = requete_texte::analyser(texte).unwrap();
    let (regles, mode) = enregistrement(texte, &arbre);
    assert_eq!(mode, "all");
    assert_eq!(
        regles,
        vec![
            json!({"field": "genre", "op": "contains", "value": "jazz"}),
            json!({"field": "year", "op": "=", "value": 1959}),
        ]
    );
    let arbre = requete_texte::analyser("genre:rock ou annee:1960").unwrap();
    assert_eq!(enregistrement("genre:rock ou annee:1960", &arbre).1, "any");
}

#[test]
fn une_requete_imbriquee_s_enregistre_en_une_regle_requete_6287() {
    let arbre = requete_texte::analyser(IMBRIQUEE).unwrap();
    let (regles, mode) = enregistrement(&format!("  {IMBRIQUEE} "), &arbre);
    assert_eq!(mode, "all");
    assert_eq!(
        regles,
        vec![json!({"field": "requete", "op": "is", "value": IMBRIQUEE})]
    );
}

/// Le témoin principal : la règle enregistrée retient les MÊMES pistes que
/// la requête essayée, par le moteur des playlists.
#[test]
fn la_regle_requete_s_evalue_comme_la_requete_6287() {
    let db = bibliotheque();
    assert_eq!(pistes(&db, &enregistree(IMBRIQUEE)), vec![1, 2]);
    assert_eq!(
        pistes(&db, &enregistree("genre:jazz non (annee:1959 titre:blue)")),
        vec![2, 3]
    );
    // À côté d'une règle ordinaire, elle se combine comme une règle.
    let regles = json!([
        regle(IMBRIQUEE),
        {"field": "title", "op": "contains", "value": "what"}
    ]);
    assert_eq!(pistes(&db, &regles.to_string()), vec![2]);
}

/// Échec fermé : une règle `requete` qui ne s'évalue plus ne retient RIEN, et
/// se nomme dans le rapport — jamais toute la bibliothèque (#1231, #4467).
#[test]
fn une_regle_requete_illisible_ne_retient_rien_et_se_nomme_6287() {
    let db = bibliotheque();
    for regle_fautive in [
        regle("genre:jazz couleur:bleu"),
        regle("non ajout:foo..bar"),
        json!({"field": "requete", "op": "is_not", "value": IMBRIQUEE}),
        json!({"field": "requete", "op": "is", "value": 12}),
    ] {
        let regles = Value::Array(vec![regle_fautive.clone()]).to_string();
        assert_eq!(
            refusees(&regles),
            vec![format!(
                "requete {}",
                regles_sql::normaliser_op(regles_sql::lire_op(&regle_fautive))
            )],
            "{regles}"
        );
        let ctx = RefCtx::root(&EmptyResolver, Some(1));
        let (w, ..) = build_smart_query_rapport(&regles, "all", "title", "asc", None, &ctx);
        let sql = format!("SELECT COUNT(*) FROM tracks t {w}");
        let n = db.query_one(&sql, &[]).unwrap().unwrap()[0].as_i64();
        assert_eq!(n, Some(0), "{regles} : {sql}");
    }
}

#[test]
fn un_refus_hors_negation_reste_signale_6287() {
    assert_eq!(
        refusees(&enregistree("genre:jazz (ajout:foo..bar ou annee:1959)")),
        vec!["added_at between".to_string()]
    );
}

#[test]
fn l_enregistrement_refuse_une_requete_fautive_6287() {
    let ctx = RefCtx::root(&EmptyResolver, None);
    assert_eq!(valider(&json!([regle(IMBRIQUEE)]), &ctx), Ok(()));
    // Les autres règles ne sont pas son affaire.
    assert_eq!(
        valider(&json!([{"field": "genre", "op": "zz", "value": 1}]), &ctx),
        Ok(())
    );
    let Err(Refus::Illisible(e)) = valider(&json!([regle("genre:jazz couleur:bleu")]), &ctx) else {
        panic!("une requête illisible doit être refusée");
    };
    assert_eq!(e.position, 11);
    let refus = valider(&json!([regle("non ajout:foo..bar")]), &ctx).unwrap_err();
    assert!(matches!(refus, Refus::SousNegation(_)), "{refus:?}");
    let corps = refus.corps();
    assert_eq!(corps["error"], "requete_invalide");
    assert_eq!(corps["position"], Value::Null);
    // Le même tableau, encodé en chaîne.
    let en_chaine = Value::String(json!([regle("(")]).to_string());
    assert!(valider(&en_chaine, &ctx).is_err());
}

struct UnePlaylist(&'static str);

impl RefResolver for UnePlaylist {
    fn smart_entity(&self, kind: RefKind, id: i64) -> Option<SmartEntity> {
        (kind == RefKind::SmartPlaylist && id == 7).then(|| SmartEntity {
            rules_json: self.0.to_string(),
            match_mode: "all".into(),
            name: "Sept".into(),
        })
    }
    fn collection_album_ids(&self, _id: i64) -> Option<Vec<i64>> {
        None
    }
}

/// Une référence écrite dans le TEXTE d'une requête compte pour la
/// détection de cycle, comme celle d'une règle d'éditeur.
#[test]
fn un_cycle_a_travers_une_requete_est_refuse_6287() {
    let vers_sept = json!([regle(
        r#"non genre:rock (playlist:"smart:7" ou annee:1959)"#
    )]);
    assert_eq!(
        feuilles(&vers_sept[0]),
        vec![
            json!({"field": "genre", "op": "contains", "value": "rock"}),
            json!({"field": "in_playlist", "op": "in", "value": "smart:7"}),
            json!({"field": "year", "op": "=", "value": 1959}),
        ]
    );
    // « Sept » cite déjà la playlist 3 : enregistrer la 3 vers « Sept » boucle.
    let sept = UnePlaylist(r#"[{"field":"in_playlist","op":"in","value":"smart:3"}]"#);
    let err = check_no_cycle(
        &sept,
        RefKind::SmartPlaylist,
        Some(3),
        "Trois",
        &vers_sept.to_string(),
    )
    .unwrap_err();
    assert!(err.contains("circulaire"), "{err}");
    // Sans boucle, elle passe.
    let sept = UnePlaylist("[]");
    assert!(
        check_no_cycle(
            &sept,
            RefKind::SmartPlaylist,
            Some(3),
            "Trois",
            &vers_sept.to_string()
        )
        .is_ok()
    );
}
