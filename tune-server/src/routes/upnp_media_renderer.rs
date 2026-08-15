//! Couche route du MediaRenderer:1 par zone (#1750).
//!
//! Le parsing SOAP, les SCPD et les annonces SSDP vivent dans
//! `tune_core::upnp_renderer` (pur). Ici : exécution des commandes via
//! l'orchestrateur, session par zone (URI + métadonnées posées par
//! SetAVTransportURI), annonceur SSDP des zones opt-in.

use std::collections::HashMap;
use std::sync::Mutex;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use tracing::{debug, info, warn};

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::upnp_renderer::{self, RendererCommand, RendererSnapshot};

use crate::state::AppState;

/// URI + métadonnées posées par le dernier SetAVTransportURI, par zone.
/// Volatile : un point de contrôle repose toujours l'URI avant Play.
#[derive(Debug, Clone, Default)]
struct RendererSession {
    uri: String,
    title: Option<String>,
    artist: Option<String>,
    duration_ms: Option<i64>,
}

fn sessions() -> &'static Mutex<HashMap<i64, RendererSession>> {
    static SESSIONS: std::sync::OnceLock<Mutex<HashMap<i64, RendererSession>>> =
        std::sync::OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/{zone_id}/description.xml", get(description))
        .route("/{zone_id}/AVTransport/control", post(avtransport_control))
        .route("/{zone_id}/AVTransport/scpd.xml", get(avtransport_scpd))
        .route("/{zone_id}/AVTransport/event", any(event_subscription))
        .route(
            "/{zone_id}/RenderingControl/control",
            post(renderingcontrol_control),
        )
        .route(
            "/{zone_id}/RenderingControl/scpd.xml",
            get(renderingcontrol_scpd),
        )
        .route("/{zone_id}/RenderingControl/event", any(event_subscription))
        .route(
            "/{zone_id}/ConnectionManager/control",
            post(connection_manager_control),
        )
        .route(
            "/{zone_id}/ConnectionManager/scpd.xml",
            get(connection_manager_scpd),
        )
        .route(
            "/{zone_id}/ConnectionManager/event",
            any(event_subscription),
        )
}

/// Le réglage opt-in d'une zone. Défaut : off — on ne pollue pas le réseau
/// avec des renderers que personne n'a demandés.
pub fn zone_renderer_enabled(settings: &SettingsRepo, zone_id: i64) -> bool {
    settings
        .get(&format!("zone_{zone_id}_upnp_renderer"))
        .ok()
        .flatten()
        .as_deref()
        == Some("true")
}

/// UDN stable d'un renderer de zone — même exigence que le MediaServer
/// (#1719) : JPlay mémorise par UDN, un uuid par boot casse l'appairage.
fn renderer_udn(settings: &SettingsRepo, zone_id: i64) -> String {
    let key = format!("upnp_renderer_udn_{zone_id}");
    match settings.get(&key).ok().flatten().filter(|v| !v.is_empty()) {
        Some(u) => u,
        None => {
            let fresh = format!("uuid:{}", uuid::Uuid::new_v4());
            let _ = settings.set(&key, &fresh);
            fresh
        }
    }
}

fn xml_response(body: String) -> Response {
    let status = if tune_core::upnp_server::is_soap_fault(&body) {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::OK
    };
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/xml; charset=utf-8")
        .body(Body::from(body))
        .unwrap()
}

async fn description(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Response {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if !zone_renderer_enabled(&settings, zone_id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten();
    let Some(zone) = zone else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let base_url = state
        .upnp
        .as_ref()
        .map(|u| u.base_url())
        .unwrap_or_else(|| format!("http://127.0.0.1:{}", state.port));
    let xml = upnp_renderer::renderer_description_xml(
        &format!("{} (Tune)", zone.name),
        &renderer_udn(&settings, zone_id),
        &base_url,
        zone_id,
    );
    xml_response(xml)
}

async fn avtransport_scpd() -> Response {
    xml_response(upnp_renderer::avtransport_scpd().to_string())
}

async fn renderingcontrol_scpd() -> Response {
    xml_response(upnp_renderer::renderingcontrol_scpd().to_string())
}

async fn connection_manager_scpd() -> Response {
    xml_response(tune_core::upnp_server::connection_manager_scpd().to_string())
}

async fn connection_manager_control(body: String) -> Response {
    xml_response(tune_core::upnp_server::build_connection_manager_response(
        &body,
    ))
}

/// GENA minimal, identique au MediaServer : accepter l'abonnement suffit aux
/// points de contrôle qui suivent l'état par GetPositionInfo/GetTransportInfo.
async fn event_subscription(method: Method) -> Response {
    match method.as_str() {
        "SUBSCRIBE" => Response::builder()
            .status(StatusCode::OK)
            .header("SID", tune_core::upnp_server::new_subscription_sid())
            .header("TIMEOUT", "Second-1800")
            .body(Body::empty())
            .unwrap(),
        _ => Response::builder()
            .status(StatusCode::OK)
            .body(Body::empty())
            .unwrap(),
    }
}

/// Photo de l'état d'une zone au format UPnP.
async fn snapshot(state: &AppState, zone_id: i64) -> RendererSnapshot {
    let ps = state.playback.get_state(zone_id).await;
    let session = sessions()
        .lock()
        .map(|s| s.get(&zone_id).cloned())
        .ok()
        .flatten()
        .unwrap_or_default();
    let transport_state = match ps.state {
        tune_core::playback::PlayState::Playing => "PLAYING",
        tune_core::playback::PlayState::Paused => "PAUSED_PLAYBACK",
        tune_core::playback::PlayState::Stopped => "STOPPED",
    };
    let duration_ms = ps
        .now_playing
        .as_ref()
        .map(|np| np.duration_ms)
        .filter(|d| *d > 0)
        .or(session.duration_ms)
        .unwrap_or(0);
    let muted = ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten()
        .map(|z| z.muted)
        .unwrap_or(false);
    RendererSnapshot {
        transport_state,
        position_ms: ps.position_ms,
        duration_ms,
        uri: session.uri,
        volume: (ps.volume.clamp(0.0, 1.0) * 100.0).round() as u8,
        muted,
    }
}

async fn avtransport_control(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    body: String,
) -> Response {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if !zone_renderer_enabled(&settings, zone_id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let cmd = upnp_renderer::parse_renderer_command(&body);
    debug!(zone_id, ?cmd, "upnp_renderer_avtransport");
    let device_id = ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten()
        .and_then(|z| z.output_device_id);

    let xml = match cmd {
        RendererCommand::SetUri {
            uri,
            title,
            artist,
            duration_ms,
        } => {
            if let Ok(mut s) = sessions().lock() {
                s.insert(
                    zone_id,
                    RendererSession {
                        uri,
                        title,
                        artist,
                        duration_ms,
                    },
                );
            }
            upnp_renderer::empty_response("SetAVTransportURI")
        }
        RendererCommand::Play => {
            let session = sessions()
                .lock()
                .map(|s| s.get(&zone_id).cloned())
                .ok()
                .flatten()
                .unwrap_or_default();
            if session.uri.is_empty() {
                tune_core::upnp_server::soap_fault(701, "No URI set")
            } else {
                // Même chemin que la lecture d'un media server externe : le
                // flux traverse toute la chaîne Tune (EQ, convolveur, trim).
                let req = tune_core::orchestrator::PlayRequest {
                    zone_id,
                    output_device_id: device_id.clone(),
                    track_id: None,
                    source: Some("upnp".into()),
                    source_id: Some(session.uri.clone()),
                    title: session.title.clone(),
                    artist_name: session.artist.clone(),
                    duration_ms: session.duration_ms,
                    ..Default::default()
                };
                match state.orchestrator.play(req).await {
                    Ok(_) => {
                        info!(zone_id, uri = %session.uri, "upnp_renderer_play");
                        upnp_renderer::empty_response("Play")
                    }
                    Err(e) => {
                        warn!(zone_id, error = %e, "upnp_renderer_play_failed");
                        tune_core::upnp_server::soap_fault(701, &e)
                    }
                }
            }
        }
        RendererCommand::Pause => {
            state
                .orchestrator
                .pause(zone_id, device_id.as_deref())
                .await;
            upnp_renderer::empty_response("Pause")
        }
        RendererCommand::Stop => {
            state.orchestrator.stop(zone_id, device_id.as_deref()).await;
            upnp_renderer::empty_response("Stop")
        }
        RendererCommand::Seek(ms) => {
            state
                .orchestrator
                .seek(zone_id, ms, device_id.as_deref())
                .await;
            upnp_renderer::empty_response("Seek")
        }
        RendererCommand::GetTransportInfo => {
            upnp_renderer::transport_info_response(&snapshot(&state, zone_id).await)
        }
        RendererCommand::GetPositionInfo => {
            upnp_renderer::position_info_response(&snapshot(&state, zone_id).await)
        }
        RendererCommand::GetMediaInfo => {
            upnp_renderer::media_info_response(&snapshot(&state, zone_id).await)
        }
        RendererCommand::Unsupported(name) => {
            debug!(zone_id, action = %name, "upnp_renderer_unsupported_action");
            tune_core::upnp_server::soap_fault(401, "Invalid Action")
        }
        // Actions RenderingControl arrivées sur le mauvais endpoint.
        _ => tune_core::upnp_server::soap_fault(401, "Invalid Action"),
    };
    xml_response(xml)
}

async fn renderingcontrol_control(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    body: String,
) -> Response {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if !zone_renderer_enabled(&settings, zone_id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let cmd = upnp_renderer::parse_renderer_command(&body);
    debug!(zone_id, ?cmd, "upnp_renderer_renderingcontrol");
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let device_id = repo
        .get(zone_id)
        .ok()
        .flatten()
        .and_then(|z| z.output_device_id);

    let xml = match cmd {
        RendererCommand::GetVolume => {
            upnp_renderer::volume_response(&snapshot(&state, zone_id).await)
        }
        RendererCommand::SetVolume(v) => {
            state
                .orchestrator
                .set_volume(zone_id, f64::from(v) / 100.0, device_id.as_deref())
                .await;
            upnp_renderer::empty_response("SetVolume")
        }
        RendererCommand::GetMute => upnp_renderer::mute_response(&snapshot(&state, zone_id).await),
        RendererCommand::SetMute(m) => {
            let _ = repo.update_muted(zone_id, m);
            upnp_renderer::empty_response("SetMute")
        }
        RendererCommand::Unsupported(name) => {
            debug!(zone_id, action = %name, "upnp_renderer_unsupported_action");
            tune_core::upnp_server::soap_fault(401, "Invalid Action")
        }
        _ => tune_core::upnp_server::soap_fault(401, "Invalid Action"),
    };
    xml_response(xml)
}

/// Annonceur SSDP des renderers de zones opt-in. Relit la liste à CHAQUE
/// cycle (une zone activée/désactivée prend effet sans redémarrage) et
/// alimente le registre lu par le répondeur M-SEARCH.
pub fn spawn_renderer_advertiser(state: AppState) {
    tokio::spawn(async move {
        use std::net::{Ipv4Addr, SocketAddrV4};
        use tokio::net::UdpSocket;
        let bind = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0);
        let dest = std::net::SocketAddr::from((Ipv4Addr::new(239, 255, 255, 250), 1900u16));
        loop {
            let settings = SettingsRepo::with_backend(state.backend.clone());
            let zones = ZoneRepo::with_backend(state.backend.clone())
                .list()
                .unwrap_or_default();
            let base_url = state.upnp.as_ref().map(|u| u.base_url());
            let mut adverts = Vec::new();
            if let Some(base) = base_url {
                for z in &zones {
                    let Some(id) = z.id else { continue };
                    if !zone_renderer_enabled(&settings, id) {
                        continue;
                    }
                    adverts.push(tune_core::upnp_renderer::RendererAdvert {
                        uuid: renderer_udn(&settings, id),
                        location: format!(
                            "{base}{}/{id}/description.xml",
                            tune_core::upnp_renderer::RENDERER_MOUNT
                        ),
                    });
                }
            }
            if !adverts.is_empty()
                && let Ok(socket) = UdpSocket::bind(bind).await
            {
                for adv in &adverts {
                    for msg in
                        tune_core::upnp_renderer::renderer_notify_messages(&adv.uuid, &adv.location)
                    {
                        let _ = socket.send_to(msg.as_bytes(), dest).await;
                    }
                }
            }
            tune_core::upnp_renderer::set_renderer_adverts(adverts);
            tokio::time::sleep(std::time::Duration::from_secs(600)).await;
        }
    });
}
