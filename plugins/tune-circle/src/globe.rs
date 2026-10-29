//! Tune Circle rc8 (#6900) : l'écran de Levente et le globe en opt-in.
//!
//! | Tune (`/api/v1/ext/circle`)              | mozaiklabs (`/api/v1/circle`)           |
//! |------------------------------------------|-----------------------------------------|
//! | `GET /contacts/{user_id}/library/summary` | idem                                   |
//! | `GET /me/location`                       | idem                                    |
//! | `PUT /me/location` `{ label, country_code?, lat, lon }` | idem                     |
//! | `DELETE /me/location`                    | idem                                    |
//! | `GET /contacts/locations`                | idem                                    |
//! | `GET /world/points` (rc9, #7122)          | idem                                    |
//! | `GET\|PUT\|DELETE /me/public-map-opt-out` (rc9, #7122) | idem                    |
//!
//! Même relais que les autres routes du cercle ([`crate::relais::Relais`]) :
//! session SSO relue à chaque appel, réponses du cloud relayées à l'octet près
//! (`200`, `204`, `403`, `404`…), cloud injoignable ou en panne → `503
//! circle.cloud_unavailable`, sans session → `412 circle.not_connected`.
//!
//! Le cloud porte le droit (règle T2 pour le résumé, contacts acceptés pour
//! les lieux) et l'arrondi. Le greffon ne garde rien.
//!
//! ## Vie privée du lieu
//!
//! * Rien ne part sans le `PUT` de l'utilisateur : aucune géolocalisation, ni
//!   du navigateur ni par l'IP, n'est faite ici.
//! * `PUT` est jugé AVANT tout appel, avec les règles du contrat figé du cloud
//!   (annexe du contrat rc8, site-mozaiklabs#347) : `lat` dans [-90, 90],
//!   `lon` dans [-180, 180], `label` de 1 à 60 caractères après retrait des
//!   caractères de contrôle et repli des espaces, `country_code` absent,
//!   `null`, `""` ou deux lettres (passées en majuscules). Un corps refusé ne
//!   part pas : `422` à la forme de validation du cloud (`message`, `errors`),
//!   plus `code: circle.invalid_body`.
//! * Les coordonnées sont arrondies à 0,5° ICI aussi, avant l'envoi : une
//!   position plus fine que ce que le contrat garde ne quitte jamais le
//!   serveur. L'arrondi du cloud, appliqué ensuite, ne change plus rien.
//! * Seuls les quatre champs du contrat partent ; tout autre champ du client
//!   est ignoré.
//! * **Aucun journal** du libellé ni des coordonnées : le relais n'écrit que
//!   le gabarit de la route, le statut et la durée.
//! * Les réponses des routes de lieu portent `Cache-Control: no-store`, comme
//!   celles du cloud (que le relais ne recopie pas).
//!
//! ## Carte publique (rc9, #7122, décision de Bertrand du 26/10/2026)
//!
//! La vue Monde montre aussi, en points anonymes, tous les comptes qui ont
//! posé leur lieu, sauf ceux qui s'en retirent. Le cloud fait l'anonymisation
//! (case de 1°, un point par case, seuil de comptes par case, aucun nom ni
//! identifiant) ; le greffon relaie `GET /world/points` tel quel et relaie
//! l'interrupteur « Me retirer de la carte publique » (`PUT` = me retirer,
//! `DELETE` = revenir, `GET` = l'état), sans corps envoyé. Mêmes états que les
//! autres routes : `412`, `503`, et `404` sur un serveur plus ancien.
//! `Cache-Control: no-store` partout.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use reqwest::Method;
use serde_json::{Map, Value, json};

use crate::relais::Relais;
use crate::routes::{en_reponse, identifiant_valide, introuvable, refus};

/// Code d'un corps de `PUT /me/location` refusé avant tout appel.
pub const CODE_CORPS_INVALIDE: &str = "circle.invalid_body";

/// Longueur maximale du libellé, en caractères (contrat rc8).
pub const LIBELLE_MAX: usize = 60;

/// Le pas de l'arrondi des coordonnées, en degrés (contrat rc8).
pub const PAS_DEGRES: f64 = 0.5;

/// Le corps qui part vers le cloud, ou le champ fautif.
pub fn corps_du_lieu(corps: &[u8]) -> Result<Value, &'static str> {
    let Ok(Value::Object(v)) = serde_json::from_slice::<Value>(corps) else {
        return Err("body");
    };
    let lat = coordonnee(&v, "lat", 90.0)?;
    let lon = coordonnee(&v, "lon", 180.0)?;
    let label = match v.get("label") {
        Some(Value::String(s)) => libelle_nettoye(s),
        _ => return Err("label"),
    };
    if label.is_empty() || label.chars().count() > LIBELLE_MAX {
        return Err("label");
    }
    let country_code = match v.get("country_code") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(c)) if c.is_empty() => Value::Null,
        Some(Value::String(c)) if c.len() == 2 && c.bytes().all(|b| b.is_ascii_alphabetic()) => {
            Value::String(c.to_ascii_uppercase())
        }
        _ => return Err("country_code"),
    };
    Ok(json!({
        "label": label,
        "country_code": country_code,
        "lat": arrondir(lat, 90.0),
        "lon": arrondir(lon, 180.0),
    }))
}

/// Le libellé tel que le cloud le garde : sans caractères de contrôle, les
/// suites d'espaces repliées en une seule, sans espaces de bord.
fn libelle_nettoye(brut: &str) -> String {
    brut.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Un nombre fini dans [-borne, borne].
fn coordonnee(
    v: &Map<String, Value>,
    champ: &'static str,
    borne: f64,
) -> Result<f64, &'static str> {
    match v.get(champ).and_then(Value::as_f64) {
        Some(x) if x.is_finite() && (-borne..=borne).contains(&x) => Ok(x),
        _ => Err(champ),
    }
}

/// Arrondi au pas de 0,5°, demi-pas loin de zéro (`f64::round` : -12.25 →
/// -12.5), sans sortir des bornes et sans `-0`.
fn arrondir(x: f64, borne: f64) -> f64 {
    let r = ((x / PAS_DEGRES).round() * PAS_DEGRES).clamp(-borne, borne);
    if r == 0.0 { 0.0 } else { r }
}

pub(crate) async fn resume_de_la_bibliotheque(
    State(relais): State<Arc<Relais>>,
    Path(user_id): Path<String>,
) -> Response {
    if !identifiant_valide(&user_id) {
        return introuvable();
    }
    en_reponse(
        relais
            .appeler(
                "GET /contacts/{user_id}/library/summary",
                Method::GET,
                &["contacts", &user_id, "library", "summary"],
                None,
            )
            .await,
    )
}

/// `Cache-Control: no-store` : un lieu ne reste dans aucun cache.
fn sans_cache(mut r: Response) -> Response {
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

pub(crate) async fn mon_lieu(State(relais): State<Arc<Relais>>) -> Response {
    sans_cache(en_reponse(
        relais
            .appeler("GET /me/location", Method::GET, &["me", "location"], None)
            .await,
    ))
}

pub(crate) async fn poser_mon_lieu(State(relais): State<Arc<Relais>>, corps: Bytes) -> Response {
    let envoi = match corps_du_lieu(&corps) {
        Ok(e) => e,
        Err(champ) => {
            return sans_cache(refus(
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({
                    "message": format!("The {champ} field is invalid."),
                    "errors": { (champ): [format!("The {champ} field is invalid.")] },
                    "code": CODE_CORPS_INVALIDE,
                }),
            ));
        }
    };
    sans_cache(en_reponse(
        relais
            .appeler(
                "PUT /me/location",
                Method::PUT,
                &["me", "location"],
                Some(&envoi),
            )
            .await,
    ))
}

pub(crate) async fn retirer_mon_lieu(State(relais): State<Arc<Relais>>) -> Response {
    sans_cache(en_reponse(
        relais
            .appeler(
                "DELETE /me/location",
                Method::DELETE,
                &["me", "location"],
                None,
            )
            .await,
    ))
}

pub(crate) async fn lieux_des_contacts(State(relais): State<Arc<Relais>>) -> Response {
    sans_cache(en_reponse(
        relais
            .appeler(
                "GET /contacts/locations",
                Method::GET,
                &["contacts", "locations"],
                None,
            )
            .await,
    ))
}

/// rc9 (#7122) : les points anonymes de la carte publique, `[{lat, lon}]`.
pub(crate) async fn points_du_monde(State(relais): State<Arc<Relais>>) -> Response {
    sans_cache(en_reponse(
        relais
            .appeler("GET /world/points", Method::GET, &["world", "points"], None)
            .await,
    ))
}

/// rc9 (#7122) : l'interrupteur « Me retirer de la carte publique ».
async fn retrait_de_la_carte(relais: &Relais, route: &'static str, methode: Method) -> Response {
    sans_cache(en_reponse(
        relais
            .appeler(route, methode, &["me", "public-map-opt-out"], None)
            .await,
    ))
}

pub(crate) async fn mon_retrait_de_la_carte(State(relais): State<Arc<Relais>>) -> Response {
    retrait_de_la_carte(&relais, "GET /me/public-map-opt-out", Method::GET).await
}

pub(crate) async fn me_retirer_de_la_carte(State(relais): State<Arc<Relais>>) -> Response {
    retrait_de_la_carte(&relais, "PUT /me/public-map-opt-out", Method::PUT).await
}

pub(crate) async fn revenir_sur_la_carte(State(relais): State<Arc<Relais>>) -> Response {
    retrait_de_la_carte(&relais, "DELETE /me/public-map-opt-out", Method::DELETE).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn juge(v: Value) -> Result<Value, &'static str> {
        corps_du_lieu(v.to_string().as_bytes())
    }

    #[test]
    fn les_bornes_et_le_libelle_sont_juges() {
        let ok = json!({ "label": "Talant", "lat": 47.33, "lon": 4.99 });
        assert_eq!(
            juge(ok).unwrap(),
            json!({ "label": "Talant", "country_code": null, "lat": 47.5, "lon": 5.0 })
        );
        for (corps, champ) in [
            (json!({ "label": "x", "lat": 90.01, "lon": 0 }), "lat"),
            (json!({ "label": "x", "lat": -90.01, "lon": 0 }), "lat"),
            (json!({ "label": "x", "lat": "47", "lon": 0 }), "lat"),
            (json!({ "label": "x", "lon": 0 }), "lat"),
            (json!({ "label": "x", "lat": 0, "lon": 180.5 }), "lon"),
            (json!({ "label": "x", "lat": 0, "lon": -181 }), "lon"),
            (json!({ "label": "x", "lat": 0 }), "lon"),
            (json!({ "lat": 0, "lon": 0 }), "label"),
            (json!({ "label": "   ", "lat": 0, "lon": 0 }), "label"),
            (
                json!({ "label": "é".repeat(61), "lat": 0, "lon": 0 }),
                "label",
            ),
            (json!({ "label": 3, "lat": 0, "lon": 0 }), "label"),
            (
                json!({ "label": "x", "lat": 0, "lon": 0, "country_code": "FRA" }),
                "country_code",
            ),
            (
                json!({ "label": "x", "lat": 0, "lon": 0, "country_code": "F1" }),
                "country_code",
            ),
            (json!([1, 2]), "body"),
        ] {
            assert_eq!(juge(corps.clone()), Err(champ), "{corps}");
        }
        assert_eq!(corps_du_lieu(b"pas du json"), Err("body"));
        // Contrôles et espaces repliés avant la borne, comme le cloud.
        let v = juge(json!({ "label": format!("Saint{}Apollinaire\u{7}", " ".repeat(80)), "lat": 0, "lon": 0, "country_code": "" })).unwrap();
        assert_eq!(v["label"], json!("Saint Apollinaire"));
        assert_eq!(v["country_code"], Value::Null, "\"\" vaut null");
        assert_eq!(
            juge(json!({ "label": "\u{1}\t ", "lat": 0, "lon": 0 })),
            Err("label")
        );
        // Demi-pas loin de zéro (annexe : -12.25 → -12.5).
        let v = juge(json!({ "label": "x", "lat": -12.25, "lon": 12.25 })).unwrap();
        assert_eq!(
            (v["lat"].clone(), v["lon"].clone()),
            (json!(-12.5), json!(12.5))
        );
        // 60 caractères multioctets passent : la borne est en caractères.
        assert!(juge(json!({ "label": "é".repeat(60), "lat": 90, "lon": -180 })).is_ok());
    }

    #[test]
    fn seuls_les_champs_du_contrat_partent_arrondis() {
        let v = juge(json!({
            "label": "  Dijon ",
            "country_code": "fr",
            "lat": -0.2,
            "lon": 179.9,
            "address": "12 rue X",
            "accuracy": 3,
        }))
        .unwrap();
        assert_eq!(
            v,
            json!({ "label": "Dijon", "country_code": "FR", "lat": 0.0, "lon": 180.0 })
        );
        assert_eq!(
            v["lat"].as_f64().unwrap().to_bits(),
            0.0f64.to_bits(),
            "pas de -0"
        );
    }
}
