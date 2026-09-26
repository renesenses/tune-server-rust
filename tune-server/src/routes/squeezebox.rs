use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::CreationDeZone;
use tune_core::outputs::squeezebox::LMS_CLI_PORT;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(squeezebox_status))
        .route("/players", get(list_players))
        .route("/discover", post(discover_players))
        .route("/players/{id}/play", post(play_player))
        .route("/players/{id}/pause", post(pause_player))
        .route("/players/{id}/volume", post(set_player_volume))
        .route("/players/{id}/power", post(power_player))
        .route("/players/{id}/create-zone", post(create_zone_for_player))
}

/// La forme d'UN lecteur telle que le client web la lit (`SqueezeboxPlayer`).
///
/// Le panneau « Squeezebox / Lyrion » des réglages lit `id`, `name`, `model`,
/// `ip` et `connected`. Le serveur n'émettait que `playerid` et `name` : côté
/// navigateur `player.id` et `player.connected` valaient `undefined`, donc la
/// pastille affichait **« Déconnecté »** et le bouton « Créer une zone »
/// (`disabled={!player.connected}`) restait grisé pour TOUS les lecteurs, quel
/// que soit l'état réel de LMS. C'est le « Lyrion non connecté » du fil (#2066).
///
/// `model` et `ip` restent VIDES, et c'est délibéré : Tune n'interroge que
/// `player count ?`, `player id N ?` et `player name N ?`. Ajouter deux
/// commandes CLI de plus sans trace réelle sous les yeux serait deviner le
/// protocole. `connected` dit exactement ce que Tune sait — LMS a énuméré ce
/// lecteur à cet instant précis ; un lecteur que LMS ne cite pas n'apparaît pas
/// du tout dans la liste.
fn joueur_json(player_id: &str, player_name: &str) -> Value {
    json!({
        // `id` est ce que lit la carte du panneau ; `playerid` est conservé
        // parce que `discover_and_register` s'en sert déjà.
        "id": player_id,
        "playerid": player_id,
        "name": player_name,
        "model": "",
        "ip": "",
        "connected": true,
        "power": true,
    })
}

/// La forme de l'ÉTAT complet telle que le client web la lit
/// (`SqueezeboxStatus` : `enabled`, `lms_host`, `lms_discovered`, `players`).
///
/// `players` manquait entièrement dans la réponse de `/squeezebox/status` : le
/// panneau retombait donc sur « Aucun lecteur Squeezebox trouvé » même quand
/// LMS répondait parfaitement. C'est le « pas de platine » du même fil.
fn statut_json(enabled: bool, lms_host: &str, lms_discovered: bool, players: Vec<Value>) -> Value {
    json!({
        "enabled": enabled,
        "lms_host": lms_host,
        "lms_discovered": lms_discovered,
        "players": players,
        "lms_cli": tune_core::slimproto::cli_server::etat_ecoute(),
        "slimproto_udp": tune_core::slimproto::discovery::etat_ecoute(),
    })
}

/// LMS répond, mais aucune platine n'est énumérée. Cela ne démontre pas
/// pourquoi (éteinte, absente, pont non configuré) : ne pas inventer la cause.
fn diagnostic_sans_platine(hote: &str) -> Value {
    json!({
        "code": "lms_sans_platine",
        "message": format!("LMS ({hote}) répond mais n'annonce aucune platine. Vérifier les lecteurs et leur connexion à ce LMS. Pour HQPlayer, vérifier le pont qui doit le présenter comme platine à LMS."),
    })
}

/// La marque que le pont de commande LMS **de Tune** pose dans sa propre
/// réponse `serverstatus` (`slimproto::cli_server::handle_serverstatus`). Un
/// vrai LMS y met un GUID hexadécimal ; celui-ci est une constante littérale.
const UUID_DE_TUNE: &str = "uuid:tune-server";

/// L'adresse configurée désigne-t-elle le pont Squeezebox **de Tune** plutôt
/// qu'un vrai serveur Lyrion / LMS ?
///
/// La question se tranche sur le PROTOCOLE, pas sur l'adresse : c'est le seul
/// critère qui survit à `localhost`, à un nom d'hôte, à l'IP de la machine vue
/// depuis une autre interface ou à un conteneur. La réponse `serverstatus` de
/// Tune porte `uuid:tune-server`, littéralement.
fn reponse_identifie_tune_lui_meme(reponse_serverstatus: &str) -> bool {
    reponse_serverstatus
        .split_whitespace()
        .any(|jeton| jeton == UUID_DE_TUNE)
}

/// Le diagnostic d'un recensement VIDE, sachant ce que `serverstatus` a
/// répondu.
///
/// 🔴 #4703 — Le recensement était structurellement vide chez Yacine, et le
/// message le plus utile n'était pas celui qu'on affichait.
///
/// `parse_lms_host` retombe sur `"localhost"` quand aucun `lms_host` n'est
/// enregistré, et le pont de commande LMS **de Tune** écoute, lui, sur
/// `0.0.0.0:9090` — le port CLI par défaut. Tune s'interroge alors LUI-MÊME :
/// `serverstatus` réussit (donc pas de 502, et le volet « impossible de
/// connecter » disparaît, exactement ce que le testeur rapporte le 22/09),
/// puis `player count ?` rend `player count 0` puisque le registre slimproto
/// de Tune est vide. L'écran affichait « LMS répond mais n'annonce aucune
/// platine » et envoyait chercher le défaut chez Lyrion, où il n'est pas.
///
/// Mesuré en lecture seule sur le .18 le 22/09/2026,
/// `GET /api/v1/squeezebox/status` : `lms_host: "localhost"`,
/// `lms_cli.port: 9090`, `players: []`, et la réponse rendue telle quelle —
/// `serverstatus 0 100 lastscan:0 version:0.9.161 uuid:tune-server …`. Le
/// `uuid:tune-server` est la signature du pont de Tune.
///
/// Ce que cette fonction ne fait PAS : changer le repli `"localhost"`. Un LMS
/// qui tourne sur la même machine prend 9090 en premier, et le pont de Tune se
/// replie alors sur 9091 (#4361) ; l'adresse `localhost` est donc légitime
/// dans ce cas et sa réponse ne porte aucun `uuid:tune-server`. Le critère de
/// protocole distingue les deux, une comparaison d'adresses non.
fn diagnostic_recensement_vide(hote: &str, reponse_serverstatus: &str) -> Value {
    if reponse_identifie_tune_lui_meme(reponse_serverstatus) {
        return json!({
            "code": "lms_est_tune_lui_meme",
            "message": format!("L'adresse configurée ({hote}) désigne le pont de commande Squeezebox de Tune, pas un serveur Lyrion/LMS : Tune s'interroge lui-même, et le recensement des platines restera vide quoi que fasse Lyrion. Saisir l'adresse de votre serveur Lyrion dans Réglages → Squeezebox (port CLI 9090 par défaut)."),
        });
    }
    diagnostic_sans_platine(hote)
}

/// Lit un réglage booléen stocké en texte (`"true"` / `"1"`).
fn drapeau(settings: &SettingsRepo, cle: &str) -> bool {
    settings
        .get(cle)
        .ok()
        .flatten()
        .map(|v| {
            let v = v.trim();
            v.eq_ignore_ascii_case("true") || v == "1"
        })
        .unwrap_or(false)
}

/// Les deux drapeaux que le panneau affiche à côté de la liste des lecteurs.
///
/// `lms_host_auto` retient l'adresse posée par l'auto-configuration mDNS ;
/// sans lui, `lms_discovered` valait toujours `undefined` et l'indication
/// « Auto-détecté : … » ne s'affichait jamais. On compare les deux adresses
/// plutôt que de lire un booléen : dès que l'utilisateur saisit autre chose,
/// l'indication disparaît d'elle-même, sans second écrivain à tenir à jour.
fn drapeaux_reglages(state: &AppState) -> (bool, bool) {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let courant = settings
        .get("lms_host")
        .ok()
        .flatten()
        .or_else(|| settings.get("squeezebox_host").ok().flatten())
        .unwrap_or_default();
    let auto = settings.get("lms_host_auto").ok().flatten();
    (
        drapeau(&settings, "squeezebox_enabled"),
        adresse_auto_detectee(auto.as_deref(), &courant),
    )
}

/// L'adresse en vigueur est-elle celle que la découverte mDNS avait posée ?
///
/// Fonction pure pour que la règle se teste sans base ni réseau.
fn adresse_auto_detectee(auto: Option<&str>, courant: &str) -> bool {
    match auto {
        Some(a) if !a.trim().is_empty() => a.trim() == courant.trim(),
        _ => false,
    }
}

/// Parse the LMS host setting into (host, port).
/// Default CLI port is 9090.
/// The web client saves this as "lms_host"; legacy key is "squeezebox_host".
fn parse_lms_host(state: &AppState) -> (String, u16) {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    // Try "lms_host" first (what the web client actually saves), then fall back to legacy "squeezebox_host"
    let raw = settings
        .get("lms_host")
        .ok()
        .flatten()
        .or_else(|| settings.get("squeezebox_host").ok().flatten())
        .unwrap_or_else(|| "localhost".into());

    let (host, port) = split_lms_host(&raw);
    tracing::debug!(raw = %raw, host = %host, port, "parse_lms_host resolved");
    (host, port)
}

/// Split a user-entered LMS address into (host, port).
///
/// Kept free of `AppState` so the parsing rules are unit-testable: this field is
/// typed by hand and every sloppy variant lands here. Whitespace is trimmed on
/// each side of the `:` separately, not just on the whole string — Yacine had
/// `"192.168.0.34 :9090"` stored, whose trailing space survived the outer
/// `trim()`, rode into the host, and made every single CLI call fail with
/// `invalid socket address syntax` (31 errors in 4 hours of log).
fn split_lms_host(raw: &str) -> (String, u16) {
    // Strip http:// or https:// prefix if user pasted a URL
    let cleaned = raw
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        // Strip trailing path segments (e.g. "192.168.1.7:9000/")
        .trim_end_matches('/')
        .trim();

    match cleaned.split_once(':') {
        Some((host, port)) => {
            let mut port = port.trim().parse::<u16>().unwrap_or(LMS_CLI_PORT);
            // Auto-correct: port 9000 is LMS HTTP, CLI is 9090
            if port == 9000 {
                port = LMS_CLI_PORT;
            }
            (host.trim().to_string(), port)
        }
        None => (cleaned.to_string(), LMS_CLI_PORT),
    }
}

/// Send a raw CLI command to LMS via TCP and return the response line.
fn lms_cli_command(host: &str, port: u16, cmd: &str) -> Result<String, String> {
    let addr = format!("{host}:{port}");
    tracing::debug!(addr = %addr, cmd = %cmd, "lms_cli_command connecting");
    // Resolve rather than `str::parse::<SocketAddr>()`: the latter only accepts
    // an IP literal, so every hostname — including our own "localhost" default
    // — was rejected as "invalid address" before a single packet was sent.
    let sock = addr
        .to_socket_addrs()
        .map_err(|e| {
            tracing::warn!(addr = %addr, error = %e, "lms_cli_command: address not resolvable");
            format!(
                "Adresse LMS invalide ou introuvable ({addr}) : {e}. Verifiez le champ Serveur Squeezebox dans les reglages."
            )
        })?
        .next()
        .ok_or_else(|| {
            tracing::warn!(addr = %addr, "lms_cli_command: address resolved to nothing");
            format!("Adresse LMS {addr} ne resout vers aucune adresse IP.")
        })?;
    let stream = TcpStream::connect_timeout(&sock, Duration::from_secs(5)).map_err(|e| {
        // An unreachable/refused optional LMS is a warning, not an app-level
        // ERROR — logging it at error! flooded Yacine's log (a Daphile box that
        // refuses :9090 every poll). The Err string still reaches the UI.
        tracing::warn!(addr = %addr, error = %e, "lms_cli_command: TCP connect failed");
        format!(
            "Impossible de se connecter au serveur Squeezebox (LMS) sur {addr}: {e}. Verifiez que Logitech Media Server est demarre."
        )
    })?;

    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| format!("set read timeout: {e}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| format!("set write timeout: {e}"))?;

    let mut writer = stream
        .try_clone()
        .map_err(|e| format!("clone stream: {e}"))?;
    let line = format!("{cmd}\n");
    writer
        .write_all(line.as_bytes())
        .map_err(|e| format!("LMS CLI write: {e}"))?;
    writer.flush().map_err(|e| format!("LMS CLI flush: {e}"))?;

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader
        .read_line(&mut response)
        .map_err(|e| format!("LMS CLI read: {e}"))?;

    let decoded = urlencoding::decode(response.trim())
        .map(|s| s.into_owned())
        .unwrap_or_else(|_| response.trim().to_string());

    Ok(decoded)
}

/// Send a player-scoped CLI command.
fn lms_player_command(host: &str, port: u16, player_id: &str, cmd: &str) -> Result<String, String> {
    let encoded_mac = urlencoding::encode(player_id);
    lms_cli_command(host, port, &format!("{encoded_mac} {cmd}"))
}

async fn squeezebox_status(State(state): State<AppState>) -> impl IntoResponse {
    let (host, port) = parse_lms_host(&state);
    let (enabled, lms_discovered) = drapeaux_reglages(&state);
    let lms_host_display = if port == LMS_CLI_PORT {
        host.clone()
    } else {
        format!("{host}:{port}")
    };
    match lms_cli_command(&host, port, "serverstatus 0 100") {
        Ok(resp) => {
            // Le panneau des réglages ne lit QUE `players` : sans cette liste il
            // affiche « Aucun lecteur Squeezebox trouvé » alors que LMS vient de
            // répondre. Le recensement qui échoue ne fait pas échouer l'état —
            // le serveur répond bien, ce sont ses lecteurs qu'on n'a pas su lire,
            // et c'est une ligne de journal, pas un 502.
            let (players, diagnostic) = match list_players_cli(&host, port) {
                Ok(p) => {
                    // #4703 : `resp` est la réponse `serverstatus` qu'on vient
                    // de recevoir. Elle dit à QUI on parle — et quand c'est au
                    // pont de Tune lui-même, « aucune platine » envoie chercher
                    // le défaut chez Lyrion, où il n'est pas.
                    let diagnostic = p
                        .is_empty()
                        .then(|| diagnostic_recensement_vide(&lms_host_display, &resp));
                    (p, diagnostic)
                }
                Err(e) => {
                    tracing::warn!(
                        host = %host, port, error = %e,
                        "squeezebox_status: player listing failed"
                    );
                    (
                        Vec::new(),
                        Some(json!({
                            "code": "lms_recensement_impossible",
                            "message": e,
                        })),
                    )
                }
            };
            let mut body = statut_json(enabled, &lms_host_display, lms_discovered, players);
            if let Some(obj) = body.as_object_mut() {
                // Champs historiques conservés : d'autres clients les lisent.
                obj.insert("diagnostic".into(), json!(diagnostic));
                obj.insert("status".into(), json!("ok"));
                obj.insert("response".into(), json!(resp));
            }
            Json(body).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": e, "lms_host": lms_host_display})),
        )
            .into_response(),
    }
}

async fn list_players(State(state): State<AppState>) -> impl IntoResponse {
    let (host, port) = parse_lms_host(&state);
    match list_players_cli(&host, port) {
        Ok(players) => Json(json!(players)).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

/// Discover players via CLI commands: `player count ?`, then `player id/name {i} ?`
fn list_players_cli(host: &str, port: u16) -> Result<Vec<Value>, String> {
    let count_resp = lms_cli_command(host, port, "player count ?")?;
    // Response: "player count 3"
    let count: usize = count_resp
        .strip_prefix("player count ")
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| {
            format!(
                "LMS ({host}:{port}) a répondu avec un nombre de platines invalide : {count_resp}"
            )
        })?;

    let mut players = Vec::new();
    for i in 0..count {
        let id_resp = lms_cli_command(host, port, &format!("player id {i} ?"))?;
        let name_resp = lms_cli_command(host, port, &format!("player name {i} ?"))?;

        // Response: "player id 0 00:04:20:ab:cd:ef"
        let player_id = id_resp.rsplit(' ').next().unwrap_or("").to_string();
        // Response: "player name 0 Kitchen"
        let player_name = name_resp
            .rsplit(&format!("player name {i} "))
            .next()
            .unwrap_or("Squeezebox")
            .to_string();
        // Better extraction: everything after the last known prefix
        let player_name = if let Some(pos) = name_resp.find(&format!("player name {i} ")) {
            let start = pos + format!("player name {i} ").len();
            name_resp[start..].to_string()
        } else {
            player_name
        };

        if !player_id.is_empty() {
            players.push(joueur_json(&player_id, &player_name));
        }
    }
    Ok(players)
}

async fn play_player(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let (host, port) = parse_lms_host(&state);
    match lms_player_command(&host, port, &id, "play") {
        Ok(_) => Json(json!({"status": "playing"})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

async fn pause_player(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let (host, port) = parse_lms_host(&state);
    match lms_player_command(&host, port, &id, "pause") {
        Ok(_) => Json(json!({"status": "paused"})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(Deserialize)]
struct VolumeBody {
    volume: u8,
}

#[derive(Deserialize)]
struct PowerBody {
    #[serde(default = "default_power_on")]
    state: u8,
}

fn default_power_on() -> u8 {
    1
}

async fn set_player_volume(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<VolumeBody>,
) -> impl IntoResponse {
    let (host, port) = parse_lms_host(&state);
    match lms_player_command(&host, port, &id, &format!("mixer volume {}", body.volume)) {
        Ok(_) => Json(json!({"volume": body.volume})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

async fn power_player(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PowerBody>,
) -> impl IntoResponse {
    let (host, port) = parse_lms_host(&state);
    let label = if body.state == 1 { "on" } else { "off" };
    match lms_player_command(&host, port, &id, &format!("power {}", body.state)) {
        Ok(_) => Json(json!({"power": label})).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

async fn discover_players(State(state): State<AppState>) -> impl IntoResponse {
    let (host, port) = parse_lms_host(&state);
    let (enabled, lms_discovered) = drapeaux_reglages(&state);
    let lms_host_display = if port == LMS_CLI_PORT {
        host.clone()
    } else {
        format!("{host}:{port}")
    };
    match discover_and_register(&state).await {
        Ok(registered) if registered.is_empty() => {
            // Un rafraîchissement ne peut enregistrer aucune sortie : le client
            // affiche déjà le champ error des réponses HTTP non réussies.
            // GET /status conserve, lui, le succès d'un LMS joignable.
            //
            // #4703 : le bouton « Rafraîchir » est la deuxième porte que le
            // testeur pousse, et elle rendait le même message trompeur que le
            // panneau. La sonde `serverstatus` n'a lieu QUE sur ce chemin
            // d'échec, où une poignée de main de plus ne coûte rien.
            let reponse = lms_cli_command(&host, port, "serverstatus 0 1").unwrap_or_default();
            let diagnostic = diagnostic_recensement_vide(&lms_host_display, &reponse);
            let mut body = statut_json(enabled, &lms_host_display, lms_discovered, registered);
            body["discovered"] = json!(0);
            body["error"] = diagnostic["message"].clone();
            body["code"] = diagnostic["code"].clone();
            body["diagnostic"] = diagnostic;
            (StatusCode::CONFLICT, Json(body)).into_response()
        }
        // Le client attend ici la MÊME forme que sur `/status` : c'est le
        // résultat du bouton « Actualiser », et il remplace `squeezeboxStatus`
        // en entier. Rendre une forme différente sur deux routes qui alimentent
        // le même écran, c'est se garantir qu'une des deux vues sera vide.
        Ok(registered) => {
            let mut body = statut_json(
                enabled,
                &lms_host_display,
                lms_discovered,
                registered.clone(),
            );
            if let Some(obj) = body.as_object_mut() {
                obj.insert("discovered".into(), json!(registered.len()));
            }
            Json(body).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(Deserialize)]
struct CreateZoneBody {
    /// Accepté pour ne pas rejeter le corps que le client envoie. Le nom de la
    /// zone reste celui que LMS donne au lecteur : deux sources de vérité pour
    /// un même libellé finissent toujours par diverger.
    #[serde(default)]
    #[allow(dead_code)]
    name: Option<String>,
}

/// `POST /squeezebox/players/{id}/create-zone` — le bouton « Créer une zone ».
///
/// **La route n'existait pas.** Le client l'appelle depuis la v0.8 ; le serveur
/// répondait 404. Le trou était masqué par le bouton lui-même, grisé tant que
/// `player.connected` était `undefined` — c'est-à-dire toujours. Rendre
/// `connected` sans ouvrir la route aurait transformé un bouton mort en 404 :
/// les deux vont ensemble (#2066).
///
/// `{id}` est l'identifiant LMS du lecteur (son adresse MAC), comme sur les
/// routes sœurs `play` / `pause` / `volume` / `power` de ce même routeur.
async fn create_zone_for_player(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(_body): Json<CreateZoneBody>,
) -> impl IntoResponse {
    let device_id = format!("squeezebox-{id}");
    // On passe par la découverte complète plutôt que par un enregistrement ad
    // hoc : c'est elle qui pose la SORTIE puis la zone, et un second chemin
    // d'enregistrement finirait par diverger du premier.
    if let Err(e) = discover_and_register(&state).await {
        return (StatusCode::BAD_GATEWAY, Json(json!({"error": e}))).into_response();
    }
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    match zone_repo.get_by_device_id(&device_id) {
        Ok(Some(zone)) => {
            tracing::info!(id = %device_id, "squeezebox_zone_created_on_demand");
            Json(zone).into_response()
        }
        Ok(None) => {
            tracing::warn!(id = %device_id, "squeezebox_create_zone: player unknown to LMS");
            (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": format!("Le lecteur {id} n'est pas (ou plus) annonce par LMS.")
                })),
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

/// Query LMS for connected players via CLI and register them as Squeezebox outputs + auto-create zones.
/// Called at startup (when squeezebox_enabled=true) and via POST /squeezebox/discover.
pub async fn discover_and_register(state: &AppState) -> Result<Vec<Value>, String> {
    let (lms_host_str, lms_port) = parse_lms_host(state);

    let players = list_players_cli(&lms_host_str, lms_port)?;

    if players.is_empty() {
        tracing::info!(host = %lms_host_str, port = lms_port, "squeezebox_discover: no players found on LMS");
        return Ok(vec![]);
    }

    let mut registered = Vec::new();
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());

    for player in &players {
        let player_id = match player.get("playerid").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => continue,
        };
        let player_name = player
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("Squeezebox")
            .to_string();
        let device_id = format!("squeezebox-{player_id}");

        // The DAC on a Lyrion/Daphile box is exposed to Tune TWICE: as a
        // Squeezebox player (this CLI path) AND as the LMS's UPnP-bridge DLNA
        // renderer — same name AND same host (the LMS box), two protocols. This
        // produced Yacine/Jean-Pierre's duplicate zones. The earlier dedup
        // PREFERRED the DLNA zone and SKIPPED this Squeezebox one — but auto-
        // advance never works on that LMS UPnP bridge (it reports no track
        // duration; 0/196 advances), while it works natively on the Squeezebox
        // zone (LMS gapless). So the user was routed to the broken path and
        // couldn't wake the working Squeezebox zone.
        //
        // Reverse the preference: always register the Squeezebox output so its
        // zone stays wakeable. The DLNA duplicate is DEFERRED — but only
        // passively: it is never removed/offlined here, because SSDP owns its
        // lifecycle and re-onlines it every scan pass, so touching it would just
        // flip-flop. The same-name + same-host match is precise, so a different
        // renderer that merely shares a display name is not affected; matching
        // is used only to log the preference for diagnosability.
        {
            let reg = state.outputs.lock().await;
            let dlna_duplicates =
                reg.conflicting_outputs_same_host(&player_name, "squeezebox", &lms_host_str);
            if !dlna_duplicates.is_empty() {
                drop(reg);
                tracing::info!(
                    name = %player_name,
                    id = %device_id,
                    dlna_duplicates = ?dlna_duplicates,
                    "squeezebox_output_preferred_over_dlna_duplicate"
                );
            }
        }

        // Register the output using the CLI port — but only when it is genuinely
        // new or its LMS host changed. discover_and_register runs every 60s;
        // calling register() unconditionally replaced the output object and
        // re-logged squeezebox_output_registered on every pass (register-thrash
        // + log spam, 1441x). device_id is stable per player, so contains() +
        // an unchanged host means nothing to do.
        let needs_register = {
            let reg = state.outputs.lock().await;
            !reg.contains(&device_id) || reg.host_of(&device_id).as_deref() != Some(&lms_host_str)
        };
        if needs_register {
            let output = tune_core::outputs::squeezebox::SqueezeboxOutput::new(
                player_name.clone(),
                device_id.clone(),
                lms_host_str.clone(),
                lms_port,
            );
            {
                let mut reg = state.outputs.lock().await;
                reg.register(Box::new(output));
            }
            tracing::info!(name = %player_name, id = %device_id, lms_host = %lms_host_str, lms_port, "squeezebox_output_registered");
        }

        // Auto-create zone if not already present. Only log a reconnect on an
        // actual offline→online transition — the previous code logged
        // squeezebox_zone_reconnected on every 60s pass for every live zone.
        //
        // #3529 — ce sondeur tourne toutes les 60 s tant que `squeezebox_enabled`
        // est vrai, et il ne consultait pas « Créer automatiquement les zones ».
        // Un nouveau lecteur branché sur le LMS faisait donc apparaître une
        // zone en moins d'une minute, réglage décoché.
        match zone_repo.get_or_create_si_autorise(
            &player_name,
            Some("squeezebox"),
            &device_id,
            "squeezebox_poller",
        ) {
            Ok(CreationDeZone::Creee(zid)) => {
                tracing::info!(name = %player_name, zone_id = zid, "squeezebox_zone_auto_created");
            }
            Ok(CreationDeZone::Refusee) => {
                tracing::info!(name = %player_name, id = %device_id, "squeezebox_zone_auto_create_disabled_skipping");
            }
            Ok(CreationDeZone::Existante(_)) => {
                let was_online = zone_repo
                    .get_by_device_id(&device_id)
                    .ok()
                    .flatten()
                    .map(|z| z.online)
                    .unwrap_or(false);
                let _ = zone_repo.set_online_by_device(&device_id, true);
                if !was_online {
                    tracing::info!(name = %player_name, id = %device_id, "squeezebox_zone_reconnected");
                }
            }
            Err(e) => {
                tracing::warn!(name = %player_name, id = %device_id, error = %e, "squeezebox_zone_create_failed");
            }
        }

        // Même forme que sur `/status` : `id` désigne le lecteur LMS, comme sur
        // toutes les routes `/players/{id}/…` de ce routeur. L'identifiant de
        // sortie interne reste disponible sous `device_id` — il valait `id`
        // auparavant, ce qui donnait deux sens à la même clé selon la route et
        // aurait envoyé le bouton « Créer une zone » sur `squeezebox-squeezebox-…`.
        let mut entry = joueur_json(&player_id, &player_name);
        if let Some(obj) = entry.as_object_mut() {
            obj.insert("device_id".into(), json!(device_id));
        }
        registered.push(entry);
    }

    Ok(registered)
}

#[cfg(test)]
mod tests {
    use super::{
        LMS_CLI_PORT, adresse_auto_detectee, diagnostic_recensement_vide, list_players_cli,
        reponse_identifie_tune_lui_meme, split_lms_host, statut_json,
    };
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// La réponse RÉELLE du .18, relevée en lecture seule le 22/09/2026 par
    /// `GET /api/v1/squeezebox/status` — champ `response`, recopié tel quel.
    /// `lms_host` y valait `"localhost"` et `lms_cli.port` `9090` : Tune
    /// s'interrogeait lui-même, et le panneau affichait pourtant « LMS répond
    /// mais n'annonce aucune platine » (#4703).
    const SERVERSTATUS_DU_18: &str = "serverstatus 0 100 lastscan:0 version:0.9.161 \
         uuid:tune-server info total albums:0 info total artists:0 info total songs:0 \
         player count:1 other player count:0";

    /// Un VRAI LMS : `uuid` est un GUID hexadécimal, jamais un littéral.
    /// Forme reprise de la documentation du CLI Lyrion.
    const SERVERSTATUS_DUN_VRAI_LMS: &str = "serverstatus 0 100 lastscan:1758470000 \
         version:8.5.2 uuid:2e6f6a1c-9b3d-4f05-8f5a-7d2c1e4b9a30 \
         info total albums:1204 info total artists:517 info total songs:14892 \
         player count:0 other player count:0";

    /// 🔴 #4703 — le recensement rend zéro parce que Tune parle à Tune.
    ///
    /// Le testeur écrit « toujours pas de platine, aucun lecteur trouvé alors
    /// que Lyrion s'ouvre en affichant ma bibliothèque ». Les deux faits sont
    /// vrais en même temps : son Lyrion va bien, et Tune ne l'interroge pas.
    #[test]
    fn une_reponse_serverstatus_de_tune_se_reconnait() {
        assert!(
            reponse_identifie_tune_lui_meme(SERVERSTATUS_DU_18),
            "la réponse mesurée sur le .18 porte `uuid:tune-server` : c'est le \
             pont de commande de Tune qui a répondu, pas Lyrion"
        );
        assert!(
            !reponse_identifie_tune_lui_meme(SERVERSTATUS_DUN_VRAI_LMS),
            "un vrai LMS porte un GUID : le confondre avec Tune ferait dire à \
             l'écran d'aller corriger une adresse pourtant juste"
        );
        assert!(
            !reponse_identifie_tune_lui_meme(""),
            "une sonde muette (LMS injoignable sur ce chemin) ne prouve rien : \
             le message générique reste le bon"
        );
        assert!(
            !reponse_identifie_tune_lui_meme("serverstatus 0 100 uuid:tune-server-de-jp"),
            "le jeton se compare ENTIER : un uuid qui COMMENCE par la marque \
             de Tune n'est pas Tune"
        );
    }

    /// Le diagnostic choisi pour un recensement vide : c'est lui que l'écran
    /// affiche, et c'est lui qui décide où le testeur va chercher.
    #[test]
    fn un_recensement_vide_contre_tune_lui_meme_ne_blame_plus_lyrion() {
        let d = diagnostic_recensement_vide("localhost", SERVERSTATUS_DU_18);
        assert_eq!(
            d["code"], "lms_est_tune_lui_meme",
            "tant que ce code vaut `lms_sans_platine`, l'écran envoie vérifier \
             les lecteurs d'un LMS que Tune n'a jamais interrogé (#4703)"
        );
        let message = d["message"].as_str().expect("un message");
        assert!(
            message.contains("localhost"),
            "le message doit citer l'adresse fautive : {message}"
        );
        assert!(
            message.contains("Lyrion"),
            "le message doit dire quoi saisir à la place : {message}"
        );

        // Le TÉMOIN : un vrai LMS sans platine garde le message d'avant.
        let temoin = diagnostic_recensement_vide("192.168.0.34", SERVERSTATUS_DUN_VRAI_LMS);
        assert_eq!(
            temoin["code"], "lms_sans_platine",
            "un vrai LMS réellement sans platine doit garder son diagnostic : \
             un correctif qui rendrait le nouveau code PARTOUT serait vert \
             au-dessus sans rien prouver"
        );
    }

    /// Un LMS bouchonné : il accepte une connexion par commande, lit la ligne
    /// et rend la réponse prévue. Aucun trafic ne sort de la boucle locale.
    fn faux_lms(reponses: Vec<&'static str>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind du faux LMS");
        let port = listener.local_addr().expect("adresse locale").port();
        std::thread::spawn(move || {
            for reponse in reponses {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let mut lecteur = BufReader::new(stream.try_clone().expect("clone"));
                let mut demande = String::new();
                let _ = lecteur.read_line(&mut demande);
                let mut ecrivain = stream;
                let _ = ecrivain.write_all(format!("{reponse}\n").as_bytes());
                let _ = ecrivain.flush();
            }
        });
        port
    }

    /// Le panneau « Squeezebox / Lyrion » lit `id` et `connected`. Le serveur
    /// n'émettait que `playerid` : la pastille disait « Déconnecté » et
    /// « Créer une zone » restait grisé pour tous les lecteurs (#2066).
    #[test]
    fn un_lecteur_recense_porte_id_et_connected() {
        let port = faux_lms(vec![
            "player count 1",
            "player id 0 00:04:20:ab:cd:ef",
            "player name 0 Salon",
        ]);
        let joueurs = list_players_cli("127.0.0.1", port).expect("le faux LMS repond");
        assert_eq!(joueurs.len(), 1);
        let j = &joueurs[0];
        assert_eq!(
            j["id"], "00:04:20:ab:cd:ef",
            "le client lit `id`, pas `playerid`"
        );
        assert_eq!(j["name"], "Salon");
        assert_eq!(
            j["connected"], true,
            "sans `connected`, la pastille affiche « Deconnecte »"
        );
        assert!(
            j.get("model").is_some() && j.get("ip").is_some(),
            "la carte affiche `model` et `ip` : absents, elle ecrit « undefined »"
        );
        // Le champ historique reste, `discover_and_register` s'en sert.
        assert_eq!(j["playerid"], "00:04:20:ab:cd:ef");
    }

    /// `/squeezebox/status` ne portait aucune liste de lecteurs : le panneau
    /// retombait sur « Aucun lecteur Squeezebox trouve » meme quand LMS
    /// repondait parfaitement.
    #[test]
    fn l_etat_porte_toujours_la_liste_des_lecteurs() {
        let vide = statut_json(true, "192.168.0.34", false, Vec::new());
        assert!(
            vide.get("players").is_some_and(|p| p.is_array()),
            "`players` doit exister meme vide, sinon le panneau ne sait rien afficher"
        );
        assert_eq!(vide["enabled"], true);
        assert_eq!(vide["lms_host"], "192.168.0.34");
        assert_eq!(vide["lms_discovered"], false);
    }

    /// L'indication « Auto-detecte : … » ne doit plus s'afficher des que
    /// l'utilisateur a saisi une autre adresse a la main.
    #[test]
    fn une_adresse_saisie_a_la_main_n_est_plus_dite_auto_detectee() {
        assert!(adresse_auto_detectee(
            Some("192.168.0.34:9090"),
            "192.168.0.34:9090"
        ));
        assert!(adresse_auto_detectee(
            Some(" 192.168.0.34:9090 "),
            "192.168.0.34:9090"
        ));
        assert!(!adresse_auto_detectee(
            Some("192.168.0.34:9090"),
            "192.168.0.99:9090"
        ));
        assert!(!adresse_auto_detectee(None, "192.168.0.34:9090"));
        assert!(!adresse_auto_detectee(Some(""), ""));
    }

    /// Toute route appelee par le client web doit exister ici. `create-zone`
    /// manquait depuis la v0.8 : le bouton du panneau tombait sur un 404 (#2066).
    #[test]
    fn le_routeur_declare_toutes_les_routes_appelees_par_le_client() {
        let source = include_str!("squeezebox.rs");
        for chemin in ["/status", "/discover", "/players/{id}/create-zone"] {
            assert!(
                source.contains(&format!(".route(\"{chemin}\"")),
                "route absente du routeur : {chemin}"
            );
        }
    }

    #[test]
    fn plain_host_gets_the_default_cli_port() {
        assert_eq!(
            split_lms_host("192.168.0.34"),
            ("192.168.0.34".into(), 9090)
        );
        assert_eq!(split_lms_host("localhost"), ("localhost".into(), 9090));
    }

    #[test]
    fn explicit_port_is_kept() {
        assert_eq!(
            split_lms_host("192.168.0.34:9091"),
            ("192.168.0.34".into(), 9091)
        );
    }

    #[test]
    fn http_port_9000_is_corrected_to_the_cli_port() {
        assert_eq!(
            split_lms_host("192.168.0.34:9000"),
            ("192.168.0.34".into(), LMS_CLI_PORT)
        );
    }

    #[test]
    fn a_url_paste_is_reduced_to_host_and_port() {
        assert_eq!(
            split_lms_host("http://192.168.0.34:9000/"),
            ("192.168.0.34".into(), LMS_CLI_PORT)
        );
        assert_eq!(
            split_lms_host("https://lms.local/"),
            ("lms.local".into(), LMS_CLI_PORT)
        );
    }

    /// Yacine's stored value: the space before the colon survived the outer
    /// trim(), rode into the host, and made every CLI call fail with
    /// "invalid socket address syntax".
    #[test]
    fn stray_whitespace_around_the_separator_is_dropped() {
        assert_eq!(
            split_lms_host("192.168.0.34 :9090"),
            ("192.168.0.34".into(), 9090)
        );
        assert_eq!(
            split_lms_host("  192.168.0.34 : 9090  "),
            ("192.168.0.34".into(), 9090)
        );
    }

    #[test]
    fn a_junk_port_falls_back_to_the_default_instead_of_failing() {
        assert_eq!(
            split_lms_host("192.168.0.34:abc"),
            ("192.168.0.34".into(), LMS_CLI_PORT)
        );
    }
}
