//! Tune Circle rc9 (#7130, décision de Bertrand du 26/10/2026) : les
//! notifications de Tune web, relayées depuis le cloud.
//!
//! | Tune (`/api/v1/ext/circle`)               | mozaiklabs (`/api/v1/circle`) |
//! |-------------------------------------------|-------------------------------|
//! | `GET /notifications`                      | idem                          |
//! | `POST /notifications/{id}/read`           | idem, sans corps              |
//! | `POST /notifications/read-all`            | idem, sans corps              |
//! | `GET /notifications/preferences`          | idem                          |
//! | `PUT /notifications/preferences` `{ playlist_shared?, circle_invitation?, message?, contact? }` | idem |
//!
//! Le cloud fait naître les notifications (playlist partagée dans un cercle,
//! rangement dans un cercle, message, demande de contact reçue ou acceptée) et
//! porte la règle (recalculée à chaque lecture, 404 pour ce qui n'est pas à
//! l'appelant). Le greffon relaie, et ne garde rien : ni copie, ni cache.
//!
//! * Mêmes états que les autres routes Circle : `412 circle.not_connected`,
//!   `503 circle.cloud_unavailable`, et `404` sur un serveur plus ancien.
//! * `Cache-Control: no-store` partout.
//! * Rien ne part qui ne soit nécessaire : aucun corps pour lire ou tout
//!   marquer lu ; pour les réglages, seules les quatre clés du contrat, en
//!   booléens. Un autre corps est refusé AVANT tout appel (`422`, forme de
//!   validation du cloud plus `code: circle.invalid_body`).
//! * Journal : le gabarit de la route, le statut, la durée. Jamais un nom.
//! * Pas de temps réel : le cloud n'a aucun canal poussé vers les serveurs
//!   Tune ; le client interroge `GET /notifications` à intervalle espacé.

use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use reqwest::Method;
use serde_json::{Map, Value, json};

use crate::globe::CODE_CORPS_INVALIDE;
use crate::relais::Relais;
use crate::routes::{en_reponse, introuvable, refus};

/// Les interrupteurs du contrat, un par type de notification.
pub const REGLAGES: [&str; 4] = ["playlist_shared", "circle_invitation", "message", "contact"];

pub fn router(relais: Arc<Relais>) -> Router<()> {
    Router::new()
        .route("/notifications", get(liste))
        .route("/notifications/read-all", post(tout_lu))
        .route("/notifications/{id}/read", post(lue))
        .route("/notifications/preferences", get(reglages).put(regler))
        .with_state(relais)
}

/// Le corps de `PUT /notifications/preferences` qui part vers le cloud, ou le
/// champ fautif.
pub fn corps_des_reglages(corps: &[u8]) -> Result<Value, String> {
    let Ok(Value::Object(v)) = serde_json::from_slice::<Value>(corps) else {
        return Err("preferences".into());
    };
    if v.is_empty() {
        return Err("preferences".into());
    }
    let mut envoi = Map::new();
    for (cle, valeur) in v {
        if !REGLAGES.contains(&cle.as_str()) {
            return Err(cle);
        }
        let Value::Bool(b) = valeur else {
            return Err(cle);
        };
        envoi.insert(cle, Value::Bool(b));
    }
    Ok(Value::Object(envoi))
}

/// Un identifiant de notification : des chiffres, 18 au plus (comme le cloud).
fn identifiant_de_notification(id: &str) -> bool {
    !id.is_empty() && id.len() <= 18 && id.bytes().all(|b| b.is_ascii_digit())
}

fn sans_cache(mut r: Response) -> Response {
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

async fn liste(State(relais): State<Arc<Relais>>) -> Response {
    sans_cache(en_reponse(
        relais
            .appeler("GET /notifications", Method::GET, &["notifications"], None)
            .await,
    ))
}

async fn lue(State(relais): State<Arc<Relais>>, Path(id): Path<String>) -> Response {
    if !identifiant_de_notification(&id) {
        return sans_cache(introuvable());
    }
    sans_cache(en_reponse(
        relais
            .appeler(
                "POST /notifications/{id}/read",
                Method::POST,
                &["notifications", &id, "read"],
                None,
            )
            .await,
    ))
}

async fn tout_lu(State(relais): State<Arc<Relais>>) -> Response {
    sans_cache(en_reponse(
        relais
            .appeler(
                "POST /notifications/read-all",
                Method::POST,
                &["notifications", "read-all"],
                None,
            )
            .await,
    ))
}

async fn reglages(State(relais): State<Arc<Relais>>) -> Response {
    sans_cache(en_reponse(
        relais
            .appeler(
                "GET /notifications/preferences",
                Method::GET,
                &["notifications", "preferences"],
                None,
            )
            .await,
    ))
}

async fn regler(State(relais): State<Arc<Relais>>, corps: Bytes) -> Response {
    let envoi = match corps_des_reglages(&corps) {
        Ok(e) => e,
        Err(champ) => {
            let message = format!("The {champ} field is invalid.");
            return sans_cache(refus(
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({
                    "message": message,
                    "errors": { (champ): [message] },
                    "code": CODE_CORPS_INVALIDE,
                }),
            ));
        }
    };
    sans_cache(en_reponse(
        relais
            .appeler(
                "PUT /notifications/preferences",
                Method::PUT,
                &["notifications", "preferences"],
                Some(&envoi),
            )
            .await,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn juge(v: Value) -> Result<Value, String> {
        corps_des_reglages(v.to_string().as_bytes())
    }

    #[test]
    fn seules_les_quatre_cles_booleennes_partent() {
        assert_eq!(
            juge(json!({ "message": false, "contact": true })).unwrap(),
            json!({ "message": false, "contact": true })
        );
        for (corps, champ) in [
            (json!({}), "preferences"),
            (json!([true]), "preferences"),
            (json!({ "message": "false" }), "message"),
            (json!({ "message": 0 }), "message"),
            (json!({ "message": null }), "message"),
            (json!({ "push": true }), "push"),
            (json!({ "contact": true, "email": "a@b.c" }), "email"),
        ] {
            assert_eq!(juge(corps.clone()), Err(champ.to_string()), "{corps}");
        }
        assert_eq!(
            corps_des_reglages(b"pas du json"),
            Err("preferences".into())
        );
    }

    #[test]
    fn un_identifiant_n_est_fait_que_de_chiffres() {
        assert!(identifiant_de_notification("17"));
        for id in ["", "abc", "1a", "-1", "..", &"9".repeat(19)] {
            assert!(!identifiant_de_notification(id), "{id}");
        }
    }
}
