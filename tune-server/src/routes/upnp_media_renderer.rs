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
    /// Piste suivante (SetNextAVTransportURI) — enchaînée par le watcher
    /// quand la courante se termine. Un Stop commandé ou un nouveau
    /// SetAVTransportURI l'efface : enchaîner après un arrêt voulu serait
    /// une surprise, pas du gapless.
    next: Option<NextItem>,
}

#[derive(Debug, Clone)]
struct NextItem {
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

/// Zones dont le watcher d'enchaînement tourne déjà — un seul par zone.
fn watchers() -> &'static Mutex<std::collections::HashSet<i64>> {
    static WATCHERS: std::sync::OnceLock<Mutex<std::collections::HashSet<i64>>> =
        std::sync::OnceLock::new();
    WATCHERS.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Réveil de l'annonceur SSDP : un opt-in fraîchement activé doit s'annoncer
/// tout de suite, pas au prochain cycle de 10 minutes.
pub fn advertiser_wakeup() -> &'static tokio::sync::Notify {
    static NOTIFY: std::sync::OnceLock<tokio::sync::Notify> = std::sync::OnceLock::new();
    NOTIFY.get_or_init(tokio::sync::Notify::new)
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
    // Le ConnectionManager du renderer, PAS celui du media server : celui-ci
    // annonce en `Source` ce qu'un serveur sert et rend un `Sink` vide, ce qui
    // pour un renderer dit « je n'accepte aucun format ».
    xml_response(tune_core::upnp_server::build_renderer_connection_manager_response(&body))
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
                // Nouveau contexte de lecture : la suivante en attente est
                // celle de l'ANCIEN contexte, elle ne survit pas.
                s.insert(
                    zone_id,
                    RendererSession {
                        uri,
                        title,
                        artist,
                        duration_ms,
                        next: None,
                    },
                );
            }
            upnp_renderer::empty_response("SetAVTransportURI")
        }
        RendererCommand::SetNextUri {
            uri,
            title,
            artist,
            duration_ms,
        } => {
            if let Ok(mut s) = sessions().lock() {
                s.entry(zone_id).or_default().next = Some(NextItem {
                    uri,
                    title,
                    artist,
                    duration_ms,
                });
            }
            spawn_gapless_watcher(state.clone(), zone_id);
            upnp_renderer::empty_response("SetNextAVTransportURI")
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
            match state
                .orchestrator
                .pause(zone_id, device_id.as_deref())
                .await
            {
                Ok(()) => upnp_renderer::empty_response("Pause"),
                Err(error) => tune_core::upnp_server::soap_fault(701, &error.to_string()),
            }
        }
        RendererCommand::Stop => {
            // Arrêt COMMANDÉ : la suivante en attente s'efface AVANT le stop,
            // sinon le watcher lirait « stoppé + next posée » et relancerait.
            if let Ok(mut s) = sessions().lock()
                && let Some(session) = s.get_mut(&zone_id)
            {
                session.next = None;
            }
            state.orchestrator.stop(zone_id, device_id.as_deref()).await;
            upnp_renderer::empty_response("Stop")
        }
        RendererCommand::Seek(ms) => {
            match state
                .orchestrator
                .seek(zone_id, ms, device_id.as_deref())
                .await
            {
                Ok(()) => upnp_renderer::empty_response("Seek"),
                Err(error) => tune_core::upnp_server::soap_fault(701, &error.to_string()),
            }
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
            match state
                .orchestrator
                .set_volume(zone_id, f64::from(v) / 100.0, device_id.as_deref())
                .await
            {
                Ok(()) => upnp_renderer::empty_response("SetVolume"),
                Err(error) => tune_core::upnp_server::soap_fault(701, &error.to_string()),
            }
        }
        RendererCommand::GetMute => upnp_renderer::mute_response(&snapshot(&state, zone_id).await),
        RendererCommand::SetMute(m) => {
            match state
                .orchestrator
                .set_mute(zone_id, m, device_id.as_deref())
                .await
            {
                Ok(()) => upnp_renderer::empty_response("SetMute"),
                Err(error) => tune_core::upnp_server::soap_fault(701, &error.to_string()),
            }
        }
        RendererCommand::Unsupported(name) => {
            debug!(zone_id, action = %name, "upnp_renderer_unsupported_action");
            tune_core::upnp_server::soap_fault(401, "Invalid Action")
        }
        _ => tune_core::upnp_server::soap_fault(401, "Invalid Action"),
    };
    xml_response(xml)
}

/// Enchaîne la piste posée par SetNextAVTransportURI quand la courante se
/// termine (#1750, gapless v1). Aucun événement de fin de piste n'existe sur
/// le bus (`playback.stopped` n'est jamais émis) : on observe l'état toutes
/// les 2 s. Enchaînement = la zone était en LECTURE avec une suivante posée,
/// et passe à STOPPED — un arrêt commandé via notre Stop a déjà effacé la
/// suivante, donc ne relance rien.
///
/// v1 assumée : l'enchaînement passe par un play complet — le contrat UPnP
/// (le point de contrôle n'a pas à re-commander) est tenu, le zéro-gap réel
/// viendra avec le préchargement orchestrateur.
fn spawn_gapless_watcher(state: AppState, zone_id: i64) {
    {
        let Ok(mut w) = watchers().lock() else { return };
        if !w.insert(zone_id) {
            return; // déjà un watcher sur cette zone
        }
    }
    tokio::spawn(async move {
        let mut was_playing = false;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let pending = sessions()
                .lock()
                .ok()
                .and_then(|s| s.get(&zone_id).and_then(|x| x.next.clone()));
            let Some(next) = pending else { break };

            let ps = state.playback.get_state(zone_id).await;
            match ps.state {
                tune_core::playback::PlayState::Playing => was_playing = true,
                tune_core::playback::PlayState::Paused => {}
                tune_core::playback::PlayState::Stopped if was_playing => {
                    // Fin naturelle : promouvoir la suivante et relancer.
                    if let Ok(mut s) = sessions().lock() {
                        s.insert(
                            zone_id,
                            RendererSession {
                                uri: next.uri.clone(),
                                title: next.title.clone(),
                                artist: next.artist.clone(),
                                duration_ms: next.duration_ms,
                                next: None,
                            },
                        );
                    }
                    let device_id = ZoneRepo::with_backend(state.backend.clone())
                        .get(zone_id)
                        .ok()
                        .flatten()
                        .and_then(|z| z.output_device_id);
                    let req = tune_core::orchestrator::PlayRequest {
                        zone_id,
                        output_device_id: device_id,
                        track_id: None,
                        source: Some("upnp".into()),
                        source_id: Some(next.uri.clone()),
                        title: next.title,
                        artist_name: next.artist,
                        duration_ms: next.duration_ms,
                        ..Default::default()
                    };
                    match state.orchestrator.play(req).await {
                        Ok(_) => info!(zone_id, uri = %next.uri, "upnp_renderer_gapless_advance"),
                        Err(e) => {
                            warn!(zone_id, error = %e, "upnp_renderer_gapless_advance_failed")
                        }
                    }
                    break;
                }
                tune_core::playback::PlayState::Stopped => {}
            }
        }
        if let Ok(mut w) = watchers().lock() {
            w.remove(&zone_id);
        }
    });
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
            // Cycle standard de 10 min, mais un opt-in fraîchement activé
            // (PATCH upnp_renderer) réveille la boucle tout de suite.
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(600)) => {}
                _ = advertiser_wakeup().notified() => {}
            }
        }
    });
}
