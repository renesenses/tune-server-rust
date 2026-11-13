//! Les groupes de lecture rangés dans le réglage `zone_groups` (#7937).
//!
//! Fil forum 2661 : deux zones groupées ne se dégroupaient plus, et une zone
//! supprimée puis recréée revenait « avec la mémoire » du groupe. Supprimer
//! une zone n'est qu'un MASQUAGE : la ligne garde son numéro, et
//! `POST /zones` sur le même appareil la ressuscite sous ce numéro. Or rien
//! ne retirait ce numéro de `zone_groups` : la zone recréée retombait dans
//! son ancien groupe, et le relais de groupe (#6155) la refaisait suivre sa
//! meneuse. De son côté, `POST /zones/group` empilait un nouveau groupe sans
//! retirer ses zones de leurs groupes précédents : un ancien groupe survivait
//! au nouveau, et défaire l'un laissait l'autre en place.
//!
//! Les règles tiennent ici, en un seul endroit :
//! - une zone appartient à UN groupe au plus ;
//! - un groupe réduit à moins de deux zones n'est plus un groupe ;
//! - une meneuse retirée cède sa place à la première zone qui reste.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// La clé du réglage.
pub const CLE_ZONE_GROUPS: &str = "zone_groups";

fn ids_du_groupe(groupe: &Value) -> Vec<i64> {
    groupe
        .get("zone_ids")
        .and_then(Value::as_array)
        .map(|l| l.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

/// Retire `zones` de tous les groupes de `groupes`. Un groupe qui tombe sous
/// deux zones disparaît ; une meneuse retirée est remplacée par la première
/// zone restante. Rend `true` si quelque chose a changé.
pub fn retirer_les_zones(groupes: &mut Vec<Value>, zones: &[i64]) -> bool {
    if zones.is_empty() {
        return false;
    }
    let avant = groupes.clone();
    groupes.retain_mut(|groupe| {
        let ids = ids_du_groupe(groupe);
        if !ids.iter().any(|id| zones.contains(id)) {
            return true;
        }
        let restants: Vec<i64> = ids.into_iter().filter(|id| !zones.contains(id)).collect();
        if restants.len() < 2 {
            return false;
        }
        let meneuse = groupe
            .get("leader_id")
            .and_then(Value::as_i64)
            .filter(|id| restants.contains(id))
            .unwrap_or(restants[0]);
        groupe["zone_ids"] = json!(restants);
        if groupe.get("leader_id").is_some() {
            groupe["leader_id"] = json!(meneuse);
        }
        true
    });
    *groupes != avant
}

/// Le numéro du prochain groupe : un de plus que le plus grand. Le compter
/// (`len() + 1`) redonnait le numéro d'un groupe encore là dès qu'un groupe
/// plus ancien avait été défait.
pub fn prochain_numero(groupes: &[Value]) -> i64 {
    groupes
        .iter()
        .filter_map(|g| g.get("id").and_then(Value::as_i64))
        .max()
        .unwrap_or(0)
        + 1
}

/// #8107 — les AUTRES zones du groupe de `zone` (vide si elle joue seule).
pub fn zones_compagnes(groupes: &[Value], zone: i64) -> Vec<i64> {
    groupes
        .iter()
        .map(ids_du_groupe)
        .find(|ids| ids.contains(&zone))
        .map(|ids| ids.into_iter().filter(|id| *id != zone).collect())
        .unwrap_or_default()
}

/// Lit `zone_groups` (vide s'il est absent ou illisible).
pub fn lire(settings: &SettingsRepo) -> Vec<Value> {
    settings
        .get(CLE_ZONE_GROUPS)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Retire `zones` des groupes enregistrés, et réécrit le réglage seulement
/// s'il change. Rend `true` si un groupe a été modifié ou défait.
///
/// Un réglage illisible n'est pas réécrit : on ne remplace pas ce qu'on ne
/// comprend pas par une liste vide.
pub fn retirer_des_groupes_enregistres(
    backend: &Arc<dyn DbBackend>,
    zones: &[i64],
) -> Result<bool, String> {
    let settings = SettingsRepo::with_backend(backend.clone());
    let Some(brut) = settings.get(CLE_ZONE_GROUPS)? else {
        return Ok(false);
    };
    let Ok(mut groupes) = serde_json::from_str::<Vec<Value>>(&brut) else {
        return Ok(false);
    };
    if !retirer_les_zones(&mut groupes, zones) {
        return Ok(false);
    }
    settings.set(
        CLE_ZONE_GROUPS,
        &serde_json::to_string(&groupes).map_err(|e| e.to_string())?,
    )?;
    tracing::info!(zones = ?zones, restants = groupes.len(), "zone_groups_zones_retirees");
    Ok(true)
}

/// Vide `zone_groups` (toutes les zones supprimées). Rend `true` s'il y
/// avait au moins un groupe.
pub fn vider_les_groupes_enregistres(backend: &Arc<dyn DbBackend>) -> Result<bool, String> {
    let settings = SettingsRepo::with_backend(backend.clone());
    if lire(&settings).is_empty() {
        return Ok(false);
    }
    settings.set(CLE_ZONE_GROUPS, "[]")?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn groupe(id: i64, meneuse: i64, zones: &[i64]) -> Value {
        json!({ "id": id, "group_id": id.to_string(), "leader_id": meneuse, "zone_ids": zones })
    }

    #[test]
    fn un_groupe_de_deux_disparait_quand_une_zone_part() {
        let mut g = vec![groupe(1, 1, &[1, 2])];
        assert!(retirer_les_zones(&mut g, &[2]));
        assert!(g.is_empty());
    }

    #[test]
    fn un_groupe_de_trois_garde_les_deux_autres_et_change_de_meneuse() {
        let mut g = vec![groupe(1, 1, &[1, 2, 3])];
        assert!(retirer_les_zones(&mut g, &[1]));
        assert_eq!(g[0]["zone_ids"], json!([2, 3]));
        assert_eq!(g[0]["leader_id"], json!(2));
    }

    #[test]
    fn un_groupe_sans_ces_zones_reste_intact() {
        let mut g = vec![groupe(1, 1, &[1, 2])];
        assert!(!retirer_les_zones(&mut g, &[5]));
        assert_eq!(g, vec![groupe(1, 1, &[1, 2])]);
    }

    #[test]
    fn le_prochain_numero_ne_reprend_pas_celui_d_un_groupe_restant() {
        // Le groupe 1 a été défait, le 2 reste : compter donnait 2.
        assert_eq!(prochain_numero(&[groupe(2, 3, &[3, 4])]), 3);
        assert_eq!(prochain_numero(&[]), 1);
    }
}
