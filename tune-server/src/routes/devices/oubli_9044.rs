//! #9044 — OUBLIER un appareil qui n'est plus sur le réseau.
//!
//! Chaque appareil vu reçoit une zone ; parti du réseau, il restait listé,
//! grisé, à vie, et supprimer sa zone ne faisait que la masquer — masquée,
//! elle empêchait même l'appareil revenu de réapparaître.
//!
//! * `GET /devices/absent` : les appareils connus (par leur zone) qui ne sont
//!   plus là. **Sans durée** : `zones.last_seen_at` date le dernier RETOUR en
//!   ligne, pas le départ — l'afficher comme « absent depuis » mentirait. Une
//!   vraie date de départ viendra avec l'oubli groupé (rc14).
//! * `DELETE /devices/{id}` : oublier UN appareil — registre, appareils
//!   ajoutés à la main, DLNA découverts (#6299) et ses zones, supprimées
//!   DÉFINITIVEMENT pour qu'il revienne comme neuf s'il revient.
//!
//! Pas d'oubli groupé en rc13 (décision de Bertrand, 18/11).
//!
//! ## La zone liée — le choix le plus sûr
//!
//! Une zone porte des réglages (DSP, volume, limites) : on ne la supprime
//! jamais à l'insu de l'utilisateur.
//!
//! * zone **en lecture** (ou en pause) : `409 zone_en_lecture`, qui la nomme.
//!   Jamais d'oubli sous une lecture.
//! * zone **visible** d'un appareil **présent** : `409 appareil_present`. Le
//!   supprimer ne servirait à rien (la découverte le recrée) et casserait une
//!   zone qui marche ; c'est « Ignorer » (#1280) qui le fait taire.
//! * zone **visible** d'un appareil absent : `409 zone_liee`, qui nomme la
//!   zone, tant que la requête ne porte pas `?with_zone=true` — que le client
//!   n'envoie qu'après une confirmation qui a nommé la zone.
//! * zone **masquée par l'utilisateur** (`suppression_utilisateur`,
//!   `suppression_totale`, sans motif), y compris celle qui a lâché sa sortie
//!   (#8987) : supprimée — c'est elle qui empêchait l'appareil de revenir.
//! * zone masquée par un **garde-fou de la découverte** (`fusion`,
//!   `zone_reflet`, `appareil_ignore`…) : jamais touchée (voir
//!   `tune_core::db::zone_repo::ZoneDAppareil::oubliable`).
//!
//! `DELETE` exige l'administrateur (`RequireAdmin`) : l'écriture est
//! destructive. Les autres routes du parc (`ignore`, `clear`…) ne l'exigent
//! pas encore ; aucune app (iOS, Android, Flutter, macOS) n'appelle
//! `DELETE /devices/{id}`, dont la réponse passe de 204 à 200 avec un bilan.

use std::collections::HashSet;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};
use tune_core::db::zone_repo::{ZoneDAppareil, ZoneRepo};

use crate::auth::RequireAdmin;
use crate::state::AppState;

/// Les identifiants que le réseau annonce en ce moment : scanner (identités
/// secondaires comprises) et registre des sorties.
async fn ids_presents(state: &AppState) -> HashSet<String> {
    let mut ids: HashSet<String> = state.outputs.lock().await.list().into_iter().collect();
    for d in state.scanner.devices().await {
        if let Some(alts) = d.capabilities.get("alternatives").and_then(Value::as_array) {
            ids.extend(
                alts.iter()
                    .filter_map(|a| a.get("id").and_then(Value::as_str))
                    .map(str::to_owned),
            );
        }
        ids.insert(d.id);
    }
    ids
}

/// La zone désigne-t-elle un appareil ABSENT ?
///
/// Réseau : même règle que `online` dans `GET /zones` (`zones::lecture`) —
/// présent s'il est annoncé par le scanner ou inscrit au registre. Une zone
/// navigateur n'a pas d'appareil : jamais absente.
///
/// Sortie LOCALE : règle DIFFÉRENTE, et voulue. `GET /zones` la dit en ligne
/// dès qu'elle a un `output_device_id`, ce qui rendrait impossible d'oublier
/// un DAC débranché pour de bon. On lit ici la colonne `online`, que le relevé
/// des périphériques met à 0 quand le périphérique disparaît (#1626, #6551).
pub(crate) fn zone_absente(z: &ZoneDAppareil, presents: &HashSet<String>) -> bool {
    match z.output_type.as_deref() {
        Some("browser") => false,
        Some("local") => !z.online,
        _ => !presents.contains(&z.output_device_id),
    }
}

/// Pourquoi l'oubli est refusé, avec la zone à nommer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refus {
    ZoneEnLecture { zone_id: i64, zone: String },
    AppareilPresent { zone_id: i64, zone: String },
    ZoneLiee { zone_id: i64, zone: String },
}

impl Refus {
    fn reponse(&self, device_id: &str) -> Response {
        let (code, cle, zone_id, zone) = match self {
            Self::ZoneEnLecture { zone_id, zone } => (
                "zone_en_lecture",
                "api.devices.forgetZonePlaying",
                zone_id,
                zone,
            ),
            Self::AppareilPresent { zone_id, zone } => (
                "appareil_present",
                "api.devices.forgetDevicePresent",
                zone_id,
                zone,
            ),
            Self::ZoneLiee { zone_id, zone } => {
                ("zone_liee", "api.devices.forgetZoneLinked", zone_id, zone)
            }
        };
        (
            StatusCode::CONFLICT,
            Json(crate::message_api::avec_message(
                json!({
                    "error": code,
                    "code": code,
                    "device_id": device_id,
                    "zone_id": zone_id,
                    "zone_name": zone,
                }),
                cle,
                &[("zone", zone.clone())],
            )),
        )
            .into_response()
    }
}

/// Le verdict, sans effet : quelles zones supprimer, ou pourquoi refuser.
///
/// `zones` : les zones de l'appareil (masquées et libérées comprises) ;
/// `en_lecture` : celles qui jouent ou sont en pause ; `present` : l'appareil
/// est-il annoncé ; `avec_zone` : l'utilisateur a-t-il accepté, après une
/// confirmation qui la nommait, de perdre la zone visible.
///
/// Les zones masquées par un garde-fou de la découverte ne sont ni
/// supprimées ni bloquantes : elles restent, et c'est tout.
pub(crate) fn verdict(
    zones: &[ZoneDAppareil],
    en_lecture: &HashSet<i64>,
    present: bool,
    avec_zone: bool,
) -> Result<Vec<i64>, Refus> {
    let oubliables: Vec<&ZoneDAppareil> = zones.iter().filter(|z| z.oubliable()).collect();
    if let Some(z) = oubliables.iter().find(|z| en_lecture.contains(&z.id)) {
        return Err(Refus::ZoneEnLecture {
            zone_id: z.id,
            zone: z.name.clone(),
        });
    }
    if let Some(z) = oubliables.iter().find(|z| !z.hidden) {
        if present {
            return Err(Refus::AppareilPresent {
                zone_id: z.id,
                zone: z.name.clone(),
            });
        }
        if !avec_zone {
            return Err(Refus::ZoneLiee {
                zone_id: z.id,
                zone: z.name.clone(),
            });
        }
    }
    Ok(oubliables.iter().map(|z| z.id).collect())
}

/// L'appareil est-il présent ? Ses zones locales répondent par `online`,
/// le reste par le réseau.
fn appareil_present(device_id: &str, zones: &[ZoneDAppareil], presents: &HashSet<String>) -> bool {
    if zones.is_empty() {
        return presents.contains(device_id);
    }
    zones.iter().any(|z| !zone_absente(z, presents))
}

/// Oublier `device_id` : vérifie, puis efface. Rend les zones supprimées,
/// avec leur nom.
async fn oublier(
    state: &AppState,
    device_id: &str,
    avec_zone: bool,
    presents: &HashSet<String>,
) -> Result<Vec<(i64, String)>, Response> {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let zones = repo.zones_de_l_appareil(device_id).map_err(|e| {
        warn!(device_id, error = %e, "appareil_oubli_lecture_zones_echouee");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "database_error"})),
        )
            .into_response()
    })?;
    let mut en_lecture = HashSet::new();
    for z in &zones {
        if state.playback.get_state(z.id).await.state != tune_core::playback::PlayState::Stopped {
            en_lecture.insert(z.id);
        }
    }
    let present = appareil_present(device_id, &zones, presents);
    let a_supprimer = verdict(&zones, &en_lecture, present, avec_zone).map_err(|r| {
        info!(device_id, refus = ?r, "appareil_oubli_refuse");
        r.reponse(device_id)
    })?;

    // Ce que faisait déjà `DELETE /devices/{id}` : registre, appareils
    // manuels, DLNA découverts (#6299).
    let retiree = state.outputs.lock().await.remove(device_id);
    // #7949 — arrêtée hors du verrou.
    tune_core::outputs::registry::arreter_les_sorties_retirees(retiree).await;
    super::forget_manual_device(state, device_id);
    super::forget_discovered_dlna(&state.backend, device_id);

    let mut supprimees: Vec<(i64, String)> = Vec::new();
    for id in a_supprimer {
        let nom = zones
            .iter()
            .find(|z| z.id == id)
            .map(|z| z.name.clone())
            .unwrap_or_default();
        tune_core::audio::dash_epingles::liberer_la_zone(id);
        match repo.supprimer_definitivement(id) {
            Ok(n) if n > 0 => {
                supprimees.push((id, nom));
                state.event_bus.emit_typed(
                    tune_core::event_types::EventType::ZoneDeleted,
                    json!({"id": id}),
                );
            }
            Ok(_) => {}
            Err(e) => {
                warn!(device_id, zone_id = id, error = %e, "appareil_oubli_zone_non_supprimee");
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "database_error", "zones_deleted": corps_des_zones(&supprimees)})),
                )
                    .into_response());
            }
        }
    }
    state.event_bus.emit_typed(
        tune_core::event_types::EventType::DeviceLost,
        json!({ "device_id": device_id }),
    );
    info!(device_id, zones = ?supprimees, "appareil_oublie");
    Ok(supprimees)
}

fn corps_des_zones(zones: &[(i64, String)]) -> Value {
    json!(
        zones
            .iter()
            .map(|(id, nom)| json!({"zone_id": id, "zone_name": nom}))
            .collect::<Vec<_>>()
    )
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct OublierQuery {
    #[serde(default)]
    with_zone: bool,
}

/// `DELETE /devices/{id}` — oublier l'appareil.
///
/// `200 {device_id, forgotten: true, zones_deleted: [{zone_id, zone_name}]}`
/// — chaque zone supprimée est NOMMÉE ; `409` (voir l'en-tête du module) avec
/// `code`, `zone_id`, `zone_name` et une phrase qui nomme la zone.
pub(crate) async fn delete_device(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    Query(q): Query<OublierQuery>,
) -> Response {
    if device_id.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "device_id"}))).into_response();
    }
    let presents = ids_presents(&state).await;
    match oublier(&state, &device_id, q.with_zone, &presents).await {
        Ok(zones) => Json(json!({
            "device_id": device_id,
            "forgotten": true,
            "zones_deleted": corps_des_zones(&zones),
        }))
        .into_response(),
        Err(r) => r,
    }
}

/// Les appareils absents, un par appareil, avec TOUTES les zones qu'un oubli
/// emporterait (visibles, masquées par l'utilisateur, libérées #8987). Un
/// appareil dont aucune zone n'est oubliable n'est pas listé : l'oublier ne
/// supprimerait rien de ce que l'utilisateur voit.
pub(crate) fn absents(zones: &[ZoneDAppareil], presents: &HashSet<String>) -> Vec<Value> {
    let mut ordre: Vec<&str> = Vec::new();
    for z in zones.iter().filter(|z| z.oubliable()) {
        if !ordre.contains(&z.output_device_id.as_str()) {
            ordre.push(&z.output_device_id);
        }
    }
    ordre
        .into_iter()
        .filter_map(|device_id| {
            let toutes: Vec<ZoneDAppareil> = zones
                .iter()
                .filter(|z| z.output_device_id == device_id)
                .cloned()
                .collect();
            if appareil_present(device_id, &toutes, presents) {
                return None;
            }
            let siennes: Vec<&ZoneDAppareil> = toutes.iter().filter(|z| z.oubliable()).collect();
            // La zone qui le représente : une visible d'abord.
            let tete = siennes.iter().find(|z| !z.hidden).unwrap_or(&siennes[0]);
            Some(json!({
                "device_id": device_id,
                "name": tete.name,
                "type": tete.output_type,
                "zone_id": tete.id,
                "zones": siennes
                    .iter()
                    .map(|z| json!({
                        "zone_id": z.id,
                        "zone_name": z.name,
                        "zone_hidden": z.hidden,
                    }))
                    .collect::<Vec<_>>(),
            }))
        })
        .collect()
}

/// `GET /devices/absent` — les appareils connus qui ne sont plus là.
pub(crate) async fn list_absent_devices(State(state): State<AppState>) -> Response {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let zones = match repo.zones_d_appareils() {
        Ok(z) => z,
        Err(e) => {
            warn!(error = %e, "appareils_absents_lecture_echouee");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "database_error"})),
            )
                .into_response();
        }
    };
    let presents = ids_presents(&state).await;
    let items = absents(&zones, &presents);
    Json(json!({
        "total": items.len(),
        "items": items,
    }))
    .into_response()
}

#[cfg(test)]
#[path = "oubli_9044_tests.rs"]
mod tests;
