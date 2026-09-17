//! #4237: the actual detector must follow work, not a clamped audio clock.
use super::*;
use std::time::Duration;

const SEUIL: Duration = Duration::from_secs(600);

async fn en_transfert() -> PlaybackManager {
    let pm = PlaybackManager::new();
    pm.play(
        102,
        NowPlaying {
            source: "local".into(),
            duration_ms: 579_000,
            ..Default::default()
        },
    )
    .await;
    let mut zones = pm.zones.lock().await;
    let state = zones.get_mut(&102).unwrap();
    state.position_ms = 578_999;
    state.derniere_avance_de_position = Instant::now().checked_sub(Duration::from_secs(643));
    state.progression_hors_temps_reel = Some((1_000, Instant::now() - Duration::from_secs(643)));
    drop(zones);
    pm
}

async fn observe(pm: &PlaybackManager, bytes: Option<u64>) {
    let generation = pm.get_state(102).await.track_generation;
    pm.observe_processing_progress(102, generation, false, bytes)
        .await;
}

#[tokio::test]
async fn les_octets_avancent_le_detecteur_garde_la_zone_en_lecture() {
    let pm = en_transfert().await;
    observe(&pm, Some(2_000)).await;
    assert!(pm.arreter_les_zones_figees(SEUIL).await.is_empty());
    let state = pm.get_state(102).await;
    assert_eq!(state.state, PlayState::Playing);
    assert_eq!(state.position_ms, 578_999);
    assert!(state.derniere_avance_de_position.unwrap().elapsed() >= SEUIL);
}

#[tokio::test]
async fn un_compteur_fixe_absent_ou_reinitialise_ne_cache_pas_un_blocage() {
    for bytes in [Some(1_000), None, Some(0)] {
        let pm = en_transfert().await;
        observe(&pm, bytes).await;
        // A synthetic clock can still advance while the byte transfer stalls.
        pm.update_position(102, 579_000).await;
        assert_eq!(
            pm.arreter_les_zones_figees(SEUIL).await,
            vec![102],
            "{bytes:?}"
        );
        assert_eq!(pm.get_state(102).await.state, PlayState::Stopped);
        assert!(
            pm.get_state(102)
                .await
                .progression_hors_temps_reel
                .is_none()
        );
    }
}

#[tokio::test]
async fn attendre_les_premiers_octets_ouvre_une_seule_fenetre() {
    let pm = en_transfert().await;
    pm.zones
        .lock()
        .await
        .get_mut(&102)
        .unwrap()
        .progression_hors_temps_reel = None;
    observe(&pm, Some(0)).await;
    assert!(pm.arreter_les_zones_figees(SEUIL).await.is_empty());
    pm.zones
        .lock()
        .await
        .get_mut(&102)
        .unwrap()
        .progression_hors_temps_reel
        .as_mut()
        .unwrap()
        .1 = Instant::now() - Duration::from_secs(601);
    observe(&pm, Some(0)).await;
    assert_eq!(pm.arreter_les_zones_figees(SEUIL).await, vec![102]);
}

#[tokio::test]
async fn une_sortie_temps_reel_reste_soumise_a_sa_position() {
    let pm = en_transfert().await;
    let generation = pm.get_state(102).await.track_generation;
    pm.observe_processing_progress(102, generation, true, Some(2_000))
        .await;
    assert_eq!(pm.arreter_les_zones_figees(SEUIL).await, vec![102]);
}

#[tokio::test]
async fn les_commandes_ne_reutilisent_pas_la_progression_du_flux_precedent() {
    for action in ["play", "resume", "seek", "stop", "clear"] {
        let pm = en_transfert().await;
        match action {
            "play" => {
                pm.play(
                    102,
                    NowPlaying {
                        title: "Autre piste".into(),
                        ..Default::default()
                    },
                )
                .await
            }
            "resume" => {
                pm.pause(102).await;
                pm.resume(102).await;
            }
            "seek" => pm.seek(102, 100).await,
            "stop" => pm.stop(102).await,
            "clear" => pm.stop_and_clear(102).await,
            _ => unreachable!(),
        }
        assert!(
            pm.get_state(102)
                .await
                .progression_hors_temps_reel
                .is_none(),
            "{action}"
        );
    }
}

#[tokio::test]
async fn une_sonde_de_la_piste_precedente_ne_recree_pas_de_progression() {
    let pm = en_transfert().await;
    let generation = pm.get_state(102).await.track_generation;
    pm.play(
        102,
        NowPlaying {
            title: "Autre piste".into(),
            ..Default::default()
        },
    )
    .await;
    pm.observe_processing_progress(102, generation, false, Some(2_000))
        .await;
    assert!(
        pm.get_state(102)
            .await
            .progression_hors_temps_reel
            .is_none()
    );
}
