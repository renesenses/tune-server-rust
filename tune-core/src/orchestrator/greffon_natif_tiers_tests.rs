//! Greffon natif TIERS, de bout en bout dans l'orchestrateur, avec une VRAIE
//! bibliothèque native : l'étage casque de la zone le porte en Premium, le
//! perd en Free, en mode PURE, greffon désactivé ou case décochée, et il
//! traite réellement les échantillons (flottants et entiers).
//!
//! La bibliothèque est un greffon DSP de test (un gain), généré par
//! `cargo tune-plugin new <id> --template dsp` puis étendu aux encodages
//! entiers, que l'adaptateur `Stage` de l'hôte prépare tous. Son chemin vient
//! de `TUNE_NATIVE_TIERS_TEST_LIB` ; le test est `#[ignore]` sans elle, et
//! ÉCHOUE (il ne passe pas en silence) si on le lance sans la fournir :
//!
//! ```sh
//! TUNE_NATIVE_TIERS_TEST_LIB=/chemin/libgreffon_essai.so \
//!   cargo test -p tune-core --lib greffon_natif_tiers -- --ignored
//! ```
use super::PlaybackOrchestrator;
use crate::db::settings_repo::SettingsRepo;
use std::sync::Arc;

fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    )
}

/// Charge et enregistre la bibliothèque de test ; rend son identifiant.
fn enregistrer_la_bibliotheque_de_test() -> String {
    let chemin = std::env::var_os("TUNE_NATIVE_TIERS_TEST_LIB")
        .expect("TUNE_NATIVE_TIERS_TEST_LIB : chemin de la bibliothèque du greffon de test");
    // Safety : bibliothèque de test construite par le banc lui-même.
    let bibliotheque =
        unsafe { tune_plugin_native::Library::load_trusted(std::path::Path::new(&chemin)) }
            .expect("bibliothèque de test illisible");
    let id = bibliotheque.manifest.id.clone();
    assert!(
        crate::audio::natifs_tiers::identifiant_admissible(&id),
        "le greffon de test doit porter un identifiant tiers, pas {id}"
    );
    tune_plugin_native::register(bibliotheque).unwrap();
    id
}

#[tokio::test]
#[ignore = "exige TUNE_NATIVE_TIERS_TEST_LIB (bibliothèque native de test)"]
async fn greffon_natif_tiers_porte_par_l_etage_casque_en_premium_seulement() {
    let id = enregistrer_la_bibliotheque_de_test();
    let mut orch = orchestrateur();
    let licence = Arc::new(crate::license::LicenseManager::new_with_limit(
        orch.db.clone(),
        3,
    ));
    orch.license = Some(licence.clone());
    let s = SettingsRepo::with_backend(orch.db.clone());
    s.set(&format!("plugin_{id}_installed"), "true").unwrap();
    s.set(&format!("plugin_{id}_enabled"), "true").unwrap();
    let cle = crate::audio::natifs_tiers::cle_de_zone(1, &id);
    s.set(&cle, r#"{"enabled":true,"gain":0.5}"#).unwrap();

    // Free : aucun étage, même installé, activé et réglé.
    assert!(
        orch.load_crossfeed_processor(1, 48_000).is_none(),
        "greffon natif tiers actif en Free"
    );

    licence.set_account_premium(true, None).await;
    let mut etage = orch
        .load_crossfeed_processor(1, 48_000)
        .expect("greffon natif tiers absent de l'étage casque en Premium");
    assert_eq!(etage.etages_tiers(), 1, "étage tiers non préparé");
    // Flottants.
    let mut f = vec![0.8_f32; 512];
    etage.process_interleaved(&mut f);
    assert!(
        f.iter().all(|v| (v - 0.4).abs() < 1e-6),
        "le greffon n'a pas traité les flottants : {:?}",
        &f[..4]
    );
    // Entiers 16 bits, le chemin de la sortie locale.
    let mut pcm: Vec<u8> = std::iter::repeat_n(16_000_i16.to_le_bytes(), 512)
        .flatten()
        .collect();
    etage.process_pcm(&mut pcm, 16, 2);
    let premier = i16::from_le_bytes([pcm[0], pcm[1]]);
    assert!(
        (7_999..=8_001).contains(&premier),
        "le greffon n'a pas traité le PCM 16 bits : {premier}"
    );
    // Il entre dans l'empreinte du traitement du flux.
    assert!(
        orch.empreinte_du_traitement(1, None).contains("tiers="),
        "empreinte du traitement sans l'étage tiers"
    );
    assert_eq!(
        orch.traitement_que_pure_gouverne(1, None),
        Some("greffon_natif_tiers")
    );

    // Case décochée.
    s.set(&cle, r#"{"enabled":false,"gain":0.5}"#).unwrap();
    assert!(orch.load_crossfeed_processor(1, 48_000).is_none());
    s.set(&cle, r#"{"enabled":true,"gain":0.5}"#).unwrap();
    // Greffon désactivé.
    s.set(&format!("plugin_{id}_enabled"), "false").unwrap();
    assert!(orch.load_crossfeed_processor(1, 48_000).is_none());
    s.set(&format!("plugin_{id}_enabled"), "true").unwrap();
    // Mode PURE.
    s.set("zone_1_audiophile", r#"{"enabled":true}"#).unwrap();
    assert!(
        orch.load_crossfeed_processor(1, 48_000).is_none(),
        "PURE doit désarmer l'étage tiers"
    );
    s.delete("zone_1_audiophile").unwrap();
    // Retour en Free : coupure immédiate, réglage conservé.
    licence.set_account_premium(false, None).await;
    assert!(orch.load_crossfeed_processor(1, 48_000).is_none());
    assert_eq!(
        s.get(&cle).unwrap().as_deref(),
        Some(r#"{"enabled":true,"gain":0.5}"#)
    );
}
