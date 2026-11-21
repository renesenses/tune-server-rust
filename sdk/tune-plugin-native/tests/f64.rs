//! TESS T05 — négociation du F64 dans l'hôte natif, ABI 1 inchangée, avec de
//! VRAIES bibliothèques natives que le banc construit lui-même (sources
//! écrites dans un dossier temporaire, `cargo build --offline` contre le SDK
//! du dépôt), comme `tune-core/src/orchestrator/greffon_natif_tiers_tests.rs`.
//!
//! - un greffon F32 qui ne demande pas `audio-f64`, empaqueté, signé,
//!   installé puis chargé comme au démarrage, tourne inchangé : l'hôte ne
//!   lui passe jamais de F64 et convertit à sa frontière ;
//! - un greffon qui demande `audio-f64` reçoit bien des blocs F64, au bit
//!   près ;
//! - contre-épreuve : un greffon qui déclare `audio-f64` sans l'implémenter
//!   est refusé au chargement (ou, si la sonde ne peut pas conclure, à la
//!   préparation de l'étage avec ses vrais réglages).
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};
use tune_plugin_native::{Library, package, stage::Stage};
use tune_plugin_sdk::Error;
mod support;

/// Le greffon de test : un gain sur tous les encodages que prépare `Stage`.
/// `ACCEPTE_F64` et `REGLAGES_D_ABORD` sont posés par variante. Le
/// diagnostic compte les blocs F64 reçus par l'instance.
const SOURCE: &str = r#"
use std::sync::atomic::{AtomicU64, Ordering};
use tune_plugin_sdk::{Error, Settings, audio::*};

pub struct Plugin;
struct Gain {
    gain: f64,
    format: AudioFormat,
    max_frames: usize,
    blocs_f64: AtomicU64,
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
    fn assess(&self, _: &PlaybackContext, _: &Settings) -> Result<Applicability, Error> {
        Ok(Applicability::Process { requires_pcm: true })
    }
    fn prepare(
        &self,
        format: AudioFormat,
        max_frames: usize,
        settings: &Settings,
    ) -> Result<Box<dyn Processor>, Error> {
        let g = if crate::REGLAGES_D_ABORD { Some(gain(settings)?) } else { None };
        if format.encoding() == SampleEncoding::F64 && !crate::ACCEPTE_F64 {
            return Err(Error::UnsupportedFormat);
        }
        let g = match g {
            Some(g) => g,
            // La sonde de l'hôte passe `null` : un gain neutre.
            None if settings.is_null() => 1.0,
            None => gain(settings)?,
        };
        Ok(Box::new(Gain { gain: g, format, max_frames, blocs_f64: AtomicU64::new(0) }))
    }
}

impl Processor for Gain {
    fn process(&mut self, block: &mut AudioBlock<'_>, _: BlockContext) -> Result<ProcessReport, Error> {
        if block.format() != self.format || block.frames() > self.max_frames {
            return Err(Error::InvalidFormat);
        }
        let g = self.gain;
        match block.samples_mut() {
            SamplesMut::F64(s) => {
                self.blocs_f64.fetch_add(1, Ordering::Relaxed);
                s.iter_mut().for_each(|v| *v *= g)
            }
            SamplesMut::F32(s) => s.iter_mut().for_each(|v| *v = (f64::from(*v) * g) as f32),
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
    fn diagnostics(&self) -> Settings {
        serde_json::json!({"encoding": format!("{:?}", self.format.encoding()),
                           "f64_blocks": self.blocs_f64.load(Ordering::Relaxed)})
    }
    fn reset(&mut self, _: ResetReason) {}
    fn latency_frames(&self) -> u32 { 0 }
    fn drain(&mut self, _: &mut AudioBlock<'_>) -> Result<DrainReport, Error> {
        Ok(DrainReport { frames_written: 0, complete: true })
    }
}

tune_plugin_abi::export_dsp!(crate::Plugin, include_str!("../manifest.json"));
"#;

/// `(identifiant, demande audio-f64, accepte F64, lit ses réglages d'abord)`.
const VARIANTES: [(&str, bool, bool, bool); 4] = [
    ("gain-f32-t05", false, false, false),
    ("gain-f64-t05", true, true, false),
    ("menteur-f64-t05", true, false, false),
    ("menteur-tardif-f64-t05", true, false, true),
];

fn manifeste(id: &str, f64: bool) -> serde_json::Value {
    let mut capacites = vec![serde_json::json!(
        {"id": "audio-process", "version": {"major": 0, "minor": 1}, "required": true}
    )];
    if f64 {
        capacites.push(serde_json::json!(
            {"id": tune_plugin_sdk::manifest::AUDIO_F64, "version": {"major": 0, "minor": 1}, "required": false}
        ));
    }
    serde_json::json!({
        "id": id, "sdk": {"major": 0, "minor": 1}, "kind": "dsp", "config_version": 1,
        "entitlement": format!("plugin.{id}"), "distribution": "source",
        "capabilities": capacites,
    })
}

struct Banc {
    _dossier: tempfile::TempDir,
    chemins: BTreeMap<&'static str, PathBuf>,
}

/// Construit les quatre bibliothèques une seule fois pour tout le fichier.
fn banc() -> &'static Banc {
    static BANC: OnceLock<Banc> = OnceLock::new();
    BANC.get_or_init(|| {
        let sdk = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let dossier = tempfile::tempdir().unwrap();
        let racine = dossier.path();
        let chemin = |p: PathBuf| format!("{:?}", p.to_string_lossy());
        let mut membres = Vec::new();
        for (id, f64, accepte, d_abord) in VARIANTES {
            let paquet = format!("tune-plugin-essai-{id}");
            let caisse = racine.join(&paquet);
            std::fs::create_dir_all(caisse.join("src")).unwrap();
            std::fs::write(
                caisse.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{paquet}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
                     publish = false\n\n[lib]\ncrate-type = [\"cdylib\"]\n\n[dependencies]\n\
                     tune-plugin-sdk = {{ path = {} }}\ntune-plugin-abi = {{ path = {} }}\n\
                     serde_json = \"1\"\n",
                    chemin(sdk.join("tune-plugin-sdk")),
                    chemin(sdk.join("tune-plugin-abi")),
                ),
            )
            .unwrap();
            std::fs::write(caisse.join("manifest.json"), manifeste(id, f64).to_string()).unwrap();
            std::fs::write(
                caisse.join("src/lib.rs"),
                format!(
                    "const ACCEPTE_F64: bool = {accepte};\nconst REGLAGES_D_ABORD: bool = {d_abord};\n{SOURCE}"
                ),
            )
            .unwrap();
            membres.push(format!("{paquet:?}"));
        }
        std::fs::write(
            racine.join("Cargo.toml"),
            format!(
                "[workspace]\nresolver = \"2\"\nmembers = [{}]\n",
                membres.join(", ")
            ),
        )
        .unwrap();
        std::fs::copy(sdk.join("Cargo.lock"), racine.join("Cargo.lock")).unwrap();
        let cible = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("greffons-f64-t05");
        let sortie = std::process::Command::new(env!("CARGO"))
            .args(["build", "--offline", "--quiet", "--workspace", "--manifest-path"])
            .arg(racine.join("Cargo.toml"))
            .env("CARGO_TARGET_DIR", &cible)
            .env_remove("CARGO_BUILD_TARGET")
            .output()
            .expect("cargo introuvable");
        assert!(
            sortie.status.success(),
            "construction des greffons de test impossible :\n{}",
            String::from_utf8_lossy(&sortie.stderr)
        );
        let chemins = VARIANTES
            .iter()
            .map(|(id, ..)| {
                let nom = format!(
                    "{}tune_plugin_essai_{}{}",
                    std::env::consts::DLL_PREFIX,
                    id.replace('-', "_"),
                    std::env::consts::DLL_SUFFIX
                );
                (*id, cible.join("debug").join(nom))
            })
            .collect();
        Banc {
            _dossier: dossier,
            chemins,
        }
    })
}

fn charger(id: &str) -> Result<Arc<Library>, String> {
    // Safety : bibliothèque construite par le banc lui-même.
    unsafe { Library::load_trusted(&banc().chemins[id]) }
}

fn reglage() -> serde_json::Value {
    serde_json::json!({"gain": 0.5})
}

/// Un signal stéréo dont AUCUN échantillon n'est représentable en f32.
fn signal_f64() -> Vec<f64> {
    (0..9000)
        .map(|i| (f64::from(i) * 0.0137).sin() * 0.7 + 1e-12 * f64::from(i % 7 + 1))
        .collect()
}

#[test]
fn un_greffon_f32_signe_et_installe_tourne_inchange_par_la_conversion_a_sa_frontiere() {
    let id = "gain-f32-t05";
    // Le paquet signé, installé puis chargé comme au démarrage de l'hôte.
    let temp = tempfile::tempdir().unwrap();
    let racine = temp.path().join("plugins");
    let manifeste: tune_plugin_sdk::manifest::Manifest =
        serde_json::from_value(manifeste(id, false)).unwrap();
    assert!(!manifeste.declares_f64());
    let octets = package::pack(
        manifeste,
        &banc().chemins[id],
        package::host_target(),
        &BTreeMap::new(),
    )
    .unwrap();
    let (cles, signature) = support::sign(&octets);
    package::install_for(&racine, &octets, &signature, &cles, id).unwrap();
    let bibliotheque = package::load(&racine, id, &cles).expect("paquet F32 signé refusé");

    let mut etage = Stage::prepare(bibliotheque.clone(), 48_000, 2, &reglage()).unwrap();
    assert!(
        !etage.processes_f64_natively(),
        "un greffon qui n'a pas demandé audio-f64 ne doit jamais recevoir de F64"
    );
    let entree = signal_f64();
    let mut sortie = entree.clone();
    etage.process_f64(&mut sortie).unwrap();
    // Même résultat, au bit près, que le chemin F32 historique élargi.
    let mut historique: Vec<f32> = entree.iter().map(|v| *v as f32).collect();
    let mut temoin = Stage::prepare(bibliotheque, 48_000, 2, &reglage()).unwrap();
    temoin.process_f32(&mut historique).unwrap();
    let attendu: Vec<u64> = historique.iter().map(|v| f64::from(*v).to_bits()).collect();
    let obtenu: Vec<u64> = sortie.iter().map(|v| v.to_bits()).collect();
    assert_eq!(
        obtenu, attendu,
        "le greffon F32 ne tourne pas comme avant derrière la conversion"
    );
    assert_eq!(
        etage.diagnostics()["f64_blocks"],
        0,
        "l'instance F32 a reçu du F64"
    );
    // Les entiers restent servis comme avant.
    let mut pcm: Vec<u8> = (0..4096u32)
        .flat_map(|i| (i as i16).to_le_bytes())
        .collect();
    etage.process_pcm(&mut pcm, 16).unwrap();
}

#[test]
fn un_greffon_qui_demande_audio_f64_recoit_bien_du_f64() {
    let bibliotheque = charger("gain-f64-t05").expect("greffon F64 refusé");
    let mut etage = Stage::prepare(bibliotheque, 48_000, 2, &reglage()).unwrap();
    assert!(etage.processes_f64_natively());
    let entree = signal_f64();
    let mut sortie = entree.clone();
    // Découpé en blocs inégaux : l'hôte découpe aussi en blocs de 4096 trames.
    for bloc in sortie.chunks_mut(2 * 1111) {
        etage.process_f64(bloc).unwrap();
    }
    let attendu: Vec<u64> = entree.iter().map(|v| (v * 0.5).to_bits()).collect();
    let obtenu: Vec<u64> = sortie.iter().map(|v| v.to_bits()).collect();
    assert_eq!(
        obtenu, attendu,
        "le greffon F64 n'a pas reçu les échantillons f64 tels quels"
    );
    let diagnostic = etage.diagnostics();
    assert_eq!(diagnostic["encoding"], "F64", "{diagnostic}");
    assert!(
        diagnostic["f64_blocks"].as_u64().unwrap() >= 5,
        "{diagnostic}"
    );
    // Le chemin F32 de ce greffon reste disponible pour l'hôte actuel.
    let mut f = vec![0.5f32; 64];
    etage.process_f32(&mut f).unwrap();
    assert!(f.iter().all(|v| *v == 0.25));
}

#[test]
fn contre_epreuve_un_greffon_qui_declare_f64_sans_l_implementer_est_refuse_au_chargement() {
    let erreur = charger("menteur-f64-t05")
        .err()
        .expect("un greffon qui déclare audio-f64 et refuse F64 a été chargé");
    assert!(erreur.contains("audio-f64"), "{erreur}");
}

#[test]
fn contre_epreuve_si_la_sonde_ne_conclut_pas_la_preparation_le_refuse() {
    // Ce menteur refuse les réglages `null` de la sonde avant de juger le
    // format : le chargement passe, la préparation avec ses vrais réglages
    // le refuse.
    let bibliotheque = charger("menteur-tardif-f64-t05").expect("sonde trop stricte");
    assert!(matches!(
        Stage::prepare(bibliotheque, 48_000, 2, &reglage()),
        Err(Error::CapabilityMissing)
    ));
}
