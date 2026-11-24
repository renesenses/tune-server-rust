//! #9174 — d'où écoute le lecteur d'une zone navigateur.
//!
//! Couche posée sur le routeur des zones, donc SOUS l'authentification : une
//! requête refusée par `auth_middleware` n'arrive jamais ici. N'est retenu
//! qu'un `POST …/browser-*` réussi (2xx) sur une zone qui existe et dont la
//! sortie est `browser`. Relayé (relais, écoute directe), il met la zone à
//! distance ; local, il la ramène. Voir
//! `tune_core::orchestrator::flux_a_distance_9174`.

use axum::extract::{OriginalUri, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::orchestrator::flux_a_distance_9174::provenance_d_une_requete_de_zone;

use crate::garde_du_relais::requete_relayee;
use crate::state::AppState;

pub async fn noter_la_provenance(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let chemin = request
        .extensions()
        .get::<OriginalUri>()
        .map(|u| u.path().to_string())
        .unwrap_or_else(|| request.uri().path().to_string());
    let provenance = provenance_d_une_requete_de_zone(
        request.method().as_str(),
        &chemin,
        requete_relayee(request.headers()),
    );
    let reponse = next.run(request).await;
    if let Some((zone, a_distance)) = provenance
        && reponse.status().is_success()
        && ZoneRepo::with_backend(state.backend.clone())
            .get(zone)
            .ok()
            .flatten()
            .is_some_and(|z| z.output_type.as_deref() == Some("browser"))
    {
        state
            .orchestrator
            .noter_provenance_de_zone(zone, a_distance);
    }
    reponse
}
