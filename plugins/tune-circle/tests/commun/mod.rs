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

//!
//! T2, le catalogue d'un contact (#5325), selon le contrat de l'issue : sous
//! [`Faux::t2`], chaque cercle de `GET /` porte `"sharing": { "library",
//! "server_id" }`. `PUT /circles/{id}/sharing/library` `{ "server_id" }` : un
//! `server_id` qui n'est pas une chaîne = 422 de validation, qui n'est pas un
//! serveur du compte ([`Faux::serveurs_du_compte`]) = 404, cercle d'un autre
//! = 404, sinon 200 et le cercle ; `DELETE` = 200 `{ "ok": true }`.
//! `GET /shared-with-me` rend [`Faux::partagent_avec_moi`] ; les lectures
//! `/contacts/{user_id}/library/…` rendent 404 pour tout identifiant absent de
//! [`Faux::contacts_qui_partagent`], et notent la chaîne de requête reçue.
//! `POST /api/v1/cloud-library/{server_id}/sync` note chaque corps reçu, brut.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
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
/// Le `server_id` de CE serveur Tune, inscrit au compte (T2).
pub const SERVEUR_DU_COMPTE: &str = "srv-moi-5325";
/// Un autre serveur du même compte (`cloud_servers.user_id` non unique).
pub const AUTRE_SERVEUR_DU_COMPTE: &str = "srv-salon-5325";
/// Le serveur d'un AUTRE compte.
pub const SERVEUR_D_UN_AUTRE: &str = "srv-etranger-5325";

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
    // T2 (#5325) ------------------------------------------------------------
    /// `GET /` porte la clé `sharing` de chaque cercle (cloud T2 déployé).
    pub t2: bool,
    /// Les `cloud_servers` du détenteur du jeton.
    pub serveurs_du_compte: Vec<String>,
    /// Partages actifs : (circle_id, server_id).
    pub partages: Vec<(i64, String)>,
    /// Le dernier corps reçu par `PUT …/sharing/library`.
    pub dernier_corps_partage: Option<Value>,
    /// Ce que rend `GET /shared-with-me`.
    pub partagent_avec_moi: Vec<Value>,
    /// Les `user_id` dont la bibliothèque est lisible par le détenteur.
    pub contacts_qui_partagent: Vec<String>,
    /// La chaîne de requête de la dernière lecture de bibliothèque.
    pub derniere_requete: Option<String>,
    /// Lectures de bibliothèque encore permises avant le 429.
    pub lectures_permises: u32,
    /// `POST /api/v1/cloud-library/{server_id}/sync` : (server_id, corps brut).
    pub synchros: Vec<(String, String)>,
    /// Liaison du serveur au compte (site-mozaiklabs#233) : le jeton en cours,
    /// `None` tant qu'aucun lien n'a été fait.
    pub jeton_de_liaison: Option<String>,
    /// Appels reçus par `POST …/{server_id}/link`.
    pub liaisons: usize,
    /// Le corps brut du dernier `POST …/link` (le contrat : aucun).
    pub dernier_corps_liaison: Option<Vec<u8>>,
    /// `…/link` rend ce statut au lieu du jeton.
    pub liaison_refusee: Option<u16>,
    /// `sync` exige le jeton de liaison (401 `server_token_invalid`, 403
    /// `server_not_linked`).
    pub sync_exige_liaison: bool,
    /// `sync` refuse tout jeton (401 `server_token_invalid`), même neuf.
    pub jeton_toujours_refuse: bool,
    /// `sync` rend 403 `premium_required` (gratuit dont aucun cercle ne
    /// partage ce serveur).
    pub premium_requis: bool,
    /// L'en-tête `X-Tune-Server-Token` de chaque tentative de `sync`.
    pub entetes_de_synchro: Vec<Option<String>>,
    // T5 (#5328) ------------------------------------------------------------
    /// Les playlists de cercle visibles par le détenteur du jeton.
    pub playlists: Vec<Value>,
    /// Révocation, retrait du cercle ou suppression : toute route de playlist
    /// rend 404, comme pour une playlist inexistante.
    pub playlists_coupees: bool,
    /// (route, corps reçu) de chaque écriture sur une playlist.
    pub ecritures_de_playlist: Vec<(String, Value)>,
    /// La chaîne de requête du dernier `DELETE …/items/{item_id}`.
    pub requete_de_retrait: Option<String>,
    /// Lectures `GET /playlists/{id}` reçues.
    pub lectures_de_playlist: usize,
    /// L'en-tête `If-Match` de chaque écriture reçue.
    pub if_match_recus: Vec<Option<String>>,
    /// Les playlists récupérables (cercle supprimé) du détenteur.
    pub recuperables: Vec<Value>,
    /// Les `DELETE /recoverable-playlists/{id}` reçus.
    pub renonciations: Vec<String>,
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
            t2: false,
            serveurs_du_compte: vec![SERVEUR_DU_COMPTE.into(), AUTRE_SERVEUR_DU_COMPTE.into()],
            partages: Vec::new(),
            dernier_corps_partage: None,
            partagent_avec_moi: vec![json!({ "user_id": 7, "name": "Alice", "library": true })],
            contacts_qui_partagent: vec!["7".into()],
            derniere_requete: None,
            lectures_permises: 1000,
            synchros: Vec::new(),
            jeton_de_liaison: None,
            liaisons: 0,
            dernier_corps_liaison: None,
            liaison_refusee: None,
            sync_exige_liaison: false,
            jeton_toujours_refuse: false,
            premium_requis: false,
            entetes_de_synchro: Vec::new(),
            playlists: vec![playlist_initiale()],
            playlists_coupees: false,
            ecritures_de_playlist: Vec::new(),
            requete_de_retrait: None,
            lectures_de_playlist: 0,
            if_match_recus: Vec::new(),
            recuperables: vec![recuperable_initiale()],
            renonciations: Vec::new(),
        }
    }

    pub fn cercle(&self) -> Value {
        let mut c = json!({
            "members": self.members, "sent": self.sent, "received": self.received
        });
        // Comme site-mozaiklabs#224 : la clé n'existe que s'il y a un cercle.
        if !self.circles.is_empty() {
            let mut cercles = self.circles.clone();
            if self.t2 {
                for cercle in &mut cercles {
                    cercle["sharing"] = self.partage_du_cercle(&cercle["id"]);
                }
            }
            c["circles"] = json!(cercles);
        }
        c
    }

    /// `{ "library", "server_id" }` du cercle `id` (contrat T2).
    pub fn partage_du_cercle(&self, id: &Value) -> Value {
        match self.partages.iter().find(|(c, _)| Some(*c) == id.as_i64()) {
            Some((_, server_id)) => json!({ "library": true, "server_id": server_id }),
            None => json!({ "library": false, "server_id": null }),
        }
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

// T2 (#5325) ---------------------------------------------------------------

async fn partager(
    State(e): State<Partage>,
    h: HeaderMap,
    Path(id): Path<String>,
    corps: Bytes,
) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let mut f = e.lock().unwrap();
    let v: Value = serde_json::from_slice(&corps).unwrap_or(Value::Null);
    f.dernier_corps_partage = Some(v.clone());
    let Some(server_id) = v["server_id"].as_str().map(str::to_string) else {
        return validation("server_id", MESSAGE_SERVER_ID);
    };
    let Some(k) = position(&f.circles, "id", &id) else {
        return introuvable();
    };
    // Un serveur que ce compte n'a pas lié (site-mozaiklabs#233).
    if !f.serveurs_du_compte.contains(&server_id) {
        return refus(StatusCode::NOT_FOUND, "server_not_linked");
    }
    let circle_id = f.circles[k]["id"].as_i64().unwrap();
    f.partages.retain(|(c, _)| *c != circle_id);
    f.partages.push((circle_id, server_id));
    let mut cercle = f.circles[k].clone();
    cercle["sharing"] = f.partage_du_cercle(&json!(circle_id));
    Json(cercle).into_response()
}

async fn ne_plus_partager(
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
    let circle_id = f.circles[k]["id"].as_i64().unwrap();
    f.partages.retain(|(c, _)| *c != circle_id);
    Json(json!({ "ok": true })).into_response()
}

async fn partagent_avec_moi(State(e): State<Partage>, h: HeaderMap) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    Json(json!(e.lock().unwrap().partagent_avec_moi)).into_response()
}

/// Le catalogue d'Alice, projeté (liste blanche du contrat).
pub fn catalogue(quoi: &str) -> Value {
    match quoi {
        "stats" => json!({ "tracks": 2, "albums": 1, "artists": 1,
                           "last_sync": "2026-09-28T08:00:00Z" }),
        "artists" => json!({ "data": [ { "id": 20, "name": "Miles Davis" } ],
                             "current_page": 1, "last_page": 1, "total": 1 }),
        "albums" => json!({ "data": [ { "id": 10, "title": "Kind of Blue",
                            "artist_name": "Miles Davis", "genre": "Jazz",
                            "track_count": 2, "year": 1959 } ],
                            "current_page": 1, "last_page": 1, "total": 1 }),
        _ => json!({ "data": [
            { "id": 1, "title": "So What", "artist_name": "Miles Davis",
              "album_title": "Kind of Blue", "album_id": 10, "format": "flac",
              "sample_rate": 96000, "bit_depth": 24, "duration_ms": 562000,
              "genre": "Jazz", "track_number": 1, "disc_number": 1 }
        ], "current_page": 1, "last_page": 1, "total": 1 }),
    }
}

fn lire_catalogue(
    e: &Partage,
    h: &HeaderMap,
    uid: &str,
    requete: Option<String>,
    quoi: &str,
) -> Response {
    if let Some(r) = garde(e, h) {
        return r;
    }
    let mut f = e.lock().unwrap();
    f.derniere_requete = requete;
    if f.lectures_permises == 0 {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "17")],
            Json(json!({ "message": "Too Many Attempts." })),
        )
            .into_response();
    }
    f.lectures_permises -= 1;
    // Non-contact, révoqué, non rangé, partage coupé : le même 404.
    if !f.contacts_qui_partagent.iter().any(|c| c == uid) {
        return introuvable();
    }
    Json(catalogue(quoi)).into_response()
}

async fn catalogue_de(
    State(e): State<Partage>,
    h: HeaderMap,
    Path((uid, quoi)): Path<(String, String)>,
    RawQuery(requete): RawQuery,
) -> Response {
    if !["stats", "artists", "albums", "tracks"].contains(&quoi.as_str()) {
        return introuvable();
    }
    lire_catalogue(&e, &h, &uid, requete, &quoi)
}

async fn pistes_de_l_album(
    State(e): State<Partage>,
    h: HeaderMap,
    Path((uid, _album)): Path<(String, String)>,
    RawQuery(requete): RawQuery,
) -> Response {
    lire_catalogue(&e, &h, &uid, requete, "tracks")
}

/// `POST /api/v1/cloud-library/{server_id}/sync` : note l'en-tête de liaison
/// de chaque tentative, et le corps BRUT de chaque synchro acceptée.
async fn synchro(
    State(e): State<Partage>,
    h: HeaderMap,
    Path(server_id): Path<String>,
    corps: Bytes,
) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let mut f = e.lock().unwrap();
    let presente = h
        .get("x-tune-server-token")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    f.entetes_de_synchro.push(presente.clone());
    if f.jeton_toujours_refuse {
        return refus(StatusCode::UNAUTHORIZED, "server_token_invalid");
    }
    if f.sync_exige_liaison {
        let Some(attendu) = f.jeton_de_liaison.clone() else {
            return refus(StatusCode::FORBIDDEN, "server_not_linked");
        };
        if presente.as_deref() != Some(attendu.as_str()) {
            return refus(StatusCode::UNAUTHORIZED, "server_token_invalid");
        }
    }
    if f.premium_requis {
        return refus(StatusCode::FORBIDDEN, "premium_required");
    }
    f.synchros
        .push((server_id, String::from_utf8_lossy(&corps).into_owned()));
    Json(json!({ "ok": true })).into_response()
}

/// Préfixe des jetons de liaison du faux cloud (64 caractères en tout).
pub const PREFIXE_JETON_DE_LIAISON: &str = "jeton-liaison-SECRET-5325-";

/// `POST /api/v1/cloud-library/{server_id}/link` : chaque appel renouvelle le
/// jeton, le précédent cesse de valoir.
async fn lier(
    State(e): State<Partage>,
    h: HeaderMap,
    Path(server_id): Path<String>,
    corps: Bytes,
) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let mut f = e.lock().unwrap();
    f.liaisons += 1;
    f.dernier_corps_liaison = Some(corps.to_vec());
    if let Some(statut) = f.liaison_refusee {
        return refus(StatusCode::from_u16(statut).unwrap(), "not_found");
    }
    let jeton = format!("{PREFIXE_JETON_DE_LIAISON}{:0>38}", f.liaisons);
    assert_eq!(jeton.len(), 64);
    f.jeton_de_liaison = Some(jeton.clone());
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({ "server_id": server_id, "token": jeton })),
    )
        .into_response()
}

// T5 (#5328) : les playlists collaboratives ---------------------------------

/// Les clés qu'une référence peut porter (contrat de #5328, décision 4).
pub const CLES_DE_REFERENCE: [&str; 11] = [
    "title",
    "artist_name",
    "album_title",
    "duration_ms",
    "isrc",
    "musicbrainz_recording_id",
    "qobuz_id",
    "tidal_id",
    "spotify_id",
    "deezer_id",
    "youtube_id",
];

/// La playlist 5, du cercle 1 : trois morceaux, par références seulement.
pub fn playlist_initiale() -> Value {
    json!({
        "id": 5, "name": "Dimanche", "version": 3,
        "owner": { "user_id": 1, "name": "Moi" }, "mine": true,
        "items": [
            { "item_id": 51, "title": "So What", "artist_name": "Miles Davis",
              "album_title": "Kind of Blue", "duration_ms": 562000,
              "isrc": "USSM15900113", "qobuz_id": "q-so-what", "tidal_id": "t-so-what",
              "added_by": { "user_id": 7, "name": "Alice" }, "added_at": "2026-09-28T09:00:00Z" },
            { "item_id": 52, "title": "Blue in Green", "artist_name": "Miles Davis",
              "album_title": "Kind of Blue", "duration_ms": 337000,
              "isrc": "USSM15900115",
              "added_by": null, "added_at": "2026-09-28T09:01:00Z" },
            { "item_id": 53, "title": "Un titre que personne n'a", "artist_name": "Inconnu",
              "duration_ms": 200000,
              "added_by": null, "added_at": "2026-09-28T09:02:00Z" }
        ]
    })
}

/// La playlist 8, d'un cercle supprimé : récupérable par le détenteur.
pub fn recuperable_initiale() -> Value {
    json!({
        "id": 8, "name": "Jazz du samedi", "owner": { "user_id": 7, "name": "Alice" },
        "mine": false, "archived_at": "2026-09-28T11:00:00Z",
        "items": [
            { "item_id": 81, "title": "So What", "artist_name": "Miles Davis",
              "album_title": "Kind of Blue", "duration_ms": 562000, "isrc": "USSM15900113",
              "musicbrainz_recording_id": null, "qobuz_id": null, "tidal_id": null,
              "spotify_id": null, "deezer_id": null, "youtube_id": null,
              "added_by": null, "mine": false, "added_at": "2026-09-28T09:00:00Z" },
            { "item_id": 82, "title": "Blue in Green", "artist_name": "Miles Davis",
              "album_title": "Kind of Blue", "duration_ms": 337000, "isrc": null,
              "musicbrainz_recording_id": null, "qobuz_id": "q-bleu", "tidal_id": null,
              "spotify_id": null, "deezer_id": null, "youtube_id": null,
              "added_by": null, "mine": false, "added_at": "2026-09-28T09:01:00Z" },
            { "item_id": 83, "title": "Un titre que personne n'a", "artist_name": "Inconnu",
              "album_title": null, "duration_ms": 200000, "isrc": null,
              "musicbrainz_recording_id": null, "qobuz_id": null, "tidal_id": null,
              "spotify_id": null, "deezer_id": null, "youtube_id": null,
              "added_by": null, "mine": false, "added_at": "2026-09-28T09:02:00Z" }
        ]
    })
}

async fn lister_recuperables(State(e): State<Partage>, h: HeaderMap) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let f = e.lock().unwrap();
    let liste: Vec<Value> = f
        .recuperables
        .iter()
        .map(|p| {
            json!({ "id": p["id"], "name": p["name"], "owner": p["owner"], "mine": p["mine"],
                         "count": p["items"].as_array().map_or(0, Vec::len),
                         "archived_at": p["archived_at"] })
        })
        .collect();
    Json(json!(liste)).into_response()
}

async fn lire_recuperable(
    State(e): State<Partage>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let f = e.lock().unwrap();
    match f.recuperables.iter().find(|p| meme_id(&p["id"], &id)) {
        Some(p) => Json(p.clone()).into_response(),
        None => introuvable(),
    }
}

async fn renoncer_a_la_recuperable(
    State(e): State<Partage>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let mut f = e.lock().unwrap();
    f.renonciations.push(id.clone());
    let avant = f.recuperables.len();
    f.recuperables.retain(|p| !meme_id(&p["id"], &id));
    if f.recuperables.len() == avant {
        return introuvable();
    }
    Json(json!({ "ok": true })).into_response()
}

fn resume(p: &Value) -> Value {
    json!({
        "id": p["id"], "name": p["name"], "owner": p["owner"],
        "count": p["items"].as_array().map_or(0, Vec::len),
        "version": p["version"], "updated_at": "2026-09-28T09:02:00Z", "mine": p["mine"],
    })
}

/// La playlist `id`, sous la garde du faux : `None` = 404.
fn playlist_de<'a>(f: &'a mut Faux, id: &str) -> Option<&'a mut Value> {
    if f.playlists_coupees {
        return None;
    }
    f.playlists.iter_mut().find(|p| meme_id(&p["id"], id))
}

fn etag(p: &Value) -> String {
    format!("\"{}\"", p["version"])
}

/// Une `Playlist`, avec son `ETag` (site-mozaiklabs#236).
fn avec_etag(p: &Value) -> Response {
    ([(header::ETAG, etag(p))], Json(p.clone())).into_response()
}

fn conflit(p: &Value) -> Response {
    (
        StatusCode::CONFLICT,
        [(header::ETAG, etag(p))],
        Json(json!({ "error": "version_conflict", "playlist": p })),
    )
        .into_response()
}

/// La version envoyée : le corps, sinon `If-Match: "<n>"`.
fn version_recue(f: &mut Faux, v: &Value, h: &HeaderMap) -> Value {
    let if_match = h
        .get(header::IF_MATCH)
        .and_then(|x| x.to_str().ok())
        .map(str::to_string);
    f.if_match_recus.push(if_match.clone());
    if !v["version"].is_null() {
        return v["version"].clone();
    }
    if_match
        .and_then(|m| m.trim_matches('"').parse::<i64>().ok())
        .map_or(Value::Null, Value::from)
}

async fn lister_playlists(State(e): State<Partage>, h: HeaderMap) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let f = e.lock().unwrap();
    if f.playlists_coupees {
        return Json(json!([])).into_response();
    }
    Json(json!(f.playlists.iter().map(resume).collect::<Vec<_>>())).into_response()
}

async fn creer_playlist(State(e): State<Partage>, h: HeaderMap, corps: Bytes) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let v: Value = serde_json::from_slice(&corps).unwrap_or(Value::Null);
    let mut f = e.lock().unwrap();
    f.ecritures_de_playlist
        .push(("POST /playlists".into(), v.clone()));
    if !f.circles.iter().any(|c| c["id"] == v["circle_id"]) {
        return introuvable();
    }
    let p = json!({ "id": 6, "name": v["name"], "version": 1,
                    "owner": { "user_id": 1, "name": "Moi" }, "mine": true, "items": [] });
    f.playlists.push(p.clone());
    (StatusCode::CREATED, Json(p)).into_response()
}

async fn lire_playlist(State(e): State<Partage>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let mut f = e.lock().unwrap();
    f.lectures_de_playlist += 1;
    match playlist_de(&mut f, &id) {
        Some(p) => avec_etag(p),
        None => introuvable(),
    }
}

async fn renommer_playlist(
    State(e): State<Partage>,
    h: HeaderMap,
    Path(id): Path<String>,
    corps: Bytes,
) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let v: Value = serde_json::from_slice(&corps).unwrap_or(Value::Null);
    let mut f = e.lock().unwrap();
    f.ecritures_de_playlist
        .push(("PATCH /playlists/{id}".into(), v.clone()));
    let version = version_recue(&mut f, &v, &h);
    let Some(p) = playlist_de(&mut f, &id) else {
        return introuvable();
    };
    if p["version"] != version {
        return conflit(p);
    }
    p["name"] = v["name"].clone();
    p["version"] = json!(p["version"].as_i64().unwrap() + 1);
    avec_etag(p)
}

async fn supprimer_playlist(
    State(e): State<Partage>,
    h: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let mut f = e.lock().unwrap();
    if playlist_de(&mut f, &id).is_none() {
        return introuvable();
    }
    f.playlists.retain(|p| !meme_id(&p["id"], &id));
    Json(json!({ "ok": true })).into_response()
}

async fn ajouter_a_la_playlist(
    State(e): State<Partage>,
    h: HeaderMap,
    Path(id): Path<String>,
    corps: Bytes,
) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let v: Value = serde_json::from_slice(&corps).unwrap_or(Value::Null);
    let mut f = e.lock().unwrap();
    f.ecritures_de_playlist
        .push(("POST /playlists/{id}/items".into(), v.clone()));
    let version = version_recue(&mut f, &v, &h);
    let Some(p) = playlist_de(&mut f, &id) else {
        return introuvable();
    };
    if p["version"] != version {
        return conflit(p);
    }
    let Some(items) = v["items"].as_array().filter(|i| !i.is_empty()) else {
        return validation("items", "The items field is required.");
    };
    // Liste blanche : une clé inconnue (chemin, URL, source_id…) = 422, rien
    // n'est écrit.
    for item in items {
        let ok = item.as_object().is_some_and(|o| {
            o.keys().all(|k| CLES_DE_REFERENCE.contains(&k.as_str()))
                && o.get("title")
                    .and_then(Value::as_str)
                    .is_some_and(|t| !t.is_empty())
        });
        if !ok {
            return validation("items", "The items field is invalid.");
        }
    }
    let premier = 100 + p["items"].as_array().unwrap().len() as i64;
    for (k, item) in items.iter().enumerate() {
        let mut ligne = item.clone();
        ligne["item_id"] = json!(premier + k as i64);
        ligne["added_by"] = json!({ "user_id": 1, "name": "Moi" });
        ligne["added_at"] = json!("2026-09-28T10:00:00Z");
        p["items"].as_array_mut().unwrap().push(ligne);
    }
    p["version"] = json!(p["version"].as_i64().unwrap() + 1);
    avec_etag(p)
}

async fn retirer_de_la_playlist(
    State(e): State<Partage>,
    h: HeaderMap,
    Path((id, item_id)): Path<(String, String)>,
    RawQuery(requete): RawQuery,
) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let mut f = e.lock().unwrap();
    f.requete_de_retrait = requete.clone();
    let version = requete
        .as_deref()
        .and_then(|q| q.strip_prefix("version="))
        .and_then(|v| v.parse::<i64>().ok());
    let Some(p) = playlist_de(&mut f, &id) else {
        return introuvable();
    };
    if p["version"].as_i64() != version {
        return conflit(p);
    }
    let items = p["items"].as_array_mut().unwrap();
    let Some(i) = position(items, "item_id", &item_id) else {
        return introuvable();
    };
    items.remove(i);
    p["version"] = json!(p["version"].as_i64().unwrap() + 1);
    avec_etag(p)
}

async fn ordonner_la_playlist(
    State(e): State<Partage>,
    h: HeaderMap,
    Path(id): Path<String>,
    corps: Bytes,
) -> Response {
    if let Some(r) = garde(&e, &h) {
        return r;
    }
    let v: Value = serde_json::from_slice(&corps).unwrap_or(Value::Null);
    let mut f = e.lock().unwrap();
    f.ecritures_de_playlist
        .push(("PUT /playlists/{id}/order".into(), v.clone()));
    let version = version_recue(&mut f, &v, &h);
    let Some(p) = playlist_de(&mut f, &id) else {
        return introuvable();
    };
    if p["version"] != version {
        return conflit(p);
    }
    let actuels: Vec<Value> = p["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["item_id"].clone())
        .collect();
    let demandes = v["item_ids"].as_array().cloned().unwrap_or_default();
    let mut a = actuels.iter().map(Value::to_string).collect::<Vec<_>>();
    let mut b = demandes.iter().map(Value::to_string).collect::<Vec<_>>();
    a.sort();
    b.sort();
    if a != b {
        return validation("item_ids", "The item ids must be an exact permutation.");
    }
    let anciens = p["items"].as_array().unwrap().clone();
    let neufs: Vec<Value> = demandes
        .iter()
        .map(|d| anciens.iter().find(|i| i["item_id"] == *d).unwrap().clone())
        .collect();
    p["items"] = json!(neufs);
    p["version"] = json!(p["version"].as_i64().unwrap() + 1);
    avec_etag(p)
}

pub const MESSAGE_SERVER_ID: &str = "The server id field must be a string.";

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
        .route(
            "/api/v1/circle/circles/{id}/sharing/library",
            put(partager).delete(ne_plus_partager),
        )
        .route("/api/v1/circle/shared-with-me", get(partagent_avec_moi))
        .route(
            "/api/v1/circle/contacts/{user_id}/library/{quoi}",
            get(catalogue_de),
        )
        .route(
            "/api/v1/circle/contacts/{user_id}/library/albums/{album_id}/tracks",
            get(pistes_de_l_album),
        )
        .route(
            "/api/v1/circle/playlists",
            get(lister_playlists).post(creer_playlist),
        )
        .route(
            "/api/v1/circle/playlists/{id}",
            get(lire_playlist)
                .patch(renommer_playlist)
                .delete(supprimer_playlist),
        )
        .route(
            "/api/v1/circle/playlists/{id}/items",
            post(ajouter_a_la_playlist),
        )
        .route(
            "/api/v1/circle/playlists/{id}/items/{item_id}",
            delete(retirer_de_la_playlist),
        )
        .route(
            "/api/v1/circle/playlists/{id}/order",
            put(ordonner_la_playlist),
        )
        .route(
            "/api/v1/circle/recoverable-playlists",
            get(lister_recuperables),
        )
        .route(
            "/api/v1/circle/recoverable-playlists/{id}",
            get(lire_recuperable).delete(renoncer_a_la_recuperable),
        )
        .route("/api/v1/cloud-library/{server_id}/sync", post(synchro))
        .route("/api/v1/cloud-library/{server_id}/link", post(lier))
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
    let license = Arc::new(tune_core::license::LicenseManager::new(backend.clone()));
    tune_circle::routes::router(Arc::new(tune_circle::relais::Relais::new(backend)), license)
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
