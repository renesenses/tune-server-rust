/// Qui tient réellement le renderer, d'après l'URI qu'il rapporte.
///
/// Le terrain (24/08, DMP-A8 de Bertrand) : DEUX serveurs Tune avaient
/// chacun une zone sur le même appareil, et le lecteur interne de
/// l'Eversolo s'y ajoutait après un redémarrage. Chaque perdant échouait
/// EN SILENCE — l'interface relançait la lecture toutes les quinze
/// secondes, et un conflit d'appareil s'est déguisé en « bug DSD ».
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TenueDuRenderer {
    /// Il joue notre flux — ou ne rapporte rien d'exploitable.
    LeNotre,
    /// Il joue le flux d'un AUTRE serveur Tune (l'hôte extrait de l'URI).
    AutreServeurTune(String),
    /// Il joue autre chose (BubbleUPnP, une autre application…).
    AutreApplication,
    /// URI vide mais transport actif : son propre lecteur interne
    /// (l'Eversolo restaure sa lecture locale après un redémarrage).
    LecteurInterne,
}

/// Décide qui tient le renderer.
///
/// - `current_uri = None` → [`TenueDuRenderer::LeNotre`] : beaucoup de
///   renderers ne rapportent pas `TrackURI` — l'absence de signal n'est
///   pas une preuve, on ne crie pas sans preuve ;
/// - URI vide → lecteur interne (ne se juge que transport actif, c'est à
///   l'appelant de ne demander qu'alors) ;
/// - URI portant NOTRE `stream_id` → à nous ;
/// - URI au motif `/stream/…` d'un flux Tune, sans notre id → un autre
///   serveur Tune, dont on extrait l'hôte pour le NOMMER à l'écran ;
/// - toute autre URI → une autre application.
pub fn qui_tient_le_renderer(
    current_uri: Option<&str>,
    notre_stream_id: Option<&str>,
) -> TenueDuRenderer {
    let Some(uri) = current_uri else {
        return TenueDuRenderer::LeNotre;
    };
    let uri = uri.trim();
    if uri.is_empty() {
        return TenueDuRenderer::LecteurInterne;
    }
    if let Some(sid) = notre_stream_id
        && !sid.is_empty()
        && uri.contains(sid)
    {
        return TenueDuRenderer::LeNotre;
    }
    if uri.contains("/stream/") {
        let hote = uri
            .strip_prefix("http://")
            .or_else(|| uri.strip_prefix("https://"))
            .and_then(|r| r.split('/').next())
            .unwrap_or("?")
            .to_string();
        return TenueDuRenderer::AutreServeurTune(hote);
    }
    TenueDuRenderer::AutreApplication
}

/// Retrouver, dans l'URI qu'un renderer déclare jouer, le `stream_id` de la
/// session Tune qu'il tire.
///
/// ── Pourquoi cette fonction existe (#2991) ──
///
/// La reprise « le renderer joue, Tune ne le croyait pas » reconstruit un
/// now-playing depuis la BASE (`last_track_source`, `last_track_source_id`)
/// et pose `stream_id: None` — la base ne mémorise pas l'identifiant d'une
/// session, qui est éphémère. Pour une piste locale c'est sans conséquence.
/// Pour une RADIO, c'est la panne : `refresh_radio_metadata` recopie ce
/// `None` dans chaque now-playing suivant, et la garde qui publie le titre
/// vers le flux ne mord plus JAMAIS de toute la session. L'interface Tune,
/// elle, continue d'être servie par `update_now_playing`, qui ne dépend pas
/// du `stream_id` — d'où « dans Tune ça fonctionne, sur le lecteur réseau
/// non » (Serge Asselin, Hifi Rose RS250A, fil 1529).
///
/// L'identifiant n'est pas perdu : le renderer l'annonce lui-même, dans
/// l'URI qu'il est en train de tirer. On ne l'invente pas, on le RELIT — et
/// l'appelant vérifie ensuite auprès du gestionnaire de flux que cette
/// session est bien l'une des NÔTRES (une URI `/stream/…` peut venir d'un
/// autre Tune du réseau, cf. [`TenueDuRenderer::AutreServeurTune`]).
///
/// La découpe `<id>.<ext>` est celle du serveur de flux lui-même
/// ([`crate::http::streamer::extract_stream_id`]), appelée et non recopiée :
/// le jour où la convention change, les deux bougent ensemble.
pub fn stream_id_de_l_uri(current_uri: Option<&str>) -> Option<String> {
    let uri = current_uri?.trim();
    let apres = uri.rsplit_once("/stream/")?.1;
    // Un renderer peut recopier une chaîne de requête ou une ancre.
    let sans_suffixe = apres.split(['?', '#']).next().unwrap_or(apres);
    let id = crate::http::streamer::extract_stream_id(sans_suffixe);
    (!id.is_empty()).then(|| id.to_string())
}

use super::{
    DEAD_START_RETRY_COOLDOWN_SECS, GAPLESS_STAGE_MAX_AGE_SECS, GAPLESS_STUCK_THRESHOLD,
    GAPLESS_WINDOW_MS, MIN_PEAK_UNKNOWN_DURATION_MS, MIN_PLAYED_FRACTION, MIN_TRACK_WALL_SECS,
    MIN_WALL_FRACTION_FOR_NATURAL_END, POLL_FAIL_END_MIN_ERRORS, POLL_INTERVAL_MS,
    POSITION_PAST_END_TICKS, STOPPED_TICKS_THRESHOLD,
};

/// Margin (ms) added to the track duration before position-based
/// end-of-track is accepted, to avoid clipping the last fraction of a
/// second on renderers that report position slightly ahead of playback.
pub const END_MARGIN_MS: u64 = 3000;

/// Motif — stable, journalisé — de la branche qui a conclu « la piste est
/// finie ». Un seul mot par branche, écrit tel quel dans `track_end_gap`.
///
/// Ce ne sont PAS des étiquettes décoratives : chaque branche porte un
/// plancher de silence différent (voir [`plancher_de_detection_ms`]), et
/// c'est ce plancher qui décide si un blanc de 3-4 s est explicable par la
/// détection ou s'il vient d'ailleurs (#2488).
pub mod motif_fin {
    /// La sortie locale a signalé l'EOF elle-même (`ended_naturally`).
    pub const FIN_NATURELLE_LOCALE: &str = "local_ended_naturally";
    /// DSD sur DLNA : le pic de position a atteint la fin (#402).
    pub const DSD_DLNA_PIC_ATTEINT: &str = "dlna_dsd_reached_end";
    /// Le renderer annonce `Stopped` et le compteur a atteint son seuil.
    pub const FIN_NATURELLE_APRES_STOPPED: &str = "natural_end_after_stopped";
    /// Le renderer avait accepté `SetNext` mais n'a jamais transitionné.
    pub const AVANCE_GAPLESS_BLOQUEE: &str = "gapless_advance_stuck";
    /// La position a dépassé la fin sans que le renderer s'arrête.
    pub const POSITION_AU_DELA_DE_LA_FIN: &str = "position_past_end";
}

/// Plancher de silence, en millisecondes, imposé par la BRANCHE de
/// détection qui a conclu — avant même que la résolution de la piste
/// suivante ne commence.
///
/// Pourquoi ce chiffre existe (#2488) : `playback_timing` démarre à
/// `play_inner`, c'est-à-dire APRÈS que le sondeur a décidé d'avancer.
/// Tout ce qui précède cette décision est aujourd'hui invisible dans le
/// journal, alors que c'est structurellement le plus gros terme du blanc
/// entre deux pistes. Cette fonction rend ce terme lisible sans avoir à
/// relire la source : elle ne dérive que des constantes du sondeur.
///
/// C'est un PLANCHER, pas une mesure : le temps réel s'y ajoute (un tick
/// n'est pas aligné sur la fin du morceau, et le réseau a son mot à dire).
///
/// Un motif inconnu rend `0` — on ne devine pas.
pub fn plancher_de_detection_ms(motif: &str) -> u64 {
    let tick = POLL_INTERVAL_MS;
    match motif {
        // La sortie locale réveille le sondeur par `TRACK_END_NOTIFY`
        // au lieu d'attendre le tick : aucun plancher de sondage.
        motif_fin::FIN_NATURELLE_LOCALE => 0,
        // Il faut UN sondage pour observer que le pic a atteint la fin.
        motif_fin::DSD_DLNA_PIC_ATTEINT => tick,
        // Le compteur `stopped_ticks` doit atteindre son seuil.
        motif_fin::FIN_NATURELLE_APRES_STOPPED => STOPPED_TICKS_THRESHOLD as u64 * tick,
        // Compté depuis la fin de la temporisation gapless, qui s'ajoute
        // encore devant : le renderer a accepté `SetNext` puis n'a rien
        // fait, et on attend `GAPLESS_STUCK_THRESHOLD` sondages.
        motif_fin::AVANCE_GAPLESS_BLOQUEE => GAPLESS_STUCK_THRESHOLD as u64 * tick,
        // La position doit d'abord dépasser la fin de `END_MARGIN_MS`
        // — autant de silence déjà écoulé — puis tenir
        // `POSITION_PAST_END_TICKS` sondages.
        motif_fin::POSITION_AU_DELA_DE_LA_FIN => {
            END_MARGIN_MS + POSITION_PAST_END_TICKS as u64 * tick
        }
        _ => 0,
    }
}

/// Has enough of the current track been played to accept a track-end or
/// gapless transition?
///
/// - Known duration: `peak_position_ms >= MIN_PLAYED_FRACTION * duration`.
/// - Unknown duration (`0`): `peak_position_ms >= MIN_PEAK_UNKNOWN_DURATION_MS`
///   (guards slow renderers that report duration 0 while buffering).
///
/// Both branches additionally require `wall_elapsed >= MIN_TRACK_WALL_SECS`.
/// Le renderer peut-il réellement avoir terminé le morceau ?
///
/// Un `Stopped` au-delà de [`MIN_PLAYED_FRACTION`] est accepté comme une fin
/// naturelle, en se fiant à la position qu'annonce le renderer. Or sur un
/// réseau qui hoquette, il cale, cesse de récupérer le flux et annonce
/// `Stopped` — Tune enchaînait alors sur la piste suivante, amputant la fin
/// du morceau **sans laisser la moindre trace** (« Us And Them » de JP :
/// 6:36 jouées sur 7:49).
///
/// Les octets servis tranchent, indépendamment de ce que le renderer
/// raconte : on ne finit pas de jouer un fichier qu'on n'a pas reçu.
/// `total_bytes` à `None` (radio, flux décodé) ⇒ on ne juge pas.
///
/// `seeked` neutralise le critère : après un saut dans le morceau, le
/// renderer ne récupère que la portion restante, les octets servis sont donc
/// légitimement incomplets et vetoraient une fin parfaitement normale.
/// (`ZoneState::last_seek_at` est remis à zéro par `play()` à chaque
/// changement de piste, il vaut donc bien « un saut a eu lieu sur CETTE
/// piste ».)
pub fn renderer_could_have_finished(
    bytes_sent: u64,
    total_bytes: Option<u64>,
    seeked: bool,
) -> bool {
    if seeked {
        return true;
    }
    match total_bytes {
        None | Some(0) => true,
        Some(total) => {
            bytes_sent.saturating_mul(100)
                >= total.saturating_mul(super::MIN_SERVED_PERCENT_FOR_NATURAL_END)
        }
    }
}

pub fn played_enough(track_duration_ms: u64, peak_position_ms: u64, wall_elapsed: u64) -> bool {
    if track_duration_ms == 0 {
        peak_position_ms >= MIN_PEAK_UNKNOWN_DURATION_MS && wall_elapsed >= MIN_TRACK_WALL_SECS
    } else {
        // A track shorter than MIN_TRACK_WALL_SECS can never reach that much
        // wall-clock time even when played in full — cap the wall-time floor
        // at ~80% of the track's own duration so short tracks (e.g. a 27s
        // streaming variation) are still recognized as ending naturally.
        // Without this, sub-30s tracks never trigger natural-end, so
        // auto-advance and single-track Repeat All silently stop.
        let wall_floor = MIN_TRACK_WALL_SECS.min(track_duration_ms / 1000 * 4 / 5);
        peak_position_ms as f64 >= track_duration_ms as f64 * MIN_PLAYED_FRACTION
            && wall_elapsed >= wall_floor
    }
}

/// Whether a renderer's `ended_naturally` signal is plausible given elapsed
/// wall-clock time. For a known-duration track it must have been playing at
/// least `MIN_WALL_FRACTION_FOR_NATURAL_END` of its duration (you cannot end
/// a 4-minute track in 35 seconds at 1x). Unknown duration keeps the original
/// modest 5-second floor. Rejects the DMP-A8's spurious early ended_naturally.
///
/// The premise — nothing finishes a track faster than 1x — only holds for a
/// renderer. An output that reports `realtime: false` is exempt; see
/// [`natural_end`], which is where that exemption is applied.
pub fn ended_naturally_wall_ok(wall_elapsed: u64, track_duration_ms: u64) -> bool {
    if track_duration_ms == 0 {
        wall_elapsed >= 5
    } else {
        wall_elapsed as f64 * 1000.0 >= track_duration_ms as f64 * MIN_WALL_FRACTION_FOR_NATURAL_END
    }
}

/// « Démarrage mort » (#2394) : l'échec de lecture DLNA où la piste n'a
/// JAMAIS été tirée (0 octet servi). C'est le profil du pipeline Eversolo
/// coincé — SOAP et HTTP vivants, lecture morte — que la relance guérit.
/// Un décrochage en cours de lecture (octets déjà servis) n'en est pas un.
///
/// `bytes_sent` est un compte MESURÉ : l'appelant ne doit l'appeler que
/// quand la consommation est connue ([`super::fsm::ConsommationFlux::ASec`]).
/// Un flux dont personne ne connaît le compteur n'est pas un démarrage
/// mort — voir [`super::fsm::consommation_flux`].
pub fn demarrage_mort(output_type: &str, bytes_sent: u64) -> bool {
    output_type == "dlna" && bytes_sent == 0
}

/// Is a `Playing`-but-dead watchdog meaningful for this sample?
///
/// Every gate removes a known false positive: this is DLNA-only, Tune must
/// own an actual realtime stream, startup and seek recovery must be over,
/// the renderer must already have proven that it reports position, and a
/// known track end is left to the normal end-of-track machinery.
pub fn dlna_playing_stall_eligible(
    output_type: &str,
    tune_is_playing: bool,
    renderer_is_playing: bool,
    realtime: bool,
    has_stream_id: bool,
    in_seek_grace: bool,
    load_elapsed_secs: u64,
    peak_position_ms: u64,
    position_ms: u64,
    track_duration_ms: u64,
) -> bool {
    let near_known_end =
        track_duration_ms > 0 && position_ms.saturating_add(END_MARGIN_MS) >= track_duration_ms;
    output_type == "dlna"
        && tune_is_playing
        && renderer_is_playing
        && realtime
        && has_stream_id
        && !in_seek_grace
        && load_elapsed_secs >= super::TRACK_LOAD_GRACE_SECS
        && peak_position_ms >= 5_000
        && !near_known_end
}

/// Advance the consecutive-stall counter only when both independent signs
/// of life are frozen. Any movement resets the full observation window.
pub fn next_dlna_playing_stall_ticks(
    previous_ticks: u8,
    eligible: bool,
    previous_position_ms: u64,
    position_ms: u64,
    previous_bytes_sent: u64,
    bytes_sent: u64,
) -> u8 {
    if !eligible || position_ms > previous_position_ms || bytes_sent > previous_bytes_sent {
        0
    } else {
        previous_ticks.saturating_add(1)
    }
}

/// Une relance automatique après démarrage mort est-elle permise ?
/// Au plus une par fenêtre : si la précédente date de moins de
/// DEAD_START_RETRY_COOLDOWN_SECS, l'échec suivant coupe la zone comme
/// avant — on ne martèle pas un appareil réellement planté ou éteint.
pub fn relance_demarrage_mort_autorisee(derniere_il_y_a_secs: Option<u64>) -> bool {
    derniere_il_y_a_secs.is_none_or(|s| s > DEAD_START_RETRY_COOLDOWN_SECS)
}

/// Le verrou « suivant DSD sur DLNA » (#2394) tient-il encore ? Il ne
/// tient que pour LA position de file constatée : si la file bouge (ajout,
/// saut, avance), la position suivante change et on re-résout — au pire on
/// perd UNE occasion d'armer le gapless (petit blanc), jamais une lecture.
pub fn dsd_skip_latched(latch: Option<i64>, next_pos: Option<i64>) -> bool {
    latch.is_some() && latch == next_pos
}

/// Position dropped from `>30s` to `<5s` while a gapless transition was
/// armed — a strong signal the renderer auto-advanced to the next track.
pub fn position_reset(last_position_ms: u64, position_ms: u64, gapless_armed: bool) -> bool {
    last_position_ms > 30_000 && position_ms < 5_000 && gapless_armed
}

/// The `position_reset` fallback advances metadata only, assuming the
/// renderer auto-transitioned internally — its position dropped to 0 because
/// it is already playing the next track. That premise holds only for outputs
/// that do internal gapless (DLNA). For a Chromecast / slimproto /
/// exclusive-local output, a drop to 0 means the track ENDED (device went
/// IDLE/FINISHED), not that it advanced — advancing metadata sends no `play`
/// and steals the event from the natural-end path (Stopped branch →
/// play_from_queue = real load), causing the endless 1-2s-then-zero loop
/// (Rhorn, #1072). So the fallback only fires for internal-gapless outputs.
/// It is also forbidden during the seek grace period: recreating the current
/// stream can produce the same position-drop shape, but it never means the
/// renderer moved to the next queue item (#2170).
pub fn position_reset_fires(
    raw_position_reset: bool,
    can_internal_gapless: bool,
    in_seek_grace: bool,
) -> bool {
    raw_position_reset && can_internal_gapless && !in_seek_grace
}

/// A renderer can report the PREVIOUS session's position for the first
/// seconds after a fresh Play (Villerio's DMP-A6: ~374s — yesterday's end
/// position — reported 6s into a new start). That stale sample poisons the
/// peak, triggers near-end gapless staging seconds into the track, and the
/// snap back to the real position then reads as a phantom
/// `position_reset` advance.
///
/// A real position can never exceed the wall time actually elapsed (+15s
/// margin for seek-restore/clock slack): `track_started_at` is folded by
/// the seek/resume target (see the "Fold a NEW seek" baseline above), so
/// `wall_elapsed` tracks the true 1x play position at every point in the
/// track — not only the first few seconds. Any sample above that ceiling
/// is therefore provably impossible and must be discarded outright,
/// whenever it arrives.
///
/// This used to be gated on `wall_elapsed_secs < 30`, which let a renderer
/// that keeps reporting a stale near-end position for LONGER than 30s
/// (Bertrand's DMP-A8, .18) poison the peak the instant the 30s grace
/// lapsed: peak jumped to the fake near-end value, `played_enough` flipped
/// true, and the very next honest snap-to-0 read as a `position_reset`
/// advance ~30s into track 1 — the queue pointer ran ahead of the renderer
/// and the Qobuz playlist appeared to "stop at the first track". Dropping
/// the window makes the invariant hold for the whole track and cures both
/// the poisoned-peak advance and the near-end gapless mis-staging.
pub fn stale_start_position(wall_elapsed_secs: u64, position_ms: u64) -> bool {
    position_ms > wall_elapsed_secs * 1000 + 15_000
}

/// The peak position reached (near) the track's full duration, so the track
/// has demonstrably finished — independent of the wall clock.
///
/// The wall-clock guards (`played_enough`, `ended_naturally_wall_ok`) reset
/// `track_started_at` on a gapless metadata advance, so when a local FLAC
/// track (whose gapless pre-arm falls back — the next stream isn't WAV) ends
/// a couple seconds later, `wall_elapsed` under-counts and those guards
/// wrongly reject the real end. That stalled auto-advance for ~30s, which
/// surfaced as tracks restarting/being skipped in a gapless album (Jean
/// Valjean, local FLAC on WASAPI). When the peak has reached the duration
/// the track is over regardless of the (unreliable) wall clock.
pub fn peak_reached_end(track_duration_ms: u64, peak_position_ms: u64) -> bool {
    track_duration_ms > 0
        && peak_position_ms as f64 >= track_duration_ms as f64 * MIN_PLAYED_FRACTION
}

/// A DSD track on a DLNA renderer that has demonstrably reached its end.
///
/// Gapless (`SetNextAVTransportURI`) is intentionally NOT armed when the next
/// track is DSD on a DLNA renderer (`prepare_gapless` skips it — the renderer
/// accepts SetNext for a DSD stream but never consumes it, so the album cuts
/// after track 1; HiFi Rose RS130, Benjithom, #402). But DLNA `poll_status`
/// never reports `ended_naturally`, so with gapless off the only end-of-track
/// signal left is counting `STOPPED_TICKS_THRESHOLD` Stopped polls — a fixed
/// ~5s inter-track gap for a DSD album (Benjithom, RS130). When the peak
/// position has reached the track's end we already know it finished, so the
/// poller can advance immediately instead of waiting out the counter. PCM/FLAC
/// on DLNA keep their armed-gapless path and never reach this predicate; DSD on
/// a local output keeps its internal gapless chain and is out of scope here.
pub fn dlna_dsd_reached_end(
    output_type: &str,
    current_format: Option<&str>,
    track_duration_ms: u64,
    peak_position_ms: u64,
) -> bool {
    if output_type != "dlna" {
        return false;
    }
    let is_dsd = current_format.is_some_and(crate::playback::gapless::est_dsd);
    is_dsd && peak_reached_end(track_duration_ms, peak_position_ms)
}

/// After `STOPPED_TICKS_THRESHOLD` consecutive Stopped ticks, should this be
/// treated as a natural track end (re-trigger play) rather than a playback
/// failure (stop the zone)?
///
/// `realtime` is [`OutputStatus::realtime`](tune_output_api::OutputStatus):
/// `false` means the output does not consume the track at 1x — a recorder
/// that writes the container to disk at network speed finishes a 5-minute
/// track in a second or two. Every wall-clock plausibility guard here
/// (`played_enough`'s floor, `ended_naturally_wall_ok`) assumes 1x playback,
/// so for such an output `ended_naturally` + Stopped is taken at face value.
/// Without this the queue advanced only after half of each track's DURATION
/// had elapsed, and a rip ran at half of listening speed instead of network
/// speed.
///
/// # `peak_reached_end` était écrit pour Jean Valjean, et ne l'atteignait pas (#3229)
///
/// [`peak_reached_end`] n'avait qu'UN appelant de production : la branche
/// `status.ended_naturally` du bras `Stopped`, dont le commentaire dit
/// lui-même « Local outputs (WASAPI/ALSA/CoreAudio) signal ended_naturally
/// when the audio stream reaches EOF ». Or **DLNA ne rend JAMAIS
/// `ended_naturally`** — c'est écrit noir sur blanc dans
/// [`dlna_dsd_reached_end`], qui n'existe que pour cette raison. Le seul
/// autre usage, [`dlna_dsd_reached_end`], est réservé au DSD.
///
/// Le correctif écrit POUR le signalement de Jean Valjean ne mordait donc
/// sur aucune zone DLNA en PCM/FLAC : là, la fin de piste retombe ici,
/// après `STOPPED_TICKS_THRESHOLD` sondes `Stopped`. Et ici, `played_enough`
/// exige un plancher d'horloge murale que l'avance gapless vient justement
/// de fausser (elle remet `track_started_at`) — la vraie fin était rejetée,
/// et la zone partait vers la branche d'ÉCHEC qui l'arrête.
///
/// `peak_reached_end` est la même mesure que `played_enough` SANS ce
/// plancher d'horloge, et elle reste plus étroite que lui sur les deux
/// autres axes : elle exige une durée connue (`> 0`) et le même
/// [`MIN_PLAYED_FRACTION`]. L'ajouter ici ne change RIEN au calendrier — on
/// est toujours après les cinq sondes `Stopped` — mais change le VERDICT :
/// une piste dont le pic a atteint sa fin est une fin, pas une panne.
pub fn natural_end(
    played_enough: bool,
    repeat_active: bool,
    peak_position_ms: u64,
    ended_naturally: bool,
    wall_elapsed: u64,
    track_duration_ms: u64,
    realtime: bool,
) -> bool {
    let is_short_track = track_duration_ms > 0 && track_duration_ms < MIN_TRACK_WALL_SECS * 1000;
    let repeat_end = repeat_active && peak_position_ms > 5_000;
    played_enough
        // Le pic a atteint la fin : la piste est finie, quoi qu'en dise
        // l'horloge murale. C'est ce qui branche enfin le correctif sur le
        // chemin DLNA — le seul que Jean Valjean ait jamais emprunté.
        || peak_reached_end(track_duration_ms, peak_position_ms)
        || repeat_end
        || (ended_naturally
            && (!realtime || ended_naturally_wall_ok(wall_elapsed, track_duration_ms)))
        || (is_short_track && peak_position_ms as f64 >= track_duration_ms as f64 * 0.5)
}

/// Should the poller adopt the renderer-reported volume into the saved
/// zone volume? Only when the reported value actually MOVED since the last
/// poll (a real change on the device) AND it now differs from what we have
/// stored. A renderer that keeps reporting a stale default (Fabien's
/// Devialet stuck at 50%) reports the same value every tick, so `prev`
/// never differs from `device` and the user's saved volume is preserved.
pub fn should_adopt_device_volume(
    prev_device_vol: Option<f64>,
    device_vol: f64,
    db_vol: f64,
) -> bool {
    prev_device_vol.is_some_and(|prev| (device_vol - prev).abs() > 0.02)
        && (device_vol - db_vol).abs() > 0.02
}

/// The renderer now reports a duration that differs from the current
/// track's by more than 2s — a signal that a gapless transition to the
/// next track has occurred (only meaningful once gapless was armed).
pub fn duration_changed(
    gapless_sent: bool,
    track_duration_ms: u64,
    reported_duration_ms: u64,
) -> bool {
    gapless_sent
        && track_duration_ms > 0
        && reported_duration_ms > 0
        && (reported_duration_ms as i64 - track_duration_ms as i64).unsigned_abs() > 2000
}

/// Does the reported position confirm we are genuinely at the end of the
/// current track (or reset to the start of the next one)? Guarded by
/// `played_enough` to reject false transitions on renderers (DMP-A8) that
/// briefly report position < 5s right after SetNextAVTransportURI.
pub fn position_confirms_transition(
    played_enough: bool,
    position_ms: u64,
    track_duration_ms: u64,
) -> bool {
    played_enough
        && (position_ms < 5000
            || (track_duration_ms > 0
                && position_ms >= track_duration_ms.saturating_sub(GAPLESS_WINDOW_MS)))
}

/// Should `SetNextAVTransportURI` be sent now — i.e. playback has entered
/// the final `GAPLESS_WINDOW_MS` of the track and gapless is not yet armed?
///
/// Uses the renderer-reported duration when it is available, otherwise falls
/// back to the queue-known duration (`queue_duration_ms`). The LMS UPnP
/// bridge (Yacine/Jean-Pierre) reports `reported_duration_ms == 0`, so
/// without the fallback gapless was never armed for it (0/196 advances). A
/// well-behaved renderer reports its own duration and is unaffected.
pub fn should_arm_gapless(
    gapless_sent: bool,
    reported_duration_ms: u64,
    queue_duration_ms: u64,
    position_ms: u64,
) -> bool {
    let effective_duration_ms = sane_current_duration(reported_duration_ms, queue_duration_ms);
    !gapless_sent
        && effective_duration_ms > GAPLESS_WINDOW_MS
        && position_ms >= effective_duration_ms - GAPLESS_WINDOW_MS
}

/// La piste mise en attente a-t-elle expire ?
///
/// `age_secs` est le temps ecoule depuis `prepare_gapless`. Au-dela de
/// `GAPLESS_STAGE_MAX_AGE_SECS`, le flux ouvert pour elle a ete abandonne
/// cote serveur et l'adresse ne repond plus : il faut repreparer.
pub fn gapless_stage_expired(gapless_sent: bool, age_secs: Option<u64>) -> bool {
    gapless_sent && age_secs.is_some_and(|a| a > GAPLESS_STAGE_MAX_AGE_SECS)
}

/// La preparation gapless deja acceptee par le renderer vise-t-elle encore
/// la piste que la file designe MAINTENANT comme suivante ? (#3026)
///
/// `armed_row_id` est l'identifiant de LIGNE de file (`queue_items.id`)
/// realement passe a `SetNextAVTransportURI` ; `row_id_at_next` celui que
/// `next_position` designe a cet instant. Une ligne, et non une position :
/// « Lire ensuite » DECALE les positions — `insert_at` ouvre un trou par un
/// `UPDATE position = position + 1`, il ne reecrit pas les lignes — donc la
/// piste armee change de position sans changer de piste, et l'index+1
/// designe alors quelqu'un d'autre.
///
/// Ni l'identifiant de piste ni le titre ne feraient l'affaire : la meme
/// piste peut occuper deux lignes de la file (journal Sandro du 01/09 :
/// `Hold Me` ajoute deux fois, positions 4 puis 5), et ces deux lignes-la
/// doivent rester distinctes.
///
/// Sens de defaut : sans les DEUX identifiants on ne conclut rien. Un
/// « perime » de trop desarmerait un enchainement valide, c'est-a-dire
/// ferait payer un blanc a qui n'a rien demande.
pub fn gapless_arm_outdated(armed_row_id: Option<i64>, row_id_at_next: Option<i64>) -> bool {
    match (armed_row_id, row_id_at_next) {
        (Some(arme), Some(suivant)) => arme != suivant,
        _ => false,
    }
}

/// The renderer-reported duration for the CURRENT track, sanitised against
/// the queue-known (DB) duration.
///
/// The renderer's own reported duration is normally authoritative and is
/// trusted verbatim — a well-behaved renderer that reports a slightly (or
/// even a few times) different value than the scanned duration is kept as-is
/// on purpose. But some renderers report an *egregiously* wrong duration for
/// the playing track — the HiFi Rose RS130 reports e.g. 17000 ms for a track
/// that is really 174693 ms. Fed into the gapless-arming window that either
/// armed SetNextAVTransportURI near t=0 (far too small) or never at all (far
/// past the real end), cutting the album. Only when the reported value is
/// off by more than 4x (or under a quarter) of a known DB duration — a gap
/// no legitimate renderer/encoding difference produces — do we distrust it
/// and use the DB duration. A `0` reported (LMS UPnP bridge) falls back to
/// the DB as before; an unknown DB (0) means we can't judge, so keep the
/// reported value.
pub fn sane_current_duration(reported_ms: u64, db_ms: u64) -> u64 {
    let reported_is_egregious = db_ms > 0
        && reported_ms > 0
        && (reported_ms > db_ms.saturating_mul(4) || reported_ms < db_ms / 4);
    if reported_ms == 0 || reported_is_egregious {
        db_ms
    } else {
        reported_ms
    }
}

/// Position-based end-of-track: the output still reports Playing but the
/// position has run past `duration + END_MARGIN_MS` (e.g. a local/cpal
/// output draining its ring buffer). One tick's worth of the condition —
/// the caller still requires `POSITION_PAST_END_TICKS` consecutive hits.
pub fn past_end_reached(track_duration_ms: u64, played_enough: bool, position_ms: u64) -> bool {
    track_duration_ms > END_MARGIN_MS
        && played_enough
        && position_ms >= track_duration_ms.saturating_add(END_MARGIN_MS)
}

/// UN tick ou l'etat annonce se contredit lui-meme : la piste a une duree
/// connue, et la position rapportee est arrivee a la fin — ou l'a depassee —
/// alors que l'appareil dit toujours jouer (#2493, Tades : 1'46 « en
/// lecture » depuis dix minutes sur une Serenade/upmpdcli).
///
/// Ce predicat ne DECIDE rien : l'appelant s'en sert uniquement pour
/// compter, puis pour le DIRE. Aucune piste n'est avancee ni arretee par ce
/// chemin, et c'est delibere — la meme forme est produite par deux causes
/// qu'aucune horloge ne sait distinguer :
///
/// - la lecture est reellement bloquee (le renderer ment sur son etat) ;
/// - la duree que Tune connait est FAUSSE (etiquette erronee, piste plus
///   longue que ce que le scan a mesure) et la lecture est parfaitement
///   valide.
///
/// Couper dans le second cas amputerait une ecoute legitime. On se contente
/// donc de cesser d'affirmer ce qu'on ne sait pas.
///
/// Ce que le predicat refuse de qualifier :
/// - `source_radio` : un flux de radio n'a pas de fin, sa position n'a rien
///   a depasser ;
/// - `duree_connue_ms` nulle ou derisoire : sans duree il n'y a pas de
///   depassement possible — le seuil reprend celui de [`past_end_reached`]
///   ([`END_MARGIN_MS`]) pour que les deux parlent de la meme « fin ».
///
/// La duree passee par l'appelant est la duree EFFECTIVE
/// (`max(file d'attente, sane_current_duration)`) : une duree de base trop
/// courte face a un renderer credible est deja elargie la, donc le cas « le
/// scan a sous-estime la piste » ne remonte meme pas jusqu'ici.
pub fn position_au_dela_de_la_duree(
    source_radio: bool,
    duree_connue_ms: u64,
    position_ms: u64,
) -> bool {
    !source_radio
        && duree_connue_ms > END_MARGIN_MS
        && position_ms.saturating_add(END_MARGIN_MS) >= duree_connue_ms
}

/// Position to persist for auto-resume. A position within `END_MARGIN_MS`
/// of the track end means the track is effectively complete; persist 0 so a
/// later auto-resume plays it from the start instead of seeking into the
/// end zone — which, on an exclusive output, immediately trips the
/// `reached_end_exclusive` past-end detector and (repeat=All) restarts the
/// track at 0:00. Seen by DEvir on an ASIO Fireface with Tidal HI-RES whose
/// real decoded duration (201.377 s) exceeds the rounded metadata (201.000 s),
/// so the periodically-saved position (201215 ms) landed past `duration`.
/// `duration_ms == 0` (unknown) persists the raw position unchanged.
pub fn position_to_persist(position_ms: u64, duration_ms: u64) -> u64 {
    if duration_ms > 0 && position_ms.saturating_add(END_MARGIN_MS) >= duration_ms {
        0
    } else {
        position_ms
    }
}

/// Identity key of the currently playing track for the once-per-track
/// scrobble latch (#1113).
///
/// The latch used to be a plain boolean reset only when
/// `track_generation` changed — but a gapless advance
/// (`advance_queue_metadata`) deliberately does NOT bump the generation
/// (the poller needs its cooldown intact), so every gapless-reached track
/// (tracks 2, 4, 6… of an album) kept the latch stuck `true` and was never
/// scrobbled. Keying the latch on the track's identity re-arms it on ANY
/// path that changes what is playing — explicit play (generation bump),
/// gapless metadata advance (queue position / track change), or the local
/// output's internal chain — without per-call-site reset bookkeeping.
///
/// `track_generation` still participates so repeat-one (same track, same
/// queue position, new play) scrobbles each pass.
pub fn scrobble_track_key(
    track_generation: u64,
    queue_position: i64,
    track_id: Option<i64>,
    title: &str,
    artist: Option<&str>,
) -> String {
    // Prefer the stable library id: mid-track metadata refinements
    // (cover/format updates) must not look like a new track.
    match track_id {
        Some(id) => format!("{track_generation}:{queue_position}:id={id}"),
        None => format!(
            "{track_generation}:{queue_position}:{title}\u{1f}{}",
            artist.unwrap_or_default()
        ),
    }
}

/// Should the poller dispatch a scrobble this tick?
///
/// L'echeance de sondage d'une radio est-elle atteinte ?
///
/// Ne prend PAS l'etat du transport en parametre, et c'est tout l'objet du
/// correctif : le titre diffuse se lit sur une API externe ou dans le flux
/// ICY, independamment de ce que fait le renderer. Seul le TEMPS ecoule
/// commande — le tick du poller est a la seconde, l'API de la station non.
pub fn radio_poll_due(since_last: std::time::Duration, interval_secs: u64) -> bool {
    since_last >= std::time::Duration::from_secs(interval_secs)
}

/// Faut-il rafraichir les metadonnees radio d'une zone qui n'a AUCUN
/// peripherique de sortie ?
///
/// Trois conditions, et la troisieme est celle qu'on oublie : la zone joue,
/// la source est une radio, et l'etranglement est echu. Sans le dernier
/// point on interrogerait l'API de la station a chaque tick du poller,
/// c'est-a-dire toutes les secondes.
///
/// Le cas `source != "radio"` n'est pas theorique : une zone navigateur qui
/// joue un fichier local passe ici a chaque tick et ne doit declencher
/// aucun appel reseau.
pub fn deviceless_radio_refresh_due(
    is_playing: bool,
    source: Option<&str>,
    since_last_poll: std::time::Duration,
    interval_secs: u64,
) -> bool {
    is_playing && source == Some("radio") && radio_poll_due(since_last_poll, interval_secs)
}

/// Une lecture sans destination doit-elle cesser d'être annoncée ?
///
/// Une zone sans périphérique de sortie prépare son flux, renonce à
/// l'envoyer (`no_output_device_id_skipping_send_to_output`) et passe
/// quand même en lecture : `orchestrator_play … output_sent=false`. Rien
/// ne revenait ensuite sur cet état — la branche « pas de périphérique »
/// du poller se termine par un `continue` — et la zone restait annoncée
/// « en cours » indéfiniment, barre de progression comprise (#2630,
/// journal de Pierre M).
///
/// **Ce que ce prédicat ne regarde surtout pas : la présence d'un
/// périphérique.** Une zone navigateur n'en a JAMAIS — sa sortie est
/// l'onglet, qui tire `stream_url` lui-même. Une garde « pas de
/// périphérique ⇒ pas de lecture » les rendrait toutes muettes ; c'est la
/// régression exacte de `70401f2d`, réparée par #2657. Le seul fait
/// observable est la CONSOMMATION du flux, et c'est la même preuve que
/// `output_reach` utilise déjà pour dire `browser_unattended`.
///
/// On n'abandonne donc que sur des faits POSITIFS :
/// - le démarrage est daté (`last_play_started_at` est `#[serde(skip)]` :
///   après une restauration d'état il vaut `None`, et on ne conclut rien),
///   et il remonte à plus que [`DELAI_SILENCE_ETABLI`] ;
/// - le streamer CONNAÎT ce flux et déclare `0` octet servi. `None`
///   (session inconnue, disparue, requête en échec) n'est pas une preuve
///   de silence : on laisse la lecture tranquille.
///
/// Le sens du doute est celui de #2657 : on préfère taire un silence réel
/// que d'en inventer un.
pub fn lecture_sans_destination_abandonnee(
    depuis_le_demarrage: Option<std::time::Duration>,
    octets_servis: Option<u64>,
) -> bool {
    depuis_le_demarrage.is_some_and(|d| d >= super::DELAI_SILENCE_ETABLI)
        && octets_servis == Some(0)
}

/// L'autoplay doit-il chercher DANS LE SERVICE de la piste en cours ?
///
/// Vrai des que l'ecoute vient d'un service de streaming. Le repli
/// streaming existait deja (#1443) mais restait conditionne a « rien
/// trouve en local » : chez qui possede une bibliotheque locale ET un
/// abonnement, le generateur local repondait presque toujours quelque
/// chose, et l'autoplay renvoyait donc des titres locaux au milieu d'une
/// ecoute Qobuz (Sandro, 0.9.70).
///
/// La source de la piste en cours est la meilleure expression de ce que
/// l'auditeur ecoute : on la suit, et le local reste le filet.
pub fn autoplay_prefers_streaming(source: Option<&str>) -> bool {
    matches!(source, Some(s) if !s.is_empty() && s != "local")
}

/// True when the playing track differs from the one already latched
/// (`latched_key`) AND it has genuinely been listened past the Last.fm
/// threshold (50% / 4 min, `should_scrobble`). Radio never scrobbles.
pub fn should_dispatch_scrobble(
    latched_key: Option<&str>,
    current_key: &str,
    source: &str,
    duration_ms: i64,
    position_ms: i64,
) -> bool {
    source != "radio"
        && latched_key != Some(current_key)
        && crate::scrobble::should_scrobble((duration_ms > 0).then_some(duration_ms), position_ms)
}

/// Wall-clock end-of-track fallback for a DLNA renderer that reports no
/// usable duration of its own (`reported_duration_ms == 0`) — the LMS UPnP
/// bridge over a USB/Squeezebox DAC (Yacine/Jean-Pierre). Such a bridge
/// never reports an advancing position past the end and never signals
/// `ended_naturally`, so BOTH `past_end_reached` (needs a real position) and
/// the Stopped-arm natural-end path stall — 0/196 auto-advances.
///
/// When Tune knows the track length from the QUEUE (`queue_duration_ms`) and
/// the wall clock (from `track_started_at`, folded on seek) says the whole
/// track plus `END_MARGIN_MS` has elapsed while the renderer still claims to
/// be Playing, the track has effectively ended. The caller still requires
/// `POSITION_PAST_END_TICKS` consecutive hits (shared counter) so a single
/// stray tick can't false-advance.
///
/// Guards against regressing a well-behaved renderer:
/// - `is_dlna`: only the DLNA output type (openhome/chromecast/bluos/local
///   keep their own paths).
/// - `reported_duration_ms == 0`: a renderer that reports its own duration
///   uses the accurate position/duration path and never reaches here.
/// - Only evaluated inside the `Playing` arm, so a Paused device is excluded.
/// - The caller additionally gates on `!in_seek_grace`.
///
/// It intentionally does NOT require the peak-position `played_enough` guard:
/// the offending bridge freezes its reported position (often at 0), so a
/// peak-based check would veto every real end. The wall clock — which only
/// reaches `duration + margin` after that much real time has genuinely
/// elapsed at 1x — is the sole reliable evidence here.
pub fn wall_clock_past_end(
    is_dlna: bool,
    reported_duration_ms: u64,
    queue_duration_ms: u64,
    wall_elapsed_secs: u64,
) -> bool {
    is_dlna
        && reported_duration_ms == 0
        && queue_duration_ms > END_MARGIN_MS
        && wall_elapsed_secs.saturating_mul(1000) >= queue_duration_ms.saturating_add(END_MARGIN_MS)
}

/// Wall-clock end-of-track fallback for a **Chromecast** output.
///
/// Cast tears down its media session the instant a track's byte stream ends
/// and broadcasts the `idle_reason = FINISHED` transition only ONCE. The
/// poller queries with a fresh-connect `GET_STATUS` every ~1 s and never
/// listens for that broadcast, so it routinely misses the FINISHED window
/// and then reads an EMPTY `entries` array — state=Stopped, position=0,
/// `ended_naturally = false` — which the Stopped arm cannot distinguish from
/// a mid-track blip. And if the receiver instead keeps claiming
/// Playing/Buffering with its position frozen a little short of the known
/// duration, the position paths (`reached_end_exclusive` needs position
/// within 250 ms of duration, `past_end_reached` needs it *beyond*) never
/// fire either. The album then stalls after track 1 on Chromecast while a
/// DLNA renderer — which has BOTH this fallback and `poll_failed_past_end` —
/// advances fine (Rhorn, Chromecast Audio, forum #1226; #648/#649 cured the
/// 30-60 s stall but left this never-advances gap).
///
/// Unlike the DLNA LMS-bridge fallback this KEEPS the `played_enough` (peak
/// ≥ 80 %) guard: a Chromecast reports an honest advancing position while it
/// plays, so the peak is trustworthy and gating on it means a genuine
/// mid-track buffering stall (position frozen well before 80 %) can NOT
/// false-advance. Fires only once Tune's own wall clock has passed the
/// queue-known duration + margin, and the caller still requires
/// `POSITION_PAST_END_TICKS` consecutive hits. A well-behaved Chromecast
/// reaches the end via `reached_end_exclusive` a beat earlier, so this only
/// takes over when the device's own end-of-track signal never lands.
pub fn chromecast_wall_clock_past_end(
    output_type: &str,
    played_enough: bool,
    track_duration_ms: u64,
    wall_elapsed_secs: u64,
) -> bool {
    output_type == "chromecast"
        && played_enough
        && track_duration_ms > END_MARGIN_MS
        && wall_elapsed_secs.saturating_mul(1000) >= track_duration_ms.saturating_add(END_MARGIN_MS)
}

/// Fin de piste à l'horloge murale pour un renderer DLNA au poll SAIN qui
/// gèle sa position dans la zone de fin sans jamais la dépasser ni passer
/// STOPPED (Villerio, Eversolo DMP-A6, 25/08 : SetNext acquitté jamais
/// honoré, PLAYING éternel, position figée exactement à la durée).
/// `past_end_reached` exige la position AU-DELÀ de durée+marge,
/// `wall_clock_past_end` exige une durée rapportée nulle, et
/// `duration_changed` attend un changement qui ne vient pas : aucun ne
/// couvre ce gel. Garde-fous : pic ≥ 80 % (le trajet jusqu'à la fin fut
/// honnête), position ÉPINGLÉE en zone de fin (une vraie transition
/// gapless la ramène près de zéro — pas de double-play), et l'horloge de
/// Tune ayant réellement dépassé durée + marge à 1x. L'appelant exige
/// toujours `POSITION_PAST_END_TICKS` coups consécutifs.
pub fn dlna_frozen_at_end_wall_clock(
    is_dlna: bool,
    played_enough: bool,
    track_duration_ms: u64,
    position_ms: u64,
    wall_elapsed_secs: u64,
) -> bool {
    is_dlna
        && played_enough
        && track_duration_ms > END_MARGIN_MS
        && position_ms.saturating_add(2000) >= track_duration_ms
        && wall_elapsed_secs.saturating_mul(1000) >= track_duration_ms.saturating_add(END_MARGIN_MS)
}

/// Wall-clock end-of-track for a DLNA renderer whose status poll is FAILING
/// outright — the LMS UPnP bridge's `GetPositionInfo` SOAP call errors, so
/// `get_status` returns `Err` and Tune gets NO transport state, position, or
/// duration from the renderer at all (Yacine/Jean-Pierre's Denafrips on
/// Daphile: `soap_all_retries_failed action="GetPositionInfo"`).
///
/// The decision is based purely on Tune's OWN wall clock versus the
/// queue-known duration — it never touches renderer-reported values (there
/// are none). Distinguishing "genuinely still playing" from "poll failing
/// but the track really ended":
/// - `tune_playing`: Tune's own intended state is `Playing` (NOT Paused or
///   Stopped). A user pause/stop through Tune flips this false, so a paused
///   track never advances. (`track_started_at` is set when the orchestrator
///   starts the track — generation change — so the wall clock counts real
///   elapsed play time, resetting on each track and on seek.)
/// - `wall_elapsed_secs >= queue_duration + END_MARGIN_MS`: the whole track
///   plus margin has actually elapsed at 1x. Below that, still playing.
/// - `consecutive_errors >= POLL_FAIL_END_MIN_ERRORS`: the poll is really
///   down, not a one-off blip.
/// - `already_fired`: fire at most once per track (the caller sets a per-track
///   latch, cleared on track-generation change).
///
/// A well-behaved DLNA renderer keeps answering `GetPositionInfo`, so its
/// `consecutive_errors` stays 0 and this never triggers — no regression.
pub fn poll_failed_past_end(
    is_dlna: bool,
    tune_playing: bool,
    queue_duration_ms: u64,
    wall_elapsed_secs: u64,
    consecutive_errors: u8,
    already_fired: bool,
) -> bool {
    is_dlna
        && tune_playing
        && !already_fired
        && consecutive_errors >= POLL_FAIL_END_MIN_ERRORS
        && queue_duration_ms > END_MARGIN_MS
        && wall_elapsed_secs.saturating_mul(1000) >= queue_duration_ms.saturating_add(END_MARGIN_MS)
}
