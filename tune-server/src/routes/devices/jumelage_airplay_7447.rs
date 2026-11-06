//! #7447 — le bouton « Pair » d'Audio / Network Devices.
//!
//! Le client envoie `POST /devices/{id}/pair` SANS corps. La route exigeait un
//! corps JSON (refus 415 avant d'entrer), ne reconnaissait l'AirPlay 2 qu'à un
//! préfixe `airplay2:` que la découverte ne donne jamais (`airplay-…`), et
//! répondait sinon 201 « paired » en écrivant un réglage que rien ne relit :
//! un succès annoncé sans aucun jumelage. Le code PIN était de même « vérifié »
//! sans jamais parvenir à l'appareil.
//!
//! Ici, le jumelage passe par le composant AirPlay 2 de la sortie enregistrée
//! (`Airplay2Output::start_pin_pairing` / `submit_pin`, comme
//! `routes/airplay_pairing.rs`), reconnue par son TYPE dans le registre et non
//! par son identifiant. La route attend, au plus [`ATTENTE_DU_RECEPTEUR`], que
//! le récepteur réponde, puis rend :
//!
//! - `awaiting_pin` : le récepteur affiche un code à saisir ;
//! - `paired` : le récepteur est jumelé (`connected` du démon) ;
//! - sinon une erreur `{"error", "code"}` qui nomme la cause, jamais 201.
//!
//! Un récepteur qui refuse le jumelage par code (enceinte ou Mac sans écran)
//! est essayé une fois en jumelage transitoire (code fixe `3939`, celui que la
//! lecture emploie) : s'il l'accepte, il est jumelé sans code à saisir.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tokio::sync::Mutex;
use tracing::{info, warn};
use tune_core::outputs::OutputTarget;
use tune_core::outputs::airplay::AirplayOutput;
use tune_core::outputs::airplay2::Airplay2Output;
use tune_core::outputs::airplay2::composant_manquant_7084::{
    CODE_AIRPLAY2_COMPONENT_MISSING, manque_pour, message_composant_manquant,
};

use crate::state::AppState;

/// Délai laissé au démon et au récepteur pour répondre à une demande de
/// jumelage ou à un code.
pub(super) const ATTENTE_DU_RECEPTEUR: Duration = Duration::from_secs(8);

/// Code fixe du jumelage transitoire, le même que `ensure_connected`.
const CODE_TRANSITOIRE: &str = "3939";

/// Codes stables des réponses de la route.
pub(super) const CODE_INCONNU: &str = "device_not_found";
pub(super) const CODE_SANS_JUMELAGE: &str = "pairing_not_supported";
pub(super) const CODE_SERVI_EN_AIRPLAY1: &str = "airplay2_pairing_unavailable";
pub(super) const CODE_DEMON: &str = "airplay2_daemon_failed";
pub(super) const CODE_ECHEC: &str = "airplay2_pairing_failed";
pub(super) const CODE_DELAI: &str = "airplay2_pairing_timeout";
pub(super) const CODE_PAS_DE_JUMELAGE_EN_COURS: &str = "airplay2_no_pairing_in_progress";

type Sortie = Arc<Mutex<Box<dyn OutputTarget>>>;

fn refus(statut: StatusCode, code: &str, device_id: &str, message: String) -> Response {
    warn!(device = %device_id, code, error = %message, "device_pair_refused");
    (
        statut,
        Json(json!({ "error": message, "code": code, "device_id": device_id })),
    )
        .into_response()
}

/// Ce que le registre sait de l'appareil, du point de vue du jumelage.
enum Appareil {
    Airplay2(Sortie),
    /// Servi par le chemin AirPlay 1 ; `exige` : il lui faudrait AirPlay 2.
    Airplay1 {
        exige: bool,
    },
    Autre(String),
}

async fn trouver(state: &AppState, device_id: &str) -> Result<Appareil, Response> {
    let sortie = state.outputs.lock().await.get(device_id).ok_or_else(|| {
        refus(
            StatusCode::NOT_FOUND,
            CODE_INCONNU,
            device_id,
            format!("device {device_id} is not available: it is no longer seen on the network"),
        )
    })?;
    let guard = sortie.lock().await;
    if guard.as_any().downcast_ref::<Airplay2Output>().is_some() {
        drop(guard);
        return Ok(Appareil::Airplay2(sortie));
    }
    if let Some(ap1) = guard.as_any().downcast_ref::<AirplayOutput>() {
        return Ok(Appareil::Airplay1 {
            exige: ap1.exige_l_appairage_airplay2(),
        });
    }
    Ok(Appareil::Autre(guard.output_type().to_string()))
}

/// Un appareil qui n'est pas servi par le composant AirPlay 2 : la réponse
/// qui dit pourquoi il n'y a rien à jumeler, ou pourquoi on ne le peut pas.
fn sans_composant(device_id: &str, appareil: Appareil) -> Response {
    match appareil {
        Appareil::Airplay2(_) => unreachable!("traité par l'appelant"),
        Appareil::Airplay1 { exige: true } if manque_pour(device_id) => refus(
            StatusCode::SERVICE_UNAVAILABLE,
            CODE_AIRPLAY2_COMPONENT_MISSING,
            device_id,
            message_composant_manquant(),
        ),
        Appareil::Airplay1 { exige: true } => refus(
            StatusCode::CONFLICT,
            CODE_SERVI_EN_AIRPLAY1,
            device_id,
            "this device needs AirPlay 2 pairing, but Tune can only reach it in AirPlay 1: it \
             did not announce its AirPlay identifier on the network. Restart the device, then \
             wait for it to be detected again"
                .to_string(),
        ),
        Appareil::Airplay1 { exige: false } => refus(
            StatusCode::CONFLICT,
            CODE_SANS_JUMELAGE,
            device_id,
            "this AirPlay device does not need pairing: create a zone with it and play".to_string(),
        ),
        Appareil::Autre(genre) => refus(
            StatusCode::CONFLICT,
            CODE_SANS_JUMELAGE,
            device_id,
            format!("pairing is not needed for {genre} devices: create a zone with it and play"),
        ),
    }
}

/// Phase de jumelage du composant, lue sans garder le verrou de la sortie.
async fn phase(sortie: &Sortie) -> String {
    let guard = sortie.lock().await;
    match guard.as_any().downcast_ref::<Airplay2Output>() {
        Some(ap2) => ap2.pairing_status().await,
        None => "idle".to_string(),
    }
}

/// Attend que la phase quitte `idle`, au plus [`ATTENTE_DU_RECEPTEUR`].
async fn attendre_la_reponse(sortie: &Sortie) -> Option<String> {
    let echeance = tokio::time::Instant::now() + ATTENTE_DU_RECEPTEUR;
    loop {
        let actuelle = phase(sortie).await;
        if actuelle != "idle" {
            return Some(actuelle);
        }
        if tokio::time::Instant::now() >= echeance {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

enum Demande<'a> {
    Code,
    Saisi(&'a str),
}

/// Envoie la demande au composant, borné par [`ATTENTE_DU_RECEPTEUR`]
/// (le démon peut ne jamais dire `ready`).
async fn envoyer(sortie: &Sortie, demande: Demande<'_>) -> Result<(), String> {
    let guard = sortie.lock().await;
    let ap2 = guard
        .as_any()
        .downcast_ref::<Airplay2Output>()
        .ok_or("output is not an AirPlay 2 receiver")?;
    let envoi = async {
        match demande {
            Demande::Code => ap2.start_pin_pairing().await,
            Demande::Saisi(pin) => ap2.submit_pin(pin).await,
        }
    };
    match tokio::time::timeout(ATTENTE_DU_RECEPTEUR, envoi).await {
        Ok(resultat) => resultat,
        Err(_) => Err(format!(
            "the AirPlay 2 component did not start within {} s",
            ATTENTE_DU_RECEPTEUR.as_secs()
        )),
    }
}

fn echec_du_composant(device_id: &str, erreur: String) -> Response {
    if erreur.starts_with(CODE_AIRPLAY2_COMPONENT_MISSING) {
        return refus(
            StatusCode::SERVICE_UNAVAILABLE,
            CODE_AIRPLAY2_COMPONENT_MISSING,
            device_id,
            erreur,
        );
    }
    refus(
        StatusCode::BAD_GATEWAY,
        CODE_DEMON,
        device_id,
        format!("the AirPlay 2 component could not start pairing: {erreur}"),
    )
}

fn delai_depasse(device_id: &str, etape: &str) -> Response {
    refus(
        StatusCode::GATEWAY_TIMEOUT,
        CODE_DELAI,
        device_id,
        format!(
            "the device did not answer the {etape} within {} s: check that it is on and \
             reachable, then try again",
            ATTENTE_DU_RECEPTEUR.as_secs()
        ),
    )
}

fn jumele(device_id: &str, sans_code: bool) -> Response {
    info!(device = %device_id, sans_code, "device_paired");
    Json(json!({
        "status": "paired",
        "device_id": device_id,
        "pin_required": !sans_code,
        "message": "Device paired",
    }))
    .into_response()
}

/// `POST /devices/{id}/pair`.
pub(super) async fn jumeler(state: &AppState, device_id: &str) -> Response {
    let sortie = match trouver(state, device_id).await {
        Ok(Appareil::Airplay2(sortie)) => sortie,
        Ok(autre) => return sans_composant(device_id, autre),
        Err(reponse) => return reponse,
    };
    if let Err(erreur) = envoyer(&sortie, Demande::Code).await {
        return echec_du_composant(device_id, erreur);
    }
    let refus_du_code = match attendre_la_reponse(&sortie).await {
        None => return delai_depasse(device_id, "pairing request"),
        Some(p) if p == "pin_requested" => {
            info!(device = %device_id, "device_pair_awaiting_pin");
            return Json(json!({
                "status": "awaiting_pin",
                "device_id": device_id,
                "message": "Enter the 4-digit PIN shown on the device screen",
            }))
            .into_response();
        }
        Some(p) if p == "connected" => return jumele(device_id, true),
        Some(p) => p.strip_prefix("failed:").unwrap_or(&p).to_string(),
    };

    // Pas de code affiché : le jumelage transitoire, celui de la lecture.
    if let Err(erreur) = envoyer(&sortie, Demande::Saisi(CODE_TRANSITOIRE)).await {
        return refus(
            StatusCode::BAD_GATEWAY,
            CODE_ECHEC,
            device_id,
            format!(
                "the device refused PIN pairing ({refus_du_code}) and could not be paired without a PIN: {erreur}"
            ),
        );
    }
    match attendre_la_reponse(&sortie).await {
        None => delai_depasse(device_id, "pairing request"),
        Some(p) if p == "connected" => jumele(device_id, true),
        Some(p) => refus(
            StatusCode::BAD_GATEWAY,
            CODE_ECHEC,
            device_id,
            format!(
                "the device refused pairing: with a PIN ({refus_du_code}), without a PIN ({})",
                p.strip_prefix("failed:").unwrap_or(&p)
            ),
        ),
    }
}

/// `POST /devices/{id}/pair/pin`.
pub(super) async fn confier_le_code(state: &AppState, device_id: &str, pin: &str) -> Response {
    let pin = pin.trim();
    if pin.is_empty() {
        return refus(
            StatusCode::BAD_REQUEST,
            "bad_request",
            device_id,
            "the PIN is empty".to_string(),
        );
    }
    let sortie = match trouver(state, device_id).await {
        Ok(Appareil::Airplay2(sortie)) => sortie,
        Ok(autre) => return sans_composant(device_id, autre),
        Err(reponse) => return reponse,
    };
    if let Err(erreur) = envoyer(&sortie, Demande::Saisi(pin)).await {
        return refus(
            StatusCode::CONFLICT,
            CODE_PAS_DE_JUMELAGE_EN_COURS,
            device_id,
            format!("no pairing in progress for this device, press Pair first ({erreur})"),
        );
    }
    match attendre_la_reponse(&sortie).await {
        None => delai_depasse(device_id, "PIN"),
        Some(p) if p == "connected" => jumele(device_id, false),
        Some(p) => refus(
            StatusCode::FORBIDDEN,
            CODE_ECHEC,
            device_id,
            format!(
                "the device refused the PIN: {}",
                p.strip_prefix("failed:").unwrap_or(&p)
            ),
        ),
    }
}

/// `POST /devices/{id}/airplay2/pair-pin-start` : la seule demande de code.
pub(super) async fn demander_le_code(state: &AppState, device_id: &str) -> Response {
    let sortie = match trouver(state, device_id).await {
        Ok(Appareil::Airplay2(sortie)) => sortie,
        Ok(autre) => return sans_composant(device_id, autre),
        Err(reponse) => return reponse,
    };
    if let Err(erreur) = envoyer(&sortie, Demande::Code).await {
        return echec_du_composant(device_id, erreur);
    }
    match attendre_la_reponse(&sortie).await {
        None => delai_depasse(device_id, "PIN request"),
        Some(p) if p == "pin_requested" => Json(json!({
            "status": "pin_requested",
            "device_id": device_id,
            "message": "Check the device screen for a 4-digit PIN",
        }))
        .into_response(),
        Some(p) if p == "connected" => jumele(device_id, true),
        Some(p) => refus(
            StatusCode::BAD_GATEWAY,
            CODE_ECHEC,
            device_id,
            format!(
                "the device refused PIN pairing: {}",
                p.strip_prefix("failed:").unwrap_or(&p)
            ),
        ),
    }
}
