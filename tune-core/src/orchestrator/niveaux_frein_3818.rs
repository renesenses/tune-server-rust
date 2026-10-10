//! #3818 (Pierre M, fil 2181, 08/10/2026) — « le vu-mètre s'arrête souvent
//! à 31 et 32 secondes après le début du titre, reprend au morceau suivant ;
//! idem en local (NAS) ou par Qobuz ».
//!
//! Le décodage-pour-niveaux est FREINÉ : il ne prend pas plus de
//! `PROXY_LEVELS_MAX_AHEAD_MS` (30 s) d'avance. Jusqu'ici l'avance se
//! mesurait contre la seule position RAPPORTÉE par la zone. Quand cette
//! position ne progresse pas alors que la zone joue (renderer qui ne publie
//! pas sa position, échantillons écartés par le sondeur…), le décodeur
//! s'arrête à 30 s de PCM — plus le canal borné du puits, 31,9 s mesurées en
//! 44,1/16 par `pcm_retenu_passthrough` — pendant que le forwarder, cadencé
//! sur l'horloge murale, continue de publier au temps réel. À 31-32 s, sa
//! file est vide : plus une trame, aiguilles et spectre à plat, jusqu'au
//! forwarder neuf de la piste suivante.
//!
//! Le banc joue la vraie chaîne de production (`spawn_local_file_levels_decode`
//! : forwarder cadencé + puits freiné + décodeur en flux) sur une zone en
//! lecture dont la position reste à 0, et attend une trame au-delà de 36 s.

use std::sync::Arc;

use crate::playback::{NowPlaying, PlaybackManager};

/// WAV 44,1 kHz / 16 bits / stéréo, carré pleine échelle à −2 dB.
fn ecrire_wav(path: &std::path::Path, duree_ms: u64) {
    const SR: u32 = 44_100;
    let trames = (SR as u64 * duree_ms / 1000) as usize;
    let mut buf = Vec::with_capacity(44 + trames * 4);
    buf.extend_from_slice(&crate::audio::wav::build_wav_header_with_duration(
        2,
        SR,
        16,
        Some(duree_ms),
    ));
    let demi_periode = (SR / 86) as usize;
    for t in 0..trames {
        let v: i16 = if (t / demi_periode).is_multiple_of(2) {
            26_000
        } else {
            -26_000
        };
        buf.extend_from_slice(&v.to_le_bytes());
        buf.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, &buf).unwrap();
}

/// Position (`position_ms`) de la dernière trame publiée pour la zone, en
/// s'arrêtant dès que `cible_ms` est atteinte ou qu'aucune trame n'arrive
/// pendant `silence`.
async fn derniere_position_publiee(
    rx: &mut tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
    zone_id: i64,
    cible_ms: i64,
    silence: std::time::Duration,
) -> i64 {
    let mut derniere = -1;
    loop {
        match tokio::time::timeout(silence, rx.recv()).await {
            Ok(Ok(ev))
                if ev.event_type == "playback.audio_levels"
                    && ev.data.get("zone_id").and_then(|v| v.as_i64()) == Some(zone_id) =>
            {
                derniere = ev
                    .data
                    .get("position_ms")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(derniere);
                if derniere >= cible_ms {
                    return derniere;
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
            _ => return derniere,
        }
    }
}

/// Le témoin : zone EN LECTURE, position rapportée figée à 0, piste de 45 s.
/// Avant le correctif, la dernière trame décrit ~31 s et plus rien ne vient.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn les_niveaux_ne_s_arretent_pas_a_31_s_quand_la_position_rapportee_reste_figee() {
    let fichier = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
    ecrire_wav(fichier.path(), 45_000);
    let chemin = fichier.path().to_str().unwrap().to_string();

    let zone_id = 3818;
    let playback = Arc::new(PlaybackManager::new());
    playback.play(zone_id, NowPlaying::default()).await;
    let bus = Arc::new(super::EventBus::new());
    let mut rx = bus.subscribe();
    let play_seq = playback.current_play_seq(zone_id).await;

    super::spawn_local_file_levels_decode(bus, playback.clone(), zone_id, play_seq, chemin);

    let derniere =
        derniere_position_publiee(&mut rx, zone_id, 36_000, std::time::Duration::from_secs(4))
            .await;

    // Le décor tient : personne n'a fait avancer la position de la zone.
    assert_eq!(
        playback.get_state(zone_id).await.position_ms,
        0,
        "le banc doit garder la position rapportée figée à 0"
    );
    assert!(
        derniere >= 36_000,
        "#3818 — les niveaux se sont arrêtés à {derniere} ms de piste alors que la zone \
         joue toujours : le décodage-pour-niveaux, freiné contre une position rapportée \
         figée, a cessé d'alimenter le forwarder"
    );
}

/// Le frein garde son rôle : sans consommateur (forwarder qui ne publie
/// rien), l'avance reste plafonnée à ~30 s même avec la consommation en jeu.
#[test]
fn la_reference_du_frein_est_la_plus_avancee_des_deux_horloges() {
    assert_eq!(super::reference_du_frein(0, 0), 0);
    assert_eq!(super::reference_du_frein(0, 12_000), 12_000);
    assert_eq!(super::reference_du_frein(50_000, 12_000), 50_000);
    assert!(!super::levels_decode_doit_freiner(
        42_000,
        super::reference_du_frein(0, 12_000)
    ));
    assert!(super::levels_decode_doit_freiner(
        42_001,
        super::reference_du_frein(0, 12_000)
    ));
}
