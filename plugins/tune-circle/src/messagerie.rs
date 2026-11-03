//! Messagerie privée (#6902) : relais sans copie locale vers le contrat cloud.
//! Le cloud juge les contacts, les cartes, les limites et les droits. Le
//! journal ne reçoit que les gabarits des routes ; jamais texte ni curseur.
//!
//! [rc11] Message à tout un cercle (site-mozaiklabs#454) : trois routes de
//! plus, relayées de même. Lecture, « lu », suppression et signalement d'une
//! conversation de cercle passent par les routes `/conversations/{id}/…`.

use crate::{
    relais::Relais,
    routes::{en_reponse, refus},
};
use axum::{
    Router,
    body::Bytes,
    extract::{Path, RawQuery, State},
    http::{HeaderValue, StatusCode, header},
    middleware,
    response::Response,
    routing::{delete, get, post, put},
};
use reqwest::Method;
use serde_json::{Value, json};
use std::sync::Arc;

pub fn router(relais: Arc<Relais>) -> Router<()> {
    Router::new()
        .route("/conversations", get(liste))
        .route("/contacts/{id}/conversation", post(ouvrir))
        .route("/conversations/{id}/messages", get(messages).post(envoyer))
        .route("/conversations/{id}/read", post(lire))
        .route("/conversations/{id}/messages/{message}", delete(effacer))
        .route("/conversations/{id}/reports", post(signaler))
        .route("/circles/{id}/conversation", post(ouvrir_cercle))
        .route(
            "/circles/{id}/conversation/messages",
            post(ecrire_au_cercle),
        )
        .route("/circles/{id}/messaging", put(regler_cercle))
        .layer(middleware::map_response(sans_cache))
        .with_state(relais)
}

async fn sans_cache(mut reponse: Response) -> Response {
    reponse
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    reponse
}

fn id_valide(id: &str) -> bool {
    !id.is_empty() && id.len() <= 18 && id.bytes().all(|b| b.is_ascii_digit())
}

fn introuvable() -> Response {
    refus(StatusCode::NOT_FOUND, json!({"error": "not_found"}))
}

fn corps_json(corps: &[u8], vide_permis: bool) -> Result<Value, ()> {
    if corps.is_empty() && vide_permis {
        return Ok(json!({}));
    }
    serde_json::from_slice(corps).map_err(|_| ())
}

fn corps_invalide() -> Response {
    refus(
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({
            "message": "The body must be JSON.", "errors": {"body": ["The body must be JSON."]}, "code": "circle.invalid_body"
        }),
    )
}

async fn liste(State(r): State<Arc<Relais>>) -> Response {
    en_reponse(
        r.appeler("GET /conversations", Method::GET, &["conversations"], None)
            .await,
    )
}

async fn ouvrir(State(r): State<Arc<Relais>>, Path(id): Path<String>) -> Response {
    if !id_valide(&id) {
        return introuvable();
    }
    en_reponse(
        r.appeler(
            "POST /contacts/{id}/conversation",
            Method::POST,
            &["contacts", &id, "conversation"],
            None,
        )
        .await,
    )
}

async fn messages(
    State(r): State<Arc<Relais>>,
    Path(id): Path<String>,
    RawQuery(q): RawQuery,
) -> Response {
    if !id_valide(&id) {
        return introuvable();
    }
    en_reponse(
        r.appeler_avec_requete(
            "GET /conversations/{id}/messages",
            Method::GET,
            &["conversations", &id, "messages"],
            q.as_deref(),
            None,
        )
        .await,
    )
}

async fn envoyer(State(r): State<Arc<Relais>>, Path(id): Path<String>, corps: Bytes) -> Response {
    if !id_valide(&id) {
        return introuvable();
    }
    let corps = match corps_json(&corps, false) {
        Ok(c) => c,
        Err(()) => return corps_invalide(),
    };
    en_reponse(
        r.appeler(
            "POST /conversations/{id}/messages",
            Method::POST,
            &["conversations", &id, "messages"],
            Some(&corps),
        )
        .await,
    )
}

async fn lire(State(r): State<Arc<Relais>>, Path(id): Path<String>, corps: Bytes) -> Response {
    if !id_valide(&id) {
        return introuvable();
    }
    let corps = match corps_json(&corps, true) {
        Ok(c) => c,
        Err(()) => return corps_invalide(),
    };
    en_reponse(
        r.appeler(
            "POST /conversations/{id}/read",
            Method::POST,
            &["conversations", &id, "read"],
            Some(&corps),
        )
        .await,
    )
}

async fn effacer(
    State(r): State<Arc<Relais>>,
    Path((id, message)): Path<(String, String)>,
) -> Response {
    if !id_valide(&id) || !id_valide(&message) {
        return introuvable();
    }
    en_reponse(
        r.appeler(
            "DELETE /conversations/{id}/messages/{message}",
            Method::DELETE,
            &["conversations", &id, "messages", &message],
            None,
        )
        .await,
    )
}

async fn signaler(State(r): State<Arc<Relais>>, Path(id): Path<String>, corps: Bytes) -> Response {
    if !id_valide(&id) {
        return introuvable();
    }
    let corps = match corps_json(&corps, false) {
        Ok(c) => c,
        Err(()) => return corps_invalide(),
    };
    en_reponse(
        r.appeler(
            "POST /conversations/{id}/reports",
            Method::POST,
            &["conversations", &id, "reports"],
            Some(&corps),
        )
        .await,
    )
}

/// [rc11] Ouvre (ou retrouve) la conversation d'un cercle dont on est membre.
async fn ouvrir_cercle(State(r): State<Arc<Relais>>, Path(id): Path<String>) -> Response {
    if !id_valide(&id) {
        return introuvable();
    }
    en_reponse(
        r.appeler(
            "POST /circles/{id}/conversation",
            Method::POST,
            &["circles", &id, "conversation"],
            None,
        )
        .await,
    )
}

/// [rc11] Écrit à tout le cercle ; le site tient les droits et les seaux.
async fn ecrire_au_cercle(
    State(r): State<Arc<Relais>>,
    Path(id): Path<String>,
    corps: Bytes,
) -> Response {
    if !id_valide(&id) {
        return introuvable();
    }
    let corps = match corps_json(&corps, false) {
        Ok(c) => c,
        Err(()) => return corps_invalide(),
    };
    en_reponse(
        r.appeler(
            "POST /circles/{id}/conversation/messages",
            Method::POST,
            &["circles", &id, "conversation", "messages"],
            Some(&corps),
        )
        .await,
    )
}

/// [rc11] « Seul le créateur peut écrire » ou « tout le monde » : `{ write }`.
async fn regler_cercle(
    State(r): State<Arc<Relais>>,
    Path(id): Path<String>,
    corps: Bytes,
) -> Response {
    if !id_valide(&id) {
        return introuvable();
    }
    let corps = match corps_json(&corps, false) {
        Ok(c) => c,
        Err(()) => return corps_invalide(),
    };
    en_reponse(
        r.appeler(
            "PUT /circles/{id}/messaging",
            Method::PUT,
            &["circles", &id, "messaging"],
            Some(&corps),
        )
        .await,
    )
}
