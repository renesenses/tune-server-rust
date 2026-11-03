mod api_proxy;
mod circle;
mod licence;
mod morceau_binaire;
mod protocol;
#[cfg(test)]
mod relais_par_url_tests;
mod session_routes;
mod sessions;
#[cfg(test)]
mod sessions_tests;
mod site;
mod state;
mod stream_proxy;
mod trafic;
#[cfg(test)]
mod trafic_tests;
mod web_ui;
mod ws_client;
mod ws_server;

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::WebSocketUpgrade;
use axum::response::IntoResponse;
use axum::routing::{any, get, post};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::info;

use state::RelayState;

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();

    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "tune_bridge=info".to_string());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let port: u16 = std::env::var("BRIDGE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(9090);

    let state = Arc::new(RelayState::new());

    // Spawn heartbeat sender
    let heartbeat_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            let ping = serde_json::json!({"type": "relay.ping"}).to_string();
            for conn in heartbeat_state.servers.iter() {
                if conn.ws_tx.send(ping.clone()).await.is_ok() {
                    heartbeat_state
                        .trafic
                        .sortie(&conn.trafic, trafic::Categorie::Api, ping.len());
                }
            }
        }
    });

    // Trafic du pont (#6512) : agregats par minute, ENVOYES au site avec le
    // jeton de service du relais. Pourquoi un envoi plutot qu'une route a
    // interroger : le pont publie son port 9090 sur toutes les interfaces du
    // VPS (docker-compose.bridge.yml) et le vhost bridge.mozaiklabs.fr le
    // relaie tout entier. Une route de lecture y serait exposee a Internet,
    // gardee par un seul secret. L'envoi n'ouvre AUCUNE route : il reutilise
    // le chemin pont -> site et le jeton qui existent deja pour
    // `listen/verify`, et le site le garde comme celle-ci (`auth.tune-bridge`).
    // Les minutes non acceptees attendent 24 h au plus sur le pont.
    tokio::spawn(trafic::boucle(state.trafic.clone()));

    // Ecoute a distance par laissez-passer (#6603) : jeu de cles du site,
    // battement de toutes les sessions chaque minute.
    tokio::spawn(state.sessions.clone().boucle());

    let app = routes();
    // Le client web, s'il est fourni. Monte APRES les routes du relais : axum
    // fait primer les segments statiques (`/api`, `/ws`, `/stream`, `/health`)
    // sur le segment dynamique `{server_id}`, mais l'ordre rend l'intention
    // lisible plutot que dependante d'une regle implicite.
    let app = web_ui::monter(app);
    let app = app
        .layer(CorsLayer::permissive())
        // Traces HTTP avec l'URI MASQUEE : `/stream/relay`, `/ws/client` et
        // les lectures relayees portent le jeton de pont en `?token=`, et la
        // trace par defaut de tower-http ecrirait l'URI en clair.
        .layer(TraceLayer::new_for_http().make_span_with(
            |requete: &axum::http::Request<axum::body::Body>| {
                tracing::debug_span!(
                    "request",
                    method = %requete.method(),
                    uri = %api_proxy::uri_masquee(requete.uri()),
                    version = ?requete.version(),
                )
            },
        ))
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    info!(addr = %addr, "Tune Bridge relay starting");

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("failed to bind");

    info!("Tune Bridge listening on {addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
}

/// La table des routes du relais, sans le client web ni les couches : celle
/// que les tests de bout en bout montent aussi.
pub(crate) fn routes() -> Router<Arc<RelayState>> {
    Router::new()
        .route("/health", get(health))
        // Sessions d'ecoute a distance (#6603). `renouveler` et `defi` sont
        // des segments statiques : axum les fait primer sur `{server_id}`.
        .route("/session/defi/{server_id}", get(session_routes::defi))
        .route("/session/renouveler", post(session_routes::renouveler))
        .route("/session/{server_id}", post(session_routes::ouvrir))
        .route("/ws/server", get(ws_server_handler))
        .route("/api/relay/{server_id}/{*path}", any(api_proxy::proxy_api))
        .route("/ws/client/{server_id}", get(ws_client::ws_client_handler))
        .route(
            "/stream/relay/{server_id}/{*stream_path}",
            get(stream_proxy::proxy_stream),
        )
        // Tune Circle T4 (#5327) : l'ecoute chez un contact, par billet
        // seulement. Voir `circle.rs`.
        .route("/stream/circle/{ticket}", get(circle::flux_de_cercle))
}

async fn health() -> &'static str {
    "ok"
}

async fn ws_server_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<RelayState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_server::handle_server_ws(socket, state))
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install Ctrl+C handler");
    info!("shutdown signal received");
}
