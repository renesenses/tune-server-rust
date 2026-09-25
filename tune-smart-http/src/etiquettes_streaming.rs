//! La règle « Étiquette » d'une collection intelligente fait entrer les albums
//! de SERVICE étiquetés — #5026.
//!
//! Sevy Tabroc, fil forum 1937 (25/09/2026, 0.9.164) : le sélecteur annonçait
//! « Sept Oct 2026 (1) », l'aperçu « 0 albums correspondent ». Le compteur de
//! `/tags` (`TagRepo::count_per_tag`) additionne `item_tags` ET
//! `streaming_item_tags` (#3699) ; la règle ne lisait que les albums et les
//! artistes de `item_tags`. Un album Qobuz ou Bandcamp étiqueté était compté,
//! jamais rendu.
//!
//! Décision de Bertrand (25/09/2026) : ÉLARGIR la règle. Un album de service
//! étiqueté entre dans la collection, par le même chemin que les favoris de
//! service de la règle « Source » (#4299, `source_streaming`) : même forme
//! JSON (`id` nul, `source` + `source_id`, `cover_path` = pochette de
//! l'instantané), même place (après les albums de la bibliothèque), même
//! borne, et compté dans `album_count` et dans le `total` de l'aperçu.
//!
//! ## Ce qu'une ligne `streaming_item_tags` sait dire
//!
//! Service, identifiant, titre, artiste, album et pochette, posés à
//! l'étiquetage. On y traduit ce qui a un sens :
//!
//!  * `tag`      → l'album porte-t-il cette étiquette (une ligne de la table) ;
//!  * `source`   → le service ;
//!  * artiste, titre → les colonnes du même nom ;
//!  * `favorite` album → l'album est-il un favori de service du profil.
//!
//! Toute AUTRE règle (genre, année, dossier, favori piste ou artiste…) ne
//! peut pas être évaluée sur une ligne qui n'a pas la donnée : elle vaut FAUX,
//! comme dans `source_streaming`. En mode « toutes les règles », l'album est
//! écarté ; en mode « une des règles », elle ne compte pas.
//!
//! ## Quand ce module intervient
//!
//! SEULEMENT si une règle `tag` POSITIVE nomme une étiquette lisible, et
//! seuls les albums de service portant l'une de ces étiquettes sont
//! candidats. « Ne porte pas l'étiquette X » ne fait entrer aucun album de
//! service : une collection existante sans règle positive ne change pas.
//!
//! ## Ce qui n'est PAS couvert
//!
//! * l'étiquette posée sur un ARTISTE de la bibliothèque ne fait pas entrer
//!   les albums de service de cet artiste (un nom ne désigne pas un album de
//!   service) ;
//! * une PISTE de service étiquetée ne fait pas entrer son album : la ligne ne
//!   porte que le TITRE de l'album, pas son identifiant chez le service.

use serde_json::Value;

use crate::smart_refs::{etiquette_de, is_negated};
use crate::source_streaming::{self, Objet};

fn texte(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// Les étiquettes que nomment les règles `tag` POSITIVES et lisibles.
fn etiquettes_demandees(rules: &[Value]) -> Vec<i64> {
    let mut ids: Vec<i64> = rules
        .iter()
        .filter(|r| r.get("field").and_then(|v| v.as_str()) == Some("tag"))
        .filter(|r| !is_negated(crate::regles_sql::lire_op(r)))
        .filter_map(|r| etiquette_de(&texte(r.get("value"))))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// La condition d'UNE règle sur une ligne `streaming_item_tags sit`.
fn condition(rule: &Value, profile_id: i64) -> String {
    let champ = rule.get("field").and_then(|v| v.as_str()).unwrap_or("");
    let op = source_streaming::operateur(rule);
    let vals = source_streaming::valeurs(rule);
    let colonne = match champ {
        "source" => Some("sit.source"),
        "artist" | "artist_name" => Some("sit.artist"),
        "title" | "album" | "album_title" => Some("sit.title"),
        _ => None,
    };
    if let Some(col) = colonne {
        return source_streaming::comparaison_texte(col, &op, &vals);
    }
    let neg = is_negated(crate::regles_sql::lire_op(rule));
    let existe = |sous: String| {
        if neg {
            format!("NOT EXISTS ({sous})")
        } else {
            format!("EXISTS ({sous})")
        }
    };
    match champ {
        "tag" => match etiquette_de(&texte(rule.get("value"))) {
            Some(id) => existe(format!(
                "SELECT 1 FROM streaming_item_tags x9 \
                 WHERE x9.tag_id = {id} AND x9.item_type = 'album' \
                 AND x9.source = sit.source AND x9.source_id = sit.source_id"
            )),
            // Illisible : comme une référence introuvable côté bibliothèque.
            None if neg => "1=1".into(),
            None => "1=0".into(),
        },
        "favorite" if texte(rule.get("value")).eq_ignore_ascii_case("album") => existe(format!(
            "SELECT 1 FROM streaming_favorites f9 \
                 WHERE f9.profile_id = {profile_id} AND f9.item_type = 'album' \
                 AND f9.service = sit.source AND f9.service_id = sit.source_id"
        )),
        // Une donnée que la ligne n'a pas : FAUX (voir l'en-tête).
        _ => "1=0".into(),
    }
}

/// `FROM … WHERE … GROUP BY` : un album de service par paire
/// `source` + `source_id`, quel que soit le nombre d'étiquettes demandées
/// qu'il porte (la clef primaire de la table inclut `tag_id`).
fn selection(rules_json: &str, match_mode: &str, profile_id: i64) -> Option<String> {
    let rules: Vec<Value> = serde_json::from_str(rules_json).unwrap_or_default();
    let ids = etiquettes_demandees(&rules);
    if ids.is_empty() {
        return None;
    }
    let joiner = if match_mode == "any" { " OR " } else { " AND " };
    let conditions: Vec<String> = rules.iter().map(|r| condition(r, profile_id)).collect();
    // Un album que le chemin des favoris (`source_streaming`) rend déjà n'est
    // pas rendu une seconde fois.
    let deja = source_streaming::filtre(rules_json, match_mode, Objet::Album, profile_id)
        .map(|ou| {
            format!(
                " AND NOT EXISTS (SELECT 1 FROM streaming_favorites sf WHERE {ou} \
                 AND sf.service = sit.source AND sf.service_id = sit.source_id)"
            )
        })
        .unwrap_or_default();
    let liste = ids
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "FROM streaming_item_tags sit \
         WHERE sit.item_type = 'album' AND sit.tag_id IN ({liste}) \
         AND ({conds}){deja} \
         GROUP BY sit.source, sit.source_id",
        conds = conditions.join(joiner),
    ))
}

/// Les albums de service étiquetés que les règles sélectionnent, ou `None`
/// sans règle `tag` positive.
///
/// Colonnes : service, service_id, title, artist, album, cover_url — celles de
/// `source_streaming::requete`, pour `source_streaming::album_json`.
pub(crate) fn requete(
    rules_json: &str,
    match_mode: &str,
    profile_id: i64,
    sort_by: &str,
    sort_order: &str,
    limite: Option<i64>,
) -> Option<String> {
    let selection = selection(rules_json, match_mode, profile_id)?;
    let tri = if sort_by == "random" {
        "RANDOM()".to_string()
    } else {
        let col = match sort_by {
            "artist" | "artist_name" => "MAX(sit.artist)",
            "added_at" => "MAX(sit.created_at)",
            _ => "MAX(sit.title)",
        };
        format!(
            "LOWER({col}) {}",
            if sort_order == "desc" { "DESC" } else { "ASC" }
        )
    };
    let limite = limite
        .filter(|n| *n > 0)
        .map(|n| format!(" LIMIT {n}"))
        .unwrap_or_default();
    Some(format!(
        "SELECT sit.source, sit.source_id, MAX(sit.title), MAX(sit.artist), \
         MAX(sit.album), MAX(sit.cover_url) {selection} ORDER BY {tri}{limite}"
    ))
}

/// COMBIEN d'albums de service étiquetés les règles sélectionnent — la même
/// sélection que [`requete`], sans borne.
pub(crate) fn requete_compte(
    rules_json: &str,
    match_mode: &str,
    profile_id: i64,
) -> Option<String> {
    let selection = selection(rules_json, match_mode, profile_id)?;
    Some(format!(
        "SELECT COUNT(*) FROM (SELECT sit.source, sit.source_id {selection}) z9"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tune_core::db::backend::DbBackend;
    use tune_core::db::sqlite::SqliteDb;

    /// Une base MIGRÉE : les vraies tables `streaming_item_tags` et
    /// `streaming_favorites`, pas une copie qui pourrait diverger.
    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        db.execute_batch(
            "INSERT INTO tags (id, name) VALUES (12,'Sept Oct 2026'),(13,'Autre'); \
             INSERT INTO streaming_item_tags \
               (tag_id, item_type, source, source_id, title, artist, cover_url, created_at) VALUES \
               (12,'album','qobuz','q1','Sevy','Artiste A','https://c/q1.jpg','2026-09-25T10:00:00Z'), \
               (13,'album','qobuz','q1','Sevy','Artiste A','https://c/q1.jpg','2026-09-25T10:00:00Z'), \
               (12,'album','bandcamp','b1','Bandcamp','Artiste B',NULL,'2026-09-25T11:00:00Z'), \
               (13,'album','tidal','t1','Tidal seul','Artiste C',NULL,'2026-09-25T12:00:00Z'), \
               (12,'track','qobuz','p1','Une piste','Artiste A',NULL,'2026-09-25T12:00:00Z'); \
             INSERT INTO streaming_favorites \
               (profile_id, item_type, service, service_id, title, artist, created_at) VALUES \
               (1,'album','qobuz','q1','Sevy','Artiste A','2026-09-25T10:00:00Z');",
        )
        .unwrap();
        Arc::new(db)
    }

    fn titres(b: &Arc<dyn DbBackend>, regles: &str, mode: &str) -> Vec<String> {
        let Some(sql) = requete(regles, mode, 1, "title", "asc", None) else {
            return vec![];
        };
        b.query_many(&sql, &[])
            .unwrap_or_else(|e| panic!("{e}\n{sql}"))
            .iter()
            .map(|r| r[2].as_string().unwrap_or_default())
            .collect()
    }

    fn compte(b: &Arc<dyn DbBackend>, regles: &str, mode: &str) -> i64 {
        let Some(sql) = requete_compte(regles, mode, 1) else {
            return 0;
        };
        b.query_many(&sql, &[])
            .unwrap_or_else(|e| panic!("{e}\n{sql}"))
            .first()
            .and_then(|r| r.first())
            .and_then(|v| v.as_i64())
            .unwrap()
    }

    #[test]
    fn l_album_de_service_etiquete_entre_une_seule_fois() {
        let b = base();
        let r = r#"[{"field":"tag","op":"is","value":"12"}]"#;
        // q1 porte 12 ET 13 : une seule ligne ; la PISTE p1 n'est pas un album.
        assert_eq!(titres(&b, r, "all"), vec!["Bandcamp", "Sevy"]);
        assert_eq!(compte(&b, r, "all"), 2);
        // Deux règles positives en « une des règles » : toujours une ligne par album.
        let deux =
            r#"[{"field":"tag","op":"is","value":"12"},{"field":"tag","op":"is","value":"13"}]"#;
        assert_eq!(
            titres(&b, deux, "any"),
            vec!["Bandcamp", "Sevy", "Tidal seul"]
        );
        assert_eq!(compte(&b, deux, "any"), 3);
        // Les deux à la fois : seul q1.
        assert_eq!(titres(&b, deux, "all"), vec!["Sevy"]);
    }

    #[test]
    fn la_ligne_rendue_a_la_forme_d_un_album_de_service() {
        let b = base();
        let sql = requete(
            r#"[{"field":"tag","op":"is","value":"12"}]"#,
            "all",
            1,
            "title",
            "desc",
            None,
        )
        .unwrap();
        let lignes = b.query_many(&sql, &[]).unwrap();
        let album = source_streaming::album_json(&lignes[0]);
        assert_eq!(album["id"], Value::Null);
        assert_eq!(album["source"], "qobuz");
        assert_eq!(album["source_id"], "q1");
        assert_eq!(album["title"], "Sevy");
        assert_eq!(album["artist_name"], "Artiste A");
        assert_eq!(album["cover_path"], "https://c/q1.jpg");
    }

    #[test]
    fn sans_regle_positive_aucun_album_de_service() {
        for r in [
            r#"[{"field":"tag","op":"is_not","value":"12"}]"#,
            r#"[{"field":"tag","op":"is","value":"x"}]"#,
            r#"[{"field":"genre","op":"contains","value":"jazz"}]"#,
            "[]",
        ] {
            assert!(requete(r, "all", 1, "title", "asc", None).is_none(), "{r}");
            assert!(requete_compte(r, "all", 1).is_none(), "{r}");
        }
    }

    #[test]
    fn les_autres_regles_s_appliquent_a_la_ligne() {
        let b = base();
        let avec = |autre: &str| format!(r#"[{{"field":"tag","op":"is","value":"12"}},{autre}]"#);
        assert_eq!(
            titres(
                &b,
                &avec(r#"{"field":"source","op":"=","value":"bandcamp"}"#),
                "all"
            ),
            vec!["Bandcamp"]
        );
        assert_eq!(
            titres(
                &b,
                &avec(r#"{"field":"artist","op":"=","value":"artiste a"}"#),
                "all"
            ),
            vec!["Sevy"]
        );
        assert_eq!(
            titres(
                &b,
                &avec(r#"{"field":"favorite","op":"is","value":"album"}"#),
                "all"
            ),
            vec!["Sevy"]
        );
        assert_eq!(
            titres(
                &b,
                &avec(r#"{"field":"favorite","op":"is_not","value":"album"}"#),
                "all"
            ),
            vec!["Bandcamp"]
        );
        assert_eq!(
            titres(
                &b,
                &avec(r#"{"field":"tag","op":"is_not","value":"13"}"#),
                "all"
            ),
            vec!["Bandcamp"]
        );
        // Une donnée que la ligne n'a pas : écartée en « toutes », neutre en « une des ».
        let genre = avec(r#"{"field":"genre","op":"contains","value":"jazz"}"#);
        assert!(titres(&b, &genre, "all").is_empty());
        assert_eq!(titres(&b, &genre, "any"), vec!["Bandcamp", "Sevy"]);
    }

    /// q1 est à la fois favori Qobuz et étiqueté : la règle « Source = qobuz »
    /// le rend déjà par `source_streaming` ; il ne doit pas revenir ici.
    #[test]
    fn un_favori_deja_rendu_par_la_source_n_est_pas_double() {
        let b = base();
        let r = r#"[{"field":"source","op":"=","value":"qobuz"},{"field":"tag","op":"is","value":"12"}]"#;
        assert_eq!(titres(&b, r, "any"), vec!["Bandcamp"]);
        assert_eq!(compte(&b, r, "any"), 1);
        // En « toutes », le chemin des favoris ne rend rien (la règle `tag` y
        // vaut FAUX) : c'est ici que q1 entre.
        assert_eq!(titres(&b, r, "all"), vec!["Sevy"]);
    }
}
