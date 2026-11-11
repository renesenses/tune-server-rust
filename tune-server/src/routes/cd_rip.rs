//! Extraction de CD — et ce que cette route peut réellement promettre (#2466).
//!
//! `POST /cd-rip/rip` écrivait `cd_rip_current = {"status":"running"}` puis
//! rendait `200 {"status":"started"}` **sans lancer le moindre processus** :
//! les seuls `Command::new` du fichier sont ceux de la DÉTECTION
//! (`cd_status`, `list_drives`) et de la lecture de sommaire (`disc_info`).
//! Aucun extracteur n'est appelé, ici ni ailleurs — `cd_rip_current` n'est lu
//! et écrit que par ce fichier, aucune tâche de fond ne le consomme.
//!
//! Deux conséquences mesurées : `rip_status` rendait `running` indéfiniment,
//! et `cancel_rip` annonçait « Rip task cancelled » sans rien avoir à tuer.
//!
//! ⚠️ Ce module ne prétend PAS extraire. L'extracteur réel est un greffon
//! Python jamais porté ; l'arbitrage du 03/09 est de rendre la route honnête,
//! pas d'écrire le moteur. La correction porte donc sur trois points :
//!
//! 1. une GARDE DE DISPONIBILITÉ devant l'écriture de `cd_rip_current` —
//!    quand la machine ne porte aucun extracteur, la route le DIT
//!    (`status: "not_available"`, `reason` stable, outils nommés) au lieu de
//!    répondre « started » ;
//! 2. la réconciliation d'un `running` ORPHELIN — un enregistrement antérieur
//!    au démarrage de ce processus ne peut pas courir, et sans cela un serveur
//!    qui redémarre affiche une extraction en cours à vie ;
//! 3. le message de `cancel_rip`, qui dit désormais ce qu'il fait vraiment :
//!    effacer un état enregistré, pas interrompre un processus.
//!
//! L'idiome est celui de `sacd_rip.rs` (`status: "not_available"` + phrase en
//! clair, HTTP 200 : le refus est un DOCUMENT d'état, pas une panne de
//! transport — le client sonde ensuite la même forme sur `/rip/status`) et
//! celui de `crossfeed` (#2742) pour le couple `reason` / phrase en clair.
//!
//! ⚠️ La décision de refus est une fonction PURE de l'outillage détecté
//! ([`refus_extraction`]), jamais un `#[cfg]` : les deux branches doivent être
//! éprouvables depuis n'importe quelle machine de compilation, et la garde ne
//! doit pas refuser une machine correctement équipée.
//!
//! #8141 : un lecteur PRÉSENT n'est pas forcément LISIBLE. Dans un conteneur,
//! `/dev/sr0` garde le groupe de l'hôte (gid 11 sous ZimaOS) ; le compte du
//! serveur qui n'en est pas membre ne peut pas l'ouvrir. `/drives` le listait
//! pourtant comme utilisable, et `/disc` répondait « cdparanoia not available
//! or no disc inserted », qui mélange trois causes. Chaque lecteur est donc
//! ESSAYÉ en lecture ; le classement ([`classer_acces`]) et le refus
//! ([`refus_lecteur`]) sont, eux aussi, des fonctions pures.

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::error::AppError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(cd_status))
        .route("/drives", get(list_drives))
        .route("/disc", get(disc_info))
        .route("/rip", post(start_rip))
        .route("/rip/status", get(rip_status))
        .route("/rip/cancel", post(cancel_rip))
}

/// Les outils que porte réellement la machine.
///
/// `diskutil` sait monter et éjecter un disque sur macOS ; il n'extrait rien.
/// Il ne compte donc pas dans `extractor`, sous peine de déclarer
/// équipé un Mac qui ne peut pas lire une seule piste.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CdTooling {
    cdparanoia: bool,
    cdda2wav: bool,
    diskutil: bool,
}

/// Les deux extracteurs que ce serveur sait nommer, dans l'ordre de préférence.
const EXTRACTEURS_CONNUS: [&str; 2] = ["cdparanoia", "cdda2wav"];

impl CdTooling {
    /// L'extracteur retenu, ou `None` si la machine n'en porte aucun.
    fn extractor(self) -> Option<&'static str> {
        if self.cdparanoia {
            Some(EXTRACTEURS_CONNUS[0])
        } else if self.cdda2wav {
            Some(EXTRACTEURS_CONNUS[1])
        } else {
            None
        }
    }
}

/// `which <bin>` — la même sonde que celle qu'affiche `GET /cd-rip/status`.
async fn presente(bin: &str) -> bool {
    tokio::process::Command::new("which")
        .arg(bin)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Sonde l'outillage une fois pour toutes.
///
/// UNE seule implémentation, partagée par `GET /cd-rip/status` et par la garde
/// de `POST /cd-rip/rip` : c'est précisément leur DÉSACCORD qui faisait #2466 —
/// `/status` annonçait `available: false` pendant que `/rip` répondait
/// « started ».
async fn detecter_outillage() -> CdTooling {
    CdTooling {
        cdparanoia: presente("cdparanoia").await,
        cdda2wav: presente("cdda2wav").await,
        diskutil: presente("diskutil").await,
    }
}

/// Check whether cdparanoia or cdda2wav is available on the system.
async fn cd_status() -> Json<Value> {
    let outillage = detecter_outillage().await;

    Json(json!({
        "available": outillage.extractor().is_some(),
        "tool": outillage.extractor().unwrap_or("none"),
        "cdparanoia": outillage.cdparanoia,
        "cdda2wav": outillage.cdda2wav,
        "diskutil": outillage.diskutil,
    }))
}

/// Ce que l'ouverture en lecture d'un lecteur a donné au compte du serveur.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AccesLecteur {
    /// Le compte a le droit de l'ouvrir (avec ou sans disque).
    Lisible,
    /// Le système a refusé l'ouverture : le compte n'est pas du groupe.
    Refuse,
}

/// Un lecteur présent, et ce que le serveur peut en faire (#8141).
#[derive(Clone, Debug, PartialEq, Eq)]
struct EtatLecteur {
    device: String,
    acces: AccesLecteur,
    /// Le groupe propriétaire du périphérique, quand il a pu être lu.
    groupe: Option<u32>,
}

/// Le classement d'une ouverture, en fonction PURE de son résultat.
///
/// Seul un refus de DROITS rend le lecteur illisible. « Pas de disque »
/// (`ENOMEDIUM`) ou un lecteur occupé ne disent rien des droits : le compte a
/// pu aller jusqu'au périphérique, ce n'est pas le cas de #8141.
fn classer_acces(ouverture: Result<(), std::io::ErrorKind>) -> AccesLecteur {
    match ouverture {
        Err(std::io::ErrorKind::PermissionDenied) => AccesLecteur::Refuse,
        _ => AccesLecteur::Lisible,
    }
}

/// La phrase lue par l'utilisateur pour un lecteur présent mais illisible.
///
/// Elle nomme le périphérique et, quand il est connu, le numéro de groupe à
/// donner au conteneur : c'est le seul geste qui manque.
fn message_lecteur_illisible(device: &str, groupe: Option<u32>) -> String {
    match groupe {
        Some(gid) => format!(
            "The CD drive {device} is present, but this server is not allowed to open it: \
             it belongs to group {gid}, which the server account is not a member of. \
             With Docker, start the container as root (the default, so that the image \
             joins that group itself) or add the group to the container \
             (compose: group_add: [\"{gid}\"]; docker run: --group-add {gid}), then restart it."
        ),
        None => format!(
            "The CD drive {device} is present, but this server is not allowed to open it. \
             Add the server account to the group that owns {device} \
             (with Docker: group_add, the group number shown by `ls -ln {device}` on the host), \
             then restart it."
        ),
    }
}

/// Essaie d'ouvrir le lecteur en lecture, sans attendre de disque.
///
/// `O_NONBLOCK` : sans lui, ouvrir un lecteur optique peut refermer son tiroir
/// et attend le disque ; la sonde ne doit rien faire de tel.
#[cfg(target_os = "linux")]
fn ouvrir_en_lecture(device: &str) -> Result<(), std::io::ErrorKind> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(device)
        .map(drop)
        .map_err(|e| e.kind())
}

/// Les lecteurs `/dev/sr0` à `/dev/sr3` présents, chacun essayé en lecture.
#[cfg(target_os = "linux")]
fn sonder_lecteurs() -> Vec<EtatLecteur> {
    use std::os::unix::fs::MetadataExt;
    (0..4)
        .filter_map(|i| {
            let device = format!("/dev/sr{i}");
            let meta = std::fs::metadata(&device).ok()?;
            Some(EtatLecteur {
                acces: classer_acces(ouvrir_en_lecture(&device)),
                groupe: Some(meta.gid()),
                device,
            })
        })
        .collect()
}

/// Hors Linux, aucun `/dev/sr*` : la liste vient de `diskutil` (macOS).
#[cfg(not(target_os = "linux"))]
fn sonder_lecteurs() -> Vec<EtatLecteur> {
    Vec::new()
}

/// L'entrée de `/drives` d'un lecteur Linux.
fn decrire_lecteur(lecteur: &EtatLecteur) -> Value {
    let numero = lecteur.device.trim_start_matches("/dev/sr");
    let mut doc = json!({
        "device": lecteur.device,
        "name": format!("CD Drive {numero}"),
        "readable": lecteur.acces == AccesLecteur::Lisible,
    });
    if lecteur.acces == AccesLecteur::Refuse {
        doc["reason"] = json!("permission_denied");
        doc["group_id"] = json!(lecteur.groupe);
        doc["message"] = json!(message_lecteur_illisible(&lecteur.device, lecteur.groupe));
    }
    doc
}

/// List CD/DVD drives. On Linux scan /dev/sr*, on macOS use diskutil.
async fn list_drives() -> Json<Value> {
    let mut drives = Vec::new();

    // Linux: /dev/sr*, each one tried for reading (#8141)
    if cfg!(target_os = "linux") {
        drives.extend(sonder_lecteurs().iter().map(decrire_lecteur));
    }

    // macOS: list optical drives via diskutil
    if cfg!(target_os = "macos")
        && let Ok(output) = tokio::process::Command::new("diskutil")
            .args(["list", "external"])
            .output()
            .await
        && output.status.success()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("/dev/disk") {
                drives.push(json!({
                    "device": trimmed.split_whitespace().next().unwrap_or(trimmed),
                    "name": trimmed,
                }));
            }
        }
    }

    Json(json!({
        "drives": drives,
        "count": drives.len(),
    }))
}

/// Pourquoi la lecture du sommaire n'a rien donné, en fonction PURE (#8141).
///
/// `lancement` : le nombre de pistes lues, ou l'erreur du lancement de
/// cdparanoia. `None` = des pistes ont été lues, rien à expliquer.
fn diagnostic_sommaire(
    lancement: Result<usize, std::io::ErrorKind>,
    lecteurs: &[EtatLecteur],
) -> Option<(&'static str, String)> {
    match lancement {
        Ok(n) if n > 0 => None,
        Err(std::io::ErrorKind::NotFound) => Some((
            "no_cd_extractor",
            format!(
                "{} is not installed on this server: the disc cannot be read. \
                 Install it and try again.",
                EXTRACTEURS_CONNUS[0]
            ),
        )),
        Err(_) => Some((
            "cd_extractor_failed",
            format!(
                "{} could not be started on this server.",
                EXTRACTEURS_CONNUS[0]
            ),
        )),
        Ok(_) => Some(match refus_lecteur(lecteurs, None) {
            Some(refus) => (refus.reason, refus.message),
            None => (
                "no_disc_detected",
                "No audio CD could be read: insert a disc, or check that the drive \
                 is connected to this server."
                    .to_string(),
            ),
        }),
    }
}

/// Read Table of Contents from the CD using cdparanoia -Q.
async fn disc_info() -> impl IntoResponse {
    let result = tokio::process::Command::new("cdparanoia")
        .arg("-Q")
        .output()
        .await;

    match result {
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let combined = format!("{stderr}{stdout}");

            // Parse track count from cdparanoia output
            let tracks: Vec<Value> = combined
                .lines()
                .filter(|line| {
                    let trimmed = line.trim();
                    trimmed.starts_with(|c: char| c.is_ascii_digit()) && trimmed.contains('.')
                })
                .enumerate()
                .map(|(i, line)| {
                    json!({
                        "number": i + 1,
                        "raw": line.trim(),
                    })
                })
                .collect();

            let mut doc = json!({
                "disc_detected": !tracks.is_empty(),
                "tracks": tracks,
                "track_count": tracks.len(),
                "raw_output": combined,
            });
            if let Some((reason, message)) =
                diagnostic_sommaire(Ok(tracks.len()), &sonder_lecteurs())
            {
                doc["reason"] = json!(reason);
                doc["message"] = json!(message);
            }
            Json(doc).into_response()
        }
        Err(e) => {
            let (reason, message) = diagnostic_sommaire(Err(e.kind()), &[])
                .unwrap_or(("cd_extractor_failed", String::new()));
            Json(json!({
                "disc_detected": false,
                "tracks": [],
                "track_count": 0,
                "reason": reason,
                "error": message,
                "message": message,
            }))
            .into_response()
        }
    }
}

#[derive(Deserialize)]
struct RipRequest {
    /// Output directory for ripped files
    output_dir: Option<String>,
    /// Audio format: "wav", "flac", "aiff"
    format: Option<String>,
    /// Specific tracks to rip (empty = all)
    #[serde(default)]
    tracks: Vec<u32>,
    /// CD drive device path
    device: Option<String>,
}

/// Le refus d'extraire, et de quoi l'écrire dans la réponse.
///
/// `reason` est un code STABLE que le client lit pour choisir sa traduction ;
/// `message` est la phrase que lit l'utilisateur. Elle nomme l'outil qui
/// manque, ne promet aucune date et ne cite aucun nom de fonction interne.
struct RefusExtraction {
    reason: &'static str,
    missing: Vec<&'static str>,
    message: String,
}

/// La garde de #2466, en fonction PURE de l'outillage.
///
/// `None` = machine équipée : la route garde le comportement d'aujourd'hui.
/// `Some(..)` = aucun extracteur, donc rien ne pourra jamais démarrer, et la
/// route doit le dire au lieu de répondre « started ».
///
/// Aucun `#[cfg]` ici, et aucune sonde : l'outillage arrive en PARAMÈTRE, pour
/// que les deux branches soient éprouvables sur une machine quelconque.
fn refus_extraction(outillage: CdTooling) -> Option<RefusExtraction> {
    if outillage.extractor().is_some() {
        return None;
    }
    Some(RefusExtraction {
        reason: "no_cd_extractor",
        missing: EXTRACTEURS_CONNUS.to_vec(),
        message: format!(
            "No CD extraction tool on this server: neither {} nor {} was found. \
             Install one of them and start the rip again.",
            EXTRACTEURS_CONNUS[0], EXTRACTEURS_CONNUS[1]
        ),
    })
}

/// Le refus d'extraire d'un lecteur illisible (#8141), en fonction PURE.
///
/// `demande` : le lecteur que vise la requête, sinon tous les lecteurs
/// présents. Refus seulement si AUCUN lecteur visé n'est lisible ; sans
/// lecteur détecté, rien n'est décidé ici (macOS, chemin donné à la main).
fn refus_lecteur(lecteurs: &[EtatLecteur], demande: Option<&str>) -> Option<RefusExtraction> {
    let vises: Vec<&EtatLecteur> = lecteurs
        .iter()
        .filter(|l| demande.is_none_or(|d| l.device == d))
        .collect();
    if vises.is_empty() || vises.iter().any(|l| l.acces == AccesLecteur::Lisible) {
        return None;
    }
    let premier = vises[0];
    Some(RefusExtraction {
        reason: "cd_drive_not_readable",
        missing: Vec::new(),
        message: message_lecteur_illisible(&premier.device, premier.groupe),
    })
}

/// Start a background CD rip task.
async fn start_rip(
    State(state): State<AppState>,
    Json(body): Json<RipRequest>,
) -> Result<Json<Value>, AppError> {
    let settings = SettingsRepo::with_backend(state.backend.clone());

    let output_dir = body
        .output_dir
        .or_else(|| settings.get("cd_rip_output_dir").ok().flatten())
        .unwrap_or_else(|| {
            // #4770 : un dossier par compte, jamais un nom fixe partagé.
            tune_core::chemins_de_travail::racine_de_travail("tune-rip")
                .to_string_lossy()
                .to_string()
        });
    let format = body.format.unwrap_or_else(|| "wav".into());
    let rip_id = uuid::Uuid::new_v4().to_string();

    // ─── GARDE DE DISPONIBILITÉ (#2466) ─────────────────────────────────────
    // Elle précède l'écriture de `cd_rip_current` : sans extracteur, aucune
    // extraction ne démarrera jamais, et écrire `running` ici est le défaut.
    // #8141 : un extracteur ne sert à rien si le lecteur ne s'ouvre pas.
    let refus = refus_extraction(detecter_outillage().await)
        .or_else(|| refus_lecteur(&sonder_lecteurs(), body.device.as_deref()));
    if let Some(refus) = refus {
        let refus_doc = json!({
            "id": rip_id,
            "status": "not_available",
            "reason": refus.reason,
            "missing": refus.missing,
            "output_dir": output_dir,
            "format": format,
            "tracks": body.tracks,
            "device": body.device,
            "progress": 0,
            "message": refus.message,
        });
        // Le refus remplace l'état courant : `/rip/status` doit rendre la même
        // chose que le POST, sans quoi l'écran reprend sa lecture ailleurs.
        settings
            .set("cd_rip_current", &serde_json::to_string(&refus_doc)?)
            .ok();
        return Ok(Json(refus_doc));
    }
    // ─── fin de la garde ────────────────────────────────────────────────────

    // Store rip state
    let rip_state = json!({
        "id": rip_id,
        "status": "running",
        "output_dir": output_dir,
        "format": format,
        "tracks": body.tracks,
        "device": body.device,
        "progress": 0,
        "started_at": chrono_now(),
    });

    settings
        .set("cd_rip_current", &serde_json::to_string(&rip_state)?)
        .ok();

    Ok(Json(json!({
        "id": rip_id,
        "status": "started",
        "output_dir": output_dir,
        "format": format,
        "message": "CD rip task queued. Poll /rip/status for progress.",
    })))
}

/// L'horodatage d'un enregistrement, en secondes UNIX.
///
/// `chrono_now` écrit une chaîne de chiffres ; un enregistrement plus ancien a
/// pu porter un nombre. Les deux formes sont acceptées, tout le reste vaut
/// « inconnu ».
fn horodatage_unix(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse::<i64>().ok(),
        _ => None,
    }
}

/// Le `running` ORPHELIN, réconcilié (#2466).
///
/// Rien ne relance une extraction au démarrage, et aucun processus ne survit à
/// l'arrêt du serveur : un enregistrement `running` antérieur au démarrage de
/// CE processus — ou dont l'horodatage est illisible, donc rattachable à aucun
/// processus — ne peut pas courir. Sans cette réconciliation, le résidu écrit
/// par l'ancien code affiche une extraction en cours à vie.
///
/// `None` = rien à corriger, y compris pour un `running` écrit par ce
/// processus-ci : le comportement d'une machine équipée ne change pas.
fn reconcilier_orphelin(rip: &Value, demarrage_processus_unix: i64) -> Option<Value> {
    if rip.get("status").and_then(Value::as_str) != Some("running") {
        return None;
    }
    if horodatage_unix(rip.get("started_at")).is_some_and(|t| t >= demarrage_processus_unix) {
        return None;
    }
    let mut corrige = rip.clone();
    corrige["status"] = json!("interrupted");
    corrige["message"] = json!(
        "No rip is running. This record was left by an earlier run of the server \
         and has been cleared; a rip never survives a restart."
    );
    Some(corrige)
}

/// Get current rip progress.
async fn rip_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let current = settings
        .get("cd_rip_current")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    match current {
        Some(rip) => {
            match reconcilier_orphelin(&rip, state.process_started_at.unix_timestamp()) {
                // La correction est PERSISTÉE : le résidu doit disparaître de
                // la base, pas seulement de cette réponse-ci.
                Some(corrige) => {
                    if let Ok(texte) = serde_json::to_string(&corrige) {
                        settings.set("cd_rip_current", &texte).ok();
                    }
                    Json(corrige)
                }
                None => Json(rip),
            }
        }
        None => Json(json!({
            "status": "idle",
            "message": "No rip in progress",
        })),
    }
}

/// Cancel a running rip task.
async fn cancel_rip(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Some(current) = settings.get("cd_rip_current").ok().flatten()
        && let Ok(mut rip) = serde_json::from_str::<Value>(&current)
    {
        rip["status"] = json!("cancelled");
        settings
            .set("cd_rip_current", &serde_json::to_string(&rip)?)
            .ok();
        return Ok(Json(json!({
            "status": "cancelled",
            // #2466 : ce serveur ne lance aucun processus d'extraction, il
            // n'en interrompt donc aucun. Il efface l'état enregistré, et
            // c'est tout ce que cette phrase a le droit d'annoncer.
            "message": "Recorded rip state cleared. No extraction process was running to stop.",
        })));
    }
    Ok(Json(json!({
        "status": "idle",
        "message": "No rip in progress to cancel",
    })))
}

/// Secondes UNIX, en chaîne — PAS un horodatage ISO 8601, contrairement à ce
/// que disait le commentaire d'origine. [`horodatage_unix`] relit cette forme.
fn chrono_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    //! Les deux décisions de #2466, prises en fonctions PURES, éprouvées ici
    //! sur les DEUX branches — ce qu'une sonde `which` ne permettrait pas :
    //! aucune machine de compilation ne porte les deux configurations à la
    //! fois. Le contrat HTTP, lui, est mesuré par
    //! `tests/extraction_cd_ne_promet_pas_ce_qu_elle_ne_fait_pas.rs`.

    use super::*;

    // -----------------------------------------------------------------------
    // LE TÉMOIN — une machine équipée n'est jamais refusée.
    // -----------------------------------------------------------------------

    /// Le témoin du ticket : la garde ne doit RIEN changer là où un extracteur
    /// existe. Si ce test rougit, la correction du cas « machine nue » a
    /// désarmé le cas nominal.
    #[test]
    fn le_temoin_une_machine_equipee_n_est_pas_refusee() {
        for outillage in [
            CdTooling {
                cdparanoia: true,
                ..Default::default()
            },
            CdTooling {
                cdda2wav: true,
                ..Default::default()
            },
            CdTooling {
                cdparanoia: true,
                cdda2wav: true,
                diskutil: true,
            },
        ] {
            assert!(
                refus_extraction(outillage).is_none(),
                "une machine équipée doit garder le comportement d'avant : {outillage:?}"
            );
        }
    }

    /// `diskutil` monte et éjecte ; il n'extrait pas une piste. Un Mac qui ne
    /// porte que lui n'est PAS équipé — sans quoi le refus mentirait dans
    /// l'autre sens et l'utilisateur chercherait une panne inexistante.
    #[test]
    fn diskutil_seul_ne_suffit_pas_a_extraire() {
        let mac_nu = CdTooling {
            diskutil: true,
            ..Default::default()
        };
        assert_eq!(mac_nu.extractor(), None);
        assert!(refus_extraction(mac_nu).is_some());
    }

    /// Le refus NOMME les deux outils : « indisponible » en bloc n'apprend
    /// rien, et aucune date n'est promise.
    #[test]
    fn le_refus_nomme_les_outils_qui_manquent() {
        let refus = refus_extraction(CdTooling::default()).expect("machine nue : refus attendu");
        assert_eq!(refus.reason, "no_cd_extractor");
        assert_eq!(refus.missing, ["cdparanoia", "cdda2wav"]);
        for outil in refus.missing {
            assert!(
                refus.message.contains(outil),
                "le message doit nommer {outil} : {}",
                refus.message
            );
        }
        assert!(
            !refus.message.contains("start_rip") && !refus.message.contains("cd_rip_current"),
            "aucun nom interne dans une phrase lue par un humain : {}",
            refus.message
        );
    }

    // -----------------------------------------------------------------------
    // #8141 — un lecteur PRÉSENT mais ILLISIBLE (groupe de l'hôte).
    // -----------------------------------------------------------------------

    /// Le `/dev/sr0` du testeur ZimaOS : `brw-rw---- root:11`, ouvert par un
    /// compte qui n'est pas du groupe 11.
    fn sr0_du_testeur() -> EtatLecteur {
        EtatLecteur {
            device: "/dev/sr0".into(),
            acces: classer_acces(Err(std::io::ErrorKind::PermissionDenied)),
            groupe: Some(11),
        }
    }

    fn lisible(device: &str) -> EtatLecteur {
        EtatLecteur {
            device: device.into(),
            acces: classer_acces(Ok(())),
            groupe: Some(24),
        }
    }

    /// LE TÉMOIN de #8141 : le lecteur refusé n'est plus annoncé utilisable,
    /// et `/drives` dit pourquoi, groupe compris.
    #[test]
    fn le_temoin_un_lecteur_present_mais_illisible_est_dit_illisible() {
        let doc = decrire_lecteur(&sr0_du_testeur());
        assert_eq!(
            doc["readable"],
            json!(false),
            "un /dev/sr0 que le compte ne peut pas ouvrir est annoncé lisible : {doc}"
        );
        assert_eq!(doc["reason"], json!("permission_denied"));
        assert_eq!(doc["group_id"], json!(11));
        let message = doc["message"].as_str().unwrap_or_default();
        for attendu in ["/dev/sr0", "group 11", "group_add", "--group-add 11"] {
            assert!(
                message.contains(attendu),
                "le message doit citer {attendu} : {message}"
            );
        }
    }

    /// Le témoin dans l'autre sens : un lecteur ouvert, ou seulement vide,
    /// reste lisible et sans explication.
    #[test]
    fn le_temoin_un_lecteur_ouvert_ou_vide_reste_lisible() {
        for ouverture in [
            Ok(()),
            // ENOMEDIUM (pas de disque) et EBUSY ne disent rien des droits.
            Err(std::io::ErrorKind::Other),
            Err(std::io::ErrorKind::ResourceBusy),
        ] {
            assert_eq!(
                classer_acces(ouverture),
                AccesLecteur::Lisible,
                "{ouverture:?}"
            );
        }
        let doc = decrire_lecteur(&lisible("/dev/sr1"));
        assert_eq!(doc["readable"], json!(true));
        assert_eq!(doc["name"], json!("CD Drive 1"));
        assert!(doc.get("reason").is_none(), "{doc}");
    }

    /// `/rip` refuse quand aucun lecteur visé ne s'ouvre, au lieu de répondre
    /// « started » sur un lecteur qu'il ne pourra jamais lire.
    #[test]
    fn l_extraction_est_refusee_sur_un_lecteur_illisible() {
        let refus = refus_lecteur(&[sr0_du_testeur()], None).expect("refus attendu");
        assert_eq!(refus.reason, "cd_drive_not_readable");
        assert!(
            refus.missing.is_empty(),
            "aucun outil ne manque : {:?}",
            refus.missing
        );
        assert!(refus.message.contains("/dev/sr0"), "{}", refus.message);

        // Le lecteur demandé est illisible, même si un autre s'ouvre.
        let deux = [lisible("/dev/sr1"), sr0_du_testeur()];
        assert!(refus_lecteur(&deux, Some("/dev/sr0")).is_some());
    }

    /// Les cas où le refus de lecteur ne doit RIEN changer.
    #[test]
    fn le_temoin_un_lecteur_lisible_n_est_pas_refuse() {
        let deux = [lisible("/dev/sr1"), sr0_du_testeur()];
        assert!(refus_lecteur(&deux, None).is_none(), "un lecteur s'ouvre");
        assert!(refus_lecteur(&deux, Some("/dev/sr1")).is_none());
        assert!(
            refus_lecteur(&[], None).is_none(),
            "aucun lecteur détecté (macOS)"
        );
        assert!(
            refus_lecteur(&[sr0_du_testeur()], Some("/dev/disk4")).is_none(),
            "un chemin hors /dev/sr* n'est pas jugé ici"
        );
    }

    /// Sans groupe connu, le message donne encore le geste à faire.
    #[test]
    fn le_message_sans_groupe_connu_dit_ou_le_lire() {
        let message = message_lecteur_illisible("/dev/sr0", None);
        assert!(message.contains("ls -ln /dev/sr0"), "{message}");
        assert!(message.contains("group_add"), "{message}");
    }

    /// `/disc` ne confond plus « outil absent », « droits » et « pas de
    /// disque » : chacun a son code.
    #[test]
    fn le_sommaire_distingue_outil_absent_droits_et_disque_absent() {
        let code = |lancement, lecteurs: &[EtatLecteur]| {
            diagnostic_sommaire(lancement, lecteurs).map(|(reason, _)| reason)
        };
        assert_eq!(
            code(Err(std::io::ErrorKind::NotFound), &[]),
            Some("no_cd_extractor")
        );
        assert_eq!(
            code(Ok(0), &[sr0_du_testeur()]),
            Some("cd_drive_not_readable")
        );
        assert_eq!(
            code(Ok(0), &[lisible("/dev/sr0")]),
            Some("no_disc_detected")
        );
        assert_eq!(
            code(Err(std::io::ErrorKind::PermissionDenied), &[]),
            Some("cd_extractor_failed")
        );
        assert_eq!(code(Ok(12), &[sr0_du_testeur()]), None, "12 pistes lues");

        let (_, message) = diagnostic_sommaire(Err(std::io::ErrorKind::NotFound), &[]).unwrap();
        assert!(message.contains("cdparanoia"), "{message}");
    }

    // -----------------------------------------------------------------------
    // LE RÉSIDU — un `running` qui a survécu à un redémarrage.
    // -----------------------------------------------------------------------

    /// Le résidu écrit par l'ancien code : `running`, horodaté AVANT le
    /// démarrage de ce processus. Rien ne le relance, donc il ne court pas.
    #[test]
    fn un_running_anterieur_au_processus_est_dit_interrompu() {
        let residu = json!({ "id": "x", "status": "running", "started_at": "1000" });
        let corrige = reconcilier_orphelin(&residu, 2000).expect("résidu à réconcilier");
        assert_eq!(corrige["status"], json!("interrupted"));
        assert_ne!(corrige["status"], json!("running"));
        assert_eq!(corrige["id"], json!("x"), "le reste du document est gardé");
        assert!(
            corrige["message"].as_str().unwrap_or_default().len() > 20,
            "un état corrigé sans explication ne vaut pas mieux : {corrige}"
        );
    }

    /// Un `running` que CE processus vient d'écrire n'est pas touché : le
    /// témoin de la machine équipée, côté relecture.
    #[test]
    fn le_temoin_un_running_de_ce_processus_reste_intact() {
        let frais = json!({ "status": "running", "started_at": "2000" });
        assert!(reconcilier_orphelin(&frais, 2000).is_none());
        let plus_frais = json!({ "status": "running", "started_at": 4242 });
        assert!(reconcilier_orphelin(&plus_frais, 2000).is_none());
    }

    /// Un horodatage absent ou illisible ne se rattache à aucun processus :
    /// il est traité comme orphelin, faute de quoi le résidu resterait
    /// `running` à vie précisément dans le cas le plus ancien.
    #[test]
    fn un_running_sans_horodatage_lisible_est_orphelin() {
        for residu in [
            json!({ "status": "running" }),
            json!({ "status": "running", "started_at": "hier" }),
            json!({ "status": "running", "started_at": null }),
        ] {
            assert_eq!(
                reconcilier_orphelin(&residu, 2000)
                    .as_ref()
                    .map(|c| c["status"].clone()),
                Some(json!("interrupted")),
                "{residu}"
            );
        }
    }

    /// Les autres états ne sont pas réécrits : `cancelled`, `not_available` et
    /// `interrupted` sont des états FINAUX, pas des extractions en cours.
    #[test]
    fn les_etats_finaux_ne_sont_pas_reecrits() {
        for etat in ["cancelled", "not_available", "interrupted", "idle"] {
            let doc = json!({ "status": etat, "started_at": "1000" });
            assert!(
                reconcilier_orphelin(&doc, 2000).is_none(),
                "{etat} n'est pas une extraction en cours"
            );
        }
    }
}
