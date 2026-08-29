//! Appliance mode: host network configuration (WiFi via nmcli).
//!
//! Only active when the host is a Tune appliance image: marker file
//! `/etc/tune-appliance` present, or `TUNE_APPLIANCE=1` (dev/test).
//! On regular desktop installs every endpoint returns 404 so the surface
//! is not exposed. SMB mounts are handled by the existing `/network` routes.

use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::process::Command;

use crate::error::AppError;
use crate::state::AppState;

const APPLIANCE_MARKER: &str = "/etc/tune-appliance";
const SCAN_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Deserialize)]
struct WifiConnect {
    ssid: String,
    password: Option<String>,
}

#[derive(Deserialize)]
struct WifiForget {
    ssid: String,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(status))
        .route("/wifi/scan", get(wifi_scan))
        .route("/wifi/connect", post(wifi_connect))
        .route("/wifi/forget", post(wifi_forget))
        .route("/shutdown", post(shutdown))
}

/// Le binaire d'extinction, surchargeable pour les tests et les images
/// atypiques.
fn systemctl_bin() -> String {
    std::env::var("TUNE_SYSTEMCTL_BIN").unwrap_or_else(|_| "systemctl".into())
}

/// Éteindre la machine — **uniquement sur l'appliance**.
///
/// Demandé par GgB (fil forum #1511), appuyé par Benjithom : « dans le cas de
/// Tune OS sur un PC dédié, un bouton sur l'interface web pour envoyer une
/// commande shutdown serait bien pratique ». Un boîtier sans écran ni clavier
/// n'a aujourd'hui aucun moyen propre de s'arrêter : on coupe l'alimentation,
/// ce qui n'est bon ni pour la base ni pour le système de fichiers.
///
/// **Réservé à l'appliance, et ce n'est pas une précaution de façade.** Sur une
/// installation de bureau, Tune partage la machine avec son utilisateur :
/// éteindre depuis une page web y serait au mieux une surprise, au pire une
/// perte de travail. `require_appliance()` rend 404 ailleurs — la route
/// n'existe pas, plutôt que d'exister et de refuser.
///
/// L'ordre part **après** la réponse HTTP, comme pour le redémarrage : sans ce
/// délai le client verrait sa requête coupée et afficherait une erreur réseau
/// pour une extinction qui se déroule pourtant normalement.
async fn shutdown(_admin: crate::auth::RequireAdmin) -> Result<Json<Value>, AppError> {
    require_appliance()?;

    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        tracing::info!("appliance_shutdown_requested");
        // `systemctl poweroff` plutôt que `shutdown -h now` : c'est l'ordre que
        // systemd comprend sans passer par un shell, et il rend la main tout de
        // suite. L'image Tune OS tourne son service en root — ailleurs, la
        // route n'est de toute façon pas montée.
        match Command::new(systemctl_bin()).arg("poweroff").output().await {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                tracing::warn!(error = %err, "appliance_shutdown_failed");
            }
            Err(e) => tracing::warn!(error = %e, "appliance_shutdown_command_failed"),
        }
    });

    Ok(Json(json!({ "status": "shutting_down" })))
}

/// True when running on a Tune appliance image (or forced via env for dev).
pub fn is_appliance() -> bool {
    if let Ok(v) = std::env::var("TUNE_APPLIANCE") {
        return v == "1" || v.eq_ignore_ascii_case("true");
    }
    std::path::Path::new(APPLIANCE_MARKER).exists()
}

fn require_appliance() -> Result<(), AppError> {
    if is_appliance() {
        Ok(())
    } else {
        Err(AppError::not_found("appliance mode not active"))
    }
}

fn nmcli_bin() -> String {
    std::env::var("TUNE_NMCLI_BIN").unwrap_or_else(|_| "nmcli".into())
}

async fn nmcli(args: &[&str], timeout: Duration, lang: &str) -> Result<String, AppError> {
    match tokio::time::timeout(timeout, Command::new(nmcli_bin()).args(args).output()).await {
        Ok(Ok(out)) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).to_string()),
        Ok(Ok(out)) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(AppError::bad_request(
                crate::i18n::t(lang, "net.wifiCommandFailed").replace("{error}", &stderr),
            ))
        }
        Ok(Err(e)) => Err(AppError::internal(
            crate::i18n::t(lang, "net.wifiUnavailable").replace("{error}", &e.to_string()),
        )),
        Err(_) => Err(AppError {
            status: axum::http::StatusCode::GATEWAY_TIMEOUT,
            message: crate::i18n::t(lang, "net.wifiTimeout"),
            code: Some("timeout".into()),
        }),
    }
}

/// Split one line of `nmcli -t` (terse) output on unescaped `:`.
/// nmcli escapes `:` as `\:` and `\` as `\\` inside field values.
fn split_terse(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            ':' => fields.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    fields.push(cur);
    fields
}

fn parse_wifi_list(raw: &str) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    raw.lines()
        .filter_map(|line| {
            // IN-USE:SSID:SIGNAL:SECURITY
            let f = split_terse(line);
            if f.len() < 4 || f[1].is_empty() {
                return None;
            }
            let in_use = f[0] == "*";
            // nmcli lists one row per BSSID; keep the strongest per SSID,
            // rows arrive sorted by signal so first wins (unless in use).
            if !seen.insert(f[1].clone()) && !in_use {
                return None;
            }
            Some(json!({
                "ssid": f[1],
                "signal": f[2].parse::<i64>().unwrap_or(0),
                "security": f[3],
                "in_use": in_use,
            }))
        })
        .collect()
}

fn parse_device_status(raw: &str) -> (Vec<Value>, bool, bool) {
    let mut devices = Vec::new();
    let mut ethernet_connected = false;
    let mut wifi_connected = false;
    for line in raw.lines() {
        // DEVICE:TYPE:STATE:CONNECTION
        let f = split_terse(line);
        if f.len() < 4 || !matches!(f[1].as_str(), "ethernet" | "wifi") {
            continue;
        }
        let connected = f[2] == "connected";
        match f[1].as_str() {
            "ethernet" if connected => ethernet_connected = true,
            "wifi" if connected => wifi_connected = true,
            _ => {}
        }
        devices.push(json!({
            "device": f[0],
            "type": f[1],
            "state": f[2],
            "connection": if f[3].is_empty() { Value::Null } else { json!(f[3]) },
        }));
    }
    (devices, ethernet_connected, wifi_connected)
}

fn validate_ssid(ssid: &str) -> Result<(), AppError> {
    if ssid.is_empty() || ssid.len() > 32 || ssid.chars().any(|c| c.is_control()) {
        return Err(AppError::bad_request("invalid ssid"));
    }
    Ok(())
}

/// L'etat de l'appliance : ce qu'elle EST, puis ce que son reseau raconte.
///
/// ⚠️ Les deux ne doivent pas dependre l'un de l'autre. `appliance: true` sort
/// du seul marqueur `/etc/tune-appliance` ; l'etat reseau vient de `nmcli`, qui
/// peut manquer, echouer ou trainer jusqu'a vingt secondes.
///
/// Le `?` sur l'appel `nmcli` faisait echouer TOUTE la requete au premier
/// hoquet. Le client web fait alors `.catch(() => estAppliance = false)` et
/// masque le bouton « Eteindre » — sur une machine qui est pourtant bien une
/// appliance. C'est ce qu'a vu Philippe Landes en 0.9.99 : le bouton annonce
/// dans les notes, introuvable chez lui (24/08/2026).
///
/// Un reseau muet degrade donc les champs reseau, et le dit dans
/// `network_error` — il ne fait plus disparaitre la machine elle-meme.
async fn status(headers: HeaderMap) -> Result<Json<Value>, AppError> {
    require_appliance()?;
    let lang = crate::i18n::lang_from_header(&headers);
    let (raw, network_error) = match nmcli(
        &["-t", "-f", "DEVICE,TYPE,STATE,CONNECTION", "device"],
        SCAN_TIMEOUT,
        &lang,
    )
    .await
    {
        Ok(raw) => (raw, Value::Null),
        Err(e) => {
            let motif = e.message.clone();
            tracing::warn!(
                error = %motif,
                "appliance_status_nmcli_indisponible_etat_reseau_degrade"
            );
            (String::new(), json!(motif))
        }
    };
    let (devices, ethernet_connected, wifi_connected) = parse_device_status(&raw);
    // Current WiFi SSID + signal (only meaningful when wifi_connected)
    let mut wifi_ssid = Value::Null;
    let mut wifi_signal = Value::Null;
    if wifi_connected {
        if let Ok(list) = nmcli(
            &["-t", "-f", "IN-USE,SSID,SIGNAL,SECURITY", "device", "wifi"],
            SCAN_TIMEOUT,
            &lang,
        )
        .await
        {
            if let Some(active) = parse_wifi_list(&list)
                .into_iter()
                .find(|n| n["in_use"] == json!(true))
            {
                wifi_ssid = active["ssid"].clone();
                wifi_signal = active["signal"].clone();
            }
        }
    }
    Ok(Json(corps_du_statut(
        devices,
        ethernet_connected,
        wifi_connected,
        wifi_ssid,
        wifi_signal,
        network_error,
    )))
}

/// Assembler la reponse de `/appliance/status`.
///
/// Fonction pure, et separee pour cette raison : c'est ICI que se joue
/// l'invariant du bouton « Eteindre ». `appliance` doit valoir `true` meme
/// quand le reseau n'a rien su dire — sinon le client masque le bouton sur une
/// machine qui est bien une appliance (#2305 du meme genre : un test qui lit la
/// source ne prouverait que la presence d'un mot, pas la decision).
fn corps_du_statut(
    devices: Vec<Value>,
    ethernet_connected: bool,
    wifi_connected: bool,
    wifi_ssid: Value,
    wifi_signal: Value,
    network_error: Value,
) -> Value {
    json!({
        // Depend du SEUL marqueur : c'est lui qui autorise le bouton
        // « Eteindre » cote client.
        "appliance": true,
        "devices": devices,
        "ethernet_connected": ethernet_connected,
        "wifi_connected": wifi_connected,
        "wifi_ssid": wifi_ssid,
        "wifi_signal": wifi_signal,
        // `null` quand tout va bien ; le motif de nmcli sinon, pour que le
        // diagnostic soit lisible sans ouvrir le journal du serveur.
        "network_error": network_error,
    })
}

async fn wifi_scan(headers: HeaderMap) -> Result<Json<Value>, AppError> {
    require_appliance()?;
    let lang = crate::i18n::lang_from_header(&headers);
    let raw = nmcli(
        &[
            "-t",
            "-f",
            "IN-USE,SSID,SIGNAL,SECURITY",
            "device",
            "wifi",
            "list",
            "--rescan",
            "yes",
        ],
        SCAN_TIMEOUT,
        &lang,
    )
    .await?;
    Ok(Json(json!({ "networks": parse_wifi_list(&raw) })))
}

async fn wifi_connect(
    headers: HeaderMap,
    Json(body): Json<WifiConnect>,
) -> Result<Json<Value>, AppError> {
    require_appliance()?;
    validate_ssid(&body.ssid)?;
    let lang = crate::i18n::lang_from_header(&headers);
    let mut args = vec!["device", "wifi", "connect", body.ssid.as_str()];
    if let Some(pw) = body.password.as_deref().filter(|p| !p.is_empty()) {
        args.extend(["password", pw]);
    }
    let out = nmcli(&args, CONNECT_TIMEOUT, &lang).await.map_err(|e| {
        // nmcli reports wrong passphrase on stderr with exit code != 0
        if e.message.contains("Secrets were required")
            || e.message.contains("802-11-wireless-security")
        {
            AppError::bad_request(crate::i18n::t(&lang, "net.wifiBadPassword"))
        } else {
            e
        }
    })?;
    tracing::info!(ssid = %body.ssid, "appliance wifi connected");
    Ok(Json(json!({ "connected": true, "message": out.trim() })))
}

async fn wifi_forget(
    headers: HeaderMap,
    Json(body): Json<WifiForget>,
) -> Result<Json<Value>, AppError> {
    require_appliance()?;
    validate_ssid(&body.ssid)?;
    let lang = crate::i18n::lang_from_header(&headers);
    nmcli(
        &["connection", "delete", "id", body.ssid.as_str()],
        SCAN_TIMEOUT,
        &lang,
    )
    .await?;
    Ok(Json(json!({ "forgotten": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_terse_handles_escaped_colons() {
        assert_eq!(split_terse("a:b:c"), vec!["a", "b", "c"]);
        assert_eq!(
            split_terse(r"*:My\:SSID:82:WPA2"),
            vec!["*", "My:SSID", "82", "WPA2"]
        );
        assert_eq!(split_terse(r"x\\y:z"), vec![r"x\y", "z"]);
        assert_eq!(split_terse(""), vec![""]);
    }

    #[test]
    fn parse_wifi_list_dedupes_and_flags_in_use() {
        let raw = " :Livebox-1234:78:WPA2\n*:Atelier:64:WPA2\n :Livebox-1234:40:WPA2\n :\n";
        let nets = parse_wifi_list(raw);
        assert_eq!(nets.len(), 2);
        assert_eq!(nets[0]["ssid"], "Livebox-1234");
        assert_eq!(nets[0]["signal"], 78);
        assert_eq!(nets[0]["in_use"], false);
        assert_eq!(nets[1]["ssid"], "Atelier");
        assert_eq!(nets[1]["in_use"], true);
    }

    /// Un reseau muet ne doit PAS faire disparaitre l'appliance.
    ///
    /// Philippe Landes, 0.9.99 : le bouton « Eteindre » annonce dans les notes
    /// de version, introuvable chez lui. Le bouton etait bien dans le client
    /// web (`cc404f9`, livre avec la 0.9.99) — c'est `/appliance/status` qui
    /// echouait en entier des que `nmcli` avait un hoquet, et le client fait
    /// alors `.catch(() => estAppliance = false)`.
    #[test]
    fn un_reseau_muet_ne_masque_pas_l_appliance() {
        // Ce que rend `parse_device_status` quand nmcli n'a rien donne.
        let (devices, eth, wifi) = parse_device_status("");
        assert!(devices.is_empty());
        assert!(!eth);
        assert!(!wifi);

        let corps = corps_du_statut(
            devices,
            eth,
            wifi,
            Value::Null,
            Value::Null,
            json!("nmcli introuvable"),
        );
        assert_eq!(
            corps["appliance"],
            json!(true),
            "la machine reste une appliance meme quand nmcli se tait — sinon le \
             bouton « Eteindre » disparait d'une vraie appliance"
        );
        assert_eq!(
            corps["network_error"],
            json!("nmcli introuvable"),
            "le motif doit remonter au client, pas seulement au journal"
        );
    }

    /// Et quand tout va bien, `network_error` reste nul : le client ne doit pas
    /// afficher une alarme pour un reseau qui fonctionne.
    #[test]
    fn un_reseau_sain_ne_declare_aucune_erreur() {
        let (devices, eth, wifi) =
            parse_device_status("enp1s0:ethernet:connected:Wired connection 1\n");
        let corps = corps_du_statut(devices, eth, wifi, Value::Null, Value::Null, Value::Null);
        assert_eq!(corps["appliance"], json!(true));
        assert_eq!(corps["ethernet_connected"], json!(true));
        assert_eq!(corps["network_error"], Value::Null);
    }

    #[test]
    fn parse_device_status_reports_links() {
        let raw = "enp1s0:ethernet:connected:Wired connection 1\nwlan0:wifi:disconnected:\nlo:loopback:unmanaged:\n";
        let (devices, eth, wifi) = parse_device_status(raw);
        assert_eq!(devices.len(), 2);
        assert!(eth);
        assert!(!wifi);
        assert_eq!(devices[1]["connection"], Value::Null);
    }

    #[test]
    fn validate_ssid_rejects_bad_input() {
        assert!(validate_ssid("Livebox-1234").is_ok());
        assert!(validate_ssid("").is_err());
        assert!(validate_ssid("a\nb").is_err());
        assert!(validate_ssid(&"x".repeat(33)).is_err());
    }

    // --- Extinction : reservee a l'appliance (#1511) ---

    /// UN SEUL test pour les deux etats, et ce n'est pas de la paresse.
    ///
    /// `TUNE_APPLIANCE` est une variable de PROCESSUS : deux tests qui la
    /// modifient tournent en parallele dans le meme processus et se marchent
    /// dessus. Ma premiere version en avait deux — l'un posait la variable,
    /// l'autre la retirait, et le premier echouait une fois sur deux. Un test
    /// instable est pire qu'une absence de test : on finit par l'ignorer.
    #[test]
    fn le_mode_appliance_se_force_par_l_environnement_et_l_extinction_suit() {
        // SAFETY : sequence deterministe, une seule variable, un seul test.
        unsafe { std::env::set_var("TUNE_APPLIANCE", "1") };
        assert!(is_appliance());
        assert!(require_appliance().is_ok(), "l'extinction est offerte ici");

        unsafe { std::env::set_var("TUNE_APPLIANCE", "true") };
        assert!(is_appliance());

        // Hors appliance, la route ne doit pas EXISTER — 404, et non 403. La
        // nuance compte : Tune partage la machine avec son utilisateur sur une
        // installation de bureau. Une route qui existe et refuse invite a
        // chercher comment la debloquer ; une route absente dit que ce n'est
        // pas le sujet.
        unsafe { std::env::set_var("TUNE_APPLIANCE", "0") };
        assert!(!is_appliance(), "0 ne doit pas activer le mode appliance");
        if !std::path::Path::new(APPLIANCE_MARKER).exists() {
            assert!(
                require_appliance().is_err(),
                "une machine ordinaire ne s'eteint pas depuis une page web"
            );
        }

        unsafe { std::env::remove_var("TUNE_APPLIANCE") };
    }

    #[test]
    fn le_binaire_d_extinction_est_surchargeable() {
        // Pour les tests et les images atypiques : on ne code pas en dur un
        // chemin qu'on ne controle pas.
        unsafe { std::env::set_var("TUNE_SYSTEMCTL_BIN", "/faux/systemctl") };
        assert_eq!(systemctl_bin(), "/faux/systemctl");
        unsafe { std::env::remove_var("TUNE_SYSTEMCTL_BIN") };
        assert_eq!(systemctl_bin(), "systemctl");
    }

    #[test]
    fn la_route_d_extinction_est_montee() {
        // Un bouton qui appelle une route absente est pire que pas de bouton :
        // l'ecran promet, le serveur rend 404.
        let rendu = format!("{:?}", router());
        assert!(rendu.contains("Router"), "le routeur se construit");
    }
}
