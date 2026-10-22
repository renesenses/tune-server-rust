//! #6166 (Gilles, fil 2228, rc3 Linux ALSA vers un SMSL) — sur une sortie
//! locale, toute lecture PCM passe par un WAV 32 bits INTERNE
//! (`resolve_local.rs`, `local_needs_wav`) : un simple bourrage d'échantillons
//! i32, sans perte. Le panneau le présentait comme un transcodage
//! « FLAC 44kHz/16bit → FLAC 44kHz/32bit », vert en 16 bits et ORANGE en
//! 24 bits, et le testeur y lisait un suréchantillonnage en 32 bits.

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
        .create("SMSL", Some(output_type), Some(device))
        .unwrap();
    (backend.clone(), repo.get(id).unwrap().unwrap())
}

fn flac_playing(bit_depth: i32) -> ZoneState {
    ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Piste FLAC".into(),
            format: Some("flac".into()),
            sample_rate: Some(44_100),
            bit_depth: Some(bit_depth as _),
            stream_id: Some("sid-6166".into()),
            ..Default::default()
        }),
        volume: 1.0,
        ..Default::default()
    }
}

/// Le WAV de passage, tel que la session locale le publie.
fn fil(bit_depth: u16) -> StreamInfo {
    fil_a(44_100, bit_depth)
}

fn fil_a(sample_rate: u32, bit_depth: u16) -> StreamInfo {
    StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: sample_rate as _,
        bit_depth: bit_depth as _,
        ..Default::default()
    }
}

fn transcoder(sp: &Value) -> Option<Value> {
    sp.get("steps")?
        .as_array()?
        .iter()
        .find(|s| s["name"] == "Transcoder")
        .cloned()
}

/// ⭐ Le cas du signalement : FLAC 16 et 24 bits vers une sortie locale ALSA.
/// Aucune étape de conversion, donc le même verdict dans les deux cas.
#[test]
fn le_wav_32_bits_interne_d_une_sortie_locale_n_est_pas_une_conversion_6166() {
    for bd in [16, 24] {
        let (backend, zone) = zone("local", "local:alsa:SMSL USB");
        let sp = build_signal_path(
            &flac_playing(bd),
            &zone,
            &backend,
            Some("SMSL USB"),
            "ALSA",
            Some(&fil(32)),
        )
        .unwrap();
        assert!(
            transcoder(&sp).is_none(),
            "FLAC {bd} bits : le bourrage en 32 bits n'est pas un transcodage : {sp}"
        );
        let texte = sp.to_string();
        assert!(
            !texte.contains("32bit"),
            "FLAC {bd} bits : rien ne doit annoncer « 32bit » : {texte}"
        );
    }
}

/// La contre-épreuve : le même fil WAV vers un renderer DLNA EST un vrai
/// transcodage (conteneur réécrit, plafond LPCM 16 bits) et reste affiché.
#[test]
fn le_meme_fil_wav_vers_un_renderer_dlna_reste_un_transcodage_6166() {
    let (backend, zone) = zone("dlna", "dev-1");
    let sp = build_signal_path(
        &flac_playing(24),
        &zone,
        &backend,
        Some("Wiim Pro"),
        "none",
        Some(&fil(16)),
    )
    .unwrap();
    let etape = transcoder(&sp).expect("un FLAC servi en WAV à un renderer transcode");
    assert_eq!(etape["bit_perfect"], false, "24 → 16 bits : {etape}");
}

/// Seul le conteneur 32 bits À LA CADENCE de la source est un simple
/// bourrage : un fil local rééchantillonné (plafond de fréquence) reste une
/// vraie conversion, et garde son étape.
#[test]
fn un_fil_local_a_une_autre_cadence_reste_une_conversion_6166() {
    let (backend, zone) = zone("local", "local:alsa:SMSL USB");
    let sp = build_signal_path(
        &flac_playing(24),
        &zone,
        &backend,
        Some("SMSL USB"),
        "ALSA",
        Some(&fil_a(48_000, 32)),
    )
    .unwrap();
    assert!(
        transcoder(&sp).is_some(),
        "44,1 → 48 kHz n'est pas un simple conteneur : {sp}"
    );
}
