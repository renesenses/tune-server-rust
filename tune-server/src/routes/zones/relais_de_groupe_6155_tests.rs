//! #6155 — les règles du relais de groupe, sans base ni renderer.
//! Le banc de bout en bout (deux renderers DLNA factices) est
//! `tests/groupe_dlna_joue_sur_les_deux_6155.rs`.

use super::*;
use serde_json::json;

fn piste(track_id: i64) -> Option<Piste> {
    Some(Piste {
        source: "local".into(),
        track_id: Some(track_id),
        source_id: None,
    })
}

fn zone(etat: PlayState, piste: Option<Piste>) -> Instantane {
    Instantane { etat, piste }
}

#[test]
fn la_suiveuse_est_celle_que_le_client_web_a_cochee() {
    // Le corps exact du client web : la zone de la fiche en tête.
    let groupes = vec![json!({ "id": 1, "leader_id": 7, "zone_ids": [7, 9] })];
    assert_eq!(zones_suiveuses(&groupes, 7), vec![9]);
    assert!(
        zones_suiveuses(&groupes, 9).is_empty(),
        "une suiveuse ne mène pas : ses commandes ne valent que pour elle"
    );
}

#[test]
fn sans_leader_id_la_meneuse_est_la_premiere_zone() {
    let groupes = vec![
        json!({ "id": 1, "zone_ids": [3, 4, 3] }),
        json!({ "id": 2, "leader_zone_id": 5, "zone_ids": [6, 5] }),
    ];
    assert_eq!(zones_suiveuses(&groupes, 3), vec![4]);
    assert_eq!(zones_suiveuses(&groupes, 5), vec![6]);
    assert!(zones_suiveuses(&groupes, 42).is_empty());
}

#[test]
fn la_meneuse_lance_une_piste_la_suiveuse_muette_la_rejoint() {
    // Le cas de Pierre : le Phantom joue, la KEF ne fait rien.
    assert_eq!(
        geste_pour_la_suiveuse(
            &zone(PlayState::Playing, piste(1)),
            &zone(PlayState::Stopped, None)
        ),
        Some(Geste::Rejoindre),
        "la suiveuse à l'arrêt doit prendre la piste de la meneuse — sinon \
         le groupe ne produit du son que sur une enceinte (#6155)"
    );
    assert_eq!(
        geste_pour_la_suiveuse(
            &zone(PlayState::Playing, piste(2)),
            &zone(PlayState::Playing, piste(1))
        ),
        Some(Geste::Rejoindre),
        "la meneuse passe au morceau suivant : la suiveuse suit"
    );
}

#[test]
fn une_suiveuse_deja_sur_la_piste_n_est_pas_relancee() {
    assert_eq!(
        geste_pour_la_suiveuse(
            &zone(PlayState::Playing, piste(1)),
            &zone(PlayState::Playing, piste(1))
        ),
        None
    );
    assert_eq!(
        geste_pour_la_suiveuse(
            &zone(PlayState::Playing, piste(1)),
            &zone(PlayState::Paused, piste(1))
        ),
        Some(Geste::Reprise)
    );
}

#[test]
fn pause_et_arret_de_la_meneuse_sont_relayes() {
    assert_eq!(
        geste_pour_la_suiveuse(
            &zone(PlayState::Paused, piste(1)),
            &zone(PlayState::Playing, piste(1))
        ),
        Some(Geste::Pause)
    );
    assert_eq!(
        geste_pour_la_suiveuse(
            &zone(PlayState::Paused, piste(1)),
            &zone(PlayState::Stopped, None)
        ),
        None,
        "une pause ne lance rien sur une suiveuse à l'arrêt"
    );
    assert_eq!(
        geste_pour_la_suiveuse(
            &zone(PlayState::Stopped, piste(1)),
            &zone(PlayState::Paused, piste(1))
        ),
        Some(Geste::Arret)
    );
    assert_eq!(
        geste_pour_la_suiveuse(
            &zone(PlayState::Stopped, piste(1)),
            &zone(PlayState::Stopped, piste(1))
        ),
        None
    );
}

#[test]
fn une_meneuse_sans_piste_ne_fait_rien_jouer() {
    assert_eq!(
        geste_pour_la_suiveuse(
            &zone(PlayState::Playing, None),
            &zone(PlayState::Stopped, None)
        ),
        None
    );
}

/// Le relais n'existe que si le démarrage le lance. Le compte se fait sur
/// `background.rs`, où la fonction n'est pas DÉFINIE : la garde ne peut pas
/// être satisfaite par sa propre cible.
#[test]
fn le_demarrage_lance_le_relais() {
    let source = include_str!("../../background.rs");
    let appels = source
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .filter(|l| l.contains("relais_de_groupe_6155::lancer_le_relais_de_groupe(state)"))
        .count();
    assert_eq!(
        appels, 1,
        "spawn_background_tasks doit lancer le relais de groupe une fois (#6155)"
    );
}
