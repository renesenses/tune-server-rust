use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::error::AppError;
use crate::smb;
use crate::state::AppState;

#[derive(Deserialize)]
struct CreateMount {
    mount_type: Option<String>,
    server: String,
    share: String,
    mount_path: String,
    username: Option<String>,
    password: Option<String>,
}

#[derive(Deserialize)]
struct ScanHostQuery {
    host: String,
    protocol: Option<String>,
    username: Option<String>,
    password: Option<String>,
}

#[derive(Deserialize)]
struct MountRequest {
    host: String,
    share_name: String,
    username: Option<String>,
    password: Option<String>,
    mount_path: Option<String>,
    #[serde(default)]
    dry_run: bool,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/mounts", get(list_mounts).post(create_mount))
        .route("/mounts/{id}", axum::routing::delete(delete_mount))
        .route("/media-servers", get(list_media_servers))
        .route("/shares", get(list_shares))
        .route("/scan-host", get(scan_host))
        .route("/smb/discover", get(list_smb_shares).post(trigger_smb_scan))
        .route("/smb/mounts", get(list_smb_mounts))
        .route("/smb/mount", post(mount_smb_share))
        .route("/media-servers/{id}/browse", get(browse_media_server))
        // Phase 2 du chantier `unifier-serveurs-upnp-et-bibliotheque` :
        // indexer UNE source, choisie à la main. Voir
        // `routes/indexation_upnp.rs` pour la clé d'identité retenue et la
        // mesure qui a écarté l'`ObjectID`.
        .route(
            "/media-servers/{id}/indexer",
            post(crate::routes::indexation_upnp::indexer_une_source),
        )
        .route("/media-servers/{id}/search", get(search_media_server))
        .route(
            "/media-servers/{id}/item/{item_id}/stream-url",
            get(media_server_stream_url),
        )
        .route(
            "/media-servers/{id}/item/{item_id}/play/{zone_id}",
            post(play_media_server_item),
        )
        .route("/mounts/test", post(test_mount))
        .route("/shares/{id}", get(get_share_detail))
}

async fn list_mounts(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let rows = state.backend.query_many(
        "SELECT id, mount_type, server, share, mount_path, username, active FROM network_mounts ORDER BY id", &[],
    ).map_err(|e| AppError::internal(e))?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "mount_type": r.get(1).and_then(|v| v.as_string()),
                "server": r.get(2).and_then(|v| v.as_string()),
                "share": r.get(3).and_then(|v| v.as_string()),
                "mount_path": r.get(4).and_then(|v| v.as_string()),
                "username": r.get(5).and_then(|v| v.as_string()),
                "active": r.get(6).and_then(|v| v.as_i64()).unwrap_or(1) != 0,
            })
        })
        .collect();
    Ok(Json(json!(items)))
}

/// L'identite d'un montage : le quadruplet que l'index unique protege
/// (migration 83). Rend l'id de la ligne existante, s'il y en a une.
///
/// GgB (fil 1562, #2453) : sans ce controle, une seconde validation du meme
/// formulaire ajoutait une ligne jumelle que l'ecran Emplacements affichait
/// indefiniment. Depuis l'index unique, elle echouerait a la place — une 500
/// pour un geste anodin. On rend donc la ligne deja la.
fn montage_existant(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    mount_type: &str,
    server: &str,
    share: &str,
    mount_path: &str,
) -> Option<i64> {
    use tune_core::db::backend::ToSqlValue;
    backend
        .query_one(
            "SELECT id FROM network_mounts \
             WHERE mount_type = ? AND server = ? AND share = ? AND mount_path = ?",
            &[
                &mount_type as &dyn ToSqlValue,
                &server as &dyn ToSqlValue,
                &share as &dyn ToSqlValue,
                &mount_path as &dyn ToSqlValue,
            ],
        )
        .ok()
        .flatten()
        .and_then(|row| row.first().and_then(|v| v.as_i64()))
}

async fn create_mount(
    State(state): State<AppState>,
    Json(body): Json<CreateMount>,
) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;
    let mount_type = body.mount_type.unwrap_or_else(|| "smb".into());
    if let Some(id) = montage_existant(
        &state.backend,
        &mount_type,
        &body.server,
        &body.share,
        &body.mount_path,
    ) {
        tracing::info!(id, server = %body.server, share = %body.share, "montage_reseau_deja_enregistre");
        return (StatusCode::OK, Json(json!({ "id": id, "existant": true }))).into_response();
    }
    match state.backend.execute_returning_id(
        "INSERT INTO network_mounts (mount_type, server, share, mount_path, username, password) VALUES (?, ?, ?, ?, ?, ?)",
        &[&mount_type as &dyn ToSqlValue, &body.server as &dyn ToSqlValue, &body.share as &dyn ToSqlValue, &body.mount_path as &dyn ToSqlValue, &body.username as &dyn ToSqlValue, &body.password as &dyn ToSqlValue],
    ) {
        Ok(id) => {
            (StatusCode::CREATED, Json(json!({ "id": id }))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_mount(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;
    let p1 = if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
        "$1".to_string()
    } else {
        "?".to_string()
    };
    state
        .backend
        .execute(
            &format!("DELETE FROM network_mounts WHERE id = {p1}"),
            &[&id as &dyn ToSqlValue],
        )
        .ok();
    StatusCode::NO_CONTENT
}

/// Verser dans le registre DURABLE ce que la découverte tient en mémoire.
///
/// Le registre en mémoire (`state.media_servers`) est la vue de la couche
/// SSDP ; la table `media_servers` est la vue qui SURVIT. Les tenir d'accord
/// ici, plutôt que dans `discovery_setup`, a une raison mesurée : la couche
/// SSDP n'émet un évènement qu'à la PREMIÈRE découverte
/// (`SsdpEvent::MediaServerDiscovered`) — un serveur déjà connu qui se
/// réannonce voit sa fraîcheur remise à zéro EN PLACE, dans la carte du
/// SCANNER, sans que rien ne soit publié. Un branchement sur le seul évènement
/// daterait donc chaque serveur de sa première apparition et jamais de la
/// dernière — c'est-à-dire exactement le défaut qu'on corrige.
///
/// ⚠️ Ce raisonnement portait juste sur la couche SSDP et lisait la MAUVAISE
/// carte. `state.media_servers` n'EST PAS la carte du scanner : c'en est une
/// copie, dont le seul écrivain est ce même évènement
/// (`discovery_setup.rs`, `media_servers.lock().await.insert(...)`). Elle était
/// donc gelée à la première découverte, son `Instant` figé pour toujours, et
/// `horodatage_il_y_a(ms.age())` y rendait `maintenant - (maintenant - t0)`,
/// soit `t0`, constant. Mesure du 14/09/2026 sur le `.18` : quatre serveurs,
/// et `first_seen_at == last_seen_at` à la seconde près sur les quatre, à
/// 56 s d'intervalle entre deux relevés (#4125).
///
/// On reprend donc d'abord la fraîcheur de la carte du scanner — celle qui,
/// elle, est tenue à jour — avant de verser quoi que ce soit en base.
///
/// L'observation est datée `maintenant - âge` : on écrit ce que la découverte
/// SAIT, jamais « vu à l'instant ». C'est la contre-épreuve de la phase 1 —
/// un serveur éteint ne doit pas ressusciter parce qu'on a relu la liste.
///
/// Idempotent, quelques lignes au plus, et sans effet de bord visible : une
/// observation ne réécrit ni `first_seen_at`, ni `active`.
async fn synchroniser_le_registre(state: &AppState) {
    use tune_core::db::media_server_repo::{
        MediaServerRepo, ObservationServeurRecue, horodatage_il_y_a,
    };

    // La carte du scanner est la seule tenue à jour. La reprise n'insère ni ne
    // retire rien : voir `reprendre_la_fraicheur`, et le rideau de #3688.
    {
        let vue_du_balayage = state.scanner.media_servers().await;
        let mut registre = state.media_servers.lock().await;
        let reprises = tune_core::discovery::presence_serveur::reprendre_la_fraicheur(
            &mut registre,
            vue_du_balayage,
        );
        debug!(reprises, "media_server_fraicheur_reprise_du_balayage");
    }

    // Le verrou est relâché AVANT d'écrire en base : une écriture SQLite sous
    // le mutex du registre ferait attendre la découverte SSDP, qui le prend à
    // chaque annonce reçue.
    let observations: Vec<(ObservationServeurRecue, String)> = {
        let servers = state.media_servers.lock().await;
        servers
            .values()
            .map(|ms| {
                let vu_le = horodatage_il_y_a(ms.age().as_secs() as i64);
                (
                    ObservationServeurRecue {
                        udn: ms.id.clone(),
                        name: ms.name.clone(),
                        manufacturer: Some(ms.manufacturer.clone()).filter(|s| !s.is_empty()),
                        model: Some(ms.model.clone()).filter(|s| !s.is_empty()),
                        device_type: "upnp_media_server".into(),
                        location: ms.location.clone(),
                        content_directory_url: Some(ms.content_directory_url.clone())
                            .filter(|s| !s.is_empty()),
                        host: Some(ms.host.clone()).filter(|s| !s.is_empty()),
                        port: Some(i64::from(ms.port)),
                        max_age_secs: None,
                    },
                    vu_le,
                )
            })
            .collect()
    };

    let repo = MediaServerRepo::with_backend(state.backend.clone());
    for (obs, vu_le) in &observations {
        if let Err(e) = repo.enregistrer_observation_a(obs, vu_le) {
            warn!(udn = %obs.udn, error = %e, "media_server_registre_ecriture_echouee");
        }
    }
}

async fn list_media_servers(State(state): State<AppState>) -> Json<Value> {
    use tune_core::discovery::presence_serveur::{
        ObservationServeur, PART_MAX_ABSENCE_SIMULTANEE, PLANCHER_PLAFOND_ABSENCE,
        SERVEUR_ABSENT_APRES, qualifier_le_registre,
    };

    synchroniser_le_registre(&state).await;

    let repo =
        tune_core::db::media_server_repo::MediaServerRepo::with_backend(state.backend.clone());
    let enregistres = match repo.lister() {
        Ok(v) => v,
        Err(e) => {
            // On ne rend PAS une liste vide sur une erreur de base : une liste
            // vide se lit « aucun serveur », et c'est un mensonge de plus. Le
            // registre en mémoire prend le relais, dégradé mais honnête.
            warn!(error = %e, "media_server_registre_lecture_echouee");
            Vec::new()
        }
    };

    // Le calcul de fraîcheur porte sur la LISTE, jamais sur la ligne : le
    // plafond de bascule en masse ne peut se juger que sur l'ensemble — même
    // raison que `verdict_purge` (`routes/system/scan.rs:454-481`).
    let ages: Vec<Option<i64>> = enregistres.iter().map(|s| s.age_secs()).collect();
    let observations: Vec<ObservationServeur<'_>> = enregistres
        .iter()
        .zip(&ages)
        .map(|(s, age)| ObservationServeur {
            udn: &s.udn,
            age_secs: *age,
            disparition_confirmee: s.absence_reason.as_deref() == Some("disparition_confirmee"),
        })
        .collect();
    let verdict = qualifier_le_registre(&observations);

    // Le constat écrit suit le calcul : la table doit pouvoir se relire seule,
    // sans rejouer la qualification (c'est `network_mounts.mount_state`).
    for (udn, presence) in &verdict.presences {
        let ecriture = match presence.raison() {
            Some(raison) => repo.marquer_absent(udn, raison.code()),
            None => Ok(()),
        };
        if let Err(e) = ecriture {
            warn!(udn = %udn, error = %e, "media_server_constat_ecriture_echouee");
        }
    }

    let items: Vec<Value> = enregistres
        .iter()
        .zip(&ages)
        .map(|(s, age)| {
            let presence = verdict
                .pour(&s.udn)
                .unwrap_or(tune_core::discovery::presence_serveur::PresenceServeur::Present);
            json!({
                "id": s.udn,
                "name": s.name,
                "manufacturer": s.manufacturer.clone().unwrap_or_default(),
                "model": s.model.clone().unwrap_or_default(),
                "host": s.host.clone().unwrap_or_default(),
                "port": s.port.unwrap_or(0),
                "location": s.location,
                // Le marquage demandé par Bertrand dans le fil forum 1425 :
                // l'interface grise un serveur qui ne répond plus au lieu de
                // le faire clignoter en le retirant puis le remettant. Champs
                // AJOUTÉS — aucun client existant ne casse (#2139).
                //
                // Ce que `reachable` MESURE, puisque son nom laissait le doute
                // (#4125) : « le balayage SSDP l'a revu il y a moins de
                // `MEDIA_SERVER_STALE_AFTER` (900 s) ». Ce n'est PAS une sonde
                // HTTP, ce n'est PAS un `Browse` réussi — le registre ne sonde
                // personne au moment de rendre la liste, et il ne doit pas :
                // la route serait alors aussi lente que le plus lent des
                // serveurs du réseau.
                //
                // Il se calcule désormais sur le MÊME âge que `presence`, celui
                // du registre durable. Il lisait jusqu'ici un second `Instant`,
                // celui de la copie en mémoire : deux horloges pour une seule
                // question, et l'une d'elles était gelée. Trois serveurs vivants
                // et qui se réannonçaient correctement portaient `reachable:
                // false` sur le `.18` le 14/09/2026, quinze minutes après leur
                // découverte et pour toujours.
                //
                // Les deux seuils restent distincts et cette hiérarchie est
                // voulue : 900 s marque « plus revu depuis un moment » sans
                // aucune conséquence, 5 400 s (`SERVEUR_ABSENT_APRES`) retire
                // des propositions. Un serveur peut donc être `reachable:
                // false` et `proposable: true` — c'est la zone grise, et c'est
                // exactement ce que le fil 1425 demandait de montrer.
                "reachable": age.is_some_and(|a| tune_core::discovery::ssdp::media_server_reachable(
                    Duration::from_secs(a.max(0) as u64)
                )),
                "last_seen_secs": age.unwrap_or(0),
                // #2219 phase 1 — ce que la liste ne savait pas dire.
                //
                // `last_seen_at` est la réparation la plus concrète : la date
                // était un `Instant` `#[serde(skip)]` (`ssdp.rs:146`), donc
                // rien d'absolu n'était exposable, et le client ne pouvait que
                // relire un âge relatif à un instant qu'il ignorait.
                "presence": presence.code(),
                "proposable": presence.proposable(),
                "absence_reason": presence.raison().map(|r| r.code()),
                "first_seen_at": s.first_seen_at,
                "last_seen_at": s.last_seen_at,
                // L'INTENTION, distincte du constat (`network_mounts`, #1916).
                "active": s.active,
            })
        })
        .collect();

    let total = items.len();
    let proposables = items
        .iter()
        .filter(|i| i.get("proposable").and_then(Value::as_bool) == Some(true))
        .count();
    let mut sortie = json!({
        "items": items,
        "total": total,
        // Combien sont réellement utilisables. `total` seul laissait croire
        // que trois serveurs vus il y a 23 h étaient trois serveurs.
        "proposables": proposables,
        "absent_apres_secs": SERVEUR_ABSENT_APRES.as_secs(),
    });
    // Le refus du plafond est PUBLIÉ, avec ses nombres — comme le refus de
    // purge de `scan.rs:531-536`. Un refus muet serait indébogable.
    if let Some(refus) = verdict.bascule_refusee {
        sortie["bascule_en_masse_refusee"] = json!({
            "candidats": refus.candidats,
            "total": refus.total,
            "plafond": refus.plafond,
            "part_max": PART_MAX_ABSENCE_SIMULTANEE,
            "plancher": PLANCHER_PLAFOND_ABSENCE,
            "confirmation_apres_secs": refus.confirmation_apres_secs,
            "motif": "une bascule de cette ampleur est bien plus souvent notre propre lien réseau \
                      qui tombe qu'une extinction simultanée. Les serveurs restent proposés ; \
                      si le silence dure une seconde fenêtre, l'absence sera actée.",
        });
    }
    Json(sortie)
}

// ---------------------------------------------------------------------------
// SMB discovery and mount management
// ---------------------------------------------------------------------------

/// Discover network shares via mDNS service browsing (_smb._tcp).
async fn list_shares() -> Json<Value> {
    let result = tokio::task::spawn_blocking(|| {
        let daemon = mdns_sd::ServiceDaemon::new().ok()?;
        let receiver = daemon.browse("_smb._tcp.local.").ok()?;
        let mut shares = Vec::new();
        let mut seen = std::collections::HashSet::new();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match receiver.recv_timeout(Duration::from_millis(500)) {
                Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                    let host = info.get_hostname().trim_end_matches('.').to_string();
                    let addrs: Vec<String> = info
                        .get_addresses()
                        .iter()
                        .map(|a| a.to_ip_addr().to_string())
                        .collect();
                    let ip = addrs.first().cloned().unwrap_or_default();
                    let name = info
                        .get_fullname()
                        .split("._smb._tcp")
                        .next()
                        .unwrap_or(&host)
                        .to_string();
                    let key = format!("{}:{}", ip, info.get_port());
                    if seen.contains(&key) {
                        continue;
                    }
                    seen.insert(key);
                    shares.push(json!({
                        "id": format!("smb://{}", ip),
                        "name": name,
                        "host": if ip.is_empty() { host.clone() } else { ip },
                        "hostname": host,
                        "port": info.get_port(),
                        "protocol": "smb",
                        "available": true,
                    }));
                }
                Ok(_) => {}
                Err(_) => {}
            }
        }
        daemon.shutdown().ok();
        Some(shares)
    })
    .await;

    match result {
        Ok(Some(shares)) => Json(json!(shares)),
        _ => Json(json!([])),
    }
}

/// Scan a specific host for SMB or NFS shares.
async fn scan_host(
    headers: axum::http::HeaderMap,
    Query(q): Query<ScanHostQuery>,
) -> impl IntoResponse {
    let lang = crate::i18n::lang_from_header(&headers);
    let host = &q.host;
    let protocol = q.protocol.as_deref().unwrap_or("smb");

    let raw_output = if protocol == "smb" {
        // Platform-specific SMB share enumeration
        let mut output = String::new();
        let mut success = false;
        let mut last_error = String::new();

        // Windows: net view \\host
        if !success {
            if let Ok(Ok(out)) = tokio::time::timeout(
                Duration::from_secs(10),
                Command::new("net")
                    .args(["view", &format!("\\\\{host}")])
                    .output(),
            )
            .await
            {
                if out.status.success() {
                    output = String::from_utf8_lossy(&out.stdout).to_string();
                    success = true;
                } else {
                    last_error = String::from_utf8_lossy(&out.stderr).to_string();
                }
            }
        }

        // macOS: smbutil view
        if !success {
            let smb_user = q.username.as_deref().unwrap_or("guest");
            let smb_url = if let Some(ref pw) = q.password {
                if !pw.is_empty() {
                    format!("//{}:{}@{}", smb_user, pw, host)
                } else {
                    format!("//{}@{}", smb_user, host)
                }
            } else {
                format!("//{}@{}", smb_user, host)
            };
            if let Ok(Ok(out)) = tokio::time::timeout(
                Duration::from_secs(10),
                Command::new("smbutil").args(["view", &smb_url]).output(),
            )
            .await
            {
                if out.status.success() {
                    output = String::from_utf8_lossy(&out.stdout).to_string();
                    success = true;
                } else {
                    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                    if !stdout.trim().is_empty() {
                        output = stdout;
                        success = true;
                    } else {
                        last_error = stderr;
                    }
                }
            }
        }

        // Linux: smbclient -N -L
        if !success {
            let mut smb_args = vec!["-L".to_string(), format!("//{host}")];
            if let Some(ref user) = q.username {
                smb_args.push("-U".to_string());
                if let Some(ref pw) = q.password {
                    if !pw.is_empty() {
                        smb_args.push(format!("{}%{}", user, pw));
                    } else {
                        smb_args.push(user.clone());
                        smb_args.push("-N".to_string());
                    }
                } else {
                    smb_args.push(user.clone());
                    smb_args.push("-N".to_string());
                }
            } else {
                smb_args.push("-N".to_string());
            }
            match tokio::time::timeout(
                Duration::from_secs(10),
                Command::new("smbclient").args(&smb_args).output(),
            )
            .await
            {
                Ok(Ok(out)) => {
                    output = String::from_utf8_lossy(&out.stdout).to_string();
                }
                Ok(Err(e)) => {
                    // smbclient not available — use last_error from previous tools
                    tracing::warn!(host = %host, error = %e, "network_smb_smbclient_spawn_failed (smbclient not installed?)");
                }
                Err(_) => {
                    tracing::warn!(host = %host, "network_smb_scan_timed_out (smbclient -L)");
                    return (
                        StatusCode::GATEWAY_TIMEOUT,
                        Json(json!({ "error": "scan timed out" })),
                    )
                        .into_response();
                }
            }
        }

        if output.trim().is_empty() && !last_error.is_empty() {
            tracing::warn!(host = %host, error = %last_error.trim(), "network_smb_scan_failed");
            let msg = if last_error.contains("Authentication")
                || last_error.contains("auth")
                || last_error.contains("STATUS_ACCESS_DENIED")
            {
                crate::i18n::t(&lang, "net.smbAccessDenied").replace("{error}", &last_error)
            } else {
                crate::i18n::t(&lang, "net.smbScanFailed")
                    .replace("{host}", host)
                    .replace("{error}", &last_error)
            };
            return (StatusCode::OK, Json(json!({ "shares": [], "error": msg }))).into_response();
        }

        output
    } else {
        // NFS: showmount -e host
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            Command::new("showmount").args(["-e", host]).output(),
        )
        .await;
        match result {
            Ok(Ok(out)) => String::from_utf8_lossy(&out.stdout).to_string(),
            Ok(Err(e)) => {
                tracing::warn!(host = %host, error = %e, "network_nfs_showmount_spawn_failed (showmount not installed?)");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("scan failed: {e}") })),
                )
                    .into_response();
            }
            Err(_) => {
                tracing::warn!(host = %host, "network_nfs_scan_timed_out (showmount -e)");
                return (
                    StatusCode::GATEWAY_TIMEOUT,
                    Json(json!({ "error": "scan timed out" })),
                )
                    .into_response();
            }
        }
    };

    // Parse share names from command output.
    let shares: Vec<Value> = if protocol == "smb" {
        // smbclient -L / smbutil view / net view all print a "Sharename Type
        // Comment" table where column 2 is the share TYPE (Disk / Printer /
        // IPC). Keying on that type is far more robust than a prefix filter:
        // the previous filter tested "Sharing" (typo — the header word is
        // "Sharename") so the header was never dropped, and every non-empty
        // line — the header, the `----` rule, client-side Kerberos warnings,
        // `mkdir failed on /var/lib/samba/lock`, `SMB1 disabled…`, the second
        // Server/Workgroup table — was emitted as a bogus "share" (Dominique,
        // Fedora). Only rows whose 2nd column is a real file/printer share type
        // survive; IPC$ (admin share, never a music source) is dropped too.
        raw_output
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() < 2 {
                    return None;
                }
                let name = parts[0];
                let stype = parts[1];
                let is_share_type = stype.eq_ignore_ascii_case("Disk")
                    || stype.eq_ignore_ascii_case("Printer")
                    || stype.eq_ignore_ascii_case("Print");
                // Skip admin/hidden shares ($-suffixed: IPC$, ADMIN$, C$, …).
                if !is_share_type || name.ends_with('$') {
                    return None;
                }
                Some(json!({
                    "name": name,
                    "type": stype,
                    "host": host,
                    "protocol": protocol,
                    "path": format!("//{host}/{name}"),
                }))
            })
            .collect()
    } else {
        // NFS `showmount -e host`: "Export list for host:" header then
        // "/export/path  clients" rows — the export path is column 1.
        raw_output
            .lines()
            .filter(|line| {
                let t = line.trim();
                !t.is_empty() && !t.starts_with("Export") && !t.starts_with("---")
            })
            .filter_map(|line| {
                let parts: Vec<&str> = line.split_whitespace().collect();
                let name = *parts.first()?;
                Some(json!({
                    "name": name,
                    "type": "NFS",
                    "host": host,
                    "protocol": protocol,
                    "path": format!("{host}:{name}"),
                }))
            })
            .collect()
    };

    tracing::info!(
        host = %host,
        protocol,
        shares = shares.len(),
        "network_scan_host_complete"
    );
    Json(json!(shares)).into_response()
}

/// Return cached SMB shares (stub — future mDNS integration).
async fn list_smb_shares() -> Json<Value> {
    Json(json!({
        "items": [],
        "total": 0,
        "message": "SMB share discovery pending",
    }))
}

/// Trigger an SMB network scan using mDNS service discovery.
async fn trigger_smb_scan() -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(|| {
        let daemon = mdns_sd::ServiceDaemon::new().ok()?;
        let receiver = daemon.browse("_smb._tcp.local.").ok()?;
        let mut shares = Vec::new();

        // Collect discoveries for 3 seconds
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            match receiver.recv_timeout(Duration::from_millis(500)) {
                Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                    shares.push(json!({
                        "name": info.get_fullname(),
                        "host": info.get_hostname(),
                        "port": info.get_port(),
                        "addresses": info.get_addresses()
                            .iter()
                            .map(|a| a.to_ip_addr().to_string())
                            .collect::<Vec<_>>(),
                        "properties": info.get_properties()
                            .iter()
                            .map(|p| (p.key().to_string(), p.val_str().to_string()))
                            .collect::<std::collections::HashMap<_, _>>(),
                    }));
                }
                Ok(_) => {}  // other events (SearchStarted, ServiceFound, etc.)
                Err(_) => {} // recv timeout, continue until deadline
            }
        }
        daemon.shutdown().ok();
        Some(shares)
    })
    .await;

    match result {
        Ok(Some(shares)) => {
            let count = shares.len();
            Json(json!({
                "status": "scan_complete",
                "shares": shares,
                "count": count,
            }))
            .into_response()
        }
        _ => Json(json!({
            "status": "scan_failed",
            "shares": [],
        }))
        .into_response(),
    }
}

/// List all stored SMB mounts from the network_mounts table.
///
/// La liste ne rendait que `active` — l'INTENTION de l'utilisateur. Un partage
/// dont le remontage au demarrage avait echoue s'affichait donc exactement
/// comme un partage monte, et l'echec ne se voyait qu'a la lecture, sous la
/// forme d'une erreur reseau generique qui ne le nommait pas (#1916, Eric
/// `ricouxxx`). Trois champs portent desormais le CONSTAT :
///
/// - `mounted` : verifie a l'instant, sur le systeme de fichiers ;
/// - `mount_state` / `last_mount_error` : ce qu'a donne le dernier essai ;
/// - `smb_version` : le dialecte retenu, que l'interface doit afficher quand
///   il vaut `1.0` — retomber sur un protocole obsolete et non chiffre n'est
///   pas neutre, et se fait aujourd'hui en silence (#1834).
async fn list_smb_mounts(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let rows = state
        .backend
        .query_many(
            "SELECT id, server, share, mount_path, username, active, \
             smb_version, mount_state, last_mount_error \
             FROM network_mounts WHERE mount_type = 'smb' ORDER BY id",
            &[],
        )
        .map_err(|e| AppError::internal(e))?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            let mount_path = r.get(3).and_then(|v| v.as_string());
            // Le constat de l'instant prime sur celui du dernier essai : un NAS
            // rallume et remonte a la main doit apparaitre monte, meme si le
            // demarrage s'etait solde par un echec.
            let monte = mount_path
                .as_deref()
                .is_some_and(|p| smb::est_un_point_de_montage(std::path::Path::new(p)));
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "server": r.get(1).and_then(|v| v.as_string()),
                "share": r.get(2).and_then(|v| v.as_string()),
                "mount_path": mount_path,
                "username": r.get(4).and_then(|v| v.as_string()),
                "active": r.get(5).and_then(|v| v.as_i64()).unwrap_or(1) != 0,
                "smb_version": r.get(6).and_then(|v| v.as_string()),
                "mount_state": r.get(7).and_then(|v| v.as_string()),
                "last_mount_error": r.get(8).and_then(|v| v.as_string()),
                "mounted": monte,
            })
        })
        .collect();
    Ok(Json(json!(items)))
}

/// Mount an SMB share: execute the OS mount command, then persist in the database.

/// Traduire l'échec de création du point de montage en un obstacle NOMMÉ.
///
/// Le message rendu était `failed to create mount dir: Permission denied
/// (os error 13)`. Exact, et inutile : il ne dit pas ce qui manque, et surtout
/// pas que **le montage lui-même** demandera le même privilège juste après —
/// de sorte que créer le dossier à la main ne débloquerait rien.
///
/// Vécu le 2026-08-21 par Dominique Comet, dont le serveur tourne depuis son
/// répertoire personnel et non sous `root` : trois échecs identiques dans ses
/// journaux, un 500 à l'écran, et la conclusion naturelle — mais fausse — que
/// son partage SMB ou son NAS étaient en cause. La découverte avait pourtant
/// réussi juste avant (`shares=1`).
///
/// On sépare donc le refus de privilège du reste : c'est le seul cas où
/// l'utilisateur peut agir, et l'action n'est pas celle qu'il croit.
fn obstacle_de_montage(e: &std::io::Error, chemin: &str) -> (&'static str, String) {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => (
            "privileges_insuffisants",
            format!(
                "Le serveur n'a pas les droits de créer le point de montage {chemin}. \
                 Monter un partage SMB demande des privilèges système (root, ou la \
                 capacité CAP_SYS_ADMIN) : créer ce dossier à la main ne suffira pas, \
                 car le montage lui-même les redemandera. Deux issues : donner ces \
                 privilèges au service, ou monter le partage par le système \
                 (/etc/fstab) et déclarer le dossier obtenu dans les dossiers de musique."
            ),
        ),
        std::io::ErrorKind::NotFound => (
            "chemin_parent_absent",
            format!("Le dossier parent de {chemin} n'existe pas."),
        ),
        _ => (
            "creation_impossible",
            format!("Impossible de créer le point de montage {chemin} : {e}"),
        ),
    }
}

async fn mount_smb_share(
    State(state): State<AppState>,
    Json(body): Json<MountRequest>,
) -> impl IntoResponse {
    let share_safe = body.share_name.replace(['/', '\\', ' '], "_");
    let mount_path = body
        .mount_path
        .unwrap_or_else(|| format!("/mnt/{}_{}", body.host, share_safe));

    // Dry run: just test reachability without mounting
    if body.dry_run {
        let reachable = tokio::net::TcpStream::connect(format!("{}:445", body.host))
            .await
            .is_ok();
        // Le message disait « Host reachable on SMB port 445 », que l'interface
        // affichait en vert comme une validation. Il ne teste QUE l'ouverture du
        // port : ni les identifiants, ni l'existence du partage, ni la
        // possibilite de monter. Chez Philippe Landes il etait au vert et
        // l'etape suivante rendait 500 — un voyant vert juste avant l'etape qui
        // echoue est pire qu'aucun voyant, il envoie chercher la panne du cote
        // du reseau, precisement la seule chose qui ait ete verifiee (#1847).
        return Json(json!({
            "ok": reachable,
            "host": body.host,
            "share_name": body.share_name,
            "message": if reachable {
                "Serveur joignable (port 445) — identifiants et partage non vérifiés"
            } else {
                "Serveur injoignable sur le port 445"
            },
        }))
        .into_response();
    }

    // Create mount directory
    if let Err(e) = tokio::fs::create_dir_all(&mount_path).await {
        // Journalise AUSSI, et pas seulement dans la reponse HTTP : le client
        // web n'affichait que le statut, donc la cause n'existait nulle part
        // (#1847).
        warn!(host = %body.host, path = %mount_path, error = %e, "smb_mount_dir_failed");
        let (motif, message) = obstacle_de_montage(&e, &mount_path);
        // `message` porte le TEXTE, `error` porte le CODE — et cet ordre n'est
        // pas decoratif : `apiError()` du client lit `detail` ou `message` pour
        // ce qu'il affiche, et range `error` dans un code machine. La reponse
        // d'avant mettait sa phrase dans `error` : elle n'etait donc affichee
        // NULLE PART, et l'utilisateur ne voyait que « 500 Internal Server
        // Error ». C'est ce qui a laisse Dominique Comet sans autre indice que
        // ses journaux.
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "message": message, "error": motif })),
        )
            .into_response();
    }

    // Dialecte qui a effectivement monte le partage, a persister pour que le
    // remontage au demarrage reparte du bon (#1834). Reste NUL sur macOS :
    // `mount_smbfs` negocie seul, des deux cotes, il n'y a rien a retenir.
    let mut dialecte_retenu: Option<String> = None;

    // Build the mount command depending on the platform
    let mount_result = if cfg!(target_os = "macos") {
        let credentials = match (&body.username, &body.password) {
            (Some(u), Some(p)) => format!("{u}:{p}@"),
            (Some(u), None) => format!("{u}@"),
            _ => "guest@".to_string(),
        };
        let unc = format!("//{credentials}{}/{}", body.host, body.share_name);
        tokio::time::timeout(
            Duration::from_secs(15),
            Command::new("mount_smbfs")
                .args([&unc, &mount_path])
                .output(),
        )
        .await
    } else {
        // Linux: mount.cifs, en NEGOCIANT le dialecte au lieu de l'imposer.
        //
        // `vers=3.0` etait code en dur, sans repli ni choix. Or le module CIFS
        // du noyau ne negocie de lui-meme qu'entre 2.1, 3.0 et 3.1.1 : il ne
        // descend jamais plus bas et refuse par `mount error(22): Invalid
        // argument`, un message qui ne dit rien de la cause.
        //
        // Philippe Landes l'a paye cher : `smbclient -L` listait parfaitement
        // le partage ROSEDISK de son NAS Rose, avec les memes identifiants,
        // pendant que le montage echouait. L'asymetrie tient a ce que
        // `smbclient` est du Samba en espace utilisateur — il descend plus bas
        // que le noyau. Le materiel audio embarque souvent un Samba ancien ;
        // tout ce parc etait donc inaccessible, sans que rien ne l'explique.
        //
        // On essaie donc, dans l'ordre : negociation libre (le noyau prend le
        // meilleur dialecte moderne), puis 2.0, puis 1.0. Le premier qui monte
        // gagne.
        //
        // L'echelle vit desormais dans `crate::smb` : le remontage au demarrage
        // doit imperativement essayer les MEMES dialectes, dans le MEME ordre.
        // Il ne le faisait pas — il imposait toujours `vers=3.0` — et le partage
        // que cette route venait de monter en SMB 1.0 se perdait au premier
        // redemarrage (#1834).
        let user = body.username.as_deref().unwrap_or("guest");
        let pass = body.password.as_deref().unwrap_or("");
        let unc = format!("//{}/{}", body.host, body.share_name);

        let mut dernier = None;
        for dialecte in smb::DIALECTES {
            let mut opts = format!("username={user},password={pass}");
            if let Some(v) = dialecte {
                opts.push_str(&format!(",vers={v}"));
            }
            // JAMAIS `opts` dans une trace : il porte le mot de passe.
            info!(
                host = %body.host,
                share = %body.share_name,
                dialect = smb::etiquette(dialecte),
                "smb_mount_attempt"
            );
            let res = tokio::time::timeout(
                smb::ESSAI_TIMEOUT,
                Command::new("mount.cifs")
                    .args([&unc, &mount_path, "-o", &opts])
                    .output(),
            )
            .await;

            let arreter = match &res {
                Ok(Ok(out)) if out.status.success() => {
                    info!(
                        host = %body.host,
                        share = %body.share_name,
                        dialect = smb::etiquette(dialecte),
                        "smb_mount_ok"
                    );
                    // Le dialecte qui a gagne doit survivre a la reponse HTTP :
                    // c'est lui que le remontage au demarrage rejouera.
                    dialecte_retenu = Some(smb::etiquette(dialecte).to_string());
                    true
                }
                Ok(Ok(out)) => {
                    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    // Sans cette trace, un echec de montage ne laissait AUCUNE
                    // marque nulle part : ni a l'ecran (le client jetait le
                    // corps de la reponse), ni au journal. Le diagnostic
                    // existait deux fois et disparaissait deux fois.
                    warn!(
                        host = %body.host,
                        share = %body.share_name,
                        dialect = smb::etiquette(dialecte),
                        error = %stderr,
                        "smb_mount_failed"
                    );
                    // Un refus d'authentification ne se repare pas en changeant
                    // de dialecte : inutile de faire patienter l'utilisateur
                    // vingt secondes de plus pour la meme reponse.
                    smb::est_refus_d_authentification(&stderr)
                }
                Ok(Err(e)) => {
                    // mount.cifs absent ou non executable : reessayer avec un
                    // autre dialecte ne changera rien.
                    warn!(host = %body.host, error = %e, "smb_mount_command_failed");
                    true
                }
                Err(_) => {
                    warn!(
                        host = %body.host,
                        dialect = smb::etiquette(dialecte),
                        "smb_mount_timeout"
                    );
                    false
                }
            };
            dernier = Some(res);
            if arreter {
                break;
            }
        }
        dernier.expect("DIALECTES n'est jamais vide")
    };

    let mount_ok = match mount_result {
        Ok(Ok(out)) if out.status.success() => true,
        Ok(Ok(out)) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("mount failed: {stderr}") })),
            )
                .into_response();
        }
        Ok(Err(e)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("mount command failed: {e}") })),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                Json(json!({ "error": "mount timed out" })),
            )
                .into_response();
        }
    };

    // Persist to database
    use tune_core::db::backend::ToSqlValue;
    // Remonter un partage deja enregistre passe souvent par cet ecran plutot
    // que par le bouton de remontage : sans ce controle on ajoutait une ligne
    // jumelle (#2453), et depuis l'index unique on echouerait. On rafraichit
    // la ligne existante — le dialecte retenu et le constat de montage sont
    // justement ce qui vient d'etre etabli.
    if let Some(id) = montage_existant(
        &state.backend,
        "smb",
        &body.host,
        &body.share_name,
        &mount_path,
    ) {
        let _ = state.backend.execute(
            "UPDATE network_mounts SET username = ?, password = ?, smb_version = ?, \
             mount_state = ?, active = 1 WHERE id = ?",
            &[
                &body.username as &dyn ToSqlValue,
                &body.password as &dyn ToSqlValue,
                &dialecte_retenu as &dyn ToSqlValue,
                &"mounted" as &dyn ToSqlValue,
                &id as &dyn ToSqlValue,
            ],
        );
        tracing::info!(id, host = %body.host, share = %body.share_name, "montage_reseau_rafraichi");
        return (
            StatusCode::OK,
            Json(json!({
                "id": id,
                "mounted": mount_ok,
                "mount_path": mount_path,
                "smb_version": dialecte_retenu,
                "existant": true,
            })),
        )
            .into_response();
    }
    match state.backend.execute_returning_id(
        // Le mot de passe est persiste AVEC le reste. Sans lui, le partage
        // enregistre est inexploitable au redemarrage : Tune connait l'adresse
        // et l'identifiant, pas le secret, donc il ne peut pas remonter — et
        // l'utilisateur doit re-saisir son partage ET ses identifiants a chaque
        // fois (Dominique Comet, #1692 : « il faut que je relance Ajouter un
        // partage reseau SMB, que je rechoisisse le disque avec les identifiants
        // adequats »).
        //
        // Ce n'est pas une nouvelle exposition : la route de montage generique
        // (`create_mount`) enregistre deja le mot de passe dans cette meme
        // colonne, et la meme base porte les jetons de streaming. Le chiffrer
        // ici seul donnerait l'illusion d'une protection sans en apporter —
        // `secret_envelope` exige une passphrase utilisateur, incompatible avec
        // un remontage sans personne devant la machine.
        //
        // `smb_version` retient le dialecte qui a gagne. Sans lui, le remontage
        // au demarrage repartait de `vers=3.0` en dur : le partage SMB 1.0 de
        // Philippe Landes montait ici, puis disparaissait au premier
        // redemarrage (#1834). `mount_state` porte le CONSTAT, la ou `active`
        // n'exprime qu'une intention (#1916) — on n'arrive ici qu'apres un
        // montage reussi, d'ou 'mounted'.
        "INSERT INTO network_mounts (mount_type, server, share, mount_path, username, password, smb_version, mount_state) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        &[&"smb" as &dyn ToSqlValue, &body.host as &dyn ToSqlValue, &body.share_name as &dyn ToSqlValue, &mount_path as &dyn ToSqlValue, &body.username as &dyn ToSqlValue, &body.password as &dyn ToSqlValue, &dialecte_retenu as &dyn ToSqlValue, &"mounted" as &dyn ToSqlValue],
    ) {
        Ok(id) => {
            (
                StatusCode::CREATED,
                Json(json!({
                    "id": id,
                    "mounted": mount_ok,
                    "mount_path": mount_path,
                    "smb_version": dialecte_retenu,
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("db error: {e}") })),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Media Server browsing / streaming
// ---------------------------------------------------------------------------

async fn browse_media_server(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<BrowseQuery>,
) -> Result<Json<Value>, AppError> {
    let object_id = q.object_id.as_deref().unwrap_or("0");
    let servers = state.media_servers.lock().await;
    // Un serveur que la carte mémoire ne connaît pas (démarrage avant la
    // première découverte, serveur oublié) sortait par la même porte qu'un
    // dossier vide : 200, listes vides, pas un mot. L'écran affichait un
    // dossier vide — lu « pas d'ouverture des dossiers » (Yacine, #4134).
    let ms = servers
        .get(&id)
        .cloned()
        .ok_or_else(|| EchecParcours::ServeurInconnu(id.clone()).en_erreur_http("?"))?;
    drop(servers);

    let (containers, items, total_matches) =
        parcourir_les_enfants(&ms.content_directory_url, &ms.name, object_id)
            .await
            .map_err(|e| e.en_erreur_http(&ms.name))?;
    let fetched = containers.len() + items.len();
    let total = (total_matches as usize).max(fetched);
    Ok(Json(json!({
        "object_id": object_id,
        "containers": containers,
        "items": items,
        "total_matches": total,
        "number_returned": fetched,
    })))
}

/// Pourquoi un `Browse` n'a rien rendu — pour que l'écran puisse le dire au
/// lieu d'afficher un dossier vide (#4134).
///
/// Jusqu'ici tout échec sortait par la porte du dossier vide : serveur absent
/// de la carte, délai ou erreur de transport, réponse HTTP 500/401 ou SOAP
/// Fault dont le corps partait dans l'analyseur DIDL, qui rendait `(0, 0)`.
/// Un 200 à listes vides ne rejette pas côté client : `folderNoAnswer` ne
/// s'affichait jamais.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EchecParcours {
    /// L'identifiant n'est pas dans la carte mémoire des serveurs.
    ServeurInconnu(String),
    /// Le POST SOAP n'est pas parti ou n'est pas revenu (délai de 10 s compris).
    Transport(String),
    /// Le serveur a répondu, mais hors 2xx.
    Statut(u16),
    /// 2xx sans élément `<Result>` — un SOAP Fault, ou autre chose qu'un
    /// ContentDirectory.
    SansResultat,
}

impl EchecParcours {
    /// La réponse HTTP de `GET /media-servers/{id}/browse` : 404 pour
    /// l'inconnu, 504 pour le transport, 502 pour un serveur qui répond mal.
    /// Le message nomme le serveur et la cause ; le code reste stable pour
    /// les clients qui voudront le lire.
    fn en_erreur_http(&self, nom_du_serveur: &str) -> AppError {
        let (status, message) = match self {
            EchecParcours::ServeurInconnu(id) => (
                StatusCode::NOT_FOUND,
                format!(
                    "serveur multimédia {id} inconnu du registre en mémoire — \
                     pas encore découvert, ou oublié"
                ),
            ),
            EchecParcours::Transport(e) => (
                StatusCode::GATEWAY_TIMEOUT,
                format!("{nom_du_serveur} n'a pas répondu au Browse : {e}"),
            ),
            EchecParcours::Statut(code) => (
                StatusCode::BAD_GATEWAY,
                format!("{nom_du_serveur} a répondu HTTP {code} au Browse"),
            ),
            EchecParcours::SansResultat => (
                StatusCode::BAD_GATEWAY,
                format!(
                    "{nom_du_serveur} a répondu sans élément <Result> au Browse \
                     (SOAP Fault ?)"
                ),
            ),
        };
        warn!(serveur = nom_du_serveur, error = %message, "browse_media_server_failed");
        AppError {
            status,
            message,
            code: Some("media_server_browse_failed".into()),
        }
    }
}

/// Parcourt TOUTES les pages d'un conteneur d'un serveur ContentDirectory.
///
/// Extrait tel quel du corps de [`browse_media_server`], qui l'appelle
/// toujours — l'indexation de la phase 2 du chantier
/// `unifier-serveurs-upnp-et-bibliotheque` a besoin du MEME parcours, page par
/// page, et le recopier aurait fait diverger deux lecteurs du même protocole.
///
/// Rend `(conteneurs, items, total_matches)`. Un `total_matches` de 0 avec des
/// items rendus signifie seulement que le serveur ne l'annonce pas.
// UPnP Browse returns results in PAGES. The old code issued a single
// Browse with RequestedCount=200 and returned only that page, so a server
// with thousands of albums showed just its first page (~100 on MinimServer /
// Twonky / Asset, which cap a single response) — "le résumé est juste mais la
// liste est très incomplète (~100 sur x xxx)" (Pierre M). Loop over
// StartingIndex, accumulating children until NumberReturned==0 or
// StartingIndex>=TotalMatches, with a safety bound.
pub(crate) async fn parcourir_les_enfants(
    content_directory_url: &str,
    nom_du_serveur: &str,
    object_id: &str,
) -> Result<(Vec<Value>, Vec<Value>, u32), EchecParcours> {
    const PAGE_SIZE: u32 = 200;
    const MAX_PAGES: u32 = 500; // up to 100k children
    // Client partagé (voir `tune_core::http::client`). Le délai d'attente de
    // 30 s compte ici : la boucle ci-dessous peut enchaîner jusqu'à 500 pages,
    // et un client reqwest nu n'impose aucune limite — un serveur DLNA qui
    // accepte la connexion sans jamais répondre bloquait la requête sans fin.
    let client = tune_core::http::client::shared();
    let mut containers: Vec<Value> = Vec::new();
    let mut items: Vec<Value> = Vec::new();
    let mut starting_index: u32 = 0;
    let mut total_matches: u32 = 0;

    for _page in 0..MAX_PAGES {
        let soap_body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
<ObjectID>{object_id}</ObjectID>
<BrowseFlag>BrowseDirectChildren</BrowseFlag>
<Filter>*</Filter>
<StartingIndex>{starting_index}</StartingIndex>
<RequestedCount>{PAGE_SIZE}</RequestedCount>
<SortCriteria></SortCriteria>
</u:Browse>
</s:Body>
</s:Envelope>"#
        );

        let resp = match client
            .post(content_directory_url)
            .header("Content-Type", "text/xml; charset=utf-8")
            .header(
                "SOAPAction",
                "\"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"",
            )
            .body(soap_body)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    "browse_media_server soap_error server={nom_du_serveur} \
                     start={starting_index} err={e}"
                );
                // Première page : l'échec est celui du dossier, on le dit.
                // Pages suivantes : on rend ce qu'on a, comme avant.
                if starting_index == 0 {
                    return Err(EchecParcours::Transport(e.to_string()));
                }
                break;
            }
        };

        // Le statut n'était jamais examiné : le corps d'un 500 ou d'un SOAP
        // Fault partait dans l'analyseur DIDL, qui rendait (0, 0) — un
        // dossier vide, silencieux (#4134).
        let statut = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if starting_index == 0 {
            if !statut.is_success() {
                return Err(EchecParcours::Statut(statut.as_u16()));
            }
            if !reponse_porte_un_result(&body) {
                return Err(EchecParcours::SansResultat);
            }
        }
        let (mut page_containers, mut page_items) = parse_didl_browse_response(&body);
        let parsed = (page_containers.len() + page_items.len()) as u32;

        // NumberReturned / TotalMatches are un-escaped siblings of the escaped
        // DIDL <Result> in the SOAP body — no collision with the payload.
        let number_returned: u32 = extract_xml_tag(&body, "NumberReturned")
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(parsed);
        if let Some(tm) = extract_xml_tag(&body, "TotalMatches").and_then(|s| s.trim().parse().ok())
        {
            total_matches = tm;
        }

        containers.append(&mut page_containers);
        items.append(&mut page_items);

        if number_returned == 0 || parsed == 0 {
            break;
        }
        // Advance by what the server actually returned (robust against servers
        // that page smaller than RequestedCount).
        starting_index += number_returned.max(parsed);
        if total_matches != 0 && starting_index >= total_matches {
            break;
        }
    }

    Ok((containers, items, total_matches))
}

/// Une réponse de `Browse` porte un élément `<Result>` — vide pour un dossier
/// vide, mais présent. Un SOAP Fault n'en a pas.
fn reponse_porte_un_result(xml: &str) -> bool {
    xml.contains("<Result>") || xml.contains("<Result ")
}

#[derive(serde::Deserialize)]
struct SearchQuery {
    /// Le texte cherché.
    q: String,
    /// Le conteneur où chercher. Absent = tout le serveur (`0`).
    container: Option<String>,
}

/// Cherche DANS un serveur de médias, par son action ContentDirectory `Search`.
///
/// Pourquoi ce n'est pas un simple `Browse` filtré : parcourir une
/// arborescence de plusieurs milliers d'entrées côté client pour y chercher un
/// titre est intenable, et c'est précisément ce que `Search` évite — le
/// serveur cherche dans SON index.
///
/// La règle du chantier, symétrique de celle qu'on s'applique à nous-mêmes
/// (#2312) : **ne demander que ce que le serveur distant annonce**. On lit donc
/// d'abord ses `SearchCapabilities`. S'il n'annonce pas `dc:title`, on ne lui
/// envoie pas de critère qu'il ne sait pas évaluer — beaucoup répondent alors
/// par toute la bibliothèque, ce qui ressemble à un résultat et n'en est pas.
/// La réponse porte `supported: false` et le client se rabat sur un filtrage
/// du dossier courant, en le disant à l'écran.
async fn search_media_server(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<SearchQuery>,
) -> Json<Value> {
    let container = q.container.as_deref().unwrap_or("0");
    let vide = |supported: bool, raison: &str| {
        Json(json!({
            "container": container,
            "query": q.q,
            "supported": supported,
            "reason": raison,
            "containers": [],
            "items": [],
            "total_matches": 0,
            "number_returned": 0,
        }))
    };

    let servers = state.media_servers.lock().await;
    let ms = match servers.get(&id) {
        Some(ms) => ms.clone(),
        None => return vide(false, "serveur inconnu"),
    };
    drop(servers);

    if q.q.trim().is_empty() {
        return vide(true, "");
    }

    let caps = capacites_de_recherche(&ms.content_directory_url).await;
    let criteria = match critere_de_recherche(&caps, &q.q) {
        Some(c) => c,
        None => return vide(false, "ce serveur n'annonce pas la recherche par titre"),
    };

    const PAGE_SIZE: u32 = 200;
    const MAX_PAGES: u32 = 50;
    let client = tune_core::http::client::shared();
    let mut containers: Vec<Value> = Vec::new();
    let mut items: Vec<Value> = Vec::new();
    let mut starting_index: u32 = 0;
    let mut total_matches: u32 = 0;

    for _page in 0..MAX_PAGES {
        let soap_body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:Search xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
<ContainerID>{container}</ContainerID>
<SearchCriteria>{criteria}</SearchCriteria>
<Filter>*</Filter>
<StartingIndex>{starting_index}</StartingIndex>
<RequestedCount>{PAGE_SIZE}</RequestedCount>
<SortCriteria></SortCriteria>
</u:Search>
</s:Body>
</s:Envelope>"#,
            criteria = xml_escape(&criteria),
        );

        let resp = match client
            .post(&ms.content_directory_url)
            .header("Content-Type", "text/xml; charset=utf-8")
            .header(
                "SOAPAction",
                "\"urn:schemas-upnp-org:service:ContentDirectory:1#Search\"",
            )
            .body(soap_body)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    "search_media_server soap_error server={} start={starting_index} err={e}",
                    ms.name
                );
                break;
            }
        };

        let body = resp.text().await.unwrap_or_default();
        // Un 708 (« critère non supporté ») n'est pas une panne : c'est un
        // serveur qui annonce plus qu'il n'évalue. On le dit, plutôt que de
        // rendre une liste vide qui se lirait « aucun résultat ».
        if body.contains("<errorCode>") {
            let code = extract_xml_tag(&body, "errorCode").unwrap_or_default();
            tracing::info!(
                "search_media_server refus server={} code={code} criteria={criteria}",
                ms.name
            );
            return vide(false, "ce serveur a refusé le critère de recherche");
        }

        let (mut page_containers, mut page_items) = parse_didl_browse_response(&body);
        let parsed = (page_containers.len() + page_items.len()) as u32;
        let number_returned: u32 = extract_xml_tag(&body, "NumberReturned")
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(parsed);
        if let Some(tm) = extract_xml_tag(&body, "TotalMatches").and_then(|s| s.trim().parse().ok())
        {
            total_matches = tm;
        }

        containers.append(&mut page_containers);
        items.append(&mut page_items);

        if number_returned == 0 || parsed == 0 {
            break;
        }
        starting_index += number_returned.max(parsed);
        if total_matches != 0 && starting_index >= total_matches {
            break;
        }
    }

    let fetched = containers.len() + items.len();
    Json(json!({
        "container": container,
        "query": q.q,
        "supported": true,
        "reason": "",
        "containers": containers,
        "items": items,
        "total_matches": (total_matches as usize).max(fetched),
        "number_returned": fetched,
    }))
}

/// Ce que le serveur distant DIT savoir chercher.
///
/// Mis en cache dix minutes : une zone de recherche interroge à chaque frappe,
/// et cette capacité ne change pas d'une seconde à l'autre. Une panne réseau
/// n'est pas mise en cache — on réessaiera.
async fn capacites_de_recherche(content_directory_url: &str) -> String {
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};
    type Cache = std::sync::Mutex<std::collections::HashMap<String, (Instant, String)>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    const TTL: Duration = Duration::from_secs(600);

    let cache = CACHE.get_or_init(Default::default);
    if let Ok(map) = cache.lock() {
        if let Some((pose, caps)) = map.get(content_directory_url) {
            if pose.elapsed() < TTL {
                return caps.clone();
            }
        }
    }

    let soap = r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body><u:GetSearchCapabilities xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"/></s:Body>
</s:Envelope>"#;
    let caps = match tune_core::http::client::shared()
        .post(content_directory_url)
        .header("Content-Type", "text/xml; charset=utf-8")
        .header(
            "SOAPAction",
            "\"urn:schemas-upnp-org:service:ContentDirectory:1#GetSearchCapabilities\"",
        )
        .body(soap)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(r) => {
            extract_xml_tag(&r.text().await.unwrap_or_default(), "SearchCaps").unwrap_or_default()
        }
        Err(e) => {
            tracing::debug!("get_search_capabilities err={e}");
            return String::new();
        }
    };

    if let Ok(mut map) = cache.lock() {
        map.insert(
            content_directory_url.to_string(),
            (Instant::now(), caps.clone()),
        );
    }
    caps
}

/// Le critère à envoyer, construit UNIQUEMENT avec les champs annoncés.
///
/// `*` est la façon dont beaucoup de serveurs disent « tout m'est
/// interrogeable ». Sans `dc:title` — ni `*` —, on rend `None` : mieux vaut
/// dire au client qu'on ne sait pas chercher que lui rendre la bibliothèque
/// entière sous le nom de « résultats ».
///
/// La restriction de classe n'est ajoutée que si `upnp:class` est annoncé :
/// c'est un champ de plus à évaluer, et un serveur qui ne le connaît pas
/// refuserait tout le critère.
fn critere_de_recherche(caps: &str, texte: &str) -> Option<String> {
    let annonce = |champ: &str| {
        caps.split(',')
            .any(|c| c.trim() == "*" || c.trim().eq_ignore_ascii_case(champ))
    };
    if !annonce("dc:title") {
        return None;
    }
    let valeur = echapper_valeur_critere(texte);
    let titre = format!("dc:title contains \"{valeur}\"");
    Some(if annonce("upnp:class") {
        format!("upnp:class derivedfrom \"object.item.audioItem\" and {titre}")
    } else {
        titre
    })
}

/// Dans un `SearchCriteria`, une valeur est entre guillemets : la barre
/// oblique inverse et le guillemet doivent y être échappés, sinon un titre
/// contenant `"` casse le critère — ou, pire, en injecte un autre.
fn echapper_valeur_critere(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Le critère voyage dans du XML : `&`, `<` et les guillemets doivent y être
/// écrits en entités, sinon le SOAP est invalide.
fn xml_escape(v: &str) -> String {
    v.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn parse_didl_browse_response(xml: &str) -> (Vec<Value>, Vec<Value>) {
    let result_start = xml.find("<Result>").or_else(|| xml.find("<Result "));
    let result_end = xml.find("</Result>");
    let didl = match (result_start, result_end) {
        (Some(s), Some(e)) => {
            let after = &xml[s..];
            let content_start = after.find('>').map(|i| s + i + 1).unwrap_or(s);
            &xml[content_start..e]
        }
        _ => return (vec![], vec![]),
    };
    let decoded = didl
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&apos;", "'");

    let mut containers = Vec::new();
    let mut items = Vec::new();

    for tag in ["container", "item"] {
        let open = format!("<{tag} ");
        let close = format!("</{tag}>");
        let mut pos = 0;
        while let Some(start) = decoded[pos..].find(&open) {
            let abs_start = pos + start;
            if let Some(end) = decoded[abs_start..].find(&close) {
                let element = &decoded[abs_start..abs_start + end + close.len()];
                let id = extract_attr(element, "id").unwrap_or_default();
                let parent_id = extract_attr(element, "parentID").unwrap_or_default();
                let title = extract_xml_tag(element, "dc:title").unwrap_or_default();
                let album_art_uri = extract_xml_tag(element, "upnp:albumArtURI");
                let artist = extract_xml_tag(element, "upnp:artist")
                    .or_else(|| extract_xml_tag(element, "dc:creator"));

                if tag == "container" {
                    let child_count: u32 = extract_attr(element, "childCount")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    containers.push(json!({
                        "id": id,
                        "parent_id": parent_id,
                        "title": title,
                        // Le serveur envoie dc:creator sur les conteneurs album
                        // depuis toujours — c'est ICI qu'il se perdait : extrait
                        // quatre lignes plus haut, jamais posé dans le JSON.
                        // Une grille d'albums sans artiste n'est pas une
                        // bibliothèque (jeu des sept erreurs, 25/08).
                        "artist": artist,
                        "child_count": child_count,
                        "album_art_uri": album_art_uri,
                    }));
                } else {
                    let album = extract_xml_tag(element, "upnp:album");
                    // A server may announce SEVERAL <res> per item — Lyrion/LMS
                    // lists the original file (download.flc, with duration) plus
                    // on-the-fly transcodes (download.pcm headerless raw PCM with
                    // duration 0:00, download.mp3, …). The old code took the FIRST
                    // <res> blindly, so whenever the raw-PCM transcode came first
                    // the DLNA renderer was handed an unplayable headerless stream
                    // (Yacine: immediate failure, 0:00, replay loop). Pick the best
                    // resource instead; single-res items are untouched.
                    let resources = parse_res_elements(element);
                    let best = select_best_res(&resources);
                    let res_url = best.map(|r| r.url.clone());
                    // Real resolution + codec from the CHOSEN res@ attributes.
                    // Without these the signal path defaulted to "AAC 44kHz/16bit —
                    // Avec perte", mislabelling a hi-res ALAC (audio/mp4) as lossy AAC
                    // (Yves: NAS ALAC shown as AAC while the DartZeel read 24-bit).
                    let duration_ms = best.and_then(|r| r.duration_ms);
                    let sample_rate = best.and_then(|r| r.sample_rate);
                    let bit_depth = best.and_then(|r| r.bit_depth);
                    let channels = best.and_then(|r| r.channels);
                    let protocol_info = best.and_then(|r| r.protocol_info.clone());
                    // `res@size` : jamais rendu jusqu'ici. L'indexation de la
                    // phase 2 en fait une composante de la clé d'identité.
                    let size = best.and_then(|r| r.size);
                    items.push(json!({
                        "id": id,
                        "title": title,
                        "artist": artist,
                        "album": album,
                        "res_url": res_url,
                        "album_art_uri": album_art_uri,
                        "duration_ms": duration_ms,
                        "sample_rate": sample_rate,
                        "bit_depth": bit_depth,
                        "channels": channels,
                        "protocol_info": protocol_info,
                        "size": size,
                    }));
                }

                pos = abs_start + end + close.len();
            } else {
                break;
            }
        }
    }
    (containers, items)
}

/// One `<res>` element of a DIDL-Lite item.
#[derive(Debug, Clone)]
struct DidlRes {
    url: String,
    protocol_info: Option<String>,
    duration_ms: Option<u64>,
    sample_rate: Option<u32>,
    bit_depth: Option<u16>,
    channels: Option<u16>,
    /// `res@size` — la taille du fichier amont, en octets.
    ///
    /// Le DIDL la publie depuis toujours (mesuré le 14/09 sur Asset UPnP comme
    /// sur un Tune : `size="31911291"`), et le parseur la jetait. Elle entre
    /// ici parce qu'elle est le seul discriminant **stable** qui reste quand
    /// l'`ObjectID` ne l'est pas — voir `cle_d_identite` dans
    /// `routes/indexation_upnp.rs`.
    size: Option<u64>,
}

/// Parse every `<res …>url</res>` of a DIDL item, in document order.
fn parse_res_elements(element: &str) -> Vec<DidlRes> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(start) = element[pos..].find("<res") {
        let abs = pos + start;
        // Only match the actual <res> tag ("<res " / "<res>"), not e.g. <resType>.
        let after = &element[abs + 4..];
        if !(after.starts_with(' ') || after.starts_with('>')) {
            pos = abs + 4;
            continue;
        }
        let Some(tag_end_rel) = element[abs..].find('>') else {
            break;
        };
        let tag_end = abs + tag_end_rel;
        let res_tag = &element[abs..tag_end];
        let Some(close_rel) = element[tag_end..].find("</res>") else {
            break;
        };
        let url = element[tag_end + 1..tag_end + close_rel].trim().to_string();
        if !url.is_empty() {
            out.push(DidlRes {
                url,
                protocol_info: extract_attr(res_tag, "protocolInfo"),
                duration_ms: extract_attr(res_tag, "duration")
                    .and_then(|d| parse_upnp_duration(&d)),
                sample_rate: extract_attr(res_tag, "sampleFrequency")
                    .and_then(|s| s.parse::<u32>().ok()),
                bit_depth: extract_attr(res_tag, "bitsPerSample")
                    .and_then(|s| s.parse::<u16>().ok()),
                channels: extract_attr(res_tag, "nrAudioChannels")
                    .and_then(|s| s.parse::<u16>().ok()),
                size: extract_attr(res_tag, "size").and_then(|s| s.parse::<u64>().ok()),
            });
        }
        pos = tag_end + close_rel + "</res>".len();
    }
    out
}

/// Format-preference rank for a `<res>` — lower is better.
///
/// 0 = original/lossless WITH headers (flac/flc, alac/m4a, wav, aiff)
/// 1 = encapsulated lossy (mp3, aac, ogg/opus, wma)
/// 2 = unknown audio format
/// 3 = raw headerless PCM (audio/L16, audio/L24, LPCM, .pcm) — LMS announces
///     these transcodes with duration 0:00 and DLNA renderers choke on them
/// 4 = non-audio res (cover images some servers attach as extra <res>)
fn res_format_rank(res: &DidlRes) -> u8 {
    let path = res
        .url
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .to_lowercase();
    // protocolInfo = "http-get:*:<mime>:<extra>"
    let mime = res
        .protocol_info
        .as_deref()
        .and_then(|p| p.split(':').nth(2))
        .unwrap_or("")
        .trim()
        .to_lowercase();

    if mime.starts_with("image/") || mime.starts_with("video/") {
        return 4;
    }
    if mime.starts_with("audio/l16")
        || mime.starts_with("audio/l24")
        || mime.contains("lpcm")
        || path.ends_with(".pcm")
    {
        return 3;
    }
    const LOSSLESS_EXT: [&str; 8] = [
        ".flac", ".flc", ".m4a", ".mp4", ".alac", ".wav", ".aif", ".aiff",
    ];
    const LOSSLESS_MIME: [&str; 11] = [
        "audio/flac",
        "audio/x-flac",
        "audio/mp4",
        "audio/m4a",
        "audio/x-m4a",
        "audio/wav",
        "audio/x-wav",
        "audio/wave",
        "audio/aiff",
        "audio/x-aiff",
        "audio/x-aif",
    ];
    if LOSSLESS_EXT.iter().any(|e| path.ends_with(e)) || LOSSLESS_MIME.contains(&mime.as_str()) {
        return 0;
    }
    const LOSSY_EXT: [&str; 6] = [".mp3", ".aac", ".ogg", ".oga", ".opus", ".wma"];
    const LOSSY_MIME: [&str; 9] = [
        "audio/mpeg",
        "audio/mp3",
        "audio/aac",
        "audio/x-aac",
        "audio/ogg",
        "audio/x-ogg",
        "application/ogg",
        "audio/opus",
        "audio/x-ms-wma",
    ];
    if LOSSY_EXT.iter().any(|e| path.ends_with(e)) || LOSSY_MIME.contains(&mime.as_str()) {
        return 1;
    }
    2
}

/// Pick the best `<res>` of an item: by format rank, then prefer a resource
/// with a non-zero `duration` attribute, then keep document order. Items that
/// announce a single res keep it unconditionally (behaviour unchanged for
/// servers that only expose one resource, even raw PCM).
fn select_best_res(resources: &[DidlRes]) -> Option<&DidlRes> {
    if resources.len() <= 1 {
        return resources.first();
    }
    resources
        .iter()
        .enumerate()
        .min_by_key(|(i, r)| {
            (
                res_format_rank(r),
                u8::from(r.duration_ms.unwrap_or(0) == 0),
                *i,
            )
        })
        .map(|(_, r)| r)
}

fn parse_upnp_duration(d: &str) -> Option<u64> {
    let parts: Vec<&str> = d.split(':').collect();
    if parts.len() == 3 {
        let h: f64 = parts[0].parse().ok()?;
        let m: f64 = parts[1].parse().ok()?;
        let s: f64 = parts[2].parse().ok()?;
        Some((h * 3_600_000.0 + m * 60_000.0 + s * 1_000.0) as u64)
    } else if parts.len() == 2 {
        let m: f64 = parts[0].parse().ok()?;
        let s: f64 = parts[1].parse().ok()?;
        Some((m * 60_000.0 + s * 1_000.0) as u64)
    } else {
        None
    }
}

fn extract_attr(element: &str, name: &str) -> Option<String> {
    let pattern = format!("{name}=\"");
    let start = element.find(&pattern)? + pattern.len();
    let end = element[start..].find('"')? + start;
    Some(element[start..end].to_string())
}

fn extract_xml_tag(element: &str, tag: &str) -> Option<String> {
    let open_full = format!("<{tag}>");
    let open_attr = format!("<{tag} ");
    let close = format!("</{tag}>");
    let content_start = if let Some(s) = element.find(&open_full) {
        s + open_full.len()
    } else if let Some(s) = element.find(&open_attr) {
        let after = &element[s..];
        after.find('>')? + s + 1
    } else {
        return None;
    };
    let content_end = element[content_start..].find(&close)? + content_start;
    Some(element[content_start..content_end].to_string())
}

#[derive(Deserialize)]
struct BrowseQuery {
    object_id: Option<String>,
}

async fn media_server_stream_url(Path((id, item_id)): Path<(String, String)>) -> Json<Value> {
    Json(json!({
        "server_id": id,
        "item_id": item_id,
        "stream_url": null,
        "message": "UPnP stream URL resolution not yet implemented",
    }))
}

async fn play_media_server_item(
    Path((id, item_id, zone_id)): Path<(String, String, i64)>,
) -> Json<Value> {
    Json(json!({
        "server_id": id,
        "item_id": item_id,
        "zone_id": zone_id,
        "status": "not_implemented",
        "message": "UPnP media server playback not yet implemented",
    }))
}

#[derive(Deserialize)]
struct TestMountRequest {
    path: String,
}

async fn test_mount(Json(body): Json<TestMountRequest>) -> impl IntoResponse {
    let path = std::path::Path::new(&body.path);
    let exists = path.exists();
    let is_dir = path.is_dir();
    let readable = if exists {
        std::fs::read_dir(path).is_ok()
    } else {
        false
    };
    let file_count = if readable {
        std::fs::read_dir(path).map(|rd| rd.count()).unwrap_or(0)
    } else {
        0
    };

    Json(json!({
        "path": body.path,
        "exists": exists,
        "is_directory": is_dir,
        "readable": readable,
        "file_count": file_count,
    }))
}

async fn get_share_detail(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    use tune_core::db::backend::ToSqlValue;
    let p1 = if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
        "$1".to_string()
    } else {
        "?".to_string()
    };
    let result = state.backend.query_one(
        &format!(
            "SELECT id, mount_type, server, share, mount_path, username, active \
             FROM network_mounts WHERE id = {p1}"
        ),
        &[&id as &dyn ToSqlValue],
    );
    match result {
        Ok(Some(r)) => Ok(Json(json!({
            "id": r.get(0).and_then(|v| v.as_i64()),
            "mount_type": r.get(1).and_then(|v| v.as_string()),
            "server": r.get(2).and_then(|v| v.as_string()),
            "share": r.get(3).and_then(|v| v.as_string()),
            "mount_path": r.get(4).and_then(|v| v.as_string()),
            "username": r.get(5).and_then(|v| v.as_string()),
            "active": r.get(6).and_then(|v| v.as_i64()).unwrap_or(1) != 0,
        }))
        .into_response()),
        Ok(None) => Ok(StatusCode::NOT_FOUND.into_response()),
        Err(_) => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

#[cfg(test)]
mod tests {

    /// La regle du chantier : ne demander QUE ce que le serveur annonce.
    ///
    /// Un serveur qui n'annonce pas `dc:title` ne doit pas recevoir de critere
    /// de titre. Beaucoup repondent alors par toute la bibliotheque, ce qui
    /// ressemble a un resultat et n'en est pas.
    #[test]
    fn on_ne_demande_que_ce_que_le_serveur_annonce() {
        assert_eq!(critere_de_recherche("upnp:class", "blue"), None);
        assert_eq!(critere_de_recherche("", "blue"), None);
        assert_eq!(
            critere_de_recherche("upnp:class,dc:title", "blue").as_deref(),
            Some("upnp:class derivedfrom \"object.item.audioItem\" and dc:title contains \"blue\"")
        );
        // Sans `upnp:class` annonce, la restriction de classe est retiree :
        // l'ajouter ferait refuser tout le critere.
        assert_eq!(
            critere_de_recherche("dc:title", "blue").as_deref(),
            Some("dc:title contains \"blue\"")
        );
        // `*` est la facon dont beaucoup de serveurs disent « tout ».
        assert!(critere_de_recherche("*", "blue").is_some());
        // La casse annoncee varie d'un serveur a l'autre.
        assert!(critere_de_recherche("DC:TITLE", "blue").is_some());
    }

    /// Un titre contenant un guillemet ne doit pas pouvoir fermer la valeur du
    /// critere — ni casser le SOAP, ni y injecter un predicat.
    #[test]
    fn un_guillemet_dans_le_texte_cherche_est_echappe() {
        let c = critere_de_recherche("dc:title", r#"say "hello""#).unwrap();
        assert!(c.contains(r#"\"hello\""#), "{c}");
        assert_eq!(echapper_valeur_critere(r#"a\b"c"#), r#"a\\b\"c"#);
        assert_eq!(xml_escape(r#"a&b<c>"d""#), "a&amp;b&lt;c&gt;&quot;d&quot;");
    }

    use super::{
        critere_de_recherche, echapper_valeur_critere, obstacle_de_montage,
        parse_didl_browse_response, parse_res_elements, select_best_res, xml_escape,
    };

    /// Build a SOAP Browse response whose escaped DIDL contains one item with
    /// the given raw `<res>` elements (LMS-style).
    fn soap_with_res(res_elements: &str) -> String {
        let didl = format!(
            r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item id="t1" parentID="a1" restricted="1"><dc:title>Track</dc:title><upnp:artist>Artist</upnp:artist><upnp:album>Album</upnp:album>{res_elements}</item></DIDL-Lite>"#
        );
        let escaped = didl
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;");
        format!(
            "<s:Envelope><s:Body><u:BrowseResponse><Result>{escaped}</Result><NumberReturned>1</NumberReturned><TotalMatches>1</TotalMatches></u:BrowseResponse></s:Body></s:Envelope>"
        )
    }

    // Yacine's case: LMS announces the headerless raw-PCM transcode FIRST
    // (duration 0:00), then the original FLAC (with duration), then an MP3
    // transcode. The FLAC must win.
    const LMS_MULTI_RES: &str = concat!(
        r#"<res protocolInfo="http-get:*:audio/L16;rate=44100;channels=2:DLNA.ORG_PN=LPCM" duration="0:00:00">http://192.168.1.7:9000/music/123/download.pcm</res>"#,
        r#"<res protocolInfo="http-get:*:audio/x-flac:*" duration="0:04:33.000" sampleFrequency="44100" bitsPerSample="16" nrAudioChannels="2">http://192.168.1.7:9000/music/123/download.flc</res>"#,
        r#"<res protocolInfo="http-get:*:audio/mpeg:*" duration="0:04:33.000">http://192.168.1.7:9000/music/123/download.mp3</res>"#,
    );

    #[test]
    fn multi_res_lms_prefers_flac_over_raw_pcm() {
        let resources = parse_res_elements(LMS_MULTI_RES);
        assert_eq!(resources.len(), 3);
        let best = select_best_res(&resources).expect("a res must be selected");
        assert!(
            best.url.ends_with("download.flc"),
            "expected the FLAC original, got {}",
            best.url
        );
        assert_eq!(best.duration_ms, Some(273_000));
        assert_eq!(best.sample_rate, Some(44100));
        assert_eq!(best.bit_depth, Some(16));
        assert_eq!(
            best.protocol_info.as_deref(),
            Some("http-get:*:audio/x-flac:*")
        );
    }

    /// Jeu des sept erreurs (25/08) : le serveur envoie dc:creator sur les
    /// conteneurs album depuis toujours — extrait par le parseur, jamais posé
    /// dans le JSON. Une grille d'albums sans artiste n'est pas une
    /// bibliothèque. Contre-épreuve faite : fix neutralisé → FAILED.
    #[test]
    fn le_createur_d_un_conteneur_atterrit_dans_le_json() {
        let soap = format!(
            "<Envelope><Body><BrowseResponse><Result>{}</Result></BrowseResponse></Body></Envelope>",
            xml_escape(
                r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><container id="album/18" parentID="albums" restricted="1" childCount="10"><dc:title>18</dc:title><dc:creator>Moby</dc:creator><upnp:class>object.container.album.musicAlbum</upnp:class></container></DIDL-Lite>"#
            )
        );
        let (containers, items) = parse_didl_browse_response(&soap);
        assert!(items.is_empty());
        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0]["title"].as_str(), Some("18"));
        assert_eq!(
            containers[0]["artist"].as_str(),
            Some("Moby"),
            "dc:creator doit survivre jusqu'au JSON du conteneur"
        );
    }

    #[test]
    fn multi_res_end_to_end_didl_parse_picks_flac() {
        let soap = soap_with_res(LMS_MULTI_RES);
        let (containers, items) = parse_didl_browse_response(&soap);
        assert!(containers.is_empty());
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(
            item["res_url"].as_str().unwrap(),
            "http://192.168.1.7:9000/music/123/download.flc"
        );
        // duration/resolution must come from the CHOSEN res, not the first one
        assert_eq!(item["duration_ms"].as_u64(), Some(273_000));
        assert_eq!(item["sample_rate"].as_u64(), Some(44100));
        assert_eq!(
            item["protocol_info"].as_str(),
            Some("http-get:*:audio/x-flac:*")
        );
    }

    #[test]
    fn single_res_pcm_is_kept() {
        // A server that only announces raw PCM must keep working as before.
        let element = r#"<res protocolInfo="http-get:*:audio/L16;rate=44100;channels=2:*">http://10.0.0.2:9000/music/9/download.pcm</res>"#;
        let resources = parse_res_elements(element);
        assert_eq!(resources.len(), 1);
        let best = select_best_res(&resources).unwrap();
        assert!(best.url.ends_with("download.pcm"));
    }

    #[test]
    fn only_lossy_res_picks_mp3() {
        let element = concat!(
            r#"<res protocolInfo="http-get:*:audio/L16:*" duration="0:00:00">http://h:9000/music/5/download.pcm</res>"#,
            r#"<res protocolInfo="http-get:*:audio/mpeg:*" duration="0:03:10.000">http://h:9000/music/5/download.mp3</res>"#,
        );
        let resources = parse_res_elements(element);
        assert_eq!(resources.len(), 2);
        let best = select_best_res(&resources).unwrap();
        assert!(
            best.url.ends_with("download.mp3"),
            "mp3 must beat raw pcm, got {}",
            best.url
        );
        assert_eq!(best.duration_ms, Some(190_000));
    }

    #[test]
    fn equal_format_prefers_res_with_duration() {
        // Same rank (both FLAC): the one with a real duration wins even if
        // listed second.
        let element = concat!(
            r#"<res protocolInfo="http-get:*:audio/flac:*">http://h/1/nodur.flac</res>"#,
            r#"<res protocolInfo="http-get:*:audio/flac:*" duration="0:04:00.000">http://h/1/dur.flac</res>"#,
        );
        let resources = parse_res_elements(element);
        let best = select_best_res(&resources).unwrap();
        assert!(best.url.ends_with("dur.flac"));
    }

    #[test]
    fn equal_format_and_duration_keeps_document_order() {
        let element = concat!(
            r#"<res protocolInfo="http-get:*:audio/flac:*" duration="0:04:00.000">http://h/1/first.flac</res>"#,
            r#"<res protocolInfo="http-get:*:audio/flac:*" duration="0:04:00.000">http://h/1/second.flac</res>"#,
        );
        let resources = parse_res_elements(element);
        let best = select_best_res(&resources).unwrap();
        assert!(best.url.ends_with("first.flac"));
    }

    #[test]
    fn image_res_never_beats_audio() {
        // Some servers attach the cover as an extra <res>.
        let element = concat!(
            r#"<res protocolInfo="http-get:*:image/jpeg:*">http://h/cover.jpg</res>"#,
            r#"<res protocolInfo="http-get:*:audio/mpeg:*" duration="0:03:00.000">http://h/track.mp3</res>"#,
        );
        let resources = parse_res_elements(element);
        let best = select_best_res(&resources).unwrap();
        assert!(best.url.ends_with("track.mp3"));
    }

    // --- Nommer l'obstacle, au lieu de rendre un errno (#1515 voisin) ---

    fn err(kind: std::io::ErrorKind) -> std::io::Error {
        std::io::Error::new(kind, "essai")
    }

    /// Le cas de Dominique Comet : serveur lance depuis son repertoire
    /// personnel, /mnt appartient a root. Le message rendu etait « failed to
    /// create mount dir: Permission denied (os error 13) » — exact, et
    /// inutile : il ne dit pas ce qui manque, et surtout pas que le MONTAGE
    /// redemandera le meme privilege juste apres.
    #[test]
    fn un_refus_de_privilege_est_nomme_comme_tel() {
        let (motif, msg) = obstacle_de_montage(
            &err(std::io::ErrorKind::PermissionDenied),
            "/mnt/192.168.1.146_Music",
        );
        assert_eq!(motif, "privileges_insuffisants");
        assert!(msg.contains("/mnt/192.168.1.146_Music"), "{msg}");
        // Le point qui a coute une soiree a Dominique : creer le dossier a la
        // main ne suffit pas. Le message doit le dire, sinon il essaiera.
        assert!(
            msg.contains("ne suffira pas"),
            "le message doit prevenir que creer le dossier ne debloque rien : {msg}"
        );
        assert!(
            msg.contains("CAP_SYS_ADMIN") || msg.contains("root"),
            "{msg}"
        );
        // Et il doit offrir la sortie, pas seulement le constat.
        assert!(
            msg.contains("fstab"),
            "la solution non privilegiee manque : {msg}"
        );
    }

    #[test]
    fn les_autres_echecs_gardent_leur_cause_exacte() {
        // On ne noie pas tout dans « privileges » : un parent absent est un
        // probleme different, avec une reparation differente.
        let (motif, msg) = obstacle_de_montage(&err(std::io::ErrorKind::NotFound), "/x/y");
        assert_eq!(motif, "chemin_parent_absent");
        assert!(msg.contains("/x/y"), "{msg}");
        assert!(
            !msg.contains("CAP_SYS_ADMIN"),
            "pas de conseil hors sujet : {msg}"
        );

        let (motif, msg) = obstacle_de_montage(&err(std::io::ErrorKind::AlreadyExists), "/x/y");
        assert_eq!(motif, "creation_impossible");
        // Le cas inconnu garde l'erreur systeme : mieux vaut un message brut
        // qu'un message faux.
        assert!(
            msg.contains("essai"),
            "l'erreur d'origine doit survivre : {msg}"
        );
    }

    #[test]
    fn chaque_motif_est_distinct() {
        // Trois motifs, trois codes : le client peut les traduire, et un
        // journal les distingue. Les confondre ramenerait au message unique.
        let m: Vec<&str> = [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::AlreadyExists,
        ]
        .iter()
        .map(|k| obstacle_de_montage(&err(*k), "/x").0)
        .collect();
        let uniques: std::collections::HashSet<&&str> = m.iter().collect();
        assert_eq!(uniques.len(), 3, "{m:?}");
    }
}

/// #4134 — un `Browse` qui échoue ne rend plus un dossier vide muet : il
/// dit sa cause, et la route la traduit en 404 / 502 / 504.
#[cfg(test)]
mod tests_browse_dit_son_echec_4134 {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Un faux ContentDirectory qui rend toujours la même réponse HTTP.
    async fn faux_serveur(statut: &'static str, corps: &'static str) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = l.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let reponse = format!(
                        "HTTP/1.1 {statut}\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{corps}",
                        corps.len()
                    );
                    let _ = sock.write_all(reponse.as_bytes()).await;
                });
            }
        });
        format!("http://127.0.0.1:{port}/cd/control")
    }

    const BROWSE_VIDE: &str = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:BrowseResponse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><Result>&lt;DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"/&gt;</Result><NumberReturned>0</NumberReturned><TotalMatches>0</TotalMatches><UpdateID>1</UpdateID></u:BrowseResponse></s:Body></s:Envelope>"#;
    const SOAP_FAULT: &str = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>701</errorCode><errorDescription>No such object</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#;

    #[tokio::test]
    async fn un_dossier_reellement_vide_reste_un_succes() {
        let url = faux_serveur("200 OK", BROWSE_VIDE).await;
        let (c, i, total) = parcourir_les_enfants(&url, "faux", "0")
            .await
            .expect("vide ≠ échec");
        assert!(c.is_empty() && i.is_empty());
        assert_eq!(total, 0);
    }

    #[tokio::test]
    async fn un_500_est_un_echec_nomme_pas_un_dossier_vide() {
        let url = faux_serveur("500 Internal Server Error", "<html>boom</html>").await;
        let err = parcourir_les_enfants(&url, "faux", "0")
            .await
            .expect_err("500 doit échouer");
        assert_eq!(err, EchecParcours::Statut(500));
        assert_eq!(err.en_erreur_http("Syno").status, StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn un_soap_fault_en_200_est_un_echec_nomme() {
        let url = faux_serveur("200 OK", SOAP_FAULT).await;
        let err = parcourir_les_enfants(&url, "faux", "0")
            .await
            .expect_err("un Fault n'est pas un dossier");
        assert_eq!(err, EchecParcours::SansResultat);
        assert_eq!(
            err.en_erreur_http("Freebox").status,
            StatusCode::BAD_GATEWAY
        );
    }

    #[tokio::test]
    async fn un_serveur_qui_ne_repond_pas_est_un_echec_de_transport() {
        // Port libre, personne n'écoute : refus de connexion immédiat.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let url = format!("http://127.0.0.1:{port}/cd/control");
        let err = parcourir_les_enfants(&url, "faux", "0")
            .await
            .expect_err("connexion refusée");
        assert!(matches!(err, EchecParcours::Transport(_)), "{err:?}");
        assert_eq!(
            err.en_erreur_http("DESKTOP").status,
            StatusCode::GATEWAY_TIMEOUT
        );
    }

    #[test]
    fn le_serveur_inconnu_rend_404_avec_son_identifiant() {
        let e = EchecParcours::ServeurInconnu("uuid:abc".into()).en_erreur_http("?");
        assert_eq!(e.status, StatusCode::NOT_FOUND);
        assert!(e.message.contains("uuid:abc"), "{}", e.message);
        assert_eq!(e.code.as_deref(), Some("media_server_browse_failed"));
    }

    #[test]
    fn reponse_porte_un_result_distingue_le_vide_du_fault() {
        assert!(reponse_porte_un_result(BROWSE_VIDE));
        assert!(!reponse_porte_un_result(SOAP_FAULT));
        assert!(reponse_porte_un_result("<Result xmlns=\"x\"></Result>"));
    }
}
