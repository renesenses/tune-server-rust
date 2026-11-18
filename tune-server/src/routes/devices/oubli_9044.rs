//! #9044 — OUBLIER les appareils qui ne sont plus sur le réseau.
//!
//! Chaque appareil vu reçoit une zone ; parti du réseau, il restait listé,
//! grisé, à vie, et supprimer sa zone ne faisait que la masquer — masquée,
//! elle empêchait même l'appareil revenu de réapparaître.
//!
//! * `GET /devices/absent` : les appareils connus (par leur zone) qui ne sont
//!   plus là, avec la dernière fois qu'ils ont répondu ;
//! * `DELETE /devices/{id}` : oublier UN appareil — registre, appareils
//!   ajoutés à la main, DLNA découverts (#6299) et sa zone, supprimée
//!   DÉFINITIVEMENT pour qu'il revienne comme neuf s'il revient ;
//! * `POST /devices/absent/forget?older_than_days=N` : oublier ceux qui sont
//!   absents depuis plus de N jours (7 par défaut).
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
//!   zone, tant que la requête ne porte pas `?with_zone=true`. Le client
//!   affiche le nom dans sa confirmation, puis renvoie avec l'accord.
//! * zone **masquée** (déjà supprimée par l'utilisateur) : supprimée sans
//!   condition — c'est elle qui empêchait l'appareil de revenir.
//!
//! Réservé à l'administrateur (`RequireAdmin`), comme les autres écritures
//! sur le parc.

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

/// Âge d'absence par défaut de « Oublier les appareils absents ».
pub(crate) const JOURS_PAR_DEFAUT: i64 = 7;

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
/// Même règle que `online` dans `GET /zones` (`zones::lecture`) : une zone
/// navigateur n'a pas d'appareil ; une sortie locale suit la colonne `online`,
/// tenue par le relevé des périphériques (#1626, #6551) ; le reste est présent
/// s'il est annoncé par le scanner ou inscrit au registre.
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
/// `zones` : les zones de l'appareil (masquées comprises) ; `en_lecture` :
/// celles qui jouent ou sont en pause ; `present` : l'appareil est-il annoncé ;
/// `avec_zone` : l'utilisateur a-t-il accepté de perdre la zone visible.
pub(crate) fn verdict(
    zones: &[ZoneDAppareil],
    en_lecture: &HashSet<i64>,
    present: bool,
    avec_zone: bool,
) -> Result<Vec<i64>, Refus> {
    if let Some(z) = zones.iter().find(|z| en_lecture.contains(&z.id)) {
        return Err(Refus::ZoneEnLecture {
            zone_id: z.id,
            zone: z.name.clone(),
        });
    }
    if let Some(z) = zones.iter().find(|z| !z.hidden) {
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
    Ok(zones.iter().map(|z| z.id).collect())
}

/// L'appareil est-il présent ? Ses zones locales répondent par `online`,
/// le reste par le réseau.
fn appareil_present(device_id: &str, zones: &[ZoneDAppareil], presents: &HashSet<String>) -> bool {
    if zones.is_empty() {
        return presents.contains(device_id);
    }
    zones.iter().any(|z| !zone_absente(z, presents))
}

/// Oublier `device_id` : vérifie, puis efface. Rend les zones supprimées.
async fn oublier(
    state: &AppState,
    device_id: &str,
    avec_zone: bool,
    presents: &HashSet<String>,
) -> Result<Vec<i64>, Response> {
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

    let mut supprimees = Vec::new();
    for id in a_supprimer {
        tune_core::audio::dash_epingles::liberer_la_zone(id);
        match repo.supprimer_definitivement(id) {
            Ok(n) if n > 0 => {
                supprimees.push(id);
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
                    Json(json!({"error": "database_error", "zones_deleted": supprimees})),
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

#[derive(Debug, Default, Deserialize)]
pub(crate) struct OublierQuery {
    #[serde(default)]
    with_zone: bool,
}

/// `DELETE /devices/{id}` — oublier l'appareil.
///
/// `200 {device_id, forgotten: true, zones_deleted: [...]}` ; `409` (voir
/// l'en-tête du module) avec `code`, `zone_id`, `zone_name` et une phrase.
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
            "zones_deleted": zones,
        }))
        .into_response(),
        Err(r) => r,
    }
}

/// Une ligne de `GET /devices/absent`.
pub(crate) fn decrire(z: &ZoneDAppareil) -> Value {
    json!({
        "device_id": z.output_device_id,
        "name": z.name,
        "type": z.output_type,
        "zone_id": z.id,
        "zone_name": z.name,
        "zone_hidden": z.hidden,
        "last_seen_at": z.last_seen_at,
        "absent_for_s": z.age_secs,
    })
}

/// Les zones absentes, triées de la plus ancienne absence à la plus récente
/// (jamais vues en tête : leur absence est la plus ancienne connue).
pub(crate) fn absentes<'a>(
    zones: &'a [ZoneDAppareil],
    presents: &HashSet<String>,
) -> Vec<&'a ZoneDAppareil> {
    let mut v: Vec<&ZoneDAppareil> = zones.iter().filter(|z| zone_absente(z, presents)).collect();
    v.sort_by_key(|z| std::cmp::Reverse(z.age_secs.unwrap_or(i64::MAX)));
    v
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
    let items: Vec<Value> = absentes(&zones, &presents)
        .into_iter()
        .map(decrire)
        .collect();
    Json(json!({
        "total": items.len(),
        "default_older_than_days": JOURS_PAR_DEFAUT,
        "items": items,
    }))
    .into_response()
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct OublierAbsentsQuery {
    older_than_days: Option<i64>,
}

/// Les appareils à oublier : absents depuis au moins `jours` jours. Une
/// absence non datée (jamais vue depuis la pose de `last_seen_at`) n'est pas
/// comptée : on ne sait pas depuis quand — l'oubli un par un reste possible.
pub(crate) fn a_oublier(absentes: &[&ZoneDAppareil], jours: i64) -> Vec<String> {
    let seuil = jours.max(0).saturating_mul(86_400);
    let mut ids: Vec<String> = Vec::new();
    for z in absentes {
        if z.age_secs.is_some_and(|a| a >= seuil) && !ids.contains(&z.output_device_id) {
            ids.push(z.output_device_id.clone());
        }
    }
    ids
}

/// `POST /devices/absent/forget?older_than_days=N` — oublier les appareils
/// absents depuis plus de N jours (7 par défaut).
///
/// `200 {older_than_days, forgotten: [...], zones_deleted: [...],
/// skipped: [{device_id, status}]}` — une zone en lecture ne bloque
/// pas les autres, elle est sautée et dite.
pub(crate) async fn forget_absent_devices(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Query(q): Query<OublierAbsentsQuery>,
) -> Response {
    let jours = q.older_than_days.unwrap_or(JOURS_PAR_DEFAUT);
    if jours < 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "older_than_days"})),
        )
            .into_response();
    }
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
    let cibles = a_oublier(&absentes(&zones, &presents), jours);
    let mut oublies = Vec::new();
    let mut zones_supprimees = Vec::new();
    let mut sautes = Vec::new();
    for device_id in cibles {
        match oublier(&state, &device_id, true, &presents).await {
            Ok(z) => {
                zones_supprimees.extend(z);
                oublies.push(device_id);
            }
            Err(r) => sautes.push(json!({
                "device_id": device_id,
                "status": r.status().as_u16(),
            })),
        }
    }
    info!(
        jours,
        oublies = oublies.len(),
        sautes = sautes.len(),
        "appareils_absents_oublies"
    );
    Json(json!({
        "older_than_days": jours,
        "forgotten": oublies,
        "zones_deleted": zones_supprimees,
        "skipped": sautes,
    }))
    .into_response()
}

#[cfg(test)]
#[path = "oubli_9044_tests.rs"]
mod tests;
