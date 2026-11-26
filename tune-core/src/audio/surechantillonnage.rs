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
    FrequenceFixe,
    Desactive,
    PremiumRequis,
    GreffonNonInstalle,
    Pure,
    TestDeLaMachineAbsent,
}

impl Ecarte {
    pub fn code(self) -> &'static str {
        match self {
            Self::FrequenceFixe => "fixed_rate",
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

/// rc14 — pourquoi une zone RÉSEAU (famille UPnP) ne suréchantillonne pas
/// la piste : codes stables, publiés par TESS (`delivery.upsampling`) et par
/// le chemin du signal.
pub mod refus_reseau {
    /// Le lecteur n'annonce aucune cadence exploitable (`rate=` absent de
    /// son `GetProtocolInfo`, sonde inconcluante, ou type sans sonde).
    pub const CADENCE_INCONNUE: &str = "renderer_rate_unknown";
    /// Le lecteur annonce des cadences, mais aucune n'est un multiple entier
    /// de la source au-dessus d'elle (sous les plafonds du réglage, de la
    /// zone et du test de la machine).
    pub const AUCUNE_CADENCE_PLUS_HAUTE: &str = "renderer_rate_not_higher";
    /// Source DSD : le DoP en UPnP n'est pas dans ce périmètre.
    pub const DSD: &str = "dsd_not_supported_on_network";
    /// PURE : bit-perfect.
    pub const PURE: &str = "pure";
    /// Service (Qobuz, TIDAL…) ou radio : pas encore porté sur UPnP.
    pub const SOURCE_NON_PORTEE: &str = "source_not_supported_on_network";
    /// La piste ne part pas par le fichier refait (flux progressif imposé
    /// par le format : APE, WavPack, piste trop lourde…).
    pub const VOIE_NON_PORTEE: &str = "progressive_stream_not_supported";
    /// Lecteur de la famille UPnP sans sonde de cadences (BluOS,
    /// Chromecast, Squeezebox).
    pub const LECTEUR_SANS_SONDE: &str = "renderer_without_rate_probe";
    /// La piste, une fois suréchantillonnée, serait trop lourde pour le
    /// fichier refait (mémoire de la machine).
    pub const PISTE_TROP_LOURDE: &str = "track_too_heavy_for_upsampling";
    /// Le fichier refait mettrait trop longtemps avant la première note sur
    /// cette machine (voir [`super::delai_estime_du_fichier_s`]) : on ne
    /// suréchantillonne pas plutôt que d'imposer un long silence.
    pub const TROP_LENT_POUR_LE_FICHIER: &str = "upsampling_too_slow_for_file";
    /// Bit-perfect strict : aucune conversion de cadence.
    pub const STRICT: &str = "strict_bitperfect";
}

/// rc14 — la cadence cible d'une zone RÉSEAU : la plus haute que le lecteur
/// ANNONCE (`annoncees`, lues dans son `GetProtocolInfo`), multiple entier de
/// la source, sous le plafond de la zone (`plafond_zone`), les plafonds du
/// réglage (`max_ratio`, `max_rate_hz`) et le test de la machine. Seules les
/// cadences explicitement annoncées comptent : un lecteur qui annonce
/// 192 kHz sans 176,4 kHz ne reçoit pas 176,4 kHz.
pub fn cible_reseau(
    choix: &ChoixSurechantillonnage,
    source_hz: u32,
    annoncees: &[u32],
    plafond_zone: Option<u32>,
) -> Result<u32, &'static str> {
    if annoncees.is_empty() {
        return Err(refus_reseau::CADENCE_INCONNUE);
    }
    let acceptees: Vec<u32> = annoncees
        .iter()
        .copied()
        .filter(|r| plafond_zone.is_none_or(|p| *r <= p))
        .collect();
    choix
        .negocier(source_hz, &acceptees)
        .ok_or(refus_reseau::AUCUNE_CADENCE_PLUS_HAUTE)
}

/// rc14 — le délai au-delà duquel une piste n'est pas suréchantillonnée par
/// le fichier refait : la première note ne doit jamais attendre plus que
/// cela (décision du 26/11, tant que le relais progressif ne porte pas
/// l'étage).
pub const DELAI_MAX_AVANT_LA_PREMIERE_NOTE_S: f64 = 3.0;

/// Ce que coûte le fichier refait autour de l'étage (décodage, encodage FLAC
/// de L fois plus de données, écriture), en multiple du coût de l'étage seul.
/// Mesuré le 26/11 (banc `banc_charge_piste_de_10_minutes_44_1_vers_176_4`,
/// Ryzen 9 9950X) : 7,3 s pour le fichier, 4,5 s pour l'étage seul, soit
/// ×1,6 ; arrondi à ×2 par prudence.
pub const FACTEUR_DU_FICHIER: f64 = 2.0;

/// rc14 — le délai ESTIMÉ avant la première note d'une piste de `duree_s`
/// secondes, suréchantillonnée à `vers` par le fichier refait, sur CETTE
/// machine : le facteur temps réel que le test de la machine a mesuré pour
/// `filtre` au couple de référence (au moins aussi coûteux que `vers`),
/// ramené à `vers`, multiplié par [`FACTEUR_DU_FICHIER`]. `None` : aucune
/// mesure pour ce filtre et cette famille (on ne sait pas : on s'abstient).
pub fn delai_estime_du_fichier_s(
    verdict: &VerdictMachine,
    filtre: &str,
    vers: u32,
    duree_s: f64,
) -> Option<f64> {
    let (de, ref_vers) = greffon::machine_reference_pair(vers)?;
    let facteur = verdict
        .results
        .iter()
        .find(|r| r.filter == filtre && r.from_hz == de && r.to_hz == ref_vers)
        .map(|r| r.realtime_factor)
        .filter(|f| *f > 0.0)?;
    Some(duree_s.max(0.0) / facteur * (f64::from(vers) / f64::from(ref_vers)) * FACTEUR_DU_FICHIER)
}

/// rc14 — le MÊME greffon (une [`Instance`]) en `f64`, pour la chaîne
/// réseau de TESS (`orchestrator::chaine_reseau_tess`) : aucun second
/// moteur, aucun aller-retour par `f32`. La latence du noyau est RETIRÉE en
/// tête ([`Self::latence_trames`]) et la vidange rend exactement ce délai :
/// n trames d'entrée donnent exactement n·L trames, alignées sur la source.
pub struct SurechantillonneurF64 {
    instance: Instance,
    canaux: usize,
    entree: Vec<f64>,
    sortie: Vec<f64>,
    format_entree: AudioFormat,
    format_sortie: AudioFormat,
    reglages: UpsamplerSettings,
}

impl SurechantillonneurF64 {
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
            AudioFormat::new(de, layout, SampleEncoding::F64).map_err(|e| format!("{e:?}"))?;
        let requete =
            RateRequest::new(format_entree, vers, BLOC_MAX).map_err(|e| format!("{e:?}"))?;
        let instance = Instance::new(requete, &reglages.filter).map_err(|e| format!("{e:?}"))?;
        let n = usize::from(canaux);
        Ok(Self {
            format_sortie: requete.output_format(),
            instance,
            canaux: n,
            entree: vec![0.0; BLOC_MAX * n],
            sortie: vec![0.0; requete.max_output_frames() * n],
            format_entree,
            reglages: reglages.clone(),
        })
    }

    pub fn filtre(&self) -> &'static str {
        self.instance.filter_id()
    }

    /// La réserve fixe du filtre, en dB (≤ 0 : le gain n'excède jamais 1).
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

    pub fn rapport(&self) -> usize {
        (self.cadence_sortie() / self.cadence_entree().max(1)) as usize
    }

    pub fn reglages(&self) -> &UpsamplerSettings {
        &self.reglages
    }

    /// Le délai du noyau, en trames de SORTIE : ce que l'appelant retire en
    /// tête du flux, et exactement ce que [`Self::vider`] rend.
    pub fn latence_trames(&self) -> usize {
        self.instance.latency_frames() as usize
    }

    /// Suréchantillonne des trames entrelacées et AJOUTE le rendu à `rendu`.
    pub fn traiter(&mut self, mots: &[f64], rendu: &mut Vec<f64>) {
        let ch = self.canaux;
        for morceau in mots.chunks(BLOC_MAX * ch) {
            let trames = morceau.len() / ch;
            if trames == 0 {
                continue;
            }
            self.entree[..trames * ch].copy_from_slice(&morceau[..trames * ch]);
            let ecrites = {
                let Ok(entree) = AudioBlock::new(
                    self.format_entree,
                    SamplesMut::F64(&mut self.entree[..trames * ch]),
                    BLOC_MAX,
                ) else {
                    continue;
                };
                let capacite = self.sortie.len() / ch;
                let Ok(mut sortie) = AudioBlock::new(
                    self.format_sortie,
                    SamplesMut::F64(&mut self.sortie[..]),
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
                        tracing::warn!(error = ?e, "upsampler_network_block_refused");
                        0
                    }
                }
            };
            rendu.extend_from_slice(&self.sortie[..ecrites * ch]);
        }
    }

    /// La queue (exactement [`Self::latence_trames`]), AJOUTÉE à `rendu` ;
    /// puis l'étage repart à zéro.
    pub fn vider(&mut self, rendu: &mut Vec<f64>) {
        let ch = self.canaux;
        loop {
            let capacite = self.sortie.len() / ch;
            let rapport = {
                let Ok(mut sortie) = AudioBlock::new(
                    self.format_sortie,
                    SamplesMut::F64(&mut self.sortie[..]),
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
    }
}

#[cfg(test)]
#[path = "surechantillonnage_tests.rs"]
mod tests;
