//! #5077 — une zone MASQUÉE qui joue doit rester dans `GET /zones`.
//!
//! Terrain (Stéphane Villerio, Eversolo DMP-A6, 0.9.165, fils 1926 et 1951) :
//! « Les pistes s'enchaînent bien maintenant, mais de nouveau rien dans
//! Lecture en cours ». Sa fiche : `Zones (0)`, et dans le même journal
//! `orchestrator_play zone_id=11`. La zone DLNA 11 a été masquée en
//! 0.9.163/0.9.164 par l'ignorance de l'appareil AirPlay de même IP (#4957) ;
//! elle le reste après la mise à jour. #4970 l'a rendue au sondeur, pas à la
//! liste que lit le client : `GET /zones` passait par `ZoneRepo::list()`, qui
//! écarte toute zone masquée. Le client relit `/zones` à chaque `playback.*` :
//! la zone qu'il regardait disparaissait, et avec elle la piste courante.
//!
//! Le banc : la VRAIE route (`list_zones`), une base neuve, la zone DLNA
//! masquée par `ZoneRepo::delete` (le masquage que pose aussi l'ignorance
//! d'un appareil), et l'état de lecture posé dans le vrai `PlaybackManager`.

use super::*;
use tune_core::playback::NowPlaying;

const APPAREIL: &str = "dlna:uuid:3E151150-D9C0-11F0-A7C6-800A805C2689";

/// Une base neuve, une zone DLNA « DMP-A6 », masquée.
fn etat_avec_zone_masquee() -> (AppState, i64) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let depot = ZoneRepo::with_backend(state.backend.clone());
    let zone_id = depot
        .create("DMP-A6", Some("dlna"), Some(APPAREIL))
        .unwrap();
    depot.delete(zone_id).unwrap();
    assert!(
        !depot.list().unwrap().iter().any(|z| z.id == Some(zone_id)),
        "prémisse : la zone masquée n'est plus dans `list()`"
    );
    (state, zone_id)
}

fn piste() -> NowPlaying {
    NowPlaying {
        title: "Liberty".into(),
        source: "local".into(),
        duration_ms: 240_000,
        ..Default::default()
    }
}

/// La zone de la route dont l'identifiant est `zone_id`, s'il y en a une.
fn zone_de(liste: &Value, zone_id: i64) -> Option<Value> {
    liste
        .as_array()
        .expect("GET /zones rend un tableau")
        .iter()
        .find(|z| z["id"].as_i64() == Some(zone_id))
        .cloned()
}

/// 🔴 Le symptôme du fil 1951 : la zone joue, la route doit la rendre, avec
/// sa piste courante.
#[tokio::test]
async fn une_zone_masquee_qui_joue_reste_dans_la_liste_des_zones() {
    let (state, zone_id) = etat_avec_zone_masquee();
    state.playback.play(zone_id, piste()).await;

    let Json(liste) = list_zones(State(state.clone())).await;
    let zone = zone_de(&liste, zone_id).unwrap_or_else(|| {
        panic!(
            "la zone {zone_id} joue mais `GET /zones` ne la rend pas : le client \
             perd la zone qu'il regarde et affiche « Aucune lecture en cours » \
             (#5077) ; liste = {liste}"
        )
    });
    assert_eq!(zone["state"], "playing", "{zone}");
    assert_eq!(zone["current_track"]["title"], "Liberty", "{zone}");
}

/// En pause, on l'écoute encore : elle reste montrée.
#[tokio::test]
async fn une_zone_masquee_en_pause_reste_dans_la_liste_des_zones() {
    let (state, zone_id) = etat_avec_zone_masquee();
    state.playback.play(zone_id, piste()).await;
    state.playback.pause(zone_id).await;

    let Json(liste) = list_zones(State(state.clone())).await;
    let zone = zone_de(&liste, zone_id).expect("zone masquée en pause absente de /zones");
    assert_eq!(zone["state"], "paused", "{zone}");
}

/// Témoin de non-régression : arrêtée, une zone masquée redevient invisible —
/// le masquage garde son sens (zone supprimée, appareil ignoré).
#[tokio::test]
async fn une_zone_masquee_a_l_arret_reste_invisible() {
    let (state, zone_id) = etat_avec_zone_masquee();

    let Json(liste) = list_zones(State(state.clone())).await;
    assert!(
        zone_de(&liste, zone_id).is_none(),
        "une zone masquée qui ne joue pas ne doit pas réapparaître : {liste}"
    );

    state.playback.play(zone_id, piste()).await;
    state.playback.stop(zone_id).await;
    let Json(liste) = list_zones(State(state.clone())).await;
    assert!(
        zone_de(&liste, zone_id).is_none(),
        "arrêtée, la zone masquée doit redevenir invisible : {liste}"
    );
}

/// Une zone visible qui joue n'est pas rendue deux fois.
#[tokio::test]
async fn une_zone_visible_qui_joue_n_est_pas_doublee() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zone_id = ZoneRepo::with_backend(state.backend.clone())
        .create("DMP-A6", Some("dlna"), Some(APPAREIL))
        .unwrap();
    state.playback.play(zone_id, piste()).await;

    let Json(liste) = list_zones(State(state.clone())).await;
    let n = liste
        .as_array()
        .unwrap()
        .iter()
        .filter(|z| z["id"].as_i64() == Some(zone_id))
        .count();
    assert_eq!(n, 1, "{liste}");
}
