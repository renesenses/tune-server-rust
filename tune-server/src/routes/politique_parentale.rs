//! Contrôle parental, étape 1 (#7856) : routes de la politique d'un profil et
//! filtrage des listes servies à un profil supervisé.
//!
//! `GET|PUT|DELETE /profiles/{id}/politique-parentale`, réservées au
//! superviseur de la politique et aux administrateurs ; le profil supervisé
//! n'y a aucun droit (403). Une écriture porte la révision lue : une révision
//! périmée est refusée (409). La règle d'accès est
//! [`tune_core::db::politique_parentale::peut_gerer`].
//!
//! Un profil administrateur ne se place pas sous supervision : poser sa
//! politique est refusé (422, `profil_administrateur_non_supervisable`).
//!
//! Le filtrage est une couche posée sur l'API : pour les seules routes de
//! listes (bibliothèque, recherche, radios, podcasts) et un profil qui a une
//! politique, la réponse est relue et les éléments bloqués en sont retirés.
//! Chaque GET d'une de ces routes coûte une lecture de la politique du profil
//! (une requête par clé primaire) ; sans politique, le corps de la réponse
//! passe tel quel, sans être relu. Si la table manque (création échouée au
//! démarrage), aucune politique ne peut exister : la liste passe, et l'écart
//! est journalisé. Une politique présente mais illisible refuse la liste ;
//! sa lecture rend 500, mais son superviseur peut encore la retirer (DELETE)
//! ou la remplacer (PUT), la règle d'accès ne lisant pas son contenu.
//!
//! Le profil appelant est celui d'[`ActiveProfile`] : avec l'authentification
//! active, celui du compte connecté.

use axum::Json;
use axum::body::Body;
use axum::extract::{FromRequestParts, Path, Query, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::politique_parentale::{self as pp, Ecriture, Filtre, Liste, PolitiqueParentale};

use crate::routes::active_profile::ActiveProfile;
use crate::state::AppState;

/// Plafond de la réponse relue pour être filtrée.
const CORPS_MAX: usize = 64 * 1024 * 1024;

fn refus(statut: StatusCode, code: &str) -> Response {
    (statut, Json(json!({ "error": code }))).into_response()
}

fn interne(e: String) -> Response {
    tracing::warn!(error = %e, "politique_parentale_erreur");
    refus(
        StatusCode::INTERNAL_SERVER_ERROR,
        "politique_parentale_indisponible",
    )
}

/// Le profil visé existe, et l'appelant peut gérer sa politique.
fn controler(state: &AppState, appelant: i64, cible: i64) -> Result<(), Response> {
    let existe = tune_core::db::profile_repo::ProfileRepo::with_backend(state.backend.clone())
        .get(cible)
        .map_err(interne)?
        .is_some();
    if !existe {
        return Err(refus(StatusCode::NOT_FOUND, "profil_introuvable"));
    }
    match pp::peut_gerer(&state.backend, appelant, cible) {
        Ok(true) => Ok(()),
        Ok(false) => Err(refus(
            StatusCode::FORBIDDEN,
            "politique_parentale_reservee_au_superviseur",
        )),
        Err(e) => Err(interne(e)),
    }
}

fn corps(p: &pp::PolitiqueEnregistree) -> Value {
    json!({
        "profile_id": p.profile_id,
        "superviseur_id": p.superviseur_id,
        "revision": p.revision,
        "politique": p.politique,
        "maj_le": p.maj_le,
    })
}

pub(crate) async fn lire(
    State(state): State<AppState>,
    ActiveProfile(appelant): ActiveProfile,
    Path(cible): Path<i64>,
) -> Response {
    if let Err(r) = controler(&state, appelant, cible) {
        return r;
    }
    match pp::lire(&state.backend, cible) {
        Ok(Some(p)) => Json(corps(&p)).into_response(),
        Ok(None) => refus(StatusCode::NOT_FOUND, "politique_parentale_absente"),
        Err(e) => interne(e),
    }
}

#[derive(Deserialize)]
pub(crate) struct Ecrire {
    /// 0 pour une création, la révision lue sinon.
    revision: i64,
    politique: PolitiqueParentale,
}

pub(crate) async fn poser(
    State(state): State<AppState>,
    ActiveProfile(appelant): ActiveProfile,
    Path(cible): Path<i64>,
    Json(demande): Json<Ecrire>,
) -> Response {
    if let Err(r) = controler(&state, appelant, cible) {
        return r;
    }
    match pp::est_administrateur(&state.backend, cible) {
        Ok(false) => {}
        Ok(true) => {
            return refus(
                StatusCode::UNPROCESSABLE_ENTITY,
                "profil_administrateur_non_supervisable",
            );
        }
        Err(e) => return interne(e),
    }
    if let Err(code) = demande.politique.valider() {
        return refus(StatusCode::UNPROCESSABLE_ENTITY, code);
    }
    match pp::poser(
        &state.backend,
        cible,
        appelant,
        demande.revision,
        &demande.politique,
    ) {
        Ok(Ecriture::Faite(_)) => match pp::lire(&state.backend, cible) {
            Ok(Some(p)) => Json(corps(&p)).into_response(),
            Ok(None) => refus(StatusCode::CONFLICT, "revision_perimee"),
            Err(e) => interne(e),
        },
        Ok(Ecriture::Conflit) => refus(StatusCode::CONFLICT, "revision_perimee"),
        Err(e) => interne(e),
    }
}

#[derive(Deserialize)]
pub(crate) struct Retrait {
    revision: i64,
}

pub(crate) async fn retirer(
    State(state): State<AppState>,
    ActiveProfile(appelant): ActiveProfile,
    Path(cible): Path<i64>,
    Query(q): Query<Retrait>,
) -> Response {
    if let Err(r) = controler(&state, appelant, cible) {
        return r;
    }
    match pp::retirer(&state.backend, cible, q.revision) {
        Ok(Ecriture::Faite(_)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Ecriture::Conflit) => refus(StatusCode::CONFLICT, "revision_perimee"),
        Err(e) => interne(e),
    }
}

/// Les routes de listes filtrées, et ce que leurs éléments sont.
fn liste_de_la_route(chemin: &str) -> Option<Liste> {
    let chemin = chemin.strip_prefix("/api/v1").unwrap_or(chemin);
    let chemin = chemin.trim_end_matches('/');
    match chemin {
        "/library/albums" | "/library/tracks" | "/library/search" | "/search" => {
            Some(Liste::Bibliotheque)
        }
        "/library/artists" => Some(Liste::Artistes),
        "/radios" | "/radios/search" | "/radios/favorites" | "/radios/directory" => {
            Some(Liste::Radios)
        }
        "/podcasts/subscriptions"
        | "/podcasts/search"
        | "/podcasts/discover"
        | "/podcasts/top"
        | "/podcasts/catalogue" => Some(Liste::Podcasts),
        _ => None,
    }
}

/// La couche de filtrage des listes (voir l'en-tête du module).
pub(crate) async fn filtrer_les_listes(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let Some(liste) = (req.method() == Method::GET)
        .then(|| liste_de_la_route(req.uri().path()))
        .flatten()
    else {
        return next.run(req).await;
    };
    let (mut parts, body) = req.into_parts();
    let ActiveProfile(profil) = match ActiveProfile::from_request_parts(&mut parts, &state).await {
        Ok(p) => p,
        Err(never) => match never {},
    };
    let req = Request::from_parts(parts, body);
    let politique = match pp::lire(&state.backend, profil) {
        Ok(Some(p)) => p.politique,
        Ok(None) => return next.run(req).await,
        // Sans table, aucune politique n'a pu être posée.
        Err(e) if pp::table_absente(&e) => {
            tracing::warn!(error = %e, "politique_parentale_table_absente");
            return next.run(req).await;
        }
        // Une politique illisible ne doit pas laisser passer la liste entière.
        Err(e) => return interne(e),
    };
    let reponse = next.run(req).await;
    let est_json = reponse
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if !reponse.status().is_success() || !est_json {
        return reponse;
    }
    let (mut parts, body) = reponse.into_parts();
    let octets = match axum::body::to_bytes(body, CORPS_MAX).await {
        Ok(o) => o,
        Err(e) => return interne(e.to_string()),
    };
    let Ok(mut valeur) = serde_json::from_slice::<Value>(&octets) else {
        return Response::from_parts(parts, Body::from(octets));
    };
    let retires = Filtre::depuis(&politique).filtrer(&mut valeur, liste);
    if retires == 0 {
        return Response::from_parts(parts, Body::from(octets));
    }
    tracing::debug!(profil, retires, "politique_parentale_liste_filtree");
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(valeur.to_string()))
}
