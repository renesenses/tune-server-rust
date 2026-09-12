use super::{CreateGroup, GroupRefusal, validate_group};
use tune_core::db::zone_repo::Zone;

// #1702 (Bilou, fil 1392) — deux zones pointant sur la même sortie : le
// groupement répondait « 422 unprocessable entity », un code nu, sans
// phrase. Deux causes distinctes, testées séparément :
//   1. le client web n'envoie pas de `name`, et serde rejetait le corps
//      avant même d'atteindre le handler → 422 d'axum, sans texte ;
//   2. rien ne vérifiait la sortie partagée, donc aucun message ne
//      pouvait l'expliquer.

fn zone(id: i64, name: &str, device: Option<&str>) -> Zone {
    Zone {
        id: Some(id),
        name: name.to_string(),
        output_type: Some("local".into()),
        output_device_id: device.map(str::to_string),
        volume: 50.0,
        muted: false,
        online: true,
        gapless_enabled: false,
        group_id: None,
        sync_delay_ms: 0,
        last_position_ms: 0,
        last_track_id: None,
        last_track_source: None,
        last_track_source_id: None,
        max_sample_rate: None,
        fixed_volume: false,
        autoplay_enabled: false,
    }
}

#[test]
fn payload_without_name_is_accepted() {
    // Le corps exact qu'envoie le client web. Il échouait ici.
    let body: CreateGroup =
        serde_json::from_str(r#"{"leader_id": 1, "zone_ids": [1, 2]}"#).unwrap();
    assert_eq!(body.zone_ids, vec![1, 2]);
    assert_eq!(body.leader_id, Some(1));
    assert_eq!(body.name, None);
}

#[test]
fn two_zones_on_the_same_output_are_refused_by_name() {
    let zones = vec![
        zone(1, "PC", Some("hw:0,0")),
        zone(2, "Haut parleurs", Some("hw:0,0")),
    ];
    assert_eq!(
        validate_group(&[1, 2], &zones),
        Err(GroupRefusal::SameOutput(
            "PC".into(),
            "Haut parleurs".into()
        )),
        "le refus doit nommer les deux zones pour que le message soit lisible"
    );
}

#[test]
fn two_zones_on_distinct_outputs_are_accepted() {
    let zones = vec![
        zone(1, "Salon", Some("hw:0,0")),
        zone(2, "Cuisine", Some("hw:1,0")),
    ];
    assert_eq!(validate_group(&[1, 2], &zones), Ok(vec![1, 2]));
}

#[test]
fn zones_without_an_output_are_not_duplicates_of_each_other() {
    // Deux zones orphelines ne partagent pas « la même sortie » : elles
    // n'en ont aucune. Les refuser ici afficherait un message faux.
    let zones = vec![zone(1, "Salon", None), zone(2, "Cuisine", None)];
    assert_eq!(validate_group(&[1, 2], &zones), Ok(vec![1, 2]));
}

#[test]
fn the_same_zone_twice_is_not_a_group() {
    let zones = vec![zone(1, "Salon", Some("hw:0,0"))];
    assert_eq!(
        validate_group(&[1, 1], &zones),
        Err(GroupRefusal::NotEnoughZones)
    );
}

#[test]
fn a_single_zone_is_not_a_group() {
    let zones = vec![
        zone(1, "Salon", Some("hw:0,0")),
        zone(2, "Cuisine", Some("hw:1,0")),
    ];
    assert_eq!(
        validate_group(&[1], &zones),
        Err(GroupRefusal::NotEnoughZones)
    );
}

#[test]
fn a_vanished_zone_is_named_in_the_refusal() {
    let zones = vec![zone(1, "Salon", Some("hw:0,0"))];
    assert_eq!(
        validate_group(&[1, 7], &zones),
        Err(GroupRefusal::UnknownZone(7))
    );
}

#[test]
fn duplicate_ids_are_collapsed_not_flagged_as_same_output() {
    let zones = vec![
        zone(1, "Salon", Some("hw:0,0")),
        zone(2, "Cuisine", Some("hw:1,0")),
    ];
    assert_eq!(validate_group(&[1, 2, 1], &zones), Ok(vec![1, 2]));
}

#[test]
fn every_refusal_has_a_french_sentence() {
    for key in [
        "zonegroup.needsTwoZones",
        "zonegroup.unknownZone",
        "zonegroup.sameOutput",
    ] {
        let msg = crate::i18n::t("fr", key);
        assert_ne!(
            msg, key,
            "{key} n'a pas de traduction : le client afficherait la clé"
        );
        assert!(msg.len() > 20, "{key} doit expliquer, pas juste nommer");
    }
}
