//! #6334 (pissa7, fil 2265, rc4) — un AAC 320 de Tidal, décodé en WAV
//! 16 bits pour le renderer, s'affichait **WAV · 44.1 kHz · 16-bit**, avec
//! un badge **CD** et **Bit-Perfect**. Le `NowPlaying` nomme désormais le
//! codec de la source (`aac`), sans profondeur, avec son débit (voir
//! `tune-core`, `tidal_aac_annonce_6334_tests`). Ici, le chemin du signal
//! qui le lit : la source se dit « AAC 320 kbit/s 44kHz ». Elle n'emprunte
//! pas les 16 bits du fil WAV, et rien n'y est sans perte.

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
        .create("Renderer", Some(output_type), Some(device))
        .unwrap();
    (backend.clone(), repo.get(id).unwrap().unwrap())
}

/// Ce que `composer_le_now_playing` publie pour « Keith Don't Go ».
fn aac_tidal() -> ZoneState {
    ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Keith Don't Go".into(),
            source: "tidal".into(),
            source_id: Some("29113109".into()),
            format: Some("aac".into()),
            sample_rate: Some(44_100),
            bit_depth: None,
            bitrate_kbps: Some(320),
            stream_id: Some("sid-6334".into()),
            ..Default::default()
        }),
        volume: 1.0,
        ..Default::default()
    }
}

/// Le WAV 16 bits que la session sert au renderer.
fn fil_wav_16() -> StreamInfo {
    StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 44_100,
        bit_depth: 16,
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
fn un_aac_tidal_servi_en_wav_se_dit_aac_320_sans_profondeur_6334() {
    for (type_sortie, appareil, transport) in [
        ("dlna", "uuid:renderer-6334", "none"),
        ("local", "local:alsa:Casque", "ALSA"),
    ] {
        let (backend, zone) = zone(type_sortie, appareil);
        let sp = build_signal_path(
            &aac_tidal(),
            &zone,
            &backend,
            Some("Renderer"),
            transport,
            Some(&fil_wav_16()),
        )
        .unwrap();
        let src = source(&sp);
        assert_eq!(
            src["description"], "AAC 320 kbit/s 44kHz",
            "{type_sortie} : la source est l'AAC, sans les 16 bits du fil WAV : {sp}"
        );
        assert_eq!(
            sp["bit_perfect"], false,
            "{type_sortie} : un AAC n'est pas bit-perfect"
        );
        assert_eq!(
            sp["lossless"], false,
            "{type_sortie} : un AAC est avec perte"
        );
    }
}
