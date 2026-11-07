//! #7614 — le côté HÔTE du greffon de suréchantillonnage
//! (`tune-plugin-surechantillonnage`, étage `RateChanger` du SDK).
//!
//! Ce module tient ce que le greffon ne peut pas savoir lui-même : les
//! réglages de la zone (`zone_{id}_upsampler`), la licence (Premium, comme le
//! crossfeed : décision de Bertrand du 07/11/2026), l'installation du greffon
//! (`plugin_upsampler_installed`), le verdict du test de la machine
//! (`plugin_upsampler_machine_test`, étape I5 de l'étude) et l'étage que le
//! producteur de la sortie locale fait tourner (étape I3).
//!
//! Les garde-fous, dans l'ordre où ils tombent :
//! - **désactivé par défaut** : sans réglage, `enabled = false` ;
//! - **Premium** et greffon installé et actif, sinon rien ;
//! - **PURE** : aucun choix n'est rendu, l'étage n'existe pas ;
//! - **DoP** : écarté à l'ouverture par la sortie (l'amorce le dit) ;
//! - **test de la machine** : sans verdict, ou pour un filtre que la machine
//!   ne tient pas à × 3 le temps réel, rien n'est appliqué ;
//! - **niveau** : réserve fixe de 3 dB (`RESERVE_DB`, moins si la borne L1
//!   du polyphase le permet), puis le limiteur de sécurité du mode Réaliste
//!   de l'égaliseur (`tune_plugin_audio_support::limiteur`, inactif sous
//!   −0,2 dBFS, plafond −0,01 dBFS) : aucun écrêtage, et le niveau reste
//!   proche de la lecture normale (décision de Bertrand du 07/11) ;
//! - **à la piste suivante** : l'étage garde les réglages de son ouverture ;
//!   un changement (activation, désactivation, filtre, PURE) ne prend effet
//!   qu'à la frontière suivante, où la sortie est rouverte.
use crate::db::settings_repo::SettingsRepo;
use serde::{Deserialize, Serialize};
use tune_plugin_sdk::audio::{
    AudioBlock, AudioFormat, BlockContext, ChannelLayout, SampleEncoding,
};
use tune_plugin_sdk::audio::{ResetReason, SamplesMut};
use tune_plugin_sdk::rate::{RateChanger, RateRequest};
/// Le greffon lui-même, pour les routes du serveur.
pub use tune_plugin_surechantillonnage::sdk as greffon;
use tune_plugin_surechantillonnage::sdk::Instance;
pub use tune_plugin_surechantillonnage::sdk::UpsamplerSettings;

/// L'identifiant du greffon dans le catalogue et les drapeaux.
pub const ID: &str = crate::audio::premium_plugins::UPSAMPLER;
/// Le verdict du test de la machine, en JSON.
pub const CLE_TEST_MACHINE: &str = "plugin_upsampler_machine_test";

/// Les cadences de sortie que la sortie locale sonde pour le greffon : les
/// multiples entiers des deux familles, jusqu'à 768 kHz.
pub const CADENCES_CANDIDATES: [u32; 8] = [
    88_200, 176_400, 352_800, 705_600, 96_000, 192_000, 384_000, 768_000,
];

/// Durée de signal par couple et par filtre du test de la machine. Six
/// filtres × quatre couples × deux passes : quelques secondes de calcul.
pub const DUREE_DU_TEST_S: f64 = 1.0;

pub fn cle_de_zone(zone_id: i64) -> String {
    format!("zone_{zone_id}_upsampler")
}

/// Les réglages enregistrés pour la zone, ou les réglages par défaut
/// (désactivé) quand il n'y en a pas ou qu'ils ne se relisent plus.
pub fn reglages_de_la_zone(settings: &SettingsRepo, zone_id: i64) -> UpsamplerSettings {
    settings
        .get(&cle_de_zone(zone_id))
        .ok()
        .flatten()
        .and_then(|brut| serde_json::from_str::<serde_json::Value>(&brut).ok())
        .and_then(|v| greffon::settings(&v).ok())
        .unwrap_or_default()
}

/// Valide puis enregistre. `Err` : réglage refusé par le greffon.
pub fn enregistrer_les_reglages(
    settings: &SettingsRepo,
    zone_id: i64,
    valeur: &serde_json::Value,
) -> Result<UpsamplerSettings, String> {
    let reglages = greffon::settings(valeur).map_err(|e| format!("{e:?}"))?;
    let brut = serde_json::to_string(&reglages).map_err(|e| e.to_string())?;
    settings.set(&cle_de_zone(zone_id), &brut)?;
    Ok(reglages)
}

/// Un résultat du test de la machine, tel qu'il est gardé en base.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MesureMachine {
    pub filter: String,
    pub from_hz: u32,
    pub to_hz: u32,
    pub realtime_factor: f64,
    pub admitted: bool,
}

/// Le verdict du test de la machine (étape I5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerdictMachine {
    pub simd_path: String,
    pub margin: f64,
    pub results: Vec<MesureMachine>,
    /// Secondes depuis l'époque Unix.
    pub measured_at: u64,
}

impl VerdictMachine {
    /// La machine tient-elle `filtre` vers la cadence de sortie `vers` ?
    /// Un couple absent du verdict est refusé.
    pub fn admet(&self, filtre: &str, vers: u32) -> bool {
        let Some((de, ref_vers)) = greffon::machine_reference_pair(vers) else {
            return false;
        };
        self.results
            .iter()
            .any(|r| r.filter == filtre && r.from_hz == de && r.to_hz == ref_vers && r.admitted)
    }

    /// Les filtres admis pour au moins une cadence de chaque famille.
    pub fn filtres_admis(&self) -> Vec<&str> {
        greffon::OFFERED_FILTERS
            .iter()
            .copied()
            .filter(|f| self.results.iter().any(|r| r.filter == *f && r.admitted))
            .collect()
    }
}

/// Mesure la machine. Calcul pur, plusieurs secondes : à appeler HORS de
/// tout fil audio et hors du fil asynchrone (`spawn_blocking`).
pub fn mesurer_la_machine(duree_s: f64) -> VerdictMachine {
    let rapport = greffon::machine_test(duree_s, greffon::MACHINE_MARGIN);
    VerdictMachine {
        simd_path: rapport.chemin_simd.to_string(),
        margin: rapport.marge,
        results: rapport
            .resultats
            .iter()
            .map(|r| MesureMachine {
                filter: r.filtre.to_string(),
                from_hz: r.de,
                to_hz: r.vers,
                realtime_factor: r.facteur_temps_reel,
                admitted: r.admis,
            })
            .collect(),
        measured_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
    }
}

pub fn verdict_enregistre(settings: &SettingsRepo) -> Option<VerdictMachine> {
    settings
        .get(CLE_TEST_MACHINE)
        .ok()
        .flatten()
        .and_then(|brut| serde_json::from_str(&brut).ok())
}

pub fn enregistrer_le_verdict(settings: &SettingsRepo, v: &VerdictMachine) -> Result<(), String> {
    settings.set(
        CLE_TEST_MACHINE,
        &serde_json::to_string(v).map_err(|e| e.to_string())?,
    )
}

/// Ce que l'orchestrateur pose sur la sortie locale avant une lecture.
#[derive(Debug, Clone, PartialEq)]
pub struct ChoixSurechantillonnage {
    pub reglages: UpsamplerSettings,
    pub verdict: VerdictMachine,
}

/// Pourquoi aucun choix n'est rendu : pour le journal et l'écran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ecarte {
    Desactive,
    PremiumRequis,
    GreffonNonInstalle,
    Pure,
    TestDeLaMachineAbsent,
}

impl Ecarte {
    pub fn code(self) -> &'static str {
        match self {
            Self::Desactive => "disabled",
            Self::PremiumRequis => "premium_required",
            Self::GreffonNonInstalle => "plugin_not_installed",
            Self::Pure => "pure",
            Self::TestDeLaMachineAbsent => "machine_test_missing",
        }
    }
}

/// LA décision de la zone, licence comprise. `premium` est l'instantané de
/// la licence (`LicenseManager::premium_snapshot`).
pub fn choix_de_la_zone(
    settings: &SettingsRepo,
    premium: bool,
    pure: bool,
    zone_id: i64,
) -> Result<ChoixSurechantillonnage, Ecarte> {
    let reglages = reglages_de_la_zone(settings, zone_id);
    if !reglages.enabled {
        return Err(Ecarte::Desactive);
    }
    if !premium {
        return Err(Ecarte::PremiumRequis);
    }
    if !crate::audio::premium_plugins::enabled(settings, ID) {
        return Err(Ecarte::GreffonNonInstalle);
    }
    if pure {
        return Err(Ecarte::Pure);
    }
    let verdict = verdict_enregistre(settings).ok_or(Ecarte::TestDeLaMachineAbsent)?;
    Ok(ChoixSurechantillonnage { reglages, verdict })
}

impl ChoixSurechantillonnage {
    /// La cadence de sortie retenue pour une source à `source`, parmi
    /// `acceptees` (MESURÉES par la sortie) : la plus haute que le filtre
    /// choisi tient sur cette machine, multiple entier de la source, sous
    /// les plafonds des réglages. `None` : rien d'utile, la sortie garde son
    /// chemin.
    pub fn negocier(&self, source: u32, acceptees: &[u32]) -> Option<u32> {
        let admises: Vec<u32> = acceptees
            .iter()
            .copied()
            .filter(|&r| self.verdict.admet(&self.reglages.filter, r))
            .collect();
        greffon::choose_rate_for(source, &admises, &self.reglages)
    }
}

/// Taille du plus grand bloc d'entrée passé à l'instance. Les blocs plus
/// longs sont découpés.
const BLOC_MAX: usize = 4096;

/// L'étage que le producteur de la sortie locale fait tourner : entrée et
/// sortie en `f32` entrelacés, au nombre de canaux OUVERT.
pub struct EtageSurechantillonneur {
    instance: Instance,
    canaux: usize,
    rapport: usize,
    entree: Vec<f32>,
    sortie: Vec<f32>,
    format_entree: AudioFormat,
    format_sortie: AudioFormat,
    /// Les réglages de l'ouverture : comparés à ceux de la zone à chaque
    /// frontière de piste, jamais pendant.
    reglages: UpsamplerSettings,
}

impl EtageSurechantillonneur {
    pub fn nouveau(
        reglages: &UpsamplerSettings,
        de: u32,
        vers: u32,
        canaux: u16,
    ) -> Result<Self, String> {
        let layout = match canaux {
            1 => ChannelLayout::Mono,
            2 => ChannelLayout::Stereo,
            n => ChannelLayout::Discrete(n),
        };
        let format_entree =
            AudioFormat::new(de, layout, SampleEncoding::F32).map_err(|e| format!("{e:?}"))?;
        let requete =
            RateRequest::new(format_entree, vers, BLOC_MAX).map_err(|e| format!("{e:?}"))?;
        let instance = Instance::new(requete, &reglages.filter).map_err(|e| format!("{e:?}"))?;
        let rapport = (vers / de) as usize;
        let n = usize::from(canaux);
        Ok(Self {
            format_sortie: requete.output_format(),
            instance,
            canaux: n,
            rapport,
            entree: vec![0.0; BLOC_MAX * n],
            sortie: vec![0.0; requete.max_output_frames() * n],
            format_entree,
            reglages: reglages.clone(),
        })
    }

    pub fn filtre(&self) -> &'static str {
        self.instance.filter_id()
    }

    pub fn reserve_db(&self) -> f64 {
        20.0 * self.instance.gain().log10()
    }

    pub fn cadence_entree(&self) -> u32 {
        self.format_entree.sample_rate()
    }

    pub fn cadence_sortie(&self) -> u32 {
        self.format_sortie.sample_rate()
    }

    pub fn canaux(&self) -> u16 {
        self.canaux as u16
    }

    pub fn reglages(&self) -> &UpsamplerSettings {
        &self.reglages
    }

    /// Suréchantillonne des trames entrelacées. Une trame incomplète en
    /// queue (impossible en amont : l'étage reçoit des trames entières) est
    /// ignorée plutôt que de décaler les voies.
    pub fn traiter(&mut self, mots: &[f32]) -> Vec<f32> {
        let ch = self.canaux;
        let mut rendu = Vec::with_capacity(mots.len() * self.rapport);
        for morceau in mots.chunks(BLOC_MAX * ch) {
            let trames = morceau.len() / ch;
            if trames == 0 {
                continue;
            }
            self.entree[..trames * ch].copy_from_slice(&morceau[..trames * ch]);
            let ecrites = {
                let Ok(entree) = AudioBlock::new(
                    self.format_entree,
                    SamplesMut::F32(&mut self.entree[..trames * ch]),
                    BLOC_MAX,
                ) else {
                    continue;
                };
                let capacite = self.sortie.len() / ch;
                let Ok(mut sortie) = AudioBlock::new(
                    self.format_sortie,
                    SamplesMut::F32(&mut self.sortie[..]),
                    capacite,
                ) else {
                    continue;
                };
                match self.instance.process(
                    &entree,
                    &mut sortie,
                    BlockContext {
                        zone_id: 0,
                        generation: 0,
                        position_frames: 0,
                    },
                ) {
                    Ok(r) => r.frames_written,
                    Err(e) => {
                        tracing::warn!(error = ?e, "upsampler_block_refused");
                        0
                    }
                }
            };
            rendu.extend_from_slice(&self.sortie[..ecrites * ch]);
        }
        rendu
    }

    /// La queue : exactement le délai du noyau, puis l'étage repart à zéro.
    pub fn vider(&mut self) -> Vec<f32> {
        let ch = self.canaux;
        let mut rendu = Vec::new();
        loop {
            let capacite = self.sortie.len() / ch;
            let rapport = {
                let Ok(mut sortie) = AudioBlock::new(
                    self.format_sortie,
                    SamplesMut::F32(&mut self.sortie[..]),
                    capacite,
                ) else {
                    break;
                };
                match self.instance.drain(&mut sortie) {
                    Ok(r) => r,
                    Err(_) => break,
                }
            };
            rendu.extend_from_slice(&self.sortie[..rapport.frames_written * ch]);
            if rapport.complete || rapport.frames_written == 0 {
                break;
            }
        }
        self.instance.reset(ResetReason::Stop);
        rendu
    }
}

#[cfg(test)]
#[path = "surechantillonnage_tests.rs"]
mod tests;
