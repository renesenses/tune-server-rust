//! LMS CLI telnet bridge (port 9090).
//!
//! Exposes a telnet-style command interface compatible with Squeeze-LX
//! and other LMS controllers. Maps CLI commands to Tune's internal
//! playback, zone, and library APIs.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use super::PlayerRegistry;

const CLI_PORT: u16 = 9090;

/// Combien de ports consécutifs sont essayés après le port préféré avant de
/// laisser le système en choisir un (#4361).
///
/// Pourquoi une suite ARRÊTÉE plutôt qu'un port éphémère tout de suite : un
/// port qui change à chaque démarrage oblige à reconfigurer les contrôleurs à
/// chaque redémarrage. `9090` pris par Cockpit un jour l'est encore le
/// lendemain, donc Tune retombe sur `9091` aujourd'hui comme demain. Le port
/// éphémère reste le dernier recours — servir sur un port imprévisible vaut
/// mieux que ne pas servir du tout.
const REPLIS_CONSECUTIFS: u16 = 8;

static ETAT: super::ecoute::JournalEcoute = super::ecoute::JournalEcoute::new();

/// Dernier bind CLI ; absent avant tentative et après arrêt de l'écoute.
pub fn etat_ecoute() -> Option<super::EtatEcoute> {
    ETAT.lire()
}

/// State shared across CLI connections.
pub struct CliState {
    pub players: PlayerRegistry,
    pub server_name: String,
    pub server_version: String,
    pub local_ip: String,
}

/// Ce qu'un échec définitif du pont coûte à l'utilisateur, et ce qu'il peut y
/// faire. Sert aussi bien au journal qu'à l'état lu par les écrans.
const INDISPONIBLE: &str = "Le pont de commande LMS de Tune est indisponible. Choisir un port libre avec TUNE_CLI_PORT puis redémarrer Tune et adapter les contrôleurs. Le LMS externe configuré dans les réglages est indépendant.";

/// Le port que la configuration demande : `TUNE_CLI_PORT`, sinon 9090.
fn port_prefere() -> u16 {
    std::env::var("TUNE_CLI_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(CLI_PORT)
}

/// Les ports essayés, dans l'ordre, pour un port préféré donné.
///
/// Fonction pure : la règle de repli se lit et se teste sans ouvrir de socket.
/// Un port préféré à 0 (écoute éphémère demandée explicitement) n'a pas de
/// repli — c'est déjà « le système choisit ».
fn candidats(prefere: u16) -> Vec<u16> {
    if prefere == 0 {
        return vec![0];
    }
    let mut ports = vec![prefere];
    for pas in 1..=REPLIS_CONSECUTIFS {
        match prefere.checked_add(pas) {
            Some(p) => ports.push(p),
            None => break,
        }
    }
    // Dernier recours : que le système en trouve un, plutôt que pas de pont.
    ports.push(0);
    ports
}

/// Démarre le pont de commande LMS, en se repliant sur un autre port si le port
/// demandé est déjà tenu (#4361).
///
/// 🔴 Le défaut mesuré : sur l'image Tune OS Fedora, **Cockpit** écoute déjà sur
/// 9090. Le bind échouait, le pont ne démarrait pas, et toute télécommande
/// Squeezebox (Squeeze-LX, iPeng, Material) restait sans effet — le seul témoin
/// était une ligne de journal que personne ne lit.
///
/// Deux conséquences distinctes, traitées ici toutes les deux :
///
/// 1. **la collision** : Tune choisit désormais un port plutôt que d'abandonner ;
/// 2. **le silence** : le repli est retenu dans l'état d'écoute avec sa cause
///    (`port_de_repli`) et un message qui nomme les DEUX ports. Cet état sort
///    déjà par `/api/v1/system/diagnostics/network`, `/api/v1/squeezebox/status`
///    et le rapport de bogue : on remplit le contrat existant, on n'en invente
///    pas un second.
///
/// Le port principal du serveur (8888), lui, ne se déplace pas : il échoue et le
/// dit (`bootstrap.rs`). La différence n'est pas une incohérence — déplacer le
/// port HTTP couperait tous les clients d'un coup, alors qu'ici le pont est de
/// toute façon MORT si l'on n'en change pas.
pub async fn start_cli_server(state: Arc<CliState>) {
    let prefere = port_prefere();
    let tentative = ETAT.commencer();

    let mut dernier_echec: Option<(u16, std::io::Error)> = None;
    let mut retenu: Option<TcpListener> = None;
    for candidat in candidats(prefere) {
        match TcpListener::bind(format!("0.0.0.0:{candidat}")).await {
            Ok(l) => {
                retenu = Some(l);
                break;
            }
            Err(e) => {
                warn!(error = %e, port = candidat, "lms_cli_server_bind_failed");
                dernier_echec = Some((candidat, e));
            }
        }
    }

    let Some(listener) = retenu else {
        // Tous les candidats refusés : ce n'est plus une collision de numéro
        // (permission, pile réseau…), et là il n'y a rien à replier.
        if let Some((port, e)) = dernier_echec.as_ref() {
            tentative.echec(*port, "TCP", e, INDISPONIBLE);
        }
        return;
    };

    let port = listener.local_addr().map(|a| a.port()).unwrap_or(prefere);
    if prefere != 0 && port != prefere {
        // Le dégradé est DIT : il ne se subit pas en silence.
        warn!(port, port_prefere = prefere, "lms_cli_server_port_de_repli");
        tentative.ecoute_de_repli(port, "TCP", format!(
            "Le port {prefere} du pont de commande LMS est déjà pris par un autre service (sur Tune OS Fedora, c'est Cockpit). Tune a replié son pont sur le port {port} : configurer les télécommandes Squeezebox (Squeeze-LX, iPeng, Material…) sur ce port, ou imposer un port libre avec TUNE_CLI_PORT puis redémarrer Tune. Le LMS externe configuré dans les réglages est indépendant."
        ));
    } else {
        tentative.ecoute(port, "TCP");
    }
    info!(port, "lms_cli_server_started");

    servir(listener, state).await;
}

/// Même serveur avec port explicite et SANS repli : le port demandé, ou rien.
///
/// Réservé aux appels qui savent déjà quel port ils veulent (écoute éphémère
/// avec 0, bancs d'essai qui doivent voir l'échec). Le démarrage de production
/// passe par [`start_cli_server`], qui se replie.
pub async fn start_cli_server_sur_port(state: Arc<CliState>, port: u16) {
    let tentative = ETAT.commencer();
    let addr = format!("0.0.0.0:{port}");
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            tentative.echec(port, "TCP", &e, INDISPONIBLE);
            warn!(error = %e, port, "lms_cli_server_bind_failed");
            return;
        }
    };

    let port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    tentative.ecoute(port, "TCP");
    info!(port, "lms_cli_server_started");

    servir(listener, state).await;
}

/// La boucle d'acceptation, commune aux deux entrées.
async fn servir(listener: TcpListener, state: Arc<CliState>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                info!(peer = %peer, "lms_cli_client_connected");
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_cli_client(stream, peer, state).await {
                        debug!(peer = %peer, error = %e, "lms_cli_client_error");
                    }
                    info!(peer = %peer, "lms_cli_client_disconnected");
                });
            }
            Err(e) => {
                warn!(error = %e, "lms_cli_accept_error");
                // Le raisonnement de ce delai — EMFILE/ENFILE rendus
                // immediatement, un coeur brule, le journal noye — vaut pour
                // les trois autres boucles d'ecoute. Il vit desormais dans
                // `temporisation_reseau`, en un seul endroit, plutot que
                // recopie quatre fois. Meme valeur, meme effet (#2156).
                crate::temporisation_reseau::temporiser_apres_erreur_reseau().await;
            }
        }
    }
}

async fn handle_cli_client(
    stream: TcpStream,
    _peer: SocketAddr,
    state: Arc<CliState>,
) -> Result<(), String> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        debug!(cmd = %line, "lms_cli_command_received");
        let response = handle_command(&line, &state).await;
        let out = format!("{response}\n");
        if writer.write_all(out.as_bytes()).await.is_err() {
            break;
        }
    }

    Ok(())
}

async fn handle_command(line: &str, state: &Arc<CliState>) -> String {
    let parts: Vec<&str> = line.splitn(2, ' ').collect();
    let cmd = parts[0];

    // Check if first token is a player MAC address (contains ":")
    if cmd.contains(':') || cmd.contains("%3A") {
        // Player-scoped command: "<mac> <command>"
        let player_id = urldecode(cmd);
        let sub_cmd = parts.get(1).unwrap_or(&"");
        return handle_player_command(&player_id, sub_cmd, state).await;
    }

    // Global commands
    let full = line;
    match cmd {
        "login" => format!("{full} ******"),
        "listen" => full.to_string(),
        "can" => handle_can(full),
        "player" => handle_player_query(full, state).await,
        "players" => handle_players(full, state).await,
        "serverstatus" => handle_serverstatus(full, state).await,
        "status" => handle_global_status(full, state).await,
        "pref" => handle_pref(full, state),
        "version" => format!("version {}", state.server_version),
        "connected" => "connected 1".to_string(),
        "subscribe" => full.to_string(),
        _ => {
            debug!(cmd = full, "lms_cli_unknown_command");
            full.to_string()
        }
    }
}

/// Handle "player count ?" and "player id/name N ?" queries.
async fn handle_player_query(line: &str, state: &Arc<CliState>) -> String {
    let players = state.players.lock().await;
    let count = players.len();

    if line.contains("count") {
        return format!("player count {count}");
    }
    // "player id 0 ?" → return MAC of player at index 0
    if line.contains(" id ") {
        let mac = players
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| "00:11:22:33:44:55".to_string());
        let idx = line
            .split_whitespace()
            .find_map(|t| t.parse::<usize>().ok())
            .unwrap_or(0);
        return format!("player id {idx} {mac}");
    }
    // "player name 0 ?" → return name of player at index 0
    if line.contains(" name ") {
        let name = players
            .values()
            .next()
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "Tune".to_string());
        let idx = line
            .split_whitespace()
            .find_map(|t| t.parse::<usize>().ok())
            .unwrap_or(0);
        return format!("player name {idx} {}", cli_encode(&name));
    }

    line.to_string()
}

/// Handle global "status - 1 subscribe:-" command.
async fn handle_global_status(line: &str, state: &Arc<CliState>) -> String {
    let players = state.players.lock().await;
    let mac = players
        .keys()
        .next()
        .cloned()
        .unwrap_or_else(|| "00:11:22:33:44:55".to_string());
    let name = players
        .values()
        .next()
        .map(|p| cli_encode(&p.name))
        .unwrap_or_else(|| "Tune".to_string());
    let elapsed = players
        .values()
        .next()
        .map(|p| p.elapsed_ms / 1000)
        .unwrap_or(0);
    drop(players);

    // Return a status response that satisfies Squeeze-LX's subscription handshake
    format!(
        "{line} player_name:{name} player_connected:1 \
         player_ip:{ip}:3483 power:1 signalstrength:0 mode:play \
         time:{elapsed} duration:300 \
         playlist%20repeat:0 playlist%20shuffle:0 \
         playlist%20mode:off playlist_cur_index:0 \
         playlist_timestamp:0 playlist_tracks:1 \
         mixer%20volume:80 playerid:{mac}",
        ip = state.local_ip,
    )
}

fn handle_can(line: &str) -> String {
    // Squeeze-LX checks capabilities. Answer 0 for unimplemented features.
    // e.g. "can material-skin items ?" → "can material-skin items 0"
    let without_q = line.trim_end_matches(" ?").trim_end_matches("?");
    format!("{without_q} 0")
}

async fn handle_players(line: &str, state: &Arc<CliState>) -> String {
    let players = state.players.lock().await;
    let count = players.len();

    if players.is_empty() {
        // No real players connected — return count:0.
        // Do NOT expose a virtual player or the squeezebox poller
        // will auto-create a ghost zone that steals playback.
        return format!("{line} count:0");
    }

    let mut resp = format!("{line} count:{count}");
    for (i, (mac, player)) in players.iter().enumerate() {
        let ip = player.addr.ip();
        resp.push_str(&format!(
            " playerindex:{i} playerid:{mac} uuid:tune-{mac} \
             ip:{ip}:3483 name:{name} model:squeezelite \
             modelname:Squeezelite power:1 isplaying:1 connected:1 firmware:tune",
            name = cli_encode(&player.name),
        ));
    }
    resp
}

async fn handle_serverstatus(line: &str, state: &Arc<CliState>) -> String {
    let players = state.players.lock().await;
    let count = players.len().max(1);
    format!(
        "{line} lastscan:0 version:{ver} uuid:tune-server \
         info%20total%20albums:0 info%20total%20artists:0 info%20total%20songs:0 \
         player%20count:{count} other%20player%20count:0",
        ver = state.server_version,
    )
}

fn handle_pref(line: &str, _state: &Arc<CliState>) -> String {
    if line.contains('?') {
        let key = line.split_whitespace().nth(1).unwrap_or("unknown");
        let value = match key {
            "httpport" => std::env::var("TUNE_PORT").unwrap_or_else(|_| "8888".into()),
            "language" => "en".to_string(),
            "skin" => "Default".to_string(),
            _ => String::new(),
        };
        format!("pref {key} {value}")
    } else {
        line.to_string()
    }
}

async fn handle_player_command(player_id: &str, cmd: &str, state: &Arc<CliState>) -> String {
    let parts: Vec<&str> = cmd.splitn(2, ' ').collect();
    let action = parts[0];
    let args = parts.get(1).unwrap_or(&"");

    let encoded_id = cli_encode_raw(player_id);

    match action {
        "status" => {
            // Squeeze-LX requests player status
            let players = state.players.lock().await;
            let elapsed = players
                .get(player_id)
                .map(|p| p.elapsed_ms / 1000)
                .unwrap_or(0);
            format!(
                "{encoded_id} status {args} \
                 player_name:Tune mode:play time:{elapsed} \
                 duration:300 playlist%20repeat:0 \
                 playlist%20shuffle:0 mixer%20volume:80 \
                 playlist_cur_index:0 playlist_tracks:1"
            )
        }
        "mixer" => format!("{encoded_id} mixer {args}"),
        "play" | "pause" | "stop" | "playlist" => {
            format!("{encoded_id} {cmd}")
        }
        "time" => format!("{encoded_id} time {args}"),
        "mode" => format!("{encoded_id} mode play"),
        "connected" => format!("{encoded_id} connected 1"),
        "signalstrength" => format!("{encoded_id} signalstrength 100"),
        "name" => {
            let players = state.players.lock().await;
            let name = players
                .get(player_id)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "Tune".to_string());
            format!("{encoded_id} name {}", cli_encode(&name))
        }
        "power" => format!("{encoded_id} power 1"),
        _ => {
            debug!(player = player_id, cmd, "lms_cli_unknown_player_command");
            format!("{encoded_id} {cmd}")
        }
    }
}

/// Encode a value for CLI response — percent-encode spaces but keep colons raw.
fn cli_encode(s: &str) -> String {
    s.replace(' ', "%20").replace('\n', "")
}

/// Encode a player ID for response — keep colons raw (critical for Squeeze-LX).
fn cli_encode_raw(s: &str) -> String {
    s.replace(' ', "%20")
}

/// Decode a percent-encoded value.
fn urldecode(s: &str) -> String {
    urlencoding::decode(s)
        .unwrap_or_else(|_| s.into())
        .to_string()
}
