//! Fil 1908 (Didier, 24/09/2026, Windows, SMSL SU-8 en USB, Qobuz 44,1 kHz
//! rééchantillonné à 96 kHz) : « décalage temporel de l'analyseur de spectre
//! par rapport à l'audio. L'analyseur étant en gros une à deux secondes en
//! avance sur la sortie audio. »
//!
//! ## La cause, mesurée dans le code
//!
//! * La sortie locale rapporte la position ALIMENTÉE : `total_frames_fed`
//!   divisé par la cadence, stocké à chaque écriture dans l'anneau
//!   (`outputs/local.rs`, `BoucleProducteur::tourner`). Elle ne retranche ce
//!   que l'anneau retient qu'au vidage de fin de piste.
//! * L'anneau tient `taux × canaux × 2` échantillons, soit DEUX secondes
//!   (`outputs/local/backend.rs`, `ring_cap`) ; le producteur le remplit
//!   jusqu'à la butée, il est donc plein en régime établi.
//! * Le forwarder de niveaux se cadence à l'horloge murale et « rattrape »
//!   la position rapportée à une seconde près (`lagging`). Il se calait donc
//!   sur une position qui devance le son de l'anneau entier.
//!
//! ## Ce que ce banc prouve
//!
//! La chaîne réelle (forwarder cadencé → bus) publie une fenêtre quand la
//! sortie la JOUE — position alimentée moins ce que l'anneau retient — et non
//! quand elle arrive. Contre-épreuve : sans l'attente sur l'horloge de sortie,
//! la première phase voit sortir une quinzaine de fenêtres au lieu d'une.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use crate::outputs::traits::RingStarvation;
use crate::playback::{HorlogeDeSortie, NowPlaying, PlaybackManager};

/// Une seconde de 1 kHz à −6 dBFS, 48 kHz stéréo 16 bits : 25 fenêtres de
/// 40 ms tout juste (1 920 trames chacune).
fn une_seconde_de_sinus() -> Vec<u8> {
    let mut pcm = Vec::with_capacity(48_000 * 4);
    for n in 0..48_000u32 {
        let v = (0.5
            * (2.0 * std::f64::consts::PI * 1_000.0 * n as f64 / 48_000.0).sin()
            * i16::MAX as f64) as i16;
        pcm.extend_from_slice(&v.to_le_bytes());
        pcm.extend_from_slice(&v.to_le_bytes());
    }
    pcm
}

/// Les `position_ms` publiées pendant `duree`.
async fn positions_publiees(
    rx: &mut tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
    duree: std::time::Duration,
) -> Vec<i64> {
    let fin = tokio::time::Instant::now() + duree;
    let mut vues = Vec::new();
    loop {
        let reste = fin.saturating_duration_since(tokio::time::Instant::now());
        if reste.is_zero() {
            return vues;
        }
        match tokio::time::timeout(reste, rx.recv()).await {
            Ok(Ok(ev)) if ev.event_type == "playback.audio_levels" => {
                vues.push(ev.data["position_ms"].as_i64().expect("position_ms"));
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => panic!("bus : {e:?}"),
            Err(_) => return vues,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn les_niveaux_sortent_quand_la_sortie_locale_joue_la_fenetre() {
    let zone_id = 987_908;
    let playback = Arc::new(PlaybackManager::new());

    // La sortie a poussé 2 s de piste, et son anneau de 2 s est PLEIN : le
    // pilote n'en a encore rien tiré. On entend donc la position 0.
    let anneau = Arc::new(RingStarvation::new());
    anneau.begin_stream(48_000, 2);
    anneau.noter_alimentation(48_000 * 2 * 2);
    let alimentee = Arc::new(AtomicU64::new(2_000));
    playback.brancher_l_horloge_de_sortie(
        zone_id,
        HorlogeDeSortie {
            position_alimentee_ms: alimentee.clone(),
            anneau: anneau.clone(),
        },
    );
    assert_eq!(playback.position_audible_ms(zone_id), Some(0));

    playback.play(zone_id, NowPlaying::default()).await;
    let bus = Arc::new(super::EventBus::new());
    let mut rx = bus.subscribe();
    let play_seq = playback.current_play_seq(zone_id).await;
    let levels_tx =
        super::spawn_paced_levels_forwarder(bus.clone(), playback.clone(), zone_id, play_seq, 0);
    assert!(crate::audio::tap::send_windowed_pcm(
        &levels_tx,
        &une_seconde_de_sinus(),
        16,
        2,
        48_000
    ));

    // Phase 1 — 600 ms d'horloge murale, le son n'a pas bougé de 0.
    let vues = positions_publiees(&mut rx, std::time::Duration::from_millis(600)).await;
    assert_eq!(
        vues,
        vec![0],
        "le son est à 0 ms, mais le forwarder a publié les fenêtres {vues:?} : \
         cadencé à l'horloge murale, il devance la sortie de tout ce que \
         l'anneau retient — l'analyseur « une à deux secondes en avance » du \
         fil 1908"
    );

    // Phase 2 — le pilote a tiré 400 ms : l'anneau n'en retient plus que 1,6 s.
    anneau.noter_en_attente(48_000 * 2 * 16 / 10);
    assert_eq!(playback.position_audible_ms(zone_id), Some(400));
    let vues = positions_publiees(&mut rx, std::time::Duration::from_millis(600)).await;
    assert_eq!(
        vues,
        (1..=10).map(|k| k * 40).collect::<Vec<i64>>(),
        "le son est à 400 ms : les fenêtres 40 à 400 doivent sortir, et \
         aucune au-delà"
    );

    // Phase 3 — la sortie alimente encore 200 ms, le pilote en tire autant :
    // l'anneau reste à 1,6 s, le son avance à 600 ms.
    alimentee.store(2_200, std::sync::atomic::Ordering::Relaxed);
    let vues = positions_publiees(&mut rx, std::time::Duration::from_millis(600)).await;
    assert_eq!(vues, (11..=15).map(|k| k * 40).collect::<Vec<i64>>());
}

/// Sans horloge branchée — tout rendu réseau —, le cadencement d'avant est
/// intact : les fenêtres sortent au rythme de l'horloge murale.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sans_horloge_de_sortie_le_cadencement_mural_est_inchange() {
    let zone_id = 987_909;
    let playback = Arc::new(PlaybackManager::new());
    assert_eq!(playback.position_audible_ms(zone_id), None);
    playback.play(zone_id, NowPlaying::default()).await;
    let bus = Arc::new(super::EventBus::new());
    let mut rx = bus.subscribe();
    let play_seq = playback.current_play_seq(zone_id).await;
    let levels_tx =
        super::spawn_paced_levels_forwarder(bus.clone(), playback.clone(), zone_id, play_seq, 0);
    crate::audio::tap::send_windowed_pcm(&levels_tx, &une_seconde_de_sinus(), 16, 2, 48_000);
    let vues = positions_publiees(&mut rx, std::time::Duration::from_millis(600)).await;
    assert!(
        vues.len() >= 10,
        "600 ms d'horloge murale ⇒ ~15 fenêtres de 40 ms ; vu {vues:?}"
    );
}

/// Une horloge qui décrit une AUTRE piste (avance gapless : la sortie
/// alimente déjà la suivante) ne doit pas bloquer le forwarder : il la lâche
/// et revient au cadencement mural.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn une_horloge_incoherente_est_lachee_sans_bloquer_les_niveaux() {
    let zone_id = 987_910;
    let playback = Arc::new(PlaybackManager::new());
    let anneau = Arc::new(RingStarvation::new());
    anneau.begin_stream(48_000, 2);
    // Le forwarder démarre à 180 s (reprise) ; l'horloge, elle, est à 0.
    playback.brancher_l_horloge_de_sortie(
        zone_id,
        HorlogeDeSortie {
            position_alimentee_ms: Arc::new(AtomicU64::new(0)),
            anneau,
        },
    );
    playback.play(zone_id, NowPlaying::default()).await;
    let bus = Arc::new(super::EventBus::new());
    let mut rx = bus.subscribe();
    let play_seq = playback.current_play_seq(zone_id).await;
    let levels_tx = super::spawn_paced_levels_forwarder(
        bus.clone(),
        playback.clone(),
        zone_id,
        play_seq,
        180_000,
    );
    crate::audio::tap::send_windowed_pcm(&levels_tx, &une_seconde_de_sinus(), 16, 2, 48_000);
    let vues = positions_publiees(&mut rx, std::time::Duration::from_millis(600)).await;
    assert!(
        vues.len() >= 10 && vues[0] == 180_000,
        "horloge à 0 pour une piste reprise à 180 s : le forwarder devait la \
         lâcher et publier au rythme mural ; vu {vues:?}"
    );
}
