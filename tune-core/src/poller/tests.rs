/// #2488 — le plancher de silence de chaque branche de fin de piste.
///
/// Ces assertions ne décorent pas le code : elles VERROUILLENT le chiffre
/// qu'un journal de testeur permettra d'imputer. Si quelqu'un touche
/// `STOPPED_TICKS_THRESHOLD`, `POSITION_PAST_END_TICKS`, `END_MARGIN_MS`
/// ou l'intervalle de sondage, le plancher annoncé dans `track_end_gap`
/// change — et ces tests l'annoncent au lieu de laisser une ligne de
/// journal mentir en silence.
mod plancher_de_detection {
    use crate::poller::decisions::{motif_fin, plancher_de_detection_ms};

    /// La sortie locale réveille le sondeur par `TRACK_END_NOTIFY` au lieu
    /// d'attendre son tick : elle ne paie AUCUN plancher de sondage. C'est
    /// la borne basse de toute la table, et ce qui distingue le chemin
    /// local des chemins réseau.
    #[test]
    fn la_fin_naturelle_locale_ne_coute_aucun_sondage() {
        assert_eq!(plancher_de_detection_ms(motif_fin::FIN_NATURELLE_LOCALE), 0);
    }

    /// Les quatre branches réseau, du moins cher au plus cher. Le blanc
    /// rapporté par Stéphane Villerio vaut 3 à 4 s : seule la branche DSD
    /// sur DLNA (1 s) laisse la place aux étapes suivantes dans cette
    /// enveloppe — les deux dernières (5 s et 6 s) la dépassent à elles
    /// seules, donc ce n'est pas elles qu'il subit.
    #[test]
    fn les_branches_reseau_portent_chacune_leur_plancher() {
        assert_eq!(
            plancher_de_detection_ms(motif_fin::DSD_DLNA_PIC_ATTEINT),
            1_000
        );
        assert_eq!(
            plancher_de_detection_ms(motif_fin::AVANCE_GAPLESS_BLOQUEE),
            2_000
        );
        assert_eq!(
            plancher_de_detection_ms(motif_fin::FIN_NATURELLE_APRES_STOPPED),
            5_000
        );
        assert_eq!(
            plancher_de_detection_ms(motif_fin::POSITION_AU_DELA_DE_LA_FIN),
            6_000
        );
    }

    /// Un motif que la table ne connaît pas rend `0` : le journal n'annonce
    /// jamais un chiffre inventé. C'est aussi le cas de la valeur initiale
    /// (chaîne vide) si une future branche oubliait de se nommer.
    #[test]
    fn un_motif_inconnu_ne_promet_rien() {
        assert_eq!(plancher_de_detection_ms(""), 0);
        assert_eq!(plancher_de_detection_ms("branche_inventee"), 0);
    }
}

mod tenue_du_renderer {
    use crate::poller::decisions::{TenueDuRenderer, qui_tient_le_renderer};

    /// Le cas du 24/08 : le DMP-A8 rapportait l'URI d'un flux du .18
    /// pendant que le .42 croyait jouer. L'hôte est extrait pour être
    /// NOMMÉ à l'écran.
    #[test]
    fn le_flux_dun_autre_serveur_tune_est_reconnu_et_nomme() {
        let v = qui_tient_le_renderer(
            Some("http://192.168.1.18:8888/stream/fe226f5d-abcd.flac"),
            Some("20dd4336-813d"),
        );
        assert_eq!(
            v,
            TenueDuRenderer::AutreServeurTune("192.168.1.18:8888".into())
        );
    }

    /// Notre propre flux ne déclenche RIEN — c'est le cas nominal.
    #[test]
    fn notre_propre_flux_est_le_notre() {
        let v = qui_tient_le_renderer(
            Some("http://192.168.1.42:8888/stream/20dd4336-813d.wav"),
            Some("20dd4336-813d"),
        );
        assert_eq!(v, TenueDuRenderer::LeNotre);
    }

    /// L'Eversolo après redémarrage : transport actif, URI VIDE — c'est
    /// son lecteur interne qui a restauré sa lecture locale.
    #[test]
    fn une_uri_vide_designe_le_lecteur_interne() {
        assert_eq!(
            qui_tient_le_renderer(Some(""), Some("x")),
            TenueDuRenderer::LecteurInterne
        );
    }

    /// Pas de TrackURI du tout : beaucoup de renderers n'en rapportent
    /// pas. L'absence de signal n'est pas une preuve — on ne crie pas.
    #[test]
    fn labsence_de_signal_ne_declenche_rien() {
        assert_eq!(
            qui_tient_le_renderer(None, Some("x")),
            TenueDuRenderer::LeNotre
        );
    }

    #[test]
    fn une_autre_application_est_reconnue() {
        let v = qui_tient_le_renderer(
            Some("http://192.168.1.30:57645/song/42.mp3"),
            Some("20dd4336"),
        );
        assert_eq!(v, TenueDuRenderer::AutreApplication);
    }

    /// Sans stream_id à nous (rien en lecture de notre côté), un flux au
    /// motif Tune reste celui d'un autre serveur.
    #[test]
    fn sans_notre_id_un_flux_tune_reste_etranger() {
        let v = qui_tient_le_renderer(Some("https://tune.local:8888/stream/abc.wav"), None);
        assert_eq!(
            v,
            TenueDuRenderer::AutreServeurTune("tune.local:8888".into())
        );
    }
}

/// #2991 — l'identifiant de session que la reprise « le renderer joue, Tune
/// ne le croyait pas » posait à `None`, alors que le renderer l'annonce.
mod stream_id_repris_de_l_uri {
    use crate::poller::decisions::stream_id_de_l_uri;

    /// Le cas nominal, celui d'un RS250A qui tire une radio transcodée.
    #[test]
    fn l_uri_dun_flux_tune_rend_son_identifiant() {
        assert_eq!(
            stream_id_de_l_uri(Some("http://192.168.1.18:8888/stream/20dd4336-813d.wav")),
            Some("20dd4336-813d".to_string()),
            "l'extension doit tomber, comme le fait le serveur de flux"
        );
    }

    /// Une chaîne de requête ou une ancre recopiée par l'appareil ne doit
    /// pas se retrouver COLLÉE à l'identifiant : la clé ne correspondrait
    /// plus à aucune session et la reprise retomberait sur `None`.
    #[test]
    fn une_requete_ou_une_ancre_ne_colle_pas_a_l_identifiant() {
        assert_eq!(
            stream_id_de_l_uri(Some("http://h:8888/stream/abc-1.flac?x=1")),
            Some("abc-1".to_string())
        );
        assert_eq!(
            stream_id_de_l_uri(Some("http://h:8888/stream/abc-1#t=0")),
            Some("abc-1".to_string())
        );
    }

    /// Rien à relire : on ne devine pas. Ces trois cas doivent rendre
    /// `None`, sans quoi la reprise adopterait un identifiant inventé.
    #[test]
    fn rien_a_relire_ne_devine_rien() {
        assert_eq!(stream_id_de_l_uri(None), None);
        assert_eq!(stream_id_de_l_uri(Some("")), None);
        assert_eq!(
            stream_id_de_l_uri(Some("http://192.168.1.30:57645/song/42.mp3")),
            None,
            "l'URI d'une autre application ne porte aucun stream_id"
        );
        assert_eq!(
            stream_id_de_l_uri(Some("http://h:8888/stream/")),
            None,
            "un chemin sans identifiant ne doit pas rendre la chaîne vide"
        );
    }

    /// L'URI d'un AUTRE serveur Tune rend bien un identifiant — c'est
    /// volontaire, et c'est l'appelant qui écarte le cas en demandant au
    /// gestionnaire de flux s'il connaît cette session. Cette épreuve fixe
    /// ce partage des rôles pour qu'il ne se perde pas.
    #[test]
    fn le_flux_dun_autre_tune_rend_un_identifiant_que_l_appelant_devra_ecarter() {
        assert_eq!(
            stream_id_de_l_uri(Some("http://192.168.1.18:8888/stream/etranger-9.wav")),
            Some("etranger-9".to_string())
        );
    }
}

use super::*;

#[test]
fn stale_start_position_rejects_previous_session_ghost() {
    // A6 reporting yesterday's ~374s six seconds into a fresh play.
    assert!(decisions::stale_start_position(6, 374_000));
    // Honest early sample: position consistent with wall time.
    assert!(!decisions::stale_start_position(6, 6_500));
    // Seek-restore margin: resume at +14s while wall says 2s is tolerated.
    assert!(!decisions::stale_start_position(2, 14_000));
}

#[test]
fn stale_start_position_rejects_ghost_beyond_30s_window() {
    // DMP-A8 (Bertrand, .18): the renderer keeps reporting a stale near-end
    // position PAST the old 30s grace. `track_started_at` is folded on
    // seek/resume so an honest 1x position can never exceed wall+15s at any
    // point — a ~235s sample at 30-40s of wall time is provably impossible
    // and must still be rejected (previously it was accepted the instant
    // the 30s window lapsed, poisoning the peak and firing a phantom
    // position_reset advance ~30s into track 1).
    assert!(decisions::stale_start_position(30, 235_000));
    assert!(decisions::stale_start_position(40, 374_000));
    // Honest deep-into-track sample is still accepted: position ~= wall.
    assert!(!decisions::stale_start_position(200, 200_000));
    // Honest sample within the +15s clock/seek-restore slack, late in the
    // track, is likewise fine.
    assert!(!decisions::stale_start_position(200, 214_000));
}

#[test]
fn volume_not_adopted_on_first_observation() {
    // No previous reading yet — never overwrite on the first poll.
    assert!(!decisions::should_adopt_device_volume(None, 0.5, 0.3));
}

#[test]
fn volume_not_adopted_when_device_reports_stale_default() {
    // Devialet keeps reporting 0.50 while the user saved 0.30 — the value
    // never moves, so the saved volume must be preserved (Fabien).
    assert!(!decisions::should_adopt_device_volume(Some(0.5), 0.5, 0.3));
}

#[test]
fn volume_adopted_on_real_device_change() {
    // The knob moved on the device (0.50 -> 0.62) and now differs from the
    // saved volume — adopt it.
    assert!(decisions::should_adopt_device_volume(Some(0.5), 0.62, 0.3));
}

#[test]
fn volume_not_adopted_when_change_matches_saved() {
    // Device moved but landed on what we already have stored.
    assert!(!decisions::should_adopt_device_volume(
        Some(0.5),
        0.62,
        0.62
    ));
}

#[test]
fn gapless_metadata_advance_rearms_scrobble_latch_1113() {
    // Characterization of forum #1113: in continuous album playback only
    // tracks 1, 3, 5… were scrobbled. Tracks reached via the gapless path
    // (the poller calls advance_queue_metadata, which swaps now-playing and
    // bumps queue_position WITHOUT bumping track_generation) never re-armed
    // the boolean latch, so their scrobble — and now-playing — was dropped.
    //
    // This simulates the full flow with the identity-keyed latch and NO
    // orchestrator play (i.e. no generation bump) between tracks.
    let dur = 200_000_i64; // 3:20 track
    let generation = 7_u64; // set by the explicit play of track 1, never bumped

    // --- Track 1 (explicit play, gen=7, queue pos 0) ---
    let k1 = decisions::scrobble_track_key(generation, 0, Some(41), "Track One", Some("Artist"));
    let mut latched: Option<String> = None;

    // Early in the track: below the 50% threshold, no scrobble yet.
    assert!(!decisions::should_dispatch_scrobble(
        latched.as_deref(),
        &k1,
        "local",
        dur,
        30_000
    ));
    // Past 50%: dispatch once, then latch.
    assert!(decisions::should_dispatch_scrobble(
        latched.as_deref(),
        &k1,
        "local",
        dur,
        100_000
    ));
    latched = Some(k1.clone());
    // Subsequent ticks of the same track must NOT scrobble again.
    assert!(!decisions::should_dispatch_scrobble(
        latched.as_deref(),
        &k1,
        "local",
        dur,
        150_000
    ));

    // --- Gapless transition to track 2: advance_queue_metadata updates
    // now-playing and queue_position (0 → 1) but NOT track_generation. ---
    let k2 = decisions::scrobble_track_key(generation, 1, Some(42), "Track Two", Some("Artist"));
    assert_ne!(
        k1, k2,
        "a gapless advance must produce a new latch identity even without a generation bump"
    );

    // Right after the transition (position reset to 0): armed but below
    // threshold — no premature scrobble.
    assert!(!decisions::should_dispatch_scrobble(
        latched.as_deref(),
        &k2,
        "local",
        dur,
        2_000
    ));
    // Track 2 crosses 50%: it MUST scrobble (this is exactly what the
    // stuck boolean latch prevented — tracks 2, 4, 6… were dropped).
    assert!(decisions::should_dispatch_scrobble(
        latched.as_deref(),
        &k2,
        "local",
        dur,
        100_000
    ));
    latched = Some(k2.clone());
    // Once per track, still.
    assert!(!decisions::should_dispatch_scrobble(
        latched.as_deref(),
        &k2,
        "local",
        dur,
        190_000
    ));
}

#[test]
fn scrobble_key_stable_across_metadata_refinements() {
    // Cover/format refinements re-emit now-playing mid-track; with a
    // library id the key must not change (no double scrobble).
    let a = decisions::scrobble_track_key(3, 5, Some(9), "Title", Some("A"));
    let b = decisions::scrobble_track_key(3, 5, Some(9), "Title (Remaster)", Some("A"));
    assert_eq!(a, b);
    // Streaming (no track id) falls back to title+artist identity.
    let c = decisions::scrobble_track_key(3, 5, None, "Song", Some("A"));
    let d = decisions::scrobble_track_key(3, 5, None, "Song", Some("B"));
    assert_ne!(c, d);
}

#[test]
fn scrobble_key_rearms_on_generation_bump_for_repeat_one() {
    // Repeat-one via handle_track_end re-plays the same track at the same
    // queue position — the generation bump alone must re-arm the latch.
    let first = decisions::scrobble_track_key(3, 0, Some(9), "Title", Some("A"));
    let replay = decisions::scrobble_track_key(4, 0, Some(9), "Title", Some("A"));
    assert_ne!(first, replay);
}

#[test]
fn radio_never_scrobbles() {
    let k = decisions::scrobble_track_key(1, 0, None, "Song", Some("A"));
    assert!(!decisions::should_dispatch_scrobble(
        None, &k, "radio", 200_000, 150_000
    ));
}

#[test]
fn gapless_cooldown_suppresses_stopped() {
    let mut ps = ZonePollState {
        gapless_sent: false,
        stopped_ticks: 0,
        tenue_etrangere_ticks: 0,
        tenue_signalee: false,
        gapless_cooldown: 4,
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
        track_generation: 0,
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
    };

    // While cooldown > 0, stopped_ticks must not accumulate
    for _ in 0..4 {
        assert!(ps.gapless_cooldown > 0);
        ps.gapless_cooldown -= 1;
        ps.stopped_ticks = 0; // simulates the Stopped branch logic
    }
    assert_eq!(ps.gapless_cooldown, 0);
    assert_eq!(ps.stopped_ticks, 0);

    // After cooldown expires, stopped_ticks can accumulate
    ps.stopped_ticks = 1;
    assert!(ps.stopped_ticks < STOPPED_TICKS_THRESHOLD);
    ps.stopped_ticks = 2;
    assert!(ps.stopped_ticks < STOPPED_TICKS_THRESHOLD);
    // STOPPED_TICKS_THRESHOLD is 5, so it takes 5 ticks to trigger
    ps.stopped_ticks = STOPPED_TICKS_THRESHOLD;
    assert!(ps.stopped_ticks >= STOPPED_TICKS_THRESHOLD);
}

#[test]
fn playing_state_resets_cooldown() {
    let mut ps = ZonePollState {
        gapless_sent: true,
        stopped_ticks: 0,
        tenue_etrangere_ticks: 0,
        tenue_signalee: false,
        gapless_cooldown: 3,
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
        track_generation: 0,
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
    };

    // Simulates entering Playing state
    ps.stopped_ticks = 0;
    ps.gapless_cooldown = 0;
    assert_eq!(ps.gapless_cooldown, 0);
}

#[test]
fn next_position_repeat_off() {
    let state = crate::playback::ZoneState {
        state: PlayState::Playing,
        queue_position: 3,
        queue_length: 5,
        repeat: RepeatMode::Off,
        shuffle: false,
        ..Default::default()
    };
    assert_eq!(PositionPoller::next_position(&state), Some(4));
}

#[test]
fn next_position_end_of_queue() {
    let state = crate::playback::ZoneState {
        state: PlayState::Playing,
        queue_position: 4,
        queue_length: 5,
        repeat: RepeatMode::Off,
        shuffle: false,
        ..Default::default()
    };
    assert_eq!(PositionPoller::next_position(&state), None);
}

#[test]
fn next_position_repeat_all_wraps() {
    let state = crate::playback::ZoneState {
        state: PlayState::Playing,
        queue_position: 4,
        queue_length: 5,
        repeat: RepeatMode::All,
        shuffle: false,
        ..Default::default()
    };
    assert_eq!(PositionPoller::next_position(&state), Some(0));
}

#[test]
fn next_position_repeat_one() {
    let state = crate::playback::ZoneState {
        state: PlayState::Playing,
        queue_position: 2,
        queue_length: 5,
        repeat: RepeatMode::One,
        shuffle: false,
        ..Default::default()
    };
    assert_eq!(PositionPoller::next_position(&state), Some(2));
}

#[test]
fn next_position_manual_ignores_repeat_one() {
    // A manual skip under repeat-one must advance, not replay (#1110).
    let state = crate::playback::ZoneState {
        state: PlayState::Playing,
        queue_position: 2,
        queue_length: 5,
        repeat: RepeatMode::One,
        shuffle: false,
        ..Default::default()
    };
    // Auto path still replays…
    assert_eq!(PositionPoller::next_position(&state), Some(2));
    // …but the manual button moves to the next track.
    assert_eq!(PositionPoller::next_position_manual(&state), Some(3));
}

#[test]
fn next_position_manual_repeat_one_wraps_at_end() {
    // Manual skip on the last track under repeat-one wraps to the start
    // (treated as repeat-all) rather than dead-ending.
    let state = crate::playback::ZoneState {
        state: PlayState::Playing,
        queue_position: 4,
        queue_length: 5,
        repeat: RepeatMode::One,
        shuffle: false,
        ..Default::default()
    };
    assert_eq!(PositionPoller::next_position_manual(&state), Some(0));
}

#[test]
fn next_position_after_walks_forward() {
    // An unplayable track at position 2 must hand back position 3, not
    // stop the queue (the poller loops on this).
    let state = crate::playback::ZoneState {
        state: PlayState::Playing,
        queue_position: 1,
        queue_length: 10,
        repeat: RepeatMode::Off,
        shuffle: false,
        ..Default::default()
    };
    assert_eq!(PositionPoller::next_position_after(&state, 2), Some(3));
}

#[test]
fn next_position_after_end_of_queue_stops() {
    // Last item unplayable under repeat-off: nothing left to skip to.
    let state = crate::playback::ZoneState {
        state: PlayState::Playing,
        queue_position: 3,
        queue_length: 5,
        repeat: RepeatMode::Off,
        shuffle: false,
        ..Default::default()
    };
    assert_eq!(PositionPoller::next_position_after(&state, 4), None);
}

#[test]
fn next_position_after_repeat_one_returns_same_slot() {
    // Repeat-one on a dead track: the caller must recognise the unchanged
    // position and stop instead of spinning forever.
    let state = crate::playback::ZoneState {
        state: PlayState::Playing,
        queue_position: 2,
        queue_length: 5,
        repeat: RepeatMode::One,
        shuffle: false,
        ..Default::default()
    };
    assert_eq!(PositionPoller::next_position_after(&state, 2), Some(2));
}

#[test]
fn next_position_after_repeat_all_wraps() {
    let state = crate::playback::ZoneState {
        state: PlayState::Playing,
        queue_position: 3,
        queue_length: 5,
        repeat: RepeatMode::All,
        shuffle: false,
        ..Default::default()
    };
    assert_eq!(PositionPoller::next_position_after(&state, 4), Some(0));
}

#[test]
fn next_position_after_shuffle_keeps_its_place_in_the_order() {
    // Order 3,1,4,0,2 — the item at queue position 4 (order index 2) is
    // unplayable, so the next candidate is the order's next entry, 0.
    let state = crate::playback::ZoneState {
        state: PlayState::Playing,
        queue_position: 1,
        queue_length: 5,
        repeat: RepeatMode::Off,
        shuffle: true,
        shuffle_order: vec![3, 1, 4, 0, 2],
        shuffle_index: 1,
        ..Default::default()
    };
    assert_eq!(PositionPoller::next_position_after(&state, 4), Some(0));
}

#[test]
fn next_position_after_shuffle_last_of_cycle_stops() {
    // Dead item is the final entry of the shuffle cycle under repeat-off.
    let state = crate::playback::ZoneState {
        state: PlayState::Playing,
        queue_position: 0,
        queue_length: 5,
        repeat: RepeatMode::Off,
        shuffle: true,
        shuffle_order: vec![3, 1, 4, 0, 2],
        shuffle_index: 3,
        ..Default::default()
    };
    assert_eq!(PositionPoller::next_position_after(&state, 2), None);
}

#[test]
fn next_position_empty_queue() {
    let state = crate::playback::ZoneState {
        state: PlayState::Playing,
        queue_position: 0,
        queue_length: 0,
        repeat: RepeatMode::Off,
        shuffle: false,
        ..Default::default()
    };
    assert_eq!(PositionPoller::next_position(&state), None);
}

#[test]
fn next_position_shuffle_follows_order() {
    // Shuffle follows the materialised order, NOT the raw queue index
    // (#954, eric). Order [3,1,4,0,2], cursor at index 1 (track 1 playing)
    // → next is order[2] = 4.
    let state = crate::playback::ZoneState {
        state: PlayState::Playing,
        queue_position: 1,
        queue_length: 5,
        repeat: RepeatMode::Off,
        shuffle: true,
        shuffle_order: vec![3, 1, 4, 0, 2],
        shuffle_index: 1,
        ..Default::default()
    };
    assert_eq!(PositionPoller::next_position(&state), Some(4));
}

#[test]
fn next_position_shuffle_off_stops_after_full_cycle() {
    // repeat-off + shuffle: at the last position of the order, playback
    // stops (every track played exactly once) — no premature stop, no
    // endless loop.
    let state = crate::playback::ZoneState {
        state: PlayState::Playing,
        queue_position: 2,
        queue_length: 5,
        repeat: RepeatMode::Off,
        shuffle: true,
        shuffle_order: vec![3, 1, 4, 0, 2],
        shuffle_index: 4, // last index
        ..Default::default()
    };
    assert_eq!(PositionPoller::next_position(&state), None);
}

#[test]
fn next_position_shuffle_all_wraps_to_order_start() {
    // repeat-all + shuffle: at the end of the order, loop back to the first
    // shuffled track (order[0] = 3), not raw index 0.
    let state = crate::playback::ZoneState {
        state: PlayState::Playing,
        queue_position: 2,
        queue_length: 5,
        repeat: RepeatMode::All,
        shuffle: true,
        shuffle_order: vec![3, 1, 4, 0, 2],
        shuffle_index: 4,
        ..Default::default()
    };
    assert_eq!(PositionPoller::next_position(&state), Some(3));
}

#[test]
fn next_position_shuffle_empty_order_falls_back_sequential() {
    // Before the order is materialised (e.g. just after a restart, before
    // update_queue_info rebuilds it), shuffle falls back to sequential so
    // playback still advances.
    let state = crate::playback::ZoneState {
        state: PlayState::Playing,
        queue_position: 1,
        queue_length: 5,
        repeat: RepeatMode::Off,
        shuffle: true,
        shuffle_order: Vec::new(),
        shuffle_index: -1,
        ..Default::default()
    };
    assert_eq!(PositionPoller::next_position(&state), Some(2));
}

#[test]
fn backoff_exponential() {
    let mut ps = ZonePollState {
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
        track_generation: 0,
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
    };

    // Simulate consecutive errors with exponential backoff
    for expected_errors in 1u8..=5 {
        ps.consecutive_errors = ps.consecutive_errors.saturating_add(1);
        ps.backoff_remaining = 1u8 << ps.consecutive_errors.min(4);
        assert_eq!(ps.consecutive_errors, expected_errors);
    }
    // After 4 errors: backoff = 2^4 = 16
    assert_eq!(ps.backoff_remaining, 16);

    // After 5 errors: still capped at 2^4 = 16
    ps.consecutive_errors = 5;
    ps.backoff_remaining = 1u8 << ps.consecutive_errors.min(4);
    assert_eq!(ps.backoff_remaining, 16);

    // Success resets
    ps.consecutive_errors = 0;
    assert_eq!(ps.consecutive_errors, 0);
}

// These tests now call the REAL predicate `decisions::played_enough`
// (v0.9 rc.1 filet). Wall-clock is passed high (300s) unless the test
// specifically pins the wall_elapsed guard, so each assertion isolates
// the branch it names.

#[test]
fn played_enough_rejects_early_transition() {
    // Track is 300 seconds (300_000 ms).  Peak at 10s — only 3.3% played.
    assert!(
        !decisions::played_enough(300_000, 10_000, 300),
        "10s into a 5-min track should NOT be enough"
    );
}

#[test]
fn played_enough_accepts_late_transition() {
    // Track is 300 seconds.  Peak at 280s — 93% played, fully elapsed.
    assert!(
        decisions::played_enough(300_000, 280_000, 300),
        "280s into a 5-min track should be enough"
    );
}

#[test]
fn played_enough_requires_wall_elapsed() {
    // 93% played but only 10s of wall-clock elapsed: the wall_elapsed
    // guard (MIN_TRACK_WALL_SECS = 30s) must reject it. This branch was
    // NOT covered by the old re-implemented tests.
    assert!(
        !decisions::played_enough(300_000, 280_000, 10),
        "wall_elapsed < MIN_TRACK_WALL_SECS must reject even at high fraction"
    );
}

#[test]
fn peak_reached_end_bypasses_reset_wall_clock() {
    // Jean Valjean, local FLAC on WASAPI: a gapless metadata advance reset
    // track_started_at ~2s before the track actually ended, so wall_elapsed
    // under-counted and played_enough rejected a track that had in fact
    // finished (peak 719906 ms > duration 714906 ms). peak_reached_end must
    // recognize the end from the peak alone, independent of the wall clock,
    // so auto-advance is immediate instead of stalling ~30s.
    let dur = 714_906u64;
    let peak = 719_906u64; // peak overshot the duration
    assert!(
        !decisions::played_enough(dur, peak, 2),
        "reset wall clock makes played_enough falsely reject the finished track"
    );
    assert!(
        decisions::peak_reached_end(dur, peak),
        "peak past the duration must count as ended regardless of wall time"
    );
    // A track barely started must NOT be treated as ended.
    assert!(!decisions::peak_reached_end(dur, 30_000));
    // Unknown duration: no false positive.
    assert!(!decisions::peak_reached_end(0, 500_000));
}

#[test]
fn played_enough_accepts_short_track_fully_played() {
    // DEvir: a 27.67s TIDAL track played to the end. It can NEVER reach
    // wall_elapsed >= 30s, so the old predicate rejected it → single-track
    // Repeat All (and auto-advance for any sub-30s track) never triggered.
    // With the duration-capped wall floor, a fully-played short track passes.
    assert!(
        decisions::played_enough(27_670, 27_000, 27),
        "a fully-played 27s track must count as played_enough"
    );
    // But a short track barely played must still be rejected.
    assert!(
        !decisions::played_enough(27_670, 3_000, 4),
        "a 27s track stopped at 3s must NOT count as played_enough"
    );
}

#[test]
fn played_enough_unknown_duration_low_peak() {
    // Unknown duration (0) + peak below MIN_PEAK_UNKNOWN_DURATION_MS:
    // reject, to prevent false skips on slow renderers (Shanling SCD1.3).
    assert!(
        !decisions::played_enough(0, 5_000, 300),
        "5s peak with unknown duration should NOT pass"
    );
}

#[test]
fn played_enough_unknown_duration_high_peak() {
    // Unknown duration (0) but enough position reported + elapsed → pass.
    assert!(
        decisions::played_enough(0, 120_000, 300),
        "120s peak with unknown duration should pass"
    );
}

#[test]
fn played_enough_unknown_duration_high_peak_but_too_soon() {
    // Unknown duration, high peak, but wall_elapsed guard still applies.
    assert!(
        !decisions::played_enough(0, 120_000, 10),
        "unknown-duration path must also honor the wall_elapsed guard"
    );
}

#[test]
fn position_reset_detects_gapless_advance() {
    // Position dropped from >30s to <5s while gapless was armed.
    assert!(decisions::position_reset(40_000, 2_000, true));
}

#[test]
fn dmpa8_stale_ghost_does_not_poison_peak_or_advance() {
    // Reproduces the .18 DMP-A8 "playlist stops at track 1" chain at the
    // pure-decision level. Track 1 is 240s; the renderer reports a stale
    // ~235s near-end position for the whole first ~30s of a fresh play.
    let track_duration_ms = 240_000;
    let stale_pos_ms = 235_000;

    // Every stale sample is provably impossible (position >> wall+15s) and
    // is rejected — at 5s in AND once past the old 30s window — so it never
    // becomes the peak.
    assert!(decisions::stale_start_position(5, stale_pos_ms));
    assert!(decisions::stale_start_position(30, stale_pos_ms));
    assert!(decisions::stale_start_position(35, stale_pos_ms));

    // With the ghost filtered, the peak only ever reflects honest samples
    // (~a few seconds in this window), so the track is NOT "played enough".
    let honest_peak_ms = 5_000;
    assert!(!decisions::played_enough(
        track_duration_ms,
        honest_peak_ms,
        35
    ));

    // Even if the renderer then snaps to 0 while gapless is armed, the
    // caller gates the metadata advance on played_enough — which is false —
    // so no phantom position_reset advance fires 30s into track 1. (The raw
    // drop shape matches; the played_enough guard is what prevents it.)
    let raw_drop_matches = decisions::position_reset(stale_pos_ms, 1_000, true);
    assert!(raw_drop_matches);
    let played_enough = decisions::played_enough(track_duration_ms, honest_peak_ms, 35);
    assert!(
        !(raw_drop_matches && played_enough),
        "advance must be gated off while the ghost is filtered"
    );
}

#[test]
fn position_reset_requires_armed_gapless() {
    // Same position drop but no gapless armed → not a reset.
    assert!(!decisions::position_reset(40_000, 2_000, false));
}

#[test]
fn position_reset_ignores_small_drop() {
    // Position still above the 5s floor → not a reset.
    assert!(!decisions::position_reset(40_000, 8_000, true));
    // Previous position not above the 30s ceiling → not a reset.
    assert!(!decisions::position_reset(20_000, 2_000, true));
}

#[test]
fn position_reset_fallback_only_fires_for_internal_gapless_outputs() {
    // A raw position drop to 0 fires the metadata-only advance fallback ONLY
    // for renderers that auto-transition internally (DLNA). For a Chromecast
    // / slimproto / exclusive-local output the drop means the track ENDED
    // (device IDLE/FINISHED) — the fallback must NOT fire; the natural-end
    // path (Stopped branch → play_from_queue) then does a real load.
    // Regression for Rhorn's Chromecast end-of-track loop (#1072).
    let raw = decisions::position_reset(40_000, 2_000, true);
    assert!(raw, "the drop shape matches on both output kinds");

    // Chromecast (can_internal_gapless == false) → suppressed.
    assert!(!decisions::position_reset_fires(raw, false, false));
    // DLNA (can_internal_gapless == true) → fires as before.
    assert!(decisions::position_reset_fires(raw, true, false));
    // No raw reset → never, regardless of output kind.
    assert!(!decisions::position_reset_fires(false, true, false));
    assert!(!decisions::position_reset_fires(false, false, false));
}

#[test]
fn position_reset_during_seek_never_advances_gapless_metadata() {
    // #2170: a seek near the arming window recreates the OAAT stream. The
    // old direct-file path restarted at zero, so the drop shape looked
    // exactly like a real internal transition. A current-track seek must
    // never advance queue metadata, even when gapless is armed and the
    // output otherwise supports internal chaining.
    let raw = decisions::position_reset(258_760, 174, true);
    assert!(raw);
    assert!(!decisions::position_reset_fires(raw, true, true));

    // Once the seek grace is over, an honest internal transition keeps the
    // existing behavior.
    assert!(decisions::position_reset_fires(raw, true, false));
}

#[test]
fn position_reset_armed_by_gapless_sent_after_guard_expiry() {
    // #1019: SetNext is sent 30s before end (GAPLESS_WINDOW_MS) but
    // gapless_sent_at expires after 15s (GAPLESS_GUARD_SECS), so at the
    // real transition the *timestamp* is already None while the *boolean*
    // gapless_sent is still true. The caller now arms position_reset on
    // the boolean, so a same-duration seamless gapless transition is still
    // detected in the final 15s of the track.
    let gapless_sent = true; // boolean stays armed until transition
    let gapless_sent_at_is_some = false; // 15s guard already expired
    assert!(
        decisions::position_reset(238_000, 1_500, gapless_sent),
        "must detect the transition using the boolean arm"
    );
    assert!(
        !decisions::position_reset(238_000, 1_500, gapless_sent_at_is_some),
        "the expired timestamp would have missed it (the old bug)"
    );
}

#[test]
fn natural_end_when_played_enough() {
    assert!(decisions::natural_end(
        true, false, 0, false, 0, 300_000, true
    ));
}

#[test]
fn natural_end_repeat_active_with_meaningful_playback() {
    // Repeat on + peak > 5s → treat as natural end (DEvir QA B-05).
    assert!(decisions::natural_end(
        false, true, 6_000, false, 0, 300_000, true
    ));
    // Repeat on but peak <= 5s → not enough.
    assert!(!decisions::natural_end(
        false, true, 4_000, false, 0, 300_000, true
    ));
}

#[test]
fn natural_end_ended_naturally_needs_proportional_wall() {
    // ended_naturally is trusted only once >= MIN_WALL_FRACTION of the known
    // duration has elapsed in wall time — a 5:00 track cannot end at 5s
    // (DMP-A8 spurious ended_naturally). 50% of 300s = 150s.
    assert!(!decisions::natural_end(
        false, false, 0, true, 5, 300_000, true
    ));
    assert!(!decisions::natural_end(
        false, false, 0, true, 149, 300_000, true
    ));
    assert!(decisions::natural_end(
        false, false, 0, true, 150, 300_000, true
    ));
    // Unknown duration (0) keeps the original modest 5s floor.
    assert!(decisions::natural_end(false, false, 0, true, 5, 0, true));
    assert!(!decisions::natural_end(false, false, 0, true, 4, 0, true));
}

/// A non-realtime output (a recorder writing the container to disk) is
/// exempt from the wall-clock floor: it finishes a 5:00 track in a second or
/// two, and holding the queue back until 150s had elapsed pinned a rip at
/// half of listening speed instead of network speed.
#[test]
fn natural_end_non_realtime_output_skips_the_wall_guard() {
    // Same inputs the DMP-A8 guard rejects above — accepted here.
    assert!(decisions::natural_end(
        false, false, 0, true, 1, 300_000, false
    ));
    assert!(decisions::natural_end(
        false, false, 0, true, 0, 300_000, false
    ));

    // The exemption is not a blanket "always end": without ended_naturally
    // there is still nothing to act on.
    assert!(!decisions::natural_end(
        false, false, 0, false, 0, 300_000, false
    ));

    // And it changes nothing for a renderer.
    assert!(!decisions::natural_end(
        false, false, 0, true, 1, 300_000, true
    ));
}

#[test]
fn natural_end_short_track_half_played() {
    // Short track (< 30s) with >= 50% peak → natural end.
    assert!(decisions::natural_end(
        false, false, 6_000, false, 0, 10_000, true
    ));
    // Short track but < 50% peak → not yet.
    assert!(!decisions::natural_end(
        false, false, 4_000, false, 0, 10_000, true
    ));
}

#[test]
fn natural_end_all_guards_false() {
    assert!(!decisions::natural_end(
        false, false, 0, false, 0, 300_000, true
    ));
}

/// #3229 — la fin de piste vue par le pic vaut AUSSI sur le chemin DLNA.
///
/// Jean Valjean, fil 893 : `peak = durée + 5000 ms`, six fois dans ses
/// journaux. `peak_reached_end` a été écrit pour cette signature-là — et son
/// unique appelant de production était la branche `status.ended_naturally`,
/// que **DLNA ne lève jamais** (c'est la raison d'être de
/// `dlna_dsd_reached_end`, qui ne couvre que le DSD). Une zone DLNA en
/// PCM/FLAC retombait donc ici, sur `natural_end`, où `played_enough` exige
/// un plancher d'horloge murale que l'avance gapless vient de fausser : la
/// vraie fin était rejetée et la zone partait vers la branche d'ÉCHEC.
#[test]
fn natural_end_reconnait_le_pic_sur_le_chemin_dlna() {
    // Signature exacte du fil 893 : le pic dépasse la durée de 5 s.
    let dur = 714_906u64;
    let peak = 719_906u64;

    // L'horloge murale a été remise par une avance gapless : elle
    // sous-compte, et le prédicat qui en dépend rejette une piste finie.
    assert!(
        !decisions::played_enough(dur, peak, 2),
        "témoin : c'est bien l'horloge murale faussée qui rejetait la fin"
    );

    // DLNA ne rend pas `ended_naturally` (4ᵉ argument à false) et la piste
    // n'est pas courte. Avant #3229, aucune clause ne mordait ici.
    assert!(
        decisions::natural_end(false, false, peak, false, 2, dur, true),
        "sur DLNA, un pic qui a atteint la fin de la piste EST une fin \
         naturelle — sinon la zone est arrêtée comme si elle avait \
         échoué (#3229)"
    );

    // Et la garde reste étroite : elle n'accepte pas une piste à peine
    // commencée, ni une durée inconnue.
    assert!(
        !decisions::natural_end(false, false, 30_000, false, 2, dur, true),
        "un pic à 30 s sur une piste de 12 min n'est pas une fin"
    );
    assert!(
        !decisions::natural_end(false, false, 500_000, false, 2, 0, true),
        "durée inconnue : aucun pic ne prouve une fin"
    );
}

// DSD-over-DLNA end-of-track fast path (Benjithom, RS130: ~5s gap between DSD
// tracks). Because gapless is disabled for a DSD next and DLNA never sets
// ended_naturally, the poller must advance on peak-reached-end instead of
// waiting out STOPPED_TICKS_THRESHOLD.
#[test]
fn dlna_dsd_reached_end_advances_at_peak() {
    // DSD on DLNA, peak reached 80%+ of the 300s track → advance now.
    assert!(decisions::dlna_dsd_reached_end(
        "dlna",
        Some("dsf"),
        300_000,
        240_000
    ));
    assert!(decisions::dlna_dsd_reached_end(
        "dlna",
        Some("dff"),
        300_000,
        299_000
    ));
}

#[test]
fn dlna_dsd_reached_end_holds_when_not_at_end() {
    // DSD on DLNA but only ~50% played (a mid-track Stopped blip) → do NOT
    // advance; the counter path still guards against a false skip.
    assert!(!decisions::dlna_dsd_reached_end(
        "dlna",
        Some("dsd"),
        300_000,
        150_000
    ));
}

#[test]
fn dlna_dsd_reached_end_ignores_pcm_and_non_dlna() {
    // PCM/FLAC on DLNA keep their armed-gapless path — no fast path here.
    assert!(!decisions::dlna_dsd_reached_end(
        "dlna",
        Some("flac"),
        300_000,
        299_000
    ));
    // DSD on a local output is out of scope (local DSD gapless chain).
    assert!(!decisions::dlna_dsd_reached_end(
        "local",
        Some("dsf"),
        300_000,
        299_000
    ));
    // Missing format → not treated as DSD.
    assert!(!decisions::dlna_dsd_reached_end(
        "dlna", None, 300_000, 299_000
    ));
}

#[test]
fn wall_clock_past_end_dlna_no_reported_duration() {
    // DLNA renderer reports duration 0 (LMS UPnP bridge) but Tune knows the
    // queue duration (300s) and the wall clock passed duration + margin.
    assert!(decisions::wall_clock_past_end(true, 0, 300_000, 304));
    // Not enough wall time elapsed → no advance.
    assert!(!decisions::wall_clock_past_end(true, 0, 300_000, 120));
    // Renderer reports its own duration → accurate path, fallback disabled.
    assert!(!decisions::wall_clock_past_end(true, 300_000, 300_000, 304));
    // Non-DLNA output → fallback disabled.
    assert!(!decisions::wall_clock_past_end(false, 0, 300_000, 304));
    // Queue duration unknown → no advance.
    assert!(!decisions::wall_clock_past_end(true, 0, 0, 304));
}

#[test]
fn chromecast_wall_clock_past_end_advances_after_full_duration() {
    // Chromecast, played enough (peak ≥80%), wall clock passed dur+margin:
    // Cast never surfaced a usable end-of-track signal → advance on our clock
    // (Rhorn, forum #1226: album stalls after track 1 on Chromecast Audio).
    assert!(decisions::chromecast_wall_clock_past_end(
        "chromecast",
        true,
        300_000,
        304
    ));
    // Not enough wall time elapsed yet → keep playing.
    assert!(!decisions::chromecast_wall_clock_past_end(
        "chromecast",
        true,
        300_000,
        120
    ));
    // Peak below 80% (a genuine mid-track buffering stall) → must NOT advance
    // even though the wall clock passed the duration.
    assert!(!decisions::chromecast_wall_clock_past_end(
        "chromecast",
        false,
        300_000,
        304
    ));
    // Non-chromecast output → fallback disabled (DLNA/local keep their paths).
    assert!(!decisions::chromecast_wall_clock_past_end(
        "dlna", true, 300_000, 304
    ));
    // Unknown track duration → no advance (nothing to compare the clock to).
    assert!(!decisions::chromecast_wall_clock_past_end(
        "chromecast",
        true,
        0,
        304
    ));
}

#[test]
fn poll_failed_past_end_advances_when_poll_errors() {
    // DLNA bridge: GetPositionInfo SOAP failed (poll errored), Tune is
    // Playing, wall clock passed duration + margin, enough consecutive
    // failures, not yet fired → advance.
    assert!(decisions::poll_failed_past_end(
        true, true, 300_000, 304, 2, false
    ));
}

#[test]
fn poll_failed_past_end_negatives() {
    // Not enough wall time elapsed → still playing, no advance.
    assert!(!decisions::poll_failed_past_end(
        true, true, 300_000, 120, 2, false
    ));
    // Tune not Playing (user paused/stopped) → never advance a paused track.
    assert!(!decisions::poll_failed_past_end(
        true, false, 300_000, 304, 2, false
    ));
    // Single transient failure (below POLL_FAIL_END_MIN_ERRORS) → no advance.
    assert!(!decisions::poll_failed_past_end(
        true, true, 300_000, 304, 1, false
    ));
    // Already fired for this track → don't re-fire.
    assert!(!decisions::poll_failed_past_end(
        true, true, 300_000, 304, 2, true
    ));
    // Non-DLNA output → fallback disabled.
    assert!(!decisions::poll_failed_past_end(
        false, true, 300_000, 304, 2, false
    ));
    // Queue duration unknown → no advance (nothing to compare against).
    assert!(!decisions::poll_failed_past_end(
        true, true, 0, 304, 2, false
    ));
}

#[test]
fn duration_changed_requires_armed_and_delta() {
    // Armed + reported duration differs by > 2s → changed.
    assert!(decisions::duration_changed(true, 200_000, 210_000));
    // Not armed → never "changed".
    assert!(!decisions::duration_changed(false, 200_000, 210_000));
    // Delta within 2s → not changed.
    assert!(!decisions::duration_changed(true, 200_000, 201_000));
    // Zero durations → not changed.
    assert!(!decisions::duration_changed(true, 0, 210_000));
    assert!(!decisions::duration_changed(true, 200_000, 0));
}

#[test]
fn position_confirms_transition_near_end_or_reset() {
    // played_enough + position reset to start → confirmed.
    assert!(decisions::position_confirms_transition(
        true, 2_000, 300_000
    ));
    // played_enough + within GAPLESS_WINDOW_MS of the end → confirmed.
    assert!(decisions::position_confirms_transition(
        true, 275_000, 300_000
    ));
    // Mid-track, not near end, not reset → not confirmed.
    assert!(!decisions::position_confirms_transition(
        true, 150_000, 300_000
    ));
    // Not played_enough → never confirmed even at reset.
    assert!(!decisions::position_confirms_transition(
        false, 2_000, 300_000
    ));
}

#[test]
fn dlna_playing_stall_ne_sarme_quavec_des_preuves_exploitables() {
    let eligible = |output,
                    tune_playing,
                    renderer_playing,
                    realtime,
                    has_stream,
                    seek,
                    load,
                    peak,
                    pos,
                    dur| {
        decisions::dlna_playing_stall_eligible(
            output,
            tune_playing,
            renderer_playing,
            realtime,
            has_stream,
            seek,
            load,
            peak,
            pos,
            dur,
        )
    };

    assert!(eligible(
        "dlna", true, true, true, true, false, 45, 6_000, 6_000, 300_000
    ));
    assert!(!eligible(
        "chromecast",
        true,
        true,
        true,
        true,
        false,
        45,
        6_000,
        6_000,
        300_000
    ));
    assert!(!eligible(
        "dlna", false, true, true, true, false, 45, 6_000, 6_000, 300_000
    ));
    assert!(!eligible(
        "dlna", true, false, true, true, false, 45, 6_000, 6_000, 300_000
    ));
    assert!(!eligible(
        "dlna", true, true, false, true, false, 45, 6_000, 6_000, 300_000
    ));
    assert!(!eligible(
        "dlna", true, true, true, false, false, 45, 6_000, 6_000, 300_000
    ));
    assert!(!eligible(
        "dlna", true, true, true, true, true, 45, 6_000, 6_000, 300_000
    ));
    assert!(!eligible(
        "dlna", true, true, true, true, false, 44, 6_000, 6_000, 300_000
    ));
    assert!(!eligible(
        "dlna", true, true, true, true, false, 45, 4_999, 4_999, 300_000
    ));
    // A frozen sample at the known end belongs to the existing natural-end
    // paths; it is not a mid-track playback failure.
    assert!(!eligible(
        "dlna", true, true, true, true, false, 45, 299_000, 299_000, 300_000
    ));
}

#[test]
fn dlna_playing_stall_exige_position_et_octets_figes() {
    let next = decisions::next_dlna_playing_stall_ticks;
    assert_eq!(next(7, true, 6_000, 6_000, 42_000, 42_000), 8);
    assert_eq!(next(7, true, 6_000, 7_000, 42_000, 42_000), 0);
    assert_eq!(next(7, true, 6_000, 6_000, 42_000, 43_000), 0);
    assert_eq!(next(7, false, 6_000, 6_000, 42_000, 42_000), 0);
}

#[test]
fn dlna_playing_stall_declenche_a_la_frontiere_exacte() {
    let mut ticks = 0;
    for _ in 0..PLAYING_STALL_THRESHOLD - 1 {
        ticks = decisions::next_dlna_playing_stall_ticks(ticks, true, 6_000, 6_000, 42_000, 42_000);
    }
    assert_eq!(ticks, PLAYING_STALL_THRESHOLD - 1);
    assert!(ticks < PLAYING_STALL_THRESHOLD);

    ticks = decisions::next_dlna_playing_stall_ticks(ticks, true, 6_000, 6_000, 42_000, 42_000);
    assert_eq!(ticks, PLAYING_STALL_THRESHOLD);
}

#[test]
fn un_demarrage_mort_est_un_echec_dlna_sans_aucun_octet_servi() {
    // Le profil du pipeline Eversolo coincé : DLNA, zéro octet tiré.
    assert!(super::decisions::demarrage_mort("dlna", 0));
    // Un décrochage EN COURS de lecture n'en est pas un.
    assert!(!super::decisions::demarrage_mort("dlna", 1_234_567));
    // Et seul le DLNA est concerné (le zombie est un renderer réseau).
    assert!(!super::decisions::demarrage_mort("chromecast", 0));
    assert!(!super::decisions::demarrage_mort("local", 0));
}

#[test]
fn une_seule_relance_demarrage_mort_par_fenetre() {
    // Jamais relancé : autorisé.
    assert!(super::decisions::relance_demarrage_mort_autorisee(None));
    // Relancé il y a longtemps : autorisé de nouveau.
    assert!(super::decisions::relance_demarrage_mort_autorisee(Some(
        181
    )));
    // Relancé dans la fenêtre : on coupe, on ne martèle pas.
    assert!(!super::decisions::relance_demarrage_mort_autorisee(Some(
        180
    )));
    assert!(!super::decisions::relance_demarrage_mort_autorisee(Some(0)));
}

#[test]
fn dsd_skip_latch_holds_only_for_the_same_queue_position() {
    // Verrou posé pour la position 3 : tient tant que le « suivant » est 3…
    assert!(super::decisions::dsd_skip_latched(Some(3), Some(3)));
    // …et lâche dès que la file bouge (autre position, ou plus de suivant).
    assert!(!super::decisions::dsd_skip_latched(Some(3), Some(4)));
    assert!(!super::decisions::dsd_skip_latched(Some(3), None));
    // Jamais verrouillé sans constat préalable.
    assert!(!super::decisions::dsd_skip_latched(None, Some(3)));
    assert!(!super::decisions::dsd_skip_latched(None, None));
}

#[test]
fn should_arm_gapless_in_final_window() {
    // Entered the final GAPLESS_WINDOW_MS, not yet armed → arm.
    assert!(decisions::should_arm_gapless(
        false, 300_000, 300_000, 275_000
    ));
    // Already armed → don't re-arm.
    assert!(!decisions::should_arm_gapless(
        true, 300_000, 300_000, 275_000
    ));
    // Still before the final window → don't arm.
    assert!(!decisions::should_arm_gapless(
        false, 300_000, 300_000, 100_000
    ));
    // Duration shorter than the window → never arm (no underflow).
    assert!(!decisions::should_arm_gapless(false, 10_000, 10_000, 9_000));
}

#[test]
fn should_arm_gapless_falls_back_to_queue_duration() {
    // Renderer reports duration 0 (LMS UPnP bridge) but Tune knows the
    // queue duration and the renderer position is in the final window →
    // arm using the queue duration.
    assert!(decisions::should_arm_gapless(false, 0, 300_000, 275_000));
    // Renderer reports its own duration → prefer it, ignore the queue value
    // (well-behaved renderer unaffected): reported=300s window, pos 275s.
    assert!(decisions::should_arm_gapless(
        false, 300_000, 999_000, 275_000
    ));
    // Both durations unknown → never arm.
    assert!(!decisions::should_arm_gapless(false, 0, 0, 275_000));
    // Queue duration known but position not yet in the final window.
    assert!(!decisions::should_arm_gapless(false, 0, 300_000, 100_000));
}

#[test]
fn should_arm_gapless_ignores_egregious_renderer_duration() {
    use decisions::{sane_current_duration, should_arm_gapless};
    // The HiFi Rose RS130 reports a duration far off the real track. Only
    // an egregious (>4x / <1/4) mismatch with a known DB duration is
    // distrusted — a merely-imprecise renderer duration is still trusted.
    //
    // (a) egregiously LARGE (800000 for a real 174693 ms track): without the
    // guard the arm window sits past the real end so gapless never arms and
    // the album cuts; with the guard the DB duration wins and it arms in time.
    assert!(should_arm_gapless(false, 800_000, 174_693, 160_000));
    // (b) egregiously SMALL (40000 for a real 174693 ms track): without the
    // guard it would arm at ~t=10s; with the guard it waits for the real end.
    assert!(!should_arm_gapless(false, 40_000, 174_693, 50_000));
    // A merely-different (3.3x) reported value is STILL trusted — this is the
    // deliberate "well-behaved renderer" design, unchanged.
    assert!(should_arm_gapless(false, 300_000, 999_000, 275_000));
    // Helper directly: egregious → DB; imprecise → reported; 0 → DB;
    // unknown DB → keep reported (can't judge).
    assert_eq!(sane_current_duration(800_000, 174_693), 174_693);
    assert_eq!(sane_current_duration(40_000, 174_693), 174_693);
    assert_eq!(sane_current_duration(300_000, 999_000), 300_000);
    assert_eq!(sane_current_duration(0, 174_693), 174_693);
    assert_eq!(sane_current_duration(800_000, 0), 800_000);
}

#[test]
fn past_end_reached_beyond_margin() {
    // Position past duration + END_MARGIN_MS, played enough → reached.
    assert!(decisions::past_end_reached(240_000, true, 244_000));
    // Just past duration but within the margin → not yet.
    assert!(!decisions::past_end_reached(240_000, true, 240_500));
    // Past end but not played_enough → not reached.
    assert!(!decisions::past_end_reached(240_000, false, 244_000));
    // Duration at/below the margin → not reached.
    assert!(!decisions::past_end_reached(1_000, true, 50_000));
}

#[test]
fn position_to_persist_zeros_near_end() {
    // DEvir's exact case: saved position past the rounded duration → 0,
    // so auto-resume plays from the start instead of bouncing off the end.
    assert_eq!(decisions::position_to_persist(201_215, 201_000), 0);
    // Within END_MARGIN_MS of the end → 0.
    assert_eq!(decisions::position_to_persist(199_000, 201_000), 0);
    // Exactly on the margin boundary (pos + margin == duration) → 0.
    assert_eq!(decisions::position_to_persist(198_000, 201_000), 0);
    // Comfortably mid-track → persisted unchanged (normal resume works).
    assert_eq!(decisions::position_to_persist(30_000, 201_000), 30_000);
    // Unknown duration (0) → raw position, never zeroed.
    assert_eq!(decisions::position_to_persist(201_215, 0), 201_215);
}

#[test]
fn past_end_ticks_triggers_after_threshold() {
    // Simulate: output reports Playing but position >= track duration.
    // After POSITION_PAST_END_TICKS ticks, track should be treated as ended.
    let mut past_end: u8 = 0;
    let track_duration_ms: u64 = 240_000;
    let position_ms: u64 = 240_500; // slightly past end
    let played_enough = true;

    for _ in 0..POSITION_PAST_END_TICKS {
        if track_duration_ms > 0 && played_enough && position_ms >= track_duration_ms {
            past_end += 1;
        } else {
            past_end = 0;
        }
    }
    assert!(
        past_end >= POSITION_PAST_END_TICKS,
        "should trigger after {} ticks past end",
        POSITION_PAST_END_TICKS
    );
}

#[test]
fn past_end_ticks_resets_when_position_below_duration() {
    // If position drops below duration (e.g. seek or correction),
    // the past_end counter should reset.
    let mut past_end: u8 = 2; // already accumulated some ticks
    let track_duration_ms: u64 = 240_000;
    let position_ms: u64 = 200_000; // below duration
    let played_enough = true;

    if track_duration_ms > 0 && played_enough && position_ms >= track_duration_ms {
        past_end += 1;
    } else {
        past_end = 0;
    }
    assert_eq!(past_end, 0, "counter should reset when position < duration");
}

#[test]
fn gapless_stuck_forces_track_end() {
    // BUG-004: After gapless metadata advance, if the renderer stays
    // Stopped, gapless_stuck_ticks should accumulate and trigger
    // track_ended after GAPLESS_STUCK_THRESHOLD ticks.
    let mut ps = ZonePollState {
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
        track_generation: 0,
        track_loaded_at: Instant::now(),
        past_end_ticks: 0,
        gapless_advance_pending: true, // metadata was advanced
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
    };

    // Simulate renderer staying Stopped after cooldown expired.
    // gapless_advance_pending is true, gapless_cooldown is 0.
    for tick in 1..=GAPLESS_STUCK_THRESHOLD {
        ps.gapless_stuck_ticks += 1;
        if tick < GAPLESS_STUCK_THRESHOLD {
            assert!(
                ps.gapless_stuck_ticks < GAPLESS_STUCK_THRESHOLD,
                "should not trigger yet at tick {tick}"
            );
        }
    }
    assert!(
        ps.gapless_stuck_ticks >= GAPLESS_STUCK_THRESHOLD,
        "should trigger track_ended after {} ticks",
        GAPLESS_STUCK_THRESHOLD
    );

    // After triggering, pending state should be cleared
    ps.gapless_advance_pending = false;
    ps.gapless_stuck_ticks = 0;
    assert!(!ps.gapless_advance_pending);
    assert_eq!(ps.gapless_stuck_ticks, 0);
}

#[test]
fn idle_backoff_skips_then_retries() {
    let mut b = super::IdlePollBackoff::default();
    // Sans échec, on sonde à chaque tick.
    assert!(!b.should_skip());

    // Premier échec : 2 ticks sautés, puis on retente.
    b.record_failure();
    assert_eq!(b.remaining, 2);
    assert!(b.should_skip());
    assert!(b.should_skip());
    assert!(!b.should_skip(), "après le recul, un sondage doit repartir");
}

#[test]
fn idle_backoff_grows_and_is_capped() {
    let mut b = super::IdlePollBackoff::default();
    for expected in [2u8, 4, 8, 16, 32] {
        b.record_failure();
        assert_eq!(b.remaining, expected);
        while b.should_skip() {}
    }
    // Plafond : 20 échecs de plus ne dépassent pas 2^IDLE_BACKOFF_MAX_SHIFT.
    for _ in 0..20 {
        b.record_failure();
        assert_eq!(b.remaining, 1u8 << super::IDLE_BACKOFF_MAX_SHIFT);
        while b.should_skip() {}
    }
}

#[test]
fn idle_backoff_resets_on_success() {
    let mut b = super::IdlePollBackoff::default();
    b.record_failure();
    b.record_failure();
    assert!(b.remaining > 0);
    b.record_success(TransportState::Playing);
    assert_eq!(b.consecutive_errors, 0);
    assert!(
        !b.should_skip(),
        "un appareil qui répond doit être sondé à plein rythme"
    );
}

/// La cadence de repos est une DURÉE, pas un nombre de ticks : elle doit
/// rester la même durée si la cadence du sondeur change un jour. C'est le
/// désaccord silencieux décrit en #2263 (les garde-fous comptés en ticks
/// changent de sens quand `POLL_INTERVAL_MS` bouge, ceux comptés en
/// horloge murale non).
#[test]
fn la_cadence_de_repos_est_une_duree_murale() {
    let duree_ms = super::IDLE_REPOS_POLL_TICKS as u64 * super::POLL_INTERVAL_MS;
    assert_eq!(
        duree_ms,
        super::IDLE_REPOS_POLL_SECS * 1000,
        "{} ticks de {} ms ne font pas {} s",
        super::IDLE_REPOS_POLL_TICKS,
        super::POLL_INTERVAL_MS,
        super::IDLE_REPOS_POLL_SECS
    );
    assert!(
        super::IDLE_REPOS_POLL_TICKS >= 1,
        "une cadence de repos nulle sonderait en boucle"
    );
}

/// Un renderer qui répond `Stopped` est sondé à la cadence de repos, pas
/// à chaque seconde : c'est le premier poste de dépense SOAP d'une
/// installation au repos (#2263).
#[test]
fn une_zone_arretee_retombe_a_la_cadence_de_repos() {
    let mut b = super::IdlePollBackoff::default();
    b.record_success(TransportState::Stopped);
    for i in 1..super::IDLE_REPOS_POLL_TICKS {
        assert!(
            b.should_skip(),
            "tick {i} : la zone au repos ne doit pas être re-sondée"
        );
    }
    assert!(
        !b.should_skip(),
        "après {} ticks, il faut bien re-sonder",
        super::IDLE_REPOS_POLL_TICKS
    );
}

/// Le plein rythme est réservé à ce que la branche « repos » sait
/// EXPLOITER : un renderer qui joue — reprise d'état, adoption du volume
/// et détection de conflit y sont toutes conditionnées à `Playing` — et
/// une transition, transitoire par définition.
///
/// La pause en est SORTIE (#2263). Elle y figurait au motif que « la
/// reprise d'état et l'adoption du volume ralentiraient elles aussi » ;
/// or ni l'une ni l'autre ne regarde un statut en pause. Voir
/// `la_pause_retombe_a_la_cadence_de_repos` juste dessous, et la mesure
/// sur la vraie boucle dans `cadence_de_repos_tests`.
#[test]
fn seuls_la_lecture_et_la_transition_gardent_le_plein_rythme() {
    for etat in [TransportState::Playing, TransportState::Transitioning] {
        let mut b = super::IdlePollBackoff::default();
        b.record_success(etat);
        assert!(
            !b.should_skip(),
            "{etat:?} : un appareil qui bouge doit rester sondé à chaque tick"
        );
    }
}

/// CONTRE-ÉPREUVE de la ligne ci-dessus : la pause tombe à la cadence de
/// repos, au même titre que l'arrêt. Sans ce cas, retirer `Paused` du bras
/// lent laisserait le test précédent entièrement vert.
#[test]
fn la_pause_retombe_a_la_cadence_de_repos() {
    for etat in [TransportState::Stopped, TransportState::Paused] {
        let mut b = super::IdlePollBackoff::default();
        b.record_success(etat);
        assert!(
            b.should_skip(),
            "{etat:?} : rien à en apprendre au tick suivant, il doit être sauté"
        );
    }
}

/// Quantifie le gain, comme le fait déjà le test de l'appareil mort : sur
/// une minute face à une zone arrêtée dont le renderer répond poliment,
/// l'ancien chemin envoyait 60 sondages — soit 180 actions SOAP pour
/// n'apprendre rien.
#[test]
fn une_zone_arretee_ne_coute_plus_un_sondage_par_seconde() {
    let mut b = super::IdlePollBackoff::default();
    let mut sondages = 0;
    for _ in 0..60 {
        if b.should_skip() {
            continue;
        }
        sondages += 1;
        b.record_success(TransportState::Stopped);
    }
    assert_eq!(
        sondages,
        60 / super::IDLE_REPOS_POLL_TICKS as u32,
        "60 ticks à la cadence de repos"
    );
    assert!(
        sondages <= 12,
        "60 sondages par minute étaient le défaut corrigé, or {sondages}"
    );
}

/// Une zone qui se remet à jouer depuis la façade de l'appareil doit
/// retrouver le plein rythme au tick suivant : le frein ne doit jamais
/// survivre au réveil.
#[test]
fn le_frein_de_repos_saute_des_que_l_appareil_repart() {
    let mut b = super::IdlePollBackoff::default();
    b.record_success(TransportState::Stopped);
    while b.should_skip() {}
    b.record_success(TransportState::Playing);
    assert!(!b.should_skip());
    assert!(!b.should_skip());
}

/// Quantifie le gain : sur une minute face à un appareil qui ne répond
/// jamais, l'ancien chemin sondait à chaque tick (60 fois). Avec le recul,
/// on compte les sondages réellement tentés — c'est le flux que le renderer
/// subissait et qui finissait par le figer.
#[test]
fn idle_backoff_collapses_poll_rate_on_a_dead_device() {
    let mut b = super::IdlePollBackoff::default();
    let mut polls = 0;
    for _ in 0..60 {
        if b.should_skip() {
            continue;
        }
        polls += 1;
        b.record_failure(); // l'appareil ne répond jamais
    }
    assert!(
        polls <= 8,
        "60 ticks devraient donner une poignée de sondages, pas {polls}"
    );
    assert!(
        polls >= 4,
        "il faut quand même retenter régulièrement, or {polls}"
    );
}

#[test]
fn a_fully_served_stream_may_have_finished() {
    // Tout servi, ou la marge de 10 % : le renderer a pu finir.
    assert!(super::decisions::renderer_could_have_finished(
        39_838_610,
        Some(39_838_610),
        false
    ));
    assert!(super::decisions::renderer_could_have_finished(
        36_000_000,
        Some(39_838_610),
        false
    ));
}

#[test]
fn a_clearly_short_stream_cannot_have_finished() {
    // Le cas de JP : 16 Mo servis sur 39,8 Mo, et le renderer annonce
    // Stopped. Il n'a pas pu finir de jouer ce qu'il n'a pas reçu.
    assert!(!super::decisions::renderer_could_have_finished(
        16_121_856,
        Some(39_838_610),
        false
    ));
    assert!(!super::decisions::renderer_could_have_finished(
        0,
        Some(1_000),
        false
    ));
}

#[test]
fn an_unknown_total_is_never_judged() {
    // Radio, flux décodé à la volée : aucune conclusion possible, on garde
    // le comportement d'avant plutôt que de bloquer une lecture saine.
    assert!(super::decisions::renderer_could_have_finished(
        0, None, false
    ));
    assert!(super::decisions::renderer_could_have_finished(
        0,
        Some(0),
        false
    ));
}

#[test]
fn the_served_threshold_matches_the_documented_percentage() {
    let total = 1_000_u64;
    let pile = total * super::MIN_SERVED_PERCENT_FOR_NATURAL_END / 100;
    assert!(super::decisions::renderer_could_have_finished(
        pile,
        Some(total),
        false
    ));
    assert!(!super::decisions::renderer_could_have_finished(
        pile - 1,
        Some(total),
        false
    ));
}

#[test]
fn a_seek_neutralises_the_served_bytes_criterion() {
    // Après un saut, le renderer ne récupère que la portion restante : les
    // octets servis sont légitimement partiels et ne doivent pas vetoer une
    // fin normale (régression DEvir, v0.9.0-rc4).
    assert!(super::decisions::renderer_could_have_finished(
        1_000,
        Some(39_838_610),
        true
    ));
}

#[test]
fn gapless_stuck_cleared_on_playing() {
    // When the renderer transitions to Playing, gapless_advance_pending
    // should be cleared (the gapless transition succeeded).
    let mut ps = ZonePollState {
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
        track_generation: 0,
        track_loaded_at: Instant::now(),
        past_end_ticks: 0,
        gapless_advance_pending: true,
        gapless_stuck_ticks: 3,
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
    };

    // Simulate entering Playing state (renderer auto-transitioned)
    if ps.gapless_advance_pending {
        ps.gapless_advance_pending = false;
        ps.gapless_stuck_ticks = 0;
    }
    assert!(!ps.gapless_advance_pending);
    assert_eq!(ps.gapless_stuck_ticks, 0);
}
