use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use tracing::{info, warn};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::history_repo::HistoryRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;

use crate::state::AppState;

#[derive(Deserialize)]
pub(super) struct AdminErrorsQuery {
    lines: Option<usize>,
}

/// Tail window read from the end of the log file. 1 MiB comfortably covers far
/// more than any reasonable `max_lines` of error lines while keeping the read
/// bounded regardless of how large the log has grown.
const ERROR_LOG_TAIL_BYTES: u64 = 1024 * 1024;

pub(super) async fn admin_errors(Query(q): Query<AdminErrorsQuery>) -> Json<Value> {
    let max_lines = q.lines.unwrap_or(100);

    let Ok(log_path) = std::env::var("TUNE_LOG_FILE") else {
        return admin_errors_disabled();
    };

    // Read only the tail of the log, off the async runtime. Reading the whole
    // file synchronously here (it grows to hundreds of MB on a long-running
    // server, worse under heavy random playback) blocked a Tokio worker on every
    // 5s dashboard poll and froze the UI (Jean Valjean #1096 — "F5 pour sortir").
    let result = tokio::task::spawn_blocking(move || read_error_tail(&log_path, max_lines)).await;

    match result {
        Ok(Some((recent, source))) => Json(json!({
            "errors": recent,
            "count": recent.len(),
            "source": source,
        })),
        _ => admin_errors_disabled(),
    }
}

fn admin_errors_disabled() -> Json<Value> {
    Json(json!({
        "errors": [],
        "count": 0,
        "source": null,
        "message": "Set TUNE_LOG_FILE to enable error log viewing",
    }))
}

/// Read the last `ERROR_LOG_TAIL_BYTES` of `log_path`, keep lines that look like
/// errors, and return the most recent `max_lines` of them (newest first).
/// Returns `None` if the file can't be opened/read.
fn read_error_tail(log_path: &str, max_lines: usize) -> Option<(Vec<String>, String)> {
    read_error_tail_windowed(log_path, max_lines, ERROR_LOG_TAIL_BYTES)
}

fn read_error_tail_windowed(
    log_path: &str,
    max_lines: usize,
    tail_bytes: u64,
) -> Option<(Vec<String>, String)> {
    use std::io::{Read, Seek, SeekFrom};

    let mut f = std::fs::File::open(log_path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(tail_bytes);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;

    let text = String::from_utf8_lossy(&buf);
    // If we started mid-file the first line is likely truncated — drop it.
    let body = if start > 0 {
        text.find('\n').map(|nl| &text[nl + 1..]).unwrap_or("")
    } else {
        &text
    };

    Some((filter_error_lines(body, max_lines), log_path.to_string()))
}

/// Keep lines that look like errors and return the most recent `max_lines`
/// of them, newest first.
fn filter_error_lines(body: &str, max_lines: usize) -> Vec<String> {
    body.lines()
        .filter(|l| {
            let lower = l.to_lowercase();
            lower.contains("error") || lower.contains("panic") || lower.contains("fatal")
        })
        .rev()
        .take(max_lines)
        .map(|s| s.to_string())
        .collect()
}

pub(super) async fn admin_connections(State(state): State<AppState>) -> Json<Value> {
    let streamer_sessions = state.streamer.sessions_state();
    let active_streams = streamer_sessions.lock().await.len();
    let outputs = state.outputs.lock().await;
    let registered_outputs = outputs.list().len();

    Json(json!({
        "websocket_connections": 0,
        "active_streams": active_streams,
        "registered_outputs": registered_outputs,
    }))
}

pub(super) async fn admin_discovery(State(state): State<AppState>) -> Json<Value> {
    let scanner = &state.scanner;
    let devices = scanner.devices().await;

    Json(json!({
        "device_count": devices.len(),
        "devices": devices.iter().map(|d| json!({
            "id": d.id,
            "name": d.name,
            "host": d.host,
            "type": format!("{:?}", d.device_type),
        })).collect::<Vec<_>>(),
    }))
}

pub(super) async fn admin_health(State(state): State<AppState>) -> Json<Value> {
    let uptime = state.started_at.elapsed().as_secs();
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let albums = AlbumRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let scan_status = settings
        .get("scan_status")
        .ok()
        .flatten()
        .unwrap_or_else(|| "idle".into());
    let zone_count = ZoneRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let playback_states = state.playback.all_states().await;
    let playing = playback_states
        .iter()
        .filter(|z| z.state == tune_core::playback::PlayState::Playing)
        .count();
    let outputs = state.outputs.lock().await;
    let output_count = outputs.list().len();
    drop(outputs);
    let services = state.services.lock().await;
    let service_count = services.list().len();
    drop(services);
    let disk_space = tune_core::health_monitor::disk_space_gb(&state.config.db_path);
    let (disk_free_gb, disk_total_gb) = disk_space
        .map(|(free, total)| (Some(free), Some(total)))
        .unwrap_or((None, None));

    Json(json!({
        "status": "ok",
        "uptime_seconds": uptime,
        "engine": "rust",
        "version": tune_core::version(),
        "database": {
            "tracks": tracks,
            "albums": albums,
            // #3182 : troisième copie du même « sqlite » en dur. Le moteur se
            // lit sur le backend actif, jamais sur une constante.
            "engine": state.backend.engine().as_str(),
        },
        "playback": {
            "zones_total": zone_count,
            "zones_playing": playing,
        },
        "outputs": output_count,
        "streaming_services": service_count,
        "scan_status": scan_status,
        "disk_free_gb": disk_free_gb,
        "disk_total_gb": disk_total_gb,
    }))
}

pub(super) async fn admin_zones(State(state): State<AppState>) -> Json<Value> {
    let repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let zones = repo.list().unwrap_or_default();
    let mut result = Vec::new();
    for z in &zones {
        let zone_id = z.id.unwrap_or(0);
        let ps = state.playback.get_state(zone_id).await;
        result.push(json!({
            "id": zone_id,
            "name": z.name,
            "output_type": z.output_type,
            "output_device_id": z.output_device_id,
            "state": match ps.state {
                tune_core::playback::PlayState::Playing => "playing",
                tune_core::playback::PlayState::Paused => "paused",
                tune_core::playback::PlayState::Stopped => "stopped",
            },
            "volume": if ps.volume > 0.0 { ps.volume } else { z.volume as f64 / 100.0 },
            // #1274 — lecture en dB du volume ci-dessus, `null` = silence.
            "volume_db": tune_core::audio::volume_scale::linear_to_db(
                if ps.volume > 0.0 { ps.volume } else { z.volume as f64 / 100.0 },
            ),
            "muted": z.muted,
            "current_track": ps.now_playing,
            "position_ms": ps.position_ms,
            "queue_length": ps.queue_length,
        }));
    }
    Json(json!(result))
}

// ---------------------------------------------------------------------------
// Other Tune servers on the network ("Serveurs Tune sur le réseau")
// ---------------------------------------------------------------------------
//
// Peers come from TWO sources, united by `peers_payload` (#2746).
//
// Manually added by IP:port and persisted: the robust path for the environments
// where the reporter hit an empty list (Docker macvlan + Windows), because those
// block the multicast that any auto-discovery relies on.
//
// And discovered over mDNS (`_tune-server._tcp`, already advertised via
// `register_self`): the zero-config path, which was written and left unwired to
// this route — `/system/peers` iterated the manual registry alone, so a network
// where multicast DOES pass still displayed "no Tune server detected".
//
// The two layer without changing the contract: same bare array, same fields.

/// Settings key: JSON array of manually-added `{host, port}` peers.
const TUNE_PEERS_KEY: &str = "tune_peers";

#[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq)]
struct PeerAddr {
    host: String,
    port: u16,
}

fn load_peers(state: &AppState) -> Vec<PeerAddr> {
    SettingsRepo::with_backend(state.backend.clone())
        .get(TUNE_PEERS_KEY)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_peers(state: &AppState, peers: &[PeerAddr]) {
    if let Ok(json) = serde_json::to_string(peers) {
        if let Err(e) = SettingsRepo::with_backend(state.backend.clone()).set(TUNE_PEERS_KEY, &json)
        {
            warn!(error = %e, "tune_peers_persist_failed");
        }
    }
}

fn zone_count(state: &AppState) -> i64 {
    state
        .backend
        .query_one("SELECT COUNT(*) FROM zones", &[])
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
}

/// This server's own summary, read by OTHER Tune servers to populate their
/// "servers on the network" list in one round-trip.
pub(super) async fn peer_info(State(state): State<AppState>) -> Json<Value> {
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    // Le nom choisi par l'utilisateur prime sur le nom d'hôte (#2110) : celui
    // qu'il lit dans son interface est aussi celui que ses autres serveurs
    // liront de lui.
    let nom = crate::routes::system::resolve_server_name(
        SettingsRepo::with_backend(state.backend.clone())
            .get("server_name")
            .ok()
            .flatten()
            .as_deref(),
    );
    Json(json!({
        "name": format!("Tune ({nom})"),
        "version": tune_core::version(),
        "tracks": tracks,
        "zones": zone_count(&state),
    }))
}

/// Query a peer's `/system/peer-info` with a short timeout. `None` = unreachable.
async fn fetch_peer_info(host: &str, port: u16) -> Option<Value> {
    let url = format!("http://{host}:{port}/api/v1/system/peer-info");
    let client = tune_core::http::client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .ok()?;
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<Value>().await.ok()
}

fn peer_json(host: &str, port: u16, info: Option<&Value>) -> Value {
    match info {
        Some(i) => json!({
            "name": i.get("name").and_then(|v| v.as_str()).unwrap_or("Tune"),
            "host": host, "port": port,
            "version": i.get("version").and_then(|v| v.as_str()).unwrap_or(""),
            "tracks": i.get("tracks").and_then(|v| v.as_i64()).unwrap_or(0),
            "zones": i.get("zones").and_then(|v| v.as_i64()).unwrap_or(0),
            "online": true,
        }),
        None => json!({
            "name": format!("{host}:{port}"),
            "host": host, "port": port,
            "version": "", "tracks": 0, "zones": 0, "online": false,
        }),
    }
}

/// L'ADRESSE d'un pair découvert : `host` ET `port`, jamais l'un des deux.
///
/// C'est la clef de déduplication de [`peers_payload`], et c'est le COUPLE
/// parce que rien d'autre n'est commun aux deux sources. L'hôte seul ne
/// convient pas : deux serveurs Tune cohabitent couramment sur une même
/// machine (conteneurs, un serveur de test à côté du serveur de salon), et
/// regrouper par IP les écraserait l'un l'autre — la faute exacte relevée sur
/// le #2942. L'identité annoncée ne convient pas non plus : seule la
/// découverte mDNS publie un `id`, une entrée manuelle n'en a aucun, donc
/// comparer sur elle ne dédoublonnerait jamais rien.
fn peer_addr_of(peer: &Value) -> Option<PeerAddr> {
    let host = peer.get("host")?.as_str()?.trim();
    if host.is_empty() {
        return None;
    }
    let port = u16::try_from(peer.get("port")?.as_u64()?).ok()?;
    Some(PeerAddr {
        host: host.to_string(),
        port,
    })
}

/// Corps de `GET /system/peers` : le registre manuel UNI à la découverte mDNS.
///
/// Les deux sources décrivent la même chose — « les autres serveurs Tune » —
/// et l'écran n'a qu'une liste. Jusqu'au #2746 cette route n'itérait que
/// [`load_peers`] : sur un réseau où le multicast passe et où mDNS voit
/// d'autres serveurs, le panneau affichait quand même « aucun serveur Tune
/// détecté », parce qu'aucun appelant de `discovered_tune_peers()` n'était la
/// route du panneau.
///
/// Trois invariants que le correctif ne doit pas perdre :
///
/// 1. **La forme reste un TABLEAU NU.** `tune-web-client` affecte la réponse
///    directement à `TunePeer[]` (`src/lib/api.ts`, `getTunePeers`), puis lit
///    `.length` et l'itère dans `SettingsView.svelte`. Une enveloppe
///    `{items, total, …}` ne serait pas itérable et viderait l'écran.
/// 2. **`tracks` et `zones` ne sont pas inventés.** Ils sortent de
///    `/system/peer-info` interrogé sur le pair lui-même — le même chemin que
///    pour une entrée manuelle. Un pair injoignable est rendu `online: false`
///    avec des compteurs à zéro, ce que [`peer_json`] fait déjà.
/// 3. **Le registre manuel n'est pas retiré.** Il reste le repli des réseaux
///    qui bloquent le multicast (Docker macvlan, pare-feu Windows) — c'est-à-dire
///    précisément les installations qui ont ouvert le ticket.
///
/// `discovered` est un paramètre et non une lecture interne pour que le test
/// puisse MESURER ce corps en pilotant la découverte : `MdnsScanner` n'expose
/// aucun point d'injection, et un test qui recopierait la fusion au lieu de
/// l'appeler ne la garderait pas.
pub async fn peers_payload(state: &AppState, discovered: &[Value]) -> Value {
    let self_ip = tune_core::discovery::ssdp::get_local_ip().map(|ip| ip.to_string());

    let mut adresses: Vec<PeerAddr> = Vec::new();
    for addr in load_peers(state)
        .into_iter()
        .chain(discovered.iter().filter_map(peer_addr_of))
    {
        if self_ip.as_deref() == Some(addr.host.as_str()) {
            continue; // never list ourselves
        }
        if !adresses.contains(&addr) {
            adresses.push(addr);
        }
    }

    // Un aller-retour HTTP par pair, mais TOUS EN PARALLÈLE. En série, la
    // découverte aurait payé son propre succès : dix serveurs sur le réseau,
    // trois secondes de délai chacun, et la route dépassait les huit secondes
    // après lesquelles le client abandonne (`withTimeout(…, 8_000)`) — l'écran
    // serait redevenu vide, par un autre chemin.
    let fiches = futures_util::future::join_all(
        adresses
            .iter()
            .map(|a| async move { fetch_peer_info(&a.host, a.port).await }),
    )
    .await;

    json!(
        adresses
            .iter()
            .zip(fiches.iter())
            .map(|(a, info)| peer_json(&a.host, a.port, info.as_ref()))
            .collect::<Vec<_>>()
    )
}

pub(super) async fn system_peers(State(state): State<AppState>) -> Json<Value> {
    let discovered = state.discovered_tune_peers().await;
    Json(peers_payload(&state, &discovered).await)
}

#[derive(Deserialize)]
pub(super) struct PeerBody {
    host: String,
    port: Option<u16>,
}

pub(super) async fn add_peer(
    State(state): State<AppState>,
    Json(body): Json<PeerBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let host = body.host.trim().to_string();
    let port = body.port.unwrap_or(8888);
    if host.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "error": "host_required" })),
        )
            .into_response();
    }
    // Validate: a Tune server must actually answer at this address.
    let Some(info) = fetch_peer_info(&host, port).await else {
        return (
            axum::http::StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": "peer_unreachable",
                "message": "No Tune server responded at this address. Check the IP, port, and that the other server is running and reachable.",
            })),
        )
            .into_response();
    };
    let mut peers = load_peers(&state);
    if !peers.iter().any(|p| p.host == host && p.port == port) {
        peers.push(PeerAddr {
            host: host.clone(),
            port,
        });
        save_peers(&state, &peers);
        info!(host = %host, port, "tune_peer_added");
    }
    Json(peer_json(&host, port, Some(&info))).into_response()
}

pub(super) async fn remove_peer(
    State(state): State<AppState>,
    Json(body): Json<PeerBody>,
) -> Json<Value> {
    let host = body.host.trim().to_string();
    let port = body.port.unwrap_or(8888);
    let mut peers = load_peers(&state);
    let before = peers.len();
    peers.retain(|p| !(p.host == host && p.port == port));
    if peers.len() != before {
        save_peers(&state, &peers);
        info!(host = %host, port, "tune_peer_removed");
    }
    Json(json!({ "ok": true }))
}

/// Auto-discovered peer Tune servers (mDNS `_tune-server._tcp`, #1273).
///
/// Complements `/system/peers` (manually-added, persisted peers): this is the
/// zero-config path, empty on networks that block multicast (Docker macvlan,
/// Windows firewall) where the manual list is the fallback.
pub(super) async fn discover_servers(State(state): State<AppState>) -> Json<Value> {
    let servers = state.discovered_tune_peers().await;
    Json(json!({ "total": servers.len(), "servers": servers }))
}

pub(super) async fn listening_stats(State(state): State<AppState>) -> Json<Value> {
    let repo = HistoryRepo::with_backend(state.backend.clone());
    let history = repo.listening_history(30).unwrap_or_default();
    let total_listens = repo.count().unwrap_or(0);
    let total_hours: f64 = history
        .iter()
        .map(|(_, _, ms)| *ms as f64 / 3_600_000.0)
        .sum();
    Json(json!({
        "total_listens": total_listens,
        "total_hours_30d": (total_hours * 100.0).round() / 100.0,
        "daily": history.iter().map(|(day, plays, ms)| json!({
            "day": day, "plays": plays, "hours": (*ms as f64 / 3_600_000.0 * 100.0).round() / 100.0,
        })).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn filter_keeps_only_errors_newest_first_capped() {
        let body = "\
INFO started
ERROR disk full
INFO playing
panic at the disco
WARN nothing
FATAL meltdown";
        // newest-first, all three error-ish lines
        let out = filter_error_lines(body, 10);
        assert_eq!(
            out,
            vec!["FATAL meltdown", "panic at the disco", "ERROR disk full"]
        );
        // max_lines cap keeps the most recent ones
        let capped = filter_error_lines(body, 2);
        assert_eq!(capped, vec!["FATAL meltdown", "panic at the disco"]);
    }

    #[test]
    fn tail_window_drops_partial_first_line() {
        // Unique temp path without external crates, removed on drop (#3030).
        let path = tune_core::test_scratch::scratch_file("tune_admin_errors_test", ".log");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "ERROR very-old-should-be-cut-by-window").unwrap();
        writeln!(f, "ERROR recent-one").unwrap();
        writeln!(f, "INFO tail-noise").unwrap();
        f.flush().unwrap();

        // The file is 72 bytes; a 50-byte window starts inside the first
        // ("very-old") line, so that line is partial and dropped — while the
        // whole "recent-one" line survives.
        let (lines, src) = read_error_tail_windowed(path.to_str().unwrap(), 10, 50).unwrap();
        assert_eq!(src, path.to_str().unwrap());
        assert!(
            !lines.iter().any(|l| l.contains("very-old")),
            "partial first line must be dropped, got {lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains("recent-one")));
    }

    #[test]
    fn tail_missing_file_returns_none() {
        assert!(read_error_tail("/nonexistent/tune/admin/errors.log", 100).is_none());
    }
}
