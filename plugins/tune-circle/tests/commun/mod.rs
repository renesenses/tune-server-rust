//! Un faux mozaiklabs, local, qui implémente le contrat `/api/v1/circle` de
//! #5018 — l'API réelle est codée en parallèle et n'est pas encore déployée.
//!
//! Formes alignées sur le côté cloud tel qu'écrit (renesenses/site-mozaiklabs
//! #223) : `id` et `user_id` entiers ; invitation créée = 201 et l'invitation ;
//! `accept` = 200 et le membre ; `decline` et les deux `DELETE` = 200
//! `{ "ok": true }` ; 404 `{ "error": "not_found" }` ; 409 `already_member`,
//! `already_invited`, `invitation_received` ; 422 `self_invitation` ou adresse
//! invalide ; 429 au-delà du débit ; toujours du JSON.
//!
//! Il tient le cercle d'UN compte (le détenteur du jeton), compte les appels
//! reçus, et sait jouer la panne (500) et le débit dépassé (429). Il sert
//! aussi `POST /oauth/token` pour le rafraîchissement.
//!
//! Avenant « plusieurs cercles » (#5018, 25/09), tel que mozaiklabs l'a écrit
//! (site-mozaiklabs#224) : `GET /` porte aussi `circles` (les cercles DU
//! détenteur, `{ id, name, member_ids }`) mais SEULEMENT s'il en a au moins
//! un — sinon la forme exacte `{members, sent, received}` de T1 ;
//! `POST /circles` = 201 et le cercle ; `PATCH /circles/{id}` et
//! `PUT /circles/{id}/members/{user_id}` (idempotent) = 200 et le cercle ;
//! les deux `DELETE` = 200 `{ "ok": true }`. Nom de 1 à 60 caractères (422
//! de validation Laravel), unique sans casse (409 `circle_name_taken`), au
//! plus [`CERCLES_MAX`] (422 `too_many_circles`) ; cercle d'un autre,
//! non-contact, ou contact non rangé dans le cercle retiré = 404. Révoquer un
//! contact le retire de tous les cercles ; supprimer un cercle ne révoque
//! personne. Sur `POST /invitations`, un `circle_id` non entier = 422 de
//! validation, celui d'un autre = 404 sans invitation ; sinon il est gardé
//! pour [`Faux::acceptee_par_l_invite`].

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
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
/// L'adresse du détenteur du jeton : s'inviter soi-même = 422.
pub const COURRIEL_DU_COMPTE: &str = "moi.proprietaire@exemple.fr";
/// L'adresse d'Alice, déjà membre : 409 `already_member`.
pub const COURRIEL_MEMBRE: &str = "alice.membre@exemple.fr";
/// L'adresse de Denis, qui nous a déjà invités : 409 `invitation_received`.
pub const COURRIEL_QUI_NOUS_INVITE: &str = "denis.invitant@exemple.fr";
/// L'adresse d'une invitation déjà envoyée et en attente : 409 `already_invited`.
pub const COURRIEL_DEJA_INVITE: &str = "bob.envoye@exemple.fr";
/// Plafond de cercles par utilisateur (avenant de #5018).
pub const CERCLES_MAX: usize = 50;
/// Un cercle qui existe chez mozaiklabs, mais appartient à un AUTRE compte.
pub const CERCLE_D_UN_AUTRE: i64 = 77;

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
    pub prochain_id: i64,
    /// Les cercles du détenteur du jeton.
    pub circles: Vec<Value>,
    pub prochain_cercle_id: i64,
    /// Le `circle_id` porté par chaque invitation envoyée (id → circle_id).
    pub rangement_des_invitations: Vec<(i64, Value)>,
}

pub type Partage = Arc<Mutex<Faux>>;

pub fn cercle_initial() -> Value {
    json!({
        "members": [
            { "user_id": 7, "name": "Alice", "since": "2026-09-01T10:00:00Z" },
            { "user_id": 9, "name": "Bruno", "since": "2026-09-02T11:00:00Z" }
        ],
        "sent": [
            { "id": 11, "name_or_email": "bob.envoye@exemple.fr",
              "created_at": "2026-09-20T08:00:00Z", "expires_at": "2026-10-20T08:00:00Z" }
        ],
        "received": [
            { "id": 21, "name_or_email": "Denis",
              "created_at": "2026-09-21T08:00:00Z", "expires_at": "2026-10-21T08:00:00Z" },
            { "id": 22, "name_or_email": "Emma",
              "created_at": "2026-09-22T08:00:00Z", "expires_at": "2026-10-22T08:00:00Z" }
        ],
        "circles": [
            { "id": 1, "name": "Famille", "member_ids": [7, 9] },
            { "id": 2, "name": "Jazz", "member_ids": [9] }
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
            prochain_id: 12,
            circles: c["circles"].as_array().unwrap().clone(),
            prochain_cercle_id: 3,
            rangement_des_invitations: Vec::new(),
        }
    }

    pub fn cercle(&self) -> Value {
        let mut c = json!({
            "members": self.members, "sent": self.sent, "received": self.received
        });
        // Comme site-mozaiklabs#224 : la clé n'existe que s'il y a un cercle.
        if !self.circles.is_empty() {
            c["circles"] = json!(self.circles);
        }
        c
    }

    /// Ce que fait le cloud quand l'invité accepte, de SON côté, une
    /// invitation envoyée : il devient contact, et il est rangé dans le cercle
    /// que portait l'invitation — si ce cercle existe encore. Rend son `user_id`.
    pub fn acceptee_par_l_invite(&mut self, invitation_id: i64) -> Option<i64> {
        let i = position(&self.sent, "id", &invitation_id.to_string())?;
        let inv = self.sent.remove(i);
        let user_id = 200 + invitation_id;
        self.members.push(json!({
            "user_id": user_id, "name": inv["name_or_email"],
            "since": "2026-09-25T10:00:00Z"
        }));
        let circle_id = self
            .rangement_des_invitations
            .iter()
            .find(|(id, _)| *id == invitation_id)
            .map(|(_, c)| c.clone());
        if let Some(c) = circle_id
            && let Some(k) = self.circles.iter().position(|x| x["id"] == c)
        {
            self.circles[k]["member_ids"]
                .as_array_mut()
                .unwrap()
                .push(json!(user_id));
        }
        Some(user_id)
    }

    /// Les `member_ids` du cercle `id`, `None` s'il n'existe pas.
    pub fn membres_du_cercle(&self, id: i64) -> Option<Vec<i64>> {
        self.circles.iter().find(|c| c["id"] == id).map(|c| {
            c["member_ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m.as_i64().unwrap())
                .collect()
        })
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
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not_found" }))).into_response()
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

fn refus(statut: StatusCode, motif: &str) -> Response {
    (statut, Json(json!({ "error": motif }))).into_response()
}

/// Un 422 de validation, à la forme de Laravel.
pub fn corps_de_validation(champ: &str, message: &str) -> Value {
    json!({ "message": message, "errors": { champ: [message] } })
}

fn validation(champ: &str, message: &str) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(corps_de_validation(champ, message)),
    )
        .into_response()
}

pub const MESSAGE_CIRCLE_ID: &str = "The circle id field must be an integer.";
pub const MESSAGE_NOM: &str = "The name field must be between 1 and 60 characters.";

async fn inviter(State(e): State<Partage>, h: HeaderMap, corps: Bytes) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let mut f = e.lock().unwrap();
    // Le limiteur passe avant la validation, comme `throttle` chez Laravel.
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
    let Some(email) = v["email"].as_str().filter(|m| m.contains('@')) else {
        return refus(StatusCode::UNPROCESSABLE_ENTITY, "invalid_email");
    };
    match email {
        COURRIEL_DU_COMPTE => return refus(StatusCode::UNPROCESSABLE_ENTITY, "self_invitation"),
        COURRIEL_MEMBRE => return refus(StatusCode::CONFLICT, "already_member"),
        COURRIEL_QUI_NOUS_INVITE => return refus(StatusCode::CONFLICT, "invitation_received"),
        _ => {}
    }
    if f.sent.iter().any(|i| i["name_or_email"] == email) {
        return refus(StatusCode::CONFLICT, "already_invited");
    }
    let circle_id = v.get("circle_id").filter(|c| !c.is_null()).cloned();
    if let Some(c) = &circle_id {
        if !c.is_i64() {
            return validation("circle_id", MESSAGE_CIRCLE_ID);
        }
        // Le cercle d'un autre : 404, ni invitation ni courriel.
        if !f.circles.iter().any(|x| x["id"] == *c) {
            return introuvable();
        }
    }
    if let Some(c) = circle_id {
        let id = f.prochain_id;
        f.rangement_des_invitations.push((id, c));
    }
    let invitation = json!({
        "id": f.prochain_id, "name_or_email": email,
        "created_at": "2026-09-25T08:00:00Z", "expires_at": "2026-10-25T08:00:00Z"
    });
    f.prochain_id += 1;
    f.sent.push(invitation.clone());
    // Même réponse que l'adresse soit connue ou non (aucune énumération).
    (StatusCode::CREATED, Json(invitation)).into_response()
}

/// Un identifiant JSON (entier ou chaîne) lu tel qu'il est dans le chemin.
fn meme_id(v: &Value, id: &str) -> bool {
    match v {
        Value::String(s) => s == id,
        Value::Number(n) => n.to_string() == id,
        _ => false,
    }
}

fn position(v: &[Value], cle: &str, id: &str) -> Option<usize> {
    v.iter().position(|x| meme_id(&x[cle], id))
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
    Json(membre).into_response()
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
    Json(json!({ "ok": true })).into_response()
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
    Json(json!({ "ok": true })).into_response()
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
    // Révoquer un contact le retire de TOUS les cercles, aussitôt.
    for c in &mut f.circles {
        c["member_ids"]
            .as_array_mut()
            .unwrap()
            .retain(|m| !meme_id(m, &uid));
    }
    Json(json!({ "ok": true })).into_response()
}

// Avenant « plusieurs cercles » ---------------------------------------------

/// Le nom valide (1 à 60 caractères), `None` s'il faut refuser (422).
fn nom_valide(corps: &Bytes) -> Option<String> {
    let v: Value = serde_json::from_slice(corps).unwrap_or(Value::Null);
    v["name"]
        .as_str()
        .map(str::trim)
        .filter(|n| (1..=60).contains(&n.chars().count()))
        .map(str::to_string)
}

fn nom_invalide() -> Response {
    validation("name", MESSAGE_NOM)
}

/// Un autre cercle du propriétaire porte-t-il déjà ce nom, casse ignorée ?
fn nom_pris(f: &Faux, nom: &str, sauf: Option<usize>) -> bool {
    let nom = nom.to_lowercase();
    f.circles.iter().enumerate().any(|(k, c)| {
        Some(k) != sauf
            && c["name"].as_str().map(str::to_lowercase).as_deref() == Some(nom.as_str())
    })
}

async fn creer_cercle(State(e): State<Partage>, h: HeaderMap, corps: Bytes) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let mut f = e.lock().unwrap();
    let Some(nom) = nom_valide(&corps) else {
        return nom_invalide();
    };
    if nom_pris(&f, &nom, None) {
        return refus(StatusCode::CONFLICT, "circle_name_taken");
    }
    if f.circles.len() >= CERCLES_MAX {
        return refus(StatusCode::UNPROCESSABLE_ENTITY, "too_many_circles");
    }
    let cercle = json!({ "id": f.prochain_cercle_id, "name": nom, "member_ids": [] });
    f.prochain_cercle_id += 1;
    f.circles.push(cercle.clone());
    (StatusCode::CREATED, Json(cercle)).into_response()
}

async fn renommer_cercle(
    State(e): State<Partage>,
    h: HeaderMap,
    Path(id): Path<String>,
    corps: Bytes,
) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let mut f = e.lock().unwrap();
    let Some(k) = position(&f.circles, "id", &id) else {
        return introuvable();
    };
    let Some(nom) = nom_valide(&corps) else {
        return nom_invalide();
    };
    if nom_pris(&f, &nom, Some(k)) {
        return refus(StatusCode::CONFLICT, "circle_name_taken");
    }
    f.circles[k]["name"] = json!(nom);
    Json(f.circles[k].clone()).into_response()
}

async fn supprimer_cercle(
    State(e): State<Partage>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let mut f = e.lock().unwrap();
    let Some(k) = position(&f.circles, "id", &id) else {
        return introuvable();
    };
    // Les contacts restent : supprimer un cercle ne révoque personne.
    f.circles.remove(k);
    Json(json!({ "ok": true })).into_response()
}

async fn ranger(
    State(e): State<Partage>,
    h: HeaderMap,
    Path((id, uid)): Path<(String, String)>,
) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let mut f = e.lock().unwrap();
    let Some(k) = position(&f.circles, "id", &id) else {
        return introuvable();
    };
    let Some(m) = position(&f.members, "user_id", &uid) else {
        return introuvable();
    };
    let user_id = f.members[m]["user_id"].clone();
    let ids = f.circles[k]["member_ids"].as_array_mut().unwrap();
    if !ids.contains(&user_id) {
        ids.push(user_id);
    }
    Json(f.circles[k].clone()).into_response()
}

async fn deranger(
    State(e): State<Partage>,
    h: HeaderMap,
    Path((id, uid)): Path<(String, String)>,
) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let mut f = e.lock().unwrap();
    let Some(k) = position(&f.circles, "id", &id) else {
        return introuvable();
    };
    // Non-contact, ou contact qui n'est pas rangé dans CE cercle : 404.
    let ids = f.circles[k]["member_ids"].as_array_mut().unwrap();
    let Some(i) = ids.iter().position(|m| meme_id(m, &uid)) else {
        return introuvable();
    };
    ids.remove(i);
    Json(json!({ "ok": true })).into_response()
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
        .route("/api/v1/circle/circles", post(creer_cercle))
        .route(
            "/api/v1/circle/circles/{id}",
            delete(supprimer_cercle).patch(renommer_cercle),
        )
        .route(
            "/api/v1/circle/circles/{id}/members/{user_id}",
            put(ranger).delete(deranger),
        )
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
