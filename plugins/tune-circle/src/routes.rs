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
//! T2, le catalogue d'un contact en lecture (#5325). Le cloud porte le droit
//! et la projection ; le greffon relaie, requête comprise, et ne garde rien :
//!
//! | Tune (`/api/v1/ext/circle`)                            | mozaiklabs (`/api/v1/circle`)          |
//! |--------------------------------------------------------|----------------------------------------|
//! | `PUT /circles/{id}/sharing/library` (sans corps)       | idem, `{ "server_id" }` du RÉGLAGE     |
//! | `DELETE /circles/{id}/sharing/library`                 | idem                                   |
//! | `GET /shared-with-me`                                  | idem                                   |
//! | `GET /contacts/{user_id}/library/stats`                | idem                                   |
//! | `GET /contacts/{user_id}/library/artists?…`            | idem, requête comprise                 |
//! | `GET /contacts/{user_id}/library/albums?…`             | idem, requête comprise                 |
//! | `GET /contacts/{user_id}/library/albums/{album_id}/tracks` | idem                               |
//! | `GET /contacts/{user_id}/library/tracks?…`             | idem, requête comprise                 |
//! | `GET /library-sync`                                    | — (état LOCAL de la copie en ligne)    |
//!
//! Le `server_id` partagé est celui de CE serveur (réglage `server_id`, celui
//! que pousse `library_sync`) : un `server_id` fourni par le client ne part
//! jamais. Le cloud juge s'il appartient à l'appelant (404 sinon).
//!
//! Chaque relais de `GET /`, `PUT` ou `DELETE …/sharing/library` et `DELETE
//! /circles/{id}` tient à jour UN booléen local,
//! [`library_sync::CLE_PARTAGE_DE_CERCLE`] : « ce serveur partage sa
//! bibliothèque avec au moins un cercle ». C'est lui qui fait pousser la copie
//! en ligne d'un compte gratuit (décision du 28/09). Rien du cercle n'est
//! gardé, et aucune lecture n'est servie de mémoire.
//!
//! ## Les états
//!
//! * **Réponse du cloud** (2xx, 4xx — dont 404, 409, 422, 429) : statut et
//!   corps relayés à l'octet près, `Retry-After` compris.
//! * **Non connecté** (aucune session SSO, ou session que le cloud refuse
//!   encore en 401 après UN rafraîchissement) : `GET /` rend `200 {
//!   "connected": false }` ; toute autre route rend `412 { "connected": false,
//!   "code": "circle.not_connected" }`. Le greffon ne rend JAMAIS 401 : pour
//!   le client web, un 401 est la fin de la session Tune elle-même.
//! * **Cloud indisponible** (injoignable, délai, 5xx) : `503 { "connected":
//!   true, "code": "circle.cloud_unavailable", "upstream_status": 500|null }`.
//! * Refus local, sans appel : `404 circle.not_found` (identifiant vide, `.`
//!   ou `..`). La validité de l'adresse, elle, est jugée par le cloud seul
//!   (`422 self_invitation`, adresse invalide ; `409 already_member`,
//!   `already_invited`, `invitation_received`) et relayée avec son corps.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, RawQuery, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use reqwest::Method;
use serde_json::{Value, json};
use tune_core::cloud::library_sync;
use tune_core::license::LicenseManager;

use crate::relais::{Issue, Relais};

pub const CODE_NON_CONNECTE: &str = "circle.not_connected";
pub const CODE_CLOUD_INDISPONIBLE: &str = "circle.cloud_unavailable";
pub const CODE_INTROUVABLE: &str = "circle.not_found";
pub const CODE_REPONSE_ILLISIBLE: &str = "circle.unreadable_response";
/// T2 (#5325) : ce serveur n'est pas encore lié au compte — le cloud refuse de
/// partager sa bibliothèque tant que la liaison n'est pas faite.
pub const CODE_SERVEUR_NON_LIE: &str = "circle.server_not_linked";

pub fn router(relais: Arc<Relais>, license: Arc<LicenseManager>) -> Router<()> {
    let etat_de_la_copie = Router::new()
        .route("/library-sync", get(etat_de_la_copie_en_ligne))
        .with_state(Arc::new(EtatDeLaCopie {
            relais: relais.clone(),
            license,
        }));
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
        // T2 (#5325)
        .route(
            "/circles/{id}/sharing/library",
            put(partager).delete(ne_plus_partager),
        )
        .route("/shared-with-me", get(partage_avec_moi))
        .route("/contacts/{user_id}/library/stats", get(bibliotheque_stats))
        .route(
            "/contacts/{user_id}/library/artists",
            get(bibliotheque_artistes),
        )
        .route(
            "/contacts/{user_id}/library/albums",
            get(bibliotheque_albums),
        )
        .route(
            "/contacts/{user_id}/library/albums/{album_id}/tracks",
            get(bibliotheque_pistes_de_l_album),
        )
        .route(
            "/contacts/{user_id}/library/tracks",
            get(bibliotheque_pistes),
        )
        .with_state(relais)
        .merge(etat_de_la_copie)
}

pub(crate) fn refus(statut: StatusCode, corps: Value) -> Response {
    (statut, Json(corps)).into_response()
}

/// La forme HTTP d'une [`Issue`].
pub(crate) fn en_reponse(issue: Issue) -> Response {
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
            etag,
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
            if let Some(v) = etag {
                reponse.headers_mut().insert(header::ETAG, v);
            }
            reponse
        }
    }
}

/// Un identifiant de chemin qui désigne bien UNE ressource.
pub(crate) fn identifiant_valide(id: &str) -> bool {
    !id.is_empty() && id != "." && id != ".." && id.len() <= 200
}

pub(crate) fn introuvable() -> Response {
    refus(StatusCode::NOT_FOUND, json!({ "code": CODE_INTROUVABLE }))
}

async fn cercle(State(relais): State<Arc<Relais>>) -> Response {
    match relais.appeler("GET /", Method::GET, &[], None).await {
        // Seule route où « non connecté » est un état et non un refus : c'est
        // elle que l'écran interroge pour savoir quoi afficher.
        Issue::NonConnecte => Json(json!({ "connected": false })).into_response(),
        issue => {
            noter_le_partage(&relais, &issue);
            en_reponse(issue)
        }
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
    let issue = relais
        .appeler(
            "DELETE /circles/{id}",
            Method::DELETE,
            &["circles", &id],
            None,
        )
        .await;
    // Supprimer un cercle supprime son partage (cascade côté cloud).
    if reussie(&issue) {
        relire_le_partage(&relais).await;
    }
    en_reponse(issue)
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

// T2 : le catalogue d'un contact, en lecture (#5325) -------------------------

fn reussie(issue: &Issue) -> bool {
    matches!(issue, Issue::Reponse { statut, .. } if (200..300).contains(statut))
}

/// Le `server_id` de CE serveur, `None` s'il n'en a pas.
fn server_id_du_serveur(relais: &Relais) -> Option<String> {
    relais
        .reglages()
        .get("server_id")
        .ok()
        .flatten()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn ecrire_le_partage(relais: &Relais, actif: bool) {
    relais
        .reglages()
        .set(
            library_sync::CLE_PARTAGE_DE_CERCLE,
            if actif { "true" } else { "false" },
        )
        .ok();
}

/// Lit, dans un `GET /` réussi du cloud, si un cercle partage la bibliothèque
/// de CE serveur, et le note. Tout autre résultat (panne, 404, corps
/// illisible) ne change rien : un doute ne coupe ni n'allume la poussée.
fn noter_le_partage(relais: &Relais, issue: &Issue) {
    let Issue::Reponse {
        statut: 200, corps, ..
    } = issue
    else {
        return;
    };
    let Ok(liste) = serde_json::from_slice::<Value>(corps) else {
        return;
    };
    if !liste.is_object() {
        return;
    }
    let moi = server_id_du_serveur(relais);
    let actif = moi.is_some_and(|moi| {
        liste["circles"].as_array().is_some_and(|cercles| {
            cercles.iter().any(|c| {
                c["sharing"]["library"] == Value::Bool(true)
                    && c["sharing"]["server_id"].as_str() == Some(moi.as_str())
            })
        })
    });
    ecrire_le_partage(relais, actif);
}

/// Après une coupure, un AUTRE cercle peut encore partager ce serveur : seul
/// le cloud le sait. Une relecture de `GET /`, et rien n'est servi d'elle.
async fn relire_le_partage(relais: &Relais) {
    let issue = relais.appeler("GET /", Method::GET, &[], None).await;
    noter_le_partage(relais, &issue);
}

/// `PUT /circles/{id}/sharing/library`, sans corps : le greffon joint SON
/// `server_id`. Ce que le client enverrait est ignoré — il ne choisit pas le
/// serveur partagé. Sans `server_id` local, `null` part et le cloud répond 422.
/// Les 404 du cloud deviennent `circle.server_not_linked` (serveur pas encore
/// lié au compte) ou `circle.not_found` (cercle d'un autre).
async fn partager(State(relais): State<Arc<Relais>>, Path(id): Path<String>) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    let envoi = json!({ "server_id": server_id_du_serveur(&relais) });
    let issue = relais
        .appeler(
            "PUT /circles/{id}/sharing/library",
            Method::PUT,
            &["circles", &id, "sharing", "library"],
            Some(&envoi),
        )
        .await;
    if reussie(&issue) {
        ecrire_le_partage(&relais, true);
        // Une bibliothèque jamais poussée part entière au prochain cycle —
        // sans quoi les contacts parcourraient un catalogue vide ou troué.
        let jamais_poussee = relais
            .reglages()
            .get("cloud_library_last_sync")
            .ok()
            .flatten()
            .is_none_or(|v| v.trim().is_empty());
        if jamais_poussee {
            library_sync::populate_changelog_after_scan(relais.backend());
        }
    }
    // Contrat de site-mozaiklabs#233 : les deux 404 du PUT deviennent des
    // codes nommés, que l'écran lit (le statut, lui, ne les distingue pas).
    if let Issue::Reponse {
        statut: 404, corps, ..
    } = &issue
    {
        let motif = serde_json::from_slice::<Value>(corps)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string));
        match motif.as_deref() {
            Some("server_not_linked") => {
                return refus(
                    StatusCode::NOT_FOUND,
                    json!({ "code": CODE_SERVEUR_NON_LIE }),
                );
            }
            Some("not_found") => return introuvable(),
            _ => {}
        }
    }
    en_reponse(issue)
}

async fn ne_plus_partager(State(relais): State<Arc<Relais>>, Path(id): Path<String>) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    let issue = relais
        .appeler(
            "DELETE /circles/{id}/sharing/library",
            Method::DELETE,
            &["circles", &id, "sharing", "library"],
            None,
        )
        .await;
    if reussie(&issue) {
        relire_le_partage(&relais).await;
    }
    en_reponse(issue)
}

async fn partage_avec_moi(State(relais): State<Arc<Relais>>) -> Response {
    en_reponse(
        relais
            .appeler(
                "GET /shared-with-me",
                Method::GET,
                &["shared-with-me"],
                None,
            )
            .await,
    )
}

/// Une lecture de la bibliothèque d'un contact : relais fidèle, requête
/// comprise, et rien de gardé. Un 404 du cloud (partage coupé, contact
/// révoqué ou non rangé) repart tel quel, au premier appel qui suit.
async fn lire_la_bibliotheque(
    relais: &Relais,
    route: &'static str,
    segments: &[&str],
    requete: Option<String>,
) -> Response {
    if segments.iter().any(|s| !identifiant_valide(s)) {
        return introuvable();
    }
    en_reponse(
        relais
            .appeler_avec_requete(route, Method::GET, segments, requete.as_deref(), None)
            .await,
    )
}

async fn bibliotheque_stats(
    State(relais): State<Arc<Relais>>,
    Path(user_id): Path<String>,
    RawQuery(requete): RawQuery,
) -> Response {
    lire_la_bibliotheque(
        &relais,
        "GET /contacts/{user_id}/library/stats",
        &["contacts", &user_id, "library", "stats"],
        requete,
    )
    .await
}

async fn bibliotheque_artistes(
    State(relais): State<Arc<Relais>>,
    Path(user_id): Path<String>,
    RawQuery(requete): RawQuery,
) -> Response {
    lire_la_bibliotheque(
        &relais,
        "GET /contacts/{user_id}/library/artists",
        &["contacts", &user_id, "library", "artists"],
        requete,
    )
    .await
}

async fn bibliotheque_albums(
    State(relais): State<Arc<Relais>>,
    Path(user_id): Path<String>,
    RawQuery(requete): RawQuery,
) -> Response {
    lire_la_bibliotheque(
        &relais,
        "GET /contacts/{user_id}/library/albums",
        &["contacts", &user_id, "library", "albums"],
        requete,
    )
    .await
}

async fn bibliotheque_pistes_de_l_album(
    State(relais): State<Arc<Relais>>,
    Path((user_id, album_id)): Path<(String, String)>,
    RawQuery(requete): RawQuery,
) -> Response {
    lire_la_bibliotheque(
        &relais,
        "GET /contacts/{user_id}/library/albums/{album_id}/tracks",
        &[
            "contacts", &user_id, "library", "albums", &album_id, "tracks",
        ],
        requete,
    )
    .await
}

async fn bibliotheque_pistes(
    State(relais): State<Arc<Relais>>,
    Path(user_id): Path<String>,
    RawQuery(requete): RawQuery,
) -> Response {
    lire_la_bibliotheque(
        &relais,
        "GET /contacts/{user_id}/library/tracks",
        &["contacts", &user_id, "library", "tracks"],
        requete,
    )
    .await
}

/// L'état de `/library-sync` : le relais pour ses réglages, la licence pour
/// dire Premium.
pub struct EtatDeLaCopie {
    relais: Arc<Relais>,
    license: Arc<LicenseManager>,
}

/// `last_sync` sous une seule forme : la route de synchro manuelle écrit des
/// secondes depuis l'époque, la tâche périodique du RFC 3339.
fn date_de_synchro(brute: Option<String>) -> Option<String> {
    let brute = brute
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())?;
    match brute.parse::<i64>() {
        Ok(secondes) => chrono::DateTime::from_timestamp(secondes, 0).map(|d| d.to_rfc3339()),
        Err(_) => Some(brute),
    }
}

/// `GET /library-sync` : l'état LOCAL de la copie en ligne, pour l'écran du
/// propriétaire. Sans lui, il croirait partager un catalogue vide ou vieux.
///
/// `{ server_id, premium, active, last_sync, pending }`. `active` : la
/// synchronisation périodique pousse pour ce serveur — Premium ou partage de
/// cercle actif, une session SSO et un `server_id`. Aucun appel au cloud.
async fn etat_de_la_copie_en_ligne(State(etat): State<Arc<EtatDeLaCopie>>) -> Response {
    let reglages = etat.relais.reglages();
    let premium = etat.license.is_premium().await;
    let session = reglages
        .get("mozaik_access_token")
        .ok()
        .flatten()
        .is_some_and(|v| !v.trim().is_empty());
    let server_id = server_id_du_serveur(&etat.relais);
    let active =
        library_sync::synchro_autorisee(premium, &reglages) && session && server_id.is_some();
    Json(json!({
        // Pour que l'écran sache si le partage d'un cercle (`sharing.server_id`
        // de `GET /`) vient de CE serveur.
        "server_id": server_id,
        "premium": premium,
        "active": active,
        "last_sync": date_de_synchro(reglages.get("cloud_library_last_sync").ok().flatten()),
        "pending": library_sync::pending_count(etat.relais.backend()),
    }))
    .into_response()
}
