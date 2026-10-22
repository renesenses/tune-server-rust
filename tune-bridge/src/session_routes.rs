//! Routes d'ouverture de session par laissez-passer (#6603).
//!
//! - `GET /session/defi/{server_id}` : un défi à usage unique, 60 s ;
//! - `POST /session/{server_id}` : `{laissez_passer, defi, signature}` →
//!   `{jeton, expire, secours}` ;
//! - `POST /session/renouveler` : même corps, jeton de session en
//!   `X-Bridge-Token` ; le jeton ne change pas.
//!
//! Les refus sont du JSON `{code, motif?}` : voir `crate::sessions::Rejet`.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};

use crate::sessions::{CorpsSession, Rejet};
use crate::state::RelayState;

fn serveur_absent() -> Response {
    Rejet::nouveau(404, "serveur_absent").en_reponse()
}

pub async fn defi(State(state): State<Arc<RelayState>>, Path(server_id): Path<String>) -> Response {
    if !state.servers.contains_key(&server_id) {
        return serveur_absent();
    }
    match state.sessions.nouveau_defi(&server_id) {
        Ok((defi, expire)) => {
            Json(serde_json::json!({"defi": defi, "expire": expire})).into_response()
        }
        Err(rejet) => rejet.en_reponse(),
    }
}

pub async fn ouvrir(
    State(state): State<Arc<RelayState>>,
    Path(server_id): Path<String>,
    Json(corps): Json<CorpsSession>,
) -> Response {
    // La clé est lue puis le garde relâché : rien n'est tenu à travers
    // l'appel au site.
    let cle = match state.servers.get(&server_id) {
        Some(c) => c.cle_serveur.clone(),
        None => return serveur_absent(),
    };
    match state
        .sessions
        .ouvrir(&server_id, cle.as_deref(), &corps)
        .await
    {
        Ok(o) => Json(o).into_response(),
        Err(rejet) => rejet.en_reponse(),
    }
}

pub async fn renouveler(
    State(state): State<Arc<RelayState>>,
    headers: HeaderMap,
    Json(corps): Json<CorpsSession>,
) -> Response {
    let Some(jeton) = crate::api_proxy::extraire_jeton(&headers) else {
        return Rejet::non_autorise().en_reponse();
    };
    match state.sessions.renouveler(&jeton, &corps).await {
        Ok(o) => Json(o).into_response(),
        Err(rejet) => rejet.en_reponse(),
    }
}
