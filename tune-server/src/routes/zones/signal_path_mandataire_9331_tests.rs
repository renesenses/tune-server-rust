//! #9331 — un épisode de podcast relayé TEL QUEL par le mandataire vers une
//! zone DLNA dont l'égaliseur est armé : le chemin du signal affichait
//! « DSP — EQ actif » alors qu'aucun étage ne décode ces octets
//! (`relayer_un_flux_distant`, #6468).
//!
//! Le témoin passe par la VRAIE session du mandataire (celle que
//! `play_episode` crée), lue comme les routes la lisent
//! (`stream_output_wire`), puis par le constructeur public du chemin.

use super::*;
use crate::state::AppState;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::playback::NowPlaying;

const EPISODE: &str = "https://cdn.exemple.org/podcast/episode-9331.mp3";

/// Un égaliseur ARMÉ sur la zone, écrit là où le chemin audio le lit
/// (même geste que `signal_path_tests::armer_l_eq`).
fn armer_l_eq(state: &AppState, zone_id: i64) {
    let profil = tune_core::audio::eq::EqProfile {
        enabled: true,
        bands: vec![tune_core::audio::eq::EqBandSpec {
            gain: 6.0,
            ..Default::default()
        }],
        ..Default::default()
    };
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    reglages
        .set(
            &format!("zone_{zone_id}_eq_profile"),
            &serde_json::to_string(&profil).unwrap(),
        )
        .unwrap();
    reglages.set("plugin_equalizer_installed", "true").unwrap();
}

fn zone_dlna(state: &AppState) -> Zone {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let id = repo
        .create("Salon 9331", Some("dlna"), Some("uuid:renderer-9331"))
        .unwrap();
    repo.get(id).unwrap().unwrap()
}

fn en_lecture(source: &str, format: &str, stream_id: String) -> ZoneState {
    ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Épisode 9331".into(),
            source: source.into(),
            source_id: Some(EPISODE.into()),
            format: Some(format.into()),
            stream_id: Some(stream_id),
            duration_ms: 1_800_000,
            ..Default::default()
        }),
        volume: 1.0,
        ..Default::default()
    }
}

fn etape<'a>(chemin: &'a Value, nom: &str) -> Option<&'a Value> {
    chemin["steps"]
        .as_array()
        .and_then(|etapes| etapes.iter().find(|e| e["name"] == nom))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_podcast_relaye_tel_quel_n_affiche_aucun_egaliseur_9331() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zone = zone_dlna(&state);
    armer_l_eq(&state, zone.id.unwrap());
    let (_, session) = state
        .orchestrator
        .relayer_un_flux_distant(EPISODE, "audio/mpeg", Some(1_800_000), None, None)
        .await;
    let fil = state
        .orchestrator
        .streamer
        .stream_output_wire(&session)
        .await
        .expect("la session du mandataire existe");

    let chemin = build_signal_path_pub(
        &en_lecture("podcast", "mp3", session),
        &zone,
        &state.backend,
        Some("Renderer 9331"),
        "",
        Some(&fil),
    )
    .expect("une zone en lecture a un chemin du signal");

    assert!(
        etape(&chemin, "DSP").is_none(),
        "#9331 : aucun égaliseur n'agit sur la voie mandataire : {chemin}"
    );
    assert!(
        !chemin.to_string().contains("EQ actif"),
        "#9331 : « EQ actif » sur un flux relayé tel quel : {chemin}"
    );
}

/// CONTRE-ÉPREUVE permanente : même zone, même égaliseur armé, mais un flux
/// que Tune construit (session ordinaire, sans relais). L'égaliseur y est
/// appliqué, l'étape DSP doit rester. Sans elle, une garde trop large (« plus
/// jamais d'EQ en DLNA ») laisserait le témoin vert.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_meme_egaliseur_sur_un_flux_construit_par_tune_reste_affiche_9331() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zone = zone_dlna(&state);
    armer_l_eq(&state, zone.id.unwrap());
    let fil = tune_core::http::streamer::StreamInfo {
        format: "flac".into(),
        mime_type: "audio/flac".into(),
        sample_rate: 44_100,
        bit_depth: 16,
        channels: 2,
        ..Default::default()
    };

    let chemin = build_signal_path_pub(
        &en_lecture("local", "flac", "sid-9331".into()),
        &zone,
        &state.backend,
        Some("Renderer 9331"),
        "",
        Some(&fil),
    )
    .expect("une zone en lecture a un chemin du signal");

    let dsp = etape(&chemin, "DSP").expect("l'égaliseur appliqué reste décrit");
    assert!(
        dsp["description"]
            .as_str()
            .is_some_and(|d| d.starts_with("EQ actif")),
        "{chemin}"
    );
}
