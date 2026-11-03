//! Tune Circle (#6935) : inviter sans e-mail — lien d'invitation à usage
//! unique, invitation par pseudo exact du forum, blocage d'un invitant.
//!
//! | Tune (`/api/v1/ext/circle`)                       | mozaiklabs (`/api/v1/circle`)     |
//! |---------------------------------------------------|-----------------------------------|
//! | `POST /invitations/by-pseudo` `{ pseudo, circle_id? }` | idem                         |
//! | `GET /invitation-links`                           | idem                              |
//! | `POST /invitation-links` `{ circle_id? }`         | idem                              |
//! | `DELETE /invitation-links/{id}`                   | idem                              |
//! | `POST /invitations/{id}/block`                    | idem                              |
//! | `GET /blocks`                                     | idem                              |
//! | `DELETE /blocks/{user_id}`                        | idem                              |
//!
//! Même relais que les autres routes du cercle ([`crate::relais::Relais`]) :
//! réponses du cloud relayées à l'octet près (`201`, `404`, `409
//! too_many_links`, `422`, `429` avec `Retry-After`…), cloud injoignable ou en
//! panne → `503 circle.cloud_unavailable`, sans session → `412
//! circle.not_connected`. Le greffon ne garde rien.
//!
//! * Seuls les champs du contrat partent : `pseudo` tel que le client l'a
//!   donné (`null` s'il manque — le cloud juge, 422), `circle_id` s'il est
//!   donné.
//! * Le pseudo n'entre jamais au journal : le relais n'écrit que le gabarit
//!   de la route, le statut et la durée.
//! * La création d'un lien rend son URL — donc son jeton — une seule fois :
//!   les réponses des routes de liens portent `Cache-Control: no-store`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, header};
use axum::response::Response;
use reqwest::Method;
use serde_json::{Value, json};

use crate::relais::Relais;
use crate::routes::{en_reponse, identifiant_valide, introuvable};

/// Le champ `circle_id` du client, s'il l'a donné, ajouté à `envoi`.
fn avec_cercle(mut envoi: Value, corps: &Bytes) -> Value {
    if let Some(circle_id) = serde_json::from_slice::<Value>(corps)
        .ok()
        .and_then(|v| v.get("circle_id").cloned())
    {
        envoi["circle_id"] = circle_id;
    }
    envoi
}

/// Le corps qui part pour une invitation par pseudo.
pub fn corps_du_pseudo(corps: &[u8]) -> Value {
    let corps = Bytes::copy_from_slice(corps);
    let pseudo = serde_json::from_slice::<Value>(&corps)
        .ok()
        .and_then(|v| v.get("pseudo").cloned())
        .unwrap_or(Value::Null);
    avec_cercle(json!({ "pseudo": pseudo }), &corps)
}

fn sans_cache(mut r: Response) -> Response {
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

pub async fn inviter_par_pseudo(State(relais): State<Arc<Relais>>, corps: Bytes) -> Response {
    let envoi = corps_du_pseudo(&corps);
    en_reponse(
        relais
            .appeler(
                "POST /invitations/by-pseudo",
                Method::POST,
                &["invitations", "by-pseudo"],
                Some(&envoi),
            )
            .await,
    )
}

pub async fn liens(State(relais): State<Arc<Relais>>) -> Response {
    sans_cache(en_reponse(
        relais
            .appeler(
                "GET /invitation-links",
                Method::GET,
                &["invitation-links"],
                None,
            )
            .await,
    ))
}

pub async fn creer_lien(State(relais): State<Arc<Relais>>, corps: Bytes) -> Response {
    let envoi = avec_cercle(json!({}), &corps);
    sans_cache(en_reponse(
        relais
            .appeler(
                "POST /invitation-links",
                Method::POST,
                &["invitation-links"],
                Some(&envoi),
            )
            .await,
    ))
}

pub async fn revoquer_lien(State(relais): State<Arc<Relais>>, Path(id): Path<String>) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    en_reponse(
        relais
            .appeler(
                "DELETE /invitation-links/{id}",
                Method::DELETE,
                &["invitation-links", &id],
                None,
            )
            .await,
    )
}

pub async fn bloquer(State(relais): State<Arc<Relais>>, Path(id): Path<String>) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    en_reponse(
        relais
            .appeler(
                "POST /invitations/{id}/block",
                Method::POST,
                &["invitations", &id, "block"],
                None,
            )
            .await,
    )
}

pub async fn bloques(State(relais): State<Arc<Relais>>) -> Response {
    en_reponse(
        relais
            .appeler("GET /blocks", Method::GET, &["blocks"], None)
            .await,
    )
}

pub async fn debloquer(State(relais): State<Arc<Relais>>, Path(user_id): Path<String>) -> Response {
    if !identifiant_valide(&user_id) {
        return introuvable();
    }
    en_reponse(
        relais
            .appeler(
                "DELETE /blocks/{user_id}",
                Method::DELETE,
                &["blocks", &user_id],
                None,
            )
            .await,
    )
}
