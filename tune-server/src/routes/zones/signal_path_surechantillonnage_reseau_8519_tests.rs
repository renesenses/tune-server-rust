//! Discussion #8519 — sur une zone DLNA ou OpenHome, le chemin du signal lit
//! le suréchantillonneur dans le flux servi : l'étape « 44,1 kHz → 176,4 kHz »
//! quand il est cuit, sa raison quand il ne l'est pas.

use super::*;
use std::sync::Arc;
use tune_core::db::sqlite::SqliteDb;
use tune_core::http::streamer::SurechantillonnageCuit;
use tune_core::playback::NowPlaying;

const LANGUES: [&str; 15] = [
    "fr", "en", "de", "es", "it", "zh", "ja", "ko", "ro", "sv", "vi", "hu", "nb", "pl", "el",
];

fn zone(output_type: &str) -> (Arc<dyn tune_core::db::backend::DbBackend>, Zone, ZoneState) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn tune_core::db::backend::DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo
        .create("Salon", Some(output_type), Some("uuid:eversolo-dmp-a8"))
        .unwrap();
    let settings = SettingsRepo::with_backend(backend.clone());
    settings
        .set(tune_core::audio::premium_plugins::MIGRATION, "complete")
        .unwrap();
    settings.set("plugin_upsampler_installed", "true").unwrap();
    settings.set("plugin_upsampler_enabled", "true").unwrap();
    tune_core::audio::surechantillonnage::enregistrer_les_reglages(
        &settings,
        id,
        &serde_json::json!({"enabled": true}),
    )
    .unwrap();
    let zone = repo.get(id).unwrap().unwrap();
    let ps = ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Piste".into(),
            format: Some("flac".into()),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            stream_id: Some("sid-1".into()),
            ..Default::default()
        }),
        volume: 1.0,
        ..Default::default()
    };
    (backend, zone, ps)
}

fn fil_surechantillonne() -> StreamInfo {
    StreamInfo {
        format: "flac".into(),
        mime_type: "audio/flac".into(),
        sample_rate: 176_400,
        bit_depth: 24,
        channels: 2,
        surechantillonnage: Some(SurechantillonnageCuit {
            de: 44_100,
            vers: 176_400,
            filtre: "polyvalent".into(),
            reserve_db: -3.0,
        }),
        ..Default::default()
    }
}

/// « FLAC 44,1 kHz/16 → suréchantillonné 176,4 kHz/24 → DLNA » : l'étape du
/// greffon, la conversion de cadence, et jamais bit-perfect.
#[test]
fn le_flux_dlna_surechantillonne_porte_l_etape_du_greffon_8519() {
    for t in ["dlna", "openhome"] {
        let (backend, zone, ps) = zone(t);
        let fil = fil_surechantillonne();
        let sp = build_signal_path(&ps, &zone, &backend, Some("Eversolo"), "dlna", Some(&fil))
            .expect("un chemin du signal");
        assert_eq!(sp["upsampling"]["active"], true, "{t} : {sp}");
        assert_eq!(sp["upsampling"]["from_hz"], 44_100);
        assert_eq!(sp["upsampling"]["to_hz"], 176_400);
        assert_eq!(sp["rate_conversion"]["to_hz"], 176_400, "{sp}");
        assert_eq!(
            sp["bit_perfect"], false,
            "un flux suréchantillonné n'est pas bit-perfect"
        );
        let etape = sp["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["code"] == "upsampling")
            .unwrap_or_else(|| panic!("étape du suréchantillonneur absente : {sp}"));
        assert_eq!(etape["filter"], "polyvalent");
        let d = etape["description"].as_str().unwrap();
        assert!(d.contains("44.1 kHz") && d.contains("176.4 kHz"), "{d}");
        assert_eq!(
            sp["steps"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["code"] == "rate_conversion" || e["code"] == "upsampling")
                .count(),
            1,
            "une seule étape de cadence : celle du greffon : {sp}"
        );
    }
}

/// Le flux a noté pourquoi l'étage n'y est pas : la raison et son texte,
/// dans les 15 langues ; jamais « zone non prise en charge » pour DLNA.
#[test]
fn le_refus_note_par_le_flux_dlna_est_dit_8519() {
    let (backend, zone, ps) = zone("dlna");
    for (code, cle) in [
        (
            "renderer_rates_unknown",
            "upsampling_renderer_rates_unknown",
        ),
        ("renderer_rate_ceiling", "upsampling_renderer_rate_ceiling"),
        ("progressive_stream", "upsampling_progressive_stream"),
        ("source_not_pcm", "upsampling_source_not_pcm"),
        ("track_too_heavy", "upsampling_track_too_heavy"),
        (
            "machine_test_rejects_accepted_rates",
            "upsampling_machine_rates",
        ),
        (
            "settings_caps_exclude_admitted_rates",
            "upsampling_settings_caps",
        ),
        ("strict_bitperfect", "upsampling_strict_bitperfect"),
    ] {
        let fil = StreamInfo {
            format: "flac".into(),
            sample_rate: 44_100,
            bit_depth: 16,
            refus_surechantillonnage: Some(code),
            ..Default::default()
        };
        let sp = build_signal_path(&ps, &zone, &backend, Some("Eversolo"), "dlna", Some(&fil))
            .expect("un chemin du signal");
        assert_eq!(sp["upsampling"]["active"], false, "{sp}");
        assert_eq!(sp["upsampling"]["reason"], code, "{sp}");
        let e = sp["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["code"] == "upsampling")
            .unwrap_or_else(|| panic!("ligne absente : {sp}"));
        assert_eq!(
            e["description"],
            super::textes_signal::texte("fr", cle),
            "{code}"
        );
        for lang in LANGUES {
            let (_, etape) = etat_du_surechantillonneur_lang(
                lang,
                &backend,
                zone.id.unwrap(),
                false,
                "dlna",
                None,
                Some(&fil),
            )
            .unwrap();
            let texte = etape["description"].as_str().unwrap();
            assert!(!texte.is_empty(), "{lang} {code}");
            assert_ne!(
                texte,
                super::textes_signal::texte(lang, "upsampling_not_applied"),
                "{lang} {code} : raison muette"
            );
        }
    }
}

/// Une zone DLNA sans flux décidé n'est plus « non prise en charge » ; PURE
/// garde la priorité.
#[test]
fn une_zone_dlna_n_est_plus_dite_non_prise_en_charge_8519() {
    let (backend, zone, _ps) = zone("dlna");
    let id = zone.id.unwrap();
    let (etat, _) =
        etat_du_surechantillonneur_lang("fr", &backend, id, false, "dlna", None, None).unwrap();
    assert_ne!(etat["reason"], "output_not_supported", "{etat}");
    let (etat, _) =
        etat_du_surechantillonneur_lang("fr", &backend, id, true, "dlna", None, None).unwrap();
    assert_eq!(etat["reason"], "pure", "{etat}");
}

#[test]
fn les_textes_reseau_existent_dans_toutes_les_langues_8519() {
    let textes: serde_json::Map<String, Value> =
        serde_json::from_str(include_str!("textes_signal.json")).unwrap();
    assert_eq!(textes.len(), 15);
    for (lang, t) in &textes {
        for cle in [
            "upsampling_zone_not_supported",
            "upsampling_source_not_pcm",
            "upsampling_progressive_stream",
            "upsampling_renderer_rates_unknown",
            "upsampling_renderer_rate_ceiling",
            "upsampling_track_too_heavy",
        ] {
            let v = t[cle].as_str();
            assert!(v.is_some_and(|v| !v.is_empty()), "{cle} manque en {lang}");
        }
        assert!(
            t["upsampling_zone_not_supported"]
                .as_str()
                .unwrap()
                .contains("DLNA"),
            "{lang} : le texte doit nommer DLNA, désormais pris en charge"
        );
    }
}
