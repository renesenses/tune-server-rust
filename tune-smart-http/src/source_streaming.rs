//! La règle « Source » quand elle désigne un SERVICE de streaming — #4299.
//!
//! FabienM, fil forum 1812 (16/09/2026), point 14 : « J'ai créé une
//! smartplaylist pour afficher mes titres favoris de Qobuz. J'ai donc créé 2
//! règles : source = Qobuz et Favori est piste mais le résultat me retourne
//! uniquement mes favoris de ma bibliothèque locale. »
//!
//! Deux défauts, et ce module traite le second :
//!
//!  1. `smart_playlists::build_smart_query` n'avait AUCUN bras pour `source` :
//!     la règle tombait sur `_ => continue`. Corrigé dans ce fichier-là.
//!  2. Même appliquée, `t.source` ne vaut que `local` ou `upnp` — mesuré sur le
//!     .18 le 17/09/2026 : 46 877 pistes `local`, 179 `upnp`, rien d'autre. Une
//!     piste Qobuz n'est PAS dans la table `tracks`. « Source = Qobuz » ne
//!     pouvait donc rien rendre.
//!
//! Décision de Bertrand (17/09/2026) : la liste des sources de la règle
//! propose la bibliothèque ET les services, et « Source = Qobuz » ramène ce que
//! Tune connaît durablement de ce service pour le profil : ses FAVORIS
//! (`streaming_favorites` — pistes pour une playlist, albums pour une
//! collection).
//!
//! ## Comment les autres règles s'appliquent à une ligne de service
//!
//! Une ligne `streaming_favorites` ne porte que service, identifiant, titre,
//! artiste, album et pochette. On y traduit ce qui a un sens :
//!
//!  * `source`  → le service ;
//!  * artiste, titre, album → les colonnes du même nom ;
//!  * `favorite` → vrai pour le bon type (ce sont des favoris), faux sinon.
//!
//! Toute AUTRE règle (genre, année, fréquence, dossier…) ne peut pas être
//! évaluée sur une ligne qui n'a pas la donnée : elle vaut FAUX. En mode « toutes
//! les règles », la ligne est donc écartée — on ne prétend pas qu'un favori
//! Qobuz est du jazz sans le savoir. En mode « une des règles », elle ne compte
//! simplement pas.
//!
//! ## Quand ce module intervient
//!
//! SEULEMENT si une règle `source` POSITIVE nomme un service (autre que `local`
//! et `upnp`). Une playlist « Favori est piste » existante ne se met pas à
//! avaler les favoris Qobuz du jour au lendemain : il faut les avoir demandés.

use serde_json::Value;

/// Les provenances qui vivent dans la table `tracks` elle-même.
const SOURCES_BIBLIOTHEQUE: [&str; 2] = ["local", "upnp"];

/// Le type d'objet que la vue intelligente rend.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Objet {
    /// Une playlist intelligente : des pistes.
    Piste,
    /// Une collection intelligente : des albums.
    Album,
}

impl Objet {
    fn item_type(self) -> &'static str {
        match self {
            Objet::Piste => "track",
            Objet::Album => "album",
        }
    }
}

fn texte(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Null) | None => String::new(),
        Some(autre) => autre.to_string(),
    }
}

fn operateur(rule: &Value) -> String {
    let brut = rule
        .get("operator")
        .or_else(|| rule.get("op"))
        .and_then(|v| v.as_str())
        .unwrap_or("contains");
    match brut {
        "=" | "eq" | "equals" | "is" => "=",
        "!=" | "ne" | "neq" | "not_equals" | "is_not" => "!=",
        "is_empty" | "empty" => "is_null",
        "is_not_empty" | "not_empty" => "is_not_null",
        autre => autre,
    }
    .to_string()
}

/// Les valeurs d'une règle : une liste pour `in`, sinon une seule.
fn valeurs(rule: &Value) -> Vec<String> {
    match rule.get("value") {
        Some(Value::Array(a)) => a.iter().map(|v| texte(Some(v))).collect(),
        Some(Value::String(s)) if operateur(rule) == "in" => {
            s.split(',').map(|x| x.trim().to_string()).collect()
        }
        v => vec![texte(v)],
    }
}

fn est_service(valeur: &str) -> bool {
    let v = valeur.trim().to_lowercase();
    !v.is_empty() && !SOURCES_BIBLIOTHEQUE.contains(&v.as_str()) && !v.starts_with("upnp:")
}

/// Une règle `source` positive nomme-t-elle un service ? C'est la condition
/// d'entrée des favoris de streaming dans le résultat.
pub(crate) fn demande_un_service(rules_json: &str) -> bool {
    let rules: Vec<Value> = serde_json::from_str(rules_json).unwrap_or_default();
    rules.iter().any(|r| {
        r.get("field").and_then(|v| v.as_str()) == Some("source")
            && matches!(
                operateur(r).as_str(),
                "=" | "in" | "contains" | "starts_with"
            )
            && valeurs(r).iter().any(|v| est_service(v))
    })
}

fn apostrophes(s: &str) -> String {
    s.replace('\'', "''")
}

/// Motif `LIKE` portable SQLite/PostgreSQL : jokers neutralisés, `\` d'échappement.
fn motif(s: &str) -> String {
    apostrophes(
        &s.replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_"),
    )
}

fn comparaison_texte(col: &str, op: &str, vals: &[String]) -> String {
    let v = vals.first().map(String::as_str).unwrap_or("");
    match op {
        "=" => format!("LOWER({col}) = LOWER('{}')", apostrophes(v)),
        "!=" => format!(
            "({col} IS NULL OR LOWER({col}) != LOWER('{}'))",
            apostrophes(v)
        ),
        "contains" => format!("LOWER({col}) LIKE LOWER('%{}%') ESCAPE '\\'", motif(v)),
        "starts_with" => format!("LOWER({col}) LIKE LOWER('{}%') ESCAPE '\\'", motif(v)),
        "in" => {
            let liste: Vec<String> = vals
                .iter()
                .map(|x| format!("LOWER('{}')", apostrophes(x)))
                .collect();
            if liste.is_empty() {
                "1=0".into()
            } else {
                format!("LOWER({col}) IN ({})", liste.join(", "))
            }
        }
        "is_null" => format!("({col} IS NULL OR {col} = '')"),
        "is_not_null" => format!("({col} IS NOT NULL AND {col} != '')"),
        _ => "1=0".into(),
    }
}

/// La condition d'UNE règle sur une ligne `streaming_favorites sf`.
fn condition(rule: &Value, objet: Objet) -> String {
    let champ = rule.get("field").and_then(|v| v.as_str()).unwrap_or("");
    let op = operateur(rule);
    let vals = valeurs(rule);
    let colonne = match (champ, objet) {
        ("source", _) => Some("sf.service"),
        ("artist" | "artist_name", _) => Some("sf.artist"),
        // Dans une PLAYLIST, `title` est le titre de la piste et `album` celui
        // de son album ; dans une COLLECTION, `title` est le titre de l'album.
        ("title", Objet::Piste) => Some("sf.title"),
        ("album" | "album_title", Objet::Piste) => Some("sf.album"),
        ("title" | "album" | "album_title", Objet::Album) => Some("sf.title"),
        _ => None,
    };
    if let Some(col) = colonne {
        return comparaison_texte(col, &op, &vals);
    }
    if champ == "favorite" {
        // Ce sont des favoris : « est favori de ce type » est vrai, sa
        // négation fausse. Un autre type (un album pour une playlist) : faux.
        let bon_type = vals
            .first()
            .map(|v| v.eq_ignore_ascii_case(objet.item_type()))
            .unwrap_or(false);
        return match (op.as_str(), bon_type) {
            ("=", true) => "1=1".into(),
            ("!=", false) => "1=1".into(),
            _ => "1=0".into(),
        };
    }
    // Une donnée que la ligne n'a pas : FAUX (voir l'en-tête).
    "1=0".into()
}

/// La requête des favoris de service qui satisfont les règles, ou `None`
/// quand aucune règle `source` ne nomme de service.
///
/// Colonnes : service, service_id, title, artist, album, cover_url.
pub(crate) fn requete(
    rules_json: &str,
    match_mode: &str,
    objet: Objet,
    profile_id: i64,
    sort_by: &str,
    sort_order: &str,
    limite: Option<i64>,
) -> Option<String> {
    if !demande_un_service(rules_json) {
        return None;
    }
    let rules: Vec<Value> = serde_json::from_str(rules_json).unwrap_or_default();
    let joiner = if match_mode == "any" { " OR " } else { " AND " };
    let conditions: Vec<String> = rules.iter().map(|r| condition(r, objet)).collect();
    let tri = if sort_by == "random" {
        "RANDOM()".to_string()
    } else {
        let col = match (sort_by, objet) {
            ("artist" | "artist_name", _) => "sf.artist",
            ("album" | "album_title", Objet::Piste) => "sf.album",
            ("added_at", _) => "sf.created_at",
            _ => "sf.title",
        };
        format!(
            "LOWER({col}) {}",
            if sort_order == "desc" { "DESC" } else { "ASC" }
        )
    };
    let limite = limite.map(|n| format!(" LIMIT {n}")).unwrap_or_default();
    Some(format!(
        "SELECT sf.service, sf.service_id, sf.title, sf.artist, sf.album, sf.cover_url \
         FROM streaming_favorites sf \
         WHERE sf.profile_id = {profile_id} AND sf.item_type = '{item}' AND ({conds}) \
         ORDER BY {tri}{limite}",
        item = objet.item_type(),
        conds = conditions.join(joiner),
    ))
}

/// COMBIEN de favoris de service satisfont les règles — la même sélection que
/// [`requete`], comptée au lieu d'être listée.
///
/// 🔴 #1231 — Bertrand, 18/09/2026 : « Smart Collection, source qobuz retourne
/// 0 album ». Mesuré sur le .18 : la collection rend bien ses **3** albums
/// Qobuz quand on l'ouvre, et la liste des collections annonce
/// `"album_count": 0`. Le compte est fait dans la table `albums`
/// (`smart_collections.rs`), où un favori de service n'est jamais : il vient
/// de `streaming_favorites`, ajouté APRÈS le SQL par [`requete`]. La liste
/// était juste, son compteur mentait.
///
/// Sans limite : un compteur dit l'appartenance entière, pas la vue plafonnée.
pub(crate) fn requete_compte(
    rules_json: &str,
    match_mode: &str,
    objet: Objet,
    profile_id: i64,
) -> Option<String> {
    if !demande_un_service(rules_json) {
        return None;
    }
    let rules: Vec<Value> = serde_json::from_str(rules_json).unwrap_or_default();
    let joiner = if match_mode == "any" { " OR " } else { " AND " };
    let conditions: Vec<String> = rules.iter().map(|r| condition(r, objet)).collect();
    Some(format!(
        "SELECT COUNT(*) FROM streaming_favorites sf \
         WHERE sf.profile_id = {profile_id} AND sf.item_type = '{item}' AND ({conds})",
        item = objet.item_type(),
        conds = conditions.join(joiner),
    ))
}

/// Une ligne de la requête, mise à la forme d'une PISTE de playlist
/// intelligente : mêmes clés que les pistes de la bibliothèque, `id` nul, et la
/// paire `source` + `source_id` qui permet au client de l'ouvrir et de la jouer.
pub(crate) fn piste_json(cols: &[tune_core::db::backend::SqlValue]) -> Value {
    serde_json::json!({
        "id": Value::Null,
        "source": cols.first().and_then(|v| v.as_string()),
        "source_id": cols.get(1).and_then(|v| v.as_string()),
        "title": cols.get(2).and_then(|v| v.as_string()),
        "artist_name": cols.get(3).and_then(|v| v.as_string()),
        "album_title": cols.get(4).and_then(|v| v.as_string()),
        "cover_path": cols.get(5).and_then(|v| v.as_string()),
        "duration_ms": 0,
        "format": Value::Null,
        "genre": Value::Null,
        "year": Value::Null,
        "album_id": Value::Null,
        "is_compilation": false,
    })
}

/// Même chose à la forme d'un ALBUM de collection intelligente.
pub(crate) fn album_json(cols: &[tune_core::db::backend::SqlValue]) -> Value {
    serde_json::json!({
        "id": Value::Null,
        "source": cols.first().and_then(|v| v.as_string()),
        "source_id": cols.get(1).and_then(|v| v.as_string()),
        "title": cols.get(2).and_then(|v| v.as_string()),
        "artist_name": cols.get(3).and_then(|v| v.as_string()),
        "year": Value::Null,
        "cover_path": cols.get(5).and_then(|v| v.as_string()),
        "genre": Value::Null,
        "track_count": 0,
        "is_compilation": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FABIEN: &str = r#"[{"field":"source","op":"eq","value":"Qobuz"},
                             {"field":"favorite","op":"is","value":"track"}]"#;

    #[test]
    fn la_playlist_de_fabien_demande_les_favoris_qobuz() {
        assert!(demande_un_service(FABIEN));
        let sql = requete(FABIEN, "all", Objet::Piste, 7, "title", "asc", None).unwrap();
        assert!(sql.contains("sf.profile_id = 7"), "{sql}");
        assert!(sql.contains("sf.item_type = 'track'"), "{sql}");
        assert!(sql.contains("LOWER(sf.service) = LOWER('Qobuz')"), "{sql}");
        assert!(
            sql.contains("1=1"),
            "« favori est piste » vaut vrai : {sql}"
        );
    }

    #[test]
    fn une_source_de_bibliotheque_ne_touche_pas_aux_favoris_de_service() {
        for v in ["local", "UPnP", "upnp:uuid:1234"] {
            let r = format!(r#"[{{"field":"source","op":"eq","value":"{v}"}}]"#);
            assert!(!demande_un_service(&r), "{v}");
            assert!(requete(&r, "all", Objet::Piste, 1, "title", "asc", None).is_none());
        }
    }

    #[test]
    fn sans_regle_source_une_playlist_existante_ne_change_pas() {
        let r = r#"[{"field":"favorite","op":"is","value":"track"}]"#;
        assert!(!demande_un_service(r));
    }

    #[test]
    fn une_negation_ne_fait_pas_entrer_les_services() {
        let r = r#"[{"field":"source","op":"neq","value":"qobuz"}]"#;
        assert!(!demande_un_service(r));
    }

    #[test]
    fn in_avec_une_liste_qui_contient_un_service() {
        let r = r#"[{"field":"source","operator":"in","value":["local","tidal"]}]"#;
        assert!(demande_un_service(r));
        let sql = requete(r, "all", Objet::Album, 1, "title", "asc", None).unwrap();
        assert!(
            sql.contains("LOWER(sf.service) IN (LOWER('local'), LOWER('tidal'))"),
            "{sql}"
        );
        assert!(sql.contains("sf.item_type = 'album'"), "{sql}");
    }

    #[test]
    fn une_regle_sans_donnee_ecarte_la_ligne_en_mode_toutes() {
        let r = r#"[{"field":"source","op":"eq","value":"qobuz"},
                    {"field":"genre","op":"contains","value":"Jazz"}]"#;
        let sql = requete(r, "all", Objet::Piste, 1, "title", "asc", None).unwrap();
        assert!(
            sql.contains("LOWER(sf.service) = LOWER('qobuz') AND 1=0"),
            "{sql}"
        );
        let sql = requete(r, "any", Objet::Piste, 1, "title", "asc", None).unwrap();
        assert!(
            sql.contains("LOWER(sf.service) = LOWER('qobuz') OR 1=0"),
            "{sql}"
        );
    }

    #[test]
    fn artiste_et_titre_filtrent_les_favoris() {
        let r = r#"[{"field":"source","op":"eq","value":"qobuz"},
                    {"field":"artist","op":"contains","value":"Cat's 100%"}]"#;
        let sql = requete(r, "all", Objet::Piste, 1, "artist", "desc", Some(50)).unwrap();
        assert!(
            sql.contains("LOWER(sf.artist) LIKE LOWER('%Cat''s 100\\%%') ESCAPE '\\'"),
            "{sql}"
        );
        assert!(
            sql.ends_with("ORDER BY LOWER(sf.artist) DESC LIMIT 50"),
            "{sql}"
        );
    }

    #[test]
    fn favori_du_mauvais_type_est_faux() {
        let r = r#"[{"field":"source","op":"eq","value":"qobuz"},
                    {"field":"favorite","op":"is","value":"album"}]"#;
        let sql = requete(r, "all", Objet::Piste, 1, "title", "asc", None).unwrap();
        assert!(sql.contains("AND 1=0"), "{sql}");
    }

    /// 🔴 #1231 — le compteur d'une collection ignorait les favoris de service.
    ///
    /// Mesuré sur le .18 le 19/09/2026 : une collection dont la seule règle est
    /// `source = qobuz` rend ses **3** albums quand on l'ouvre, et la liste des
    /// collections annonce `"album_count": 0`. La liste était juste, son
    /// compteur mentait — c'est très probablement le « retourne 0 album » de
    /// Bertrand.
    #[test]
    fn le_compte_des_favoris_de_service_existe_et_vise_le_bon_type() {
        let regles = r#"[{"field":"source","op":"=","value":"qobuz"}]"#;
        let sql = requete_compte(regles, "all", Objet::Album, 1).expect("une règle de service");
        assert!(sql.contains("COUNT(*)"), "{sql}");
        assert!(sql.contains("FROM streaming_favorites sf"), "{sql}");
        assert!(
            sql.contains("sf.item_type = 'album'"),
            "un compte d'ALBUMS : {sql}"
        );
        assert!(sql.contains("sf.profile_id = 1"), "{sql}");
        // Un compteur dit l'appartenance ENTIÈRE : pas de plafond.
        assert!(
            !sql.contains("LIMIT"),
            "un compte ne se plafonne pas : {sql}"
        );
        // Et il vise le même ensemble que la liste.
        let liste =
            requete(regles, "all", Objet::Album, 1, "title", "asc", None).expect("la liste");
        let ou_compte = sql.split("WHERE").nth(1).unwrap();
        let ou_liste = liste.split("WHERE").nth(1).unwrap();
        assert_eq!(
            ou_compte.trim(),
            ou_liste.split("ORDER BY").next().unwrap().trim(),
            "le compte et la liste doivent sélectionner la MÊME chose"
        );
    }

    /// Une playlist compte des PISTES, une collection des ALBUMS.
    #[test]
    fn une_playlist_compte_des_pistes() {
        let sql = requete_compte(
            r#"[{"field":"source","op":"=","value":"qobuz"}]"#,
            "all",
            Objet::Piste,
            7,
        )
        .expect("une règle de service");
        assert!(sql.contains("sf.item_type = 'track'"), "{sql}");
        assert!(sql.contains("sf.profile_id = 7"), "{sql}");
    }

    /// Sans règle de service, il n'y a rien à compter là — et surtout rien à
    /// AJOUTER au compte de la bibliothèque.
    #[test]
    fn sans_regle_de_service_aucun_compte_supplementaire() {
        assert!(
            requete_compte(
                r#"[{"field":"year","op":"=","value":"2025"}]"#,
                "all",
                Objet::Album,
                1
            )
            .is_none()
        );
        assert!(
            requete_compte(
                r#"[{"field":"source","op":"=","value":"local"}]"#,
                "all",
                Objet::Album,
                1
            )
            .is_none()
        );
        assert!(requete_compte("[]", "all", Objet::Album, 1).is_none());
    }
}
