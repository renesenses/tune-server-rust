use super::PlaybackOrchestrator;

/// Le repli de `stop` ne doit JAMAIS toucher l'appareil d'une autre zone.
///
/// Le défaut mesuré sur .18 le 28/08/2026 : la zone 15 « Cet ordinateur »
/// est une sortie navigateur, donc sans `output_device_id` par
/// construction. Chaque `next` dessus tombait dans le repli, qui arrêtait
/// TOUTES les sorties enregistrées — l'Eversolo de la zone 10 compris, en
/// pleine lecture. Même famille que #2571.
#[test]
fn le_repli_de_stop_epargne_les_sorties_des_autres_zones() {
    // Zone 15 : navigateur, aucun appareil. Zones 10 et 8 : renderers.
    let zones = [
        (Some(15i64), None),
        (Some(10i64), Some("uuid:eversolo-dmp-a8")),
        (Some(8i64), Some("uuid:sonos-chambre")),
    ];
    let revendiquees = PlaybackOrchestrator::sorties_revendiquees_par_les_autres_zones(zones, 15);

    let enregistrees = vec![
        "uuid:eversolo-dmp-a8".to_string(),
        "uuid:sonos-chambre".to_string(),
        "uuid:orpheline-sans-zone".to_string(),
    ];
    let a_arreter = PlaybackOrchestrator::sorties_a_arreter_en_repli(&enregistrees, &revendiquees);

    assert!(
        !a_arreter.contains(&"uuid:eversolo-dmp-a8".to_string()),
        "un stop sur la zone 15 ne doit pas couper l'Eversolo, qui joue la zone 10"
    );
    assert!(
        !a_arreter.contains(&"uuid:sonos-chambre".to_string()),
        "ni le Sonos de la zone 8"
    );
    assert_eq!(
        a_arreter,
        vec!["uuid:orpheline-sans-zone".to_string()],
        "le repli garde son seul objet légitime : une sortie qu'aucune zone ne revendique"
    );
}

/// Et la zone qui demande l'arrêt ne s'épargne pas elle-même : si elle a
/// laissé une sortie ouverte, le repli doit encore pouvoir la fermer.
#[test]
fn le_repli_peut_toujours_fermer_la_sortie_de_la_zone_qui_arrete() {
    let zones = [
        (Some(10i64), Some("uuid:eversolo-dmp-a8")),
        (Some(8i64), Some("uuid:sonos-chambre")),
    ];
    let revendiquees = PlaybackOrchestrator::sorties_revendiquees_par_les_autres_zones(zones, 10);
    assert!(
        !revendiquees.contains("uuid:eversolo-dmp-a8"),
        "son propre appareil n'est pas « revendiqué ailleurs »"
    );

    let enregistrees = vec![
        "uuid:eversolo-dmp-a8".to_string(),
        "uuid:sonos-chambre".to_string(),
    ];
    let a_arreter = PlaybackOrchestrator::sorties_a_arreter_en_repli(&enregistrees, &revendiquees);
    assert_eq!(a_arreter, vec!["uuid:eversolo-dmp-a8".to_string()]);
}
