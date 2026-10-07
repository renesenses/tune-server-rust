//! #4969 (Gros Bidon, fil 1929) — « avoir un bargraphe avec une barre par
//! canal quand un album est en multicanal », pour voir un LFE muet, un centre
//! vide ou un « 7.1 » qui n'est qu'un 5.1.
//!
//! `playback.audio_levels` ne portait que gauche et droite : les canaux 2..N
//! n'étaient jamais mesurés, aucun client ne pouvait tracer cette barre. Banc
//! sur la chaîne réelle (forwarder cadencé → bus) : un flux 5.1 publie six
//! niveaux et leurs noms ; un flux stéréo garde l'évènement d'avant, champ pour
//! champ.

use std::sync::Arc;

use crate::playback::{NowPlaying, PlaybackManager};

/// 100 ms à 48 kHz, 16 bits : un sinus 1 kHz d'amplitude `amplitudes[c]` sur
/// le canal `c`.
fn pcm(amplitudes: &[f64]) -> Vec<u8> {
    let mut out = Vec::new();
    for n in 0..4_800u32 {
        let phase = 2.0 * std::f64::consts::PI * 1_000.0 * f64::from(n) / 48_000.0;
        for a in amplitudes {
            let v = (a * phase.sin() * f64::from(i16::MAX)) as i16;
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out
}

/// Le premier `playback.audio_levels` publié pour ce PCM.
async fn premier_evenement(zone_id: i64, amplitudes: &[f64]) -> serde_json::Value {
    let playback = Arc::new(PlaybackManager::new());
    playback.play(zone_id, NowPlaying::default()).await;
    let bus = Arc::new(super::EventBus::new());
    let mut rx = bus.subscribe();
    let play_seq = playback.current_play_seq(zone_id).await;
    let tx = super::spawn_paced_levels_forwarder(bus.clone(), playback, zone_id, play_seq, 0);
    assert!(crate::audio::tap::send_windowed_pcm(
        &tx,
        &pcm(amplitudes),
        16,
        amplitudes.len() as u16,
        48_000
    ));
    let fin = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let reste = fin.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(reste, rx.recv()).await {
            Ok(Ok(ev)) if ev.event_type == "playback.audio_levels" => return ev.data,
            Ok(Ok(_)) => {}
            Ok(Err(e)) => panic!("bus : {e:?}"),
            Err(_) => panic!("aucun playback.audio_levels publié en 5 s"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_flux_5_1_publie_six_niveaux_et_le_lfe_muet_se_voit() {
    // FL FR FC LFE BL BR : LFE muet, le reste à −6 dBFS.
    let ev = premier_evenement(984_969, &[0.5, 0.5, 0.5, 0.0, 0.5, 0.5]).await;
    assert_eq!(ev["channels"], 6);
    let canaux = ev["channel_levels"].as_array().unwrap_or_else(|| {
        panic!(
            "un flux 5.1 doit publier channel_levels (#4969) ; l'évènement ne \
             porte que gauche et droite : {ev}"
        )
    });
    assert_eq!(canaux.len(), 6);
    assert_eq!(
        canaux[3]["peak_db"].as_f64(),
        Some(-96.0),
        "LFE muet : {}",
        canaux[3]
    );
    for c in [0, 1, 2, 4, 5] {
        let crete = canaux[c]["peak_db"].as_f64().expect("peak_db");
        assert!(
            (crete + 6.02).abs() < 0.1,
            "canal {c} : {crete} dB, attendu −6,02"
        );
        assert_eq!(canaux[c]["over"], false);
    }
    assert_eq!(
        ev["channel_names"],
        serde_json::json!(["FL", "FR", "FC", "LFE", "BL", "BR"])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_flux_stereo_garde_l_evenement_d_avant() {
    let ev = premier_evenement(984_970, &[0.5, 0.5]).await;
    assert_eq!(ev["channels"], 2);
    assert!(
        ev.get("channel_levels").is_none(),
        "stéréo : aucun champ neuf, {ev}"
    );
    assert!(ev.get("channel_names").is_none());
    assert!(ev["peak_left_db"].as_f64().is_some());
}
