//! #7335 (laguespa, fil 2510) — une seule collection qui ne peut pas être
//! servie ne vide plus l'écran Collections.
//!
//! `list_collections` collectait en `Result<Vec<_>, _>` : la première
//! erreur de `dossier_servi` faisait répondre `500` à la liste ENTIÈRE, et
//! l'écran n'affichait plus rien — pas même le dossier qu'on venait de créer.

use serde_json::{Value, json};

use super::servir_en_ecartant;
use crate::error::AppError;

fn ids(servis: &[Value]) -> Vec<i64> {
    servis.iter().filter_map(|c| c["id"].as_i64()).collect()
}

#[test]
fn un_dossier_fautif_est_ecarte_les_autres_restent_7335() {
    let stockes = vec![json!({"id": 1}), json!({"id": 2}), json!({"id": 3})];
    let servis = servir_en_ecartant(&stockes, |c| {
        if c["id"] == 2 {
            Err(AppError::internal("lecture des albums impossible"))
        } else {
            Ok(c.clone())
        }
    });
    let servis = match servis {
        Ok(s) => s,
        Err(e) => panic!(
            "#7335 : un seul dossier illisible fait échouer toute la liste ({}) — l'écran Collections reste vide",
            e.message
        ),
    };
    assert_eq!(ids(&servis), vec![1, 3]);
}

#[test]
fn base_muette_pour_tous_reste_une_erreur_7335() {
    let stockes = vec![json!({"id": 1}), json!({"id": 2})];
    let servis = servir_en_ecartant(&stockes, |_| Err(AppError::internal("base muette")));
    assert!(
        servis.is_err(),
        "aucun dossier servable : une liste vide mentirait, l'erreur doit remonter"
    );
}

#[test]
fn aucun_dossier_rend_une_liste_vide_7335() {
    let servis = servir_en_ecartant(&[], |c| Ok(c.clone())).ok().unwrap();
    assert!(servis.is_empty());
}
