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
use crate::snapshots::{RETENTION_PAR_PLAYLIST, Snapshots, mode};

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

        // -- #4718 — snapshots ------------------------------------------------
        ("POST", "/snapshot") => {
            let service = texte(&corps, "service");
            let playlist_id = texte(&corps, "playlist_id");
            if service.is_empty() || playlist_id.is_empty() {
                return reponse(
                    400,
                    json!({ "error": "service et playlist_id sont obligatoires" }),
                );
            }
            let nom = corps.get("nom").and_then(Value::as_str);
            match Snapshots::new(hote).prendre(&service, &playlist_id, nom, "manuel") {
                Ok(e) => reponse(200, json!({ "snapshot": e })),
                Err(e) => erreur(&e),
            }
        }

        ("GET", "/snapshots") => {
            let snapshots = Snapshots::new(hote);
            let service = parametre(requete_query, "service");
            let playlist_id = parametre(requete_query, "playlist_id");
            if service.is_empty() && playlist_id.is_empty() {
                return match snapshots.playlists() {
                    Ok(p) => reponse(
                        200,
                        json!({
                            "count": p.len(),
                            "playlists": p,
                            "retention_par_playlist": RETENTION_PAR_PLAYLIST,
                        }),
                    ),
                    Err(e) => erreur(&e),
                };
            }
            if service.is_empty() || playlist_id.is_empty() {
                return reponse(
                    400,
                    json!({ "error": "?service= et ?playlist_id= vont ensemble" }),
                );
            }
            match snapshots.lister(&service, &playlist_id) {
                Ok(l) => reponse(
                    200,
                    json!({
                        "count": l.len(),
                        "snapshots": l,
                        "retention_par_playlist": RETENTION_PAR_PLAYLIST,
                    }),
                ),
                Err(e) => erreur(&e),
            }
        }

        ("GET", "/snapshot") => {
            let id = parametre(requete_query, "id");
            if id.is_empty() {
                return reponse(400, json!({ "error": "paramètre ?id= manquant" }));
            }
            match Snapshots::new(hote).lire(&id) {
                Ok(s) => reponse(200, json!({ "snapshot": s })),
                Err(e) => erreur(&e),
            }
        }

        ("POST", "/snapshot/restauration/apercu") => {
            let snapshot_id = texte(&corps, "snapshot_id");
            if snapshot_id.is_empty() {
                return reponse(400, json!({ "error": "snapshot_id manquant" }));
            }
            let mode = corps
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or(mode::COMPLETER);
            match Snapshots::new(hote).apercu_restauration(&snapshot_id, mode) {
                Ok((plan, a_rajouter, a_retirer)) => reponse(
                    200,
                    json!({
                        "plan": plan,
                        "a_rajouter": a_rajouter,
                        "a_retirer_par_vous": a_retirer,
                    }),
                ),
                Err(e) => erreur(&e),
            }
        }

        ("POST", "/snapshot/restauration") => {
            let plan_id = texte(&corps, "plan_id");
            if plan_id.is_empty() {
                return reponse(400, json!({ "error": "plan_id manquant" }));
            }
            let accord = corps
                .get("accord")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            match Snapshots::new(hote).restaurer(&plan_id, accord) {
                Ok((plan, a_retirer)) => reponse(
                    200,
                    json!({ "plan": plan, "a_retirer_par_vous": a_retirer }),
                ),
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
    } else if message.starts_with("lot_inconnu")
        || message.starts_with("snapshot_inconnu")
        || message.starts_with("snapshot_expire")
        || message.starts_with("plan_inconnu")
    {
        404
    } else if message.starts_with("lot_deja_engage")
        || message.starts_with("plan_deja_engage")
        || message.starts_with("cible_locale_non_supportee")
    {
        409
    } else if message.starts_with("demande_invalide") {
        400
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

/// Lire un paramètre de la chaîne de requête, décodé (`%XX` et `+`).
///
/// Les identifiants de lot (`lot-<n>`) n'en ont pas besoin, mais un
/// identifiant de playlist de service (#4718) peut porter n'importe quoi : il
/// arrive encodé par `encodeURIComponent` côté client.
fn parametre(query: &str, nom: &str) -> String {
    query
        .split('&')
        .filter_map(|p| p.split_once('='))
        .find(|(k, _)| *k == nom)
        .map(|(_, v)| decoder(v))
        .unwrap_or_default()
}

fn decoder(v: &str) -> String {
    let octets = v.as_bytes();
    let mut sortie = Vec::with_capacity(octets.len());
    let mut i = 0;
    while i < octets.len() {
        match octets[i] {
            b'+' => sortie.push(b' '),
            b'%' if i + 2 < octets.len() => match (hexa(octets[i + 1]), hexa(octets[i + 2])) {
                (Some(h), Some(l)) => {
                    sortie.push(h * 16 + l);
                    i += 2;
                }
                _ => sortie.push(b'%'),
            },
            b => sortie.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&sortie).into_owned()
}

fn hexa(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}
