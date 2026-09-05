use super::*;

pub(super) struct ZonePollState {
    pub(super) gapless_sent: bool,
    pub(super) stopped_ticks: u8,
    /// Ticks consecutifs ou le renderer rapporte une URI qui n'est pas la
    /// notre. Trois d'affilee avant de parler : une transition de piste peut
    /// montrer un instant l'URI precedente.
    pub(super) tenue_etrangere_ticks: u8,
    /// Le conflit a deja ete signale pour cette generation de piste : on ne
    /// harcele pas l'utilisateur a chaque tick.
    pub(super) tenue_signalee: bool,
    /// Ticks to ignore Stopped state after a gapless advance, so the
    /// poller doesn't re-send play_from_queue to a renderer that already
    /// transitioned via SetNextAVTransportURI.
    pub(super) gapless_cooldown: u8,
    /// Consecutive poll failures — used for exponential backoff.
    /// After N failures, skip 2^min(N,4) ticks before retrying.
    pub(super) consecutive_errors: u8,
    pub(super) backoff_remaining: u8,
    /// Comptabilité du journal (#2566), sans effet sur les deux champs
    /// ci-dessus : le recul et le compte d'erreurs sont tenus par le site
    /// d'appel, avant elle, et ce sont eux que lisent `poll_failed_past_end`
    /// et l'arrêt de zone. Voir [`JournalSondage`].
    pub(super) journal: JournalSondage,
    pub(super) total_polls: u64,
    pub(super) total_errors: u64,
    pub(super) last_latency_ms: u32,
    pub(super) max_latency_ms: u32,
    pub(super) last_radio_poll: Instant,
    /// When SetNextAVTransportURI was sent — used to guard against
    /// false track-end detection during gapless transitions on renderers
    /// like Eversolo DMP-A6 that briefly report Stopped or reset position.
    pub(super) gapless_sent_at: Option<Instant>,
    /// Last polled position in milliseconds — used to detect position
    /// resets (jumps from >30s to <5s) that signal a gapless transition.
    pub(super) last_position_ms: u64,
    /// Peak position reached in the current track — high-water mark used
    /// to verify that enough of the track was actually played before
    /// accepting a gapless transition.
    pub(super) peak_position_ms: u64,
    /// Identity key (`decisions::scrobble_track_key`) of the track already
    /// scrobbled, latched once it crosses the Last.fm threshold (50% / 4 min)
    /// so it scrobbles exactly once. A plain boolean here was only reset on
    /// `track_generation` changes, which gapless advances skip — so every
    /// gapless-reached track was silently dropped (#1113). The identity key
    /// re-arms on any track change regardless of the advance path.
    pub(super) scrobbled_key: Option<String>,
    /// Tick counter for throttling DB position saves.
    pub(super) ticks_since_db_save: u64,
    /// When the current track started playing (wall clock).
    /// Used to reject false gapless transitions that happen too soon.
    pub(super) track_started_at: Option<Instant>,
    /// The `ZoneState::last_seek_at` instant we last folded into
    /// `track_started_at`. A user seek moves the play position without moving
    /// the wall clock, which starves every wall-clock guard downstream
    /// (`played_enough`, `ended_naturally_wall_ok`): seek to the end right
    /// after start and the real track end is rejected as a spurious
    /// renderer signal — playback just stops instead of advancing (DEvir,
    /// v0.9.0-rc4). On each NEW seek we rewind `track_started_at` by the seek
    /// target so `wall_elapsed` matches "played at 1x from the start" again.
    pub(super) last_seek_seen: Option<Instant>,
    /// Tracks the `ZoneState::track_generation` we last observed.
    /// When the generation changes (new track started via `play()`),
    /// we reset all per-track state so stale values from the previous
    /// track cannot trigger false gapless advances or premature track ends.
    pub(super) track_generation: u64,
    /// When the orchestrator loaded the current track (track_generation changed).
    /// Used for the startup grace period — DLNA renderers report Stopped while
    /// buffering a new stream, especially after transcoding delays.
    pub(super) track_loaded_at: Instant,
    /// Counts ticks where the output reports Playing but position_ms has
    /// reached or exceeded the known track duration.  After
    /// POSITION_PAST_END_TICKS consecutive ticks in this state, the poller
    /// treats the track as ended even though the output hasn't reported
    /// Stopped.  This handles local/cpal outputs where the playback thread
    /// may be slow to set `playing = false`.
    pub(super) past_end_ticks: u8,
    /// Set to true after `gapless_natural_end_advancing_metadata` — the poller
    /// advanced metadata expecting the renderer to auto-transition.  If the
    /// renderer stays Stopped after gapless_cooldown expires, this flag lets
    /// the poller detect the stuck state and force a play_from_queue.
    pub(super) gapless_advance_pending: bool,
    /// Counts Stopped ticks after gapless_cooldown expires while
    /// gapless_advance_pending is true.  When this reaches
    /// GAPLESS_STUCK_THRESHOLD, the poller gives up on the gapless
    /// transition and forces play_from_queue.
    pub(super) gapless_stuck_ticks: u8,
    pub(super) last_bytes_sent: u64,
    /// Consecutive `Playing` polls with neither renderer position nor served
    /// bytes progressing. See `decisions::dlna_playing_stall_eligible`.
    pub(super) playing_stall_ticks: u8,
    /// Ticks CONSECUTIFS ou la position rapportee est a la fin de la piste — ou
    /// au-dela — alors que l'appareil annonce toujours jouer. Voir
    /// [`decisions::position_au_dela_de_la_duree`] et [`DEPASSEMENT_DUREE_TICKS`].
    ///
    /// Compte dans le bras `Playing` uniquement : une zone en pause n'y passe
    /// pas, donc une pause de vingt minutes ne gonfle pas ce compteur — c'est
    /// precisement ce que l'horloge murale (`track_started_at`, jamais repliee
    /// a la reprise) ne sait pas faire. Remis a zero des que la position quitte
    /// la zone de fin, et a chaque changement de piste.
    pub(super) depassement_duree_ticks: u8,
    /// Latch par piste : l'incoherence a deja ete DITE une fois (journal +
    /// metrique de zone). Sans lui la boucle ecrirait la meme ligne chaque
    /// seconde pendant tout le temps que dure le blocage.
    pub(super) depassement_duree_signale: bool,
    /// Ticks pendant lesquels on a refusé de conclure à une fin naturelle parce
    /// que le flux servi était manifestement incomplet (voir
    /// STALL_DECLINE_MAX_TICKS). Remis à zéro à chaque changement de piste.
    pub(super) stall_declines: u8,
    pub(super) radio_stopped_ticks: u8,
    /// Last position (ms) the renderer reported on the previous radio poll.
    /// An advancing position means the renderer is actually streaming even
    /// when it (mis)reports TransportState=Stopped for a live source — the
    /// Yamaha R-N2000A does this on MP3 ICEcast streams (AAC plays fine).
    pub(super) last_radio_position_ms: u64,
    /// Last volume the renderer reported (0.0–1.0) on a previous poll. Used to
    /// distinguish a real external volume change (the value moved) from a
    /// renderer that persistently reports a stale default (e.g. Devialet at
    /// 50%), which must not overwrite the user's saved volume.
    pub(super) last_device_volume: Option<f64>,
    /// Per-track latch for the DLNA poll-fail wall-clock end-of-track fallback
    /// (`decisions::poll_failed_past_end`). The Err poll branch can't remove the
    /// poll state (it holds a live borrow), so this ensures the fallback fires at
    /// most once per track. Cleared on every track-generation change.
    pub(super) wall_clock_end_fired: bool,
    /// Instrumentation latch (#1239): last `should_arm_gapless` decision we
    /// emitted in the `gapless_arm_trace` INFO line for the current track. The
    /// trace fires only when this value flips (arming window opens/closes) —
    /// `None` on a fresh track forces one line per track — so it never spams at
    /// the ~1 s tick rate. Read-only diagnostic; drives no playback decision.
    pub(super) gapless_arm_logged: Option<bool>,
    /// Verrou par piste (#2394) : la position de file pour laquelle
    /// `prepare_gapless` a constaté « suivant DSD sur DLNA, gapless refusé ».
    /// Sans lui, la fenêtre d'armement re-résout la piste suivante À CHAQUE
    /// tick — création puis destruction d'une session fichier par seconde
    /// pendant toute la fin d'une piste DSD (constaté sur DMP-A8, 96
    /// occurrences en 2 h). On ne peut PAS poser `gapless_sent = true` comme
    /// pour la sortie exclusive : sur DLNA, ce drapeau active les détecteurs
    /// de transition (durée/position) et le DMP-A8 rapporte des durées
    /// inexactes — fausse transition garantie. Cleared au changement de
    /// génération et à chaque transition, comme `gapless_arm_logged`.
    pub(super) gapless_dsd_skip_pos: Option<i64>,
    /// La LIGNE de file (`queue_items.id`) que le renderer a ACCEPTEE comme
    /// piste suivante, et la position qu'elle occupait alors (#3026).
    ///
    /// L'armement ne laissait aucune trace de ce qu'il avait arme. A la
    /// transition, le poller avancait donc sur `next_position()` — l'index+1
    /// COURANT — qui n'est plus la piste armee des que la file a bouge entre
    /// les deux. Un « Lire ensuite » dans les 30 dernieres secondes suffit : le
    /// renderer joue ce qu'on lui a envoye, l'ecran nomme l'inseree, et le
    /// compteur de fin de piste adopte la duree de l'INSEREE — d'ou la coupure
    /// de l'audio reellement en cours (`dlna_frozen_end=true`, journal Sandro
    /// du 01/09 a 14:23:10).
    pub(super) gapless_armed: Option<ArmedNext>,
}

impl ZonePollState {
    /// Etat de sondage neuf pour une zone qui vient d'entrer en lecture.
    ///
    /// Etait construit en ligne, champ par champ, a un seul endroit. Il en
    /// faut desormais deux — la zone avec peripherique et celle sans — et
    /// recopier vingt-neuf champs est le genre de chose qui diverge en
    /// silence.
    pub(super) fn new(track_generation: u64) -> Self {
        Self {
            gapless_sent: false,
            stopped_ticks: 0,
            tenue_etrangere_ticks: 0,
            tenue_signalee: false,
            gapless_cooldown: 0,
            consecutive_errors: 0,
            backoff_remaining: 0,
            journal: JournalSondage::default(),
            total_polls: 0,
            total_errors: 0,
            last_latency_ms: 0,
            max_latency_ms: 0,
            last_radio_poll: Instant::now(),
            gapless_sent_at: None,
            last_position_ms: 0,
            peak_position_ms: 0,
            scrobbled_key: None,
            ticks_since_db_save: 0,
            track_started_at: None,
            last_seek_seen: None,
            track_generation: track_generation,
            track_loaded_at: Instant::now(),
            past_end_ticks: 0,
            gapless_advance_pending: false,
            gapless_stuck_ticks: 0,
            last_bytes_sent: 0,
            playing_stall_ticks: 0,
            depassement_duree_ticks: 0,
            depassement_duree_signale: false,
            stall_declines: 0,
            radio_stopped_ticks: 0,
            last_radio_position_ms: 0,
            last_device_volume: None,
            wall_clock_end_fired: false,
            gapless_arm_logged: None,
            gapless_dsd_skip_pos: None,
            gapless_armed: None,
        }
    }
}

/// Issue de `prepare_gapless` : distinguer « rien à armer / échec (re-tenter
/// au prochain tick) » de « suivant DSD sur DLNA (inutile de re-tenter pour
/// cette position — verrou `gapless_dsd_skip_pos`, #2394) ».
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GaplessPrep {
    /// Le renderer a accepte la piste suivante. Porte la LIGNE de file
    /// reellement envoyee (`None` si la file n'a pas su la rendre) : c'est
    /// elle, et non l'index, qui decide ou avancer a la transition (#3026).
    Armed(Option<ArmedNext>),
    DsdNextSkipped,
    NotArmed,
}

/// Ce que le renderer a ACCEPTE comme piste suivante — a distinguer de ce que
/// la file designe comme suivante : les deux divergent des qu'on insere (#3026).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ArmedNext {
    /// `queue_items.id`. Stable quand `insert_at` decale les positions.
    pub(super) row_id: i64,
    /// La position occupee AU MOMENT de l'armement. Journalisee seule : elle
    /// dit de combien la file a glisse sous l'armement.
    pub(super) position: i64,
}
