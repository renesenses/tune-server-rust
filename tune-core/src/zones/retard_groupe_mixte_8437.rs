//! #8437 (Jon, fil 2757) — le retard manuel d'une sortie locale groupée avec
//! des enceintes réseau.
//!
//! Un groupe qui mêle une sortie locale (DAC d'un Pi) et une enceinte AirPlay
//! est un groupe de COMMANDES (`independent_renderers`, `supported: false`) :
//! chaque membre démarre au mieux. L'enceinte AirPlay tamponne environ 2 s
//! avant de jouer ; la carte locale joue tout de suite. Rien ne compensait :
//! le réglage `sync_delay_ms` de la zone n'était lu que par la sortie double
//! (deux sorties locales de la même machine).
//!
//! Version minimale (décision du 11/11) : une zone LOCALE membre d'un groupe
//! qui compte au moins une zone RÉSEAU retarde le départ de sa sortie de son
//! propre `sync_delay_ms`, réglé à l'oreille. Le retard vaut à chaque
//! ouverture du flux (départ, piste suivante non enchaînée, saut) ; une
//! transition gapless ne rouvre rien, l'avance prise est gardée. Pas de
//! mesure automatique de la latence du récepteur.
//!
//! Un groupe entièrement local reste à la sortie double (#6132), qui lit déjà
//! `sync_delay_ms` comme décalage B − A : il n'est pas retardé ici. Une zone
//! réseau n'est jamais retardée : sa lecture déclenche le relais de groupe, la
//! retarder retarderait d'autant les suiveuses.

use std::sync::Arc;

use serde_json::Value;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::{Zone, ZoneRepo};

/// Plafond du retard : la borne du champ de l'interface (10 s).
pub const RETARD_MAX_MS: u64 = 10_000;

fn est_locale(zone: &Zone) -> bool {
    zone.output_device_id
        .as_deref()
        .is_some_and(|d| d.starts_with("local:"))
}

fn est_reseau(zone: &Zone) -> bool {
    zone.output_device_id
        .as_deref()
        .is_some_and(|d| !d.trim().is_empty() && !d.starts_with("local:"))
}

/// Le retard de départ (ms) de la sortie de `zone_id` : son `sync_delay_ms`
/// (borné à `0..=RETARD_MAX_MS`) si c'est une zone locale membre d'un groupe
/// qui compte une zone réseau, sinon `0`.
pub fn retard_de_depart_ms(groupes: &[Value], zones: &[Zone], zone_id: i64) -> u64 {
    let Some(zone) = zones.iter().find(|z| z.id == Some(zone_id)) else {
        return 0;
    };
    if !est_locale(zone) || zone.sync_delay_ms <= 0 {
        return 0;
    }
    let groupe_mixte = groupes.iter().any(|groupe| {
        let ids: Vec<i64> = groupe
            .get("zone_ids")
            .and_then(Value::as_array)
            .map(|l| l.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        ids.contains(&zone_id)
            && ids
                .iter()
                .filter(|id| **id != zone_id)
                .filter_map(|id| zones.iter().find(|z| z.id == Some(*id)))
                .any(est_reseau)
    });
    if !groupe_mixte {
        return 0;
    }
    (zone.sync_delay_ms as u64).min(RETARD_MAX_MS)
}

/// [`retard_de_depart_ms`] lu en base (groupes enregistrés et zones).
pub fn retard_de_depart_de_la_zone(db: &Arc<dyn DbBackend>, zone_id: i64) -> u64 {
    let groupes = super::groupes_enregistres::lire(&SettingsRepo::with_backend(db.clone()));
    if groupes.is_empty() {
        return 0;
    }
    let zones = ZoneRepo::with_backend(db.clone())
        .list()
        .unwrap_or_default();
    retard_de_depart_ms(&groupes, &zones, zone_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn zone(id: i64, device: &str, delai: i32) -> Zone {
        let mut z: Zone = serde_json::from_value(json!({
            "id": id, "name": format!("z{id}"), "output_type": null,
            "output_device_id": device, "volume": 50.0, "muted": false,
            "online": true, "gapless_enabled": true, "group_id": null,
            "sync_delay_ms": 0, "last_position_ms": 0, "last_track_id": null,
            "last_track_source": null, "last_track_source_id": null,
            "max_sample_rate": null, "fixed_volume": false,
            "autoplay_enabled": false
        }))
        .unwrap();
        z.sync_delay_ms = delai;
        z
    }

    fn groupe(zones: &[i64]) -> Value {
        json!({ "id": 1, "leader_id": zones[0], "zone_ids": zones })
    }

    #[test]
    fn la_zone_locale_d_un_groupe_mixte_prend_son_reglage() {
        let zones = [zone(1, "local:DAC+", 2000), zone(2, "airplay2:salon", 0)];
        assert_eq!(retard_de_depart_ms(&[groupe(&[1, 2])], &zones, 1), 2000);
        // Meneuse réseau : la locale suiveuse est retardée aussi.
        assert_eq!(retard_de_depart_ms(&[groupe(&[2, 1])], &zones, 1), 2000);
    }

    #[test]
    fn sans_reglage_ou_hors_groupe_mixte_aucun_retard() {
        let sans = [zone(1, "local:DAC+", 0), zone(2, "airplay2:salon", 0)];
        assert_eq!(retard_de_depart_ms(&[groupe(&[1, 2])], &sans, 1), 0);
        let avec = [zone(1, "local:DAC+", 2000), zone(2, "airplay2:salon", 0)];
        assert_eq!(retard_de_depart_ms(&[], &avec, 1), 0, "pas de groupe");
        // La zone réseau n'est jamais retardée.
        let reseau = [zone(1, "local:DAC+", 0), zone(2, "airplay2:salon", 2000)];
        assert_eq!(retard_de_depart_ms(&[groupe(&[1, 2])], &reseau, 2), 0);
        // Groupe tout local : c'est la sortie double qui lit le réglage.
        let local = [zone(1, "local:A", 2000), zone(2, "local:B", 0)];
        assert_eq!(retard_de_depart_ms(&[groupe(&[1, 2])], &local, 1), 0);
        // Réglage négatif : aucun retard (on ne peut pas avancer une sortie).
        let negatif = [zone(1, "local:DAC+", -300), zone(2, "airplay2:salon", 0)];
        assert_eq!(retard_de_depart_ms(&[groupe(&[1, 2])], &negatif, 1), 0);
    }

    #[test]
    fn le_retard_est_borne() {
        let zones = [zone(1, "local:DAC+", 60_000), zone(2, "airplay:x", 0)];
        assert_eq!(
            retard_de_depart_ms(&[groupe(&[1, 2])], &zones, 1),
            RETARD_MAX_MS
        );
    }
}
