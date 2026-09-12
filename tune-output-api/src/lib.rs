//! Output plugin API for Tune Server.
//!
//! This crate is the stable contract between Tune Server and its output
//! plugins: the [`OutputTarget`] trait plus the data types it exchanges
//! ([`PlayMedia`], [`OutputStatus`], [`TransportState`]).
//!
//! Out-of-tree plugins (e.g. the Diretta output) depend on this crate —
//! `tune-output-api = { git = "https://github.com/renesenses/tune-server-rust" }`
//! — instead of vendoring a copy of the trait; tune-core re-exports it from
//! `outputs::traits` so in-tree code is unaffected.
//!
//! # Les deux puits (#2218, #2219)
//!
//! La caisse porte aussi les deux extrémités de PCM du chemin local, et elles
//! ne sont pas interchangeables :
//!
//! * [`PuitsDEchantillons`] est le puits **flottant** : des `f32` sortis du
//!   décodage, du DSP et du rééchantillonnage, sur le chemin CPAL partagé. Il
//!   RESTE le chemin DSP ; rien ici ne le remplace.
//! * [`PuitsNatif`] est le puits des mots **entiers** : un [`BlocPcm`] — des
//!   octets et leur [`AudioSpec`], indissociables — rangé octet pour octet,
//!   sans conversion. Il existe pour les bras exclusifs de REF-8 (CoreAudio,
//!   ASIO, WASAPI), qui transportent des mots natifs dont des trames DoP
//!   qu'un puits flottant ne peut pas porter, et pour le banc de #2218 qui
//!   doit mesurer ce qui part réellement au DAC sur ces bras.
//!   [`CaptureOutputNatif`] est sa capture. Il ne fait **aucune** conversion
//!   et ne prend **aucune** décision DoP : les marqueurs le traversent sans
//!   être lus.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Version du contrat de capacités compris par ce binaire.
pub const OUTPUT_CAPABILITIES_VERSION: u16 = 1;

/// Commande optionnelle qu'une sortie peut accepter ou refuser.
///
/// Le nom fait partie du contrat HTTP : il est sérialisé en `snake_case` et
/// permet à un client de distinguer une commande impossible d'une panne du
/// renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputCommand {
    Pause,
    Resume,
    Seek,
    SetVolume,
    SetMute,
}

impl std::fmt::Display for OutputCommand {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Seek => "seek",
            Self::SetVolume => "set_volume",
            Self::SetMute => "set_mute",
        };
        formatter.write_str(name)
    }
}

/// Erreur structurée du chemin de commande d'une sortie.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutputCommandError {
    Unsupported {
        command: OutputCommand,
    },
    Failed {
        command: OutputCommand,
        message: String,
    },
}

impl OutputCommandError {
    pub fn unsupported(command: OutputCommand) -> Self {
        Self::Unsupported { command }
    }

    pub fn failed(command: OutputCommand, message: impl Into<String>) -> Self {
        Self::Failed {
            command,
            message: message.into(),
        }
    }

    pub fn command(&self) -> OutputCommand {
        match self {
            Self::Unsupported { command } | Self::Failed { command, .. } => *command,
        }
    }
}

impl std::fmt::Display for OutputCommandError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported { command } => {
                write!(formatter, "output command {command} is unsupported")
            }
            Self::Failed { command, message } => {
                write!(formatter, "output command {command} failed: {message}")
            }
        }
    }
}

impl std::error::Error for OutputCommandError {}

pub type OutputCommandResult<T> = Result<T, OutputCommandError>;

/// Résolution de volume que la sortie tient RÉELLEMENT (#1274).
///
/// `can_set_volume` répond « cette sortie sait régler le volume » ; il ne dit
/// pas *avec quelle finesse*. La question n'existait pas tant que l'API ne
/// parlait qu'en pour-cent : le pas de l'interface et le pas du protocole
/// étaient le même nombre. Elle apparaît avec `volume_db`, qui permet enfin de
/// viser −18 dB — une consigne que sept des treize sorties intégrées ne
/// peuvent pas recevoir telle quelle.
///
/// Ce que chaque sortie envoie réellement, relevé dans son `set_volume` :
///
/// | sortie | ce qui part sur le fil | grille |
/// |---|---|---|
/// | locale | facteur × 1000, entier | `Linear { steps: 1000 }` |
/// | SlimProto | gain 16.16, 65536 = unité | `Linear { steps: 65536 }` |
/// | DLNA/UPnP, OpenHome, BluOS, Squeezebox, HQPlayer, OAAT | entier 0..100 | `Linear { steps: 100 }` |
/// | AirPlay (RTSP) | des dB, un chiffre après la virgule | `Decibels { step_mdb: 100 }` |
/// | Chromecast, AirPlay 2, bridge, mock | un flottant | `Continuous` |
///
/// Sur une grille de 100 pas, un pas ne vaut pas partout la même chose en dB :
/// 0,09 dB sous la pleine échelle, mais **6,02 dB** entre 1 % et 2 %, et
/// au-dessous de 1 % il n'y a plus rien du tout. C'est la raison d'être de
/// [`VolumeResolution::holds`] : une consigne en dB plus basse que le plus
/// petit pas ne *manque* pas sa cible, elle **éteint** la zone — et le serveur
/// répondait 200 sur ce silence.
///
/// La valeur par défaut est `Continuous`, y compris pour un plugin ancien
/// (`version == 0`) : aucune consigne ne lui est refusée, exactement comme
/// avant l'existence de ce champ. Ce champ ne ferme donc jamais une porte
/// qui était ouverte ; il n'en ouvre une que là où une sortie a déclaré sa
/// grille.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VolumeResolution {
    /// Le niveau voyage en flottant : la sortie n'impose aucune grille.
    #[default]
    Continuous,
    /// Grille linéaire de `steps` intervalles égaux sur `0..=1`.
    ///
    /// `steps: 100` est le pour-cent entier de UPnP RenderingControl et de la
    /// plupart des protocoles réseau.
    Linear { steps: u32 },
    /// Grille en décibels, exprimée en millièmes de dB pour rester comparable
    /// sans flottant (`100` = 0,1 dB, le pas d'AirPlay).
    Decibels { step_mdb: u32 },
}

impl VolumeResolution {
    /// Le plus petit niveau **non nul** que la grille sache représenter.
    ///
    /// `None` pour `Continuous` et pour `Decibels` : ni l'un ni l'autre ne
    /// transforme un niveau audible en silence. Une grille en dB n'a pas de
    /// plancher de représentation — c'est le mute, pas l'arrondi, qui coupe.
    #[must_use]
    pub fn smallest_audible(&self) -> Option<f64> {
        match self {
            Self::Linear { steps } if *steps > 0 => Some(1.0 / f64::from(*steps)),
            _ => None,
        }
    }

    /// Le même seuil, en dB — le chiffre à montrer à qui règle sa chaîne.
    #[must_use]
    pub fn floor_db(&self) -> Option<f64> {
        self.smallest_audible().map(|l| 20.0 * l.log10())
    }

    /// Cette sortie sait-elle tenir ce niveau linéaire ?
    ///
    /// Le silence (`0.0`) est toujours tenable : c'est le mute, et toute
    /// grille sait envoyer zéro. Au-dessus, un niveau est tenable dès qu'il
    /// atteint le premier pas ; la tolérance couvre le fait que `10^(-40/20)`
    /// ne rend pas `0.01` au bit près, et qu'un plancher annoncé doit être
    /// accepté par le contrôle qui l'annonce.
    #[must_use]
    pub fn holds(&self, linear: f64) -> bool {
        if linear.is_nan() {
            return false;
        }
        match self.smallest_audible() {
            None => true,
            Some(pas) => linear <= 0.0 || linear >= pas * (1.0 - 1e-9),
        }
    }
}

/// Capacités déclarées par une sortie.
///
/// `version == 0` signifie « plugin ancien, contrat inconnu ». Le serveur le
/// traite de façon conservatrice : aucune commande optionnelle n'est supposée
/// réussir. Les listes de formats et de dispositions sont vides quand la
/// sortie ne sait pas encore publier cette partie du contrat ; elles ne
/// signifient donc pas que la sortie ne sait lire aucun son.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputCapabilities {
    pub version: u16,
    pub can_pause: bool,
    pub can_resume: bool,
    pub can_seek: bool,
    pub can_set_volume: bool,
    pub can_mute: bool,
    pub can_gapless: bool,
    #[serde(default)]
    pub formats: Vec<String>,
    #[serde(default)]
    pub channel_layouts: Vec<String>,
    /// La finesse que cette sortie tient réellement (#1274).
    ///
    /// Additif : absent d'une charge utile ancienne, il vaut `Continuous`,
    /// et rien ne change pour la sortie qui ne le déclare pas.
    #[serde(default)]
    pub volume_resolution: VolumeResolution,
}

impl OutputCapabilities {
    pub fn v1(
        can_pause: bool,
        can_resume: bool,
        can_seek: bool,
        can_set_volume: bool,
        can_mute: bool,
        can_gapless: bool,
    ) -> Self {
        Self {
            version: OUTPUT_CAPABILITIES_VERSION,
            can_pause,
            can_resume,
            can_seek,
            can_set_volume,
            can_mute,
            can_gapless,
            formats: Vec::new(),
            channel_layouts: Vec::new(),
            volume_resolution: VolumeResolution::Continuous,
        }
    }

    /// Déclare les dispositions de canaux que cette sortie sait rendre (#3322).
    ///
    /// `v1` pose `channel_layouts: Vec::new()` en dur, et il n'existait AUCUN
    /// chemin d'écriture : le champ était déclaré, publié, et vide sur les
    /// quatorze zones mesurées le 04/09/2026. C'est le builder qui manquait.
    ///
    /// La liste vide garde son sens : « on ne sait pas », jamais « aucune ».
    /// Une sortie qui ignore le nombre de canaux de son appareil ne doit rien
    /// déclarer plutôt qu'inventer une valeur.
    #[must_use]
    pub fn with_channel_layouts(mut self, layouts: Vec<String>) -> Self {
        self.channel_layouts = layouts;
        self
    }

    /// Déclare une grille linéaire de `steps` pas (voir [`VolumeResolution`]).
    #[must_use]
    pub fn with_linear_volume(mut self, steps: u32) -> Self {
        self.volume_resolution = VolumeResolution::Linear { steps };
        self
    }

    /// Déclare le pour-cent entier — la grille de la majorité des protocoles
    /// réseau, et la seule sur laquelle une consigne en dB peut se perdre.
    #[must_use]
    pub fn with_percent_volume(self) -> Self {
        self.with_linear_volume(100)
    }

    /// Déclare une grille en décibels, en millièmes de dB (`100` = 0,1 dB).
    #[must_use]
    pub fn with_decibel_volume(mut self, step_mdb: u32) -> Self {
        self.volume_resolution = VolumeResolution::Decibels { step_mdb };
        self
    }

    pub fn supports(&self, command: OutputCommand) -> bool {
        match command {
            OutputCommand::Pause => self.can_pause,
            OutputCommand::Resume => self.can_resume,
            OutputCommand::Seek => self.can_seek,
            OutputCommand::SetVolume => self.can_set_volume,
            OutputCommand::SetMute => self.can_mute,
        }
    }

    pub fn require(&self, command: OutputCommand) -> OutputCommandResult<()> {
        self.supports(command)
            .then_some(())
            .ok_or_else(|| OutputCommandError::unsupported(command))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportState {
    Stopped,
    Playing,
    Paused,
    Transitioning,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputStatus {
    pub state: TransportState,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub volume: f64,
    pub muted: bool,
    pub current_uri: Option<String>,
    pub track_title: Option<String>,
    pub track_artist: Option<String>,
    /// The local audio thread has finished draining all audio data naturally
    /// (not via stop/skip). When true + state==Stopped, this is a definitive
    /// end-of-track that should trigger auto_next regardless of played_enough.
    pub ended_naturally: bool,
    /// Whether this output consumes the track at 1x, in real time.
    ///
    /// `true` for every renderer, and the default: a speaker, a DLNA device or
    /// a Chromecast cannot finish a five-minute track in under five minutes, so
    /// the poller treats an early `ended_naturally` as a device bug (an Eversolo
    /// DMP-A8 reporting a phantom end mid-track) and holds the queue back until
    /// enough wall-clock time has passed.
    ///
    /// `false` for an output that legitimately finishes faster than 1x — a
    /// recorder writing the container to disk at network speed. Those
    /// wall-clock plausibility guards do not apply to it: its
    /// `ended_naturally` + `Stopped` means the track really is done, one second
    /// into a five-minute piece.
    pub realtime: bool,
    /// La sortie est en train de servir du **DoP** : un train DSD emballé dans
    /// du PCM 24 bits, reconnu à son marqueur alternant dans l'octet de poids
    /// fort (`0x05`/`0xFA`).
    ///
    /// Conséquence visible pour l'utilisateur, et seule raison d'être de ce
    /// champ : **le curseur de volume ne fait plus rien.** Tout facteur autre
    /// que l'unité réécrit le marqueur, le DAC quitte le mode DSD et se coupe ;
    /// le serveur épingle donc le volume à l'unité tant que dure le DoP
    /// (#1735). Sans ce champ, le client ne peut pas distinguer un curseur
    /// inerte d'un curseur cassé — et on remplacerait un silence inexpliqué par
    /// une commande morte inexpliquée.
    ///
    /// **Détecté sur les octets, pas déduit des réglages.** Le mode DSD de la
    /// zone dit ce qui a été *demandé* ; le plafond « Fréquence max » peut faire
    /// retomber en PCM sans rien annoncer. Rejouer ces règles côté affichage
    /// est précisément ce qui a fait mentir le chemin du signal (#1595).
    ///
    /// `false` pour tout ce qui n'est pas une sortie locale en DoP, donc pour
    /// l'immense majorité des lectures et pour tous les plugins : champ
    /// additif, aucun n'a à le renseigner.
    pub dop_active: bool,
}

/// Runtime truth observed at the last boundary before an output backend.
///
/// This deliberately lives beside [`OutputTarget`] instead of adding fields to
/// [`OutputStatus`]: out-of-tree plugins commonly construct `OutputStatus`
/// with a struct literal, so extending that structure would be a source-level
/// breaking change. The trait method returning this type has a default and is
/// therefore additive for those plugins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputSignalPathStatus {
    pub bit_perfect: bool,
    pub sample_transport: OutputSampleTransport,
    pub dsp: OutputDspState,
    pub volume: OutputVolumeState,
    pub reasons: Vec<OutputSignalReason>,
}

/// Compteurs DSP réellement observés par la sortie pendant la piste courante.
///
/// Séparé de [`OutputSignalPathStatus`] pour ne pas casser les plugins externes
/// qui construisent encore cette structure par littéral. Le trait expose une
/// méthode à défaut `None`, donc l'ajout reste compatible côté source (#2212).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputDspMetrics {
    pub eq_overs: u64,
    pub eq_non_finite_samples: u64,
}

/// Famine de l'anneau, telle que le rappel audio temps réel l'a vécue (#3205).
///
/// « Famine » désigne ici une chose précise et une seule : le pilote a réclamé
/// N échantillons au rappel, l'anneau en a rendu moins, et le manque a été
/// comblé par des **zéros**. C'est un trou audible, et il dit que le
/// PRODUCTEUR n'a pas suivi : réseau, décodage ou convolution.
///
/// 🔴 **Il ne dit RIEN de l'ordonnancement du noyau**, contrairement à ce que
/// ce commentaire a affirmé jusqu'ici. Quand le noyau réveille le fil de
/// sortie trop tard, ALSA a déjà sous-alimenté le DAC ; cpal signale
/// `StreamError::BufferUnderrun`, recouvre, et **saute le rappel de données**
/// (`cpal/src/host/alsa/mod.rs`, branche `PollDescriptorsFlow::XRun`). Le
/// rappel n'est jamais appelé, l'anneau est resté PLEIN, et `events` ne bouge
/// pas d'un cran pendant que le DAC encaisse un trou. Le chiffre qui voit
/// cet incident-là est [`driver_underruns`](Self::driver_underruns).
///
/// ⚠️ **À ne pas confondre avec l'« underrun » ALSA** que cpal remonte en
/// `StreamError` et que `make_stream_error_cb` laisse délibérément passer sans
/// démonter le flux (« ALSA underruns are routine »). Celui-là décrit le
/// PILOTE qui n'a pas été servi à temps par le processus ; il est routinier,
/// il est remonté par une couche qui ne voit pas l'anneau, et il ne dit pas si
/// des zéros sont partis vers le DAC. Additionner les deux produirait un
/// nombre que plus rien ne permettrait d'interpréter — et c'est précisément ce
/// nombre qui doit décider du sort du noyau `PREEMPT_RT` de Tune OS. Les deux
/// vivent donc sous deux noms distincts et ne sont jamais cumulés.
///
/// Un compteur d'ÉVÉNEMENTS seul ne distingue pas un micro-trou d'une coupure
/// d'une seconde, et un compteur brut ne se compare pas d'une machine à
/// l'autre ; d'où les quatre champs : les deux premiers disent la gravité, les
/// deux derniers donnent le dénominateur qui rend le taux calculable après une
/// heure de lecture (`events` par heure, `missing_samples / served_samples`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputRingStarvation {
    /// Rappels qui ont manqué de données depuis le démarrage du flux.
    pub events: u64,
    /// Échantillons entrelacés manquants, cumulés sur tous les événements.
    pub missing_samples: u64,
    /// Échantillons entrelacés réclamés par le pilote depuis le démarrage.
    /// Dénominateur de `missing_samples`.
    pub served_samples: u64,
    /// Sous-alimentations du PILOTE depuis le démarrage du flux (#3205).
    ///
    /// Le pilote a réclamé des échantillons et le processus n'était pas là
    /// pour les fournir : le fil de sortie n'a pas été ordonnancé à temps.
    /// C'est LE chiffre qui décide du sort du noyau `PREEMPT_RT` de Tune OS —
    /// s'il reste à zéro une heure de lecture sur noyau standard, le noyau RT
    /// est un coût sans gain, et le Secure Boot revient.
    ///
    /// Jamais additionné à `events` : les deux décrivent des pannes disjointes
    /// et une somme ne s'interpréterait plus (voir l'avertissement du type).
    ///
    /// `serde(default)` : cette structure est le contrat des greffons de sortie
    /// HORS ARBRE. Un relevé sérialisé par un greffon construit avant ce champ
    /// doit continuer à se relire — sans quoi l'ajout casserait les greffons
    /// tiers, et en silence.
    #[serde(default)]
    pub driver_underruns: u64,
    /// Durée d'audio écoulée depuis le démarrage du flux, en millisecondes,
    /// déduite de `served_samples` et de la cadence (taux × canaux).
    ///
    /// Déduite du COMPTE d'échantillons plutôt que d'une horloge : le rappel
    /// audio est temps réel et n'a pas le droit de lire l'heure, et le temps
    /// consommé par le pilote est de toute façon la seule base honnête pour un
    /// taux d'événements par heure de lecture.
    pub stream_ms: u64,
}

/// Les compteurs vivants derrière [`OutputRingStarvation`], partagés entre le
/// rappel audio et le fil qui répond à `/api/v1/system/diagnostics`.
///
/// ⚠️ **Contrat temps réel.** [`record`](Self::record) est appelé DEPUIS le
/// rappel du pilote. Il n'a le droit de rien allouer, de rien verrouiller et
/// de rien journaliser : uniquement des atomiques en `Ordering::Relaxed`. Une
/// seule ligne de log ici fabriquerait la famine qu'elle prétend mesurer. Le
/// test `drains_temps_reel_ne_font_aucune_allocation` (outputs/local.rs) tient
/// cette garde : il enveloppe les drains dans un allocateur instrumenté.
///
/// `Relaxed` suffit : ces compteurs ne publient aucune donnée, ils ne font que
/// se compter eux-mêmes. Un lecteur peut voir deux champs d'instants
/// légèrement différents ; sur un compteur d'incidents cumulé, c'est sans
/// conséquence, et c'est le prix à ne pas payer dans le rappel.
#[derive(Debug, Default)]
pub struct RingStarvation {
    /// Le flux a-t-il vraiment démarré ? Tant qu'un premier rappel n'a pas été
    /// servi ENTIÈREMENT, rien n'est compté : au démarrage l'anneau est vide
    /// par construction, et compter ce silence-là ferait passer chaque début
    /// de piste pour un incident.
    armed: AtomicBool,
    events: AtomicU64,
    missing_samples: AtomicU64,
    served_samples: AtomicU64,
    /// Sous-alimentations du pilote (#3205). Alimenté par le rappel d'ERREUR
    /// du backend, pas par le rappel de données — c'est tout l'intérêt : sur
    /// un XRun, le rappel de données n'est pas appelé.
    driver_underruns: AtomicU64,
    /// Échantillons entrelacés par seconde (taux × canaux). Posé hors du
    /// rappel par [`begin_stream`](Self::begin_stream).
    samples_per_second: AtomicU32,
}

impl RingStarvation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Armer un flux neuf : remet les compteurs à zéro et enregistre la
    /// cadence. Appelé par le producteur, JAMAIS depuis le rappel.
    pub fn begin_stream(&self, sample_rate: u32, channels: u16) {
        self.armed.store(false, Ordering::Relaxed);
        self.events.store(0, Ordering::Relaxed);
        self.missing_samples.store(0, Ordering::Relaxed);
        self.served_samples.store(0, Ordering::Relaxed);
        self.driver_underruns.store(0, Ordering::Relaxed);
        self.samples_per_second.store(
            sample_rate.saturating_mul(u32::from(channels)),
            Ordering::Relaxed,
        );
    }

    /// Comptabiliser UN rappel : `demande` = ce que le pilote a réclamé,
    /// `rendu` = ce que l'anneau a effectivement fourni. La différence part en
    /// zéros vers le DAC.
    ///
    /// Chemin temps réel — voir le contrat sur le type.
    #[inline]
    pub fn record(&self, demande: usize, rendu: usize) {
        if !self.armed.load(Ordering::Relaxed) {
            // Le flux n'a pas encore démarré : on n'arme qu'au premier rappel
            // servi en entier, ce qui donne le même point de départ à tous les
            // backends, qu'ils aient ou non leur propre garde de pré-remplissage.
            if demande == 0 || rendu < demande {
                return;
            }
            self.armed.store(true, Ordering::Relaxed);
        }
        self.served_samples
            .fetch_add(demande as u64, Ordering::Relaxed);
        if rendu < demande {
            self.events.fetch_add(1, Ordering::Relaxed);
            self.missing_samples
                .fetch_add((demande - rendu) as u64, Ordering::Relaxed);
        }
    }

    /// Comptabiliser UNE sous-alimentation du pilote, telle que le backend la
    /// remonte dans son rappel d'ERREUR (#3205).
    ///
    /// Volontairement séparé de [`record`](Self::record) : sur un XRun ALSA,
    /// cpal saute le rappel de données, donc `record` n'est PAS appelé et
    /// l'anneau paraît sain. Sans ce compteur-ci, un incident d'ordonnancement
    /// ne laisse aucune trace chiffrée.
    ///
    /// Même contrat temps réel que `record` : un seul atomique `Relaxed`, rien
    /// d'autre. Le rappel d'erreur tourne sur le fil de sortie du backend.
    ///
    /// Pas de garde `armed` ici, contrairement à `record` : un XRun ne peut pas
    /// se produire avant que le pilote ait commencé à tirer des données, donc
    /// il n'y a pas de silence de démarrage à écarter.
    #[inline]
    pub fn record_driver_underrun(&self) {
        self.driver_underruns.fetch_add(1, Ordering::Relaxed);
    }

    /// Relevé hors chemin temps réel.
    pub fn snapshot(&self) -> OutputRingStarvation {
        let served = self.served_samples.load(Ordering::Relaxed);
        let cadence = u64::from(self.samples_per_second.load(Ordering::Relaxed));
        OutputRingStarvation {
            events: self.events.load(Ordering::Relaxed),
            missing_samples: self.missing_samples.load(Ordering::Relaxed),
            served_samples: served,
            driver_underruns: self.driver_underruns.load(Ordering::Relaxed),
            stream_ms: if cadence == 0 {
                0
            } else {
                served.saturating_mul(1000) / cadence
            },
        }
    }
}

/// #3205 — la sous-alimentation du PILOTE et la famine de l'ANNEAU sont deux
/// pannes disjointes, et le compteur doit les garder disjointes.
///
/// Ce module est EXÉCUTÉ à chaque PR : `tune-output-api` figure dans le `-p` du
/// job `Test` de `ci.yml` et ne porte aucune feature. C'est délibéré — le
/// câblage côté `local.rs` vit derrière `local-audio`, que ce job n'active pas,
/// et n'est donc gardé que par un témoin de TEXTE
/// (`tune-server/tests/famine_pilote_3205.rs`). Le contrat du compteur, lui,
/// se teste pour de vrai, ici.
#[cfg(test)]
mod famine_pilote_3205 {
    use super::*;

    /// Le scénario exact d'un incident d'ORDONNANCEMENT, tel que cpal le
    /// produit : le noyau réveille le fil de sortie trop tard, ALSA a déjà
    /// sous-alimenté le DAC, cpal signale l'erreur, recouvre — et **saute le
    /// rappel de données**. L'anneau, lui, est resté PLEIN.
    ///
    /// C'est la raison d'être de ce compteur : avant lui, cet incident-là ne
    /// laissait aucun chiffre derrière lui, et c'est pourtant le seul qui
    /// puisse décider du noyau `PREEMPT_RT` de Tune OS.
    #[test]
    fn un_incident_d_ordonnancement_ne_touche_pas_la_famine_de_l_anneau() {
        let compteur = RingStarvation::new();
        compteur.begin_stream(44_100, 2);

        // Le flux tourne, l'anneau sert tout ce qu'on lui demande.
        compteur.record(1_024, 1_024);
        compteur.record(1_024, 1_024);

        // XRun : cpal appelle le rappel d'ERREUR, jamais le rappel de données.
        compteur.record_driver_underrun();

        let releve = compteur.snapshot();
        assert_eq!(
            releve.driver_underruns, 1,
            "la sous-alimentation du pilote n'est pas comptée : un trou audible              d'origine ordonnancement ne laisse aucune trace chiffrée, et #3205              redevient immesurable"
        );
        assert_eq!(
            releve.events, 0,
            "l'incident du PILOTE a été compté comme une famine de l'ANNEAU. Les              deux pannes sont disjointes — producteur en retard d'un côté,              processus pas ordonnancé de l'autre — et les confondre rend le              chiffre ininterprétable, donc inutile à l'arbitrage du noyau RT"
        );
        assert_eq!(
            releve.missing_samples, 0,
            "aucun échantillon n'a manqué DANS L'ANNEAU : le rappel de données              n'a même pas été appelé"
        );
    }

    /// La réciproque : un producteur en retard ne doit pas gonfler le compteur
    /// du pilote. Sans cette moitié, fusionner les deux compteurs passerait.
    #[test]
    fn une_famine_d_anneau_ne_compte_aucune_sous_alimentation_du_pilote() {
        let compteur = RingStarvation::new();
        compteur.begin_stream(44_100, 2);
        compteur.record(1_024, 1_024);
        compteur.record(1_024, 300);

        let releve = compteur.snapshot();
        assert_eq!(releve.events, 1);
        assert_eq!(releve.missing_samples, 724);
        assert_eq!(
            releve.driver_underruns, 0,
            "une famine de l'ANNEAU a été comptée comme une sous-alimentation du              PILOTE : le chiffre qui décide du noyau RT se met à monter quand              c'est le réseau ou le décodage qui est en retard"
        );
    }

    /// Un flux neuf repart de zéro. #3205 veut un TAUX par heure de lecture ;
    /// un compteur qui cumule des pistes sans rapport ne se compare à rien.
    #[test]
    fn un_flux_neuf_remet_le_compteur_du_pilote_a_zero() {
        let compteur = RingStarvation::new();
        compteur.begin_stream(44_100, 2);
        compteur.record(512, 512);
        compteur.record_driver_underrun();
        compteur.record_driver_underrun();
        assert_eq!(compteur.snapshot().driver_underruns, 2);

        compteur.begin_stream(48_000, 2);
        assert_eq!(
            compteur.snapshot().driver_underruns,
            0,
            "un flux neuf hérite des sous-alimentations du précédent : le taux              par heure de lecture cumule des pistes sans rapport"
        );
    }

    /// Le chiffre doit SORTIR. Un compteur que la route ne publie pas ne mesure
    /// rien pour le testeur qui colle son rapport de diagnostic sur le forum —
    /// et c'est ce rapport, sur un parc réel, qui doit trancher #3205.
    #[test]
    fn le_compteur_du_pilote_est_serialise_et_tolere_un_releve_ancien() {
        let releve = OutputRingStarvation {
            events: 0,
            missing_samples: 0,
            served_samples: 1,
            driver_underruns: 7,
            stream_ms: 0,
        };
        let json = serde_json::to_value(releve).unwrap();
        assert_eq!(
            json["driver_underruns"], 7,
            "le compteur n'atteint pas la route : invisible dans le rapport de              diagnostic, donc inexistant pour la mesure"
        );

        // Un greffon de sortie hors arbre, construit avant ce champ, sérialise
        // un relevé sans lui. Il doit continuer à se relire.
        let ancien: OutputRingStarvation = serde_json::from_value(serde_json::json!({
            "events": 3,
            "missing_samples": 12,
            "served_samples": 400,
            "stream_ms": 9,
        }))
        .expect(
            "un relevé produit par un greffon antérieur à ce champ ne se relit              plus : l'ajout casse les greffons de sortie hors arbre",
        );
        assert_eq!(ancien.driver_underruns, 0);
        assert_eq!(ancien.events, 3);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputSampleTransport {
    NativeInteger,
    Float,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputDspState {
    Inactive,
    Applied,
    BypassedPure,
    BypassedDop,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputVolumeState {
    Unity,
    Applied,
    BypassedDop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputSignalReason {
    FloatTransport,
    DspApplied,
    DspStateUnknown,
    SoftwareVolume,
}

impl Default for OutputStatus {
    fn default() -> Self {
        Self {
            state: TransportState::Stopped,
            position_ms: 0,
            duration_ms: 0,
            volume: 0.5,
            muted: false,
            current_uri: None,
            track_title: None,
            track_artist: None,
            ended_naturally: false,
            realtime: true,
            dop_active: false,
        }
    }
}

pub struct PlayMedia<'a> {
    pub url: &'a str,
    pub mime_type: &'a str,
    pub title: Option<&'a str>,
    pub artist: Option<&'a str>,
    pub album: Option<&'a str>,
    pub cover_url: Option<&'a str>,
    pub duration_ms: Option<u64>,
    pub file_size: Option<u64>,
    /// Local file path for outputs that can read directly (OAAT).
    pub file_path: Option<&'a str>,
    /// Audio sample rate in Hz (e.g. 176400 for DSD64->PCM).
    /// Used by DLNA renderers that require sampleFrequency in DIDL-Lite.
    pub sample_rate: Option<u32>,
    /// Audio bit depth (e.g. 24 for DSD->PCM transcoding).
    pub bit_depth: Option<u32>,
    /// Number of audio channels (e.g. 2 for stereo).
    pub channels: Option<u32>,
    /// True for infinite live streams (internet radio): the DIDL-Lite `<res>`
    /// must advertise a live/streaming source (DLNA.ORG_OP=00, senderPaced
    /// flags, no size/duration) rather than a seekable file, otherwise some
    /// renderers (Yamaha R-N2000A) accept SetAVTransportURI + Play but never
    /// produce sound.
    pub live_stream: bool,
    /// The upstream source `url` was derived from, when `url` is one of the
    /// server's own proxy or transcode endpoints — an Icecast mount, a podcast
    /// enclosure, a signed CDN link.
    ///
    /// `url` is what an output should *play*: it is proxied precisely because
    /// renderers need a format they understand, and it can be read by several
    /// consumers. This is what an output should read when it wants the bytes as
    /// the source published them — a recorder keeping the original codec instead
    /// of a PCM transcode, or anything that needs the stream's own metadata
    /// (ICY titles do not survive the proxy). `None` when `url` already *is* the
    /// upstream, so a consumer can fall back to it unconditionally.
    ///
    /// Reading it is opt-in: an output that ignores it behaves exactly as before.
    pub origin_url: Option<&'a str>,
    /// Which library or service the track came from (`"local"`, `"qobuz"`,
    /// `"tidal"`, `"radio"`, …), paired with `source_id` below.
    ///
    /// Titles are not an identity: two tracks on one album can share a title
    /// (an album and its alternate takes), and an output that keys on
    /// artist/album/title alone will treat them as the same track. This pair is
    /// the stable identity the host already has — an output can use it to tell
    /// a genuine second play from a replay of the same track.
    pub source: Option<&'a str>,
    /// Identifier of the track within `source`: the local track id as a string,
    /// or the service's own track id.
    pub source_id: Option<&'a str>,
    /// Album numbering, when the host knows it.
    ///
    /// Anything that lays tracks out in album order — an output that files
    /// tracks by their rank, a display showing "3 / 12" — has no other way to
    /// get it: the queue row
    /// and the library track carry it, but it used to stop at the output
    /// boundary, leaving outputs to invent a counter of their own.
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    /// False when `url` is a one-shot conversion channel (DSD→WAV à la volée) :
    /// aucun octet passé ne peut être rejoué, la DIDL doit annoncer
    /// `DLNA.ORG_OP=00` pour que le renderer streame séquentiellement au lieu
    /// de chercher par tranches (l'Eversolo DMP-A8 seeke parce qu'on lui a dit
    /// qu'il pouvait — et gèle à 0:00). True pour tout ce qui est servi depuis
    /// un fichier, avec un vrai support des Range.
    pub byte_seekable: bool,
}

impl Default for PlayMedia<'_> {
    fn default() -> Self {
        Self {
            url: "",
            mime_type: "",
            title: None,
            artist: None,
            album: None,
            cover_url: None,
            duration_ms: None,
            file_size: None,
            file_path: None,
            sample_rate: None,
            bit_depth: None,
            channels: None,
            live_stream: false,
            origin_url: None,
            source: None,
            source_id: None,
            track_number: None,
            disc_number: None,
            byte_seekable: true,
        }
    }
}

/// Le puits d'échantillons : l'extrémité qui reçoit le PCM déjà converti.
///
/// C'est la frontière entre le **producteur** — qui lit les octets, décide
/// PCM ou DoP, applique le DSP, adapte les canaux et rééchantillonne — et le
/// **backend**, qui n'a plus qu'à ranger des mots flottants entrelacés là où
/// son pilote viendra les chercher.
///
/// Le contrat tient en une phrase : `ecrire` rend la main quand tout `mots` est
/// rangé, ou quand il est devenu inutile de continuer.
///
/// Il vit ici, dans la caisse de contrat **sans aucune fonctionnalité**, et non
/// derrière `local-audio` : un puits est un point d'extension, au même titre
/// que [`OutputTarget`], et la porte `test` de toute PR Rust doit le compiler.
///
/// Le trait ne dit rien du rythme : un puits peut bloquer (l'anneau CPAL
/// attend que le rappel draine), écrire sans jamais bloquer (un puits de
/// capture), ou ne rien faire du tout. Il ne dit rien non plus du format —
/// cadence et canaux sont convenus à l'ouverture, hors de ce contrat, parce
/// qu'ils ne changent pas d'un bloc à l'autre.
pub trait PuitsDEchantillons {
    /// Range `mots` — du PCM `f32` entrelacé, au format de sortie convenu.
    ///
    /// Rend `false` **uniquement** quand le puits a cessé de consommer et que
    /// le producteur doit se démonter : rappel mort, périphérique arraché.
    /// Rend `true` dans tous les autres cas, **y compris un arrêt demandé** —
    /// le producteur détecte l'arrêt par ses propres témoins, jamais par cette
    /// valeur. Confondre les deux ferait passer une pause pour une panne.
    fn ecrire(&mut self, mots: &[f32]) -> bool;
}

// ───────────────────────────────────────────────────────────────────────────
// T8 de #2218 — le puits de CAPTURE.
//
// R1 (#3958) a nommé la frontière producteur → puits et l'a démontrée avec un
// puits jetable, `PuitsEmpreinte`, qui vivait dans son propre fichier de
// témoins. Ce qui suit est ce puits-là, devenu un type de première classe de
// la caisse de contrat : le MÊME hachage, aux mêmes octets près — les quatre
// relevés de R1 tombent dessus sans être retouchés —, plus les deux choses
// qu'un puits de capture doit rendre et qu'un puits jetable n'avait pas :
//
//   * le **format réellement ouvert**, qui n'est pas celui de la source. Une
//     piste 44,1 kHz servie sur un périphérique ouvert à 48 kHz est livrée à
//     48 kHz, et rien dans le dépôt ne le publiait : `audio/tap.rs` publie
//     depuis le DÉCODAGE (`send_windowed_pcm`, appelé neuf fois depuis
//     `audio/decode.rs` et zéro fois depuis la boucle producteur, relevé le
//     12/09/2026), donc au format SOURCE. Un consommateur qui croit voir ce
//     qui part au DAC voit en fait ce qui entre dans la conversion ;
//   * la **retenue** des mots, pour qu'un témoin puisse comparer le signal
//     livré à une référence externe et pas seulement une empreinte à une
//     empreinte.
//
// Pourquoi ici, et pas derrière `local-audio` : cette caisse n'a AUCUNE
// fonctionnalité et figure dans le `-p` du job `Test` de `ci.yml` (ligne 262),
// qui tourne sur toutes les PR Rust. Tout ce qui est ici est donc exécuté à
// chaque PR ; tout ce qui reste dans `outputs::local` ne l'est que sous
// `ci:full`. C'est la raison d'être de ce découpage.
// ───────────────────────────────────────────────────────────────────────────

/// Le format que la sortie a **réellement ouvert**, à l'autre bout du puits.
///
/// Ce n'est pas le format du fichier : entre les deux il y a l'adaptation de
/// canaux et le rééchantillonnage. C'est le seul format dans lequel les mots
/// d'un [`PuitsDEchantillons`] ont un sens — les interpréter avec la cadence
/// de la source donne une durée fausse, et les désentrelacer avec le nombre de
/// canaux de la source intervertit les voies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatOuvert {
    /// Cadence du périphérique, en hertz.
    pub cadence: u32,
    /// Nombre de canaux entrelacés dans chaque bloc livré.
    pub canaux: u16,
}

impl FormatOuvert {
    pub fn new(cadence: u32, canaux: u16) -> Self {
        Self { cadence, canaux }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// R5 de #2219 — le format SOURCE, porté par un type.
//
// [`FormatOuvert`] dit ce que le périphérique a ouvert, à la SORTIE du puits :
// des `f32`, une cadence, des canaux. Il ne dit rien de l'entrée, et il ne le
// peut pas — à l'entrée il y a des OCTETS, et des octets ne se lisent pas sans
// profondeur. C'est ce que [`AudioSpec`] ajoute, et c'est tout ce qu'il ajoute :
// les deux types sont les deux bouts de la conversion, pas deux façons de dire
// la même chose.
//
// Ce qu'il remplace : `sample_rate: u32`, `bit_depth: u16`, `channels: u16` et
// `frame_bytes: usize` circulant NUS et séparément. Quatre nombres dont trois
// sont des étiquettes et le quatrième leur conséquence — que rien n'obligeait à
// recalculer quand une étiquette changeait, et dont deux, tous deux `u16`,
// s'intervertissaient sans un mot du compilateur.
// ───────────────────────────────────────────────────────────────────────────

/// Comment un mot PCM est écrit dans les octets d'un flux.
///
/// **Le jeu est FERMÉ, et aucune méthode de ce type n'a de bras `_`.** Ce n'est
/// pas une commodité : `parse_wav_header` ne rend jamais rien d'autre que `0`,
/// `16`, `24` ou `32`, et tout le chemin de lecture locale — octets par
/// échantillon, octets par trame, alignement des trames, conversion en `f32`,
/// conversion en `i32` natif — n'énumère que ces quatre-là. Une cinquième
/// valeur n'est pas « moins précise », elle est **incohérente**, et de deux
/// façons opposées selon le chemin : bruit blanc d'un côté, silence de l'autre.
///
/// Une profondeur ajoutée ici est donc réclamée par le compilateur partout où
/// elle change quelque chose, au lieu de se glisser sous un repli silencieux.
///
/// # Le piège que ce type ferme
///
/// [`Self::FlottantIeee32`] et [`Self::Entier32`] font tous deux **quatre
/// octets** et ne se décodent pas du tout pareil. Tant que la profondeur était
/// un `u16`, le flottant se disait « 0 » — le sentinelle des en-têtes WAV — et
/// `bit_depth == 32` était donc faux pour lui, tandis que `bit_depth / 8`
/// rendait `0` octet. Chaque appelant devait se souvenir du cas particulier.
/// Ici les deux sont des variantes distinctes de même largeur, et
/// [`Self::octets`] répond `4` pour les deux sans que personne ait à y penser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProfondeurPcm {
    /// Flottant IEEE 754 32 bits, petit-boutien. Se dit `0` dans un en-tête WAV
    /// tel que ce dépôt le lit : c'est un sentinelle, pas une largeur.
    FlottantIeee32,
    /// Entier signé 16 bits, petit-boutien.
    Entier16,
    /// Entier signé 24 bits, petit-boutien, trois octets par mot.
    Entier24,
    /// Entier signé 32 bits, petit-boutien.
    Entier32,
}

impl ProfondeurPcm {
    /// Largeur d'un mot, en octets. Jamais nulle.
    #[must_use]
    pub const fn octets(self) -> usize {
        match self {
            Self::FlottantIeee32 => 4,
            Self::Entier16 => 2,
            Self::Entier24 => 3,
            Self::Entier32 => 4,
        }
    }

    /// La valeur telle qu'un en-tête WAV la déclare dans ce dépôt — `0` pour le
    /// flottant.
    ///
    /// Existe pour les fonctions qui prennent encore un `u16` et qu'on ne
    /// réécrit pas ici. Chaque appel est un endroit où l'étiquette redevient un
    /// nombre nu : ils se comptent, et ils doivent diminuer.
    #[must_use]
    pub const fn bits_declares(self) -> u16 {
        match self {
            Self::FlottantIeee32 => 0,
            Self::Entier16 => 16,
            Self::Entier24 => 24,
            Self::Entier32 => 32,
        }
    }

    /// L'inverse, **partiel** : hors du jeu fermé, il n'y a pas de réponse.
    ///
    /// `None` n'est pas « on ne sait pas faire » mais « ce flux ne se décode
    /// pas ici » : le seul geste sûr est de le refuser avant qu'un octet ne
    /// parte au DAC.
    #[must_use]
    pub const fn depuis_bits_declares(bits: u16) -> Option<Self> {
        match bits {
            0 => Some(Self::FlottantIeee32),
            16 => Some(Self::Entier16),
            24 => Some(Self::Entier24),
            32 => Some(Self::Entier32),
            _ => None,
        }
    }
}

/// Le format d'un flux PCM : ce qu'il faut, et il faut tout, pour donner un
/// sens à une suite d'octets.
///
/// # Pourquoi les champs sont PRIVÉS
///
/// C'est la raison d'être du type. `octets_par_trame` n'est pas rangé à côté
/// des trois autres : il est **calculé** à chaque demande, à partir de la
/// profondeur et des canaux. Il ne peut donc pas leur survivre.
///
/// Le défaut que cela ferme est réel et daté : à une frontière gapless,
/// `local.rs` posait quatre affectations de suite — cadence, canaux,
/// profondeur, puis `frame_bytes` recalculé à la main. Oublier la quatrième, ou
/// la calculer avec l'ancienne profondeur, ne cassait aucune compilation :
/// c'était un flux 24 bits lu par trames de 16, c'est-à-dire tout le reste de
/// la piste décalé d'un octet — le bruit blanc de #3849. Ici la quatrième
/// n'existe pas, et les trois autres ne se posent qu'ensemble.
///
/// La deuxième chose qui change : intervertir la profondeur et les canaux.
/// Ils étaient tous deux `u16` et voisins dans quatre signatures.
///
/// Il faut être exact sur ce qui est gagné, parce que ce n'est pas le même
/// verrou des deux côtés :
///
/// * par [`AudioSpec::nouvelle`], l'interversion est une **erreur de type** —
///   [`ProfondeurPcm`] n'est pas un `u16`, le compilateur refuse ;
/// * par [`AudioSpec::depuis_entete`], qui prend encore deux `u16` parce
///   qu'un en-tête WAV rend deux nombres, l'interversion compile toujours.
///   Elle est rattrapée à l'exécution par le jeu fermé : un flux stéréo
///   intervertit ses arguments en « 2 bits », qui n'existe pas, et le format
///   est REFUSÉ au lieu d'être mal lu. Mesuré — voir le témoin
///   `une_profondeur_hors_du_jeu_ferme_est_refusee_pas_approchee`.
///
/// Ce qui reste ouvert, et qu'il faut nommer plutôt que taire : un flux à 16,
/// 24 ou 32 CANAUX dont les arguments seraient intervertis passerait la porte.
/// C'est la seule fenêtre, elle tient en une ligne de code, et elle disparaîtra
/// le jour où `parse_wav_header` rendra directement un [`AudioSpec`].
///
/// # Ce que ce type ne prétend PAS
///
/// Il ne vérifie pas que les octets qu'on lui associe sont vraiment dans ce
/// format-là. Une `AudioSpec` construite sur un mensonge reste un mensonge —
/// aucun type ne lit à la place de l'en-tête. Ce qu'il garantit, c'est qu'à
/// partir du moment où l'étiquette est posée, **plus personne ne la contredit
/// en aval**.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AudioSpec {
    cadence: u32,
    profondeur: ProfondeurPcm,
    canaux: u16,
}

impl AudioSpec {
    /// L'unique constructeur. `None` quand `canaux` est nul.
    ///
    /// Zéro canal n'est pas un format « vide » : c'est un diviseur nul.
    /// `octets_par_trame` vaudrait `0`, et le calcul d'alignement qui suit —
    /// `octets.len() / octets_par_trame` — **divise par zéro**, ce qui abat le
    /// fil de lecture. Refuser ici, une fois, vaut mieux que porter la garde
    /// dans chaque calcul : c'est l'invariant qui rend [`BlocPcm::trames`]
    /// total.
    #[must_use]
    pub const fn nouvelle(cadence: u32, profondeur: ProfondeurPcm, canaux: u16) -> Option<Self> {
        if canaux == 0 {
            return None;
        }
        Some(Self {
            cadence,
            profondeur,
            canaux,
        })
    }

    /// Le même, depuis les trois nombres qu'un en-tête WAV rend.
    ///
    /// `None` dès que l'un des deux refus tombe : profondeur hors du jeu fermé,
    /// ou zéro canal. C'est la porte d'entrée du type, et la seule : tout ce qui
    /// entre dans le chemin de conversion passe par un de ces deux
    /// constructeurs.
    #[must_use]
    pub const fn depuis_entete(cadence: u32, bits_declares: u16, canaux: u16) -> Option<Self> {
        match ProfondeurPcm::depuis_bits_declares(bits_declares) {
            Some(profondeur) => Self::nouvelle(cadence, profondeur, canaux),
            None => None,
        }
    }

    /// Cadence d'échantillonnage de la source, en hertz.
    #[must_use]
    pub const fn cadence(self) -> u32 {
        self.cadence
    }

    /// Comment un mot est écrit dans les octets.
    #[must_use]
    pub const fn profondeur(self) -> ProfondeurPcm {
        self.profondeur
    }

    /// Nombre de canaux entrelacés. Toujours au moins 1.
    #[must_use]
    pub const fn canaux(self) -> u16 {
        self.canaux
    }

    /// Octets d'une trame complète — **déduit, jamais rangé**. Toujours ≥ 1.
    #[must_use]
    pub const fn octets_par_trame(self) -> usize {
        self.profondeur.octets() * self.canaux as usize
    }

    /// Étiquette ces octets avec CE format, et rien d'autre.
    ///
    /// C'est le seul moyen d'obtenir un [`BlocPcm`], et un `BlocPcm` n'a aucun
    /// moyen de changer d'étiquette ensuite.
    #[must_use]
    pub const fn bloc(self, octets: &[u8]) -> BlocPcm<'_> {
        BlocPcm { spec: self, octets }
    }
}

/// Des octets PCM **et** le format dans lequel ils ont un sens, indissociables.
///
/// # Ce que ce type interdit
///
/// De réétiqueter un bloc. `spec` est privé et n'a pas de mutateur ; il n'existe
/// aucun `BlocPcm { .. }` littéral hors de cette caisse, et
/// [`BlocPcm::spec`] rend une **copie**. Écrire dessus ne change rien au bloc —
/// le compilateur refuse même d'essayer, puisqu'il n'y a rien à qui affecter.
///
/// Ce que cela vaut, concrètement : un bloc de 48 octets stéréo fait 12 trames
/// en 16 bits, 8 en 24 bits et 6 en 32. Les trois lectures sont plausibles ; une
/// seule est la bonne, et c'est celle du format qui a produit le bloc. Tant que
/// les octets et le format voyageaient séparément, tenir les deux ensemble était
/// une discipline. Ici c'est le type.
///
/// # La partition, qui n'est pas un détail
///
/// [`Self::octets_alignes`] et [`Self::reste_non_aligne`] découpent les octets en
/// DEUX, sans recouvrement ni perte. Le reste est celui qu'une lecture réseau
/// laisse à chaque tour — la trame coupée en deux par la frontière du tampon.
/// Le jeter, c'est décaler tout le flux qui suit : le bruit blanc 24 bits de
/// #3849. Le rendre explicitement, c'est obliger l'appelant à en faire quelque
/// chose.
#[derive(Debug, Clone, Copy)]
pub struct BlocPcm<'a> {
    spec: AudioSpec,
    octets: &'a [u8],
}

impl<'a> BlocPcm<'a> {
    /// Le format de ce bloc. Une copie : l'écrire ne réétiquette rien.
    #[must_use]
    pub const fn spec(&self) -> AudioSpec {
        self.spec
    }

    /// Tous les octets du bloc, alignés ou non.
    #[must_use]
    pub const fn octets(&self) -> &'a [u8] {
        self.octets
    }

    /// Le préfixe qui fait un nombre entier de trames. Décodable tel quel.
    #[must_use]
    pub fn octets_alignes(&self) -> &'a [u8] {
        &self.octets[..self.trames() * self.spec.octets_par_trame()]
    }

    /// Le suffixe qui ne complète pas une trame : à REPORTER sur la lecture
    /// suivante, jamais à jeter.
    #[must_use]
    pub fn reste_non_aligne(&self) -> &'a [u8] {
        &self.octets[self.trames() * self.spec.octets_par_trame()..]
    }

    /// Nombre de trames complètes. Total : `octets_par_trame()` ne peut pas
    /// être nul, l'invariant de [`AudioSpec::nouvelle`] s'en charge.
    #[must_use]
    pub fn trames(&self) -> usize {
        self.octets.len() / self.spec.octets_par_trame()
    }
}

/// L'état initial de l'empreinte : le décalage de base de FNV-1a 64 bits.
///
/// Publié parce qu'il est la réponse à « ce puits n'a rien reçu » — un témoin
/// qui exige `empreinte() != EMPREINTE_DU_VIDE` dit exactement cela, là où
/// `mots() > 0` dirait la même chose deux fois.
pub const EMPREINTE_DU_VIDE: u64 = 0xcbf2_9ce4_8422_2325;

const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Un tour de FNV-1a 64 bits par octet : XOR puis multiplication.
///
/// C'est la **seule** implémentation du hachage de la caisse. [`CaptureOutput`]
/// la nourrit des octets petit-boutistes de chaque `f32`,
/// [`CaptureOutputNatif`] des octets tels qu'ils arrivent. Deux copies
/// auraient pu diverger d'un tour — et les relevés de R1 ne diraient plus
/// rien de l'une des deux.
const fn hacher_fnv1a(mut empreinte: u64, octets: &[u8]) -> u64 {
    let mut i = 0;
    while i < octets.len() {
        empreinte ^= octets[i] as u64;
        empreinte = empreinte.wrapping_mul(FNV_PRIME);
        i += 1;
    }
    empreinte
}

/// Le puits de capture : il **hache les mots livrés** et publie le **format
/// réellement ouvert**.
///
/// Il ne joue rien, ne bloque jamais et n'alloue rien en régime permanent
/// quand la retenue est désactivée. Branché à la place de l'anneau cpal, il
/// transforme la chaîne de lecture en instrument de mesure : ce qui aurait été
/// envoyé au DAC devient une empreinte comparable.
///
/// # Ce que l'empreinte hache
///
/// Les **octets** des `f32`, petit-boutistes, pas leur valeur. Deux nombres
/// mathématiquement égaux mais de représentations différentes — `0.0` et
/// `-0.0`, deux `NaN` — doivent se voir, parce qu'un DAC les voit. C'est
/// FNV-1a 64 bits, à l'identique de ce que R1 a mesuré ; les relevés de
/// `empreinte_du_puits_r1.rs` sont donc valides sur ce type sans être repris.
///
/// # Ce que ce puits ne fait PAS
///
/// Il n'implémente pas [`OutputTarget`] : une sortie, c'est un transport, une
/// file, des commandes et un état ; un puits, c'est une extrémité de PCM. Les
/// deux se rencontrent dans `outputs::local`, pas ici. Il n'est pas non plus
/// partageable entre fils — le trait prend `&mut self`, et le producteur est
/// seul à écrire.
pub struct CaptureOutput {
    format: FormatOuvert,
    empreinte: u64,
    mots: u64,
    blocs: u64,
    blocs_vides: u64,
    blocs_non_alignes: u64,
    retenue: Option<Vec<f32>>,
    plafond_de_retenue: usize,
    retenue_complete: bool,
    vivant: bool,
}

impl CaptureOutput {
    /// Un puits qui ne retient rien : il ne garde que l'empreinte et les
    /// comptes. C'est la forme utilisable sur une piste entière.
    pub fn ouvert(format: FormatOuvert) -> Self {
        Self {
            format,
            empreinte: EMPREINTE_DU_VIDE,
            mots: 0,
            blocs: 0,
            blocs_vides: 0,
            blocs_non_alignes: 0,
            retenue: None,
            plafond_de_retenue: 0,
            retenue_complete: true,
            vivant: true,
        }
    }

    /// Un puits qui retient en plus les `plafond_de_retenue` premiers mots.
    ///
    /// Le plafond est explicite et son dépassement est **constaté**, jamais
    /// silencieux : [`CaptureOutput::retenue_complete`] passe à `false` dès
    /// qu'un mot livré n'a pas été retenu. Une retenue tronquée sans le dire
    /// ferait comparer une référence entière à un tronçon, et c'est
    /// exactement la forme de faux vert que cette tranche existe pour éviter.
    pub fn avec_retenue(format: FormatOuvert, plafond_de_retenue: usize) -> Self {
        let mut puits = Self::ouvert(format);
        puits.retenue = Some(Vec::new());
        puits.plafond_de_retenue = plafond_de_retenue;
        puits
    }

    /// Le format réellement ouvert, tel qu'il a été convenu à l'ouverture.
    pub fn format(&self) -> FormatOuvert {
        self.format
    }

    /// L'empreinte de tout ce qui a été livré, dans l'ordre de livraison.
    pub fn empreinte(&self) -> u64 {
        self.empreinte
    }

    /// Le nombre de mots livrés — canaux compris.
    pub fn mots(&self) -> u64 {
        self.mots
    }

    /// Le nombre d'appels à `ecrire`, blocs vides compris.
    pub fn blocs(&self) -> u64 {
        self.blocs
    }

    /// Les appels à `ecrire` qui n'ont apporté aucun mot.
    pub fn blocs_vides(&self) -> u64 {
        self.blocs_vides
    }

    /// Les blocs dont la longueur n'est **pas** un multiple du nombre de
    /// canaux ouverts.
    ///
    /// Un seul suffit à décaler toutes les trames suivantes : la voie gauche
    /// part à droite et n'y revient jamais. Aucun anneau ne le signale — il
    /// range des mots, pas des trames — et c'est donc au puits de le compter.
    pub fn blocs_non_alignes(&self) -> u64 {
        self.blocs_non_alignes
    }

    /// Le nombre de trames livrées, au format ouvert.
    pub fn trames(&self) -> u64 {
        if self.format.canaux == 0 {
            return 0;
        }
        self.mots / u64::from(self.format.canaux)
    }

    /// La durée livrée, en millisecondes, **à la cadence ouverte**.
    ///
    /// C'est le chiffre que la cadence source rendrait faux : 8,7 % d'écart
    /// entre 44,1 et 48 kHz.
    pub fn duree_livree_ms(&self) -> u64 {
        if self.format.cadence == 0 {
            return 0;
        }
        self.trames() * 1000 / u64::from(self.format.cadence)
    }

    /// Les mots retenus, ou `None` si ce puits ne retient rien.
    pub fn mots_livres(&self) -> Option<&[f32]> {
        self.retenue.as_deref()
    }

    /// Faux dès qu'un mot livré n'a pas tenu sous le plafond de retenue.
    pub fn retenue_complete(&self) -> bool {
        self.retenue_complete
    }

    /// Déclare le puits mort : les écritures suivantes rendront `false`.
    ///
    /// C'est le rappel arraché du monde réel (#1626), reproductible sans
    /// périphérique.
    pub fn declarer_mort(&mut self) {
        self.vivant = false;
    }

    /// Le puits consomme-t-il encore ?
    pub fn vivant(&self) -> bool {
        self.vivant
    }
}

impl PuitsDEchantillons for CaptureOutput {
    fn ecrire(&mut self, mots: &[f32]) -> bool {
        self.blocs += 1;
        if mots.is_empty() {
            self.blocs_vides += 1;
        }
        if self.format.canaux != 0 && mots.len() % usize::from(self.format.canaux) != 0 {
            self.blocs_non_alignes += 1;
        }
        self.mots += mots.len() as u64;
        for mot in mots {
            self.empreinte = hacher_fnv1a(self.empreinte, &mot.to_bits().to_le_bytes());
        }
        if let Some(retenue) = self.retenue.as_mut() {
            let place = self.plafond_de_retenue.saturating_sub(retenue.len());
            if place < mots.len() {
                self.retenue_complete = false;
            }
            retenue.extend_from_slice(&mots[..place.min(mots.len())]);
        }
        self.vivant
    }
}

// ───────────────────────────────────────────────────────────────────────────
// REF-8 préparatoire de #2219 — le puits des mots ENTIERS.
//
// [`PuitsDEchantillons`] est le puits FLOTTANT : celui du chemin CPAL partagé,
// où décodage → DSP → rééchantillonnage rendent des `f32`. Les trois bras
// exclusifs (CoreAudio, ASIO, WASAPI) n'y passent pas : ils transportent des
// mots entiers tels que lus, dont des trames DoP — un train DSD emballé dans
// du PCM 24 bits, reconnu à son marqueur alternant `0x05`/`0xFA` dans l'octet
// de poids fort. Un mot DoP converti en `f32` puis reconverti n'est plus
// garanti octet pour octet, et le DAC, qui ne voit plus le marqueur, se coupe.
// Un puits flottant ne PEUT donc pas porter ces bras.
//
// R1 (#3958) a nommé la frontière et refusé, explicitement, de choisir le type
// de mot. Le choix est ici, et c'est l'option A : un **second trait**, qui
// reçoit un [`BlocPcm`] — des octets ET leur [`AudioSpec`], indissociables
// (R5, #3965) — et non un trait générique sur le type de mot. Trois raisons :
//
//   * un `BlocPcm` ne se réétiquette pas : le puits SAIT dans quel format sont
//     les octets, et peut refuser ceux qui ne sont pas au format ouvert. Un
//     `&[i32]` ou un `&[u8]` nu ne le pourrait pas ;
//   * les octets traversent tels quels : aucune conversion, donc le DoP reste
//     intact par construction, pas par précaution ;
//   * le puits flottant RESTE le chemin DSP. Un trait générique aurait invité à
//     unifier les deux, et c'est précisément ce que la tranche interdit.
//
// Ce que ce code sert : le banc de #2218 (mesurer ce qui part au DAC) étendu
// aux bras exclusifs, que REF-8 migrera un par un — CoreAudio d'abord. Aucun
// bras n'est migré ici ; aucun ne touche `outputs::local`.
// ───────────────────────────────────────────────────────────────────────────

/// Le puits des mots **entiers** : l'extrémité qui reçoit des octets PCM déjà
/// au format du périphérique, sans conversion.
///
/// C'est le pendant de [`PuitsDEchantillons`] pour les bras exclusifs : là où
/// le puits flottant reçoit des `f32` sortis du DSP, celui-ci reçoit un
/// [`BlocPcm`] — des octets et le format qui leur donne un sens — et les range
/// **octet pour octet** là où le pilote viendra les chercher. C'est ce qui
/// laisse une trame DoP traverser avec son marqueur.
///
/// # Ce que ce trait ne fait PAS
///
/// * Aucune conversion : ni profondeur, ni cadence, ni canaux. Le format est
///   convenu à l'ouverture, hors de ce contrat, et un bloc qui ne le respecte
///   pas est un défaut du producteur — voir [`CaptureOutputNatif`] pour la
///   réponse que la capture y donne.
/// * Aucune décision DoP : le puits ne renifle pas les marqueurs et ne sait
///   pas s'il porte du DSD ou du PCM. C'est le producteur qui le sait, et le
///   DAC qui le reconnaît.
/// * Il ne remplace pas le puits flottant : le chemin DSP reste
///   [`PuitsDEchantillons`].
///
/// Le trait ne dit rien du rythme, comme son jumeau : un puits peut bloquer,
/// écrire sans jamais bloquer, ou ne rien faire du tout.
pub trait PuitsNatif {
    /// Range `bloc` — des octets PCM entrelacés, au format de sortie convenu.
    ///
    /// Rend `false` **uniquement** quand le puits a cessé de consommer et que
    /// le producteur doit se démonter : rappel mort, périphérique arraché.
    /// Rend `true` dans tous les autres cas, **y compris un arrêt demandé** —
    /// le producteur détecte l'arrêt par ses propres témoins, jamais par cette
    /// valeur. Confondre les deux ferait passer une pause pour une panne.
    fn ecrire(&mut self, bloc: BlocPcm<'_>) -> bool;
}

/// Pourquoi [`CaptureOutputNatif`] a refusé un bloc.
///
/// **Aucun bras `_` nulle part** : à l'image de `MixError` (R3), un motif
/// ajouté ici est réclamé par le compilateur partout où il change quelque
/// chose. Le motif est lisible par [`CaptureOutputNatif::dernier_refus`] et
/// s'affiche avec les deux formats en clair — celui attendu, celui venu —
/// parce qu'un « spec différente » sans les deux valeurs ne se diagnostique
/// pas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusNatif {
    /// Le bloc porte un autre [`AudioSpec`] que celui ouvert.
    ///
    /// Ce n'est pas une conversion manquée, c'est un producteur qui a changé
    /// d'étiquette sans rouvrir le puits. Continuer hacherait un mélange de
    /// deux formats, et l'empreinte ne dirait plus rien de l'un ni de l'autre.
    SpecDifferente {
        /// Le format fixé à l'ouverture.
        attendue: AudioSpec,
        /// Le format que le bloc refusé portait.
        recue: AudioSpec,
    },
}

impl std::fmt::Display for RefusNatif {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SpecDifferente { attendue, recue } => write!(
                f,
                "bloc refusé : spec {recue:?} reçue, spec {attendue:?} attendue à l'ouverture"
            ),
        }
    }
}

impl std::error::Error for RefusNatif {}

/// Le puits de capture des mots entiers : il **hache les octets livrés**, tels
/// quels, au format fixé à l'ouverture.
///
/// C'est [`CaptureOutput`] pour les bras exclusifs : même hachage
/// (`hacher_fnv1a`, une seule implémentation dans la caisse), mêmes comptes,
/// même contrat de `false`. Ce qui change est ce qu'il reçoit — un
/// [`BlocPcm`], et non des `f32` — et ce qu'il en fait : rien. Aucune
/// conversion, aucune décision DoP ; un marqueur `0x05`/`0xFA` entre et sort
/// sans être lu.
///
/// # Le format est fixé à l'ouverture, et un bloc qui le contredit est REFUSÉ
///
/// [`CaptureOutputNatif::ouvrir`] prend l'[`AudioSpec`] attendu. Un bloc dont
/// la spec diffère rend `false`, n'est **pas** haché, est compté dans
/// [`CaptureOutputNatif::blocs_refuses`], et laisse son motif dans
/// [`CaptureOutputNatif::dernier_refus`]. Le puits est alors **mort** : le
/// contrat de `ecrire` ne connaît qu'un sens à `false` — « le puits a cessé de
/// consommer » — et un refus qui rendrait `false` en restant vivant lui en
/// donnerait un second. Un producteur qui change d'étiquette sans rouvrir a
/// rompu le contrat ; ce que le puits mesure ensuite n'aurait plus de sens.
///
/// # Le reste non aligné est GARDÉ, jamais jeté
///
/// Un bloc dont la longueur n'est pas un multiple de la trame est haché
/// entièrement — FNV-1a avance octet par octet, les frontières de blocs lui
/// sont invisibles — et compté dans [`CaptureOutputNatif::blocs_non_alignes`]
/// d'après sa propre partition ([`BlocPcm::reste_non_aligne`] non vide). Les
/// octets qui ne complètent pas une trame sont retenus dans
/// [`CaptureOutputNatif::reste_en_attente`] et complétés par le bloc suivant :
/// les trames se comptent sur le flux, pas sur le bloc. C'est le geste que la
/// doc de [`BlocPcm`] réclame — « à REPORTER sur la lecture suivante » — fait
/// par le puits lui-même, pour qu'un témoin voie que rien n'a été perdu.
///
/// # Ce qu'il n'alloue pas
///
/// Un seul `Vec` de capacité `octets_par_trame() - 1`, à l'ouverture. Il ne
/// grandit jamais : le reste en attente tient toujours sous une trame. Aucune
/// allocation par bloc.
pub struct CaptureOutputNatif {
    spec: AudioSpec,
    empreinte: u64,
    octets: u64,
    trames: u64,
    blocs: u64,
    blocs_vides: u64,
    blocs_non_alignes: u64,
    blocs_refuses: u64,
    dernier_refus: Option<RefusNatif>,
    reste: Vec<u8>,
    vivant: bool,
}

impl CaptureOutputNatif {
    /// Ouvre le puits sur CE format : tout bloc qui en porte un autre sera
    /// refusé.
    pub fn ouvrir(spec: AudioSpec) -> Self {
        Self {
            spec,
            empreinte: EMPREINTE_DU_VIDE,
            octets: 0,
            trames: 0,
            blocs: 0,
            blocs_vides: 0,
            blocs_non_alignes: 0,
            blocs_refuses: 0,
            dernier_refus: None,
            reste: Vec::with_capacity(spec.octets_par_trame() - 1),
            vivant: true,
        }
    }

    /// Le format fixé à l'ouverture.
    pub fn spec(&self) -> AudioSpec {
        self.spec
    }

    /// L'empreinte FNV-1a de tous les octets acceptés, dans l'ordre de
    /// livraison, reste en attente compris.
    pub fn empreinte(&self) -> u64 {
        self.empreinte
    }

    /// Le nombre d'octets acceptés, reste en attente compris.
    pub fn octets(&self) -> u64 {
        self.octets
    }

    /// Le nombre de trames COMPLÈTES reçues, comptées sur le flux : un reste
    /// complété par le bloc suivant fait une trame, pas deux morceaux.
    pub fn trames(&self) -> u64 {
        self.trames
    }

    /// Le nombre d'appels à `ecrire`, blocs vides et blocs refusés compris.
    pub fn blocs(&self) -> u64 {
        self.blocs
    }

    /// Les appels à `ecrire` qui n'ont apporté aucun octet.
    pub fn blocs_vides(&self) -> u64 {
        self.blocs_vides
    }

    /// Les blocs acceptés dont la longueur n'était pas un multiple de la trame,
    /// d'après leur propre partition.
    pub fn blocs_non_alignes(&self) -> u64 {
        self.blocs_non_alignes
    }

    /// Les blocs refusés pour spec différente. Chacun a tué le puits.
    pub fn blocs_refuses(&self) -> u64 {
        self.blocs_refuses
    }

    /// Le motif du dernier refus, ou `None` si rien n'a été refusé.
    pub fn dernier_refus(&self) -> Option<RefusNatif> {
        self.dernier_refus
    }

    /// Les octets reçus qui ne complètent pas encore une trame. Toujours plus
    /// court qu'une trame.
    pub fn reste_en_attente(&self) -> &[u8] {
        &self.reste
    }

    /// La durée acceptée, en millisecondes, à la cadence ouverte.
    pub fn duree_livree_ms(&self) -> u64 {
        if self.spec.cadence() == 0 {
            return 0;
        }
        self.trames * 1000 / u64::from(self.spec.cadence())
    }

    /// Déclare le puits mort : les écritures suivantes rendront `false`.
    pub fn declarer_mort(&mut self) {
        self.vivant = false;
    }

    /// Le puits consomme-t-il encore ?
    pub fn vivant(&self) -> bool {
        self.vivant
    }
}

impl PuitsNatif for CaptureOutputNatif {
    fn ecrire(&mut self, bloc: BlocPcm<'_>) -> bool {
        self.blocs += 1;
        if bloc.spec() != self.spec {
            self.blocs_refuses += 1;
            self.dernier_refus = Some(RefusNatif::SpecDifferente {
                attendue: self.spec,
                recue: bloc.spec(),
            });
            self.vivant = false;
            return false;
        }
        let octets = bloc.octets();
        if octets.is_empty() {
            self.blocs_vides += 1;
            return self.vivant;
        }
        if !bloc.reste_non_aligne().is_empty() {
            self.blocs_non_alignes += 1;
        }
        self.empreinte = hacher_fnv1a(self.empreinte, octets);
        self.octets += octets.len() as u64;

        // Les trames se comptent sur le flux : reste en attente + ce bloc.
        let par_trame = self.spec.octets_par_trame();
        let en_flux = self.reste.len() + octets.len();
        let trames = en_flux / par_trame;
        let reste = en_flux % par_trame;
        self.trames += trames as u64;
        if trames == 0 {
            // Le bloc entier tient sous une trame avec ce qui l'attendait :
            // le reste s'allonge, et reste sous `par_trame` par construction.
            self.reste.extend_from_slice(octets);
        } else {
            // Au moins une trame est complète : le nouveau reste est la queue
            // de CE bloc (`reste < par_trame <= octets.len() + reste.len()`
            // garantit `reste <= octets.len()`).
            self.reste.clear();
            self.reste
                .extend_from_slice(&octets[octets.len() - reste..]);
        }
        self.vivant
    }
}

#[async_trait::async_trait]
pub trait OutputTarget: Send + Sync {
    fn name(&self) -> &str;
    fn device_id(&self) -> &str;
    fn output_type(&self) -> &str;

    /// Contrat explicite des commandes optionnelles de cette sortie.
    ///
    /// Le défaut `version == 0`, volontairement conservateur, garde les
    /// plugins externes source-compatibles sans leur inventer des capacités.
    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities::default()
    }

    /// Whether this output can seamlessly chain a track staged via
    /// `set_next_media()` from inside its own playback loop (true), or whether
    /// it relies on the poller's natural-end fallback to advance the queue
    /// (false).
    ///
    /// The poller must NOT arm gapless (`set_next_media` + the gapless guard)
    /// for outputs that return false: the staged track would be orphaned and
    /// the guard would suppress the natural-end advance, stalling playback —
    /// e.g. a single-track Repeat queue never loops. Local outputs in
    /// exclusive mode (ASIO / WASAPI exclusive) take a dedicated playback path
    /// that never consumes `next_media`, so they return false.
    fn supports_internal_gapless(&self) -> bool {
        self.capabilities().can_gapless
    }

    /// Whether the poller should stage the gapless next track as a LOCAL FILE
    /// (`set_next_media` with `file_path` set, resolved WITHOUT a transcode
    /// session) rather than as a transcoded HTTP URL.
    ///
    /// OAAT returns true while it is streaming native DSD: that path reads the
    /// raw `.dsf` from disk and cannot consume the orchestrator's DSD->PCM
    /// transcode URL, so arming the URL path would spin up an unconsumed decode
    /// that stalls (`dsd_streaming_send_timeout_10s`) and orphans the transition.
    /// Default false: every other output stages the transcoded URL as today.
    fn prefers_local_file_gapless(&self) -> bool {
        false
    }

    fn as_any(&self) -> &dyn std::any::Any {
        // Default: not dowcastable. Implementations that need downcast override this.
        &()
    }

    async fn play_url(
        &self,
        url: &str,
        mime_type: &str,
        title: Option<&str>,
        artist: Option<&str>,
    ) -> Result<(), String> {
        self.play_media(&PlayMedia {
            url,
            mime_type,
            title,
            artist,
            ..Default::default()
        })
        .await
    }

    async fn play_media(&self, _media: &PlayMedia<'_>) -> Result<(), String> {
        Err("not implemented".into())
    }

    async fn pause(&self) -> Result<(), String>;
    async fn resume(&self) -> Result<(), String>;
    async fn stop(&self) -> Result<(), String>;
    async fn seek(&self, position_ms: u64) -> Result<(), String>;
    async fn set_volume(&self, volume: f64) -> Result<(), String>;
    async fn set_mute(&self, muted: bool) -> Result<(), String>;
    async fn get_status(&self) -> Result<OutputStatus, String>;
    async fn is_available(&self) -> bool;

    /// Entrées contrôlées utilisées par l'hôte. La capacité est vérifiée avant
    /// tout appel au backend : une implémentation historique qui répondait
    /// `Ok(())` sans rien faire ne peut donc plus transformer un refus en
    /// succès.
    async fn checked_pause(&self) -> OutputCommandResult<()> {
        let command = OutputCommand::Pause;
        self.capabilities().require(command)?;
        self.pause()
            .await
            .map_err(|message| OutputCommandError::failed(command, message))
    }

    async fn checked_resume(&self) -> OutputCommandResult<()> {
        let command = OutputCommand::Resume;
        self.capabilities().require(command)?;
        self.resume()
            .await
            .map_err(|message| OutputCommandError::failed(command, message))
    }

    async fn checked_seek(&self, position_ms: u64) -> OutputCommandResult<()> {
        let command = OutputCommand::Seek;
        self.capabilities().require(command)?;
        self.seek(position_ms)
            .await
            .map_err(|message| OutputCommandError::failed(command, message))
    }

    async fn checked_set_volume(&self, volume: f64) -> OutputCommandResult<()> {
        let command = OutputCommand::SetVolume;
        self.capabilities().require(command)?;
        self.set_volume(volume)
            .await
            .map_err(|message| OutputCommandError::failed(command, message))
    }

    async fn checked_set_mute(&self, muted: bool) -> OutputCommandResult<()> {
        let command = OutputCommand::SetMute;
        self.capabilities().require(command)?;
        self.set_mute(muted)
            .await
            .map_err(|message| OutputCommandError::failed(command, message))
    }

    /// A fatal error the output hit on its own, outside any call we made.
    ///
    /// Push-based outputs do their work on a background thread: by the time
    /// the device refuses to open, `play_url()` has long since returned `Ok`.
    /// Without a channel like this one the failure stays invisible while the
    /// UI shows a track advancing in total silence (Yacine, 8 Aug 2026: a DAC
    /// his account had no permission to open, and an hour spent looking for
    /// the cause because nothing said so).
    ///
    /// This doc used to promise a safety net that does not exist for a local
    /// output: "the poller's stall heuristics give up — roughly 73 seconds
    /// later". Measured on #3108: the frozen-position watchdog
    /// (`dlna_playing_stall_eligible`) is gated on `output_type == "dlna"`, so
    /// a local zone whose position stops advancing is caught by NOTHING and
    /// stays "playing" forever. This channel is not a shortcut to a slower
    /// path — for a local output it is the ONLY path.
    ///
    /// The message is user-facing and returned **once**: the implementation
    /// clears it, so the caller owns it and no stale error can kill the next
    /// track. Returning `None` — the default — means "nothing to report",
    /// which is correct for every output that reports failures synchronously.
    fn take_output_failure(&self) -> Option<String> {
        None
    }

    fn host(&self) -> Option<&str> {
        None
    }

    /// Set the ReplayGain factor for the track about to play (1.0 = untouched).
    ///
    /// Only outputs that render the audio themselves can honour this: a network
    /// renderer receives an already-encoded stream, so the gain is baked into
    /// the PCM before encoding instead. The default is deliberately a no-op —
    /// an output that ignores it plays at source level, which is the behaviour
    /// every output had before ReplayGain was applied at all.
    fn set_replaygain_factor(&self, _factor: f64) {}

    async fn set_next_url(
        &self,
        _url: &str,
        _mime_type: &str,
        _title: Option<&str>,
        _artist: Option<&str>,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn set_next_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        self.set_next_url(media.url, media.mime_type, media.title, media.artist)
            .await
    }

    fn diagnostics_json(&self) -> Option<serde_json::Value> {
        None
    }

    /// Actual signal contract observed by the output while rendering.
    ///
    /// `None` means that this output does not expose a runtime observation;
    /// callers may retain their existing static description in that case.
    fn signal_path_status(&self) -> Option<OutputSignalPathStatus> {
        None
    }

    /// Compteurs DSP de la piste courante, quand la sortie peut les observer.
    fn dsp_metrics(&self) -> Option<OutputDspMetrics> {
        None
    }
    /// Famine de l'anneau observée par le rappel temps réel de cette sortie
    /// (#3205), ou `None` quand la sortie ne rend pas l'audio elle-même — un
    /// renderer réseau reçoit un flux déjà encodé et n'a aucun anneau à
    /// affamer. Défaut `None`, donc une sortie hors-arbre existante compile et
    /// se comporte exactement comme avant.
    fn ring_starvation(&self) -> Option<OutputRingStarvation> {
        None
    }
}

/// A source of out-of-tree outputs, handed to the server at startup.
///
/// This is the seam that lets a *private* output crate (e.g. tune-diretta —
/// the Diretta Host SDK cannot ship in a public build) plug into the public
/// server without the public workspace ever referencing it: the private repo
/// builds its own composer binary that calls
/// `tune_server::bootstrap::run_with(RunOptions { output_providers, .. })`.
/// The server polls `discover()` at startup and then periodically, registers
/// each returned output in the output registry, and gives it the same zone
/// lifecycle as built-in discovery (reconnect, auto-create, hidden zones).
#[async_trait::async_trait]
pub trait OutputProvider: Send + Sync {
    /// Short provider name for logs (e.g. "diretta").
    fn provider_name(&self) -> &str;

    /// The paid module id this provider needs (e.g. `"diretta"`), or `None`
    /// for a free provider. **Declare it if you are a paid SKU.**
    ///
    /// Returning an empty list from [`discover`](Self::discover) when the
    /// module is not owned is correct, but it is indistinguishable from a
    /// provider that is absent, mis-compiled, or on a network that does not
    /// answer — and a beta tester of the Diretta module reinstalled his whole
    /// system over exactly that ambiguity (#2392). Declaring the module here
    /// lets the SERVER say, in the logs and in `/system/diagnostics`, that the
    /// provider is idle *because a paid entitlement is missing* and which one.
    ///
    /// Default `None`, so an existing out-of-tree provider keeps compiling and
    /// behaving exactly as before; opting in is a one-line change.
    fn required_module(&self) -> Option<&str> {
        None
    }

    /// Discover the devices reachable right now and build one [`OutputTarget`]
    /// per device. Return every visible device on each call — the server skips
    /// device_ids that are already registered.
    ///
    /// `ctx` carries the server-side runtime state a paid module needs —
    /// today the module entitlements: a provider that is a paid SKU must
    /// check [`ProviderContext::module_licensed`] and return an empty list
    /// when its module is not owned. The server rebuilds the context on
    /// every poll, so buying a module takes effect without a restart.
    async fn discover(&self, ctx: &ProviderContext) -> Vec<Box<dyn OutputTarget>>;
}

/// Runtime context handed to [`OutputProvider::discover`] on every poll.
///
/// Deliberately a plain data snapshot (not a handle into tune-core) so that
/// out-of-tree provider crates only ever depend on this contract crate.
#[derive(Debug, Clone, Default)]
pub struct ProviderContext {
    /// Stable ids of the paid modules the linked account owns (e.g.
    /// "diretta"), as validated by the license layer. Empty when the account
    /// owns none, is signed out, or the server runs unlicensed.
    pub licensed_modules: Vec<String>,
}

impl ProviderContext {
    /// Whether the account owns the paid module `id` (e.g. "diretta").
    pub fn module_licensed(&self, id: &str) -> bool {
        self.licensed_modules.iter().any(|m| m == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_plugin_capabilities_fail_closed() {
        let capabilities = OutputCapabilities::default();
        assert_eq!(capabilities.version, 0);
        for command in [
            OutputCommand::Pause,
            OutputCommand::Resume,
            OutputCommand::Seek,
            OutputCommand::SetVolume,
            OutputCommand::SetMute,
        ] {
            assert_eq!(
                capabilities.require(command),
                Err(OutputCommandError::Unsupported { command })
            );
        }
    }

    #[test]
    fn v1_contract_is_stable_and_machine_readable() {
        let capabilities = OutputCapabilities::v1(true, true, false, true, false, false);
        let json = serde_json::to_value(&capabilities).unwrap();

        assert_eq!(json["version"], OUTPUT_CAPABILITIES_VERSION);
        assert_eq!(json["can_pause"], true);
        assert_eq!(json["can_seek"], false);
        assert_eq!(
            capabilities.require(OutputCommand::Seek),
            Err(OutputCommandError::Unsupported {
                command: OutputCommand::Seek,
            })
        );
    }
}

#[cfg(test)]
mod resolution_de_volume {
    use super::*;

    /// Le chiffre qui a motivé tout ce module : sur une grille de 100 pas, le
    /// plus petit niveau audible vaut −40 dB. Une consigne au-dessous ne
    /// baisse pas le son, elle le coupe.
    #[test]
    fn le_pour_cent_plafonne_la_finesse_a_moins_quarante_db() {
        let pour_cent = VolumeResolution::Linear { steps: 100 };
        assert_eq!(pour_cent.smallest_audible(), Some(0.01));
        let plancher = pour_cent.floor_db().expect("une grille a un plancher");
        assert!((plancher - (-40.0)).abs() < 1e-12, "{plancher}");
    }

    /// La frontière, des deux côtés, sur les valeurs que produit vraiment
    /// `db_to_linear` — pas sur des littéraux choisis pour tomber juste.
    #[test]
    fn la_frontiere_du_pour_cent_tient_des_deux_cotes() {
        let pour_cent = VolumeResolution::Linear { steps: 100 };
        let lineaire = |db: f64| 10f64.powf(db / 20.0);
        // −40 dB EST le plancher : il doit être accepté par le contrôle qui
        // l'annonce, malgré l'inexactitude de 10^(-2) en binaire.
        assert!(pour_cent.holds(lineaire(-40.0)), "le plancher annoncé");
        assert!(
            pour_cent.holds(lineaire(-18.0)),
            "−18 dB, la cible de l'issue"
        );
        assert!(pour_cent.holds(lineaire(0.0)));
        // Au-dessous, la valeur envoyée s'arrondirait à zéro.
        assert!(!pour_cent.holds(lineaire(-40.1)));
        assert!(!pour_cent.holds(lineaire(-46.1)));
        assert!(!pour_cent.holds(lineaire(-50.0)));
        // Le silence reste tenable : c'est le mute, pas un arrondi raté.
        assert!(pour_cent.holds(0.0));
    }

    /// Une grille fine ou continue ne refuse rien : le garde-fou ne doit pas
    /// mordre là où le matériel suit.
    #[test]
    fn les_grilles_fines_ne_refusent_rien_d_audible() {
        for resolution in [
            VolumeResolution::Continuous,
            VolumeResolution::Decibels { step_mdb: 100 },
            VolumeResolution::Linear { steps: 65536 },
            VolumeResolution::Linear { steps: 1000 },
        ] {
            assert!(resolution.holds(10f64.powf(-50.0 / 20.0)), "{resolution:?}");
        }
        // Le millième de la sortie locale s'arrête tout de même à −60 dB.
        let locale = VolumeResolution::Linear { steps: 1000 };
        let plancher = locale.floor_db().expect("grille");
        assert!((plancher - (-60.0)).abs() < 1e-12, "{plancher}");
        assert!(!locale.holds(10f64.powf(-61.0 / 20.0)));
        // Ni le continu ni les dB n'ont de plancher de représentation.
        assert_eq!(VolumeResolution::Continuous.floor_db(), None);
        assert_eq!(
            VolumeResolution::Decibels { step_mdb: 100 }.floor_db(),
            None
        );
    }

    /// Le champ est ADDITIF : une charge utile écrite avant son existence se
    /// relit sans erreur, et vaut « continu », donc ne refuse rien.
    #[test]
    fn une_charge_utile_sans_le_champ_reste_lisible_et_permissive() {
        let ancien = serde_json::json!({
            "version": 1,
            "can_pause": true,
            "can_resume": true,
            "can_seek": true,
            "can_set_volume": true,
            "can_mute": true,
            "can_gapless": false,
        });
        let capabilities: OutputCapabilities = serde_json::from_value(ancien).unwrap();
        assert_eq!(capabilities.volume_resolution, VolumeResolution::Continuous);
        assert!(capabilities.volume_resolution.holds(0.0001));
    }

    /// Et il est lisible par un client : la grille sort nommée, avec son
    /// nombre de pas, pas sous forme d'un entier opaque.
    #[test]
    fn la_grille_est_publiee_sous_un_nom_lisible() {
        let capabilities =
            OutputCapabilities::v1(true, true, true, true, true, false).with_percent_volume();
        let json = serde_json::to_value(&capabilities).unwrap();
        assert_eq!(json["volume_resolution"]["kind"], "linear");
        assert_eq!(json["volume_resolution"]["steps"], 100);
        let airplay =
            OutputCapabilities::v1(true, true, false, true, true, false).with_decibel_volume(100);
        let json = serde_json::to_value(&airplay).unwrap();
        assert_eq!(json["volume_resolution"]["kind"], "decibels");
        assert_eq!(json["volume_resolution"]["step_mdb"], 100);
    }
}
