//! L'enveloppe `subsonic-response`, en XML ou en JSON selon le paramètre `f`.
//!
//! Une seule description du contenu, en JSON, et une traduction mécanique
//! vers le XML de l'API Subsonic :
//!
//! - un objet devient un élément ; ses valeurs simples en sont les attributs,
//!   ses objets et ses tableaux en sont les enfants ;
//! - un tableau répète l'élément qui porte son nom ; un tableau de valeurs
//!   simples donne des éléments à texte (`<versions>1</versions>`).
//!
//! C'est la correspondance que les clients Subsonic attendent des deux formes
//! (`"musicFolders": {"musicFolder": [{"id": 1}]}` ↔
//! `<musicFolders><musicFolder id="1"/></musicFolders>`).

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

/// La version de l'API Subsonic que le socle annonce.
pub const VERSION_API: &str = "1.16.1";
const ESPACE_DE_NOMS: &str = "http://subsonic.org/restapi";

/// Le format demandé par le client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Xml,
    Json,
}

impl Format {
    /// `f=json` donne du JSON ; toute autre valeur, ou son absence, du XML
    /// (le défaut de l'API Subsonic).
    pub fn depuis(f: Option<&str>) -> Self {
        match f {
            Some(v) if v.eq_ignore_ascii_case("json") => Format::Json,
            _ => Format::Xml,
        }
    }
}

/// Les codes d'erreur de l'API Subsonic / OpenSubsonic que le socle emploie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeErreur {
    Generique = 0,
    ParametreManquant = 10,
    /// OpenSubsonic : le mécanisme d'authentification n'est pas pris en
    /// charge (ici le jeton `md5(mot de passe + sel)` et le mot de passe).
    MecanismeNonPrisEnCharge = 42,
    /// OpenSubsonic : plusieurs mécanismes d'authentification à la fois.
    MecanismesEnConflit = 43,
    /// OpenSubsonic : clé d'API inconnue ou révoquée.
    CleInvalide = 44,
    NonAutorise = 50,
    Introuvable = 70,
}

fn entete(statut: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("status".into(), json!(statut));
    m.insert("version".into(), json!(VERSION_API));
    m.insert("type".into(), json!("tune"));
    m.insert("serverVersion".into(), json!(env!("CARGO_PKG_VERSION")));
    m.insert("openSubsonic".into(), json!(true));
    m
}

/// Une réponse réussie ; `contenu` est un objet dont chaque clé devient un
/// enfant de `subsonic-response` (vide pour `ping`).
pub fn ok(format: Format, contenu: Value) -> Response {
    let mut m = entete("ok");
    if let Value::Object(champs) = contenu {
        m.extend(champs);
    }
    rendre(format, m)
}

/// Une erreur Subsonic. Elle part en HTTP 200, comme l'exige l'API : les
/// clients lisent `status="failed"` et le code, pas le statut HTTP.
pub fn erreur(format: Format, code: CodeErreur, message: &str) -> Response {
    let mut m = entete("failed");
    m.insert(
        "error".into(),
        json!({"code": code as i32, "message": message}),
    );
    rendre(format, m)
}

fn rendre(format: Format, racine: Map<String, Value>) -> Response {
    match format {
        Format::Json => {
            let corps = json!({ "subsonic-response": Value::Object(racine) });
            (StatusCode::OK, axum::Json(corps)).into_response()
        }
        Format::Xml => {
            let mut xml = String::from(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
            let mut racine = racine;
            racine.insert("xmlns".into(), json!(ESPACE_DE_NOMS));
            ecrire_element(&mut xml, "subsonic-response", &racine);
            let mut r = (StatusCode::OK, xml).into_response();
            r.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/xml; charset=utf-8"),
            );
            r
        }
    }
}

/// Le XML d'un contenu, sans l'enveloppe : exposé pour les témoins.
pub fn en_xml(racine: &Map<String, Value>, nom: &str) -> String {
    let mut xml = String::new();
    ecrire_element(&mut xml, nom, racine);
    xml
}

fn valeur_simple(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn echapper(texte: &str, sortie: &mut String) {
    for c in texte.chars() {
        match c {
            '&' => sortie.push_str("&amp;"),
            '<' => sortie.push_str("&lt;"),
            '>' => sortie.push_str("&gt;"),
            '"' => sortie.push_str("&quot;"),
            '\'' => sortie.push_str("&apos;"),
            _ => sortie.push(c),
        }
    }
}

fn ecrire_element(sortie: &mut String, nom: &str, champs: &Map<String, Value>) {
    sortie.push('<');
    sortie.push_str(nom);
    let mut enfants = Vec::new();
    for (cle, valeur) in champs {
        match valeur_simple(valeur) {
            Some(texte) => {
                sortie.push(' ');
                sortie.push_str(cle);
                sortie.push_str("=\"");
                echapper(&texte, sortie);
                sortie.push('"');
            }
            None if !valeur.is_null() => enfants.push((cle, valeur)),
            None => {}
        }
    }
    if enfants.is_empty() {
        sortie.push_str("/>");
        return;
    }
    sortie.push('>');
    for (cle, valeur) in enfants {
        ecrire_enfant(sortie, cle, valeur);
    }
    sortie.push_str("</");
    sortie.push_str(nom);
    sortie.push('>');
}

fn ecrire_enfant(sortie: &mut String, nom: &str, valeur: &Value) {
    match valeur {
        Value::Object(champs) => ecrire_element(sortie, nom, champs),
        Value::Array(elements) => {
            for e in elements {
                ecrire_enfant(sortie, nom, e);
            }
        }
        autre => {
            if let Some(texte) = valeur_simple(autre) {
                sortie.push('<');
                sortie.push_str(nom);
                sortie.push('>');
                echapper(&texte, sortie);
                sortie.push_str("</");
                sortie.push_str(nom);
                sortie.push('>');
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_objet_devient_un_element_et_ses_tableaux_des_enfants_repetes() {
        let contenu = json!({
            "musicFolders": {"musicFolder": [{"id": 1, "name": "A & B"}]},
            "openSubsonicExtensions": [{"name": "apiKeyAuthentication", "versions": [1]}],
        });
        let Value::Object(m) = contenu else {
            unreachable!()
        };
        let xml = en_xml(&m, "r");
        assert!(
            xml.contains(r#"<musicFolders><musicFolder id="1" name="A &amp; B"/></musicFolders>"#),
            "{xml}"
        );
        assert!(
            xml.contains(r#"<openSubsonicExtensions name="apiKeyAuthentication"><versions>1</versions></openSubsonicExtensions>"#),
            "{xml}"
        );
    }

    #[test]
    fn le_format_suit_le_parametre_f() {
        assert_eq!(Format::depuis(Some("json")), Format::Json);
        assert_eq!(Format::depuis(Some("xml")), Format::Xml);
        assert_eq!(Format::depuis(None), Format::Xml);
    }
}
