//! #8058 — sur une sortie locale, curseur de volume sous ~2,5 % (`db60`) :
//! le son sort, très faible, et l'indicateur de niveau tombait au plancher.
//!
//! Depuis #7968, la sortie range son gain de rendu au millionième ; le
//! forwarder de niveaux le relisait en millièmes, arrondis au plus proche :
//! tout gain sous 0,0005 valait 0, et la crête publiée −96 dBFS.
//!
//! Banc sur la chaîne réelle (forwarder cadencé → bus), comme
//! `crete_apres_dsp_4384` : la sortie locale est représentée par ce qu'elle
//! partage avec le `PlaybackManager` (gain de rendu, gain demandé, crêtes
//! relevées après son DSP). Aucun son.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32};

use crate::audio::crete_de_sortie::CretesDeSortie;
use crate::audio::volume_scale::{UNITE_DU_GAIN_DE_RENDU, gain_perceptif};
use crate::playback::{GainDemande, NowPlaying, PlaybackManager};

const CADENCE: u32 = 44_100;
const AMPLITUDE: f64 = 0.9;
const TRAMES: u32 = 4_410;
const PLANCHER_DB: f64 = -96.0;

fn sinus() -> Vec<f64> {
    (0..TRAMES)
        .map(|n| {
            AMPLITUDE
                * (2.0 * std::f64::consts::PI * 1_000.0 * f64::from(n) / f64::from(CADENCE)).sin()
        })
        .collect()
}

fn pcm_source() -> Vec<u8> {
    let mut pcm = Vec::with_capacity(TRAMES as usize * 4);
    for s in sinus() {
        let v = (s * 32767.0) as i16;
        pcm.extend_from_slice(&v.to_le_bytes());
        pcm.extend_from_slice(&v.to_le_bytes());
    }
    pcm
}

/// Le gain de rendu que la sortie range pour une position `db60` du curseur.
fn unites_du_curseur(position: f64) -> u32 {
    (gain_perceptif(position) * f64::from(UNITE_DU_GAIN_DE_RENDU)).round() as u32
}

/// La première fenêtre `playback.audio_levels` publiée, curseur à `position`.
async fn niveaux(zone_id: i64, position: f64) -> serde_json::Value {
    let playback = Arc::new(PlaybackManager::new());
    let unites = unites_du_curseur(position);
    playback.brancher_le_gain_de_sortie(zone_id, Arc::new(AtomicU32::new(unites)));
    playback.brancher_le_gain_demande(
        zone_id,
        GainDemande {
            volume_utilisateur: Arc::new(AtomicU32::new(unites)),
            facteur_de_rendu: Arc::new(AtomicU32::new(1000)),
            dop: Arc::new(AtomicBool::new(false)),
        },
    );
    let cretes = Arc::new(CretesDeSortie::new());
    let apres_dsp: Vec<f32> = sinus()
        .into_iter()
        .flat_map(|s| [s as f32, s as f32])
        .collect();
    cretes.relever(&apres_dsp, u64::from(TRAMES), CADENCE, 0.0);
    playback.brancher_les_cretes_de_sortie(zone_id, cretes);
    playback.play(zone_id, NowPlaying::default()).await;
    let bus = Arc::new(super::EventBus::new());
    let mut rx = bus.subscribe();
    let play_seq = playback.current_play_seq(zone_id).await;
    let levels_tx =
        super::spawn_paced_levels_forwarder(bus.clone(), playback.clone(), zone_id, play_seq, 0);
    crate::audio::tap::send_windowed_pcm(&levels_tx, &pcm_source(), 16, 2, CADENCE);
    let fin = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let reste = fin.saturating_duration_since(tokio::time::Instant::now());
        assert!(!reste.is_zero(), "aucun playback.audio_levels publié");
        match tokio::time::timeout(reste, rx.recv()).await {
            Ok(Ok(ev)) if ev.event_type == "playback.audio_levels" => return ev.data,
            Ok(Ok(_)) => {}
            autre => panic!("bus muet : {autre:?}"),
        }
    }
}

/// Curseur de 1 à 10 % : la crête publiée est celle du son rendu
/// (crête × gain), jamais le plancher ; le gain annoncé suit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn en_bas_de_course_l_aiguille_suit_le_son_rendu_8058() {
    for (i, pourcent) in (1..=10).enumerate() {
        let position = f64::from(pourcent) / 100.0;
        let gain = f64::from(unites_du_curseur(position)) / f64::from(UNITE_DU_GAIN_DE_RENDU);
        assert!(gain > 0.0, "{pourcent} % : le son sort");
        let attendu = (20.0 * (AMPLITUDE * gain).log10()).max(PLANCHER_DB);
        let v = niveaux(988_058 + i as i64, position).await;
        let crete = v["peak_left_db"].as_f64().expect("peak_left_db");
        assert!(
            crete > PLANCHER_DB,
            "curseur à {pourcent} % (gain {gain:.6}) : le son sort, l'aiguille \
             ne doit pas tomber au plancher (#8058), publié {crete:.2} dBFS"
        );
        assert!(
            (crete - attendu).abs() < 0.5,
            "curseur à {pourcent} % : crête rendue {attendu:.2} dBFS attendue, publiée {crete:.2}"
        );
        let annonce = v["output_gain_db"].as_f64().expect("output_gain_db");
        assert!(
            (annonce - 20.0 * gain.log10()).abs() < 0.5,
            "curseur à {pourcent} % : gain annoncé {annonce:.2} dB, rendu {:.2} dB",
            20.0 * gain.log10()
        );
        let demande = v["output_gain_requested_db"]
            .as_f64()
            .expect("output_gain_requested_db");
        assert!(
            (demande - 20.0 * gain.log10()).abs() < 0.5,
            "curseur à {pourcent} % : gain demandé {demande:.2} dB"
        );
        assert_eq!(v["output_gain_limited"], false);
    }
}

/// Témoin : curseur à 0, la sortie rend le silence et l'aiguille le dit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn curseur_a_zero_l_aiguille_montre_le_silence_8058() {
    let v = niveaux(988_000, 0.0).await;
    let crete = v["peak_left_db"].as_f64().expect("peak_left_db");
    assert!(
        crete <= PLANCHER_DB + 0.01,
        "curseur à 0 : silence, publié {crete:.2}"
    );
}
