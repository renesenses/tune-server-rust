//! Les routes du greffon, montées par l'hôte sous
//! `/api/v1/plugins/playlists-converter/…`.
//!
//! L'hôte emballe la requête HTTP en `{method, path, query, body}` et attend
//! une enveloppe `{status, body}` (RFC §3.5). Le greffon n'a donc pas de
//! serveur à lui : il répond à ce contrat, et c'est tout.
//!
//! Le découpage suit exactement les trois temps du chantier :
//!
//! | Route | Écrit chez l'utilisateur ? |
//! |---|---|
//! | `GET  /sources`    | non — ce qu'on peut transférer |
//! | `POST /apercu`     | **non** — le plan et son rapport |
//! | `POST /executer`   | oui, et seulement avec `confirme: true` |
//! | `GET  /transferts` | non — les lots connus |
//! | `GET  /transfert`  | non — le rapport d'un lot |

use serde_json::{Value, json};

use crate::hote::Hote;
use crate::moteur::{self, Demande};

/// Répondre à une requête de route. Ne panique jamais : une entrée illisible
/// est un 400, pas un piège wasm qui tuerait le `Store` du greffon.
pub fn repondre(hote: &dyn Hote, requete: &Value) -> Value {
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

    match (methode.as_str(), chemin) {
        ("GET", "/sources") => selon(sources(hote)),
        ("POST", "/apercu") => selon(apercu(hote, &corps)),
        ("POST", "/executer") => selon(executer(hote, &corps)),
        ("GET", "/transferts") => selon(transferts(hote)),
        ("GET", "/transfert") => selon(transfert(hote, requete_query)),
        _ => enveloppe(
            404,
            json!({ "error": "route inconnue", "method": methode, "path": chemin }),
        ),
    }
}

fn selon(resultat: Result<Value, String>) -> Value {
    match resultat {
        Ok(corps) => enveloppe(200, corps),
        Err(message) => enveloppe(400, json!({ "error": message })),
    }
}

fn enveloppe(status: u16, corps: Value) -> Value {
    json!({ "status": status, "body": corps })
}

/// Ce qu'on peut transférer, et vers quoi. Lecture seule de bout en bout.
fn sources(hote: &dyn Hote) -> Result<Value, String> {
    let locales = hote.playlists_locales(500, 0)?;
    let services = hote.services()?;
    // Les playlists de chaque service authentifié, pour que l'écran n'ait pas
    // à faire un appel par service.
    let mut par_service = Vec::new();
    if let Some(liste) = services.get("services").and_then(Value::as_array) {
        for fiche in liste {
            let Some(nom) = fiche.get("name").and_then(Value::as_str) else {
                continue;
            };
            match hote.playlists_du_service(nom) {
                Ok(rendu) => par_service.push(json!({
                    "service": nom,
                    "supports_write": fiche.get("supports_write").cloned().unwrap_or(Value::Bool(false)),
                    "playlists": rendu.get("playlists").cloned().unwrap_or(json!([])),
                })),
                // Un service qui répond mal ne doit pas faire disparaître les
                // autres : on le liste avec son erreur.
                Err(e) => par_service.push(json!({
                    "service": nom,
                    "supports_write": fiche.get("supports_write").cloned().unwrap_or(Value::Bool(false)),
                    "playlists": json!([]),
                    "erreur": e,
                })),
            }
        }
    }
    Ok(json!({
        "locales": locales.get("playlists").cloned().unwrap_or(json!([])),
        "services": par_service,
    }))
}

fn apercu(hote: &dyn Hote, corps: &Value) -> Result<Value, String> {
    let demande: Demande =
        serde_json::from_value(corps.clone()).map_err(|e| format!("demande illisible : {e}"))?;
    let plan = moteur::preparer(hote, &demande)?;
    Ok(moteur::rendre(&plan))
}

fn executer(hote: &dyn Hote, corps: &Value) -> Result<Value, String> {
    let transfert_id = corps
        .get("transfert_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "transfert_id manquant".to_string())?;
    // 🔴 L'accord explicite. Absent ou faux, rien ne part : c'est la règle du
    // chantier, « aucune écriture surprise ».
    let confirme = corps
        .get("confirme")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let plan = moteur::executer(hote, transfert_id, confirme)?;
    Ok(moteur::rendre(&plan))
}

fn transferts(hote: &dyn Hote) -> Result<Value, String> {
    let ids = moteur::lister(hote)?;
    let mut fiches = Vec::with_capacity(ids.len());
    for id in &ids {
        if let Ok(plan) = moteur::charger(hote, id) {
            fiches.push(json!({
                "transfert_id": plan.transfert_id,
                "etat": plan.etat,
                "cible": plan.cible,
                "comptes": plan.comptes(),
                "playlists": plan.blocs.len(),
            }));
        }
    }
    Ok(json!({ "count": fiches.len(), "transferts": fiches }))
}

fn transfert(hote: &dyn Hote, query: &str) -> Result<Value, String> {
    let id = parametre(query, "transfert_id").ok_or_else(|| "transfert_id manquant".to_string())?;
    let plan = moteur::charger(hote, &id)?;
    Ok(moteur::rendre(&plan))
}

/// Lire un paramètre d'une chaîne de requête `a=1&b=2`.
///
/// Pas de décodage `%xx` : les identifiants de transfert sont fabriqués ici
/// (`t1`, `t2`, …) et ne contiennent que des caractères sûrs. Décoder à moitié
/// serait pire que ne pas décoder.
fn parametre(query: &str, nom: &str) -> Option<String> {
    query.split('&').find_map(|couple| {
        let (cle, valeur) = couple.split_once('=')?;
        (cle == nom).then(|| valeur.to_string())
    })
}
