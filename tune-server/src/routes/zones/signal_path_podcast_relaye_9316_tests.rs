//! #9316 — forum (relevé du 21/11) : un épisode de podcast relayé vers un
//! renderer réseau s'affichait « AAC 44kHz/16bit », alors que le DAC recevait
//! du 48 kHz.
//!
//! `play_episode` relaie l'épisode par `relayer_un_flux_distant(…, None,
//! None)` : ni le flux RSS ni la requête ne disent sa fréquence. La session
//! remplaçait ces `None` par 44 100 Hz / 16 bits, et le chemin du signal lit
//! le format du fil comme dernier recours : la valeur inventée s'affichait
//! avec l'aplomb d'une mesure.
//!
//! Le banc : la VRAIE session du mandataire, créée par l'orchestrateur, et le
//! VRAI chemin du signal d'une zone DLNA qui la joue.
//!
//! Contre-épreuve : remettre `sample_rate.unwrap_or(44_100)` et
//! `bit_depth.unwrap_or(16)` dans `relayer_un_flux_distant` fait tomber
//! `un_podcast_relaye_sans_format_connu_n_affiche_pas_44_1_16_9316`.

use super::*;
use crate::state::AppState;
use tune_core::playback::NowPlaying;

const EPISODE: &str = "http://127.0.0.1:9/episode-9316.m4a";

async fn chemin_du_podcast_relaye(
    sample_rate: Option<u32>,
    bit_depth: Option<u16>,
) -> (StreamInfo, Value) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let (_, session) = state
        .orchestrator
        .relayer_un_flux_distant(
            EPISODE,
            "audio/aac",
            Some(1_800_000),
            sample_rate,
            bit_depth,
        )
        .await;
    let fil = state
        .orchestrator
        .streamer
        .stream_output_wire(&session)
        .await
        .expect("la session du mandataire existe");
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let id = repo
        .create("Salon 9316", Some("dlna"), Some("uuid:renderer-9316"))
        .unwrap();
    let zone = repo.get(id).unwrap().unwrap();
    let ps = ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Concert du soir".into(),
            source: "podcast".into(),
            source_id: Some(EPISODE.into()),
            format: Some("aac".into()),
            stream_id: Some(session),
            duration_ms: 1_800_000,
            ..Default::default()
        }),
        volume: 1.0,
        ..Default::default()
    };
    let chemin = build_signal_path_pub(
        &ps,
        &zone,
        &state.backend,
        Some("Renderer 9316"),
        "",
        Some(&fil),
    )
    .expect("une zone en lecture a un chemin du signal");
    (fil, chemin)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_podcast_relaye_sans_format_connu_n_affiche_pas_44_1_16_9316() {
    let (fil, chemin) = chemin_du_podcast_relaye(None, None).await;
    assert_eq!(
        (fil.sample_rate, fil.bit_depth),
        (0, 0),
        "#9316 : la session relayée n'invente ni fréquence ni résolution"
    );
    let texte = chemin.to_string();
    assert!(
        !texte.contains("44kHz") && !texte.contains("16bit"),
        "#9316 : aucun 44,1 kHz / 16 bits inventé dans le chemin du signal : {chemin}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_podcast_relaye_au_format_connu_l_affiche_9316() {
    let (fil, chemin) = chemin_du_podcast_relaye(Some(48_000), Some(24)).await;
    assert_eq!((fil.sample_rate, fil.bit_depth), (48_000, 24));
    let texte = chemin.to_string();
    assert!(
        texte.contains("48kHz"),
        "#9316 : une fréquence connue s'affiche : {chemin}"
    );
    assert!(!texte.contains("44kHz"), "{chemin}");
}
