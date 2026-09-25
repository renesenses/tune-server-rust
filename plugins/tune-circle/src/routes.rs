//! Les routes du greffon, montées par l'hôte sous `/api/v1/ext/circle`.
//!
//! Même forme que le contrat mozaiklabs (#5018) :
//!
//! | Tune (`/api/v1/ext/circle`)           | mozaiklabs (`/api/v1/circle`)          |
//! |---------------------------------------|----------------------------------------|
//! | `GET /`                               | `GET /`                                |
//! | `POST /invitations` `{ "email" }`     | `POST /invitations`                    |
//! | `POST /invitations/{id}/accept`       | `POST /invitations/{id}/accept`        |
//! | `POST /invitations/{id}/decline`      | `POST /invitations/{id}/decline`       |
//! | `DELETE /invitations/{id}`            | `DELETE /invitations/{id}`             |
//! | `DELETE /members/{user_id}`           | `DELETE /members/{user_id}`            |
//!
//! Avenant « plusieurs cercles » (#5018, 25/09) : les `members` sont les
//! CONTACTS, et les cercles leurs classements privés par le propriétaire.
//! Purement additif ; `GET /` y gagne la clé `circles`, relayée telle quelle.
//!
//! | Tune (`/api/v1/ext/circle`)                  | mozaiklabs (`/api/v1/circle`)          |
//! |----------------------------------------------|----------------------------------------|
//! | `POST /invitations` `{ "email", "circle_id"? }` | `POST /invitations`                 |
//! | `POST /circles` `{ "name" }`                 | `POST /circles`                        |
//! | `PATCH /circles/{id}` `{ "name" }`           | `PATCH /circles/{id}`                  |
//! | `DELETE /circles/{id}`                       | `DELETE /circles/{id}`                 |
//! | `PUT /circles/{id}/members/{user_id}`        | `PUT /circles/{id}/members/{user_id}`  |
//! | `DELETE /circles/{id}/members/{user_id}`     | `DELETE /circles/{id}/members/{user_id}` |
//!
//! Le nom (1 à 60 caractères, unique sans casse : 409 `circle_name_taken`),
//! le plafond (422 `too_many_circles`) et la propriété du cercle (404) sont
//! jugés par le cloud seul, et relayés avec leur corps.
//!
//! ## Les états
//!
//! * **Réponse du cloud** (2xx, 4xx — dont 401, 404, 429) : statut et corps
//!   relayés à l'octet près, `Retry-After` compris.
//! * **Non connecté** (aucune session SSO) : `GET /` rend `200 { "connected":
//!   false }` ; toute autre route rend `412 { "connected": false, "code":
//!   "circle.not_connected" }`. Aucun appel ne part.
//! * **Cloud indisponible** (injoignable, délai, 5xx) : `503 { "connected":
//!   true, "code": "circle.cloud_unavailable", "upstream_status": 500|null }`.
//! * Refus local, sans appel : `404 circle.not_found` (identifiant vide, `.`
//!   ou `..`). La validité de l'adresse, elle, est jugée par le cloud seul
//!   (`422 self_invitation`, adresse invalide ; `409 already_member`,
//!   `already_invited`, `invitation_received`) et relayée avec son corps.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use reqwest::Method;
use serde_json::{Value, json};

use crate::relais::{Issue, Relais};

pub const CODE_NON_CONNECTE: &str = "circle.not_connected";
pub const CODE_CLOUD_INDISPONIBLE: &str = "circle.cloud_unavailable";
pub const CODE_INTROUVABLE: &str = "circle.not_found";
pub const CODE_REPONSE_ILLISIBLE: &str = "circle.unreadable_response";

pub fn router(relais: Arc<Relais>) -> Router<()> {
    Router::new()
        .route("/", get(cercle))
        .route("/invitations", post(inviter))
        .route("/invitations/{id}", delete(retirer))
        .route("/invitations/{id}/accept", post(accepter))
        .route("/invitations/{id}/decline", post(refuser))
        .route("/members/{user_id}", delete(revoquer))
        .route("/circles", post(creer_cercle))
        .route(
            "/circles/{id}",
            delete(supprimer_cercle).patch(renommer_cercle),
        )
        .route(
            "/circles/{id}/members/{user_id}",
            put(ranger).delete(deranger),
        )
        .with_state(relais)
}

fn refus(statut: StatusCode, corps: Value) -> Response {
    (statut, Json(corps)).into_response()
}

/// La forme HTTP d'une [`Issue`].
fn en_reponse(issue: Issue) -> Response {
    match issue {
        Issue::NonConnecte => refus(
            StatusCode::PRECONDITION_FAILED,
            json!({ "connected": false, "code": CODE_NON_CONNECTE }),
        ),
        Issue::Indisponible { statut_amont } => refus(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({
                "connected": true,
                "code": CODE_CLOUD_INDISPONIBLE,
                "upstream_status": statut_amont,
            }),
        ),
        Issue::Reponse {
            statut,
            corps,
            retry_after,
        } => {
            let statut = StatusCode::from_u16(statut).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut reponse = if corps.is_empty() {
                statut.into_response()
            } else if serde_json::from_slice::<Value>(&corps).is_ok() {
                // Les octets du cloud, pas une resérialisation : le relais est
                // fidèle jusqu'à l'ordre des clés.
                (
                    statut,
                    [(header::CONTENT_TYPE, "application/json")],
                    Body::from(corps),
                )
                    .into_response()
            } else {
                // Une page HTML d'erreur n'a rien à faire dans un client JSON :
                // le statut est gardé, le corps devient un motif nommé.
                refus(statut, json!({ "code": CODE_REPONSE_ILLISIBLE }))
            };
            if let Some(v) = retry_after {
                reponse.headers_mut().insert(header::RETRY_AFTER, v);
            }
            reponse
        }
    }
}

/// Un identifiant de chemin qui désigne bien UNE ressource.
fn identifiant_valide(id: &str) -> bool {
    !id.is_empty() && id != "." && id != ".." && id.len() <= 200
}

fn introuvable() -> Response {
    refus(StatusCode::NOT_FOUND, json!({ "code": CODE_INTROUVABLE }))
}

async fn cercle(State(relais): State<Arc<Relais>>) -> Response {
    match relais.appeler("GET /", Method::GET, &[], None).await {
        // Seule route où « non connecté » est un état et non un refus : c'est
        // elle que l'écran interroge pour savoir quoi afficher.
        Issue::NonConnecte => Json(json!({ "connected": false })).into_response(),
        issue => en_reponse(issue),
    }
}

async fn inviter(State(relais): State<Arc<Relais>>, corps: Bytes) -> Response {
    // Seul `email` part, tel que le client l'a donné : le contrat n'en demande
    // pas plus, et c'est le cloud qui juge l'adresse (422, 409). Un second
    // juge ici, plus lâche ou plus strict, ferait diverger les deux motifs.
    let email = serde_json::from_slice::<Value>(&corps)
        .ok()
        .and_then(|v| v.get("email").cloned())
        .unwrap_or(Value::Null);
    let mut envoi = json!({ "email": email });
    // Avenant « plusieurs cercles » : `circle_id`, facultatif, part tel que le
    // client l'a donné, et seulement s'il l'a donné. Le cloud juge s'il désigne
    // un cercle de l'appelant.
    if let Some(circle_id) = serde_json::from_slice::<Value>(&corps)
        .ok()
        .and_then(|v| v.get("circle_id").cloned())
    {
        envoi["circle_id"] = circle_id;
    }
    en_reponse(
        relais
            .appeler(
                "POST /invitations",
                Method::POST,
                &["invitations"],
                Some(&envoi),
            )
            .await,
    )
}

async fn accepter(State(relais): State<Arc<Relais>>, Path(id): Path<String>) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    en_reponse(
        relais
            .appeler(
                "POST /invitations/{id}/accept",
                Method::POST,
                &["invitations", &id, "accept"],
                None,
            )
            .await,
    )
}

async fn refuser(State(relais): State<Arc<Relais>>, Path(id): Path<String>) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    en_reponse(
        relais
            .appeler(
                "POST /invitations/{id}/decline",
                Method::POST,
                &["invitations", &id, "decline"],
                None,
            )
            .await,
    )
}

async fn retirer(State(relais): State<Arc<Relais>>, Path(id): Path<String>) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    en_reponse(
        relais
            .appeler(
                "DELETE /invitations/{id}",
                Method::DELETE,
                &["invitations", &id],
                None,
            )
            .await,
    )
}

async fn revoquer(State(relais): State<Arc<Relais>>, Path(user_id): Path<String>) -> Response {
    if !identifiant_valide(&user_id) {
        return introuvable();
    }
    en_reponse(
        relais
            .appeler(
                "DELETE /members/{user_id}",
                Method::DELETE,
                &["members", &user_id],
                None,
            )
            .await,
    )
}

// Avenant « plusieurs cercles » ---------------------------------------------

/// `{ "name" }` et rien d'autre, `null` s'il manque : le cloud juge le nom.
fn corps_du_nom(corps: &Bytes) -> Value {
    let name = serde_json::from_slice::<Value>(corps)
        .ok()
        .and_then(|v| v.get("name").cloned())
        .unwrap_or(Value::Null);
    json!({ "name": name })
}

async fn creer_cercle(State(relais): State<Arc<Relais>>, corps: Bytes) -> Response {
    let envoi = corps_du_nom(&corps);
    en_reponse(
        relais
            .appeler("POST /circles", Method::POST, &["circles"], Some(&envoi))
            .await,
    )
}

async fn renommer_cercle(
    State(relais): State<Arc<Relais>>,
    Path(id): Path<String>,
    corps: Bytes,
) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    let envoi = corps_du_nom(&corps);
    en_reponse(
        relais
            .appeler(
                "PATCH /circles/{id}",
                Method::PATCH,
                &["circles", &id],
                Some(&envoi),
            )
            .await,
    )
}

async fn supprimer_cercle(State(relais): State<Arc<Relais>>, Path(id): Path<String>) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    en_reponse(
        relais
            .appeler(
                "DELETE /circles/{id}",
                Method::DELETE,
                &["circles", &id],
                None,
            )
            .await,
    )
}

async fn ranger(
    State(relais): State<Arc<Relais>>,
    Path((id, user_id)): Path<(String, String)>,
) -> Response {
    if !identifiant_valide(&id) || !identifiant_valide(&user_id) {
        return introuvable();
    }
    en_reponse(
        relais
            .appeler(
                "PUT /circles/{id}/members/{user_id}",
                Method::PUT,
                &["circles", &id, "members", &user_id],
                None,
            )
            .await,
    )
}

async fn deranger(
    State(relais): State<Arc<Relais>>,
    Path((id, user_id)): Path<(String, String)>,
) -> Response {
    if !identifiant_valide(&id) || !identifiant_valide(&user_id) {
        return introuvable();
    }
    en_reponse(
        relais
            .appeler(
                "DELETE /circles/{id}/members/{user_id}",
                Method::DELETE,
                &["circles", &id, "members", &user_id],
                None,
            )
            .await,
    )
}
