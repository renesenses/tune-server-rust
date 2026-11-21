//! Greffon natif de test : une VRAIE bibliothèque native (un gain), construite
//! par le banc lui-même (`cargo build` hors ligne contre le SDK du dépôt),
//! chargée et inscrite au registre de `tune_plugin_native`. #6522 l'a écrit
//! pour l'orchestrateur ; TESS T16 (#9160) le partage avec les tests de
//! `tune-server`, qui doivent voir un greffon `crosstalk` réellement chargé
//! dans le vrai chemin du signal.
//!
//! Comme [`crate::services_factices`], compilé seulement en test ou avec la
//! feature `services-factices`, que seuls les `[dev-dependencies]` de
//! `tune-server` allument : aucun binaire publié ne le contient.

/// Le greffon de test : le gain du gabarit `dsp` du SDK, sur tous les
/// encodages que prépare `Stage` (F32, S16, S24Le, S32) et sur le F64.
pub const SOURCE_DU_GREFFON_DE_TEST: &str = r#"
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
    fn diagnostics(&self) -> Settings {
        serde_json::json!({"gain": self.gain})
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

/// Construit, dans `dossier`, une bibliothèque native du greffon de test (le
/// même gain) par identifiant de `ids`, et rend leurs chemins dans le même
/// ordre. #7616 : l'identifiant est le seul paramètre, pour poser sur une zone
/// des étages de familles différentes (`crossfeed-…`, `crosstalk`, autre).
///
/// Les sources sont écrites à la volée ; elles ne dépendent que du SDK du
/// dépôt (`sdk/tune-plugin-sdk`, `sdk/tune-plugin-abi`). Le `Cargo.lock` de la
/// racine est repris pour que `--offline` trouve les mêmes versions de serde
/// que la construction de `tune-core`, déjà dans le cache de Cargo. Les
/// constructions partagent un même dossier cible : les dépendances ne sont
/// compilées qu'une fois. Aucune variable d'environnement n'est requise ; un
/// échec de construction fait échouer le test avec la sortie de Cargo.
pub fn construire_les_bibliotheques_de_test(
    dossier: &std::path::Path,
    ids: &[&str],
) -> Vec<std::path::PathBuf> {
    let racine = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("racine du dépôt")
        .to_path_buf();
    let sdk = racine.join("sdk");
    let cible = dossier.join("target");
    let chemin_toml = |p: std::path::PathBuf| format!("{:?}", p.to_string_lossy());
    ids.iter()
        .map(|id| {
            // Un nom de caisse et de fichier PROPRE à l'identifiant : deux
            // bibliothèques chargées dans le même processus ne se confondent
            // pas, et le dossier cible partagé ne les écrase pas.
            let paquet = format!("tune-plugin-essai-{id}");
            let caisse = dossier.join(&paquet);
            std::fs::create_dir_all(caisse.join("src")).unwrap();
            std::fs::write(
                caisse.join("Cargo.toml"),
                format!(
                    "[workspace]\n\n[package]\nname = \"{paquet}\"\nversion = \"0.1.0\"\n\
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
                    "id": id,
                    "sdk": {"major": 0, "minor": 1},
                    "kind": "dsp",
                    "config_version": 1,
                    "entitlement": format!("plugin.{id}"),
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

            let sortie = std::process::Command::new(env!("CARGO"))
                .args(["build", "--offline", "--quiet", "--manifest-path"])
                .arg(caisse.join("Cargo.toml"))
                .env("CARGO_TARGET_DIR", &cible)
                // La construction de test peut viser une autre cible (CI
                // croisée) : la bibliothèque, elle, se charge dans CE processus.
                .env_remove("CARGO_BUILD_TARGET")
                .output()
                .expect("cargo introuvable");
            assert!(
                sortie.status.success(),
                "construction du greffon natif de test {id} impossible :\n{}",
                String::from_utf8_lossy(&sortie.stderr)
            );
            cible.join("debug").join(format!(
                "{}{}{}",
                std::env::consts::DLL_PREFIX,
                paquet.replace('-', "_"),
                std::env::consts::DLL_SUFFIX
            ))
        })
        .collect()
}

/// Construit, charge et enregistre une bibliothèque de test par identifiant
/// de `ids` ; rend les identifiants enregistrés. Le dossier rendu doit vivre
/// jusqu'à la fin du test.
pub fn enregistrer_les_bibliotheques_de_test(
    ids: &[&str],
) -> (Vec<String>, crate::test_scratch::ScratchDir) {
    let dossier = crate::test_scratch::scratch_dir("tune-greffon-natif-6522");
    let enregistres = construire_les_bibliotheques_de_test(&dossier, ids)
        .into_iter()
        .zip(ids)
        .map(|(chemin, attendu)| {
            // Safety : bibliothèque de test construite par le banc lui-même.
            let bibliotheque = unsafe { tune_plugin_native::Library::load_trusted(&chemin) }
                .expect("bibliothèque de test illisible");
            let id = bibliotheque.manifest.id.clone();
            assert_eq!(&id, attendu);
            assert!(
                crate::audio::natifs_tiers::identifiant_admissible(&id),
                "le greffon de test doit porter un identifiant tiers, pas {id}"
            );
            tune_plugin_native::register(bibliotheque).unwrap();
            id
        })
        .collect();
    (enregistres, dossier)
}
