//! Greffon natif TIERS, de bout en bout dans l'orchestrateur, avec une VRAIE
//! bibliothèque native : l'étage casque de la zone le porte en Premium, le
//! perd en Free, en mode PURE, greffon désactivé ou case décochée, et il
//! traite réellement les échantillons (flottants et entiers).
//!
//! La bibliothèque est un greffon DSP de test (un gain), celui que génère
//! `cargo tune-plugin new <id> --template dsp`, étendu aux encodages entiers
//! que l'adaptateur `Stage` de l'hôte prépare tous. #6522 : le banc la
//! CONSTRUIT lui-même ([`construire_la_bibliotheque_de_test`]) — sources
//! écrites dans un dossier temporaire, `cargo build` hors ligne contre le SDK
//! du dépôt —, si bien que le test tourne partout où tourne `cargo test`, au
//! lieu de dormir en `#[ignore]` faute de bibliothèque fournie à la main.
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

/// Identifiant du greffon de test : tiers (hors liste Premium), valide pour
/// le SDK.
const ID_DU_GREFFON_DE_TEST: &str = "gain-essai-6522";

/// Le greffon de test : le gain du gabarit `dsp` du SDK, sur tous les
/// encodages que prépare `Stage` (F32, S16, S24Le, S32) et sur le F64.
const SOURCE_DU_GREFFON_DE_TEST: &str = r#"
use tune_plugin_sdk::{Error, Settings, audio::*};

pub struct Plugin;
struct Gain {
    gain: f64,
    format: AudioFormat,
    max_frames: usize,
}

fn gain(settings: &Settings) -> Result<f64, Error> {
    let value = settings
        .get("gain")
        .and_then(|v| v.as_f64())
        .ok_or(Error::InvalidSettings)?;
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(Error::InvalidSettings);
    }
    Ok(value)
}

impl DspFactory for Plugin {
    fn assess(&self, _: &PlaybackContext, settings: &Settings) -> Result<Applicability, Error> {
        Ok(if gain(settings)? == 1.0 {
            Applicability::Bypass(BypassReason::Neutral)
        } else {
            Applicability::Process { requires_pcm: true }
        })
    }
    fn prepare(
        &self,
        format: AudioFormat,
        max_frames: usize,
        settings: &Settings,
    ) -> Result<Box<dyn Processor>, Error> {
        if max_frames == 0 {
            return Err(Error::BlockTooLarge);
        }
        Ok(Box::new(Gain { gain: gain(settings)?, format, max_frames }))
    }
}

impl Processor for Gain {
    fn process(&mut self, block: &mut AudioBlock<'_>, _: BlockContext) -> Result<ProcessReport, Error> {
        if block.format() != self.format {
            return Err(Error::InvalidFormat);
        }
        if block.frames() > self.max_frames {
            return Err(Error::BlockTooLarge);
        }
        let g = self.gain;
        match block.samples_mut() {
            SamplesMut::F32(s) => s.iter_mut().for_each(|v| *v = (f64::from(*v) * g) as f32),
            SamplesMut::F64(s) => s.iter_mut().for_each(|v| *v *= g),
            SamplesMut::S16(s) => s.iter_mut().for_each(|v| *v = (f64::from(*v) * g).round() as i16),
            SamplesMut::S32(s) => s.iter_mut().for_each(|v| *v = (f64::from(*v) * g).round() as i32),
            SamplesMut::S24Le(b) => {
                for e in b.chunks_exact_mut(3) {
                    let v = i32::from_le_bytes([0, e[0], e[1], e[2]]) >> 8;
                    let o = ((f64::from(v) * g).round() as i32).to_le_bytes();
                    e.copy_from_slice(&o[..3]);
                }
            }
        }
        Ok(ProcessReport { changed: g != 1.0, ..Default::default() })
    }
    fn reset(&mut self, _: ResetReason) {}
    fn latency_frames(&self) -> u32 {
        0
    }
    fn drain(&mut self, _: &mut AudioBlock<'_>) -> Result<DrainReport, Error> {
        Ok(DrainReport { frames_written: 0, complete: true })
    }
}

tune_plugin_abi::export_dsp!(crate::Plugin, include_str!("../manifest.json"));
"#;

/// Construit la bibliothèque native du greffon de test dans `dossier` et rend
/// son chemin.
///
/// Les sources sont écrites à la volée ; elles ne dépendent que du SDK du
/// dépôt (`sdk/tune-plugin-sdk`, `sdk/tune-plugin-abi`). Le `Cargo.lock` de la
/// racine est repris pour que `--offline` trouve les mêmes versions de serde
/// que la construction de `tune-core`, déjà dans le cache de Cargo. Aucune
/// variable d'environnement n'est requise ; un échec de construction fait
/// échouer le test avec la sortie de Cargo.
fn construire_la_bibliotheque_de_test(dossier: &std::path::Path) -> std::path::PathBuf {
    let racine = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("racine du dépôt")
        .to_path_buf();
    let sdk = racine.join("sdk");
    let caisse = dossier.join("greffon");
    std::fs::create_dir_all(caisse.join("src")).unwrap();
    let chemin_toml = |p: std::path::PathBuf| format!("{:?}", p.to_string_lossy());
    std::fs::write(
        caisse.join("Cargo.toml"),
        format!(
            "[workspace]\n\n[package]\nname = \"tune-plugin-gain-essai\"\nversion = \"0.1.0\"\n\
             edition = \"2024\"\npublish = false\n\n[lib]\ncrate-type = [\"cdylib\"]\n\n\
             [dependencies]\ntune-plugin-sdk = {{ path = {} }}\ntune-plugin-abi = {{ path = {} }}\n\
             serde_json = \"1\"\n",
            chemin_toml(sdk.join("tune-plugin-sdk")),
            chemin_toml(sdk.join("tune-plugin-abi")),
        ),
    )
    .unwrap();
    std::fs::write(
        caisse.join("manifest.json"),
        serde_json::json!({
            "id": ID_DU_GREFFON_DE_TEST,
            "sdk": {"major": 0, "minor": 1},
            "kind": "dsp",
            "config_version": 1,
            "entitlement": format!("plugin.{ID_DU_GREFFON_DE_TEST}"),
            "distribution": "source",
            "capabilities": [
                {"id": "audio-process", "version": {"major": 0, "minor": 1}, "required": true}
            ]
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(caisse.join("src/lib.rs"), SOURCE_DU_GREFFON_DE_TEST).unwrap();
    std::fs::copy(racine.join("Cargo.lock"), caisse.join("Cargo.lock")).unwrap();

    let cible = dossier.join("target");
    let sortie = std::process::Command::new(env!("CARGO"))
        .args(["build", "--offline", "--quiet", "--manifest-path"])
        .arg(caisse.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", &cible)
        // La construction de test peut viser une autre cible (CI croisée) :
        // la bibliothèque, elle, se charge dans CE processus.
        .env_remove("CARGO_BUILD_TARGET")
        .output()
        .expect("cargo introuvable");
    assert!(
        sortie.status.success(),
        "construction du greffon natif de test impossible :\n{}",
        String::from_utf8_lossy(&sortie.stderr)
    );
    cible.join("debug").join(format!(
        "{}tune_plugin_gain_essai{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ))
}

/// Construit, charge et enregistre la bibliothèque de test ; rend son
/// identifiant. Le dossier rendu doit vivre jusqu'à la fin du test.
fn enregistrer_la_bibliotheque_de_test() -> (String, crate::test_scratch::ScratchDir) {
    let dossier = crate::test_scratch::scratch_dir("tune-greffon-natif-6522");
    let chemin = construire_la_bibliotheque_de_test(&dossier);
    // Safety : bibliothèque de test construite par le banc lui-même.
    let bibliotheque = unsafe { tune_plugin_native::Library::load_trusted(&chemin) }
        .expect("bibliothèque de test illisible");
    let id = bibliotheque.manifest.id.clone();
    assert_eq!(id, ID_DU_GREFFON_DE_TEST);
    assert!(
        crate::audio::natifs_tiers::identifiant_admissible(&id),
        "le greffon de test doit porter un identifiant tiers, pas {id}"
    );
    tune_plugin_native::register(bibliotheque).unwrap();
    (id, dossier)
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

    // #6522 — en Premium, sur une sortie locale : PURE contourne le greffon,
    // et le verdict le dit au lieu de « rien à contourner ».
    #[cfg(feature = "local-audio")]
    {
        licence.set_account_premium(true, None).await;
        pure_contourne_le_greffon_sur_la_sortie_locale(&orch, &id).await;
    }
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
