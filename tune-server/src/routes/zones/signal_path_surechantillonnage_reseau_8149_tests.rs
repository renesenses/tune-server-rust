//! #8149 — sur une zone RÉSEAU (DLNA…), le suréchantillonneur réglé ne
//! s'applique pas : la ligne du chemin du signal le dit, avec sa raison, sans
//! promettre « la piste suivante » (il ne s'appliquera jamais).

use super::*;
use std::sync::Arc;
use tune_core::db::sqlite::SqliteDb;
use tune_core::playback::NowPlaying;

fn zone(output_type: &str) -> (Arc<dyn tune_core::db::backend::DbBackend>, Zone, ZoneState) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn tune_core::db::backend::DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(backend.clone());
    let device = format!("{output_type}:salon");
    let id = repo
        .create("Salon", Some(output_type), Some(&device))
        .unwrap();
    let settings = SettingsRepo::with_backend(backend.clone());
    // Greffon installé et actif : ce que laisse l'installation depuis le
    // catalogue (sans effet avant #8259, requis après).
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

fn ligne(lang: &str, output_type: &str) -> (Value, Value) {
    let (backend, zone, _ps) = zone(output_type);
    etat_du_surechantillonneur_lang(
        lang,
        &backend,
        zone.id.unwrap(),
        false,
        output_type,
        None,
        Default::default(),
    )
    .expect("suréchantillonneur réglé : un état et une ligne")
}

#[test]
fn zone_airplay_dit_que_le_surechantillonnage_ne_s_applique_pas_8149() {
    // rc14 — DLNA et OpenHome le portent désormais : AirPlay reste hors champ.
    let (etat, etape) = ligne("fr", "airplay");
    assert_eq!(etat["active"], false, "{etat}");
    assert_eq!(etat["reason"], "output_not_supported", "{etat}");
    let d = etape["description"].as_str().unwrap();
    assert_eq!(
        d,
        super::textes_signal::texte("fr", "upsampling_zone_not_supported")
    );
    assert!(d.contains("cette zone"), "{d}");
    assert!(
        !d.contains("suivante"),
        "#8149 : une zone réseau ne recevra jamais le suréchantillonnage, \
         « à la piste suivante » est faux : {d}"
    );
    let en = ligne("en", "airplay").1;
    let en = en["description"].as_str().unwrap();
    assert!(!en.contains("next track"), "{en}");
    assert!(en.contains("this zone"), "{en}");
}

#[test]
fn zone_locale_garde_le_non_applique_a_cette_piste_8149() {
    let (etat, etape) = ligne("fr", "local");
    assert_eq!(etat["active"], false);
    assert_ne!(
        etape["description"].as_str().unwrap(),
        super::textes_signal::texte("fr", "upsampling_zone_not_supported"),
        "la sortie locale peut l'appliquer à la piste suivante : sa ligne ne change pas"
    );
}

#[test]
fn le_texte_existe_dans_toutes_les_langues_8149() {
    let textes: serde_json::Map<String, Value> =
        serde_json::from_str(include_str!("textes_signal.json")).unwrap();
    for (lang, t) in &textes {
        let v = t["upsampling_zone_not_supported"].as_str();
        assert!(v.is_some_and(|v| !v.is_empty()), "manque en {lang}");
    }
}

#[test]
fn le_chemin_d_une_zone_airplay_porte_la_ligne_8149() {
    let (backend, zone, ps) = zone("airplay");
    let fil = StreamInfo {
        format: "flac".into(),
        sample_rate: 44_100,
        bit_depth: 16,
        ..Default::default()
    };
    let sp = build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("Eversolo"),
        "airplay",
        Some(&fil),
    )
    .expect("un chemin du signal");
    assert_eq!(sp["upsampling"]["reason"], "output_not_supported", "{sp}");
    let e = sp["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["code"] == "upsampling")
        .unwrap_or_else(|| panic!("ligne du suréchantillonneur absente : {sp}"));
    assert!(
        !e["description"].as_str().unwrap().contains("next track"),
        "{e}"
    );
}

/// #8149 (GgB, fil 2742, 10/11) — Eversolo DAC-Z8 en sortie locale, « bit-perfect
/// strict » activé : le suréchantillonneur était écarté sans un mot à l'écran
/// (« non appliqué… à la piste suivante », faux tant que le strict reste).
/// La ligne dit maintenant la vraie raison.
#[test]
fn zone_locale_en_bit_perfect_strict_dit_la_raison_8149() {
    let (backend, zone, _ps) = zone("local");
    let id = zone.id.unwrap();
    SettingsRepo::with_backend(backend.clone())
        .set(
            &tune_core::audio::bitperfect_strict::cle_de_zone(id),
            "true",
        )
        .unwrap();
    let (etat, etape) = etat_du_surechantillonneur_lang(
        "fr",
        &backend,
        id,
        false,
        "local",
        None,
        Default::default(),
    )
    .unwrap();
    assert_eq!(etat["reason"], "strict_bitperfect", "{etat}");
    let d = etape["description"].as_str().unwrap();
    assert_eq!(
        d,
        super::textes_signal::texte("fr", "upsampling_strict_bitperfect")
    );
    assert!(d.contains("bit-perfect strict"), "{d}");
    assert!(!d.contains("suivante"), "{d}");
    // PURE garde la priorité : sa ligne ne change pas.
    let (etat, _) = etat_du_surechantillonneur_lang(
        "fr",
        &backend,
        id,
        true,
        "local",
        None,
        Default::default(),
    )
    .unwrap();
    assert_eq!(etat["reason"], "pure", "{etat}");
}

#[test]
fn le_texte_du_strict_existe_dans_toutes_les_langues_8149() {
    let textes: serde_json::Map<String, Value> =
        serde_json::from_str(include_str!("textes_signal.json")).unwrap();
    assert_eq!(textes.len(), 20);
    for (lang, t) in &textes {
        let v = t["upsampling_strict_bitperfect"].as_str();
        assert!(v.is_some_and(|v| !v.is_empty()), "manque en {lang}");
    }
}

/// #8428 — sous une cadence fixe, le greffon de suréchantillonnage est écarté
/// (dsp.rs) : la ligne le dit, au lieu du « non appliqué… à la piste
/// suivante » muet. Contre-épreuve : sans cadence fixe, la raison redevient
/// celle d'avant.
#[test]
fn zone_locale_a_cadence_fixe_dit_la_raison_8428() {
    let (backend, zone, _ps) = zone("local");
    let id = zone.id.unwrap();
    let settings = SettingsRepo::with_backend(backend.clone());
    settings
        .set(&tune_core::audio::frequence_fixe::cle_de_zone(id), "48000")
        .unwrap();
    let (etat, etape) = etat_du_surechantillonneur_lang(
        "fr",
        &backend,
        id,
        false,
        "local",
        None,
        Default::default(),
    )
    .unwrap();
    assert_eq!(etat["reason"], "fixed_rate", "{etat}");
    let d = etape["description"].as_str().unwrap();
    assert_eq!(
        d,
        super::textes_signal::texte("fr", "upsampling_fixed_rate")
    );
    assert!(!d.contains("suivante"), "{d}");
    settings
        .delete(&tune_core::audio::frequence_fixe::cle_de_zone(id))
        .unwrap();
    let (etat, _) = etat_du_surechantillonneur_lang(
        "fr",
        &backend,
        id,
        false,
        "local",
        None,
        Default::default(),
    )
    .unwrap();
    assert_ne!(etat["reason"], "fixed_rate", "{etat}");
}

#[test]
fn le_texte_de_la_cadence_fixe_existe_dans_toutes_les_langues_8428() {
    let textes: serde_json::Map<String, Value> =
        serde_json::from_str(include_str!("textes_signal.json")).unwrap();
    assert_eq!(textes.len(), 20);
    for (lang, t) in &textes {
        let v = t["upsampling_fixed_rate"].as_str();
        assert!(v.is_some_and(|v| !v.is_empty()), "manque en {lang}");
    }
}

#[test]
fn cadence_8671_chemin_explique_le_repli_mesure() {
    let (backend, zone, ps) = zone("local");
    let mut fil = StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 48_000,
        bit_depth: 32,
        channels: 2,
        repli_pcm_exclusif: Some((44_100, 48_000)),
        ..Default::default()
    };
    let sp = build_signal_path(&ps, &zone, &backend, Some("Casque"), "local", Some(&fil)).unwrap();
    let steps = sp["steps"].as_array().unwrap();
    let e = steps
        .iter()
        .find(|e| e["reason"] == "wasapi_exclusive_rate_unsupported")
        .expect("le chemin doit expliquer le refus de cadence WASAPI");
    assert_eq!(e["from_hz"], 44_100);
    assert_eq!(e["to_hz"], 48_000);
    assert_eq!(e["bit_perfect"], false);
    assert!(e["description"].as_str().unwrap().contains("WASAPI"));
    assert_eq!(
        steps
            .iter()
            .filter(|e| e["code"] == "rate_conversion")
            .count(),
        1
    );
    fil.repli_pcm_exclusif = None;
    let sp = build_signal_path(&ps, &zone, &backend, Some("Casque"), "local", Some(&fil)).unwrap();
    assert!(
        !sp["steps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["reason"] == "wasapi_exclusive_rate_unsupported")
    );
    let textes: serde_json::Map<String, Value> =
        serde_json::from_str(include_str!("textes_signal.json")).unwrap();
    for (lang, t) in &textes {
        let v = t["wasapi_rate_fallback"]
            .as_str()
            .expect("traduction du repli");
        assert!(v.contains("{0}") && v.contains("{1}"), "{lang}");
    }
}

/// rc14 — zone DLNA, piste de la bibliothèque suréchantillonnée par le fichier
/// refait : le chemin du signal le dit (« 44,1 kHz → 176,4 kHz »), et le
/// verdict n'est jamais bit-perfect.
#[test]
fn zone_dlna_surechantillonnee_le_chemin_le_dit_et_n_est_pas_bit_perfect() {
    let (backend, zone, ps) = zone("dlna");
    let fil = StreamInfo {
        format: "flac".into(),
        mime_type: "audio/flac".into(),
        sample_rate: 176_400,
        bit_depth: 24,
        channels: 2,
        surechantillonnage: Some(tune_core::http::streamer::SurechDuFlux {
            de_hz: 44_100,
            vers_hz: 176_400,
            filtre: "polyvalent".into(),
            reserve_db: -3.0,
        }),
        ..Default::default()
    };
    let sp = build_signal_path(&ps, &zone, &backend, Some("Eversolo"), "dlna", Some(&fil))
        .expect("un chemin du signal");
    assert_eq!(sp["upsampling"]["active"], true, "{sp}");
    assert_eq!(sp["upsampling"]["to_hz"], 176_400, "{sp}");
    assert_eq!(sp["bit_perfect"], false, "jamais bit-perfect : {sp}");
    let e = sp["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["code"] == "upsampling")
        .unwrap_or_else(|| panic!("étape du suréchantillonneur absente : {sp}"));
    assert_eq!(e["bit_perfect"], false);
    assert_eq!(e["from_hz"], 44_100);
    assert_eq!(e["to_hz"], 176_400);
    let d = e["description"].as_str().unwrap();
    assert!(d.contains("44.1 kHz \u{2192} 176.4 kHz"), "{d}");
    // Une seule étape porte la cadence.
    assert_eq!(
        sp["steps"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["code"] == "rate_conversion" || e["code"] == "upsampling")
            .count(),
        1,
        "{sp}"
    );
    assert_eq!(sp["rate_conversion"]["to_hz"], 176_400);
}

/// rc14 — zone DLNA, refus de la lecture : la ligne dit la raison.
#[test]
fn zone_dlna_refusee_dit_la_raison() {
    let (backend, zone, _ps) = zone("dlna");
    let (etat, etape) = etat_du_surechantillonneur_lang(
        "fr",
        &backend,
        zone.id.unwrap(),
        false,
        "dlna",
        None,
        ReseauDuSurechantillonneur {
            flux: None,
            refus: Some("renderer_rate_unknown"),
            source_de_la_bibliotheque: true,
        },
    )
    .unwrap();
    assert_eq!(etat["reason"], "renderer_rate_unknown");
    assert_eq!(
        etape["description"],
        super::textes_signal::texte("fr", "upsampling_network_rate_unknown")
    );
    // Un service : pas encore porté sur UPnP.
    let (etat, _) = etat_du_surechantillonneur_lang(
        "fr",
        &backend,
        zone.id.unwrap(),
        false,
        "dlna",
        None,
        ReseauDuSurechantillonneur {
            flux: None,
            refus: None,
            source_de_la_bibliotheque: false,
        },
    )
    .unwrap();
    assert_eq!(etat["reason"], "source_not_supported_on_network");
    // PURE prime.
    let (etat, _) = etat_du_surechantillonneur_lang(
        "fr",
        &backend,
        zone.id.unwrap(),
        true,
        "dlna",
        None,
        Default::default(),
    )
    .unwrap();
    assert_eq!(etat["reason"], "pure");
}
