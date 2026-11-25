//! Les icônes UPnP de Tune (#9483) : `/upnp/icons/<nom>` et l'ancien
//! `/icon.png`, annoncés par le MediaServer et par chaque MediaRenderer de
//! zone. À la RACINE, hors de la couche d'authentification — un point de
//! contrôle UPnP n'a pas de session Tune — et toujours montées : une icône
//! publique ne dépend ni de l'interrupteur UPnP, ni d'une zone.

use axum::Router;
use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tune_core::upnp_icones::{ANCIENNE_ICONE, PREFIXE_ICONES, icone, icone_au_chemin};

fn reponse(icone: Option<&'static tune_core::upnp_icones::Icone>) -> Response {
    match icone {
        Some(i) => (
            [
                (header::CONTENT_TYPE, i.mime),
                (header::CACHE_CONTROL, "public, max-age=86400"),
            ],
            i.octets,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn par_nom(Path(nom): Path<String>) -> Response {
    reponse(icone(&nom))
}

async fn ancienne() -> Response {
    reponse(icone_au_chemin(ANCIENNE_ICONE))
}

/// Le routeur des icônes, à fusionner à la racine de l'application.
pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route(&format!("{PREFIXE_ICONES}/{{nom}}"), get(par_nom))
        .route(ANCIENNE_ICONE, get(ancienne))
}

#[cfg(test)]
#[path = "upnp_icones_9483_tests.rs"]
mod upnp_icones_9483_tests;
