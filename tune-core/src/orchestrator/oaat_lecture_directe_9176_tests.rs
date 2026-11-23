//! #9176 — la sortie OAAT lit un WAV directement sur le disque
//! (`entete_est_wav`, `play_media`) dès que `PlayMedia::file_path` est là. Sur
//! une zone qui traite le flux (égaliseur, correction de pièce, ReplayGain,
//! crossfeed), le flux traité que le serveur venait de préparer
//! (`oaat_needs_wav` → `load_streaming_dsp`) était donc jeté.
//!
//! Le banc passe par la VRAIE porte de lecture (`PlaybackOrchestrator::play`)
//! sur une zone OAAT (`oaat:` en tête de l'appareil) et note le `PlayMedia`
//! que la sortie reçoit :
//!
//! - traitement actif : `file_path` vaut `None`, la sortie lit `url` ;
//! - aucun traitement : `file_path` désigne le WAV, la lecture directe reste.
//!
//! Contre-épreuve : retirer le filtre `oaat_peut_lire_le_fichier` de
//! `envoyer_a_la_sortie` fait tomber les deux premiers tests.

use super::*;
use std::sync::Arc;

use crate::db::models::Track;
use crate::db::settings_repo::SettingsRepo;
use crate::db::track_repo::TrackRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::outputs::{OutputCapabilities, OutputStatus, OutputTarget, PlayMedia};

const WAV: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/src.wav");

/// Ce que la sortie a reçu : `(url, file_path)`.
type Recu = Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;

struct SortieOaat {
    appareil: String,
    recu: Recu,
}

#[async_trait::async_trait]
impl OutputTarget for SortieOaat {
    fn name(&self) -> &str {
        "Sortie OAAT du banc 9176"
    }
    fn device_id(&self) -> &str {
        &self.appareil
    }
    fn output_type(&self) -> &str {
        "oaat"
    }
    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities::v1(true, true, true, true, true, false)
    }
    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        self.recu
            .lock()
            .unwrap()
            .push((media.url.to_owned(), media.file_path.map(str::to_owned)));
        Ok(())
    }
    async fn pause(&self) -> Result<(), String> {
        Ok(())
    }
    async fn resume(&self) -> Result<(), String> {
        Ok(())
    }
    async fn stop(&self) -> Result<(), String> {
        Ok(())
    }
    async fn seek(&self, _: u64) -> Result<(), String> {
        Ok(())
    }
    async fn set_volume(&self, _: f64) -> Result<(), String> {
        Ok(())
    }
    async fn set_mute(&self, _: bool) -> Result<(), String> {
        Ok(())
    }
    async fn get_status(&self) -> Result<OutputStatus, String> {
        Ok(OutputStatus::default())
    }
    async fn is_available(&self) -> bool {
        true
    }
}

struct Banc {
    orch: PlaybackOrchestrator,
    zone_id: i64,
    track_id: i64,
    appareil: String,
    recu: Recu,
}

async fn banc() -> Banc {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let mut orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    );
    // Le droit `RoomCorrection` : sans lui, la RI ne compte pas.
    orch.license = Some(crate::license::licence_premium_pour_test(db.clone()));
    let appareil = "oaat:banc-9176".to_owned();
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("Zone du banc 9176", Some("oaat"), Some(&appareil))
        .unwrap();
    let recu: Recu = Arc::new(std::sync::Mutex::new(Vec::new()));
    orch.outputs.lock().await.register(Box::new(SortieOaat {
        appareil: appareil.clone(),
        recu: recu.clone(),
    }));
    let mut t = Track::new("Piste du banc 9176".into());
    t.duration_ms = 1_000;
    t.file_path = Some(WAV.into());
    t.format = Some("wav".into());
    t.sample_rate = Some(44_100);
    t.bit_depth = Some(16);
    t.channels = 2;
    t.file_size = std::fs::metadata(WAV).ok().map(|m| m.len() as i64);
    t.source = "local".into();
    let track_id = TrackRepo::with_backend(db).create(&t).unwrap();
    Banc {
        orch,
        zone_id,
        track_id,
        appareil,
        recu,
    }
}

/// Une réponse impulsionnelle WAV float 32 mono à 44,1 kHz (la cadence du
/// flux) : un Dirac de gain 0,5, que le convolveur accepte.
fn ecrire_ri() -> crate::test_scratch::ScratchFile {
    let f = crate::test_scratch::scratch_file("ri-9176-44k", ".wav");
    let (cadence, canaux, taps) = (44_100u32, 1u16, 32usize);
    let donnees = taps * 4 * usize::from(canaux);
    let mut wav = Vec::with_capacity(44 + donnees);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((36 + donnees) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&3u16.to_le_bytes());
    wav.extend_from_slice(&canaux.to_le_bytes());
    wav.extend_from_slice(&cadence.to_le_bytes());
    wav.extend_from_slice(&(cadence * 4 * u32::from(canaux)).to_le_bytes());
    wav.extend_from_slice(&(4 * canaux).to_le_bytes());
    wav.extend_from_slice(&32u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(donnees as u32).to_le_bytes());
    for i in 0..taps {
        let v: f32 = if i == 0 { 0.5 } else { 0.0 };
        for _ in 0..canaux {
            wav.extend_from_slice(&v.to_le_bytes());
        }
    }
    std::fs::write(f.path(), wav).unwrap();
    f
}

impl Banc {
    fn armer_l_egaliseur(&self) {
        let s = SettingsRepo::with_backend(self.orch.db.clone());
        s.set("plugin_equalizer_installed", "true").unwrap();
        let profil = crate::audio::eq::EqProfile {
            enabled: true,
            bands: vec![crate::audio::eq::EqBandSpec {
                freq: 1000.0,
                gain: 12.0,
                q: 1.0,
                band_type: "peak".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        s.set(
            &format!("zone_{}_eq_profile", self.zone_id),
            &serde_json::to_string(&profil).unwrap(),
        )
        .unwrap();
        assert!(
            self.orch.zone_has_active_eq(self.zone_id),
            "prémisse : l'égaliseur est actif sur la zone"
        );
    }

    fn armer_la_correction_de_piece(&self, ri: &crate::test_scratch::ScratchFile) {
        SettingsRepo::with_backend(self.orch.db.clone())
            .set(&format!("ir_path_{}", self.zone_id), ri.as_str().as_ref())
            .unwrap();
        assert!(
            self.orch.zone_has_active_ir(self.zone_id),
            "prémisse : la RI est réglée, sous licence, et le fichier existe"
        );
    }

    /// Joue la piste et rend `(url, file_path)` tels que la sortie les reçoit.
    async fn jouer(&self) -> (String, Option<String>) {
        let r = self
            .orch
            .play(PlayRequest {
                zone_id: self.zone_id,
                output_device_id: Some(self.appareil.clone()),
                track_id: Some(self.track_id),
                duration_ms: Some(1_000),
                media_format: Some("wav".into()),
                sample_rate: Some(44_100),
                bit_depth: Some(16),
                ..Default::default()
            })
            .await;
        assert!(r.is_ok(), "la lecture doit partir : {:?}", r.err());
        self.recu
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("la sortie n'a reçu aucun play_media")
    }
}

#[tokio::test]
async fn oaat_egaliseur_actif_lit_le_flux_traite_9176() {
    let b = banc().await;
    b.armer_l_egaliseur();

    let (url, file_path) = b.jouer().await;

    assert_eq!(
        file_path, None,
        "#9176 : égaliseur actif sur une zone OAAT ; remettre le WAV à la sortie \
         lui fait lire le disque et jeter le flux traité (url = {url})"
    );
    assert!(
        url.contains("/stream/"),
        "la sortie lit le flux servi : {url}"
    );
}

#[tokio::test]
async fn oaat_correction_de_piece_lit_le_flux_traite_9176() {
    let b = banc().await;
    let ri = ecrire_ri();
    b.armer_la_correction_de_piece(&ri);

    let (url, file_path) = b.jouer().await;

    assert_eq!(
        file_path, None,
        "#9176 : correction de pièce active sur une zone OAAT, la sortie doit lire \
         le flux traité (url = {url})"
    );
}

#[tokio::test]
async fn oaat_sans_traitement_garde_la_lecture_directe_9176() {
    let b = banc().await;

    let (_, file_path) = b.jouer().await;

    assert_eq!(
        file_path.as_deref(),
        Some(WAV),
        "#9176 : sans traitement, la lecture directe du WAV reste permise"
    );
}

/// Le DSD natif ne passe par aucun traitement PCM : le `.dsf` (et le `.dff`,
/// dont la sortie constate elle-même le repli) restent remis à la sortie.
#[test]
fn oaat_garde_le_fichier_dsd_meme_avec_un_traitement_9176() {
    use super::transport::oaat_peut_lire_le_fichier as peut;
    assert!(peut("/musique/a.dsf", || true));
    assert!(peut("/musique/a.DFF", || true));
    assert!(!peut("/musique/a.wav", || true));
    assert!(!peut("/musique/a.flac", || true));
    assert!(peut("/musique/a.wav", || false));
}
