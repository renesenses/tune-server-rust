//! rc14 — suréchantillonnage vers le DSD sur la SORTIE LOCALE : la chaîne
//! `fixed.upsampling = {mode: "dsd"}` d'une zone produit un vrai flux
//! DSD64/128/256 par le modulateur sigma-delta de T11b
//! (`tune_plugin_audio_support::modulateur_dsd`).
//!
//! # Le branchement
//!
//! ```text
//! fichier PCM → décodage (entiers 32 bits) → greffons de la zone (f64, une
//! quantification 32 bits, ~−186 dBFS) → f64 → ModulateurDsd (T11b)
//!   → DoP (WAV 24 bits, marqueurs 0x05/0xFA)  ┐ session du streamer,
//!   → DSD brut (`audio::dsd_brut`, MSB-first) ┘ sortie locale inchangée
//! ```
//!
//! La sortie locale reçoit EXACTEMENT ce qu'elle reçoit pour un fichier DSD :
//! un porteur DoP qu'elle reconnaît sur les octets (`is_dop_pcm` : DSP
//! contourné, volume épinglé à l'unité), ou le flux DSD brut de son bras natif
//! (ASIO `DsdU8`, ALSA `DSD_U32`). Aucun chemin de sortie n'est inventé ici.
//!
//! # Le volume
//!
//! Le volume logiciel ne peut pas s'appliquer à un flux DSD à la sortie, et
//! le producteur tourne plusieurs secondes en avance sur l'écoute : un volume
//! « juste avant le modulateur » répondrait avec ce retard. La règle retenue
//! est celle de la protection DSD existante (#8813,
//! `zone_{id}_dsd_volume_protection`) : si la zone demande une atténuation
//! logicielle (volume sous 100 %, sourdine, trim négatif), la piste part en
//! PCM, motif [`MotifDsdRefuse::SoftwareVolumeActive`]. Le ReplayGain et le
//! préampli de l'égaliseur, eux, sont appliqués dans le flux, avant le
//! modulateur : ils ne déclenchent pas la protection.
//!
//! # Refus ou repli
//!
//! Au `PUT`, une cible que la sortie locale ne prend pas est REFUSÉE (409
//! `dsd_not_supported_by_output`) : la chaîne enregistrée ne change pas, donc
//! le suréchantillonnage PCM en place n'est jamais éteint par un choix DSD
//! impossible. À la lecture, ce qui ne se sait qu'avec la piste (cadence non
//! modulable, volume, machine) retombe en PCM, et le chemin du signal le dit
//! ([`EtatModulation::Repli`]). Jamais de silence, jamais de DoP vers un DAC
//! que la zone n'a pas déclaré DSD (`dsd_mode` à `dop` ou `native`).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tune_plugin_audio_support::modulateur_dsd::{CadenceDsd, ErreurModulateur, ModulateurDsd};

use crate::audio::dsd_to_dop::DsdToDoP;

use super::chaine::TauxDsd;

/// Ordre de la boucle sigma-delta publiée au chemin du signal.
pub const ORDRE_DU_MODULATEUR: usize = tune_plugin_audio_support::modulateur_dsd::ORDRE;

/// Plafond DoP d'une sortie locale (#8789) : le DSD256 des deux familles.
pub const PLAFOND_DOP_44K1: u32 = 705_600;
pub const PLAFOND_DOP_48K: u32 = 768_000;

/// Marge de temps réel exigée de la machine, pour deux voies : le modulateur
/// doit tourner au moins deux fois plus vite que l'écoute (décodage, greffons
/// et le reste du serveur se partagent le reste).
pub const MARGE_TEMPS_REEL: f64 = 2.0;

/// Le transport d'un flux modulé vers la sortie locale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportModulation {
    /// DSD natif : ASIO `DsdU8` ou ALSA `DSD_U32`, octets DSD tels quels.
    Native,
    /// DSD over PCM : 16 bits DSD par mot de 24 bits, marqueur alterné.
    Dop,
}

/// Pourquoi une cible DSD n'est pas servie. Codes stables du contrat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MotifDsdRefuse {
    /// La zone ne sort pas par une sortie locale (les sorties réseau gardent
    /// `not_applied_on_transport`).
    OutputNotLocal,
    /// Le réglage DSD de la zone (`dsd_mode`) est `auto` ou `pcm` : la zone
    /// n'a pas déclaré un DAC qui prend le DSD. Rien n'est envoyé en DoP à un
    /// DAC qui le jouerait comme du PCM (bruit pleine échelle).
    DsdModeNotEnabled,
    /// Ni natif, et le DoP dépasserait le plafond de cadence de la zone (ou
    /// le plafond DoP local du DSD256).
    RateAboveZoneMax,
    /// La sortie a refusé un porteur DoP à cette cadence (sonde WASAPI).
    DopRefusedByOutput,
    /// La machine ne tient pas cette cadence avec la marge exigée.
    MachineTooSlow,
    /// À la lecture : la cadence de la piste n'est pas 44,1 ou 48 kHz × 2ⁿ
    /// avec un rapport d'au moins 8 vers la cible.
    SourceRateNotModulable,
    /// À la lecture : nombre de voies que le modulateur ou la sortie ne prend
    /// pas.
    ChannelsNotSupported,
    /// À la lecture : la protection du niveau DSD (#8813) — une atténuation
    /// logicielle est demandée, le DSD la perdrait.
    SoftwareVolumeActive,
    /// À la lecture : construction du modulateur impossible.
    ModulatorFailed,
    #[serde(other)]
    Unknown,
}

impl MotifDsdRefuse {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OutputNotLocal => "output_not_local",
            Self::DsdModeNotEnabled => "dsd_mode_not_enabled",
            Self::RateAboveZoneMax => "rate_above_zone_max",
            Self::DopRefusedByOutput => "dop_refused_by_output",
            Self::MachineTooSlow => "machine_too_slow",
            Self::SourceRateNotModulable => "source_rate_not_modulable",
            Self::ChannelsNotSupported => "channels_not_supported",
            Self::SoftwareVolumeActive => "software_volume_active",
            Self::ModulatorFailed => "modulator_failed",
            Self::Unknown => "unknown",
        }
    }

    /// Tous les motifs, pour le schéma.
    pub const TOUS: [Self; 9] = [
        Self::OutputNotLocal,
        Self::DsdModeNotEnabled,
        Self::RateAboveZoneMax,
        Self::DopRefusedByOutput,
        Self::MachineTooSlow,
        Self::SourceRateNotModulable,
        Self::ChannelsNotSupported,
        Self::SoftwareVolumeActive,
        Self::ModulatorFailed,
    ];
}

/// La cadence du modulateur pour un taux du contrat.
pub fn cadence_du_taux(taux: TauxDsd) -> Option<CadenceDsd> {
    match taux {
        TauxDsd::Dsd64 => Some(CadenceDsd::Dsd64),
        TauxDsd::Dsd128 => Some(CadenceDsd::Dsd128),
        TauxDsd::Dsd256 => Some(CadenceDsd::Dsd256),
        TauxDsd::Unknown => None,
    }
}

/// Cadence DSD (Hz) d'un taux dans la famille 44,1 kHz ou 48 kHz.
pub fn cadence_hz(taux: TauxDsd, famille_48k: bool) -> Option<u32> {
    let c = cadence_du_taux(taux)?;
    let base = if famille_48k { 48_000 } else { 44_100 };
    Some(base * c.multiple())
}

/// Ce que l'hôte sait de la sortie de la zone, pour décider (fonctions pures
/// ci-dessous). Les deux fermetures portent les sondes de l'orchestrateur :
///
/// - `natif(cadence_dsd, voies)` : `Ok` si la sortie part en DSD natif à
///   cette cadence (règle de `decider_dsd_natif_local_voies`), sinon le
///   libellé du motif natif (publié à titre d'information) ;
/// - `dop_refuse(cadence_dop, voies)` : la sortie a-t-elle REFUSÉ un porteur
///   DoP à cette cadence (réponse connue d'une sonde) ?
pub struct FaitsSortieDsd<'a> {
    pub sortie_locale: bool,
    /// `zones.dsd_mode` : `auto`, `native`, `dop`, `pcm`…
    pub dsd_mode: &'a str,
    /// Plafond de cadence de la zone, catalogue compris (`zone_max_sample_rate`).
    pub plafond_zone_hz: Option<u32>,
    pub natif: &'a dyn Fn(u32, u16) -> Result<(), &'static str>,
    pub dop_refuse: &'a dyn Fn(u32, u16) -> bool,
    /// Facteurs de temps réel mesurés (stéréo) pour DSD64, 128, 256 ; `None`
    /// tant que la machine n'est pas mesurée (rien n'est alors refusé pour ce
    /// motif : la lecture le mesure avant de lancer).
    pub temps_reel: Option<[f64; 3]>,
}

fn rang(taux: TauxDsd) -> Option<usize> {
    match taux {
        TauxDsd::Dsd64 => Some(0),
        TauxDsd::Dsd128 => Some(1),
        TauxDsd::Dsd256 => Some(2),
        TauxDsd::Unknown => None,
    }
}

/// La machine tient-elle `taux` sur `voies` voies avec la marge ?
pub fn machine_tient(temps_reel: Option<[f64; 3]>, taux: TauxDsd, voies: u16) -> bool {
    let (Some(mesure), Some(i)) = (temps_reel, rang(taux)) else {
        return true;
    };
    let exige = MARGE_TEMPS_REEL * f64::from(voies.max(1)) / 2.0;
    mesure[i] >= exige
}

/// Décision d'UNE cadence DSD (Hz) sur `voies` voies : le transport, ou le
/// motif du refus. Pure.
pub fn decider_cadence(
    faits: &FaitsSortieDsd<'_>,
    taux: TauxDsd,
    cadence_dsd: u32,
    voies: u16,
) -> Result<TransportModulation, MotifDsdRefuse> {
    if !faits.sortie_locale {
        return Err(MotifDsdRefuse::OutputNotLocal);
    }
    if !matches!(faits.dsd_mode, "native" | "dop") {
        return Err(MotifDsdRefuse::DsdModeNotEnabled);
    }
    if voies == 0 || voies > 8 {
        return Err(MotifDsdRefuse::ChannelsNotSupported);
    }
    if !machine_tient(faits.temps_reel, taux, voies) {
        return Err(MotifDsdRefuse::MachineTooSlow);
    }
    if faits.dsd_mode == "native" && (faits.natif)(cadence_dsd, voies).is_ok() {
        return Ok(TransportModulation::Native);
    }
    let cadence_dop = DsdToDoP::dop_rate(cadence_dsd);
    let plafond_dop = if cadence_dsd.is_multiple_of(48_000) {
        PLAFOND_DOP_48K
    } else {
        PLAFOND_DOP_44K1
    };
    let plafond = faits
        .plafond_zone_hz
        .map_or(plafond_dop, |p| p.min(plafond_dop));
    if cadence_dop > plafond {
        return Err(MotifDsdRefuse::RateAboveZoneMax);
    }
    if (faits.dop_refuse)(cadence_dop, voies) {
        return Err(MotifDsdRefuse::DopRefusedByOutput);
    }
    Ok(TransportModulation::Dop)
}

/// Une famille de cadences d'une cible, publiée.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapaciteFamille {
    pub rate_hz: u32,
    pub accepted: bool,
    /// `native` ou `dop` quand la cible est acceptée.
    pub transport: Option<TransportModulation>,
    /// Cadence du porteur DoP (Hz), quand le transport est DoP.
    pub dop_rate_hz: Option<u32>,
    pub reason: Option<MotifDsdRefuse>,
    /// Zone « natif » servie en DoP : pourquoi pas en natif (libellé de
    /// `MotifDsdServiEnDop`, informatif). `None` ailleurs.
    pub native_reason: Option<String>,
}

/// Une cible DSD et ce que la sortie en fait. `accepted`, `transport`,
/// `reason` décrivent la famille 44,1 kHz (DSD64 = 2,8224 MHz), celle que
/// l'écran grise ; `family_48k` la famille 48 kHz (une piste 48/96/192 kHz y
/// est modulée, faute de rapport entier vers 44,1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapaciteCible {
    pub rate: TauxDsd,
    #[serde(flatten)]
    pub famille_44k1: CapaciteFamille,
    pub family_48k: CapaciteFamille,
}

/// Les capacités DSD publiées dans `GET/PUT /zones/{id}/tess`
/// (`dsd_capabilities`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacitesDsd {
    /// Le réglage DSD de la zone tel que lu (`auto`, `native`, `dop`, `pcm`).
    pub dsd_mode: String,
    pub output_local: bool,
    /// Ordre de la boucle sigma-delta (T11b).
    pub modulator_order: usize,
    /// Le volume logiciel n'est pas appliqué en DSD : la protection du niveau
    /// (`software_volume_active`) fait partir en PCM une piste dont la zone
    /// demande une atténuation logicielle.
    pub volume_protection: bool,
    pub targets: Vec<CapaciteCible>,
}

impl CapacitesDsd {
    pub fn cible(&self, taux: TauxDsd) -> Option<&CapaciteCible> {
        self.targets.iter().find(|c| c.rate == taux)
    }
}

fn famille(faits: &FaitsSortieDsd<'_>, taux: TauxDsd, famille_48k: bool) -> CapaciteFamille {
    let rate_hz = cadence_hz(taux, famille_48k).unwrap_or(0);
    let decision = decider_cadence(faits, taux, rate_hz, 2);
    let native_reason = match decision {
        Ok(TransportModulation::Dop) if faits.dsd_mode == "native" => {
            (faits.natif)(rate_hz, 2).err().map(str::to_owned)
        }
        _ => None,
    };
    CapaciteFamille {
        rate_hz,
        accepted: decision.is_ok(),
        transport: decision.ok(),
        dop_rate_hz: (decision == Ok(TransportModulation::Dop))
            .then(|| DsdToDoP::dop_rate(rate_hz)),
        reason: decision.err(),
        native_reason,
    }
}

/// Les capacités des trois cibles, en stéréo. Pure.
pub fn capacites(faits: &FaitsSortieDsd<'_>, volume_protection: bool) -> CapacitesDsd {
    CapacitesDsd {
        dsd_mode: faits.dsd_mode.to_owned(),
        output_local: faits.sortie_locale,
        modulator_order: ORDRE_DU_MODULATEUR,
        volume_protection,
        targets: [TauxDsd::Dsd64, TauxDsd::Dsd128, TauxDsd::Dsd256]
            .into_iter()
            .map(|rate| CapaciteCible {
                rate,
                famille_44k1: famille(faits, rate, false),
                family_48k: famille(faits, rate, true),
            })
            .collect(),
    }
}

/// Le plan d'une piste : cadences et transport retenus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanModulation {
    pub taux: TauxDsd,
    pub cadence_pcm: u32,
    pub cadence_dsd: u32,
    pub voies: u16,
    pub transport: TransportModulation,
}

impl PlanModulation {
    /// Cadence du flux qui part vers la sortie : celle du DoP, ou la cadence
    /// DSD elle-même en natif.
    pub fn cadence_du_porteur(&self) -> u32 {
        match self.transport {
            TransportModulation::Dop => DsdToDoP::dop_rate(self.cadence_dsd),
            TransportModulation::Native => self.cadence_dsd,
        }
    }
}

/// Décision d'une PISTE PCM à `cadence_pcm` Hz et `voies` voies vers `taux`.
/// Pure.
pub fn planifier(
    faits: &FaitsSortieDsd<'_>,
    taux: TauxDsd,
    cadence_pcm: u32,
    voies: u16,
) -> Result<PlanModulation, MotifDsdRefuse> {
    let cadence = cadence_du_taux(taux).ok_or(MotifDsdRefuse::Unknown)?;
    let cadence_dsd = cadence
        .cadence_pour(cadence_pcm)
        .ok_or(MotifDsdRefuse::SourceRateNotModulable)?;
    let transport = decider_cadence(faits, taux, cadence_dsd, voies)?;
    Ok(PlanModulation {
        taux,
        cadence_pcm,
        cadence_dsd,
        voies,
        transport,
    })
}

// ── Producteur ──────────────────────────────────────────────────────

/// PCM `f64` entrelacé → octets du porteur (DoP 24 bits LE ou DSD brut
/// MSB-first). Hors du fil audio : il tourne sur le fil du producteur de la
/// session, et seul [`ModulateurDsd::traiter`] est garanti sans allocation.
pub struct ProducteurDsd {
    plan: PlanModulation,
    modulateur: ModulateurDsd,
    dop: Option<DsdToDoP>,
    dsd: Vec<u8>,
}

impl ProducteurDsd {
    /// HORS TEMPS RÉEL (conception du NTF et des filtres).
    pub fn new(plan: PlanModulation) -> Result<Self, ErreurModulateur> {
        let cadence = cadence_du_taux(plan.taux).ok_or(ErreurModulateur::Conception)?;
        let modulateur = ModulateurDsd::new(plan.cadence_pcm, plan.voies, cadence)?;
        let dop = (plan.transport == TransportModulation::Dop)
            .then(|| DsdToDoP::new(usize::from(plan.voies), false));
        Ok(Self {
            plan,
            modulateur,
            dop,
            dsd: Vec::new(),
        })
    }

    pub fn plan(&self) -> PlanModulation {
        self.plan
    }

    /// Le premier bloc de la session : l'en-tête WAV 24 bits du porteur DoP
    /// (durée annoncée comme le DoP d'un fichier), ou l'en-tête du flux DSD
    /// brut (`audio::dsd_brut::entete`).
    pub fn entete(&self, duree_ms: Option<u64>) -> Vec<u8> {
        match self.plan.transport {
            TransportModulation::Dop => crate::audio::wav::build_wav_header_with_duration(
                self.plan.voies,
                self.plan.cadence_du_porteur(),
                24,
                duree_ms,
            )
            .to_vec(),
            TransportModulation::Native => {
                crate::audio::dsd_brut::entete(self.plan.cadence_dsd, self.plan.voies).to_vec()
            }
        }
    }

    /// Module `pcm` (trames entières, pleine échelle ±1,0) et AJOUTE le
    /// porteur à `sortie`.
    pub fn moduler(&mut self, pcm: &[f64], sortie: &mut Vec<u8>) -> Result<(), ErreurModulateur> {
        let canaux = usize::from(self.plan.voies);
        let trames = pcm.len() / canaux.max(1);
        let besoin = self.modulateur.octets_pour(trames);
        self.dsd.resize(besoin, 0);
        let n = self
            .modulateur
            .traiter(&pcm[..trames * canaux], &mut self.dsd)?;
        match self.dop.as_mut() {
            Some(dop) => sortie.extend_from_slice(&dop.feed(&self.dsd[..n])),
            None => sortie.extend_from_slice(&self.dsd[..n]),
        }
        Ok(())
    }

    /// Fin de piste : vide le retard du suréchantillonneur (quelques
    /// dizaines de trames de silence, environ 1 ms) puis clôt le DoP sur un
    /// nombre pair de trames (#7704).
    pub fn terminer(&mut self, sortie: &mut Vec<u8>) -> Result<(), ErreurModulateur> {
        let rapport = self.modulateur.rapport().max(1) as f64;
        let trames = (self.modulateur.retard_dsd() / rapport).ceil() as usize + 1;
        let silence = vec![0.0; trames * usize::from(self.plan.voies)];
        self.moduler(&silence, sortie)?;
        if let Some(dop) = self.dop.as_mut() {
            sortie.extend_from_slice(&dop.terminer());
        }
        Ok(())
    }

    /// Remises à zéro de la boucle (instabilités) depuis le début.
    pub fn reinitialisations(&self) -> u64 {
        self.modulateur.reinitialisations()
    }

    /// Plus grande entrée du quantificateur vue.
    pub fn entree_max(&self) -> f64 {
        self.modulateur.entree_max()
    }
}

// ── Mesure de la machine ────────────────────────────────────────────

fn mesure_retenue() -> &'static Mutex<Option<[f64; 3]>> {
    static M: OnceLock<Mutex<Option<[f64; 3]>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(None))
}

/// La mesure retenue (facteurs de temps réel stéréo, DSD64/128/256).
pub fn mesure_de_la_machine() -> Option<[f64; 3]> {
    *mesure_retenue().lock().unwrap_or_else(|e| e.into_inner())
}

/// Retient une mesure (production : [`mesurer_la_machine`] ; bancs et tests
/// d'intégration : une valeur imposée).
pub fn retenir_mesure_de_la_machine(facteurs: [f64; 3]) {
    *mesure_retenue().lock().unwrap_or_else(|e| e.into_inner()) = Some(facteurs);
}

/// BLOQUANT (≈ 0,1 s sur un serveur, ≈ 1 s sur un Pi 4) : module 0,25 s de
/// stéréo 44,1 kHz à chaque cadence et retient le facteur de temps réel. Une
/// mesure déjà retenue est rendue sans recalcul.
pub fn mesurer_la_machine() -> [f64; 3] {
    if let Some(m) = mesure_de_la_machine() {
        return m;
    }
    const TRAMES: usize = 11_025;
    let pcm: Vec<f64> = (0..TRAMES)
        .flat_map(|i| {
            let v = 0.1 * (2.0 * std::f64::consts::PI * 1_000.0 * i as f64 / 44_100.0).sin();
            [v, v]
        })
        .collect();
    let mut facteurs = [0.0; 3];
    for (i, c) in [CadenceDsd::Dsd64, CadenceDsd::Dsd128, CadenceDsd::Dsd256]
        .into_iter()
        .enumerate()
    {
        let Ok(mut m) = ModulateurDsd::new(44_100, 2, c) else {
            continue;
        };
        let mut sortie = vec![0u8; m.octets_pour(TRAMES)];
        let debut = std::time::Instant::now();
        let _ = m.traiter(&pcm, &mut sortie);
        let duree = debut.elapsed().as_secs_f64().max(1e-6);
        facteurs[i] = (TRAMES as f64 / 44_100.0) / duree;
    }
    tracing::info!(
        dsd64 = facteurs[0],
        dsd128 = facteurs[1],
        dsd256 = facteurs[2],
        "tess_dsd_machine_mesuree"
    );
    retenir_mesure_de_la_machine(facteurs);
    facteurs
}

// ── Ce qui tourne (chemin du signal) ────────────────────────────────

/// Le modulateur en service sur une piste, publié au chemin du signal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModulationEnService {
    pub rate: TauxDsd,
    pub rate_hz: u32,
    pub transport: TransportModulation,
    pub order: usize,
    /// Cadence du flux qui part (DoP : cadence / 16 ; natif : cadence DSD).
    pub carrier_rate_hz: u32,
    pub source_rate_hz: u32,
    pub channels: u16,
    /// Les greffons de la zone sont-ils appliqués dans le flux (égaliseur,
    /// convolution, crossfeed) ?
    pub plugins_in_stream: bool,
}

/// L'état de la modulation DSD d'une piste.
#[derive(Debug, Clone, PartialEq)]
pub enum EtatModulation {
    EnService(ModulationEnService),
    /// La chaîne demande le DSD ; la piste est partie en PCM, pour ce motif.
    Repli(MotifDsdRefuse),
}

type Registre = HashMap<i64, Vec<(Option<i64>, EtatModulation)>>;

fn registre() -> &'static Mutex<Registre> {
    static R: OnceLock<Mutex<Registre>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// Retient l'état de la piste `track_id` sur la zone (les deux dernières
/// pistes : la piste en cours et la suivante préparée).
pub fn noter(zone_id: i64, track_id: Option<i64>, etat: EtatModulation) {
    let mut r = registre().lock().unwrap_or_else(|e| e.into_inner());
    let v = r.entry(zone_id).or_default();
    v.retain(|(t, _)| *t != track_id);
    v.push((track_id, etat));
    if v.len() > 2 {
        v.remove(0);
    }
}

/// L'état retenu pour la piste qui joue ; `None` : la chaîne ne demande pas
/// le DSD, ou rien n'a été décidé pour cette piste.
pub fn etat_pour(zone_id: i64, track_id: Option<i64>) -> Option<EtatModulation> {
    let r = registre().lock().unwrap_or_else(|e| e.into_inner());
    r.get(&zone_id)?
        .iter()
        .rev()
        .find(|(t, _)| *t == track_id)
        .map(|(_, e)| e.clone())
}

/// Oublie l'état d'une piste (décision sans DSD).
pub fn oublier(zone_id: i64, track_id: Option<i64>) {
    let mut r = registre().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(v) = r.get_mut(&zone_id) {
        v.retain(|(t, _)| *t != track_id);
    }
}

#[cfg(test)]
#[path = "modulation_dsd_tests.rs"]
mod tests;
