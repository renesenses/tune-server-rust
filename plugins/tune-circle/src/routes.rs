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
//! ## Les états
//!
//! * **Réponse du cloud** (2xx, 4xx — dont 401, 404, 429) : statut et corps
//!   relayés à l'octet près, `Retry-After` compris.
//! * **Non connecté** (aucune session SSO) : `GET /` rend `200 { "connected":
//!   false }` ; toute autre route rend `412 { "connected": false, "code":
//!   "circle.not_connected" }`. Aucun appel ne part.
//! * **Cloud indisponible** (injoignable, délai, 5xx) : `503 { "connected":
//!   true, "code": "circle.cloud_unavailable", "upstream_status": 500|null }`.
//! * Refus locaux, sans appel : `422 circle.email_required` (corps sans
//!   `email`), `404 circle.not_found` (identifiant vide, `.` ou `..`).

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use reqwest::Method;
use serde_json::{Value, json};

use crate::relais::{Issue, Relais};

pub const CODE_NON_CONNECTE: &str = "circle.not_connected";
pub const CODE_CLOUD_INDISPONIBLE: &str = "circle.cloud_unavailable";
pub const CODE_COURRIEL_REQUIS: &str = "circle.email_required";
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
    let email = serde_json::from_slice::<Value>(&corps)
        .ok()
        .and_then(|v| {
            v.get("email")
                .and_then(Value::as_str)
                .map(str::trim)
                .map(String::from)
        })
        .filter(|e| !e.is_empty());
    let Some(email) = email else {
        return refus(
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({ "code": CODE_COURRIEL_REQUIS }),
        );
    };
    // Seul `email` part : le contrat n'en demande pas plus, et rien d'autre
    // du client ne transite vers le cloud.
    let envoi = json!({ "email": email });
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
