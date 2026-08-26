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
        true
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

    /// A fatal error the output hit on its own, outside any call we made.
    ///
    /// Push-based outputs do their work on a background thread: by the time
    /// the device refuses to open, `play_url()` has long since returned `Ok`.
    /// Without a channel like this one the failure stays invisible until the
    /// poller's stall heuristics give up — roughly 73 seconds later — and
    /// meanwhile the UI shows a track advancing in total silence (Yacine,
    /// 8 Aug 2026: a DAC his account had no permission to open, and an hour
    /// spent looking for the cause because nothing said so).
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
