//! Ce que le greffon demande à l'hôte pour MESURER une zone : lire son
//! volume, le forcer à une valeur connue et le restaurer, poser le balayage
//! en file et le jouer, arrêter la zone, dire quels traitements sont actifs.
//!
//! Un trait SYNCHRONE : la mesure se déroule sur un fil bloquant (analyse,
//! attentes de capture). L'implémentation de production porte une poignée
//! du runtime et y bloque ; les essais fournissent un hôte simulé qui note
//! les appels.

use std::sync::Arc;

use tune_core::db::backend::DbBackend;
use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::orchestrator::{PlayRequest, PlaybackOrchestrator};
use tune_core::playback::PlaybackManager;
use tune_core::source_pcm::FormatPcm;

use crate::fournisseur::SOURCE;

/// L'unique élément de file d'une mesure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElementBalayage {
    /// L'identifiant de la mesure : c'est le `source_id` de la ligne.
    pub mesure_id: String,
    pub format: FormatPcm,
    pub duree_ms: u64,
}

impl ElementBalayage {
    pub fn titre(&self) -> String {
        format!("Calibration — balayage de mesure {}", self.mesure_id)
    }
}

/// Les traitements de la zone qui colorent la mesure s'ils restent actifs.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct TraitementsDeZone {
    pub egaliseur: bool,
    pub correction_ir: bool,
    pub pure: bool,
    pub volume_fixe: bool,
}

pub trait HoteZone: Send + Sync {
    /// Le volume de la zone, en pour cent (0–100), tel que la base le tient.
    /// `None` : zone inconnue — la mesure est alors refusée.
    fn volume(&self, zone_id: i64) -> Option<f64>;
    /// Règle le volume (0–100).
    fn regler_volume(&self, zone_id: i64, volume_pct: f64) -> Result<(), String>;
    /// Remplace la file de la zone par le balayage et le joue.
    fn jouer(&self, zone_id: i64, element: ElementBalayage) -> Result<(), String>;
    /// Arrête la zone, comme le bouton Stop.
    fn arreter(&self, zone_id: i64);
    fn traitements(&self, zone_id: i64) -> TraitementsDeZone;
}

pub struct HoteOrchestrateur {
    pub backend: Arc<dyn DbBackend>,
    pub orchestrator: Arc<PlaybackOrchestrator>,
    pub playback: Arc<PlaybackManager>,
    /// Le runtime de l'hôte : les appels bloquent dessus depuis le fil de
    /// mesure, qui n'est pas un fil du runtime.
    pub runtime: tokio::runtime::Handle,
}

impl HoteZone for HoteOrchestrateur {
    fn volume(&self, zone_id: i64) -> Option<f64> {
        ZoneRepo::with_backend(self.backend.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .map(|z| z.volume)
    }

    fn regler_volume(&self, zone_id: i64, volume_pct: f64) -> Result<(), String> {
        let v = (volume_pct / 100.0).clamp(0.0, 1.0);
        let device_id = ZoneRepo::with_backend(self.backend.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id);
        self.runtime
            .block_on(
                self.orchestrator
                    .set_volume(zone_id, v, device_id.as_deref()),
            )
            .map_err(|e| format!("{e:?}"))?;
        ZoneRepo::with_backend(self.backend.clone()).update_volume(zone_id, volume_pct)
    }

    fn jouer(&self, zone_id: i64, e: ElementBalayage) -> Result<(), String> {
        let titre = e.titre();
        let ligne = (
            e.mesure_id.clone(),
            titre.clone(),
            String::new(),
            None,
            None,
            e.duree_ms as i64,
            Some(SOURCE.to_string()),
            None,
            None,
        );
        PlayQueueRepo::with_backend(self.backend.clone()).set_streaming_queue(zone_id, &[ligne])?;
        let output_device_id = ZoneRepo::with_backend(self.backend.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id);
        let resultat = self.runtime.block_on(async {
            self.playback.update_queue_info(zone_id, 0, 1).await;
            let r = self
                .orchestrator
                .play(PlayRequest {
                    zone_id,
                    output_device_id,
                    track_id: None,
                    source: Some(SOURCE.into()),
                    source_id: Some(e.mesure_id.clone()),
                    title: Some(titre),
                    artist_name: None,
                    album_title: None,
                    cover_url: None,
                    duration_ms: Some(e.duree_ms as i64),
                    seek_ms: None,
                    temp_file_path: None,
                    sample_rate: Some(e.format.frequence),
                    bit_depth: Some(e.format.bits),
                    media_format: Some("wav".into()),
                    track_number: None,
                    disc_number: None,
                    album_ref: None,
                })
                .await;
            self.playback.update_queue_info(zone_id, 0, 1).await;
            r
        })?;
        match resultat.error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn arreter(&self, zone_id: i64) {
        self.runtime.block_on(self.orchestrator.stop(zone_id, None));
    }

    fn traitements(&self, zone_id: i64) -> TraitementsDeZone {
        let settings =
            tune_core::db::settings_repo::SettingsRepo::with_backend(self.backend.clone());
        let lit = |cle: String| settings.get(&cle).ok().flatten().filter(|v| !v.is_empty());
        let egaliseur = lit(format!("zone_{zone_id}_eq_profile"))
            .and_then(|v| serde_json::from_str::<serde_json::Value>(&v).ok())
            .is_some_and(|v| v.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false));
        let correction_ir = lit(format!("ir_path_{zone_id}")).is_some();
        let pure = tune_core::audio::audiophile::zone_enabled(&self.backend, zone_id);
        let volume_fixe = ZoneRepo::with_backend(self.backend.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .is_some_and(|z| z.fixed_volume);
        TraitementsDeZone {
            egaliseur,
            correction_ir,
            pure,
            volume_fixe,
        }
    }
}
