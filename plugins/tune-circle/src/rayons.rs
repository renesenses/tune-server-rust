//! Les routes des rayons (Tune Circle T3, #5326), montées avec celles de
//! [`crate::routes`] sous `/api/v1/ext/circle`.
//!
//! | Tune (`/api/v1/ext/circle`)                          | mozaiklabs (`/api/v1/circle`)        |
//! |------------------------------------------------------|--------------------------------------|
//! | `GET /circles/{id}/sets`                             | idem, ENRICHI de la liste locale     |
//! | `PUT /circles/{id}/sets/{kind}/{source_id}` (sans corps) | idem, membres résolus ICI        |
//! | `DELETE /circles/{id}/sets/{kind}/{source_id}`       | idem                                 |
//! | `GET /contacts/{user_id}/sets`                       | idem                                 |
//! | `GET /contacts/{user_id}/sets/{set_id}`              | idem                                 |
//! | `GET /contacts/{user_id}/sets/{set_id}/albums\|tracks\|artists\|streaming` | idem, requête comprise |
//!
//! * `GET /circles/{id}/sets` rend `{ "tags": […], "smart_collections": […] }`,
//!   chaque élément `{ kind, source_id, name, count, shared }` : les étiquettes
//!   et les collections LOCALES, et si CE cercle les voit (le cloud en juge).
//!   `count` : nombre d'éléments partageables d'une étiquette ; pour une
//!   collection, celui du dernier envoi si elle est cochée, `null` sinon (la
//!   résoudre pour la compter coûterait une requête — ou un appel de
//!   catalogue — par collection et par affichage).
//! * `PUT` : le CLIENT n'envoie rien. Un corps fourni est ignoré, octet pour
//!   octet : c'est le greffon qui résout l'ensemble, avec le profil actif de
//!   la requête (celui qui coche), et qui joint le `server_id` de ce serveur.
//! * Le reste est un relais fidèle, sans rien garder : un contact révoqué,
//!   retiré du cercle, un ensemble décoché ou un partage coupé rendent le 404
//!   du cloud dès l'appel suivant.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use reqwest::Method;
use serde_json::{Value, json};

use crate::battement::{EnsembleConnu, Pousseur};
use crate::ensembles::{self, GENRE_COLLECTION, GENRE_ETIQUETTE};
use crate::relais::Issue;
use crate::routes::{en_reponse, identifiant_valide, introuvable, reussie, server_id_du_serveur};

pub fn router(pousseur: Arc<Pousseur>) -> Router<()> {
    Router::new()
        .route("/circles/{id}/sets", get(rayons_du_cercle))
        .route(
            "/circles/{id}/sets/{kind}/{source_id}",
            axum::routing::put(cocher).delete(decocher),
        )
        .route("/contacts/{user_id}/sets", get(rayons_du_contact))
        .route("/contacts/{user_id}/sets/{set_id}", get(un_rayon))
        .route(
            "/contacts/{user_id}/sets/{set_id}/{quoi}",
            get(membres_du_rayon),
        )
        .with_state(pousseur)
}

/// `source_id` : un entier strictement positif, celui de la base locale.
fn identifiant_local(v: &str) -> Option<i64> {
    v.trim().parse::<i64>().ok().filter(|i| *i > 0)
}

fn entier(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str()?.trim().parse().ok())
}

async fn rayons_du_cercle(
    State(pousseur): State<Arc<Pousseur>>,
    Path(id): Path<String>,
) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    let relais = pousseur.relais();
    let issue = relais
        .appeler(
            "GET /circles/{id}/sets",
            Method::GET,
            &["circles", &id, "sets"],
            None,
        )
        .await;
    let Issue::Reponse {
        statut: 200, corps, ..
    } = &issue
    else {
        return en_reponse(issue);
    };
    let Ok(v) = serde_json::from_slice::<Value>(corps) else {
        return en_reponse(issue);
    };
    let Some(partages) = v
        .as_array()
        .or_else(|| v.get("data").and_then(Value::as_array))
    else {
        return en_reponse(issue);
    };
    // (kind, source_id) → le nombre d'éléments du dernier envoi.
    let coche = |kind: &str, source_id: i64| -> Option<Value> {
        partages
            .iter()
            .find(|s| {
                s["kind"].as_str() == Some(kind) && entier(&s["source_id"]) == Some(source_id)
            })
            .map(|s| s.get("count").cloned().unwrap_or(Value::Null))
    };
    let backend = relais.backend();
    let tags: Vec<Value> = ensembles::etiquettes_locales(backend)
        .into_iter()
        .map(|(source_id, name, count)| {
            json!({
                "kind": GENRE_ETIQUETTE, "source_id": source_id, "name": name,
                "count": count, "shared": coche(GENRE_ETIQUETTE, source_id).is_some(),
            })
        })
        .collect();
    let smart: Vec<Value> = ensembles::collections_locales(backend)
        .into_iter()
        .map(|(source_id, name)| {
            let envoi = coche(GENRE_COLLECTION, source_id);
            json!({
                "kind": GENRE_COLLECTION, "source_id": source_id, "name": name,
                "count": envoi.clone().unwrap_or(Value::Null), "shared": envoi.is_some(),
            })
        })
        .collect();
    Json(json!({ "tags": tags, "smart_collections": smart })).into_response()
}

/// Cocher : résoudre ICI, puis relayer. Le corps du client n'est jamais lu.
async fn cocher(
    State(pousseur): State<Arc<Pousseur>>,
    Path((id, kind, source)): Path<(String, String, String)>,
    mut parts: Parts,
) -> Response {
    let (Some(circle_id), Some(source_id)) = (identifiant_local(&id), identifiant_local(&source))
    else {
        return introuvable();
    };
    if !ensembles::genre_valide(&kind) {
        return introuvable();
    }
    let relais = pousseur.relais();
    let profil = pousseur.hote().profil_actif(&mut parts).await;
    let membres = match ensembles::resoudre(
        relais.backend(),
        &**pousseur.hote(),
        &kind,
        source_id,
        profil,
    )
    .await
    {
        Ok(Some(m)) => m,
        // L'étiquette ou la collection n'existe pas ici : rien ne part.
        Ok(None) => return introuvable(),
        Err(_) => {
            return crate::routes::refus(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "code": CODE_NON_RESOLU }),
            );
        }
    };
    let server_id = server_id_du_serveur(relais);
    let corps = ensembles::corps_du_partage(&membres, server_id.as_deref(), Some(profil));
    let digest = corps["digest"].as_str().map(str::to_string);
    let circle = circle_id.to_string();
    let source = source_id.to_string();
    let issue = relais
        .appeler(
            "PUT /circles/{id}/sets/{kind}/{source_id}",
            Method::PUT,
            &["circles", &circle, "sets", &kind, &source],
            Some(&corps),
        )
        .await;
    if reussie(&issue) {
        pousseur.noter(EnsembleConnu {
            circle_id,
            kind: kind.clone(),
            source_id,
            profile_id: Some(profil),
            digest,
            definition: (kind == GENRE_COLLECTION)
                .then(|| ensembles::definition_de_collection(relais.backend(), source_id))
                .flatten(),
        });
    }
    en_reponse(issue)
}

/// Le motif d'une résolution locale en échec (base, règle refusée).
pub const CODE_NON_RESOLU: &str = "circle.set_unresolved";

async fn decocher(
    State(pousseur): State<Arc<Pousseur>>,
    Path((id, kind, source)): Path<(String, String, String)>,
) -> Response {
    let (Some(circle_id), Some(source_id)) = (identifiant_local(&id), identifiant_local(&source))
    else {
        return introuvable();
    };
    if !ensembles::genre_valide(&kind) {
        return introuvable();
    }
    let circle = circle_id.to_string();
    let source = source_id.to_string();
    let issue = pousseur
        .relais()
        .appeler(
            "DELETE /circles/{id}/sets/{kind}/{source_id}",
            Method::DELETE,
            &["circles", &circle, "sets", &kind, &source],
            None,
        )
        .await;
    if reussie(&issue) {
        pousseur.oublier(circle_id, &kind, source_id);
    }
    en_reponse(issue)
}

async fn rayons_du_contact(
    State(pousseur): State<Arc<Pousseur>>,
    Path(user_id): Path<String>,
    RawQuery(requete): RawQuery,
) -> Response {
    if !identifiant_valide(&user_id) {
        return introuvable();
    }
    en_reponse(
        pousseur
            .relais()
            .appeler_avec_requete(
                "GET /contacts/{user_id}/sets",
                Method::GET,
                &["contacts", &user_id, "sets"],
                requete.as_deref(),
                None,
            )
            .await,
    )
}

async fn un_rayon(
    State(pousseur): State<Arc<Pousseur>>,
    Path((user_id, set_id)): Path<(String, String)>,
) -> Response {
    if !identifiant_valide(&user_id) || !identifiant_valide(&set_id) {
        return introuvable();
    }
    en_reponse(
        pousseur
            .relais()
            .appeler(
                "GET /contacts/{user_id}/sets/{set_id}",
                Method::GET,
                &["contacts", &user_id, "sets", &set_id],
                None,
            )
            .await,
    )
}

async fn membres_du_rayon(
    State(pousseur): State<Arc<Pousseur>>,
    Path((user_id, set_id, quoi)): Path<(String, String, String)>,
    RawQuery(requete): RawQuery,
) -> Response {
    let route: &'static str = match quoi.as_str() {
        "albums" => "GET /contacts/{user_id}/sets/{set_id}/albums",
        "tracks" => "GET /contacts/{user_id}/sets/{set_id}/tracks",
        "artists" => "GET /contacts/{user_id}/sets/{set_id}/artists",
        "streaming" => "GET /contacts/{user_id}/sets/{set_id}/streaming",
        _ => return introuvable(),
    };
    if !identifiant_valide(&user_id) || !identifiant_valide(&set_id) {
        return introuvable();
    }
    en_reponse(
        pousseur
            .relais()
            .appeler_avec_requete(
                route,
                Method::GET,
                &["contacts", &user_id, "sets", &set_id, &quoi],
                requete.as_deref(),
                None,
            )
            .await,
    )
}
