//! #9044 (rc14) — dater le DÉPART d'un appareil et l'OUBLIER tout seul après
//! une durée réglable.
//!
//! La rc13 a livré l'oubli un par un ([`super::oubli_9044`]), sans durée :
//! `zones.last_seen_at` date le dernier RETOUR en ligne, pas le départ. Ce
//! module tient la vraie date de départ et l'oubli automatique.
//!
//! ## La règle
//!
//! * **Départ** : un balayage toutes les 15 min (le premier 15 min après le
//!   démarrage, le temps que la découverte retrouve les appareils) compare
//!   les appareils connus par leur zone à ceux que le réseau annonce — même
//!   règle que `GET /devices/absent`. Le premier balayage qui voit un appareil
//!   absent date son départ ; celui qui le revoit efface la date. La précision
//!   est donc d'un quart d'heure, et un serveur éteint ne date rien : une
//!   absence ne commence à compter qu'une fois le serveur là pour la voir.
//! * **Oubli automatique** : absent depuis `appareils_oubli_auto_jours` jours
//!   (30 par défaut, 0 = jamais), l'appareil est OUBLIÉ : retiré du registre
//!   des sorties et du magasin des DLNA découverts (#6299, plus de re-sondage
//!   au démarrage), retiré de la liste des absents, et marqué « oublié ».
//! * **Jamais rien d'autre.** L'oubli automatique ne supprime AUCUNE zone,
//!   aucun réglage (égaliseur, TESS, limites, volume…), ni un appareil ajouté
//!   à la main (`POST /devices/add` : c'est un réglage de l'utilisateur, il
//!   ne revient pas tout seul par la découverte). La zone reste, hors ligne,
//!   marquée « appareil oublié » (`device_forgotten` dans `GET /zones`) ; la
//!   supprimer est un geste de l'utilisateur (`DELETE
//!   /devices/{id}?with_zone=true`, après une confirmation qui la nomme).
//! * **Retour** : un appareil oublié qui revient retrouve SA zone (la ligne
//!   n'a pas bougé, la découverte la reprend par son `output_device_id`) ; le
//!   balayage suivant efface la marque. `GET /zones` ne dit d'ailleurs
//!   « oublié » que d'une zone hors ligne.
//! * **Une zone en lecture** n'est jamais oubliée.
//!
//! ## Pourquoi 30 jours par défaut
//!
//! Un appareil parti en vacances avec son propriétaire, une enceinte de
//! jardin rentrée pour l'hiver ou un ampli en réparation reviennent en
//! quelques semaines ; 30 jours les couvre. Et l'oubli ne détruit rien : un
//! appareil oublié à tort revient comme avant, avec sa zone et ses réglages.
//!
//! ## Stockage
//!
//! Trois clés de `settings`, sans migration (SQLite et PostgreSQL pareils) :
//! [`CLE_JOURS`], [`CLE_ABSENCES`], [`CLE_OUBLIES`]. Les deux listes sont
//! réécrites par `update_json_list` (une transaction, un écrivain).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{info, warn};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::{ZoneDAppareil, ZoneRepo};

use crate::auth::RequireAdmin;
use crate::state::AppState;

/// Réglage : jours d'absence avant l'oubli automatique (`0` : jamais).
pub const CLE_JOURS: &str = "appareils_oubli_auto_jours";
/// Les départs datés : `[{device_id, depuis}]`.
pub const CLE_ABSENCES: &str = "appareils_absents_depuis";
/// Les appareils oubliés : `[{device_id, le, auto}]`.
pub const CLE_OUBLIES: &str = "appareils_oublies";

pub const JOURS_PAR_DEFAUT: u32 = 30;
pub const JOURS_MAX: u32 = 365;

const PREMIER_BALAYAGE: Duration = Duration::from_secs(15 * 60);
const ENTRE_DEUX_BALAYAGES: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Absence {
    pub device_id: String,
    /// RFC 3339, UTC : le premier balayage qui l'a vu absent.
    pub depuis: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Oubli {
    pub device_id: String,
    /// RFC 3339, UTC.
    pub le: String,
    /// `true` : oublié par le balayage ; `false` : par l'utilisateur.
    #[serde(default)]
    pub auto: bool,
}

fn horodatage(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn lire_date(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Le réglage lu : absent ou illisible → 30 ; borné à [`JOURS_MAX`].
pub fn jours_avant_oubli(settings: &SettingsRepo) -> u32 {
    settings
        .get(CLE_JOURS)
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map(|j| j.min(JOURS_MAX))
        .unwrap_or(JOURS_PAR_DEFAUT)
}

pub fn lire_absences(settings: &SettingsRepo) -> Vec<Absence> {
    settings.get_json_list(CLE_ABSENCES).unwrap_or_default()
}

pub fn lire_oublis(settings: &SettingsRepo) -> Vec<Oubli> {
    settings.get_json_list(CLE_OUBLIES).unwrap_or_default()
}

/// Ce que le balayage sait d'un appareil connu par sa zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EtatAppareil {
    pub device_id: String,
    pub absent: bool,
    /// Une de ses zones joue ou est en pause.
    pub en_lecture: bool,
    /// Ajouté à la main : jamais oublié tout seul.
    pub manuel: bool,
}

/// Ce qu'un balayage doit écrire.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    /// La liste des départs, réécrite en entier.
    pub absences: Vec<Absence>,
    /// Les oubliés qui sont revenus : leur marque s'efface.
    pub revenus: Vec<String>,
    /// À oublier maintenant.
    pub a_oublier: Vec<String>,
}

/// Le balayage, sans effet : pure fonction du passé, du présent et du
/// réglage. `jours == 0` : rien n'est oublié (les départs sont quand même
/// datés, pour l'affichage).
pub fn planifier(
    absences: &[Absence],
    oublis: &[Oubli],
    appareils: &[EtatAppareil],
    maintenant: DateTime<Utc>,
    jours: u32,
) -> Plan {
    let depuis: HashMap<&str, &str> = absences
        .iter()
        .map(|a| (a.device_id.as_str(), a.depuis.as_str()))
        .collect();
    let oublies: HashSet<&str> = oublis.iter().map(|o| o.device_id.as_str()).collect();
    let mut plan = Plan::default();
    for a in appareils {
        if !a.absent {
            if oublies.contains(a.device_id.as_str()) {
                plan.revenus.push(a.device_id.clone());
            }
            continue;
        }
        let date = depuis
            .get(a.device_id.as_str())
            .filter(|d| lire_date(d).is_some())
            .map(|d| d.to_string())
            .unwrap_or_else(|| horodatage(maintenant));
        if jours > 0
            && !a.en_lecture
            && !a.manuel
            && !oublies.contains(a.device_id.as_str())
            && lire_date(&date)
                .is_some_and(|d| maintenant - d >= chrono::Duration::days(i64::from(jours)))
        {
            plan.a_oublier.push(a.device_id.clone());
        }
        plan.absences.push(Absence {
            device_id: a.device_id.clone(),
            depuis: date,
        });
    }
    plan
}

/// Les appareils connus par une zone oubliable, un par appareil, dans
/// l'ordre des zones.
fn appareils_connus(zones: &[ZoneDAppareil]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for z in zones.iter().filter(|z| z.oubliable()) {
        if !ids.contains(&z.output_device_id) {
            ids.push(z.output_device_id.clone());
        }
    }
    ids
}

/// Marquer `device_id` oublié (remplace une marque précédente).
pub fn marquer_oublie(
    settings: &SettingsRepo,
    device_id: &str,
    maintenant: DateTime<Utc>,
    auto: bool,
) -> Result<(), String> {
    settings.update_json_list::<Oubli, _, _>(CLE_OUBLIES, |l| {
        l.retain(|o| o.device_id != device_id);
        l.push(Oubli {
            device_id: device_id.to_string(),
            le: horodatage(maintenant),
            auto,
        });
        Ok(())
    })
}

/// Effacer toute trace de `device_id` (retour sur le réseau, ou zone
/// supprimée par l'utilisateur).
pub fn effacer_les_traces(settings: &SettingsRepo, device_id: &str) -> Result<(), String> {
    settings.update_json_list::<Oubli, _, _>(CLE_OUBLIES, |l| {
        l.retain(|o| o.device_id != device_id);
        Ok(())
    })?;
    settings.update_json_list::<Absence, _, _>(CLE_ABSENCES, |l| {
        l.retain(|a| a.device_id != device_id);
        Ok(())
    })
}

/// Un balayage complet, à l'instant `maintenant`. Rend les appareils oubliés.
pub async fn balayer(state: &AppState, maintenant: DateTime<Utc>) -> Vec<String> {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let zones = match repo.zones_d_appareils() {
        Ok(z) => z,
        Err(e) => {
            warn!(error = %e, "appareils_balayage_lecture_zones_echouee");
            return Vec::new();
        }
    };
    let presents = super::oubli_9044::ids_presents(state).await;
    let manuels: HashSet<String> = super::load_manual_devices(state)
        .iter()
        .map(|d| d.device_id())
        .collect();
    let mut appareils = Vec::new();
    for device_id in appareils_connus(&zones) {
        let siennes: Vec<ZoneDAppareil> = zones
            .iter()
            .filter(|z| z.output_device_id == device_id)
            .cloned()
            .collect();
        let mut en_lecture = false;
        for z in &siennes {
            if state.playback.get_state(z.id).await.state != tune_core::playback::PlayState::Stopped
            {
                en_lecture = true;
            }
        }
        appareils.push(EtatAppareil {
            absent: !super::oubli_9044::appareil_present(&device_id, &siennes, &presents),
            en_lecture,
            manuel: manuels.contains(&device_id),
            device_id,
        });
    }
    let plan = planifier(
        &lire_absences(&settings),
        &lire_oublis(&settings),
        &appareils,
        maintenant,
        jours_avant_oubli(&settings),
    );
    let absences = plan.absences.clone();
    if let Err(e) = settings.update_json_list::<Absence, _, _>(CLE_ABSENCES, |l| {
        *l = absences;
        Ok(())
    }) {
        warn!(error = %e, "appareils_balayage_absences_non_ecrites");
    }
    for device_id in &plan.revenus {
        if let Err(e) = effacer_les_traces(&settings, device_id) {
            warn!(device_id, error = %e, "appareil_revenu_marque_restee");
        } else {
            info!(device_id, "appareil_oublie_revenu");
            emettre_zones(state, &zones, device_id);
        }
    }
    let mut oublies = Vec::new();
    for device_id in plan.a_oublier {
        let retiree = state.outputs.lock().await.remove(&device_id);
        tune_core::outputs::registry::arreter_les_sorties_retirees(retiree).await;
        super::forget_discovered_dlna(&state.backend, &device_id);
        if let Err(e) = marquer_oublie(&settings, &device_id, maintenant, true) {
            warn!(device_id, error = %e, "appareil_oubli_auto_non_ecrit");
            continue;
        }
        info!(device_id, "appareil_oublie_automatiquement");
        emettre_zones(state, &zones, &device_id);
        oublies.push(device_id);
    }
    oublies
}

/// `GET /zones` : `device_forgotten: true` et `device_forgotten_at` sur la
/// zone HORS LIGNE d'un appareil oublié. En ligne, l'appareil est revenu :
/// rien n'est dit (le balayage suivant efface la marque).
pub fn injecter_oubli(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    device_id: Option<&str>,
    online: bool,
    oublis: &[Oubli],
) {
    let oubli = device_id
        .filter(|_| !online)
        .and_then(|d| oublis.iter().find(|o| o.device_id == d));
    obj.insert("device_forgotten".into(), json!(oubli.is_some()));
    if let Some(o) = oubli {
        obj.insert("device_forgotten_at".into(), json!(o.le));
    }
}

/// Les clients relisent les zones de l'appareil (`device_forgotten`).
fn emettre_zones(state: &AppState, zones: &[ZoneDAppareil], device_id: &str) {
    for z in zones
        .iter()
        .filter(|z| z.output_device_id == device_id && !z.hidden)
    {
        state.event_bus.emit_typed(
            tune_core::event_types::EventType::ZoneUpdated,
            json!({ "id": z.id }),
        );
    }
}

/// Le balayage, armé au démarrage.
pub fn armer(state: &AppState) {
    let state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(PREMIER_BALAYAGE).await;
        let mut ticker = tokio::time::interval(ENTRE_DEUX_BALAYAGES);
        loop {
            ticker.tick().await;
            let oublies = balayer(&state, Utc::now()).await;
            if !oublies.is_empty() {
                info!(count = oublies.len(), "appareils_oublies_automatiquement");
            }
        }
    });
}

/// `GET /devices/forget-policy` — `{after_days, default_days, max_days}`.
pub(crate) async fn lire_la_regle(State(state): State<AppState>) -> Response {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    Json(json!({
        "after_days": jours_avant_oubli(&settings),
        "default_days": JOURS_PAR_DEFAUT,
        "max_days": JOURS_MAX,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub(crate) struct NouvelleRegle {
    after_days: u32,
}

/// `PUT /devices/forget-policy` `{after_days}` (administrateur) — `0` :
/// jamais d'oubli automatique.
pub(crate) async fn ecrire_la_regle(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(r): Json<NouvelleRegle>,
) -> Response {
    if r.after_days > JOURS_MAX {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "after_days", "max_days": JOURS_MAX})),
        )
            .into_response();
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Err(e) = settings.set(CLE_JOURS, &r.after_days.to_string()) {
        warn!(error = %e, "appareils_regle_d_oubli_non_ecrite");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "database_error"})),
        )
            .into_response();
    }
    info!(after_days = r.after_days, "appareils_regle_d_oubli_changee");
    Json(json!({"after_days": r.after_days})).into_response()
}

#[cfg(test)]
#[path = "oubli_auto_9044_tests.rs"]
mod tests;
