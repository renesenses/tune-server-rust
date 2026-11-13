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
        None,
    )
    .expect("suréchantillonneur réglé : un état et une ligne")
}

#[test]
fn zone_chromecast_dit_que_le_surechantillonnage_ne_s_applique_pas_8149() {
    // Depuis #8519, DLNA et OpenHome le prennent en charge : la zone réseau
    // témoin est un Chromecast.
    let (etat, etape) = ligne("fr", "chromecast");
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
    let en = ligne("en", "chromecast").1;
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
fn le_chemin_d_une_zone_chromecast_porte_la_ligne_8149() {
    let (backend, zone, ps) = zone("chromecast");
    let fil = StreamInfo {
        format: "flac".into(),
        sample_rate: 44_100,
        bit_depth: 16,
        ..Default::default()
    };
    let sp = build_signal_path(&ps, &zone, &backend, Some("Eversolo"), "dlna", Some(&fil))
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
    let (etat, etape) =
        etat_du_surechantillonneur_lang("fr", &backend, id, false, "local", None, None).unwrap();
    assert_eq!(etat["reason"], "strict_bitperfect", "{etat}");
    let d = etape["description"].as_str().unwrap();
    assert_eq!(
        d,
        super::textes_signal::texte("fr", "upsampling_strict_bitperfect")
    );
    assert!(d.contains("bit-perfect strict"), "{d}");
    assert!(!d.contains("suivante"), "{d}");
    // PURE garde la priorité : sa ligne ne change pas.
    let (etat, _) =
        etat_du_surechantillonneur_lang("fr", &backend, id, true, "local", None, None).unwrap();
    assert_eq!(etat["reason"], "pure", "{etat}");
}

#[test]
fn le_texte_du_strict_existe_dans_toutes_les_langues_8149() {
    let textes: serde_json::Map<String, Value> =
        serde_json::from_str(include_str!("textes_signal.json")).unwrap();
    assert_eq!(textes.len(), 15);
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
    let (etat, etape) =
        etat_du_surechantillonneur_lang("fr", &backend, id, false, "local", None, None).unwrap();
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
    let (etat, _) =
        etat_du_surechantillonneur_lang("fr", &backend, id, false, "local", None, None).unwrap();
    assert_ne!(etat["reason"], "fixed_rate", "{etat}");
}

#[test]
fn le_texte_de_la_cadence_fixe_existe_dans_toutes_les_langues_8428() {
    let textes: serde_json::Map<String, Value> =
        serde_json::from_str(include_str!("textes_signal.json")).unwrap();
    assert_eq!(textes.len(), 15);
    for (lang, t) in &textes {
        let v = t["upsampling_fixed_rate"].as_str();
        assert!(v.is_some_and(|v| !v.is_empty()), "manque en {lang}");
    }
}
