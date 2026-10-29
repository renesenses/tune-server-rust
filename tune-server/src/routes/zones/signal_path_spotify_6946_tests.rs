//! #6946 (Belkadi Yacine, fil 2423) — un titre Spotify est le PCM que
//! librespot a décodé d'un Ogg Vorbis, 320 kbit/s au plus : librespot 0.8.0 ne
//! demande aucun format sans perte à Spotify (voir
//! `tune_core::streaming::spotify_connect::QualiteDeLibrespot`). Le chemin du
//! signal le présentait comme le WAV 44,1 kHz / 16 bits du fil, sans perte et
//! bit-perfect, c'est-à-dire comme un CD. Il dit désormais « Ogg Vorbis
//! 320 kbit/s », avec la raison du plafond.

use super::*;
use std::sync::Arc;
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_core::playback::NowPlaying;

fn zone(output_type: &str, device: &str) -> (Arc<dyn DbBackend>, Zone) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo
        .create("Salon", Some(output_type), Some(device))
        .unwrap();
    (backend.clone(), repo.get(id).unwrap().unwrap())
}

/// La lecture en cours d'un titre Spotify. `avant` : ce que Tune publiait
/// avant #6946 (le conteneur du fil, sans débit) ; sinon ce que
/// `composer_le_now_playing` publie désormais.
fn titre_spotify(avant: bool) -> ZoneState {
    ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Never Gonna Give You Up".into(),
            source: "spotify".into(),
            source_id: Some("4uLU6hMCjMI75M1A2tKUQC".into()),
            format: Some(if avant { "wav" } else { "ogg" }.into()),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            bitrate_kbps: (!avant).then_some(320),
            stream_id: Some("sid-6946".into()),
            ..Default::default()
        }),
        volume: 1.0,
        ..Default::default()
    }
}

/// Le WAV 16 bits que la session sert : le PCM de librespot.
fn fil_wav_16() -> StreamInfo {
    StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 44_100,
        bit_depth: 16,
        channels: 2,
        ..Default::default()
    }
}

fn source(sp: &Value) -> Value {
    sp["steps"]
        .as_array()
        .and_then(|s| s.iter().find(|e| e["name"] == "Source"))
        .cloned()
        .expect("une étape Source")
}

#[test]
fn un_titre_spotify_se_dit_ogg_vorbis_320_avec_la_raison_6946() {
    for avant in [false, true] {
        for (type_sortie, appareil, transport) in [
            ("dlna", "uuid:renderer-6946", "none"),
            ("local", "local:alsa:Casque", "ALSA"),
        ] {
            let (backend, zone) = zone(type_sortie, appareil);
            let sp = build_signal_path_lang(
                "fr",
                &titre_spotify(avant),
                &zone,
                &backend,
                Some("Salon"),
                transport,
                Some(&fil_wav_16()),
            )
            .unwrap();
            let cas = format!("{type_sortie}, lecture d'avant #6946 : {avant}");
            let src = source(&sp);
            assert_eq!(
                src["description"], "Ogg Vorbis 320 kbit/s 44kHz",
                "{cas} : la source est l'Ogg Vorbis reçu par librespot, sans les \
                 16 bits du fil WAV : {sp}"
            );
            assert_eq!(sp["lossless"], false, "{cas} : Spotify est avec perte");
            assert_eq!(
                sp["bit_perfect"], false,
                "{cas} : un Ogg Vorbis décodé n'est pas bit-perfect"
            );
            assert_eq!(src["code"], "librespot_no_lossless", "{cas} : {src}");
            let detail = src["detail"].as_str().unwrap_or_default();
            assert!(
                detail.contains("librespot") && detail.contains("320 kbit/s"),
                "{cas} : l'étape Source dit pourquoi : {detail}"
            );

            let q = &sp["source_quality"];
            assert_eq!(q["format"], "Ogg Vorbis", "{cas} : {sp}");
            assert_eq!(q["bitrate_kbps"], 320, "{cas}");
            assert_eq!(q["ceiling"], true, "{cas} : un plafond, pas une mesure");
            assert_eq!(q["lossless"], false, "{cas}");
            assert_eq!(q["reason_code"], "librespot_no_lossless", "{cas}");
            assert_eq!(q["reason"], src["detail"], "{cas}");
        }
    }
}

/// Le témoin négatif : hors Spotify, aucune clé `source_quality`, et la
/// source d'un CD reste un WAV sans perte.
#[test]
fn hors_spotify_aucune_qualite_de_librespot_6946() {
    let (backend, zone) = zone("dlna", "uuid:renderer-6946");
    let mut ps = titre_spotify(true);
    if let Some(np) = ps.now_playing.as_mut() {
        np.source = "cd".into();
    }
    let sp = build_signal_path_lang(
        "fr",
        &ps,
        &zone,
        &backend,
        Some("Salon"),
        "none",
        Some(&fil_wav_16()),
    )
    .unwrap();
    assert!(sp.get("source_quality").is_none(), "{sp}");
    assert_eq!(sp["lossless"], true, "{sp}");
    assert_ne!(source(&sp)["code"], "librespot_no_lossless", "{sp}");
}
