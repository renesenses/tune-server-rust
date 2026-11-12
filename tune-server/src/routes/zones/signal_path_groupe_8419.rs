//! #8419 (fil 2215, go de Bertrand du 12/11) — le chemin du signal d'une
//! zone en LECTURE GROUPÉE dit chaque sortie du groupe et son étage final.
//!
//! Gros Bidon (rc11, Windows) groupe deux sorties locales (SMSL SU-8 et
//! TASCAM US-366, WASAPI exclusif) : le chemin du signal ne décrivait que la
//! zone affichée, rien ne disait que la lecture partait aussi vers la
//! seconde sortie, ni ce que celle-ci recevait (format, rééchantillonnage).
//!
//! `signal_path.group_outputs` (clé ABSENTE hors groupe) :
//! `{group_id, leader_id, outputs: [{zone_id, name, output_type, this_zone,
//! leader, state, bit_perfect, rate_conversion, final_stage}]}`, une entrée
//! par zone du groupe (`zone_groups`), dans l'ordre du groupe. `final_stage`
//! est la dernière étape du chemin du signal de CETTE sortie (ce qu'elle
//! reçoit), `null` quand elle ne joue pas.

use serde_json::{Map, Value, json};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::playback::PlayState;

use crate::state::AppState;

/// La sortie d'un membre du groupe, telle que le chemin du signal la publie.
fn sortie_du_membre(
    zone_id: i64,
    nom: &str,
    output_type: Option<&str>,
    etat: PlayState,
    chemin: Option<&Value>,
    ici: bool,
    meneuse: bool,
) -> Value {
    let final_stage = chemin
        .and_then(|c| c.get("steps"))
        .and_then(Value::as_array)
        .and_then(|s| s.last())
        .cloned();
    json!({
        "zone_id": zone_id,
        "name": nom,
        "output_type": output_type,
        "this_zone": ici,
        "leader": meneuse,
        "state": match etat {
            PlayState::Playing => "playing",
            PlayState::Paused => "paused",
            PlayState::Stopped => "stopped",
        },
        "bit_perfect": chemin.and_then(|c| c.get("bit_perfect")).cloned(),
        "rate_conversion": chemin.and_then(|c| c.get("rate_conversion")).cloned(),
        "final_stage": final_stage,
    })
}

/// Les sorties du groupe de `zone_id`, `None` si elle joue seule.
pub(crate) async fn sorties_du_groupe(state: &AppState, zone_id: i64) -> Option<Value> {
    let groupes = tune_core::zones::groupes_enregistres::lire(&SettingsRepo::with_backend(
        state.backend.clone(),
    ));
    let groupe = groupes.iter().find(|g| {
        g.get("zone_ids")
            .and_then(Value::as_array)
            .is_some_and(|ids| ids.iter().any(|v| v.as_i64() == Some(zone_id)))
    })?;
    let ids: Vec<i64> = groupe["zone_ids"]
        .as_array()?
        .iter()
        .filter_map(Value::as_i64)
        .collect();
    if ids.len() < 2 {
        return None;
    }
    let meneuse = groupe
        .get("leader_id")
        .and_then(Value::as_i64)
        .or_else(|| ids.first().copied());
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let devices = state.scanner.devices().await;
    let audio_backend_pref = state.display_audio_backend();
    #[cfg(feature = "local-audio")]
    let audio_backend = tune_core::outputs::local::active_backend_name(&audio_backend_pref);
    #[cfg(not(feature = "local-audio"))]
    let audio_backend = {
        let _ = &audio_backend_pref;
        "none"
    };
    let mut sorties = Vec::with_capacity(ids.len());
    for id in ids {
        let Ok(Some(zone)) = repo.get(id) else {
            continue;
        };
        let ps = state.playback.get_state(id).await;
        let renderer_label = zone
            .output_device_id
            .as_deref()
            .and_then(|d| devices.iter().find(|x| x.id == d).map(|x| x.name.as_str()));
        let wire = match ps
            .now_playing
            .as_ref()
            .and_then(|np| np.stream_id.as_deref())
        {
            Some(sid) => state.streamer.stream_output_wire(sid).await,
            None => None,
        };
        let backend_sortie =
            super::backend_de_la_sortie_de_la_zone(state, zone.output_device_id.as_deref()).await;
        let audio_backend_de_la_zone =
            super::backend_affiche_pour_la_sortie(audio_backend, backend_sortie.as_deref());
        let chemin = super::signal_path::build_signal_path(
            &ps,
            &zone,
            &state.backend,
            renderer_label,
            audio_backend_de_la_zone,
            wire.as_ref(),
        );
        sorties.push(sortie_du_membre(
            id,
            &zone.name,
            zone.output_type.as_deref(),
            ps.state,
            chemin.as_ref(),
            id == zone_id,
            Some(id) == meneuse,
        ));
    }
    Some(json!({
        "group_id": groupe.get("id"),
        "leader_id": meneuse,
        "outputs": sorties,
    }))
}

/// Pose `signal_path.group_outputs` sur la zone sérialisée `obj`, quand elle
/// a un chemin du signal (elle joue) et appartient à un groupe.
pub(crate) async fn injecter_les_sorties_du_groupe(
    state: &AppState,
    zone_id: i64,
    obj: &mut Map<String, Value>,
) {
    if !obj.get("signal_path").is_some_and(Value::is_object) {
        return;
    }
    if let Some(groupe) = sorties_du_groupe(state, zone_id).await
        && let Some(chemin) = obj.get_mut("signal_path")
    {
        chemin["group_outputs"] = groupe;
    }
}

#[cfg(test)]
#[path = "signal_path_groupe_8419_tests.rs"]
mod tests;
