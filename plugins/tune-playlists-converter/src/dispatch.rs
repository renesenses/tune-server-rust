//! Le routage : `{method, path, query, body}` → `{status, body}` (RFC §3.5).
//!
//! L'hôte monte un seul gestionnaire sous `/api/v1/plugins/playlists-converter/…`
//! et lui passe la requête en JSON. La garde premium est posée par l'hôte
//! AVANT d'arriver ici (`manifest.premium = true`) : le greffon ne vérifie
//! aucune licence, c'est le serveur qui possède ce sujet.

use serde_json::{Value, json};

use crate::hote::Hote;
use crate::modele::Demande;
use crate::moteur::Convertisseur;

/// Router une requête. Ne rend jamais d'`Err` : une erreur est une réponse
/// HTTP, pas un trap — un greffon qui trappe est marqué en erreur par l'hôte
/// et cesse de répondre du tout.
pub fn repondre<H: Hote + ?Sized>(hote: &H, requete: &Value) -> Value {
    let methode = requete
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_uppercase();
    let chemin = requete
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("/")
        .trim_end_matches('/')
        .to_string();
    let chemin = if chemin.is_empty() { "/" } else { &chemin };
    let corps = requete.get("body").cloned().unwrap_or(Value::Null);
    let requete_query = requete.get("query").and_then(Value::as_str).unwrap_or("");

    let moteur = Convertisseur::new(hote);

    match (methode.as_str(), chemin) {
        ("POST", "/apercu") => match serde_json::from_value::<Demande>(corps) {
            Ok(d) => match moteur.apercu(&d) {
                Ok(lot) => reponse(200, json!({ "resume": lot.resume(), "lot": lot })),
                Err(e) => erreur(&e),
            },
            Err(e) => reponse(400, json!({ "error": format!("demande illisible : {e}") })),
        },

        ("POST", "/transfert") => {
            let lot_id = texte(&corps, "lot_id");
            let accord = corps
                .get("accord")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if lot_id.is_empty() {
                return reponse(400, json!({ "error": "lot_id manquant" }));
            }
            match moteur.transferer(&lot_id, accord) {
                Ok(lot) => reponse(200, json!({ "resume": lot.resume(), "lot": lot })),
                Err(e) => erreur(&e),
            }
        }

        ("POST", "/reprise") => {
            let lot_id = texte(&corps, "lot_id");
            if lot_id.is_empty() {
                return reponse(400, json!({ "error": "lot_id manquant" }));
            }
            match moteur.reprendre(&lot_id) {
                Ok(lot) => reponse(200, json!({ "resume": lot.resume(), "lot": lot })),
                Err(e) => erreur(&e),
            }
        }

        ("GET", "/lots") => match moteur.lots() {
            Ok(lots) => reponse(200, json!({ "count": lots.len(), "lots": lots })),
            Err(e) => erreur(&e),
        },

        ("GET", "/lot") => {
            let lot_id = parametre(requete_query, "id");
            if lot_id.is_empty() {
                return reponse(400, json!({ "error": "paramètre ?id= manquant" }));
            }
            match moteur.lire_le_lot(&lot_id) {
                Ok(lot) => reponse(200, json!({ "resume": lot.resume(), "lot": lot })),
                Err(e) => erreur(&e),
            }
        }

        _ => reponse(
            404,
            json!({ "error": "route inconnue", "method": methode, "path": chemin }),
        ),
    }
}

/// Le code HTTP porte le sens : un refus faute d'accord n'est pas une panne de
/// service, et l'écran doit pouvoir les distinguer sans lire le texte.
fn erreur(message: &str) -> Value {
    let code = if message.starts_with("accord_requis") {
        409
    } else if message.starts_with("lot_inconnu") {
        404
    } else if message.starts_with("lot_deja_engage")
        || message.starts_with("cible_locale_non_supportee")
    {
        409
    } else {
        502
    };
    reponse(code, json!({ "error": message }))
}

fn reponse(status: u16, body: Value) -> Value {
    json!({ "status": status, "body": body })
}

fn texte(v: &Value, cle: &str) -> String {
    v.get(cle)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Lire un paramètre de la chaîne de requête. Pas de décodage d'échappement :
/// les identifiants de lot sont `lot-<n>`, et un identifiant qui aurait besoin
/// d'être échappé n'est pas un identifiant de lot.
fn parametre(query: &str, nom: &str) -> String {
    query
        .split('&')
        .filter_map(|p| p.split_once('='))
        .find(|(k, _)| *k == nom)
        .map(|(_, v)| v.to_string())
        .unwrap_or_default()
}
