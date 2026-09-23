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
    // Contexte UPnP et lecture effectivement démarrée par ce contexte.
    // L'URI seule ne distingue pas deux lectures successives du même titre.
    revision: u64,
    play_seq: Option<u64>,
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
///
/// Registre SÉPARÉ de `sessions` (#3967) : il survit au remplacement de la
/// session par un nouveau `SetAVTransportURI`, et c'est lui qui sérialise la
/// pose d'une suivante avec la libération du watcher — voir
/// `try_release_watcher` pour l'ordre des verrous.
fn watchers() -> &'static Mutex<std::collections::HashSet<i64>> {
    static WATCHERS: std::sync::OnceLock<Mutex<std::collections::HashSet<i64>>> =
        std::sync::OnceLock::new();
    WATCHERS.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

fn nouvelle_revision() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static REVISION: AtomicU64 = AtomicU64::new(1);
    REVISION.fetch_add(1, Ordering::Relaxed)
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
        &nom_de_facade(&zone.name),
        &renderer_udn(&settings, zone_id),
        &base_url,
        zone_id,
    );
    xml_response(xml)
}

/// Le `friendlyName` de la façade MediaRenderer d'une zone — **idempotent**.
///
/// C'est l'unique producteur du suffixe « (Tune) », et il était
/// `format!("{} (Tune)", zone.name)` : non idempotent. Or ce nom ne reste pas
/// dans le XML. Le scanner SSDP en fait `device.name`, et l'auto-découverte
/// crée une zone qui le porte (`discovery_setup.rs`, `get_or_create`) : chaque
/// tour où les deux rideaux d'auto-exclusion (`est_notre_propre_renderer`,
/// `est_un_de_nos_udn_de_facade`) cèdent AJOUTAIT un « (Tune) » de plus, écrit
/// dans `zones.name`, où il survit aux redémarrages. Le journal du testeur de
/// #3616 en porte TROIS : « DENAFRIPS USB HiRes Audio, USB Audio (Tune) (Tune)
/// (Tune) ».
///
/// Ce troisième rideau ne remplace pas les deux autres — il n'empêche pas la
/// zone fantôme, il en borne le nom. Une zone déjà suffixée par un tour
/// précédent ne se re-suffixe plus, et la dérive s'arrête net.
fn nom_de_facade(nom_de_zone: &str) -> String {
    const SUFFIXE: &str = " (Tune)";
    if nom_de_zone.ends_with(SUFFIXE) {
        return nom_de_zone.to_string();
    }
    format!("{nom_de_zone}{SUFFIXE}")
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
    let volume = if volume_verrouille(
        tune_core::audio::audiophile::volume_lock_enabled(&state.backend, zone_id),
        tune_core::audio::audiophile::zone_enabled(&state.backend, zone_id),
    ) {
        PLEINE_ECHELLE
    } else {
        (ps.volume.clamp(0.0, 1.0) * 100.0).round() as u8
    };
    RendererSnapshot {
        transport_state,
        position_ms: ps.position_ms,
        duration_ms,
        uri: session.uri,
        volume,
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
                        revision: nouvelle_revision(),
                        play_seq: None,
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
                let ps = state.playback.get_state(zone_id).await;
                let is_paused_same_uri = doit_reprendre(
                    ps.state,
                    ps.now_playing
                        .as_ref()
                        .and_then(|np| np.source_id.as_deref()),
                    &session.uri,
                );

                if is_paused_same_uri {
                    match state
                        .orchestrator
                        .resume(zone_id, device_id.as_deref())
                        .await
                    {
                        Ok(()) => {
                            memoriser_lecture_renderer(&state, zone_id, &session).await;
                            info!(zone_id, uri = %session.uri, "upnp_renderer_play_resumed");
                            upnp_renderer::empty_response("Play")
                        }
                        Err(e) => {
                            warn!(zone_id, error = %e, "upnp_renderer_resume_failed");
                            tune_core::upnp_server::soap_fault(701, &e.to_string())
                        }
                    }
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
                        Ok(result) => {
                            if result.error.is_none() {
                                memoriser_lecture_renderer(&state, zone_id, &session).await;
                            }
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
                session.play_seq = None;
                session.revision = nouvelle_revision();
            }
            state.orchestrator.stop(zone_id, device_id.as_deref()).await;
            upnp_renderer::empty_response("Stop")
        }
        RendererCommand::Seek(ms) => {
            let session = sessions()
                .lock()
                .ok()
                .and_then(|s| s.get(&zone_id).cloned());
            match state
                .orchestrator
                .seek(zone_id, ms, device_id.as_deref())
                .await
            {
                Ok(()) => {
                    if let Some(session) = session {
                        memoriser_lecture_renderer(&state, zone_id, &session).await;
                    }
                    upnp_renderer::empty_response("Seek")
                }
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
            let volume_locked = volume_verrouille(
                tune_core::audio::audiophile::volume_lock_enabled(&state.backend, zone_id),
                tune_core::audio::audiophile::zone_enabled(&state.backend, zone_id),
            );
            if volume_locked {
                info!(
                    zone_id,
                    requested_v = v,
                    "upnp_renderer_set_volume_locked_bitperfect_preserved"
                );
                upnp_renderer::empty_response("SetVolume")
            } else {
                match state
                    .orchestrator
                    .set_volume(zone_id, f64::from(v) / 100.0, device_id.as_deref())
                    .await
                {
                    Ok(()) => upnp_renderer::empty_response("SetVolume"),
                    Err(error) => tune_core::upnp_server::soap_fault(701, &error.to_string()),
                }
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

/// Le volume publié par un renderer verrouillé : la pleine échelle.
pub(crate) const PLEINE_ECHELLE: u8 = 100;

/// Le Mode PURE verrouille-t-il le volume de cette zone ? (#3972, garde #4098)
///
/// Décision unique des DEUX sites de la route `RenderingControl` : `snapshot`,
/// qui publie le volume, et `SetVolume`, qui acquitte sans toucher au gain.
/// Les deux doivent répondre pareil, sinon un contrôleur lit 100 et croit
/// pouvoir descendre, ou l'inverse.
///
/// Le verrouillage exige les DEUX conditions. Le réglage `audiophile_lock_volume`
/// seul ne suffit pas : hors Mode PURE, il n'y a pas de promesse de bit-perfect
/// à protéger, et confisquer le volume d'une zone ordinaire serait un défaut.
///
/// Extrait de la route pour être gardé : voir
/// `le_verrou_de_volume_exige_les_deux_conditions`.
fn volume_verrouille(verrou_actif: bool, mode_pure_actif: bool) -> bool {
    verrou_actif && mode_pure_actif
}

/// Un `Play` sur une session en pause doit REPRENDRE, jamais relancer (#3969).
///
/// Le point de décision de la route `AVTransport` : le contrôleur distant
/// n'a qu'une commande `Play` pour « démarrer » et pour « reprendre », c'est
/// donc au renderer de distinguer les deux. Reprendre ne vaut que si la zone
/// est en pause ET que ce qui est en pause est bien l'URI de la session — un
/// contrôleur qui pousse une NOUVELLE URI puis `Play` attend une relecture,
/// pas la reprise du morceau précédent.
///
/// Extrait de la route pour être gardé : voir
/// `un_play_en_pause_sur_la_meme_uri_reprend`.
fn doit_reprendre(
    etat: tune_core::playback::PlayState,
    uri_en_cours: Option<&str>,
    uri_de_session: &str,
) -> bool {
    etat == tune_core::playback::PlayState::Paused && uri_en_cours == Some(uri_de_session)
}

/// La lecture en cours appartient-elle encore à CETTE session UPnP ? (#4324)
///
/// L'URI seule ne suffit pas à l'affirmer — c'est le rôle de `play_seq` —
/// mais elle disqualifie tout de suite une lecture d'une autre source.
fn lecture_de_session(session: &RendererSession, ps: &tune_core::playback::ZoneState) -> bool {
    ps.now_playing.as_ref().is_some_and(|np| {
        np.source == "upnp" && np.source_id.as_deref() == Some(session.uri.as_str())
    })
}

/// Une commande UPnP qui réussit rattache sa lecture au contexte encore actif
/// (#4324). Un SetURI/Stop intervenu pendant la résolution ne peut pas être
/// réarmé par le résultat tardif d'une ancienne commande : la `revision` du
/// contexte est comparée avant d'inscrire le `play_seq` propriétaire.
async fn memoriser_lecture_renderer(state: &AppState, zone_id: i64, played: &RendererSession) {
    let ps = state.playback.get_state(zone_id).await;
    if let Ok(mut sessions) = sessions().lock()
        && let Some(session) = sessions.get_mut(&zone_id)
        && session.revision == played.revision
        && lecture_de_session(session, &ps)
    {
        session.play_seq = Some(ps.play_seq);
    }
}

/// Libère le watcher d'une zone — mais SEULEMENT s'il ne reste rien à
/// enchaîner (#3967).
///
/// C'est le point de rendez-vous de la course décrite dans #3967. Le handler
/// `SetNextAVTransportURI` pose la suivante (verrou `sessions`, relâché) PUIS
/// demande un watcher (verrou `watchers`). Cet ordre-là est imposé ; ici on
/// prend les verrous dans le MÊME ordre relatif — `watchers` d'abord,
/// `sessions` ensuite — si bien que les deux sections critiques sont
/// sérialisées par `watchers` :
///
/// * la suivante est posée avant que l'on prenne `watchers` → on la voit et on
///   refuse de se libérer : ce watcher-ci en reste responsable ;
/// * on s'est libéré avant que le handler prenne `watchers` → il ne nous
///   trouve plus et redémarre un watcher.
///
/// Aucun entre-deux. Avant le correctif, le watcher se retirait SANS condition
/// à la sortie de sa boucle : une suivante posée pendant l'`await` de la
/// promotion restait en attente sans personne pour l'enchaîner, et la chaîne
/// s'arrêtait après la piste promue.
///
/// Rend `true` quand le watcher est libéré (il doit alors s'arrêter), `false`
/// quand une suivante l'attend (il doit continuer sa boucle).
fn try_release_watcher(zone_id: i64) -> bool {
    let Ok(mut w) = watchers().lock() else {
        // Verrou empoisonné : plus rien à garantir, on s'arrête.
        return true;
    };
    let encore_une_suivante = sessions()
        .lock()
        .ok()
        .and_then(|s| s.get(&zone_id).map(|session| session.next.is_some()))
        .unwrap_or(false);
    if encore_une_suivante {
        return false;
    }
    w.remove(&zone_id);
    true
}

/// Promeut la suivante en piste courante, de façon ATOMIQUE (#3967).
///
/// Le watcher a décidé de promouvoir en début de tour, puis a relâché le
/// verrou. Pendant ce laps, un point de contrôle a pu REMPLACER la suivante.
/// Relire ici, sous le verrou, évite de démarrer l'élément périmé — critère
/// « replacement next URI [...] do not start a stale item » de #3967 — et
/// évite du même coup que l'installation de la piste promue écrase ce
/// remplacement.
///
/// #4324 : la promotion ouvre un NOUVEAU contexte de lecture — `revision`
/// avance et `play_seq` repart à `None`, si bien qu'aucun état observé avant
/// la promotion ne peut être pris pour celui de la piste promue.
///
/// Rend la session promue (clonée pour `memoriser_lecture_renderer`), ou
/// `None` si la suivante a disparu entre-temps (un Stop commandé l'efface).
fn promouvoir_la_suivante(zone_id: i64) -> Option<RendererSession> {
    let mut carte = sessions().lock().ok()?;
    let session = carte.entry(zone_id).or_default();
    let promue = session.next.take()?;
    session.uri = promue.uri;
    session.title = promue.title;
    session.artist = promue.artist;
    session.duration_ms = promue.duration_ms;
    session.revision = nouvelle_revision();
    session.play_seq = None;
    Some(session.clone())
}

/// Ce que le watcher doit faire à la fin d'un tour, décidé en UNE section
/// critique sur `sessions` (#4324) : la comparaison du contexte, le constat
/// « plus rien à enchaîner » et la détection de reprise ne doivent pas
/// pouvoir s'entrelacer avec un Play/SetURI.
enum SuiteDuTour {
    /// Rien à faire ce tour-ci : on réobserve dans 2 s.
    Attendre,
    /// Plus rien à enchaîner : demander la libération (#3967).
    Liberer,
    /// Fin naturelle de la lecture possédée : promouvoir la suivante.
    Promouvoir,
}

/// Enchaîne la piste posée par SetNextAVTransportURI quand la courante se
/// termine (#1750, gapless v1). Aucun événement de fin de piste n'existe sur
/// le bus (`playback.stopped` n'est jamais émis) : on observe l'état toutes
/// les 2 s.
///
/// v1 assumée : l'enchaînement passe par un play complet — le contrat UPnP
/// (le point de contrôle n'a pas à re-commander) est tenu, le zéro-gap réel
/// viendra avec le préchargement orchestrateur.
///
/// #3967 : le watcher ne s'arrête plus après UNE promotion. Un point de
/// contrôle qui pose la piste N+2 pendant qu'on démarre la piste N+1 — le
/// comportement normal de BubbleUPnP sur un album — trouvait sinon un watcher
/// déjà retiré, et la chaîne s'arrêtait là. La boucle continue donc tant que
/// `try_release_watcher` refuse de la libérer.
///
/// #4324 : seul l'arrêt de la lecture appartenant ENCORE au contexte UPnP
/// peut consommer sa suivante — d'où `play_seq`, le propriétaire inscrit par
/// `memoriser_lecture_renderer`. Le now-playing est conservé à l'arrêt ; une
/// reprise Tune suivie d'un arrêt entre deux ticks reste donc détectable, et
/// la suivante périmée est abandonnée au lieu d'être lancée par-dessus. Un
/// SetNext avant Play est autorisé, mais n'arme aucune lecture étrangère.
fn spawn_gapless_watcher(state: AppState, zone_id: i64) {
    {
        let Ok(mut w) = watchers().lock() else { return };
        if !w.insert(zone_id) {
            return; // déjà un watcher sur cette zone
        }
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let observed = sessions()
                .lock()
                .ok()
                .and_then(|sessions| sessions.get(&zone_id).map(|s| (s.revision, s.play_seq)));
            let ps = state.playback.get_state(zone_id).await;
            let suite = {
                let Ok(mut sessions) = sessions().lock() else {
                    return;
                };
                let Some(session) = sessions.get_mut(&zone_id) else {
                    return;
                };
                // Ne pas comparer un état lu avant un nouveau Play/SetURI
                // avec le propriétaire installé pendant cette lecture.
                if observed != Some((session.revision, session.play_seq)) {
                    SuiteDuTour::Attendre
                } else if session.next.is_none() {
                    SuiteDuTour::Liberer
                } else if let Some(owner) = session.play_seq {
                    if owner != ps.play_seq || !lecture_de_session(session, &ps) {
                        // Tune a repris la zone : la suivante du renderer est
                        // périmée, elle ne doit pas s'imposer par-dessus.
                        session.next = None;
                        session.play_seq = None;
                        info!(zone_id, "upnp_renderer_next_discarded_after_takeover");
                        SuiteDuTour::Liberer
                    } else if ps.state != tune_core::playback::PlayState::Stopped {
                        SuiteDuTour::Attendre
                    } else {
                        SuiteDuTour::Promouvoir
                    }
                } else {
                    // SetNext posé avant Play : on attend la commande du
                    // renderer, sans armer la lecture d'un autre.
                    SuiteDuTour::Attendre
                }
            };
            match suite {
                SuiteDuTour::Attendre => continue,
                SuiteDuTour::Liberer => {
                    if try_release_watcher(zone_id) {
                        return;
                    }
                    continue;
                }
                SuiteDuTour::Promouvoir => {}
            }
            // Fin naturelle : promouvoir la suivante et relancer.
            let Some(promoted) = promouvoir_la_suivante(zone_id) else {
                // Effacée entre-temps (Stop commandé) : rien à jouer.
                if try_release_watcher(zone_id) {
                    return;
                }
                continue;
            };
            let device_id = ZoneRepo::with_backend(state.backend.clone())
                .get(zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_device_id);
            let req = tune_core::orchestrator::PlayRequest {
                zone_id,
                output_device_id: device_id,
                source: Some("upnp".into()),
                source_id: Some(promoted.uri.clone()),
                title: promoted.title.clone(),
                artist_name: promoted.artist.clone(),
                duration_ms: promoted.duration_ms,
                ..Default::default()
            };
            match state.orchestrator.play(req).await {
                Ok(result) => {
                    if let Some(error) = result.error {
                        warn!(zone_id, error, "upnp_renderer_gapless_advance_failed");
                    } else {
                        memoriser_lecture_renderer(&state, zone_id, &promoted).await;
                        info!(zone_id, uri = %promoted.uri, "upnp_renderer_gapless_advance");
                    }
                }
                Err(e) => warn!(zone_id, error = %e, "upnp_renderer_gapless_advance_failed"),
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 🔴 #3616, défaut 2 — le suffixe « (Tune) » ne doit plus s'accumuler.
    ///
    /// Site d'appel gardé : la route `description` (`GET
    /// /upnp/renderer/{zone}/description.xml`), qui compose son `friendlyName`
    /// par `nom_de_facade(&zone.name)` et le passe à
    /// `upnp_renderer::renderer_description_xml`. La garde refait EXACTEMENT
    /// cette composition, et compte les suffixes dans le XML réellement servi
    /// — pas dans la seule chaîne intermédiaire.
    #[test]
    fn le_suffixe_tune_ne_s_accumule_pas_dans_la_description_servie() {
        // Le nom déjà pollué du journal du testeur, tel qu'une zone fantôme
        // l'a persisté en base : il ne doit pas en gagner un quatrième.
        let deja_pollue = "DENAFRIPS USB HiRes Audio, USB Audio (Tune) (Tune) (Tune)";
        let xml = upnp_renderer::renderer_description_xml(
            &nom_de_facade(deja_pollue),
            "uuid:0f1ac0de-0000-4000-8000-000000000001",
            "http://192.168.0.39:8888",
            17,
        );
        assert_eq!(
            xml.matches("(Tune)").count(),
            3,
            "un nom déjà suffixé ne doit pas en gagner un de plus : c'est ce \
             tour de plus, répété, qui a produit « (Tune) (Tune) (Tune) » \
             dans zones.name.\nXML servi :\n{xml}"
        );

        // Contre-épreuve, l'autre sens : une zone SAINE doit toujours recevoir
        // son suffixe. Sans lui, `est_notre_propre_renderer` et la
        // documentation UPnP-RENDERER perdraient leur repère de lecture.
        let xml = upnp_renderer::renderer_description_xml(
            &nom_de_facade("Salon"),
            "uuid:0f1ac0de-0000-4000-8000-000000000002",
            "http://192.168.0.39:8888",
            18,
        );
        assert!(
            xml.contains("Salon (Tune)"),
            "une zone jamais suffixée doit l'être : l'idempotence ne doit pas \
             supprimer le suffixe.\nXML servi :\n{xml}"
        );
        assert_eq!(
            xml.matches("(Tune)").count(),
            1,
            "et une seule fois.\nXML servi :\n{xml}"
        );
    }

    /// 🔴 #3969 — un `Play` reçu sur une session en pause reprenait la lecture
    /// à zéro au lieu de la reprendre.
    ///
    /// Point de décision gardé : `doit_reprendre`, que la route
    /// `avtransport_control` appelle sur la commande `Play` avant de choisir
    /// entre `orchestrator.resume` et un `PlayRequest` neuf. La garde de texte
    /// plus bas vérifie que la route l'appelle bien — sans elle, ce témoin
    /// pourrait rester vert sur une route qui a cessé de s'en servir.
    #[test]
    fn un_play_en_pause_sur_la_meme_uri_reprend() {
        use tune_core::playback::PlayState;
        const URI: &str = "http://192.168.0.39:8888/stream/17.flac";

        // Le cas du défaut : en pause sur cette URI, `Play` doit reprendre.
        assert!(
            doit_reprendre(PlayState::Paused, Some(URI), URI),
            "en pause sur la même URI, Play doit REPRENDRE : c'est #3969, la \
             piste repartait de 0 et le flux était re-résolu."
        );

        // Contre-épreuve 1 — une AUTRE URI en pause : le contrôleur a poussé
        // un nouveau morceau, il attend une relecture, pas la reprise.
        assert!(
            !doit_reprendre(PlayState::Paused, Some("http://ailleurs/2.flac"), URI),
            "une autre URI en pause doit relancer, pas reprendre : sinon un \
             changement de piste rejouerait la précédente."
        );

        // Contre-épreuve 2 — déjà en lecture : rien à reprendre.
        assert!(
            !doit_reprendre(PlayState::Playing, Some(URI), URI),
            "déjà en lecture, il n'y a rien à reprendre."
        );

        // Contre-épreuve 3 — arrêté : la session est morte, il faut relancer.
        assert!(
            !doit_reprendre(PlayState::Stopped, Some(URI), URI),
            "à l'arrêt, Play doit relancer."
        );

        // Contre-épreuve 4 — rien en cours : pas d'URI à comparer.
        assert!(
            !doit_reprendre(PlayState::Paused, None, URI),
            "sans rien en cours, il n'y a pas de reprise possible."
        );
    }

    /// La route appelle-t-elle encore le point de décision ?
    ///
    /// Sans cette garde, `un_play_en_pause_sur_la_meme_uri_reprend` testerait
    /// une fonction que plus personne n'appelle — un vert qui ne garde rien.
    #[test]
    fn la_route_play_passe_bien_par_doit_reprendre() {
        let source = include_str!("upnp_media_renderer.rs");
        let production = &source[..source
            .find("#[cfg(test)]")
            .expect("le module de tests doit exister")];
        // On cherche l'APPEL, pas le nom : `fn doit_reprendre(` contient la
        // sous-chaîne et suffirait à satisfaire un `contains` naïf — la garde
        // resterait verte sur une route réécrite en ligne, avec la fonction
        // laissée orpheline. Mesuré : ce faux vert existait bel et bien.
        let appels = production
            .lines()
            .filter(|l| l.contains("doit_reprendre(") && !l.trim_start().starts_with("fn "))
            .count();
        assert!(
            appels >= 1,
            "la route AVTransport doit APPELER `doit_reprendre` : si le \
             branchement a été réécrit en ligne, le témoin de #3969 ne garde \
             plus rien.\nappels trouvés hors définition : {appels}"
        );
        assert!(
            production.contains(".resume(zone_id, device_id.as_deref())"),
            "la branche de reprise doit appeler `orchestrator.resume` : \
             relancer un PlayRequest remettrait la piste à 0 (#3969)."
        );
    }

    /// 🔴 #4098 — le verrou de volume du Mode PURE n'avait aucune garde.
    ///
    /// La promesse : en Mode PURE avec verrou, la sortie reste à pleine échelle,
    /// sans atténuation numérique, donc sans troncature de bits. Un contrôleur
    /// UPnP qui pousse un volume ne doit pas pouvoir la casser.
    ///
    /// Ce que ce témoin garde : la décision elle-même, et surtout qu'elle exige
    /// les DEUX conditions. Un défaut où le verrou seul suffirait confisquerait
    /// le volume de toutes les zones ordinaires ; un défaut où le Mode PURE seul
    /// suffirait le confisquerait sans que l'utilisateur ait demandé le verrou.
    #[test]
    fn le_verrou_de_volume_exige_les_deux_conditions() {
        assert!(
            volume_verrouille(true, true),
            "verrou + Mode PURE : le volume DOIT être verrouillé, c'est la \
             promesse de bit-perfect du Mode PURE (#3972)."
        );

        // Contre-épreuve 1 — le verrou seul, sans Mode PURE : rien à protéger.
        assert!(
            !volume_verrouille(true, false),
            "hors Mode PURE, il n'y a aucune promesse de bit-perfect à \
             protéger : confisquer le volume d'une zone ordinaire serait un \
             défaut, pas une garantie."
        );

        // Contre-épreuve 2 — le Mode PURE seul, sans verrou demandé.
        assert!(
            !volume_verrouille(false, true),
            "le Mode PURE sans `audiophile_lock_volume` laisse le volume \
             réglable : le verrou est un choix de l'utilisateur."
        );

        // Contre-épreuve 3 — ni l'un ni l'autre.
        assert!(
            !volume_verrouille(false, false),
            "sans verrou ni Mode PURE, le volume est celui de la zone."
        );
    }

    /// Les DEUX sites de la route passent-ils encore par la décision ?
    ///
    /// `snapshot` publie le volume et `SetVolume` l'acquitte. Si l'un des deux
    /// cesse d'appeler `volume_verrouille`, ils peuvent diverger : le contrôleur
    /// lirait 100 tout en pouvant réellement atténuer, ou l'inverse. Le témoin
    /// ci-dessus resterait vert — il ne teste que la décision.
    #[test]
    fn les_deux_sites_du_verrou_passent_par_la_decision() {
        let source = include_str!("upnp_media_renderer.rs");
        let production = &source[..source
            .find("#[cfg(test)]")
            .expect("le module de tests doit exister")];

        // On cherche les APPELS, pas le nom : `fn volume_verrouille(` contient
        // la sous-chaîne et suffirait à satisfaire un `contains` naïf — le faux
        // vert mesuré le 13/09 sur le témoin de #3969.
        let appels = production
            .lines()
            .filter(|l| l.contains("volume_verrouille(") && !l.trim_start().starts_with("fn "))
            .count();
        assert_eq!(
            appels, 2,
            "les DEUX sites — `snapshot` et `SetVolume` — doivent appeler \
             `volume_verrouille` : s'ils divergent, le volume publié ne \
             correspond plus au volume réellement appliqué (#4098).\nappels \
             trouvés hors définition : {appels}"
        );
    }
}

#[cfg(test)]
mod temps_upnp_3971_tests {
    use super::*;

    #[tokio::test]
    async fn un_seek_invalide_ne_modifie_pas_la_position() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let zone_id = ZoneRepo::with_backend(state.backend.clone())
            .create("UPnP temps", None, None)
            .unwrap();
        SettingsRepo::with_backend(state.backend.clone())
            .set(&format!("zone_{zone_id}_upnp_renderer"), "true")
            .unwrap();
        state.playback.set_resolving(zone_id, false).await;
        state.playback.seek(zone_id, 42_000).await;
        assert_eq!(state.playback.get_state(zone_id).await.position_ms, 42_000);
        for target in [
            "18446744073709551615:00:00",
            "0:00:01.",
            "0:00:01.x",
            "2562047788015:12:55.808",
        ] {
            let body = format!(
                r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:Seek xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><InstanceID>0</InstanceID><Unit>REL_TIME</Unit><Target>{target}</Target></u:Seek></s:Body></s:Envelope>"#
            );
            let response = avtransport_control(State(state.clone()), Path(zone_id), body).await;
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert!(
                String::from_utf8_lossy(&body).contains("<errorCode>401</errorCode>"),
                "un temps UPnP invalide doit rendre un défaut SOAP (#3971)"
            );
            assert_eq!(
                state.playback.get_state(zone_id).await.position_ms,
                42_000,
                "un Seek UPnP refusé a modifié la position (#3971)"
            );
        }
    }
}

#[cfg(test)]
mod enchainement_upnp_3967_tests {
    use super::*;

    /// Enveloppe SOAP réelle, telle qu'un point de contrôle l'émet.
    fn enveloppe(action: &str, corps: &str) -> String {
        format!(
            r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:{action} xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><InstanceID>0</InstanceID>{corps}</u:{action}></s:Body></s:Envelope>"#
        )
    }

    /// Passe la commande par la ROUTE réelle (`avtransport_control`) et rend le
    /// corps XML servi. Rien n'est construit à la main côté session : c'est le
    /// parseur et le handler de production qui posent l'URI et la suivante.
    async fn commande(state: &AppState, zone_id: i64, action: &str, corps: &str) -> String {
        let reponse = avtransport_control(
            State(state.clone()),
            Path(zone_id),
            enveloppe(action, corps),
        )
        .await;
        let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&octets).to_string()
    }

    fn suivante_posee(zone_id: i64) -> Option<String> {
        sessions().lock().ok().and_then(|s| {
            s.get(&zone_id)
                .and_then(|x| x.next.as_ref().map(|n| n.uri.clone()))
        })
    }

    fn watcher_arme(zone_id: i64) -> bool {
        watchers()
            .lock()
            .map(|w| w.contains(&zone_id))
            .unwrap_or(false)
    }

    /// 🔴 #3967 — une suivante posée pendant la promotion restait SANS watcher,
    /// et la chaîne s'arrêtait après la piste promue.
    ///
    /// `sessions()` et `watchers()` sont des statiques de PROCESSUS, partagées
    /// par tout le binaire de tests de `tune-server`. On écarte donc nos zones
    /// de la plage basse (la zone 1 sert déjà à `temps_upnp_3971_tests`) en
    /// créant quelques zones de garde, et tout ce témoin tient dans UN seul
    /// `#[tokio::test]` — deux témoins parallèles se marcheraient dessus.
    #[tokio::test]
    async fn une_suivante_posee_pendant_la_promotion_garde_son_watcher() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let repo = ZoneRepo::with_backend(state.backend.clone());
        let reglages = SettingsRepo::with_backend(state.backend.clone());
        // Zones de garde : on ne veut ni la 1 ni la 2.
        let _ = repo.create("garde 1", None, None).unwrap();
        let _ = repo.create("garde 2", None, None).unwrap();
        let zone_avec = repo.create("avec suivante", None, None).unwrap();
        let zone_sans = repo.create("sans suivante", None, None).unwrap();
        let zone_remplacee = repo.create("suivante remplacee", None, None).unwrap();
        for id in [zone_avec, zone_sans, zone_remplacee] {
            reglages
                .set(&format!("zone_{id}_upnp_renderer"), "true")
                .unwrap();
        }

        // ── Cas 1 : le point de contrôle POSE une suivante ────────────────
        commande(
            &state,
            zone_avec,
            "SetAVTransportURI",
            "<CurrentURI>http://cp/piste-1.flac</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>",
        )
        .await;
        commande(
            &state,
            zone_avec,
            "SetNextAVTransportURI",
            "<NextURI>http://cp/piste-2.flac</NextURI><NextURIMetaData></NextURIMetaData>",
        )
        .await;
        assert_eq!(
            suivante_posee(zone_avec).as_deref(),
            Some("http://cp/piste-2.flac"),
            "la route SOAP réelle doit avoir posé la suivante"
        );
        assert!(
            watcher_arme(zone_avec),
            "poser une suivante doit armer un watcher d'enchaînement"
        );

        // Le point du défaut. Le watcher sort de sa boucle après la promotion
        // de la piste 2 ; pendant l'`await` de ce `play()`, le point de
        // contrôle a posé la piste 3 — c'est ce que fait BubbleUPnP sur un
        // album. `try_release_watcher` est la fonction que le watcher appelle
        // RÉELLEMENT pour se retirer : elle doit REFUSER tant qu'une suivante
        // attend, sinon la piste 3 reste orpheline et l'album s'arrête.
        assert!(
            !try_release_watcher(zone_avec),
            "#3967 : le watcher s'est retiré alors qu'une suivante attendait — \
             plus personne pour l'enchaîner, l'album s'arrête là"
        );
        assert!(
            watcher_arme(zone_avec),
            "#3967 : refuser la libération doit LAISSER la zone dans `watchers`"
        );

        // ── Cas 2 : le point de contrôle NE pose PAS de suivante ──────────
        // Le pendant entrant de « ne pas envoyer une commande optionnelle à un
        // appareil qui ne l'annonce pas » : sans `SetNextAVTransportURI`, aucun
        // watcher ne tourne, donc aucun enchaînement ne peut être inventé.
        commande(
            &state,
            zone_sans,
            "SetAVTransportURI",
            "<CurrentURI>http://cp/seule.flac</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>",
        )
        .await;
        assert_eq!(
            suivante_posee(zone_sans),
            None,
            "aucune suivante ne doit exister sans SetNextAVTransportURI"
        );
        assert!(
            !watcher_arme(zone_sans),
            "aucun watcher ne doit être armé sans SetNextAVTransportURI : \
             enchaîner de nous-mêmes serait une surprise, pas du gapless"
        );
        assert!(
            try_release_watcher(zone_sans),
            "sans rien en attente, la libération doit être accordée — sinon le \
             watcher tournerait pour toujours"
        );

        // Contre-épreuve sur la zone 1 : un Stop COMMANDÉ efface la suivante
        // (chemin SOAP réel), et la libération doit alors être accordée.
        commande(&state, zone_avec, "Stop", "").await;
        assert_eq!(
            suivante_posee(zone_avec),
            None,
            "un Stop commandé doit effacer la suivante (#1766)"
        );
        assert!(
            try_release_watcher(zone_avec),
            "une fois la suivante effacée, le watcher doit pouvoir se retirer"
        );
        assert!(
            !watcher_arme(zone_avec),
            "la libération accordée doit retirer la zone de `watchers`"
        );

        // ── Cas 3 : suivante REMPLACÉE — ne pas démarrer l'élément périmé ──
        commande(
            &state,
            zone_remplacee,
            "SetAVTransportURI",
            "<CurrentURI>http://cp/a.flac</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>",
        )
        .await;
        commande(
            &state,
            zone_remplacee,
            "SetNextAVTransportURI",
            "<NextURI>http://cp/perimee.flac</NextURI><NextURIMetaData></NextURIMetaData>",
        )
        .await;
        // Le remplacement arrive pendant que le watcher attend l'état de
        // transport, donc APRÈS qu'il a cloné la suivante périmée.
        commande(
            &state,
            zone_remplacee,
            "SetNextAVTransportURI",
            "<NextURI>http://cp/voulue.flac</NextURI><NextURIMetaData></NextURIMetaData>",
        )
        .await;
        let promue = promouvoir_la_suivante(zone_remplacee).expect("une suivante attendait");
        assert_eq!(
            promue.uri, "http://cp/voulue.flac",
            "#3967 : la promotion a démarré l'élément PÉRIMÉ au lieu du \
             remplacement — « do not start a stale item »"
        );
        let courante = sessions().lock().ok().and_then(|s| {
            s.get(&zone_remplacee)
                .map(|x| (x.uri.clone(), x.next.is_some()))
        });
        assert_eq!(
            courante,
            Some(("http://cp/voulue.flac".to_string(), false)),
            "la piste promue doit devenir la courante, et la suivante être \
             consommée — sinon le watcher la rejouerait au tour d'après"
        );

        // Et la promotion d'une zone sans suivante ne doit rien inventer.
        assert!(
            promouvoir_la_suivante(zone_sans).is_none(),
            "sans suivante posée, la promotion ne doit rien rendre"
        );
    }

    /// Le watcher passe-t-il encore par ces deux décisions ?
    ///
    /// Sans cette garde, le témoin ci-dessus testerait des fonctions que plus
    /// personne n'appelle — un vert qui ne garde rien. On cherche les APPELS,
    /// pas le nom : `fn try_release_watcher(` contient la sous-chaîne et
    /// suffirait à satisfaire un `contains` naïf.
    #[test]
    fn le_watcher_passe_bien_par_les_deux_decisions() {
        let source = include_str!("upnp_media_renderer.rs");
        let production = &source[..source
            .find("#[cfg(test)]")
            .expect("le module de tests doit exister")];
        let appels = |nom: &str| {
            production
                .lines()
                .filter(|l| l.contains(nom) && !l.trim_start().starts_with("fn "))
                .count()
        };
        assert!(
            appels("try_release_watcher(") >= 2,
            "les DEUX sorties de la boucle du watcher doivent passer par \
             `try_release_watcher` : une sortie inconditionnelle laisserait une \
             suivante orpheline (#3967).\nappels hors définition : {}",
            appels("try_release_watcher(")
        );
        assert!(
            appels("promouvoir_la_suivante(") >= 1,
            "la promotion doit passer par `promouvoir_la_suivante` : relire la \
             suivante sous le verrou est ce qui évite de démarrer l'élément \
             périmé (#3967)."
        );

        // Le retrait du watcher ne doit exister QU'À un seul endroit : dans
        // `try_release_watcher`, sous condition. C'est exactement le retrait
        // inconditionnel de fin de boucle qui était le défaut.
        assert_eq!(
            production.matches("w.remove(&zone_id)").count(),
            1,
            "`watchers` ne doit être purgé que par `try_release_watcher` : \
             tout autre retrait rouvre la course de #3967."
        );

        // Et la boucle ne doit plus s'arrêter après UNE promotion.
        let debut = production
            .find("fn spawn_gapless_watcher(")
            .expect("le watcher doit exister");
        let fin = production
            .find("/// Annonceur SSDP des renderers")
            .expect("l'annonceur doit suivre le watcher");
        let corps = &production[debut..fin];
        assert!(
            !corps.contains("break"),
            "la boucle du watcher ne doit plus contenir de `break` : elle \
             s'arrêtait après une seule promotion, et la piste suivante posée \
             entre-temps restait sans personne (#3967)."
        );
    }
}

#[cfg(test)]
#[path = "upnp_media_renderer_tests_4324.rs"]
mod session_4324_tests;

#[cfg(test)]
mod publication_de_zone_4626_tests {
    use super::*;

    /// 🔴 renesenses/tune-server-rust#4626 — « Publier cette zone sur le
    /// réseau ».
    ///
    /// L'écriture (`zones/ecriture.rs`, famille 6) est déjà gardée : cocher
    /// écrit `"true"`, décocher SUPPRIME la clé. Ce témoin-là s'arrête à la
    /// table des réglages, et une clé absente ne dit RIEN de ce que le réseau
    /// voit encore. Personne ne gardait la surface UPnP elle-même : ni le 404
    /// de la façade sans opt-in, ni sa disparition au décochage.
    ///
    /// Trou mesuré le 22/09/2026 sur `origin/main` (cf28b013), en établissant
    /// pour le testeur du fil 1867 comment cette publication s'active et se
    /// coupe. Il compte, parce que c'est exactement la promesse faite à
    /// l'écran : décocher retire la zone du réseau.
    ///
    /// Ce que ce témoin NE dit pas, et qui reste vrai (docs/UPNP-RENDERER.md
    /// §4) : aucun `ssdp:byebye` n'est émis. Tune cesse d'annoncer et de
    /// répondre, mais un point de contrôle garde son entrée en cache jusqu'à
    /// l'expiration du `max-age` (1800 s). Le client web le dit désormais à
    /// l'endroit où l'on décoche.
    #[tokio::test]
    async fn la_facade_du_renderer_suit_l_opt_in_de_la_zone() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let zone_id = ZoneRepo::with_backend(state.backend.clone())
            .create("Salon", None, None)
            .unwrap();
        let reglages = SettingsRepo::with_backend(state.backend.clone());
        let cle = format!("zone_{zone_id}_upnp_renderer");

        // 1. Case jamais cochée — le défaut. Rien à voir sur le réseau.
        let reponse = description(State(state.clone()), Path(zone_id)).await;
        assert_eq!(
            reponse.status(),
            StatusCode::NOT_FOUND,
            "sans opt-in, la façade MediaRenderer d'une zone ne doit pas exister"
        );

        // 2. Case cochée : la façade est servie, et c'est bien un MediaRenderer.
        reglages.set(&cle, "true").unwrap();
        let reponse = description(State(state.clone()), Path(zone_id)).await;
        assert_eq!(
            reponse.status(),
            StatusCode::OK,
            "zone publiée : la façade MediaRenderer doit être servie"
        );
        let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .unwrap();
        let xml = String::from_utf8_lossy(&octets).to_string();
        assert!(
            xml.contains("urn:schemas-upnp-org:device:MediaRenderer:1"),
            "la description servie doit annoncer un MediaRenderer:1 — \
             sinon aucun point de contrôle ne proposera la zone.\nservi : {xml}"
        );
        assert!(
            xml.contains("Salon (Tune)"),
            "la façade doit porter le nom de la zone, suffixé une seule fois \
             (#3616).\nservi : {xml}"
        );

        // 3. Décochée : c'est la SUPPRESSION de la clé que fait la route
        //    d'écriture, pas un « false ». La façade disparaît.
        reglages.delete(&cle).unwrap();
        let reponse = description(State(state.clone()), Path(zone_id)).await;
        assert_eq!(
            reponse.status(),
            StatusCode::NOT_FOUND,
            "décocher la case doit retirer la façade MediaRenderer de la zone"
        );

        // 4. Et la valeur littérale « false », qu'un client tiers pourrait
        //    écrire, ne publie pas non plus.
        reglages.set(&cle, "false").unwrap();
        let reponse = description(State(state.clone()), Path(zone_id)).await;
        assert_eq!(
            reponse.status(),
            StatusCode::NOT_FOUND,
            "seule la chaîne « true » publie une zone (`zone_renderer_enabled`)"
        );
    }
}
