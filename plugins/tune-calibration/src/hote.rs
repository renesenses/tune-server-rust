//! Ce que le greffon demande à l'hôte pour MESURER une zone : lire son
//! volume, le forcer à une valeur connue et le restaurer, poser le balayage
//! en file et le jouer, arrêter la zone, dire quels traitements sont actifs.
//!
//! Un trait SYNCHRONE : la mesure se déroule sur un fil bloquant (analyse,
//! attentes de capture). L'implémentation de production porte une poignée
//! du runtime et y bloque ; les essais fournissent un hôte simulé qui note
//! les appels.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tune_core::db::backend::DbBackend;
use tune_core::db::file_technique_3094::{
    FileAvant, instantane, restaurer, retirer_les_lignes_techniques,
};
use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::orchestrator::{PlayRequest, PlaybackOrchestrator};
use tune_core::outputs::identite_de_sortie::ZoneLocale;
use tune_core::playback::PlaybackManager;
use tune_core::room_correction::RoomProfile;
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
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TraitementsDeZone {
    pub egaliseur: bool,
    pub correction_ir: bool,
    pub pure: bool,
    pub volume_fixe: bool,
}

/// Fil 3094 — la zone n'a pas de sortie ACTIVE : son appareil n'est pas
/// enregistré. Le cas de Xavier : deux zones locales pointent vers le même
/// DAC, et celle qu'on mesure est l'orpheline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortieInactive {
    /// `zones.output_device_id` de la zone mesurée (`local:SmartDX1`).
    pub device_id: String,
    /// La zone qui porte cet appareil aujourd'hui, `(id, nom)`, quand une
    /// zone active a enregistré le même identifiant d'endpoint.
    pub porteuse: Option<(i64, String)>,
}

impl SortieInactive {
    /// Le motif stable de la réponse HTTP : le client le traduit.
    pub const MOTIF: &'static str = "zone_sans_sortie_active";

    /// Le nom de l'appareil, sans le préfixe de type.
    pub fn appareil(&self) -> &str {
        self.device_id
            .split_once(':')
            .map_or(self.device_id.as_str(), |(_, nom)| nom)
    }

    /// La phrase du refus, en français comme les autres refus de la mesure.
    pub fn message(&self) -> String {
        match &self.porteuse {
            Some((id, nom)) => format!(
                "Cette zone n'a pas de sortie active : une autre zone utilise déjà cet appareil \
                 (« {} »). Lancez la mesure depuis la zone « {nom} » (n° {id}).",
                self.appareil()
            ),
            None => format!(
                "Cette zone n'a pas de sortie active : l'appareil « {} » n'est pas détecté. \
                 Vérifiez qu'il est branché et allumé, puis relancez la mesure.",
                self.appareil()
            ),
        }
    }
}

/// La zone ACTIVE qui porte l'appareil de `zone_id` : une autre zone visible,
/// au même identifiant d'endpoint (`zones.output_endpoint_id`), dont la sortie
/// est enregistrée. `None` si l'identifiant est inconnu ou si aucune ne
/// convient — jamais de devinette sur le nom.
pub fn zone_porteuse(
    zones: &[ZoneLocale],
    zone_id: i64,
    enregistree: impl Fn(&str) -> bool,
) -> Option<i64> {
    let endpoint = zones
        .iter()
        .find(|z| z.id == zone_id)?
        .output_endpoint_id
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())?;
    zones
        .iter()
        .find(|z| {
            z.id != zone_id
                && !z.masquee
                && z.output_endpoint_id.as_deref().map(str::trim) == Some(endpoint)
                && enregistree(&z.output_device_id)
        })
        .map(|z| z.id)
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
    /// #9066 — le type de sortie de la zone (`local`, `dlna`, `airplay`…),
    /// pour dire clairement qu'une sortie réseau ne se mesure pas. `None` :
    /// inconnu.
    fn type_de_sortie(&self, _zone_id: i64) -> Option<String> {
        None
    }
    /// Fil 3094 — la sortie de la zone est-elle inactive (appareil non
    /// enregistré) ? La mesure est alors refusée AVANT de toucher au volume.
    /// `None` : sortie active, ou inconnu.
    fn sortie_inactive(&self, _zone_id: i64) -> Option<SortieInactive> {
        None
    }
    /// Fil 3094 — la mesure `mesure_id` est finie (réussie, échouée ou
    /// annulée) : son balayage quitte la file, la file d'avant revient, et la
    /// zone n'affiche plus le balayage. Appelé après [`Self::arreter`].
    fn liberer(&self, _zone_id: i64, _mesure_id: &str) {}
    /// Enregistre le profil de pièce de la zone (`room_profile_{zone}`, C2) :
    /// il ne touche pas au son tant qu'on ne l'applique pas par
    /// `POST /room-correction/profiles/{zone}/apply`.
    fn enregistrer_profil_piece(&self, profil: &RoomProfile) -> Result<(), String>;
}

pub struct HoteOrchestrateur {
    pub backend: Arc<dyn DbBackend>,
    pub orchestrator: Arc<PlaybackOrchestrator>,
    pub playback: Arc<PlaybackManager>,
    /// Le runtime de l'hôte : les appels bloquent dessus depuis le fil de
    /// mesure, qui n'est pas un fil du runtime.
    pub runtime: tokio::runtime::Handle,
    /// Fil 3094 — la file de chaque zone avant que la mesure ne la remplace.
    pub files_avant: Mutex<HashMap<i64, FileAvant>>,
}

impl HoteOrchestrateur {
    fn files_avant(&self) -> std::sync::MutexGuard<'_, HashMap<i64, FileAvant>> {
        self.files_avant.lock().unwrap_or_else(|e| e.into_inner())
    }
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
        let file = PlayQueueRepo::with_backend(self.backend.clone());
        // Fil 3094 — la file que la mesure remplace, rendue à la fin.
        match instantane(&file, zone_id) {
            Ok(avant) => {
                self.files_avant().entry(zone_id).or_insert(avant);
            }
            Err(e) => tracing::warn!(zone_id, error = %e, "calibration_file_avant_non_lue"),
        }
        file.set_streaming_queue(zone_id, &[ligne])?;
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

    fn sortie_inactive(&self, zone_id: i64) -> Option<SortieInactive> {
        let zones = ZoneRepo::with_backend(self.backend.clone());
        let zone = zones.get(zone_id).ok().flatten()?;
        // Une zone navigateur n'a pas d'appareil : ce n'est pas ce refus-là.
        if zone.output_type.as_deref() == Some("browser") {
            return None;
        }
        let device_id = zone.output_device_id?;
        let enregistrees: std::collections::HashSet<String> = self
            .runtime
            .block_on(async { self.orchestrator.outputs.lock().await.list() })
            .into_iter()
            .collect();
        if enregistrees.contains(&device_id) {
            return None;
        }
        let porteuse = zones
            .sorties_locales_et_leur_identite()
            .ok()
            .and_then(|locales| zone_porteuse(&locales, zone_id, |d| enregistrees.contains(d)))
            .and_then(|id| {
                let nom = zones.get(id).ok().flatten()?.name;
                Some((id, nom))
            });
        tracing::warn!(
            zone_id,
            device_id = %device_id,
            porteuse = ?porteuse,
            "calibration_zone_sans_sortie_active"
        );
        Some(SortieInactive {
            device_id,
            porteuse,
        })
    }

    fn liberer(&self, zone_id: i64, mesure_id: &str) {
        let file = PlayQueueRepo::with_backend(self.backend.clone());
        let avant = self.files_avant().remove(&zone_id);
        let retrait = retirer_les_lignes_techniques(&file, zone_id, |e| {
            e.source_id.as_deref() == Some(mesure_id)
        });
        let mut curseur = match retrait {
            Ok(r) => r.courant.map(|c| (c, r.longueur)),
            Err(e) => {
                tracing::warn!(zone_id, mesure_id, error = %e, "calibration_balayage_non_retire");
                None
            }
        };
        if let Some(avant) = avant {
            match restaurer(&file, zone_id, &avant) {
                Ok(Some(rendue)) => curseur = Some(rendue),
                Ok(None) => {}
                Err(e) => tracing::warn!(zone_id, error = %e, "calibration_file_non_rendue"),
            }
        }
        let longueur = file.count_all(zone_id).unwrap_or(0);
        let position = curseur.map_or(0, |(c, _)| c);
        self.runtime.block_on(async {
            self.playback
                .update_queue_info(zone_id, position, longueur)
                .await;
            // La zone n'affiche plus le balayage : le client ne le renverra
            // pas au prochain appui sur Lecture.
            let affiche = self.playback.get_state(zone_id).await.now_playing;
            if affiche
                .is_some_and(|np| np.source == SOURCE && np.source_id.as_deref() == Some(mesure_id))
            {
                self.playback.stop_and_clear(zone_id).await;
            }
        });
        tracing::info!(zone_id, mesure_id, longueur, "calibration_file_rendue");
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

    fn type_de_sortie(&self, zone_id: i64) -> Option<String> {
        ZoneRepo::with_backend(self.backend.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_type)
    }

    fn enregistrer_profil_piece(&self, profil: &RoomProfile) -> Result<(), String> {
        tune_core::room_correction::save_profile(&self.backend, profil)
    }
}
