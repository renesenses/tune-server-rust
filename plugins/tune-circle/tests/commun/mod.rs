//! Un faux mozaiklabs, local, qui implémente le contrat `/api/v1/circle` de
//! #5018 — l'API réelle est codée en parallèle et n'est pas encore déployée.
//!
//! Il tient le cercle d'UN compte (le détenteur du jeton), compte les appels
//! reçus, et sait jouer la panne (500) et le débit dépassé (429). Il sert
//! aussi `POST /oauth/token` pour le rafraîchissement.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::sqlite::SqliteDb;

pub const JETON: &str = "jeton-acces-SECRET-5018";
pub const JETON_PERIME: &str = "jeton-perime-SECRET-5018";
pub const RAFRAICHISSEMENT: &str = "jeton-rafraichissement-SECRET-5018";
pub const JETON_NEUF: &str = "jeton-neuf-SECRET-5018";
pub const RAFRAICHISSEMENT_NEUF: &str = "jeton-rafraichissement-neuf-SECRET-5018";
pub const COURRIEL_INVITE: &str = "claire.invitee@exemple.fr";

pub struct Faux {
    pub jeton_valide: String,
    pub rafraichissement_valide: String,
    pub members: Vec<Value>,
    pub sent: Vec<Value>,
    pub received: Vec<Value>,
    /// Appels reçus sur `/api/v1/circle…` (auth comprise).
    pub appels: usize,
    pub panne: bool,
    /// Invitations encore permises avant le 429.
    pub invitations_permises: u32,
    /// Le dernier corps reçu par `POST /invitations`.
    pub dernier_corps: Option<Value>,
}

pub type Partage = Arc<Mutex<Faux>>;

pub fn cercle_initial() -> Value {
    json!({
        "members": [
            { "user_id": 7, "name": "Alice", "since": "2026-09-01T10:00:00Z" },
            { "user_id": 9, "name": "Bruno", "since": "2026-09-02T11:00:00Z" }
        ],
        "sent": [
            { "id": "inv-s1", "name_or_email": "bob.envoye@exemple.fr",
              "created_at": "2026-09-20T08:00:00Z", "expires_at": "2026-10-20T08:00:00Z" }
        ],
        "received": [
            { "id": "inv-r1", "name_or_email": "Denis",
              "created_at": "2026-09-21T08:00:00Z", "expires_at": "2026-10-21T08:00:00Z" },
            { "id": "inv-r2", "name_or_email": "Emma",
              "created_at": "2026-09-22T08:00:00Z", "expires_at": "2026-10-22T08:00:00Z" }
        ]
    })
}

impl Faux {
    fn neuf() -> Self {
        let c = cercle_initial();
        Self {
            jeton_valide: JETON.into(),
            rafraichissement_valide: RAFRAICHISSEMENT.into(),
            members: c["members"].as_array().unwrap().clone(),
            sent: c["sent"].as_array().unwrap().clone(),
            received: c["received"].as_array().unwrap().clone(),
            appels: 0,
            panne: false,
            invitations_permises: 10,
            dernier_corps: None,
        }
    }

    pub fn cercle(&self) -> Value {
        json!({ "members": self.members, "sent": self.sent, "received": self.received })
    }
}

pub struct Serveur {
    pub base: String,
    pub etat: Partage,
}

fn non_authentifie() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "message": "Unauthenticated." })),
    )
        .into_response()
}

fn introuvable() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "message": "Not Found." })),
    )
        .into_response()
}

/// Compte l'appel, puis rend la réponse de panne ou d'auth s'il y a lieu.
fn garde(etat: &Partage, entetes: &HeaderMap) -> Option<Response> {
    let mut f = etat.lock().unwrap();
    f.appels += 1;
    if f.panne {
        return Some(
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/html")],
                "<html><body>Server Error</body></html>",
            )
                .into_response(),
        );
    }
    let attendu = format!("Bearer {}", f.jeton_valide);
    let recu = entetes
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if recu != Some(attendu.as_str()) {
        return Some(non_authentifie());
    }
    None
}

async fn lister(State(e): State<Partage>, h: HeaderMap) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    Json(e.lock().unwrap().cercle()).into_response()
}

async fn inviter(State(e): State<Partage>, h: HeaderMap, corps: Bytes) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let mut f = e.lock().unwrap();
    if f.invitations_permises == 0 {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "42")],
            Json(json!({ "message": "Too Many Attempts." })),
        )
            .into_response();
    }
    f.invitations_permises -= 1;
    let v: Value = serde_json::from_slice(&corps).unwrap_or(Value::Null);
    f.dernier_corps = Some(v.clone());
    let id = format!("inv-s{}", f.sent.len() + 1);
    f.sent.push(json!({
        "id": id, "name_or_email": v["email"],
        "created_at": "2026-09-25T08:00:00Z", "expires_at": "2026-10-25T08:00:00Z"
    }));
    // Même réponse que l'adresse soit connue ou non (aucune énumération).
    (
        StatusCode::ACCEPTED,
        Json(json!({ "status": "invitation_sent" })),
    )
        .into_response()
}

fn position(v: &[Value], cle: &str, id: &str) -> Option<usize> {
    v.iter().position(|x| match &x[cle] {
        Value::String(s) => s == id,
        Value::Number(n) => n.to_string() == id,
        _ => false,
    })
}

async fn accepter(State(e): State<Partage>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let mut f = e.lock().unwrap();
    let Some(i) = position(&f.received, "id", &id) else {
        return introuvable();
    };
    let inv = f.received.remove(i);
    let membre = json!({
        "user_id": 100 + f.members.len(), "name": inv["name_or_email"],
        "since": "2026-09-25T09:00:00Z"
    });
    f.members.push(membre.clone());
    Json(json!({ "member": membre })).into_response()
}

async fn refuser(State(e): State<Partage>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let mut f = e.lock().unwrap();
    let Some(i) = position(&f.received, "id", &id) else {
        return introuvable();
    };
    f.received.remove(i);
    Json(json!({ "status": "declined" })).into_response()
}

async fn retirer(State(e): State<Partage>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let mut f = e.lock().unwrap();
    let Some(i) = position(&f.sent, "id", &id) else {
        return introuvable();
    };
    f.sent.remove(i);
    StatusCode::NO_CONTENT.into_response()
}

async fn revoquer(State(e): State<Partage>, h: HeaderMap, Path(uid): Path<String>) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let mut f = e.lock().unwrap();
    let Some(i) = position(&f.members, "user_id", &uid) else {
        return introuvable();
    };
    f.members.remove(i);
    StatusCode::NO_CONTENT.into_response()
}

/// `POST /oauth/token`, `grant_type=refresh_token` : fait tourner la paire.
async fn jeton(State(e): State<Partage>, corps: Bytes) -> Response {
    let texte = String::from_utf8_lossy(&corps).to_string();
    let mut f = e.lock().unwrap();
    let attendu = format!("refresh_token={}", f.rafraichissement_valide);
    if !texte.contains("grant_type=refresh_token") || !texte.split('&').any(|p| p == attendu) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid_grant" })),
        )
            .into_response();
    }
    f.jeton_valide = JETON_NEUF.into();
    f.rafraichissement_valide = RAFRAICHISSEMENT_NEUF.into();
    Json(json!({
        "access_token": JETON_NEUF,
        "refresh_token": RAFRAICHISSEMENT_NEUF,
        "expires_in": 3600
    }))
    .into_response()
}

pub async fn demarrer() -> Serveur {
    let etat: Partage = Arc::new(Mutex::new(Faux::neuf()));
    let app = Router::new()
        .route("/api/v1/circle", get(lister))
        .route("/api/v1/circle/invitations", post(inviter))
        .route("/api/v1/circle/invitations/{id}", delete(retirer))
        .route("/api/v1/circle/invitations/{id}/accept", post(accepter))
        .route("/api/v1/circle/invitations/{id}/decline", post(refuser))
        .route("/api/v1/circle/members/{user_id}", delete(revoquer))
        .route("/oauth/token", post(jeton))
        .with_state(etat.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.unwrap();
    });
    Serveur {
        base: format!("http://{adresse}"),
        etat,
    }
}

/// Une adresse où rien n'écoute : le port d'un écouteur aussitôt fermé.
pub async fn adresse_morte() -> String {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    drop(ecoute);
    format!("http://{adresse}")
}

/// Une base de réglages neuve, avec (ou sans) la session SSO.
pub fn base(url_cloud: &str, jeton: Option<&str>) -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let s = SettingsRepo::with_backend(backend.clone());
    s.set("mozaik_base_url", url_cloud).unwrap();
    if let Some(j) = jeton {
        s.set("mozaik_access_token", j).unwrap();
        s.set("mozaik_refresh_token", RAFRAICHISSEMENT).unwrap();
    }
    backend
}

pub fn app(backend: Arc<dyn DbBackend>) -> Router {
    tune_circle::routes::router(Arc::new(tune_circle::relais::Relais::new(backend)))
}

pub struct Rendu {
    pub statut: StatusCode,
    pub entetes: HeaderMap,
    pub octets: Vec<u8>,
}

impl Rendu {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.octets).unwrap_or(Value::Null)
    }
}

pub async fn appel(app: &Router, methode: &str, chemin: &str, corps: Option<Value>) -> Rendu {
    use tower::ServiceExt;
    let mut req = axum::http::Request::builder().method(methode).uri(chemin);
    let body = match corps {
        Some(c) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            axum::body::Body::from(c.to_string())
        }
        None => axum::body::Body::empty(),
    };
    let r = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let statut = r.status();
    let entetes = r.headers().clone();
    let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    Rendu {
        statut,
        entetes,
        octets,
    }
}
