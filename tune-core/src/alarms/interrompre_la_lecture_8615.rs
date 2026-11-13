//! #8615 (fil 2784, laguespa) — l'option de réveil « Interrompre la lecture
//! en cours », cochée par défaut (décision de Bertrand du 13/11).
//!
//! Banc de #7058 : deux vraies pistes WAV sur une sortie factice, aucun son.
//! - zone en lecture + option cochée : le réveil programmé remplace la
//!   lecture, au volume du réveil ;
//! - zone en lecture + option décochée : la zone garde sa musique (#5669) ;
//! - zone à l'arrêt : le réveil sonne, quelle que soit l'option ;
//! - le bouton d'essai ne coupe jamais une lecture (#5669) ;
//! - un réveil sans l'option (d'avant la migration 151) l'a cochée.

use super::contenus_album_playlist_7058::Banc;
use super::*;
use crate::playback::PlayState;

/// La zone joue la playlist du banc (pistes 2 puis 1) ; renvoie sa génération.
async fn zone_en_lecture(b: &Banc, planificateur: &AlarmScheduler) -> u64 {
    let lance = planificateur
        .fire_alarm(&b.alarme("playlist", b.playlist))
        .await;
    assert_eq!(lance.sonnees, [b.zone]);
    let etat = b.orch.playback.get_state(b.zone).await;
    assert_eq!(etat.state, PlayState::Playing);
    assert_eq!(etat.now_playing.unwrap().track_id, Some(b.pistes[0]));
    etat.track_generation
}

/// Le réveil « album » du banc (première piste : `pistes[1]`), volume 0,5,
/// sans fondu, avec l'option posée ou absente.
fn reveil_album(b: &Banc, interrompt: Option<bool>) -> serde_json::Value {
    let mut reveil = b.alarme("album", 1);
    if let Some(v) = interrompt {
        reveil["interrupt_playback"] = serde_json::json!(v as i64);
    }
    reveil
}

#[tokio::test]
async fn option_cochee_le_reveil_programme_interrompt_la_lecture_8615() {
    let b = Banc::nouveau().await;
    let planificateur = AlarmScheduler::with_backend(b.db.clone(), b.orch.clone());
    let avant = zone_en_lecture(&b, &planificateur).await;

    let zones = planificateur
        .sonner(&reveil_album(&b, Some(true)), false)
        .await;

    assert_eq!(zones.sonnees, [b.zone], "#8615 : le réveil doit sonner");
    assert!(zones.sautees.is_empty());
    let etat = b.orch.playback.get_state(b.zone).await;
    assert_ne!(
        etat.track_generation, avant,
        "#8615 : option cochée, la lecture en cours doit être remplacée"
    );
    assert_eq!(etat.state, PlayState::Playing);
    assert_eq!(
        etat.now_playing.unwrap().track_id,
        Some(b.pistes[1]),
        "#8615 : la zone doit jouer la source du réveil"
    );
    // Fondu nul : le volume du réveil est posé tout de suite (tâche détachée).
    for _ in 0..50 {
        if (b.orch.playback.get_state(b.zone).await.volume - 0.5).abs() < 1e-6 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        (b.orch.playback.get_state(b.zone).await.volume - 0.5).abs() < 1e-6,
        "#8615 : le réveil prend la main au volume du réveil"
    );
}

#[tokio::test]
async fn option_decochee_la_zone_garde_sa_musique_8615() {
    let b = Banc::nouveau().await;
    let planificateur = AlarmScheduler::with_backend(b.db.clone(), b.orch.clone());
    let avant = zone_en_lecture(&b, &planificateur).await;

    let zones = planificateur
        .sonner(&reveil_album(&b, Some(false)), false)
        .await;

    assert_eq!(
        zones.sautees,
        [b.zone],
        "#5669 : option décochée, réveil sauté"
    );
    assert!(zones.sonnees.is_empty());
    let etat = b.orch.playback.get_state(b.zone).await;
    assert_eq!(
        etat.track_generation, avant,
        "la lecture en cours n'est pas touchée"
    );
    assert_eq!(etat.now_playing.unwrap().track_id, Some(b.pistes[0]));
}

#[tokio::test]
async fn zone_a_l_arret_le_reveil_sonne_quelle_que_soit_l_option_8615() {
    for option in [Some(true), Some(false), None] {
        let b = Banc::nouveau().await;
        let planificateur = AlarmScheduler::with_backend(b.db.clone(), b.orch.clone());
        assert_ne!(
            b.orch.playback.get_state(b.zone).await.state,
            PlayState::Playing
        );

        let zones = planificateur.sonner(&reveil_album(&b, option), false).await;

        assert_eq!(zones.sonnees, [b.zone], "option {option:?}");
        let etat = b.orch.playback.get_state(b.zone).await;
        assert_eq!(etat.state, PlayState::Playing, "option {option:?}");
        assert_eq!(etat.now_playing.unwrap().track_id, Some(b.pistes[1]));
    }
}

#[tokio::test]
async fn l_essai_ne_coupe_jamais_une_lecture_8615() {
    let b = Banc::nouveau().await;
    let planificateur = AlarmScheduler::with_backend(b.db.clone(), b.orch.clone());
    let avant = zone_en_lecture(&b, &planificateur).await;

    let zones = planificateur
        .fire_alarm(&reveil_album(&b, Some(true)))
        .await;

    assert_eq!(zones.sautees, [b.zone]);
    assert_eq!(
        b.orch.playback.get_state(b.zone).await.track_generation,
        avant
    );
}

#[tokio::test]
async fn un_reveil_existant_a_l_option_cochee_apres_la_migration_8615() {
    let b = Banc::nouveau().await;
    // Ligne écrite sans la colonne : la valeur par défaut de la migration 151.
    b.db.execute(
        "INSERT INTO alarms (id, name, time, zone_id, source_type, source_id, volume, enabled) \
         VALUES (8615, 'Ancien', '07:00', ?, 'album', '1', 0.5, '1')",
        &[&b.zone as &dyn ToSqlValue],
    )
    .unwrap();
    let planificateur = AlarmScheduler::with_backend(b.db.clone(), b.orch.clone());
    let reveil = planificateur.get_alarm(8615).unwrap().unwrap();
    assert_eq!(reveil["interrupt_playback"].as_i64(), Some(1));
    assert!(interrompt_la_lecture(&reveil));
    // Et un JSON sans la clé (client ancien) : cochée aussi.
    assert!(interrompt_la_lecture(&serde_json::json!({})));
    assert!(!interrompt_la_lecture(
        &serde_json::json!({"interrupt_playback": 0})
    ));
    assert!(!interrompt_la_lecture(
        &serde_json::json!({"interrupt_playback": false})
    ));
}
