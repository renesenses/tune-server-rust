//! #6178 (kiran, fil 2233) — sur une zone HQPlayer sans traitement, le chemin
//! du signal montrait une étape « Decoder » (« FLAC ») entre la source et
//! HQPlayer : on y lisait que Tune décode avant HQPlayer. C'est HQPlayer qui
//! décode ; Tune sert les octets tels quels.

use super::*;
use std::sync::Arc;
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_core::playback::NowPlaying;

fn chemin_de(output_type: &str, reglages: &[(&str, &str)]) -> Value {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    for (cle, valeur) in reglages {
        SettingsRepo::with_backend(backend.clone())
            .set(cle, valeur)
            .unwrap();
    }
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo
        .create("Salon", Some(output_type), Some("dev-1"))
        .unwrap();
    let zone = repo.get(id).unwrap().unwrap();
    let mut t = tune_core::db::models::Track::new("Piste".into());
    t.format = Some("flac".into());
    t.sample_rate = Some(96_000);
    t.bit_depth = Some(24);
    let tid = TrackRepo::with_backend(backend.clone()).create(&t).unwrap();
    tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(backend.clone())
        .set(tid, "rg_track_gain", "-6.5 dB")
        .unwrap();
    let ps = ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Piste".into(),
            track_id: Some(tid),
            format: Some("flac".into()),
            sample_rate: Some(96_000),
            bit_depth: Some(24),
            stream_id: Some("sid-1".into()),
            ..Default::default()
        }),
        volume: 1.0,
        ..Default::default()
    };
    let fil = StreamInfo {
        format: "flac".into(),
        sample_rate: 96_000,
        bit_depth: 24,
        ..Default::default()
    };
    build_signal_path(&ps, &zone, &backend, Some("HQPlayer"), "none", Some(&fil)).unwrap()
}

fn noms(sp: &Value) -> Vec<String> {
    sp["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap_or("").to_string())
        .collect()
}

#[test]
fn hqplayer_sans_traitement_n_a_pas_d_etape_de_decodage_par_tune_6178() {
    let sp = chemin_de("hqplayer", &[]);
    assert!(
        !noms(&sp).contains(&"Decoder".to_string()),
        "#6178 : HQPlayer décode lui-même, le panneau prête un décodage à Tune : {:?}",
        noms(&sp)
    );
    assert_eq!(noms(&sp)[0], "Source");
}

#[test]
fn les_sorties_que_tune_alimente_gardent_leur_decodeur() {
    for sortie in ["dlna", "local", "oaat"] {
        let sp = chemin_de(sortie, &[]);
        assert_eq!(
            noms(&sp).get(1).map(String::as_str),
            Some("Decoder"),
            "{sortie} : {:?}",
            noms(&sp)
        );
    }
}

/// Dès que Tune touche le signal pour HQPlayer (ReplayGain), il décode : l'étape
/// revient.
#[test]
fn hqplayer_avec_replaygain_retrouve_son_decodeur() {
    let sp = chemin_de(
        "hqplayer",
        &[(tune_core::audio::replaygain::MODE_KEY, "track")],
    );
    assert!(
        noms(&sp).contains(&"ReplayGain".to_string()),
        "prémisse : le ReplayGain s'applique : {:?}",
        noms(&sp)
    );
    assert_eq!(noms(&sp).get(1).map(String::as_str), Some("Decoder"));
}
