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

use serde::{Deserialize, Serialize};

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
