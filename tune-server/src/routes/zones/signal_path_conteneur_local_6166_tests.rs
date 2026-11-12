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

// ── #8629 (fil 2789) — radio FLAC 96/24 vers une sortie locale ALSA ──────

fn radio_playing() -> ZoneState {
    ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "High-Res Live Radio".into(),
            source: "radio".into(),
            format: Some("wav".into()),
            sample_rate: Some(96_000),
            bit_depth: Some(24),
            stream_id: Some("sid-8629".into()),
            ..Default::default()
        }),
        volume: 1.0,
        ..Default::default()
    }
}

/// Le fil d'une radio : décodé en WAV, avec ce que le flux annonçait.
fn fil_radio(
    codec: &'static str,
    source_rate: u32,
    source_bits: Option<u16>,
    wire_rate: u32,
    wire_bits: u16,
) -> StreamInfo {
    use tune_core::http::streamer::RadioSourceInfo;
    StreamInfo {
        radio_source: Some(RadioSourceInfo {
            channels: Some(2),
            format: Some(codec),
            sample_rate: Some(source_rate),
            bit_depth: source_bits,
            ..Default::default()
        }),
        ..fil_a(wire_rate, wire_bits)
    }
}

/// ⭐ Le cas du signalement : FLAC 96/24 décodé en WAV 24/96 pour la sortie
/// locale. Aucun échantillon touché : pas d'étape Transcoder, badge global
/// bit-perfect.
#[test]
fn une_radio_flac_24_bits_sur_sortie_locale_n_a_pas_d_etape_fantome_8629() {
    let (backend, zone) = zone("local", "local:alsa:ADI-2");
    let sp = build_signal_path(
        &radio_playing(),
        &zone,
        &backend,
        Some("ADI-2"),
        "ALSA",
        Some(&fil_radio("flac", 96_000, Some(24), 96_000, 24)),
    )
    .unwrap();
    assert!(
        transcoder(&sp).is_none(),
        "WAV 24/96 interne pour une source 24/96 : pas de transcodage : {sp}"
    );
    assert_eq!(sp["bit_perfect"], true, "fil intact : {sp}");
    assert_eq!(sp["lossless"], true, "{sp}");
}

/// Même règle pour un fichier FLAC 24 bits servi en WAV 24 bits localement.
#[test]
fn un_fil_local_wav_de_meme_profondeur_n_est_pas_une_conversion_8629() {
    for bd in [16u16, 24] {
        let (backend, zone) = zone("local", "local:alsa:SMSL USB");
        let sp = build_signal_path(
            &flac_playing(i32::from(bd)),
            &zone,
            &backend,
            Some("SMSL USB"),
            "ALSA",
            Some(&fil(bd)),
        )
        .unwrap();
        assert!(transcoder(&sp).is_none(), "FLAC {bd} → WAV {bd} : {sp}");
    }
}

/// Contre-épreuve DLNA : FLAC 24 bits servi en WAV sans `dlna_wav24` garde
/// son verdict (règle du plafond LPCM 16 bits), même pour une radio.
#[test]
fn la_regle_lpcm_16_bits_reste_celle_du_dlna_8629() {
    let (backend, zone) = zone("dlna", "dev-1");
    let flac = build_signal_path(
        &flac_playing(24),
        &zone,
        &backend,
        Some("Wiim Pro"),
        "none",
        Some(&fil(24)),
    )
    .unwrap();
    let etape = transcoder(&flac).expect("FLAC → WAV vers un renderer transcode");
    assert_eq!(etape["bit_perfect"], false, "sans dlna_wav24 : {etape}");

    let radio = build_signal_path(
        &radio_playing(),
        &zone,
        &backend,
        Some("Wiim Pro"),
        "none",
        Some(&fil_radio("flac", 96_000, Some(24), 96_000, 24)),
    )
    .unwrap();
    let etape = transcoder(&radio).expect("radio FLAC servie en WAV à un renderer");
    assert_eq!(etape["bit_perfect"], false, "sans dlna_wav24 : {etape}");
}

/// Les vraies conversions sur sortie locale restent signalées : fil plus
/// étroit que la source, rééchantillonnage, source avec perte.
#[test]
fn les_vraies_conversions_locales_restent_signalees_8629() {
    let (backend, zone) = zone("local", "local:alsa:ADI-2");
    // 24 → 16 bits : troncature.
    let sp = build_signal_path(
        &radio_playing(),
        &zone,
        &backend,
        Some("ADI-2"),
        "ALSA",
        Some(&fil_radio("flac", 96_000, Some(24), 96_000, 16)),
    )
    .unwrap();
    let etape = transcoder(&sp).expect("24 → 16 bits est une conversion");
    assert_eq!(etape["bit_perfect"], false, "{etape}");
    assert_eq!(sp["bit_perfect"], false, "{sp}");

    // 96 → 48 kHz : rééchantillonnage.
    let sp = build_signal_path(
        &radio_playing(),
        &zone,
        &backend,
        Some("ADI-2"),
        "ALSA",
        Some(&fil_radio("flac", 96_000, Some(24), 48_000, 24)),
    )
    .unwrap();
    assert!(transcoder(&sp).is_some(), "96 → 48 kHz : {sp}");
    assert_eq!(sp["bit_perfect"], false, "{sp}");

    // MP3 décodé en WAV 16 bits : avec perte, jamais bit-perfect.
    let sp = build_signal_path(
        &radio_playing(),
        &zone,
        &backend,
        Some("ADI-2"),
        "ALSA",
        Some(&fil_radio("mp3", 44_100, None, 44_100, 16)),
    )
    .unwrap();
    assert!(transcoder(&sp).is_some(), "MP3 → WAV 16 bits : {sp}");
    assert_eq!(sp["bit_perfect"], false, "{sp}");
    assert_eq!(sp["lossless"], false, "{sp}");
}
