//! rc14 — suréchantillonnage vers le DSD sur la SORTIE LOCALE : la chaîne
//! `fixed.upsampling = {mode: "dsd"}` d'une zone produit un vrai flux
//! DSD64/128/256 par le modulateur sigma-delta de T11b
//! (`tune_plugin_audio_support::modulateur_dsd`).
//!
//! # Le branchement
//!
//! ```text
//! fichier PCM → décodage (entiers 32 bits, mono dupliqué en stéréo)
//!   → greffons de la zone (f64, une quantification 32 bits, ~−186 dBFS) → f64
//!   → headroom TESS et préampli gardé (gain fixe) → volume de la zone (rampe
//!     10 ms, Q9) → ModulateurDsd (T11b : 0 dBFS = 50 % de modulation, soit
//!     0 dB SACD ; limiteur à 60 %)
//!   → DoP (WAV 24 bits, marqueurs 0x05/0xFA)  ┐ session du streamer,
//!   → DSD brut (`audio::dsd_brut`, MSB-first) ┘ sortie locale inchangée
//! ```
//!
//! La sortie locale reçoit EXACTEMENT ce qu'elle reçoit pour un fichier DSD :
//! un porteur DoP qu'elle reconnaît sur les octets (`is_dop_pcm` : DSP
//! contourné, volume épinglé à l'unité), ou le flux DSD brut de son bras natif
//! (ASIO `DsdU8`, ALSA `DSD_U32`). Aucun chemin de sortie n'est inventé ici.
//! Le porteur est TOUJOURS stéréo : un DoP mono serait coupé par la sortie,
//! qui ouvre deux voies (`rupture_du_porteur_dop`).
//!
//! # Le volume (décision Q9 du 17/11)
//!
//! Le volume de la zone s'applique JUSTE AVANT le modulateur. Il est publié
//! au changement ([`publier_volume`], valeur atomique) et relu sans verrou à
//! chaque bloc ; un modulateur neuf le pose SANS rampe avant son premier bloc
//! ([`ProducteurDsd::fixer_volume`] : jamais une milliseconde à 0 dB avant
//! une sourdine), les changements en cours passent par la rampe de 10 ms de
//! T11b ([`ProducteurDsd::definir_volume`]). Il répond avec le retard de tout
//! ce qui est déjà modulé en aval : la session (≈ 0,4 s) PLUS l'anneau de la
//! sortie locale (≈ 2 s), soit **2 à 2,5 s** — la « réponse plus lente
//! acceptée » de Q9. Une baisse de volume ou la sourdine ne relancent rien.
//! La protection #8813 reste celle des FICHIERS DSD, qui n'ont pas
//! d'échantillon où appliquer le volume. (Piste non retenue ici : réduire
//! l'anneau de la sortie en mode modulé ramènerait la réponse vers 1 s, au
//! prix d'un risque de famine à mesurer sur Pi.)
//!
//! # Refus ou repli
//!
//! Au `PUT`, une cible que la sortie locale ne prend pas est REFUSÉE (409
//! `dsd_not_supported_by_output`) : la chaîne enregistrée ne change pas, donc
//! le suréchantillonnage PCM en place n'est jamais éteint par un choix DSD
//! impossible. À la lecture, ce qui ne se sait qu'avec la piste (cadence non
//! modulable, voies, machine, porteur refusé par le périphérique) retombe en
//! PCM, et le chemin du signal le dit ([`EtatModulation::Repli`]). Jamais de
//! silence, jamais de DoP vers un DAC que la zone n'a pas déclaré DSD
//! (`dsd_mode` à `dop` ou `native`).
//!
//! Un repli en PCM joue au niveau du PCM : environ +6 dB par rapport au
//! même passage modulé (0 dBFS = 0 dB SACD = 50 % de modulation), +9 dB avec
//! le headroom automatique. Compensation possible, NON appliquée : atténuer
//! de 6 dB (9 dB) au rendu une piste repliée d'une zone vers le DSD.
//!
//! # Enchaînements
//!
//! Entre une piste modulée et sa SUIVANTE de la file (lien explicite, voir
//! [`annoncer_la_suivante`]), de même famille et de même cadence, l'état du
//! modulateur et du suréchantillonneur passe d'une piste à l'autre au moment
//! où la suivante commence vraiment à moduler ([`attendre_le_relais`]) : ni
//! silence de clôture, ni marche. Chaque piste émet un nombre PAIR de trames DoP (la
//! trame impaire attend la piste suivante) : toute piste commence par 0x05.
//! Une piste sans suite perd ce qui restait dans le filtre (≈ 1 ms) et au plus
//! une trame DoP (≈ 6 µs).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tune_plugin_audio_support::modulateur_dsd::{
    CadenceDsd, ErreurModulateur, ModulateurDsd, NIVEAU_REFERENCE,
};

use crate::audio::dsd_to_dop::DsdToDoP;

use super::chaine::TauxDsd;

/// Ordre de la boucle sigma-delta publiée au chemin du signal.
pub const ORDRE_DU_MODULATEUR: usize = tune_plugin_audio_support::modulateur_dsd::ORDRE;

/// Modulation portée par 0 dBFS PCM : 50 %, le « 0 dB SACD » (T11b).
pub const MODULATION_A_0_DBFS: f64 = NIVEAU_REFERENCE;

/// Plafond DoP d'une sortie locale (#8789) : le DSD256 des deux familles.
pub const PLAFOND_DOP_44K1: u32 = 705_600;
pub const PLAFOND_DOP_48K: u32 = 768_000;

/// Voies du porteur : toujours la stéréo (voir l'en-tête).
pub const VOIES_DU_PORTEUR: u16 = 2;

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
    /// La sortie a refusé un porteur DoP à cette cadence (sonde WASAPI
    /// exclusive).
    DopRefusedByOutput,
    /// Le périphérique a déjà été ouvert à une autre cadence que celle du
    /// porteur (rééchantillonnage, qui détruit les marqueurs) : constat retenu.
    CarrierRateNotSupported,
    /// La machine ne tient pas cette cadence avec la marge exigée.
    MachineTooSlow,
    /// À la lecture : la cadence de la piste n'est pas 44,1 ou 48 kHz × 2ⁿ
    /// avec un rapport d'au moins 8 vers la cible.
    SourceRateNotModulable,
    /// À la lecture : plus de deux voies (le porteur est stéréo).
    ChannelsNotSupported,
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
            Self::CarrierRateNotSupported => "carrier_rate_not_supported",
            Self::MachineTooSlow => "machine_too_slow",
            Self::SourceRateNotModulable => "source_rate_not_modulable",
            Self::ChannelsNotSupported => "channels_not_supported",
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
        Self::CarrierRateNotSupported,
        Self::MachineTooSlow,
        Self::SourceRateNotModulable,
        Self::ChannelsNotSupported,
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
/// ci-dessous). Les fermetures portent les sondes de l'orchestrateur :
///
/// - `natif(cadence_dsd, voies)` : `Ok` si la sortie part en DSD natif à
///   cette cadence (règle de `decider_dsd_natif_local_voies`), sinon le
///   libellé du motif natif (publié à titre d'information) ;
/// - `dop_refuse(cadence_dop, voies)` : le motif si la sortie a déjà REFUSÉ
///   un porteur DoP à cette cadence (sonde exclusive, ou rupture constatée à
///   l'ouverture : [`rupture_connue`]).
pub struct FaitsSortieDsd<'a> {
    pub sortie_locale: bool,
    /// `zones.dsd_mode` : `auto`, `native`, `dop`, `pcm`…
    pub dsd_mode: &'a str,
    /// Plafond de cadence de la zone, catalogue compris (`zone_max_sample_rate`).
    pub plafond_zone_hz: Option<u32>,
    pub natif: &'a dyn Fn(u32, u16) -> Result<(), &'static str>,
    pub dop_refuse: &'a dyn Fn(u32, u16) -> Option<MotifDsdRefuse>,
    /// Facteurs de temps réel mesurés (stéréo) pour DSD64, 128, 256 ; `None`
    /// tant que la machine n'est pas mesurée (rien n'est alors refusé pour ce
    /// motif : la lecture la mesure avant de lancer).
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

/// La machine tient-elle `taux` en stéréo avec la marge ?
pub fn machine_tient(temps_reel: Option<[f64; 3]>, taux: TauxDsd) -> bool {
    let (Some(mesure), Some(i)) = (temps_reel, rang(taux)) else {
        return true;
    };
    mesure[i] >= MARGE_TEMPS_REEL
}

/// Décision d'UNE cadence DSD (Hz) pour le porteur stéréo : le transport, ou
/// le motif du refus. Pure.
pub fn decider_cadence(
    faits: &FaitsSortieDsd<'_>,
    taux: TauxDsd,
    cadence_dsd: u32,
) -> Result<TransportModulation, MotifDsdRefuse> {
    let voies = VOIES_DU_PORTEUR;
    if !faits.sortie_locale {
        return Err(MotifDsdRefuse::OutputNotLocal);
    }
    if !matches!(faits.dsd_mode, "native" | "dop") {
        return Err(MotifDsdRefuse::DsdModeNotEnabled);
    }
    if !machine_tient(faits.temps_reel, taux) {
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
    if let Some(motif) = (faits.dop_refuse)(cadence_dop, voies) {
        return Err(motif);
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
    /// Niveau de référence : 0 dBFS PCM = 50 % de modulation (0 dB SACD).
    pub reference_level: String,
    /// Le volume de la zone s'applique juste avant le modulateur (Q9).
    pub volume_placement: String,
    /// La machine a-t-elle été mesurée ? Tant que non, `machine_too_slow`
    /// n'est jamais rendu (la lecture mesure avant de lancer).
    pub machine_measured: bool,
    /// `known` ; `unknown` quand la sortie n'a pas encore pu être lue (nom du
    /// périphérique jamais vu, sortie occupée) : `targets` est alors VIDE,
    /// plutôt qu'une réponse fausse, et le `PUT` ne refuse rien.
    pub status: String,
    pub targets: Vec<CapaciteCible>,
}

impl CapacitesDsd {
    pub fn cible(&self, taux: TauxDsd) -> Option<&CapaciteCible> {
        self.targets.iter().find(|c| c.rate == taux)
    }
}

fn famille(faits: &FaitsSortieDsd<'_>, taux: TauxDsd, famille_48k: bool) -> CapaciteFamille {
    let rate_hz = cadence_hz(taux, famille_48k).unwrap_or(0);
    let decision = decider_cadence(faits, taux, rate_hz);
    let native_reason = match decision {
        Ok(TransportModulation::Dop) if faits.dsd_mode == "native" => {
            (faits.natif)(rate_hz, VOIES_DU_PORTEUR)
                .err()
                .map(str::to_owned)
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

/// Les capacités des trois cibles. Pure. `connues` faux : la sortie n'a pas
/// pu être lue, aucune cible n'est publiée (`status: "unknown"`).
pub fn capacites(faits: &FaitsSortieDsd<'_>, connues: bool) -> CapacitesDsd {
    CapacitesDsd {
        dsd_mode: faits.dsd_mode.to_owned(),
        output_local: faits.sortie_locale,
        modulator_order: ORDRE_DU_MODULATEUR,
        reference_level: "0 dBFS = 50 % modulation (0 dB SACD)".into(),
        volume_placement: "before_dsd_modulator".into(),
        machine_measured: faits.temps_reel.is_some(),
        status: if connues { "known" } else { "unknown" }.into(),
        targets: [TauxDsd::Dsd64, TauxDsd::Dsd128, TauxDsd::Dsd256]
            .into_iter()
            .filter(|_| connues)
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
    /// Voies de la source (1 ou 2).
    pub voies_source: u16,
    /// Voies du porteur : toujours [`VOIES_DU_PORTEUR`].
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

    /// Deux plans dont le flux peut continuer sans rupture.
    pub fn enchaine_sur(&self, autre: &Self) -> bool {
        self.cadence_pcm == autre.cadence_pcm
            && self.cadence_dsd == autre.cadence_dsd
            && self.voies == autre.voies
            && self.transport == autre.transport
    }
}

/// Décision d'une PISTE PCM à `cadence_pcm` Hz et `voies_source` voies vers
/// `taux`. Une source mono est portée en stéréo (voie dupliquée) ; plus de
/// deux voies repartent en PCM. Pure.
pub fn planifier(
    faits: &FaitsSortieDsd<'_>,
    taux: TauxDsd,
    cadence_pcm: u32,
    voies_source: u16,
) -> Result<PlanModulation, MotifDsdRefuse> {
    if !(1..=2).contains(&voies_source) {
        return Err(MotifDsdRefuse::ChannelsNotSupported);
    }
    let cadence = cadence_du_taux(taux).ok_or(MotifDsdRefuse::Unknown)?;
    let cadence_dsd = cadence
        .cadence_pour(cadence_pcm)
        .ok_or(MotifDsdRefuse::SourceRateNotModulable)?;
    let transport = decider_cadence(faits, taux, cadence_dsd)?;
    Ok(PlanModulation {
        taux,
        cadence_pcm,
        cadence_dsd,
        voies_source,
        voies: VOIES_DU_PORTEUR,
        transport,
    })
}

// ── Producteur ──────────────────────────────────────────────────────

/// PCM `f64` stéréo entrelacé → octets du porteur (DoP 24 bits LE ou DSD
/// brut MSB-first). Hors du fil audio : il tourne sur le fil du producteur de
/// la session ; seul [`ModulateurDsd::traiter`] est garanti sans allocation.
pub struct ProducteurDsd {
    plan: PlanModulation,
    modulateur: ModulateurDsd,
    dop: Option<DsdToDoP>,
    dsd: Vec<u8>,
    /// DoP : octets DSD d'une trame DoP impaire, qui attendent la suivante
    /// (chaque piste émet un nombre pair de trames).
    en_attente: Vec<u8>,
    /// Gain fixe avant le volume : headroom TESS, préampli gardé (≤ 1).
    gain_fixe: f64,
    tampon: Vec<f64>,
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
            en_attente: Vec::new(),
            gain_fixe: 1.0,
            tampon: Vec::new(),
        })
    }

    pub fn plan(&self) -> PlanModulation {
        self.plan
    }

    /// Enchaînement : le même modulateur continue sur `plan` s'il est
    /// compatible (même cadence PCM, même cadence DSD, même transport).
    /// Sinon `None` (l'état est abandonné : la piste part d'un modulateur neuf).
    pub fn continuer(mut self, plan: PlanModulation) -> Option<Self> {
        if !self.plan.enchaine_sur(&plan) {
            return None;
        }
        self.plan = plan;
        Some(self)
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

    /// Volume linéaire de la zone (1,0 = 0 dB), juste avant le modulateur,
    /// atteint par une rampe de 10 ms (T11b) ; plafonné à 0 dB.
    pub fn definir_volume(&mut self, gain: f64) {
        self.modulateur.definir_volume(gain);
    }

    /// Volume posé SANS rampe, avant le premier bloc d'un modulateur neuf :
    /// le premier échantillon est déjà au niveau (une sourdine n'est jamais
    /// précédée de millisecondes à 0 dB).
    pub fn fixer_volume(&mut self, gain: f64) {
        self.modulateur.fixer_volume(gain);
    }

    /// Gain fixe (headroom, préampli gardé), appliqué avant le volume.
    pub fn definir_gain_fixe(&mut self, gain: f64) {
        if gain.is_finite() && gain >= 0.0 {
            self.gain_fixe = gain.min(1.0);
        }
    }

    pub fn gain_fixe(&self) -> f64 {
        self.gain_fixe
    }

    /// Module `pcm` (trames stéréo entières, pleine échelle ±1,0) et AJOUTE
    /// le porteur à `sortie`.
    pub fn moduler(&mut self, pcm: &[f64], sortie: &mut Vec<u8>) -> Result<(), ErreurModulateur> {
        let canaux = usize::from(self.plan.voies);
        let trames = pcm.len() / canaux.max(1);
        let pcm = &pcm[..trames * canaux];
        let entree: &[f64] = if self.gain_fixe == 1.0 {
            pcm
        } else {
            self.tampon.clear();
            let g = self.gain_fixe;
            self.tampon.extend(pcm.iter().map(|x| x * g));
            &self.tampon
        };
        let besoin = self.modulateur.octets_pour(trames);
        self.dsd.resize(besoin, 0);
        let n = self.modulateur.traiter(entree, &mut self.dsd)?;
        match self.dop.as_mut() {
            Some(dop) => {
                // Deux trames DoP à la fois : le compte reste pair.
                let paire = 4 * canaux;
                self.en_attente.extend_from_slice(&self.dsd[..n]);
                let pret = self.en_attente.len() / paire * paire;
                sortie.extend_from_slice(&dop.feed(&self.en_attente[..pret]));
                self.en_attente.drain(..pret);
            }
            None => sortie.extend_from_slice(&self.dsd[..n]),
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

    /// Échantillons DSD ramenés par le limiteur de sécurité (±60 %).
    pub fn ecretages(&self) -> u64 {
        self.modulateur.ecretages()
    }
}

// ── Enchaînements ───────────────────────────────────────────────────
//
// Ordre réel en lecture : la piste N+1 est résolue (pré-armement gapless)
// ~30 s avant la fin de N, pendant que le producteur de N tourne encore. Le
// lien est EXPLICITE : la résolution de la suivante de la file l'annonce
// ([`annoncer_la_suivante`]) avec la session qui joue ; le producteur de
// N+1 envoie son en-tête, puis ATTEND, avant son premier bloc, que celui de
// N dépose son état ([`attendre_le_relais`]). N abandonné (saut, arrêt) ou
// plan incompatible : N+1 part d'un modulateur neuf. Aucune autre piste ne
// reprend jamais un état.

enum EtatDuRelais {
    EnCours,
    Depose(Box<ProducteurDsd>),
    Abandonne,
}

struct Relais {
    zone_id: i64,
    plan: PlanModulation,
    etat: EtatDuRelais,
    ouvert: Instant,
}

#[derive(Default)]
struct TableDesRelais {
    par_session: HashMap<String, Relais>,
    /// zone → (piste annoncée comme suivante, session qui jouait).
    annonces: HashMap<i64, (i64, String, Instant)>,
}

fn relais() -> &'static (Mutex<TableDesRelais>, std::sync::Condvar) {
    static R: OnceLock<(Mutex<TableDesRelais>, std::sync::Condvar)> = OnceLock::new();
    R.get_or_init(|| {
        (
            Mutex::new(TableDesRelais::default()),
            std::sync::Condvar::new(),
        )
    })
}

/// Durée de validité d'une annonce de suivante (le pré-armement a lieu dans
/// les 30 dernières secondes de la piste).
const ANNONCE_VALABLE: Duration = Duration::from_secs(90);

/// Relais gardés au plus (les plus anciens terminés sont retirés).
const RELAIS_MAX: usize = 8;

/// Le producteur de `session` (zone, plan) démarre : son état pourra passer
/// à la piste qui lui sera EXPLICITEMENT liée.
pub fn ouvrir_le_relais(zone_id: i64, session: &str, plan: PlanModulation) {
    let (m, _) = relais();
    let mut r = m.lock().unwrap_or_else(|e| e.into_inner());
    if r.par_session.len() >= RELAIS_MAX
        && let Some(vieux) = r
            .par_session
            .iter()
            .filter(|(_, x)| !matches!(x.etat, EtatDuRelais::EnCours))
            .min_by_key(|(_, x)| x.ouvert)
            .map(|(k, _)| k.clone())
    {
        r.par_session.remove(&vieux);
    }
    r.par_session.insert(
        session.to_owned(),
        Relais {
            zone_id,
            plan,
            etat: EtatDuRelais::EnCours,
            ouvert: Instant::now(),
        },
    );
}

/// Fin NORMALE de la piste de `session` : son état attend la suivante liée.
pub fn deposer_au_relais(session: &str, producteur: ProducteurDsd) {
    let (m, c) = relais();
    let mut r = m.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(x) = r.par_session.get_mut(session) {
        x.etat = EtatDuRelais::Depose(Box::new(producteur));
    }
    c.notify_all();
}

/// Piste interrompue (session fermée, erreur) : rien ne continue.
pub fn abandonner_le_relais(session: &str) {
    let (m, c) = relais();
    let mut r = m.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(x) = r.par_session.get_mut(session) {
        x.etat = EtatDuRelais::Abandonne;
    }
    c.notify_all();
}

/// La résolution de la SUIVANTE de la file (pré-armement gapless) annonce le
/// lien : la piste `track_id` suit la session `precedente` qui joue.
pub fn annoncer_la_suivante(zone_id: i64, track_id: i64, precedente: &str) {
    let (m, _) = relais();
    let mut r = m.lock().unwrap_or_else(|e| e.into_inner());
    r.annonces
        .insert(zone_id, (track_id, precedente.to_owned(), Instant::now()));
}

/// Le lien annoncé pour cette piste sur la zone (consommé) : la session
/// précédente, si elle est un flux modulé de la même zone au plan
/// compatible. Sans annonce, aucun lien : la piste part d'un modulateur neuf.
pub fn prendre_le_lien(
    zone_id: i64,
    track_id: Option<i64>,
    plan: PlanModulation,
) -> Option<String> {
    let (m, _) = relais();
    let mut r = m.lock().unwrap_or_else(|e| e.into_inner());
    let (piste, session, quand) = r.annonces.remove(&zone_id)?;
    if Some(piste) != track_id || quand.elapsed() > ANNONCE_VALABLE {
        return None;
    }
    let x = r.par_session.get(&session)?;
    (x.zone_id == zone_id
        && x.plan.enchaine_sur(&plan)
        && !matches!(x.etat, EtatDuRelais::Abandonne))
    .then_some(session)
}

/// BLOQUANT, sur le fil du producteur, avant son premier bloc : l'état
/// déposé par la session liée, dès qu'il l'est. `None` si elle est
/// abandonnée, si `annule()` (la session de la suivante a été fermée), ou
/// après `delai`.
pub fn attendre_le_relais(
    session: &str,
    plan: PlanModulation,
    delai: Duration,
    annule: &dyn Fn() -> bool,
) -> Option<ProducteurDsd> {
    let (m, c) = relais();
    let debut = Instant::now();
    let mut r = m.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        match r.par_session.get(session).map(|x| &x.etat) {
            None | Some(EtatDuRelais::Abandonne) => return None,
            Some(EtatDuRelais::Depose(_)) => {
                let x = r.par_session.get_mut(session)?;
                let EtatDuRelais::Depose(p) =
                    std::mem::replace(&mut x.etat, EtatDuRelais::Abandonne)
                else {
                    return None;
                };
                return (*p).continuer(plan);
            }
            Some(EtatDuRelais::EnCours) => {}
        }
        if annule() || debut.elapsed() >= delai {
            return None;
        }
        r = c
            .wait_timeout(r, Duration::from_millis(200))
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

// ── Volume publié ───────────────────────────────────────────────────

/// Le volume de la zone, publié au changement (`PlaybackManager`) et lu sans
/// verrou par le producteur à chaque bloc.
pub struct VolumePublie {
    volume: std::sync::atomic::AtomicU64,
    muet: AtomicBool,
}

impl VolumePublie {
    pub fn volume(&self) -> f64 {
        f64::from_bits(self.volume.load(Ordering::Relaxed))
    }
    pub fn muet(&self) -> bool {
        self.muet.load(Ordering::Relaxed)
    }
}

fn volumes() -> &'static Mutex<HashMap<i64, std::sync::Arc<VolumePublie>>> {
    static V: OnceLock<Mutex<HashMap<i64, std::sync::Arc<VolumePublie>>>> = OnceLock::new();
    V.get_or_init(Default::default)
}

/// Le volume publié d'une zone (créé à 0 dB, non muet).
pub fn volume_de_la_zone(zone_id: i64) -> std::sync::Arc<VolumePublie> {
    volumes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(zone_id)
        .or_insert_with(|| {
            std::sync::Arc::new(VolumePublie {
                volume: std::sync::atomic::AtomicU64::new(1.0f64.to_bits()),
                muet: AtomicBool::new(false),
            })
        })
        .clone()
}

/// Publie le volume et la sourdine de la zone (appelé à chaque changement).
pub fn publier_volume(zone_id: i64, volume: f64, muet: bool) {
    let v = volume_de_la_zone(zone_id);
    v.volume.store(volume.to_bits(), Ordering::Relaxed);
    v.muet.store(muet, Ordering::Relaxed);
}

// ── Producteurs actifs ──────────────────────────────────────────────

static PRODUCTEURS_ACTIFS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Tenu par le fil d'un producteur le temps de sa piste : la mesure de la
/// machine ne se refait pas pendant une modulation.
pub struct ProducteurActif(());

impl ProducteurActif {
    pub fn tenir() -> Self {
        PRODUCTEURS_ACTIFS.fetch_add(1, Ordering::SeqCst);
        Self(())
    }
}

impl Drop for ProducteurActif {
    fn drop(&mut self) {
        PRODUCTEURS_ACTIFS.fetch_sub(1, Ordering::SeqCst);
    }
}

pub fn producteurs_actifs() -> usize {
    PRODUCTEURS_ACTIFS.load(Ordering::SeqCst)
}

// ── Ruptures constatées par la sortie ───────────────────────────────

type Ruptures = HashMap<String, Vec<(u32, u16, Instant)>>;

fn ruptures() -> &'static Mutex<Ruptures> {
    static R: OnceLock<Mutex<Ruptures>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// La sortie locale `peripherique` a dû rééchantillonner (ou adapter les
/// voies d') un porteur DoP à `cadence` Hz : retenu pour les décisions
/// suivantes (`carrier_rate_not_supported`).
pub fn noter_rupture(peripherique: &str, cadence: u32, voies: u16) {
    let mut r = ruptures().lock().unwrap_or_else(|e| e.into_inner());
    let v = r.entry(peripherique.to_owned()).or_default();
    v.retain(|(c, n, _)| (*c, *n) != (cadence, voies));
    v.push((cadence, voies, Instant::now()));
}

/// Cette cadence de porteur a-t-elle déjà été refusée par le périphérique ?
pub fn rupture_connue(peripherique: &str, cadence: u32, voies: u16) -> bool {
    let r = ruptures().lock().unwrap_or_else(|e| e.into_inner());
    r.get(peripherique)
        .is_some_and(|v| v.iter().any(|(c, n, _)| (*c, *n) == (cadence, voies)))
}

/// Une rupture a-t-elle été constatée sur ce périphérique depuis `depuis` ?
pub fn rupture_recente(peripherique: &str, depuis: Duration) -> bool {
    let r = ruptures().lock().unwrap_or_else(|e| e.into_inner());
    r.get(peripherique)
        .is_some_and(|v| v.iter().any(|(_, _, t)| t.elapsed() <= depuis))
}

// ── Mesure de la machine ────────────────────────────────────────────

struct Mesure {
    facteurs: [f64; 3],
    quand: Instant,
}

fn mesure_retenue() -> &'static Mutex<Option<Mesure>> {
    static M: OnceLock<Mutex<Option<Mesure>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(None))
}

static MESURE_EN_COURS: AtomicBool = AtomicBool::new(false);

/// Une mesure qui refuse une cadence est refaite au plus tôt après ce délai
/// (elle a pu être prise sous charge).
const NOUVELLE_MESURE_APRES: Duration = Duration::from_secs(600);

/// Passes de mesure : la meilleure est retenue.
const PASSES: usize = 3;

/// La mesure retenue (facteurs de temps réel stéréo, DSD64/128/256).
pub fn mesure_de_la_machine() -> Option<[f64; 3]> {
    mesure_retenue()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|m| m.facteurs)
}

/// Retient une mesure (production : [`mesurer_la_machine`] ; bancs et tests
/// d'intégration : une valeur imposée).
pub fn retenir_mesure_de_la_machine(facteurs: [f64; 3]) {
    *mesure_retenue().lock().unwrap_or_else(|e| e.into_inner()) = Some(Mesure {
        facteurs,
        quand: Instant::now(),
    });
}

fn mesure_a_refaire() -> bool {
    match &*mesure_retenue().lock().unwrap_or_else(|e| e.into_inner()) {
        None => true,
        Some(m) => {
            m.facteurs.iter().any(|f| *f < MARGE_TEMPS_REEL)
                && m.quand.elapsed() >= NOUVELLE_MESURE_APRES
        }
    }
}

/// Une passe : 0,25 s de stéréo 44,1 kHz modulée à chaque cadence.
fn une_passe() -> [f64; 3] {
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
        let debut = Instant::now();
        let _ = m.traiter(&pcm, &mut sortie);
        let duree = debut.elapsed().as_secs_f64().max(1e-6);
        facteurs[i] = (TRAMES as f64 / 44_100.0) / duree;
    }
    facteurs
}

/// BLOQUANT (≈ 0,3 s sur un serveur, ≈ 3 s sur un Pi 4) : [`PASSES`] passes,
/// la meilleure par cadence (une passe prise sous charge ne condamne pas la
/// machine), fusionnée avec la mesure précédente. Une mesure récente et
/// suffisante est rendue sans recalcul.
pub fn mesurer_la_machine() -> [f64; 3] {
    if !mesure_a_refaire()
        && let Some(m) = mesure_de_la_machine()
    {
        return m;
    }
    let mut meilleurs = mesure_de_la_machine().unwrap_or([0.0; 3]);
    for _ in 0..PASSES {
        for (m, f) in meilleurs.iter_mut().zip(une_passe()) {
            *m = m.max(f);
        }
    }
    tracing::info!(
        dsd64 = meilleurs[0],
        dsd128 = meilleurs[1],
        dsd256 = meilleurs[2],
        "tess_dsd_machine_mesuree"
    );
    retenir_mesure_de_la_machine(meilleurs);
    meilleurs
}

/// Lance la mesure en tâche de fond si elle manque (ou est à refaire) et
/// rend tout de suite : les routes ne l'attendent jamais.
pub fn lancer_la_mesure_en_fond() {
    // Jamais pendant une modulation : la mesure serait prise sous la charge
    // du producteur, et la prendrait à la lecture.
    if producteurs_actifs() > 0
        || !mesure_a_refaire()
        || MESURE_EN_COURS.swap(true, Ordering::SeqCst)
    {
        return;
    }
    let lance = std::thread::Builder::new()
        .name("tess-dsd-mesure".into())
        .spawn(|| {
            mesurer_la_machine();
            MESURE_EN_COURS.store(false, Ordering::SeqCst);
        });
    if lance.is_err() {
        MESURE_EN_COURS.store(false, Ordering::SeqCst);
    }
}

// ── Ce qui tourne (chemin du signal) ────────────────────────────────

/// Le modulateur en service sur un flux, publié au chemin du signal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModulationEnService {
    pub rate: TauxDsd,
    pub rate_hz: u32,
    pub transport: TransportModulation,
    pub order: usize,
    /// Cadence du flux qui part (DoP : cadence / 16 ; natif : cadence DSD).
    pub carrier_rate_hz: u32,
    pub source_rate_hz: u32,
    pub source_channels: u16,
    pub channels: u16,
    /// Les greffons de la zone sont-ils appliqués dans le flux (égaliseur,
    /// convolution, crossfeed, repli mono) ?
    pub plugins_in_stream: bool,
    /// Headroom TESS appliqué avant le modulateur (dB ≤ 0).
    pub headroom_db: f64,
    /// Le flux continue l'état de la piste précédente (enchaînement).
    pub continued: bool,
}

/// L'état de la modulation DSD d'une piste.
#[derive(Debug, Clone, PartialEq)]
pub enum EtatModulation {
    EnService(ModulationEnService),
    /// La chaîne demande le DSD ; la piste est partie en PCM, pour ce motif.
    Repli(MotifDsdRefuse),
}

/// Ce qui identifie le flux d'une décision : la session (modulée), ou la
/// piste seule (repli : la session PCM n'existe pas encore).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleFlux {
    pub track_id: Option<i64>,
    pub stream_id: Option<String>,
}

type Registre = HashMap<i64, Vec<(CleFlux, EtatModulation)>>;

fn registre() -> &'static Mutex<Registre> {
    static R: OnceLock<Mutex<Registre>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// Entrées gardées par zone : le flux en cours, le suivant préparé, et les
/// replis récents.
const ENTREES_PAR_ZONE: usize = 4;

/// Retient l'état d'un flux de la zone.
pub fn noter(zone_id: i64, cle: CleFlux, etat: EtatModulation) {
    let mut r = registre().lock().unwrap_or_else(|e| e.into_inner());
    let v = r.entry(zone_id).or_default();
    v.retain(|(c, _)| *c != cle);
    v.push((cle, etat));
    if v.len() > ENTREES_PAR_ZONE {
        v.remove(0);
    }
}

/// L'état du flux qui joue : d'abord par sa session (un flux modulé), puis,
/// pour un repli, par la piste. `None` : rien n'a été décidé pour ce flux.
pub fn etat_pour(
    zone_id: i64,
    stream_id: Option<&str>,
    track_id: Option<i64>,
) -> Option<EtatModulation> {
    let r = registre().lock().unwrap_or_else(|e| e.into_inner());
    let v = r.get(&zone_id)?;
    if let Some(s) = stream_id
        && let Some((_, e)) = v
            .iter()
            .rev()
            .find(|(c, _)| c.stream_id.as_deref() == Some(s))
    {
        return Some(e.clone());
    }
    v.iter()
        .rev()
        .find(|(c, e)| {
            c.stream_id.is_none() && c.track_id == track_id && matches!(e, EtatModulation::Repli(_))
        })
        .map(|(_, e)| e.clone())
}

/// Oublie le repli retenu pour une piste (décision sans DSD).
pub fn oublier(zone_id: i64, track_id: Option<i64>) {
    let mut r = registre().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(v) = r.get_mut(&zone_id) {
        v.retain(|(c, _)| !(c.stream_id.is_none() && c.track_id == track_id));
    }
}

/// Le bilan d'un flux modulé terminé.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BilanDuFlux {
    pub reinitialisations: u64,
    pub ecretages: u64,
    pub entree_max: f64,
}

fn bilans() -> &'static Mutex<Vec<(String, BilanDuFlux)>> {
    static B: OnceLock<Mutex<Vec<(String, BilanDuFlux)>>> = OnceLock::new();
    B.get_or_init(Default::default)
}

/// Retient le bilan d'un flux (les 16 derniers).
pub fn noter_bilan(stream_id: &str, bilan: BilanDuFlux) {
    let mut b = bilans().lock().unwrap_or_else(|e| e.into_inner());
    b.retain(|(s, _)| s != stream_id);
    b.push((stream_id.to_owned(), bilan));
    if b.len() > 16 {
        b.remove(0);
    }
}

pub fn bilan(stream_id: &str) -> Option<BilanDuFlux> {
    let b = bilans().lock().unwrap_or_else(|e| e.into_inner());
    b.iter().find(|(s, _)| s == stream_id).map(|(_, x)| *x)
}

#[cfg(test)]
#[path = "modulation_dsd_tests.rs"]
mod tests;
