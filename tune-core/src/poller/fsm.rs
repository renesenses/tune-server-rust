use super::{
    GAPLESS_STUCK_THRESHOLD, POSITION_PAST_END_TICKS, STOPPED_FAILURE_THRESHOLD,
    STOPPED_TICKS_THRESHOLD, decisions,
};

/// Ce que le sondeur SAIT du flux d'une zone, au moment où la garde
/// d'échec (#2394) décide de couper ou d'attendre.
///
/// ── Pourquoi trois états et pas un compteur ──
///
/// La garde lisait un `Option<u64>` et l'écrasait en `0` : `stream_id`
/// absent → `0`, session inconnue du gestionnaire de flux →
/// `unwrap_or(0)`. « Zéro octet servi » et « je ne sais pas » étaient donc
/// LE MÊME CHIFFRE — et c'est ce chiffre qui arme `force_stop`. Une zone
/// qui joue parfaitement mais dont le sondeur ignore le `stream_id` était
/// indiscernable d'une zone à sec, et coupée au bout de trente secondes
/// (DMP-A8, machine `.18`, #2394).
///
/// L'échappatoire « le renderer joue mais n'annonce pas son état »
/// (DMP-A10, LHC, Shanling…) était précisément désarmée par cette
/// confusion : elle exige `octets > 0`, ce qu'un `0` d'ignorance ne donne
/// jamais.
///
/// Le `stream_id` manque pour des raisons ORDINAIRES, pas seulement en cas
/// de panne : `advance_queue_metadata` — l'avance gapless, que le sondeur
/// s'appelle à lui-même — pose `stream_id: None` dans le now-playing, et
/// la reprise depuis la base en pose un aussi (cf. `stream_id_de_l_uri`,
/// #2991 : la base ne mémorise pas un identifiant de session). Une lecture
/// gapless sur un renderer DLNA passe donc structurellement en « inconnu »
/// dès la deuxième piste de l'album.
///
/// Vit ici, dans `fsm`, et non dans `decisions` : c'est le type d'un champ
/// de [`StoppedInput`], et `decisions` est `pub(crate)` — un champ public
/// portant un type privé ne serait ni nommable par une épreuve
/// d'intégration, ni propre au regard de `private_interfaces`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsommationFlux {
    /// Le compteur est connu ET il avance : le renderer tire des octets.
    Consomme,
    /// Le compteur est connu et n'avance pas : la zone est réellement à
    /// sec. C'est le SEUL état qui autorise la coupure.
    ASec,
    /// Personne ne sait : pas de `stream_id`, ou session inconnue du
    /// gestionnaire de flux. On n'a mesuré RIEN — pas « rien servi ».
    Inconnue,
}

impl ConsommationFlux {
    /// Étiquette stable pour le journal et le diagnostic. Un état qu'on ne
    /// peut pas observer se reconfond avec zéro à la première occasion :
    /// celui-ci se lit dans `journalctl -u tune-server | grep consommation`.
    pub fn etiquette(self) -> &'static str {
        match self {
            ConsommationFlux::Consomme => "consomme",
            ConsommationFlux::ASec => "a_sec",
            ConsommationFlux::Inconnue => "inconnue",
        }
    }

    /// Le compteur a-t-il été MESURÉ ? Seul un compteur mesuré autorise la
    /// coupure de la zone et le verdict « démarrage mort ».
    pub fn est_mesuree(self) -> bool {
        !matches!(self, ConsommationFlux::Inconnue)
    }
}

/// Qualifier la consommation d'un flux à partir d'un compteur qui a le
/// droit de ne pas savoir.
///
/// `octets_servis` vient de `streamer_bytes_sent`, qui rend `None` pour un
/// flux qu'il ne connaît pas ; l'appelant rend `None` aussi quand le
/// now-playing n'a pas de `stream_id`. Les deux ignorances se valent et
/// donnent [`ConsommationFlux::Inconnue`] — jamais `ASec`.
///
/// `octets_precedents` est le dernier compte MESURÉ : un tour « inconnu »
/// ne le remplace pas, sinon la reprise du `stream_id` relancerait la
/// comparaison depuis un faux zéro et un flux stable passerait pour
/// consommant.
pub fn consommation_flux(octets_servis: Option<u64>, octets_precedents: u64) -> ConsommationFlux {
    match octets_servis {
        None => ConsommationFlux::Inconnue,
        Some(octets) if octets > 0 && octets > octets_precedents => ConsommationFlux::Consomme,
        Some(_) => ConsommationFlux::ASec,
    }
}

/// Terminal decision of one poll tick when the output reports Stopped.
/// Each variant maps 1:1 to a branch of the `TransportState::Stopped` arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoppedOutcome {
    /// Tune is not playing on this zone — device Stopped is ignored.
    Ignore,
    /// Suppressed by the seek grace window.
    SuppressSeekGrace,
    /// Suppressed by the track-load grace window.
    SuppressLoadGrace,
    /// Suppressed by the post-gapless cooldown.
    SuppressCooldown,
    /// In the gapless guard but not enough played — ignore (false-skip guard).
    GuardStoppedIgnored,
    /// In the gapless guard, enough played — arm pending confirmation.
    GuardStoppedPending,
    /// Advance pending, renderer still stuck below threshold — keep waiting.
    StuckWaiting,
    /// Advance pending + stuck threshold reached — force track end.
    StuckForceEnd,
    /// Local output signalled natural EOF — track ended.
    LocalEndedNaturally,
    /// DSD-on-DLNA (gapless intentionally off) reached its end by peak
    /// position — advance now instead of waiting out the Stopped counter.
    DsdDlnaReachedEnd,
    /// Stopped-threshold reached + natural end, gapless armed — wait for transition.
    NaturalEndGaplessWaiting,
    /// Stopped-threshold reached + natural end, no gapless — advance track.
    NaturalEndAdvance,
    /// Failure threshold reached but the stream is still consuming — keep waiting.
    FailureWaitingConsuming,
    /// Seuil d'échec atteint, mais la consommation du flux est INCONNUE
    /// (pas de `stream_id`, ou session inconnue du gestionnaire de flux) —
    /// on attend (#2394). Couper une zone parce qu'on ne sait pas la
    /// mesurer est pire que le défaut qu'on croit prévenir.
    FailureWaitingUnknown,
    /// Failure threshold reached, stream idle — stop the zone.
    FailureStop,
    /// Below threshold, or above threshold without a natural end — accumulate.
    Waiting,
}

impl StoppedOutcome {
    /// Does this outcome conclude the track has ended (loop sets `track_ended`)?
    pub fn is_track_end(self) -> bool {
        matches!(
            self,
            StoppedOutcome::StuckForceEnd
                | StoppedOutcome::LocalEndedNaturally
                | StoppedOutcome::DsdDlnaReachedEnd
                | StoppedOutcome::NaturalEndAdvance
        )
    }

    /// Does this outcome stop the zone (loop sets `force_stop`)?
    pub fn is_force_stop(self) -> bool {
        matches!(self, StoppedOutcome::FailureStop)
    }
}

/// Snapshot of the inputs the Stopped arm reads, taken BEFORE the arm
/// mutates `ZonePollState`. Counters are pre-increment (the classifier
/// applies the `+1` the arm would).
#[derive(Debug, Clone, Copy)]
pub struct StoppedInput {
    pub tune_is_playing: bool,
    pub tune_has_track: bool,
    pub in_seek_grace: bool,
    pub in_track_load_grace: bool,
    pub gapless_cooldown: u8,
    pub in_gapless_guard: bool,
    pub played_enough: bool,
    pub gapless_advance_pending: bool,
    pub gapless_stuck_ticks: u8,
    pub ended_naturally: bool,
    pub wall_elapsed: u64,
    pub track_duration_ms: u64,
    pub stopped_ticks: u8,
    pub natural_end: bool,
    pub gapless_sent: bool,
    /// `OutputStatus::realtime` — `false` for an output that finishes a
    /// track faster than 1x (a recorder), which exempts it from the
    /// wall-clock plausibility guard on `ended_naturally`.
    pub realtime: bool,
    /// Whether the output can transition internally (live probe). For an
    /// exclusive local output or the OAAT direct-file loop, `gapless_sent`
    /// is only a re-arm suppressor — no internal transition ever comes, so
    /// the natural end must advance instead of waiting (the actual branch
    /// already probes this; without it the shadow predicted
    /// NaturalEndGaplessWaiting on every OAAT direct-path track end).
    pub can_internal_gapless: bool,
    /// Ce que le sondeur SAIT du flux — trois états, pas un compteur
    /// (#2394). `Inconnue` ne coupe pas.
    pub consommation: ConsommationFlux,
    /// Precomputed `decisions::dlna_dsd_reached_end` for this zone/track — a
    /// DSD track on a DLNA renderer whose peak position reached the end.
    /// Gapless is intentionally off for a DSD next on DLNA, and DLNA never
    /// reports `ended_naturally`, so without this the track only ends after
    /// `STOPPED_TICKS_THRESHOLD` polls (~5s gap).
    pub dlna_dsd_reached_end: bool,
}

/// Pure reproduction of the `TransportState::Stopped` arm's decision tree.
/// Branch order is significant and mirrors `tick()` exactly.
pub fn classify_stopped(i: &StoppedInput) -> StoppedOutcome {
    use StoppedOutcome::*;
    if !i.tune_is_playing || !i.tune_has_track {
        return Ignore;
    }
    if i.in_seek_grace {
        return SuppressSeekGrace;
    }
    if i.in_track_load_grace {
        return SuppressLoadGrace;
    }
    if i.gapless_cooldown > 0 {
        return SuppressCooldown;
    }
    if i.in_gapless_guard {
        return if !i.played_enough {
            GuardStoppedIgnored
        } else {
            GuardStoppedPending
        };
    }
    if i.gapless_advance_pending {
        return if i.gapless_stuck_ticks.saturating_add(1) >= GAPLESS_STUCK_THRESHOLD {
            StuckForceEnd
        } else {
            StuckWaiting
        };
    }
    if i.ended_naturally
        && (i.played_enough
            || !i.realtime
            || decisions::ended_naturally_wall_ok(i.wall_elapsed, i.track_duration_ms))
    {
        return LocalEndedNaturally;
    }
    if i.dlna_dsd_reached_end {
        // DSD-on-DLNA: gapless intentionally off and DLNA never reports
        // ended_naturally, so the peak reaching the end is the earliest
        // reliable end-of-track signal — advance now, ~4s before the
        // Stopped counter would.
        return DsdDlnaReachedEnd;
    }
    // Fallthrough: the arm increments stopped_ticks, then branches on it.
    let stopped_ticks = i.stopped_ticks.saturating_add(1);
    if stopped_ticks >= STOPPED_TICKS_THRESHOLD {
        if i.natural_end {
            return if i.gapless_sent && i.can_internal_gapless {
                NaturalEndGaplessWaiting
            } else {
                NaturalEndAdvance
            };
        }
        if stopped_ticks >= STOPPED_FAILURE_THRESHOLD {
            return match i.consommation {
                ConsommationFlux::Consomme => FailureWaitingConsuming,
                ConsommationFlux::Inconnue => FailureWaitingUnknown,
                ConsommationFlux::ASec => FailureStop,
            };
        }
        return Waiting;
    }
    Waiting
}

/// Decisions taken by the `Playing`/`Transitioning` arm. Unlike the Stopped
/// arm (a single-outcome tree), the Playing arm performs a *sequence* of
/// independent effects, so this is a bundle of flags, not one enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlayingDecision {
    /// (A) A gapless advance was pending and a next track exists — advance
    /// the queue metadata now.
    pub confirm_gapless_advance: bool,
    /// (B) A gapless transition to the next track was detected.
    pub transition_detected: bool,
    /// (C) Entered the final window, not armed, gapless enabled — arm SetNext.
    pub arm_gapless: bool,
    /// (D) Position ran past the end for POSITION_PAST_END_TICKS ticks —
    /// the arm sets track_ended.
    pub past_end_track_ended: bool,
}

/// Inputs read by the Playing arm, snapshot pre-mutation. `has_next` and
/// `gapless_enabled` are supplied by the caller (queue lookup / zone config).
#[derive(Debug, Clone, Copy)]
pub struct PlayingInput {
    pub gapless_advance_pending: bool,
    pub has_next: bool,
    pub gapless_sent: bool,
    pub track_duration_ms: u64,
    pub reported_duration_ms: u64,
    pub played_enough: bool,
    pub position_ms: u64,
    pub past_end_ticks: u8,
    pub gapless_enabled: bool,
    /// The zone's output is a DLNA renderer — enables the wall-clock
    /// end-of-track fallback for renderers reporting no duration.
    pub is_dlna: bool,
    /// Seconds elapsed since `track_started_at` (folded on seek).
    pub wall_elapsed_secs: u64,
}

/// Pure reproduction of the `Playing`/`Transitioning` arm's decisions.
/// Mirrors the arm's ordering: a detected transition (B) resets the
/// past-end tick counter before (D) is evaluated.
pub fn classify_playing(i: &PlayingInput) -> PlayingDecision {
    let confirm_gapless_advance = i.gapless_advance_pending && i.has_next;
    let transition_detected =
        decisions::duration_changed(i.gapless_sent, i.track_duration_ms, i.reported_duration_ms)
            && decisions::position_confirms_transition(
                i.played_enough,
                i.position_ms,
                i.track_duration_ms,
            );
    let arm_gapless = !transition_detected
        && decisions::should_arm_gapless(
            i.gapless_sent,
            i.reported_duration_ms,
            i.track_duration_ms,
            i.position_ms,
        )
        && i.gapless_enabled;
    // (B) resets past_end_ticks to 0 before (D) runs.
    let effective_past_end_ticks = if transition_detected {
        0
    } else {
        i.past_end_ticks
    };
    // (D) is reached either by a real position running past the end, or —
    // for a DLNA bridge that reports no position/duration — by the wall
    // clock passing the queue-known duration.
    let reached_end =
        decisions::past_end_reached(i.track_duration_ms, i.played_enough, i.position_ms)
            || decisions::wall_clock_past_end(
                i.is_dlna,
                i.reported_duration_ms,
                i.track_duration_ms,
                i.wall_elapsed_secs,
            )
            || decisions::dlna_frozen_at_end_wall_clock(
                i.is_dlna,
                i.played_enough,
                i.track_duration_ms,
                i.position_ms,
                i.wall_elapsed_secs,
            );
    let past_end_track_ended =
        reached_end && effective_past_end_ticks.saturating_add(1) >= POSITION_PAST_END_TICKS;
    PlayingDecision {
        confirm_gapless_advance,
        transition_detected,
        arm_gapless,
        past_end_track_ended,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> StoppedInput {
        StoppedInput {
            tune_is_playing: true,
            tune_has_track: true,
            in_seek_grace: false,
            in_track_load_grace: false,
            gapless_cooldown: 0,
            in_gapless_guard: false,
            played_enough: false,
            gapless_advance_pending: false,
            gapless_stuck_ticks: 0,
            ended_naturally: false,
            realtime: true,
            wall_elapsed: 0,
            track_duration_ms: 0,
            stopped_ticks: 0,
            natural_end: false,
            gapless_sent: false,
            can_internal_gapless: true,
            // Base : compteur MESURÉ et à sec — c'est ce qui doit couper.
            consommation: ConsommationFlux::ASec,
            dlna_dsd_reached_end: false,
        }
    }

    #[test]
    fn volume_not_adopted_on_first_observation() {
        // No previous reading yet — never overwrite on the first poll.
        assert!(!super::super::decisions::should_adopt_device_volume(
            None, 0.5, 0.3
        ));
    }

    #[test]
    fn volume_not_adopted_when_device_reports_stale_default() {
        // Devialet keeps reporting 0.50 while the user saved 0.30 — the
        // value never moves, so the saved volume must be preserved (Fabien).
        assert!(!super::super::decisions::should_adopt_device_volume(
            Some(0.5),
            0.5,
            0.3
        ));
    }

    #[test]
    fn volume_adopted_on_real_device_change() {
        // The knob moved on the device (0.50 -> 0.62) and now differs from
        // the saved volume — adopt it.
        assert!(super::super::decisions::should_adopt_device_volume(
            Some(0.5),
            0.62,
            0.3
        ));
    }

    #[test]
    fn volume_not_adopted_when_change_matches_saved() {
        // Device moved but landed on what we already have stored.
        assert!(!super::super::decisions::should_adopt_device_volume(
            Some(0.5),
            0.62,
            0.62
        ));
    }

    #[test]
    fn ignore_when_tune_not_playing() {
        assert_eq!(
            classify_stopped(&StoppedInput {
                tune_is_playing: false,
                ..base()
            }),
            StoppedOutcome::Ignore
        );
        assert_eq!(
            classify_stopped(&StoppedInput {
                tune_has_track: false,
                ..base()
            }),
            StoppedOutcome::Ignore
        );
    }

    #[test]
    fn natural_end_advances_when_output_cannot_chain_internally() {
        // gapless_sent posé par le chemin « skip » (sortie exclusive ou
        // boucle directe OAAT) : pas de transition interne possible — la
        // fin naturelle doit avancer, pas attendre (divergence shadow-FSM
        // observée à chaque fin de piste locale OAAT, 29/07).
        let i = StoppedInput {
            natural_end: true,
            gapless_sent: true,
            can_internal_gapless: false,
            stopped_ticks: STOPPED_TICKS_THRESHOLD,
            ..base()
        };
        assert_eq!(classify_stopped(&i), StoppedOutcome::NaturalEndAdvance);

        // Avec transition interne possible, le comportement historique reste.
        let i2 = StoppedInput {
            can_internal_gapless: true,
            ..i
        };
        assert_eq!(
            classify_stopped(&i2),
            StoppedOutcome::NaturalEndGaplessWaiting
        );
    }

    #[test]
    fn grace_windows_suppress() {
        assert_eq!(
            classify_stopped(&StoppedInput {
                in_seek_grace: true,
                ..base()
            }),
            StoppedOutcome::SuppressSeekGrace
        );
        assert_eq!(
            classify_stopped(&StoppedInput {
                in_track_load_grace: true,
                ..base()
            }),
            StoppedOutcome::SuppressLoadGrace
        );
        assert_eq!(
            classify_stopped(&StoppedInput {
                gapless_cooldown: 3,
                ..base()
            }),
            StoppedOutcome::SuppressCooldown
        );
    }

    #[test]
    fn seek_grace_beats_load_grace() {
        let i = StoppedInput {
            in_seek_grace: true,
            in_track_load_grace: true,
            ..base()
        };
        assert_eq!(classify_stopped(&i), StoppedOutcome::SuppressSeekGrace);
    }

    #[test]
    fn gapless_guard_branches_on_played_enough() {
        assert_eq!(
            classify_stopped(&StoppedInput {
                in_gapless_guard: true,
                played_enough: false,
                ..base()
            }),
            StoppedOutcome::GuardStoppedIgnored
        );
        assert_eq!(
            classify_stopped(&StoppedInput {
                in_gapless_guard: true,
                played_enough: true,
                ..base()
            }),
            StoppedOutcome::GuardStoppedPending
        );
    }

    #[test]
    fn stuck_waits_then_forces_end() {
        // GAPLESS_STUCK_THRESHOLD = 2. pre=0 → +1=1 < 2 → wait.
        assert_eq!(
            classify_stopped(&StoppedInput {
                gapless_advance_pending: true,
                gapless_stuck_ticks: 0,
                ..base()
            }),
            StoppedOutcome::StuckWaiting
        );
        // pre=1 → +1=2 >= 2 → force end.
        assert_eq!(
            classify_stopped(&StoppedInput {
                gapless_advance_pending: true,
                gapless_stuck_ticks: 1,
                ..base()
            }),
            StoppedOutcome::StuckForceEnd
        );
    }

    #[test]
    fn local_ended_naturally_paths() {
        // played_enough qualifies.
        assert_eq!(
            classify_stopped(&StoppedInput {
                ended_naturally: true,
                played_enough: true,
                ..base()
            }),
            StoppedOutcome::LocalEndedNaturally
        );
        // wall_elapsed >= 5 also qualifies.
        assert_eq!(
            classify_stopped(&StoppedInput {
                ended_naturally: true,
                wall_elapsed: 5,
                ..base()
            }),
            StoppedOutcome::LocalEndedNaturally
        );
        // ended_naturally but too early and not played_enough → falls through.
        assert_ne!(
            classify_stopped(&StoppedInput {
                ended_naturally: true,
                wall_elapsed: 4,
                ..base()
            }),
            StoppedOutcome::LocalEndedNaturally
        );
    }

    /// The recorder case, mirroring
    /// `natural_end_non_realtime_output_skips_the_wall_guard`: a
    /// `realtime: false` output that says the track is done is believed
    /// immediately, at the same inputs the DMP-A8 guard rejects.
    #[test]
    fn non_realtime_ended_naturally_is_immediate() {
        assert_eq!(
            classify_stopped(&StoppedInput {
                ended_naturally: true,
                played_enough: false,
                wall_elapsed: 1,
                track_duration_ms: 300_000,
                realtime: false,
                ..base()
            }),
            StoppedOutcome::LocalEndedNaturally
        );
        // Identical inputs from a renderer: still rejected.
        assert_ne!(
            classify_stopped(&StoppedInput {
                ended_naturally: true,
                played_enough: false,
                wall_elapsed: 1,
                track_duration_ms: 300_000,
                realtime: true,
                ..base()
            }),
            StoppedOutcome::LocalEndedNaturally
        );
    }

    #[test]
    fn dmp_a8_false_ended_naturally_rejected() {
        // DMP-A8 regression: renderer falsely reports ended_naturally ~35s
        // into a 4-minute (240s) track (played_enough false). Wall-clock is
        // far below MIN_WALL_FRACTION_FOR_NATURAL_END·duration → NOT an end.
        assert_ne!(
            classify_stopped(&StoppedInput {
                ended_naturally: true,
                played_enough: false,
                wall_elapsed: 35,
                track_duration_ms: 240_000,
                ..base()
            }),
            StoppedOutcome::LocalEndedNaturally
        );
        // A genuine end near the track duration is still trusted.
        assert_eq!(
            classify_stopped(&StoppedInput {
                ended_naturally: true,
                played_enough: false,
                wall_elapsed: 200,
                track_duration_ms: 240_000,
                ..base()
            }),
            StoppedOutcome::LocalEndedNaturally
        );
    }

    #[test]
    fn below_threshold_waits() {
        // STOPPED_TICKS_THRESHOLD = 5. pre=0 → +1=1 < 5 → waiting.
        assert_eq!(classify_stopped(&base()), StoppedOutcome::Waiting);
    }

    #[test]
    fn natural_end_advances_without_gapless() {
        let i = StoppedInput {
            stopped_ticks: 4,
            natural_end: true,
            gapless_sent: false,
            ..base()
        };
        assert_eq!(classify_stopped(&i), StoppedOutcome::NaturalEndAdvance);
        assert!(classify_stopped(&i).is_track_end());
    }

    #[test]
    fn natural_end_waits_when_gapless_armed() {
        let i = StoppedInput {
            stopped_ticks: 4,
            natural_end: true,
            gapless_sent: true,
            ..base()
        };
        assert_eq!(
            classify_stopped(&i),
            StoppedOutcome::NaturalEndGaplessWaiting
        );
        assert!(!classify_stopped(&i).is_track_end());
    }

    #[test]
    fn dsd_dlna_reached_end_advances_before_stopped_counter() {
        // DSD on DLNA reached its end (peak): advance now, at stopped_ticks=0,
        // without waiting out STOPPED_TICKS_THRESHOLD. This keeps the FSM
        // shadow model in sync with the imperative arm's fast path.
        let i = StoppedInput {
            stopped_ticks: 0,
            dlna_dsd_reached_end: true,
            ..base()
        };
        assert_eq!(classify_stopped(&i), StoppedOutcome::DsdDlnaReachedEnd);
        assert!(classify_stopped(&i).is_track_end());
    }

    #[test]
    fn dsd_dlna_end_yields_to_ended_naturally() {
        // A local-style ended_naturally still wins (branch order preserved).
        let i = StoppedInput {
            dlna_dsd_reached_end: true,
            ended_naturally: true,
            played_enough: true,
            ..base()
        };
        assert_eq!(classify_stopped(&i), StoppedOutcome::LocalEndedNaturally);
    }

    // #2394 — ces deux épreuves épinglaient le booléen `stream_consuming`,
    // qui ne connaissait que deux états. Elles ne sont pas supprimées :
    // elles disent MAINTENANT la même chose sur le troisième état près,
    // `ConsommationFlux`, et `octets_servis_inconnus_2394.rs` ajoute
    // l'épreuve du cas « inconnue » qu'un booléen ne pouvait pas exprimer.
    #[test]
    fn failure_stops_when_idle_past_failure_threshold() {
        // STOPPED_FAILURE_THRESHOLD = 30. pre=29 → +1=30, not natural, idle.
        // « À sec » = compteur MESURÉ qui n'avance pas : ça coupe toujours.
        let i = StoppedInput {
            stopped_ticks: 29,
            natural_end: false,
            consommation: ConsommationFlux::ASec,
            ..base()
        };
        assert_eq!(classify_stopped(&i), StoppedOutcome::FailureStop);
        assert!(classify_stopped(&i).is_force_stop());
    }

    #[test]
    fn failure_waits_when_stream_consuming() {
        let i = StoppedInput {
            stopped_ticks: 29,
            natural_end: false,
            consommation: ConsommationFlux::Consomme,
            ..base()
        };
        assert_eq!(
            classify_stopped(&i),
            StoppedOutcome::FailureWaitingConsuming
        );
        assert!(!classify_stopped(&i).is_force_stop());
    }

    /// #2394 — le cas que le booléen ne savait pas dire : consommation
    /// INCONNUE au seuil d'échec. La zone attend, elle n'est pas coupée.
    #[test]
    fn failure_waits_when_consumption_unknown() {
        let i = StoppedInput {
            stopped_ticks: 29,
            natural_end: false,
            consommation: ConsommationFlux::Inconnue,
            ..base()
        };
        assert_eq!(classify_stopped(&i), StoppedOutcome::FailureWaitingUnknown);
        assert!(!classify_stopped(&i).is_force_stop());
    }

    #[test]
    fn between_thresholds_waits() {
        // pre=10 → +1=11, >=5 but <30, not natural_end → Waiting.
        let i = StoppedInput {
            stopped_ticks: 10,
            natural_end: false,
            ..base()
        };
        assert_eq!(classify_stopped(&i), StoppedOutcome::Waiting);
    }

    fn pbase() -> PlayingInput {
        PlayingInput {
            gapless_advance_pending: false,
            has_next: true,
            gapless_sent: false,
            track_duration_ms: 300_000,
            reported_duration_ms: 300_000,
            played_enough: false,
            position_ms: 0,
            past_end_ticks: 0,
            gapless_enabled: true,
            is_dlna: false,
            wall_elapsed_secs: 0,
        }
    }

    #[test]
    fn playing_confirm_gapless_advance() {
        assert!(
            classify_playing(&PlayingInput {
                gapless_advance_pending: true,
                has_next: true,
                ..pbase()
            })
            .confirm_gapless_advance
        );
        // pending but no next → no metadata advance.
        assert!(
            !classify_playing(&PlayingInput {
                gapless_advance_pending: true,
                has_next: false,
                ..pbase()
            })
            .confirm_gapless_advance
        );
    }

    #[test]
    fn playing_transition_detected_requires_armed() {
        let armed = PlayingInput {
            gapless_sent: true,
            track_duration_ms: 200_000,
            reported_duration_ms: 210_000,
            played_enough: true,
            position_ms: 2_000,
            ..pbase()
        };
        assert!(classify_playing(&armed).transition_detected);
        // Not armed → duration_changed is false → no transition.
        assert!(
            !classify_playing(&PlayingInput {
                gapless_sent: false,
                ..armed
            })
            .transition_detected
        );
    }

    #[test]
    fn playing_arm_gapless_gated_by_enabled_and_not_transitioning() {
        let i = PlayingInput {
            gapless_sent: false,
            reported_duration_ms: 300_000,
            position_ms: 275_000,
            gapless_enabled: true,
            ..pbase()
        };
        let d = classify_playing(&i);
        assert!(d.arm_gapless && !d.transition_detected);
        // Disabled for the zone → don't arm.
        assert!(
            !classify_playing(&PlayingInput {
                gapless_enabled: false,
                ..i
            })
            .arm_gapless
        );
    }

    #[test]
    fn playing_past_end_advances_after_threshold() {
        // POSITION_PAST_END_TICKS = 3. pre=2 → +1=3 >= 3, past end reached.
        let i = PlayingInput {
            track_duration_ms: 240_000,
            played_enough: true,
            position_ms: 244_000,
            past_end_ticks: 2,
            ..pbase()
        };
        assert!(classify_playing(&i).past_end_track_ended);
        // pre=1 → +1=2 < 3 → not yet.
        assert!(
            !classify_playing(&PlayingInput {
                past_end_ticks: 1,
                ..i
            })
            .past_end_track_ended
        );
    }

    #[test]
    fn un_dmp_fige_a_la_duree_avance_par_horloge_murale() {
        // Villerio, DMP-A6, 25/08 22:24 : SetNext acquitté mais jamais
        // honoré ; le renderer reste PLAYING, position figée EXACTEMENT à
        // la durée (373000 — les stale_start_position du journal), durée
        // rapportée inchangée. past_end_reached exige position > durée+3s,
        // wall_clock_past_end exige durée rapportée nulle : aucun ne tire,
        // la piste ne finit jamais. L'horloge murale de Tune, elle, sait.
        let i = PlayingInput {
            track_duration_ms: 373_000,
            reported_duration_ms: 373_000,
            played_enough: true,
            position_ms: 373_000,
            past_end_ticks: 2,
            gapless_sent: true,
            is_dlna: true,
            wall_elapsed_secs: 380,
            ..pbase()
        };
        assert!(classify_playing(&i).past_end_track_ended);

        // Une VRAIE transition gapless à durées voisines remet la position
        // près de zéro : la position n'est plus épinglée en zone de fin,
        // le filet ne doit pas tirer (sinon double-play).
        assert!(
            !classify_playing(&PlayingInput {
                position_ms: 3_000,
                ..i
            })
            .past_end_track_ended
        );
        // Gel en PLEIN MILIEU de piste (vrai blocage réseau) : pic < 80 %,
        // on ne conclut pas une fin.
        assert!(
            !classify_playing(&PlayingInput {
                played_enough: false,
                position_ms: 180_000,
                wall_elapsed_secs: 380,
                ..i
            })
            .past_end_track_ended
        );
        // L'horloge n'a pas encore dépassé durée + marge : on attend.
        assert!(
            !classify_playing(&PlayingInput {
                wall_elapsed_secs: 370,
                ..i
            })
            .past_end_track_ended
        );
    }

    #[test]
    fn playing_transition_resets_past_end_counter() {
        // Past-end IS reached, but a detected transition resets the counter
        // to 0 before (D), so no past-end advance this tick.
        let i = PlayingInput {
            gapless_sent: true,
            track_duration_ms: 240_000,
            reported_duration_ms: 250_000,
            played_enough: true,
            position_ms: 244_000,
            past_end_ticks: 5,
            ..pbase()
        };
        let d = classify_playing(&i);
        assert!(d.transition_detected);
        assert!(!d.past_end_track_ended);
        // Without a transition, the pre-tick counter stands: 5+1 >= 3 → advance.
        let d2 = classify_playing(&PlayingInput {
            gapless_sent: false,
            ..i
        });
        assert!(!d2.transition_detected);
        assert!(d2.past_end_track_ended);
    }

    #[test]
    fn playing_dlna_wall_clock_past_end_advances() {
        // LMS UPnP bridge: renderer reports duration 0 and a frozen
        // position, but Tune knows the queue duration (300s) and the wall
        // clock has passed duration + margin. POSITION_PAST_END_TICKS = 3,
        // pre=2 → +1=3 → advance. played_enough is false (peak frozen), which
        // must NOT block the wall-clock fallback.
        let i = PlayingInput {
            is_dlna: true,
            reported_duration_ms: 0,
            track_duration_ms: 300_000,
            position_ms: 0,
            played_enough: false,
            wall_elapsed_secs: 304,
            past_end_ticks: 2,
            ..pbase()
        };
        assert!(classify_playing(&i).past_end_track_ended);
    }

    #[test]
    fn playing_dlna_wall_clock_negatives() {
        let armed = PlayingInput {
            is_dlna: true,
            reported_duration_ms: 0,
            track_duration_ms: 300_000,
            position_ms: 0,
            played_enough: false,
            wall_elapsed_secs: 304,
            past_end_ticks: 2,
            ..pbase()
        };
        // Queue duration unknown (0) → no wall-clock advance.
        assert!(
            !classify_playing(&PlayingInput {
                track_duration_ms: 0,
                ..armed
            })
            .past_end_track_ended
        );
        // Not enough wall time elapsed (< duration + margin) → no advance.
        assert!(
            !classify_playing(&PlayingInput {
                wall_elapsed_secs: 120,
                ..armed
            })
            .past_end_track_ended
        );
        // Not a DLNA renderer → fallback disabled entirely.
        assert!(
            !classify_playing(&PlayingInput {
                is_dlna: false,
                ..armed
            })
            .past_end_track_ended
        );
        // Renderer reports its own duration → uses the accurate path, the
        // wall-clock fallback is disabled (no regression for good renderers).
        assert!(
            !classify_playing(&PlayingInput {
                reported_duration_ms: 300_000,
                ..armed
            })
            .past_end_track_ended
        );
    }
}
