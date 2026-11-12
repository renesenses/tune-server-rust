//! #7614 — le suréchantillonneur dans le chemin du signal : l'étape « Upsampler »
//! (filtre, réserve) quand il porte la cadence, la ligne « inactif (mode PURE) »
//! quand PURE l'écarte, et rien quand il est désactivé.

use super::*;
use std::sync::Arc;
use tune_core::db::sqlite::SqliteDb;
use tune_core::outputs::traits::{AudioSpec, FormatOuvert, ProfondeurPcm};
use tune_core::playback::NowPlaying;

fn zone_locale() -> (Arc<dyn tune_core::db::backend::DbBackend>, Zone, ZoneState) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn tune_core::db::backend::DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo
        .create("Salon", Some("local"), Some("local:DAC"))
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

fn chemin(
    backend: &Arc<dyn tune_core::db::backend::DbBackend>,
    zone: &Zone,
    ps: &ZoneState,
) -> Value {
    let fil = StreamInfo {
        format: "flac".into(),
        sample_rate: 44_100,
        bit_depth: 16,
        ..Default::default()
    };
    build_signal_path(ps, zone, backend, Some("DAC"), "CPAL", Some(&fil)).unwrap()
}

/// Le greffon installé et actif, migration des greffons passée : ce que
/// l'installation depuis le catalogue laisse en base.
fn installer_le_greffon(backend: &Arc<dyn tune_core::db::backend::DbBackend>) {
    let s = SettingsRepo::with_backend(backend.clone());
    s.set(tune_core::audio::premium_plugins::MIGRATION, "complete")
        .unwrap();
    s.set("plugin_upsampler_installed", "true").unwrap();
    s.set("plugin_upsampler_enabled", "true").unwrap();
}

/// #8259 — ce que `marketplace::uninstall_plugin` efface : les drapeaux du
/// greffon, pas le réglage de zone (configuration préservée).
fn desinstaller_le_greffon(backend: &Arc<dyn tune_core::db::backend::DbBackend>) {
    let s = SettingsRepo::with_backend(backend.clone());
    s.delete("plugin_upsampler_installed").unwrap();
    s.delete("plugin_upsampler_enabled").unwrap();
}

fn activer(backend: &Arc<dyn tune_core::db::backend::DbBackend>, zone_id: i64) {
    installer_le_greffon(backend);
    tune_core::audio::surechantillonnage::enregistrer_les_reglages(
        &SettingsRepo::with_backend(backend.clone()),
        zone_id,
        &serde_json::json!({"enabled": true}),
    )
    .unwrap();
}

fn etape<'a>(sp: &'a Value, code: &str) -> Option<&'a Value> {
    sp["steps"].as_array()?.iter().find(|e| e["code"] == code)
}

#[test]
fn desactive_rien_n_apparait() {
    let (backend, zone, ps) = zone_locale();
    let sp = chemin(&backend, &zone, &ps);
    assert!(sp.get("upsampling").is_none(), "{sp}");
    assert!(etape(&sp, "upsampling").is_none());
}

#[test]
fn actif_l_etape_nomme_le_filtre_et_la_reserve_et_le_bit_perfect_tombe() {
    let (backend, zone, mut ps) = zone_locale();
    activer(&backend, zone.id.unwrap());
    ps.transformations_reelles = Some(
        TransformationsReelles::nouvelles(
            AudioSpec::nouvelle(44_100, ProfondeurPcm::Entier16, 2).unwrap(),
            FormatOuvert::new(352_800, 2),
            false,
        )
        .avec_surechantillonnage("polyvalent", -8.55),
    );
    let sp = chemin(&backend, &zone, &ps);
    assert_eq!(sp["bit_perfect"], false);
    let e = etape(&sp, "upsampling").expect("l'étape du suréchantillonneur");
    assert_eq!(e["from_hz"], 44_100);
    assert_eq!(e["to_hz"], 352_800);
    assert_eq!(e["filter"], "polyvalent");
    assert_eq!(e["bit_perfect"], false);
    let d = e["description"].as_str().unwrap();
    assert!(
        d.contains("44.1 kHz") && d.contains("352.8 kHz") && d.contains("-8.55"),
        "{d}"
    );
    assert_eq!(sp["upsampling"]["active"], true);
    assert_eq!(sp["upsampling"]["headroom_db"], -8.55);
    assert!(
        etape(&sp, "rate_conversion").is_none(),
        "une seule étape de cadence"
    );
}

#[test]
fn sous_pure_la_ligne_inactif_mode_pure() {
    let (backend, zone, ps) = zone_locale();
    let id = zone.id.unwrap();
    activer(&backend, id);
    SettingsRepo::with_backend(backend.clone())
        .set(&format!("zone_{id}_audiophile"), r#"{"enabled":true}"#)
        .unwrap();
    let sp = chemin(&backend, &zone, &ps);
    assert_eq!(sp["upsampling"]["active"], false);
    assert_eq!(sp["upsampling"]["reason"], "pure");
    let e = etape(&sp, "upsampling").expect("la ligne PURE");
    assert_eq!(e["bit_perfect"], true);
    assert!(e["description"].as_str().unwrap().contains("PURE"));
}

/// #8259 — greffon désinstallé, réglage de zone resté à `enabled` : plus de
/// ligne « non appliqué … à la piste suivante », plus d'état à la racine.
#[test]
fn greffon_desinstalle_plus_de_ligne_8259() {
    let (backend, zone, ps) = zone_locale();
    activer(&backend, zone.id.unwrap());
    let avant = chemin(&backend, &zone, &ps);
    assert!(
        etape(&avant, "upsampling").is_some(),
        "greffon installé : la ligne est là ({avant})"
    );
    desinstaller_le_greffon(&backend);
    assert!(
        tune_core::audio::surechantillonnage::reglages_de_la_zone(
            &SettingsRepo::with_backend(backend.clone()),
            zone.id.unwrap()
        )
        .enabled,
        "le réglage de zone survit à la désinstallation"
    );
    let sp = chemin(&backend, &zone, &ps);
    assert!(sp.get("upsampling").is_none(), "{sp}");
    assert!(etape(&sp, "upsampling").is_none(), "{sp}");
}

/// #8259 — greffon désactivé (pas désinstallé) : rien ne tourne, pas de ligne.
#[test]
fn greffon_desactive_plus_de_ligne_8259() {
    let (backend, zone, ps) = zone_locale();
    activer(&backend, zone.id.unwrap());
    SettingsRepo::with_backend(backend.clone())
        .set("plugin_upsampler_enabled", "false")
        .unwrap();
    let sp = chemin(&backend, &zone, &ps);
    assert!(etape(&sp, "upsampling").is_none(), "{sp}");
}

/// #8259 — la ligne « non appliqué » porte la raison connue : le test de la
/// machine n'a pas été passé.
#[test]
fn non_applique_nomme_le_test_de_la_machine_absent_8259() {
    let (backend, zone, ps) = zone_locale();
    activer(&backend, zone.id.unwrap());
    let sp = chemin(&backend, &zone, &ps);
    assert_eq!(sp["upsampling"]["active"], false, "{sp}");
    assert_eq!(sp["upsampling"]["reason"], "machine_test_missing", "{sp}");
    assert!(etape(&sp, "upsampling").is_some());
}

#[test]
fn diagnostic_8259_le_chemin_restitue_le_refus_observe_et_non_un_reglage_deduit() {
    let (backend, zone, mut ps) = zone_locale();
    activer(&backend, zone.id.unwrap());
    // Aucun verdict en base : une observation de la piste reste prioritaire.
    for (raison, mot) in [
        ("disabled", "désactivé à l’ouverture"),
        ("plugin_not_installed", "greffon absent"),
        ("premium_required", "Premium"),
        ("strict_bitperfect", "strict"),
        ("rates_not_measured", "mesurées"),
        ("no_candidate_rate_accepted_by_device", "périphérique"),
        ("machine_test_rejects_accepted_rates", "test"),
        ("settings_caps_exclude_admitted_rates", "limites"),
        ("source_probe_incomplete", "source"),
        ("output_path_not_supported", "chemin"),
        ("dop", "DoP"),
        ("multiple_outputs", "plusieurs"),
        ("output_rate_open_failed", "impossible"),
        ("filter_initialization_failed", "impossible"),
    ] {
        ps.transformations_reelles = Some(
            TransformationsReelles::nouvelles(
                AudioSpec::nouvelle(44_100, ProfondeurPcm::Entier16, 2).unwrap(),
                FormatOuvert::new(44_100, 2),
                false,
            )
            .avec_refus_surechantillonnage(Some(raison)),
        );
        let sp = chemin(&backend, &zone, &ps);
        assert_eq!(
            sp["upsampling"]["reason"], raison,
            "refus observé perdu : {sp}"
        );
        let description = etape(&sp, "upsampling").unwrap()["description"]
            .as_str()
            .unwrap();
        assert!(
            description.contains(mot),
            "raison non expliquée : {description}"
        );
        assert!(
            !description.contains("piste suivante"),
            "promesse injustifiée : {description}"
        );
    }
}

#[test]
fn diagnostic_8259_arrete_ne_publie_plus_de_chemin_meme_si_le_reglage_persiste() {
    let (backend, zone, mut ps) = zone_locale();
    activer(&backend, zone.id.unwrap());
    ps.state = PlayState::Stopped;
    assert!(build_signal_path(&ps, &zone, &backend, Some("DAC"), "CPAL", None).is_none());
}

#[test]
fn diagnostic_8259_machine_absente_expliquee_dans_toutes_les_langues() {
    let (backend, zone, _) = zone_locale();
    activer(&backend, zone.id.unwrap());
    for lang in [
        "fr", "en", "de", "es", "it", "zh", "ja", "ko", "ro", "sv", "vi", "hu", "nb", "pl", "el",
    ] {
        let (_, etape) = signal_path::etat_du_surechantillonneur_lang(
            lang,
            &backend,
            zone.id.unwrap(),
            false,
            "local",
            None,
        )
        .unwrap();
        let texte = etape["description"].as_str().unwrap();
        assert_ne!(texte, textes_signal::texte(lang, "upsampling_not_applied"));
        assert!(
            !texte.starts_with("upsampling_"),
            "traduction absente : {lang}"
        );
    }
}
