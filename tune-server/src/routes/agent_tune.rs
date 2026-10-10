//! Routes du rôle maître / agent (#4626).
//!
//! Deux surfaces, et la frontière est voulue :
//!
//! - [`router`], monté sous `/api/v1/agent-tune` : les gestes de
//!   l'UTILISATEUR (émettre un code, appairer, oublier). Elle passe par la
//!   couche d'authentification ordinaire, et les gestes qui écrivent exigent
//!   l'administrateur quand l'authentification est active ;
//! - [`router_entre_serveurs`], monté à la RACINE sous `/agent-tune` : ce
//!   qu'un serveur demande à un autre. Un maître n'a pas de session chez
//!   l'agent ; chaque route y vérifie elle-même le jeton d'appairage
//!   (`x-tune-agent-jeton`), sauf l'annonce (publique, comme
//!   `/system/peer-info`) et l'appairage (gardé par le code).

use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::agent_tune::agent::{self, MaitreAppaire, RefusAppairage};
use crate::agent_tune::{
    AvisDeRevocation, DemandeAppairage, DemandeEtat, ENTETE_JETON, OrdreSortie, identite, maitre,
};
use crate::auth::RequireAdmin;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/agent", get(etat_agent))
        .route("/agent/code", post(emettre_code))
        .route("/agent/maitres/{id}", delete(oublier_maitre))
        .route("/agents", get(liste_agents).post(appairer_agent))
        .route("/agents/{id}", delete(oublier_agent))
}

pub fn router_entre_serveurs() -> Router<AppState> {
    Router::new()
        .route("/annonce", get(annonce))
        .route("/appairer", post(appairer))
        .route("/sorties", get(sorties))
        .route("/sorties/etat", post(etat_sortie))
        .route("/sorties/commande", post(commande_sortie))
        .route("/oublier", post(oublier_par_le_maitre))
        .route("/revocation", post(revocation_par_l_agent))
}

// ---------------------------------------------------------------------------
// Gestes de l'utilisateur
// ---------------------------------------------------------------------------

/// Côté agent : qui je suis, quels maîtres me tiennent, ce que je prête.
async fn etat_agent(State(state): State<AppState>) -> Json<serde_json::Value> {
    let (agent_id, nom) = identite(&state);
    let maitres: Vec<serde_json::Value> = agent::maitres(&state)
        .into_iter()
        .map(|m| json!({ "maitre_id": m.maitre_id, "nom": m.nom, "appaire_le": m.appaire_le }))
        .collect();
    Json(json!({
        "agent_id": agent_id,
        "nom": nom,
        "maitres": maitres,
        "sorties": agent::sorties_exposees(&state).await,
    }))
}

async fn emettre_code(
    _admin: RequireAdmin,
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    let (code, duree) = agent::emettre_code(&state);
    Json(json!({ "code": code, "expire_dans_s": duree }))
}

async fn oublier_maitre(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    if agent::oublier_maitre(&state, &id, true).await {
        // 200 et un corps : le client web lit du JSON (pas de 204).
        Json(json!({ "ok": true })).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "maître inconnu" })),
        )
            .into_response()
    }
}

/// Côté maître : les agents appairés (sans jeton) et les candidats du réseau.
async fn liste_agents(State(state): State<AppState>) -> Json<serde_json::Value> {
    let agents: Vec<serde_json::Value> = {
        let registre = state.outputs.lock().await;
        let ids = registre.list();
        maitre::agents(&state)
            .into_iter()
            .map(|a| {
                let prefixe = format!("{}{}:", crate::agent_tune::PREFIXE_DEVICE_ID, a.agent_id);
                let sorties = ids.iter().filter(|id| id.starts_with(&prefixe)).count();
                let mut v = a.public();
                v["sorties_inscrites"] = json!(sorties);
                v
            })
            .collect()
    };
    let decouverts = state.discovered_tune_peers().await;
    let candidats = maitre::candidats(&state, &decouverts).await;
    Json(json!({ "agents": agents, "candidats": candidats }))
}

#[derive(Deserialize)]
struct CorpsAppairage {
    host: String,
    #[serde(default = "port_par_defaut")]
    port: u16,
    code: String,
}

fn port_par_defaut() -> u16 {
    8888
}

async fn appairer_agent(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(corps): Json<CorpsAppairage>,
) -> Response {
    let host = corps.host.trim();
    if host.is_empty() || corps.code.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "hôte et code d'appairage requis" })),
        )
            .into_response();
    }
    match maitre::appairer(&state, host, corps.port, corps.code.trim()).await {
        Ok((agent, zones)) => (
            StatusCode::CREATED,
            Json(json!({ "agent": agent.public(), "zones": zones })),
        )
            .into_response(),
        Err(motif) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": motif }))).into_response(),
    }
}

async fn oublier_agent(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    if maitre::oublier(&state, &id).await {
        // 200 et un corps : le client web lit du JSON (pas de 204).
        Json(json!({ "ok": true })).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "agent inconnu" })),
        )
            .into_response()
    }
}

// ---------------------------------------------------------------------------
// Entre serveurs
// ---------------------------------------------------------------------------

/// Le maître qui présente le jeton, ou un 401.
fn maitre_authentifie(state: &AppState, headers: &HeaderMap) -> Result<MaitreAppaire, Response> {
    let jeton = headers
        .get(ENTETE_JETON)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    agent::maitre_du_jeton(state, jeton).ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            "jeton d'appairage absent ou révoqué",
        )
            .into_response()
    })
}

/// Publique : ce serveur sait être agent. Même surface que
/// `/system/peer-info` — ni sortie, ni adresse, ni réglage.
async fn annonce(State(state): State<AppState>) -> Json<serde_json::Value> {
    let (agent_id, nom) = identite(&state);
    Json(json!({
        "agent_id": agent_id,
        "nom": nom,
        "version": tune_core::version(),
        "appairage": "code",
    }))
}

/// Taille maximale d'une demande d'appairage (quelques champs courts).
const CORPS_APPAIRAGE_MAX: usize = 16 * 1024;

async fn appairer(State(state): State<AppState>, requete: Request) -> Response {
    // L'adresse d'où vient la demande : c'est là que l'agent préviendra le
    // maître d'une révocation. Lue dans les extensions plutôt que par
    // l'extracteur `ConnectInfo`, qui rendrait une erreur 500 à un routeur
    // servi sans elle.
    let ip_du_maitre = requete
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map(|ConnectInfo(pair)| pair.ip());
    let demande: DemandeAppairage =
        match axum::body::to_bytes(requete.into_body(), CORPS_APPAIRAGE_MAX)
            .await
            .ok()
            .and_then(|octets| serde_json::from_slice(&octets).ok())
        {
            Some(d) => d,
            None => {
                return (StatusCode::BAD_REQUEST, "demande d'appairage illisible").into_response();
            }
        };
    match agent::appairer(&state, &demande, ip_du_maitre).await {
        Ok(reponse) => Json(reponse).into_response(),
        Err(refus) => {
            let code = match refus {
                RefusAppairage::SoiMeme => StatusCode::BAD_REQUEST,
                RefusAppairage::Epuise | RefusAppairage::Expire => StatusCode::GONE,
                RefusAppairage::AucunCode | RefusAppairage::Mauvais => StatusCode::FORBIDDEN,
            };
            tracing::info!(motif = refus.motif(), "agent_tune_appairage_refuse");
            (code, refus.motif()).into_response()
        }
    }
}

async fn sorties(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = maitre_authentifie(&state, &headers) {
        return r;
    }
    Json(agent::sorties_exposees(&state).await).into_response()
}

async fn etat_sortie(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(demande): Json<DemandeEtat>,
) -> Response {
    let m = match maitre_authentifie(&state, &headers) {
        Ok(m) => m,
        Err(r) => return r,
    };
    match agent::etat(&state, &m.maitre_id, &demande.device_id).await {
        Ok(etat) => Json(etat).into_response(),
        Err((code, motif)) => (code, motif).into_response(),
    }
}

async fn commande_sortie(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(ordre): Json<OrdreSortie>,
) -> Response {
    let m = match maitre_authentifie(&state, &headers) {
        Ok(m) => m,
        Err(r) => return r,
    };
    match agent::executer(&state, &m.maitre_id, &ordre.device_id, ordre.commande).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err((code, motif)) => (code, motif).into_response(),
    }
}

/// Le maître se retire lui-même (il a oublié cet agent).
async fn oublier_par_le_maitre(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let m = match maitre_authentifie(&state, &headers) {
        Ok(m) => m,
        Err(r) => return r,
    };
    agent::oublier_maitre(&state, &m.maitre_id, false).await;
    StatusCode::NO_CONTENT.into_response()
}

/// Côté maître : un agent annonce qu'il a révoqué l'appairage. Rien n'est
/// cru sur parole — le maître vérifie auprès de l'agent que son jeton y est
/// refusé ([`maitre::constater_revocation`]). La réponse est la même dans
/// tous les cas : elle n'apprend rien à qui l'appelle.
async fn revocation_par_l_agent(
    State(state): State<AppState>,
    Json(avis): Json<AvisDeRevocation>,
) -> Response {
    maitre::constater_revocation(&state, &avis.agent_id).await;
    StatusCode::NO_CONTENT.into_response()
}
