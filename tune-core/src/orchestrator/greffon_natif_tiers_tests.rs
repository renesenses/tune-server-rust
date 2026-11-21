//! Greffon natif TIERS, de bout en bout dans l'orchestrateur, avec une VRAIE
//! bibliothèque native : l'étage casque de la zone le porte en Premium, le
//! perd en Free, en mode PURE, greffon désactivé ou case décochée, et il
//! traite réellement les échantillons (flottants et entiers).
//!
//! La bibliothèque est un greffon DSP de test (un gain), celui que génère
//! `cargo tune-plugin new <id> --template dsp`, étendu aux encodages entiers
//! que l'adaptateur `Stage` de l'hôte prépare tous. #6522 : le banc la
//! CONSTRUIT lui-même ([`construire_les_bibliotheques_de_test`]) — sources
//! écrites dans un dossier temporaire, `cargo build` hors ligne contre le SDK
//! du dépôt —, si bien que le test tourne partout où tourne `cargo test`, au
//! lieu de dormir en `#[ignore]` faute de bibliothèque fournie à la main.
use super::PlaybackOrchestrator;
use crate::db::settings_repo::SettingsRepo;
use std::sync::Arc;

pub(super) fn orchestrateur() -> PlaybackOrchestrator {
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

/// Identifiant du greffon de test : tiers (hors liste Premium), valide pour
/// le SDK.
const ID_DU_GREFFON_DE_TEST: &str = "gain-essai-6522";

pub(super) use crate::greffon_natif_de_test::enregistrer_les_bibliotheques_de_test;

/// Construit, charge et enregistre la bibliothèque de test ; rend son
/// identifiant. Le dossier rendu doit vivre jusqu'à la fin du test.
fn enregistrer_la_bibliotheque_de_test() -> (String, crate::test_scratch::ScratchDir) {
    let (mut ids, dossier) = enregistrer_les_bibliotheques_de_test(&[ID_DU_GREFFON_DE_TEST]);
    (ids.remove(0), dossier)
}

#[tokio::test]
async fn greffon_natif_tiers_porte_par_l_etage_casque_en_premium_seulement() {
    let (id, _dossier) = enregistrer_la_bibliotheque_de_test();
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
        orch.empreinte_du_traitement(1, crate::audio::replaygain::PisteRg::AUCUNE)
            .contains("tiers="),
        "empreinte du traitement sans l'étage tiers"
    );
    assert_eq!(
        orch.traitement_que_pure_gouverne(1, crate::audio::replaygain::PisteRg::AUCUNE),
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

    // #6522 — en Premium, sur une sortie locale : PURE contourne le greffon,
    // et le verdict le dit au lieu de « rien à contourner ».
    #[cfg(feature = "local-audio")]
    {
        licence.set_account_premium(true, None).await;
        pure_contourne_le_greffon_sur_la_sortie_locale(&orch, &id).await;
    }
}

/// #7616 — étage casque EXCLUSIF, avec de VRAIES bibliothèques : une zone
/// qui a coché le crossfeed intégré, un greffon `crossfeed-…` ET `crosstalk`
/// n'a qu'un étage en service, `crosstalk`. Les deux greffons de test sont le
/// même gain ×0,5 : deux étages en série rendraient ×0,25, et le crossfeed
/// intégré ferait fuir la voie gauche dans la droite.
#[tokio::test]
async fn greffon_natif_tiers_crosstalk_eteint_tout_crossfeed_7616() {
    const FAMILLE: &str = "crossfeed-essai-7616";
    let (_ids, _dossier) = enregistrer_les_bibliotheques_de_test(&[FAMILLE, "crosstalk"]);
    let orch = orchestrateur();
    let s = SettingsRepo::with_backend(orch.db.clone());
    s.set(crate::audio::premium_plugins::MIGRATION, "complete")
        .unwrap();
    s.set("plugin_crossfeed_installed", "true").unwrap();
    s.set("plugin_crossfeed_enabled", "true").unwrap();
    s.set(
        "zone_1_crossfeed",
        r#"{"enabled":true,"amount":0.3,"delay_ms":0.3}"#,
    )
    .unwrap();
    for id in [FAMILLE, "crosstalk"] {
        s.set(&format!("plugin_{id}_installed"), "true").unwrap();
        s.set(&format!("plugin_{id}_enabled"), "true").unwrap();
        s.set(
            &crate::audio::natifs_tiers::cle_de_zone(1, id),
            r#"{"enabled":true,"gain":0.5}"#,
        )
        .unwrap();
    }

    let mut etage = orch
        .load_crossfeed_processor(1, 48_000)
        .expect("l'étage casque porte crosstalk");
    assert_eq!(
        etage.decrire(),
        vec!["tiers:crosstalk@48000".to_string()],
        "crosstalk seul en service : ni crossfeed intégré, ni {FAMILLE}"
    );
    assert_eq!(etage.amount(), 0.0);
    let mut f: Vec<f32> = std::iter::repeat_n([0.8_f32, 0.0], 256).flatten().collect();
    etage.process_interleaved(&mut f);
    assert!(
        f.chunks_exact(2)
            .all(|lr| (lr[0] - 0.4).abs() < 1e-6 && lr[1] == 0.0),
        "un seul gain, aucune voie croisée : {:?}",
        &f[..4]
    );
    assert_eq!(orch.crossfeed_configure(1), None);
    assert_eq!(
        crate::audio::natifs_tiers::greffon_de_crossfeed_de_la_zone(&s, 1).as_deref(),
        Some("crosstalk"),
        "le chemin du signal doit nommer crosstalk"
    );
    let empreinte = orch.empreinte_des_etages_tiers(1);
    assert!(
        empreinte.contains("crosstalk=") && !empreinte.contains(FAMILLE),
        "l'empreinte (cache, relance du flux) compte un étage écarté : {empreinte}"
    );

    // Témoin : crosstalk décoché, la famille du crossfeed reprend la main.
    s.set(
        &crate::audio::natifs_tiers::cle_de_zone(1, "crosstalk"),
        r#"{"enabled":false,"gain":0.5}"#,
    )
    .unwrap();
    let etage = orch.load_crossfeed_processor(1, 48_000).unwrap();
    assert_eq!(etage.decrire(), vec![format!("tiers:{FAMILLE}@48000")]);
    // Puis sans greffon : le crossfeed intégré revient.
    s.set(
        &crate::audio::natifs_tiers::cle_de_zone(1, FAMILLE),
        r#"{"enabled":false,"gain":0.5}"#,
    )
    .unwrap();
    let etage = orch.load_crossfeed_processor(1, 48_000).unwrap();
    assert!(
        etage.decrire().iter().all(|e| e.starts_with("crossfeed:")),
        "{:?}",
        etage.decrire()
    );
}

/// #6522 (fil 2333) — greffon natif tiers armé sur une zone LOCALE, piste
/// démarrée hors PURE, puis la vraie bascule PURE : l'étage casque est vidé
/// de la chaîne, et le verdict doit rester « DSP contourné par PURE ».
#[cfg(feature = "local-audio")]
async fn pure_contourne_le_greffon_sur_la_sortie_locale(orch: &PlaybackOrchestrator, id: &str) {
    use crate::outputs::traits::{OutputDspState, OutputTarget};
    let zone_id = crate::db::zone_repo::ZoneRepo::with_backend(orch.db.clone())
        .create("USB DAC ASIO", Some("local"), Some("local:DAC"))
        .unwrap();
    SettingsRepo::with_backend(orch.db.clone())
        .set(
            &crate::audio::natifs_tiers::cle_de_zone(zone_id, id),
            r#"{"enabled":true,"gain":0.5}"#,
        )
        .unwrap();
    orch.outputs
        .lock()
        .await
        .register(Box::new(crate::outputs::local::LocalOutput::new(
            "DAC".to_string(),
        )));
    let arc = orch.outputs.lock().await.get("local:DAC").unwrap();
    {
        let sortie = arc.lock().await;
        let local = sortie
            .as_any()
            .downcast_ref::<crate::outputs::local::LocalOutput>()
            .unwrap();
        local.declare_current_format_for_test(48_000, 2);
        local.set_pure_bypass(false);
        local.set_crossfeed(orch.load_crossfeed_processor(zone_id, 48_000));
    }
    SettingsRepo::with_backend(orch.db.clone())
        .set(&format!("zone_{zone_id}_audiophile"), r#"{"enabled":true}"#)
        .unwrap();
    assert!(orch.refresh_zone_pure_dsp(zone_id).await);
    let sortie = arc.lock().await;
    let local = sortie
        .as_any()
        .downcast_ref::<crate::outputs::local::LocalOutput>()
        .unwrap();
    local.publier_le_contrat_pour_test();
    assert_eq!(
        local.signal_path_status().unwrap().dsp,
        OutputDspState::BypassedPure,
        "greffon natif armé + PURE = « DSP contourné par PURE » (#6522)"
    );
}

/// `POST /audio-plugins/{id}/diagnostics` : l'hôte rend ce que la VRAIE
/// bibliothèque calcule pour un réglage, sans l'enregistrer ; un réglage
/// qu'elle refuse est refusé.
#[test]
fn les_diagnostics_d_un_reglage_viennent_du_greffon() {
    let (id, _dossier) = enregistrer_la_bibliotheque_de_test();
    let d = crate::audio::natifs_tiers::diagnostics_du_reglage(
        &id,
        &serde_json::json!({"enabled": true, "gain": 0.25}),
    )
    .unwrap();
    assert_eq!(d, serde_json::json!({"gain": 0.25}));
    let refus = crate::audio::natifs_tiers::diagnostics_du_reglage(
        &id,
        &serde_json::json!({"enabled": true, "gain": 2.0}),
    )
    .unwrap_err();
    assert!(refus.contains("refused"), "{refus}");
    assert!(
        crate::audio::natifs_tiers::valider_reglage(&id, &serde_json::json!({"gain": 0.5})).is_ok()
    );
}

/// TESS T03 (relecture de #9024) — un étage casque qui porte le crossfeed
/// intégré ET un greffon natif tiers (une VRAIE bibliothèque native) passe par
/// la chaîne TESS de la sortie locale avec les bits de l'ancien chemin, bloc
/// par bloc.
#[cfg(feature = "local-audio")]
#[tokio::test]
async fn crossfeed_et_natif_tiers_par_la_chaine_tess_rendent_les_bits_d_avant() {
    let (ids, _dossier) = enregistrer_les_bibliotheques_de_test(&["gain-essai-t03"]);
    let id = &ids[0];
    let mut orch = orchestrateur();
    let licence = Arc::new(crate::license::LicenseManager::new_with_limit(
        orch.db.clone(),
        3,
    ));
    orch.license = Some(licence.clone());
    licence.set_account_premium(true, None).await;
    let s = SettingsRepo::with_backend(orch.db.clone());
    s.set(crate::audio::premium_plugins::MIGRATION, "complete")
        .unwrap();
    s.set("plugin_crossfeed_installed", "true").unwrap();
    s.set("plugin_crossfeed_enabled", "true").unwrap();
    s.set(
        "zone_1_crossfeed",
        r#"{"enabled":true,"amount":0.5,"delay_ms":0.3}"#,
    )
    .unwrap();
    s.set(&format!("plugin_{id}_installed"), "true").unwrap();
    s.set(&format!("plugin_{id}_enabled"), "true").unwrap();
    s.set(
        &crate::audio::natifs_tiers::cle_de_zone(1, id),
        r#"{"enabled":true,"gain":0.5}"#,
    )
    .unwrap();
    let nouveau = orch
        .load_crossfeed_processor(1, 48_000)
        .expect("étage casque");
    let ancien = orch
        .load_crossfeed_processor(1, 48_000)
        .expect("étage casque");
    assert_eq!(
        nouveau.etages_tiers(),
        1,
        "le natif tiers doit être dans l'étage"
    );
    assert!(
        nouveau
            .decrire()
            .iter()
            .any(|e| e.starts_with("crossfeed:")),
        "le crossfeed intégré doit être dans l'étage : {:?}",
        nouveau.decrire()
    );
    let blocs =
        crate::outputs::local::comparer_un_etage_casque_a_l_ancien_pour_test(nouveau, ancien);
    assert_eq!(blocs, 10);
}
