//! REF-9 préparatoire (#2219) — témoins des transitions de `ZonePollState`.
//!
//! Un témoin par transition nommée dans `docs/refonte/ref9-etats-du-sondeur.md`.
//! Chaque témoin construit l'état de sondage AVANT, le passe au classifieur pur
//! de `fsm.rs` (ou au prédicat de `decisions.rs`) exactement comme `tick` le
//! fait, applique les écritures que `tick` fait sur cette issue — recopiées
//! ici avec leur `tick.rs:ligne` — et observe l'état APRÈS.
//!
//! Le miroir des écritures est ancré au texte de `tick.rs` : chaque témoin
//! vérifie que l'écriture qu'il recopie se trouve bien, dans la source, à
//! quelques lignes de la ligne de journal qui nomme la branche. Si `tick`
//! change, le témoin le dit.
//!
//! Aucune ligne de production n'est modifiée par ce module.

use super::fsm::{
    ConsommationFlux, PlayingInput, StoppedInput, StoppedOutcome, classify_playing,
    classify_stopped, consommation_flux,
};
use super::*;

/// La source de `tick`, telle qu'elle est compilée.
const TICK: &str = include_str!("tick.rs");

/// L'écriture doit apparaître dans `tick.rs` au plus `fenetre` lignes APRÈS
/// la première occurrence du marqueur (une ligne de journal, un `fsm_actual`).
fn ecriture_suit_le_marqueur(marqueur: &str, ecriture: &str, fenetre: usize) {
    let debut = TICK
        .find(marqueur)
        .unwrap_or_else(|| panic!("marqueur absent de tick.rs : {marqueur}"));
    let apres: String = TICK[debut..]
        .lines()
        .take(fenetre)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        apres.contains(ecriture),
        "tick.rs : `{ecriture}` n'est plus dans les {fenetre} lignes qui suivent `{marqueur}`"
    );
}

/// Ce que `tick` observe sur un tour, hors de `ZonePollState`.
struct Observation {
    track_duration_ms: u64,
    wall_elapsed: u64,
    ended_naturally: bool,
    in_seek_grace: bool,
    realtime: bool,
    can_internal_gapless: bool,
    consommation: ConsommationFlux,
    dlna_dsd_reached_end: bool,
    repeat_active: bool,
}

fn observation(track_duration_ms: u64, wall_elapsed: u64) -> Observation {
    Observation {
        track_duration_ms,
        wall_elapsed,
        ended_naturally: false,
        in_seek_grace: false,
        realtime: true,
        can_internal_gapless: true,
        consommation: ConsommationFlux::Inconnue,
        dlna_dsd_reached_end: false,
        repeat_active: false,
    }
}

/// Le relevé du bras `Stopped`, pris AVANT mutation — miroir de
/// `tick.rs:1411-1471`.
fn entree_stopped(ps: &ZonePollState, o: &Observation) -> StoppedInput {
    let played_enough =
        decisions::played_enough(o.track_duration_ms, ps.peak_position_ms, o.wall_elapsed);
    let in_track_load_grace = o.realtime
        && ps.track_loaded_at.elapsed().as_secs() < TRACK_LOAD_GRACE_SECS
        && ps.peak_position_ms < 5_000;
    StoppedInput {
        tune_is_playing: true,
        tune_has_track: true,
        in_seek_grace: o.in_seek_grace,
        in_track_load_grace,
        gapless_cooldown: ps.gapless_cooldown,
        in_gapless_guard: ps.gapless_sent_at.is_some(),
        played_enough,
        gapless_advance_pending: ps.gapless_advance_pending,
        gapless_stuck_ticks: ps.gapless_stuck_ticks,
        ended_naturally: o.ended_naturally,
        wall_elapsed: o.wall_elapsed,
        track_duration_ms: o.track_duration_ms,
        stopped_ticks: ps.stopped_ticks,
        natural_end: decisions::natural_end(
            played_enough,
            o.repeat_active,
            ps.peak_position_ms,
            o.ended_naturally,
            o.wall_elapsed,
            o.track_duration_ms,
            o.realtime,
        ),
        gapless_sent: ps.gapless_sent,
        realtime: o.realtime,
        can_internal_gapless: o.can_internal_gapless,
        consommation: o.consommation,
        dlna_dsd_reached_end: o.dlna_dsd_reached_end,
    }
}

/// Les écritures du bras `Stopped` pour chaque issue — miroir de
/// `tick.rs:1473-1848`. Rend `(track_ended, force_stop)`.
fn appliquer_stopped(ps: &mut ZonePollState, issue: StoppedOutcome) -> (bool, bool) {
    use StoppedOutcome::*;
    match issue {
        Ignore => {
            // tick.rs:1396-1397
            ps.stopped_ticks = 0;
            ps.playing_stall_ticks = 0;
        }
        SuppressSeekGrace | SuppressLoadGrace => ps.stopped_ticks = 0, // :1475, :1483
        SuppressCooldown => {
            // tick.rs:1492-1493
            ps.gapless_cooldown -= 1;
            ps.stopped_ticks = 0;
        }
        GuardStoppedIgnored => {} // :1496-1505 : journal seulement
        GuardStoppedPending | NaturalEndGaplessWaiting => {
            // tick.rs:1515-1524 et :1663-1672 (mêmes dix écritures)
            ps.gapless_sent = false;
            ps.gapless_armed = None;
            ps.gapless_sent_at = None;
            ps.stopped_ticks = 0;
            ps.peak_position_ms = 0;
            ps.last_position_ms = 0;
            ps.track_started_at = None;
            ps.gapless_advance_pending = true;
            ps.gapless_stuck_ticks = 0;
            ps.gapless_cooldown = 4;
        }
        StuckWaiting => ps.gapless_stuck_ticks += 1, // :1532
        StuckForceEnd => {
            // tick.rs:1532, :1540-1543
            ps.gapless_stuck_ticks += 1;
            ps.gapless_advance_pending = false;
            ps.gapless_stuck_ticks = 0;
            ps.stopped_ticks = 0;
            return (true, false);
        }
        LocalEndedNaturally => return (true, false), // :1571
        DsdDlnaReachedEnd => {
            // tick.rs:1588-1589
            ps.stopped_ticks = 0;
            return (true, false);
        }
        Waiting => ps.stopped_ticks += 1, // :1595
        NaturalEndAdvance => {
            // tick.rs:1595, :1697-1699
            ps.stopped_ticks += 1;
            ps.gapless_sent = false;
            ps.gapless_armed = None;
            return (true, false);
        }
        FailureWaitingConsuming | FailureWaitingUnknown => ps.stopped_ticks += 1, // :1595
        FailureStop => {
            // tick.rs:1595, :1806-1807
            ps.stopped_ticks += 1;
            return (false, true);
        }
    }
    (false, false)
}

/// Un état de sondage dont la piste a été chargée il y a plus de
/// `TRACK_LOAD_GRACE_SECS` : la grâce de chargement ne masque rien.
fn etat_hors_grace_de_chargement(generation: u64) -> ZonePollState {
    let mut ps = ZonePollState::new(generation);
    ps.track_loaded_at = Instant::now()
        .checked_sub(Duration::from_secs(TRACK_LOAD_GRACE_SECS + 15))
        .unwrap_or_else(Instant::now);
    ps
}

/// Un état ARMÉ : `SetNext` accepté, ligne de file 42 en position 3.
fn etat_arme(peak_position_ms: u64) -> ZonePollState {
    let mut ps = etat_hors_grace_de_chargement(1);
    ps.peak_position_ms = peak_position_ms;
    ps.last_position_ms = peak_position_ms;
    ps.gapless_sent = true;
    ps.gapless_sent_at = Some(Instant::now());
    ps.gapless_armed = Some(ArmedNext {
        row_id: 42,
        position: 3,
    });
    ps
}

/// Les cinq drapeaux, ensemble : c'est leur combinaison que la machine à
/// états devra rendre impossible.
fn drapeaux(ps: &ZonePollState) -> [bool; 5] {
    [
        ps.gapless_sent,
        ps.tenue_signalee,
        ps.gapless_advance_pending,
        ps.depassement_duree_signale,
        ps.wall_clock_end_fired,
    ]
}

// ── T1 · Neuf ──────────────────────────────────────────────────────────

/// T1 — l'état neuf (`ZonePollState::new`, et le bloc de remise à zéro au
/// changement de génération) ne lève aucun drapeau et ne porte aucune
/// option : c'est l'état « Lecture » nu de la proposition.
#[test]
fn t1_etat_neuf_aucun_drapeau_leve() {
    let ps = ZonePollState::new(7);
    assert_eq!(drapeaux(&ps), [false; 5]);
    assert_eq!(ps.track_generation, 7);
    assert!(ps.gapless_sent_at.is_none());
    assert!(ps.gapless_armed.is_none());
    assert!(ps.gapless_dsd_skip_pos.is_none());
    assert!(ps.gapless_arm_logged.is_none());
    assert!(ps.track_started_at.is_none());
    assert!(ps.scrobbled_key.is_none());
    assert_eq!(
        (
            ps.stopped_ticks,
            ps.gapless_cooldown,
            ps.gapless_stuck_ticks,
            ps.past_end_ticks,
            ps.consecutive_errors,
            ps.backoff_remaining,
        ),
        (0, 0, 0, 0, 0, 0)
    );
    // Le bloc de remise à zéro (tick.rs:428-462) rabat les cinq drapeaux.
    let marqueur = "poller_track_generation_changed_resetting_state";
    for ecriture in [
        "ps.gapless_sent = false;",
        "ps.tenue_signalee = false;",
        "ps.gapless_advance_pending = false;",
        "ps.wall_clock_end_fired = false;",
        "ps.depassement_duree_signale = false;",
    ] {
        ecriture_suit_le_marqueur(marqueur, ecriture, 34);
    }
}

// ── T2 · Lecture → Armé ───────────────────────────────────────────────

/// T2 — dans la fenêtre des 30 dernières secondes, non armé : le bras
/// `Playing` décide d'armer ; `prepare_gapless` rend `Armed`, et l'état
/// porte alors les trois champs de l'armement. Il ne ré-arme pas au tour
/// suivant.
#[test]
fn t2_lecture_vers_arme_gapless() {
    let mut ps = etat_hors_grace_de_chargement(1);
    ps.peak_position_ms = 275_000;
    ps.last_position_ms = 275_000;
    let entree = PlayingInput {
        gapless_advance_pending: ps.gapless_advance_pending,
        has_next: true,
        gapless_sent: ps.gapless_sent,
        track_duration_ms: 300_000,
        reported_duration_ms: 300_000,
        played_enough: decisions::played_enough(300_000, ps.peak_position_ms, 275),
        position_ms: 275_000,
        past_end_ticks: ps.past_end_ticks,
        gapless_enabled: true,
        is_dlna: true,
        wall_elapsed_secs: 275,
    };
    let decision = classify_playing(&entree);
    assert!(decision.arm_gapless, "en fenêtre, non armé : on arme");
    assert!(!decision.transition_detected);
    assert!(!decision.past_end_track_ended);

    // tick.rs:2163-2169 — `GaplessPrep::Armed(arme)`.
    let arme = Some(ArmedNext {
        row_id: 42,
        position: 3,
    });
    ps.gapless_sent_at = Some(Instant::now());
    ps.gapless_sent = true;
    ps.gapless_armed = arme;
    assert_eq!(drapeaux(&ps), [true, false, false, false, false]);
    assert_eq!(ps.gapless_armed.map(|a| a.row_id), Some(42));
    ecriture_suit_le_marqueur(
        "GaplessPrep::Armed(arme) => {",
        "ps.gapless_sent = true;",
        4,
    );
    ecriture_suit_le_marqueur(
        "GaplessPrep::Armed(arme) => {",
        "ps.gapless_armed = arme;",
        8,
    );

    // Tour suivant, même fenêtre : l'armement est idempotent.
    let encore = classify_playing(&PlayingInput {
        gapless_sent: ps.gapless_sent,
        position_ms: 276_000,
        ..entree
    });
    assert!(!encore.arm_gapless, "déjà armé : on ne renvoie pas SetNext");
}

// ── T3 · Armé → Lecture (nouvelle piste) ──────────────────────────────

/// T3 — armé, le renderer a enchaîné : durée rapportée différente ET
/// position qui confirme (a). Ou position retombée de >30 s à <5 s (b).
/// Dans les deux cas l'état redevient « Lecture » sur la piste suivante :
/// désarmé, compteurs de piste à zéro, refroidissement de 4 tours.
#[test]
fn t3_arme_vers_lecture_par_transition_detectee() {
    // (a) durée changée + position confirmée — tick.rs:1989-2039
    let mut ps = etat_arme(295_000);
    ps.past_end_ticks = 2;
    let decision = classify_playing(&PlayingInput {
        gapless_advance_pending: false,
        has_next: true,
        gapless_sent: ps.gapless_sent,
        track_duration_ms: 300_000,
        reported_duration_ms: 250_000,
        played_enough: decisions::played_enough(300_000, ps.peak_position_ms, 295),
        position_ms: 2_000,
        past_end_ticks: ps.past_end_ticks,
        gapless_enabled: true,
        is_dlna: true,
        wall_elapsed_secs: 295,
    });
    assert!(decision.transition_detected);
    assert!(
        !decision.arm_gapless,
        "on n'arme pas dans le tour qui transitionne"
    );
    assert!(
        !decision.past_end_track_ended,
        "la transition remet past_end_ticks à zéro avant le détecteur de dépassement"
    );
    ps.gapless_sent = false;
    ps.gapless_sent_at = None;
    let arme_avant = ps.gapless_armed.take();
    ps.peak_position_ms = 0;
    ps.last_position_ms = 0;
    ps.last_bytes_sent = 0;
    ps.playing_stall_ticks = 0;
    ps.stall_declines = 0;
    ps.track_started_at = Some(Instant::now());
    ps.stopped_ticks = 0;
    ps.past_end_ticks = 0;
    ps.gapless_advance_pending = false;
    ps.gapless_stuck_ticks = 0;
    ps.gapless_arm_logged = None;
    ps.gapless_dsd_skip_pos = None;
    ps.gapless_cooldown = 4;
    ps.scrobbled_key = None;
    assert_eq!(
        arme_avant.map(|a| a.row_id),
        Some(42),
        "la ligne armée sert à avancer"
    );
    assert_eq!(drapeaux(&ps), [false; 5]);
    assert!(ps.gapless_armed.is_none());
    assert_eq!(
        (ps.peak_position_ms, ps.past_end_ticks, ps.gapless_cooldown),
        (0, 0, 4)
    );
    ecriture_suit_le_marqueur(
        "\"gapless_transition_detected\"",
        "ps.gapless_sent = false;",
        4,
    );
    ecriture_suit_le_marqueur(
        "\"gapless_transition_detected\"",
        "ps.gapless_cooldown = 4;",
        40,
    );

    // (b) remise à zéro de position — tick.rs:1252-1322
    let ps2 = etat_arme(280_000);
    let brut = decisions::position_reset(ps2.last_position_ms, 1_000, ps2.gapless_sent);
    assert!(brut, ">30 s puis <5 s, armé : remise à zéro");
    assert!(decisions::position_reset_fires(brut, true, false));
    assert!(
        !decisions::position_reset_fires(brut, false, false),
        "sortie sans enchaînement interne : la chute à 0 est une FIN, pas une avance"
    );
    assert!(
        !decisions::position_reset_fires(brut, true, true),
        "en grâce de déplacement : rien"
    );
    assert!(
        !decisions::position_reset(ps2.last_position_ms, 1_000, false),
        "non armé : une chute de position n'est pas une transition"
    );
    ecriture_suit_le_marqueur(
        "\"gapless_position_reset_detected\"",
        "ps.gapless_sent = false;",
        4,
    );
    ecriture_suit_le_marqueur(
        "\"gapless_position_reset_detected\"",
        "ps.gapless_advance_pending = false;",
        20,
    );
}

// ── T4 · Armé → AvancePendante ────────────────────────────────────────

/// T4 — armé, dans la garde des 15 s, le renderer dit `Stopped` après avoir
/// assez joué : on n'avance pas encore, on attend qu'il rejoue. L'état
/// bascule en « AvancePendante » : désarmé, `gapless_advance_pending` levé,
/// horloge de piste effacée, refroidissement de 4 tours. Pas assez joué :
/// rien ne bouge.
#[test]
fn t4_arme_et_stopped_dans_la_garde_vers_avance_pendante() {
    let mut ps = etat_arme(290_000);
    let o = observation(300_000, 290);
    let entree = entree_stopped(&ps, &o);
    assert!(entree.in_gapless_guard && entree.played_enough);
    let issue = classify_stopped(&entree);
    assert_eq!(issue, StoppedOutcome::GuardStoppedPending);
    assert!(!issue.is_track_end() && !issue.is_force_stop());

    let (fin, arret) = appliquer_stopped(&mut ps, issue);
    assert_eq!((fin, arret), (false, false));
    assert!(
        ps.gapless_advance_pending,
        "l'avance est en attente de confirmation"
    );
    assert!(!ps.gapless_sent && ps.gapless_sent_at.is_none() && ps.gapless_armed.is_none());
    assert!(
        ps.track_started_at.is_none(),
        "l'horloge de piste est effacée"
    );
    assert_eq!(
        (
            ps.gapless_cooldown,
            ps.gapless_stuck_ticks,
            ps.peak_position_ms
        ),
        (4, 0, 0)
    );
    assert_eq!(drapeaux(&ps), [false, false, true, false, false]);
    ecriture_suit_le_marqueur(
        "\"gapless_guard_stopped_pending_confirmation\"",
        "ps.gapless_advance_pending = true;",
        10,
    );

    // Contre-cas : pas assez joué → ignoré, l'état ARMÉ reste tel quel.
    let mut peu = etat_arme(100_000);
    let issue_peu = classify_stopped(&entree_stopped(&peu, &observation(300_000, 100)));
    assert_eq!(issue_peu, StoppedOutcome::GuardStoppedIgnored);
    appliquer_stopped(&mut peu, issue_peu);
    assert!(peu.gapless_sent && peu.gapless_armed.is_some() && !peu.gapless_advance_pending);
}

// ── T5 · AvancePendante → Lecture ─────────────────────────────────────

/// T5 — avance pendante, le renderer repasse `Playing` : la transition est
/// confirmée, les métadonnées avancent, l'état redevient « Lecture ».
#[test]
fn t5_avance_pendante_et_playing_vers_lecture() {
    let mut ps = etat_hors_grace_de_chargement(1);
    ps.gapless_advance_pending = true;
    ps.gapless_stuck_ticks = 1;
    ps.scrobbled_key = Some("gen1:pos2".into());
    let decision = classify_playing(&PlayingInput {
        gapless_advance_pending: ps.gapless_advance_pending,
        has_next: true,
        gapless_sent: false,
        track_duration_ms: 300_000,
        reported_duration_ms: 300_000,
        played_enough: false,
        position_ms: 500,
        past_end_ticks: 0,
        gapless_enabled: true,
        is_dlna: true,
        wall_elapsed_secs: 0,
    });
    assert!(decision.confirm_gapless_advance);
    // tick.rs:1899-1918
    ps.gapless_advance_pending = false;
    ps.gapless_stuck_ticks = 0;
    ps.gapless_cooldown = 4;
    ps.scrobbled_key = None;
    if ps.track_started_at.is_none() {
        ps.track_started_at = Some(Instant::now());
    }
    assert_eq!(drapeaux(&ps), [false; 5]);
    assert!(
        ps.track_started_at.is_some(),
        "l'horloge de piste repart au Playing"
    );
    assert!(
        ps.scrobbled_key.is_none(),
        "le verrou de scrobble se ré-arme (#1113)"
    );
    ecriture_suit_le_marqueur(
        "\"gapless_confirmed_advancing_metadata\"",
        "ps.gapless_cooldown = 4;",
        10,
    );

    // Sans piste suivante : le drapeau tombe quand même, sans avance.
    assert!(
        !classify_playing(&PlayingInput {
            has_next: false,
            gapless_advance_pending: true,
            gapless_sent: false,
            track_duration_ms: 300_000,
            reported_duration_ms: 300_000,
            played_enough: false,
            position_ms: 500,
            past_end_ticks: 0,
            gapless_enabled: true,
            is_dlna: true,
            wall_elapsed_secs: 0,
        })
        .confirm_gapless_advance
    );
}

// ── T6 · AvancePendante → Fin (bloquée) ───────────────────────────────

/// T6 — avance pendante, le renderer reste `Stopped` : 4 tours de
/// refroidissement, puis `GAPLESS_STUCK_THRESHOLD` tours bloqués, et la
/// piste est déclarée finie de force (`play_from_queue` par
/// `handle_track_end`). Six tours en tout, au tick près.
#[test]
fn t6_avance_pendante_bloquee_vers_fin_de_piste() {
    let mut ps = etat_arme(290_000);
    let o = observation(300_000, 290);
    let entree_en_garde = classify_stopped(&entree_stopped(&ps, &o));
    appliquer_stopped(&mut ps, entree_en_garde);
    assert!(ps.gapless_advance_pending && ps.gapless_cooldown == 4);

    let mut trace = Vec::new();
    let mut verdict = (false, false);
    for _ in 0..6 {
        let issue = classify_stopped(&entree_stopped(&ps, &o));
        trace.push(issue);
        verdict = appliquer_stopped(&mut ps, issue);
        if verdict.0 {
            break;
        }
    }
    use StoppedOutcome::*;
    assert_eq!(
        trace,
        [
            SuppressCooldown,
            SuppressCooldown,
            SuppressCooldown,
            SuppressCooldown,
            StuckWaiting,
            StuckForceEnd
        ]
    );
    assert_eq!(
        verdict,
        (true, false),
        "fin de piste forcée, pas d'arrêt de zone"
    );
    assert!(
        !ps.gapless_advance_pending,
        "le drapeau tombe avec la fin forcée"
    );
    assert_eq!(ps.gapless_stuck_ticks, 0);
    assert_eq!(drapeaux(&ps), [false; 5]);
    assert_eq!(GAPLESS_STUCK_THRESHOLD, 2, "le seuil que ce témoin compte");
    ecriture_suit_le_marqueur(
        "\"gapless_advance_stuck_forcing_play\"",
        "ps.gapless_advance_pending = false;",
        4,
    );
    ecriture_suit_le_marqueur(
        "\"gapless_advance_stuck_forcing_play\"",
        "track_ended = true;",
        8,
    );
}

// ── T7 · Lecture → Fin (fin naturelle après Stopped) ──────────────────

/// T7 — non armé, assez joué, le renderer dit `Stopped` : quatre tours
/// d'attente, puis au cinquième la fin naturelle avance la file. Armé sur
/// une sortie qui enchaîne seule, la même situation bascule en
/// « AvancePendante » au lieu de finir.
#[test]
fn t7_lecture_arretee_a_la_fin_naturelle_vers_fin_de_piste() {
    let mut ps = etat_hors_grace_de_chargement(1);
    ps.peak_position_ms = 290_000;
    let o = observation(300_000, 290);
    let mut issues = Vec::new();
    let mut verdict = (false, false);
    for _ in 0..STOPPED_TICKS_THRESHOLD {
        let issue = classify_stopped(&entree_stopped(&ps, &o));
        issues.push(issue);
        verdict = appliquer_stopped(&mut ps, issue);
    }
    assert_eq!(
        issues,
        [
            StoppedOutcome::Waiting,
            StoppedOutcome::Waiting,
            StoppedOutcome::Waiting,
            StoppedOutcome::Waiting,
            StoppedOutcome::NaturalEndAdvance
        ]
    );
    assert_eq!(verdict, (true, false));
    assert_eq!(ps.stopped_ticks, STOPPED_TICKS_THRESHOLD);
    assert!(!ps.gapless_sent && ps.gapless_armed.is_none());
    // Le renderer a-t-il reçu le morceau ? Sans total connu, on lui fait
    // confiance (tick.rs:1683-1695).
    assert!(decisions::renderer_could_have_finished(0, None, false));
    assert!(!decisions::renderer_could_have_finished(
        10,
        Some(100),
        false
    ));
    assert_eq!(
        decisions::plancher_de_detection_ms(decisions::motif_fin::FIN_NATURELLE_APRES_STOPPED),
        STOPPED_TICKS_THRESHOLD as u64 * POLL_INTERVAL_MS
    );
    ecriture_suit_le_marqueur(
        "fsm_actual = Some(fsm::StoppedOutcome::NaturalEndAdvance);",
        "ps.gapless_sent = false;",
        3,
    );

    // Armé + enchaînement interne possible : on attend le renderer.
    let mut arme = etat_arme(290_000);
    arme.gapless_sent_at = None; // garde de 15 s expirée (tick.rs:1365-1370)
    arme.stopped_ticks = STOPPED_TICKS_THRESHOLD - 1;
    let issue = classify_stopped(&entree_stopped(&arme, &o));
    assert_eq!(issue, StoppedOutcome::NaturalEndGaplessWaiting);
    appliquer_stopped(&mut arme, issue);
    assert!(arme.gapless_advance_pending && !arme.gapless_sent);
    ecriture_suit_le_marqueur(
        "\"gapless_natural_end_waiting_for_transition\"",
        "ps.gapless_advance_pending = true;",
        10,
    );
}

// ── T8 · Lecture → ArrêtForcé (échec) ─────────────────────────────────

/// T8 — `Stopped` sans fin naturelle pendant `STOPPED_FAILURE_THRESHOLD`
/// tours : la zone est coupée SEULEMENT si le compteur d'octets est mesuré
/// et n'avance pas. Consommé ou inconnu : on attend encore (#2394).
#[test]
fn t8_lecture_arretee_sans_fin_vers_arret_force() {
    let mut ps = etat_hors_grace_de_chargement(1);
    ps.peak_position_ms = 10_000;
    ps.last_bytes_sent = 5_000;
    ps.stopped_ticks = STOPPED_FAILURE_THRESHOLD - 1;
    let mut o = observation(300_000, 40);
    let entree = entree_stopped(&ps, &o);
    assert!(!entree.natural_end && !entree.played_enough);

    // Ce que la sonde mesure — tick.rs:1748-1761.
    assert_eq!(
        consommation_flux(None, ps.last_bytes_sent),
        ConsommationFlux::Inconnue
    );
    assert_eq!(
        consommation_flux(Some(5_000), ps.last_bytes_sent),
        ConsommationFlux::ASec
    );
    assert_eq!(
        consommation_flux(Some(6_000), ps.last_bytes_sent),
        ConsommationFlux::Consomme
    );

    o.consommation = ConsommationFlux::ASec;
    let issue = classify_stopped(&entree_stopped(&ps, &o));
    assert_eq!(issue, StoppedOutcome::FailureStop);
    assert!(issue.is_force_stop() && !issue.is_track_end());
    let mut coupe = etat_hors_grace_de_chargement(1);
    coupe.peak_position_ms = 10_000;
    coupe.stopped_ticks = STOPPED_FAILURE_THRESHOLD - 1;
    assert_eq!(appliquer_stopped(&mut coupe, issue), (false, true));
    assert_eq!(coupe.stopped_ticks, STOPPED_FAILURE_THRESHOLD);
    assert!(
        decisions::demarrage_mort("dlna", 0),
        "0 octet sur DLNA : relance Pause→Stop→Play"
    );
    assert!(!decisions::demarrage_mort("dlna", 5_000));
    ecriture_suit_le_marqueur(
        "\"playback_failure_stopping_zone\"",
        "force_stop = true;",
        6,
    );

    o.consommation = ConsommationFlux::Consomme;
    let issue = classify_stopped(&entree_stopped(&ps, &o));
    assert_eq!(issue, StoppedOutcome::FailureWaitingConsuming);
    assert!(!issue.is_force_stop());

    o.consommation = ConsommationFlux::Inconnue;
    let issue = classify_stopped(&entree_stopped(&ps, &o));
    assert_eq!(issue, StoppedOutcome::FailureWaitingUnknown);
    assert!(
        !issue.is_force_stop(),
        "on ne coupe pas ce qu'on ne mesure pas"
    );
}

// ── T9 · Lecture → Fin (position au-delà de la fin) ───────────────────

/// T9 — le renderer dit toujours `Playing` mais la position a dépassé la
/// durée de plus de 3 s : au troisième tour consécutif, la piste est finie.
/// Un tour où la position revient sous la fin remet le compteur à zéro.
#[test]
fn t9_lecture_au_dela_de_la_fin_vers_fin_de_piste() {
    let mut ps = etat_hors_grace_de_chargement(1);
    ps.peak_position_ms = 240_000;
    let entree = |ps: &ZonePollState, position_ms: u64| PlayingInput {
        gapless_advance_pending: false,
        has_next: true,
        gapless_sent: false,
        track_duration_ms: 240_000,
        reported_duration_ms: 240_000,
        played_enough: decisions::played_enough(240_000, ps.peak_position_ms, 240),
        position_ms,
        past_end_ticks: ps.past_end_ticks,
        gapless_enabled: true,
        is_dlna: false,
        wall_elapsed_secs: 240,
    };
    let mut fins = Vec::new();
    for _ in 0..POSITION_PAST_END_TICKS {
        let d = classify_playing(&entree(&ps, 244_000));
        fins.push(d.past_end_track_ended);
        // tick.rs:2288
        ps.past_end_ticks += 1;
    }
    assert_eq!(fins, [false, false, true]);
    assert_eq!(ps.past_end_ticks, POSITION_PAST_END_TICKS);
    // Le plancher payé : la marge de 3 s AVANT que la position ne compte
    // comme dépassée, puis les trois ticks.
    assert_eq!(
        decisions::plancher_de_detection_ms(decisions::motif_fin::POSITION_AU_DELA_DE_LA_FIN),
        decisions::END_MARGIN_MS + POSITION_PAST_END_TICKS as u64 * POLL_INTERVAL_MS
    );
    ecriture_suit_le_marqueur("\"position_past_end_advancing\"", "track_ended = true;", 4);

    // La position revient avant la fin : tick.rs:2307.
    let mut retour = etat_hors_grace_de_chargement(1);
    retour.peak_position_ms = 240_000;
    retour.past_end_ticks = 2;
    assert!(!classify_playing(&entree(&retour, 200_000)).past_end_track_ended);
    retour.past_end_ticks = 0;
    assert_eq!(retour.past_end_ticks, 0);
    ecriture_suit_le_marqueur(
        "\"position_past_end_advancing\"",
        "ps.past_end_ticks = 0;",
        20,
    );
}

// ── T10 · Armé → Lecture (désarmement sans transition) ────────────────

/// T10 — l'armement se défait sans que rien n'ait joué : (a) la préparation
/// a plus de `GAPLESS_STAGE_MAX_AGE_SECS` (le flux est mort côté serveur),
/// (b) la file a changé sous l'armement (« Lire ensuite », #3026). Dans les
/// deux cas l'état revient à « Lecture » non armé, et le même tour ré-arme.
#[test]
fn t10_arme_vers_lecture_par_desarmement_sans_transition() {
    // (a) tick.rs:2048-2060
    let mut ps = etat_arme(280_000);
    assert!(decisions::gapless_stage_expired(
        ps.gapless_sent,
        Some(GAPLESS_STAGE_MAX_AGE_SECS + 1)
    ));
    assert!(!decisions::gapless_stage_expired(ps.gapless_sent, Some(30)));
    ps.gapless_sent = false;
    ps.gapless_sent_at = None;
    ps.gapless_armed = None;
    assert_eq!(drapeaux(&ps), [false; 5]);
    ecriture_suit_le_marqueur(
        "\"gapless_stage_expired_rearming\"",
        "ps.gapless_armed = None;",
        8,
    );

    // (b) tick.rs:2077-2108
    let mut ps = etat_arme(280_000);
    ps.gapless_arm_logged = Some(true);
    assert!(decisions::gapless_arm_outdated(
        ps.gapless_armed.map(|a| a.row_id),
        Some(43)
    ));
    assert!(
        !decisions::gapless_arm_outdated(ps.gapless_armed.map(|a| a.row_id), Some(42)),
        "même ligne : rien n'est désarmé"
    );
    assert!(
        !decisions::gapless_arm_outdated(None, Some(43)),
        "armé sans ligne connue (sortie exclusive) : rien à comparer"
    );
    ps.gapless_sent = false;
    ps.gapless_sent_at = None;
    ps.gapless_armed = None;
    ps.gapless_arm_logged = None;
    assert!(
        ps.gapless_arm_logged.is_none(),
        "une trace neuve pour le nouvel armement"
    );
    ecriture_suit_le_marqueur(
        "\"gapless_rearm_queue_changed\"",
        "ps.gapless_arm_logged = None;",
        16,
    );

    // … et le même tour ré-arme : tick.rs:2110-2115.
    assert!(decisions::should_arm_gapless(
        ps.gapless_sent,
        300_000,
        300_000,
        280_000
    ));
}

// ── T11 · SondageEnEchec → Fin (horloge murale) ───────────────────────

/// T11 — la sonde elle-même échoue (pont LMS UPnP) : au deuxième échec
/// consécutif, une fois la durée écoulée à l'horloge murale, la fin est
/// prononcée UNE fois par piste (`wall_clock_end_fired`). Le recul double
/// à chaque échec, plafonné à 16 tours.
#[test]
fn t11_sondage_en_echec_vers_fin_par_horloge_murale() {
    let mut ps = etat_hors_grace_de_chargement(1);
    // tick.rs:651-653, deux échecs.
    for _ in 0..2 {
        ps.consecutive_errors = ps.consecutive_errors.saturating_add(1);
        ps.total_errors += 1;
        ps.backoff_remaining = 1u8 << ps.consecutive_errors.min(4);
    }
    assert_eq!((ps.consecutive_errors, ps.backoff_remaining), (2, 4));
    assert!(decisions::poll_failed_past_end(
        true,
        true,
        300_000,
        304,
        ps.consecutive_errors,
        ps.wall_clock_end_fired
    ));
    // tick.rs:712
    ps.wall_clock_end_fired = true;
    assert_eq!(drapeaux(&ps), [false, false, false, false, true]);
    assert!(
        !decisions::poll_failed_past_end(true, true, 300_000, 400, 5, ps.wall_clock_end_fired),
        "le verrou par piste empêche de tirer deux fois"
    );
    assert!(
        !decisions::poll_failed_past_end(false, true, 300_000, 304, 2, false),
        "hors DLNA : pas de fin par horloge murale sur échec de sonde"
    );
    ecriture_suit_le_marqueur(
        "\"dlna_poll_failed_wall_clock_advancing\"",
        "ps.wall_clock_end_fired = true;",
        4,
    );

    // Le plafond du recul : 5 échecs → 16 tours, 9 échecs → 16 tours encore.
    for _ in 0..7 {
        ps.consecutive_errors = ps.consecutive_errors.saturating_add(1);
        ps.backoff_remaining = 1u8 << ps.consecutive_errors.min(4);
    }
    assert_eq!((ps.consecutive_errors, ps.backoff_remaining), (9, 16));
    // Un succès efface le compte (tick.rs:629), pas le total.
    ps.consecutive_errors = 0;
    assert_eq!((ps.consecutive_errors, ps.total_errors), (0, 2));
}

// ═══════════════════════════════════════════════════════════════════════
// REF-9, nuit du 12/09 — l'énumération en ombre et l'invariant.
//
// Un témoin par transition instrumentée dans `tick.rs` (E1-E22, la
// numérotation suit `fsm::Transition`). Chacun construit l'état, applique la
// décision et le miroir des écritures comme T1-T11, puis :
//   1. montre ce que l'invariant dirait SANS l'appel de transition (les
//      drapeaux ont bougé, l'état non) ;
//   2. applique la transition et vérifie `coherent()` ;
//   3. ancre l'appel `ps.transition(…)` au texte de `tick.rs`, à quelques
//      lignes de l'écriture qu'il suit — avec, dans le message, le nom de
//      l'incohérence que l'invariant rendrait sans lui.
// Retirer UN appel de `tick.rs` fait rougir le témoin de cette transition
// (contre-épreuve du 12/09, collée dans la PR) — et, pour les transitions
// que le banc `lire_ensuite_dans_la_fenetre_gapless` traverse, le
// `debug_assert!` de fin de tour de `tick` lui-même.
// ═══════════════════════════════════════════════════════════════════════

use super::etat::{Armement, CauseDeCoupure, Depuis, EtatDeLecture, Incoherence, Issue, MotifFin};
use super::fsm::{Transition, armement_accepte, suivant};

/// L'appel de transition doit suivre le marqueur dans `tick.rs`. Le message
/// nomme l'incohérence que `coherent()` rendrait si l'appel manquait — ou
/// dit qu'aucun drapeau ne la trahirait (seul l'ancrage garde alors l'appel).
fn appel_suit_le_marqueur(
    marqueur: &str,
    appel: &str,
    fenetre: usize,
    sans_lui: Option<&Incoherence>,
) {
    let debut = TICK
        .find(marqueur)
        .unwrap_or_else(|| panic!("marqueur absent de tick.rs : {marqueur}"));
    let apres: String = TICK[debut..]
        .lines()
        .take(fenetre)
        .collect::<Vec<_>>()
        .join("\n");
    let consequence = match sans_lui {
        Some(inc) => format!("sans cet appel, l'invariant rend : {inc}"),
        None => {
            "aucun drapeau ne distingue les deux états : seul cet ancrage garde l'appel".to_string()
        }
    };
    assert!(
        apres.contains(appel),
        "tick.rs : `{appel}` n'est plus dans les {fenetre} lignes qui suivent `{marqueur}` — {consequence}"
    );
}

/// L'incohérence attendue quand les drapeaux ont bougé et pas l'état.
fn drapeau(etat: &'static str, drapeau: &'static str, attendu: &str, lu: &str) -> Incoherence {
    Incoherence::Drapeau {
        etat,
        drapeau,
        attendu: attendu.to_string(),
        lu: lu.to_string(),
    }
}

const LIGNE_42: Option<ArmedNext> = Some(ArmedNext {
    row_id: 42,
    position: 3,
});

/// `etat_arme`, l'ombre posée avec : c'est l'état ARMÉ cohérent.
fn etat_arme_en_ombre(peak_position_ms: u64) -> ZonePollState {
    let mut ps = etat_arme(peak_position_ms);
    ps.etat = EtatDeLecture::Armee {
        armement: Armement::Accepte { ligne: LIGNE_42 },
    };
    ps.coherent().expect("l'état armé du banc est cohérent");
    ps
}

/// Un état en LECTURE, hors grâce de chargement.
fn etat_en_lecture(peak_position_ms: u64) -> ZonePollState {
    let mut ps = etat_hors_grace_de_chargement(1);
    ps.peak_position_ms = peak_position_ms;
    ps.last_position_ms = peak_position_ms;
    ps.etat = EtatDeLecture::Lecture;
    ps.coherent()
        .expect("l'état en lecture du banc est cohérent");
    ps
}

/// Un état ARRÊTÉ depuis la lecture : `n` ticks `Stopped` comptés.
fn etat_arrete(peak_position_ms: u64, n: u8) -> ZonePollState {
    let mut ps = etat_en_lecture(peak_position_ms);
    ps.stopped_ticks = n;
    ps.etat = EtatDeLecture::Arretee {
        depuis: Depuis::Lecture,
    };
    ps.coherent().expect("l'état arrêté du banc est cohérent");
    ps
}

/// Un état en AVANCE PENDANTE, tel que `ArretDansLaGarde` le laisse.
fn etat_avance_pendante() -> ZonePollState {
    let mut ps = etat_arme_en_ombre(290_000);
    let issue = classify_stopped(&entree_stopped(&ps, &observation(300_000, 290)));
    assert_eq!(issue, StoppedOutcome::GuardStoppedPending);
    appliquer_stopped(&mut ps, issue);
    ps.transition(Transition::ArretDansLaGarde);
    ps.coherent()
        .expect("l'avance pendante du banc est cohérente");
    ps
}

// ── E0 · la table elle-même ───────────────────────────────────────────

const TOUTES: [Transition; 22] = [
    Transition::NouvellePiste,
    Transition::PremierEchantillonPlausible,
    Transition::Armement {
        armement: Armement::Accepte { ligne: LIGNE_42 },
    },
    Transition::TransitionDetectee,
    Transition::Desarmement,
    Transition::ArretDansLaGarde,
    Transition::RendererArrete,
    Transition::ArretEfface,
    Transition::FinNaturelleEnAttenteDEnchainement,
    Transition::FinNaturelleApresArret,
    Transition::PanneDeLecture {
        cause: CauseDeCoupure::FluxASec,
    },
    Transition::AttenteProlongee,
    Transition::EnchainementConfirme,
    Transition::EnchainementBloque,
    Transition::PositionAuDelaDeLaFin,
    Transition::FinConstateeAvantLeSeuil {
        motif: MotifFin::FinNaturelleLocale,
    },
    Transition::LectureSansProgres,
    Transition::SondeEnEchec,
    Transition::FinParHorlogeMurale,
    Transition::SondeRetablie,
    Transition::SourceRadio,
    Transition::RadioAbandonnee,
];

fn toutes_les_formes() -> Vec<EtatDeLecture> {
    use EtatDeLecture::*;
    vec![
        Neuve,
        Lecture,
        Armee {
            armement: Armement::Accepte { ligne: LIGNE_42 },
        },
        Armee {
            armement: Armement::Renonce,
        },
        Arretee {
            depuis: Depuis::Lecture,
        },
        Arretee {
            depuis: Depuis::Armee(Armement::Accepte { ligne: LIGNE_42 }),
        },
        AvancePendante,
        Radio,
        SondageEnEchec {
            precedent: Box::new(Lecture),
        },
        Terminee(Issue::Finie(MotifFin::HorlogeMuraleSurSondeEnEchec)),
        Terminee(Issue::Coupee(CauseDeCoupure::FluxASec)),
    ]
}

/// E0 — 22 transitions, 8 variantes ; chaque transition a au moins un état
/// de départ prévu, chaque état de départ inattendu est une `Incoherence`
/// nommée (jamais un silence), et `NouvellePiste` ramène de partout à
/// `Neuve`. Sur `Terminee`, seules la sonde et la nouvelle piste sont
/// admises : un état terminal ne « joue » plus.
#[test]
fn e0_la_table_n_a_pas_de_bras_muet() {
    let formes = toutes_les_formes();
    assert_eq!(
        formes
            .iter()
            .map(|e| e.nom())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        8
    );
    for t in TOUTES {
        let mut prevues = 0;
        for depart in &formes {
            match suivant(depart, t) {
                Ok(_) => prevues += 1,
                Err(Incoherence::TransitionInattendue { etat, transition }) => {
                    assert!(etat.starts_with(depart.nom()), "{etat} / {}", depart.nom());
                    assert!(transition.contains(&format!("{t:?}")[..4]));
                }
                Err(autre) => panic!("la table ne rend que TransitionInattendue : {autre}"),
            }
        }
        assert!(prevues >= 1, "{t:?} n'a aucun état de départ prévu");
        assert_eq!(
            suivant(&EtatDeLecture::Neuve, Transition::NouvellePiste),
            Ok(EtatDeLecture::Neuve)
        );
    }
    for depart in &formes {
        assert_eq!(
            suivant(depart, Transition::NouvellePiste),
            Ok(EtatDeLecture::Neuve)
        );
        if let EtatDeLecture::Terminee(_) = depart {
            for t in TOUTES {
                let admise = matches!(
                    t,
                    Transition::NouvellePiste
                        | Transition::SondeEnEchec
                        | Transition::SondeRetablie
                );
                assert_eq!(suivant(depart, t).is_ok(), admise, "{depart:?} + {t:?}");
            }
        }
    }
    // Une transition inattendue est RAPPORTÉE et ne change rien.
    let mut ps = etat_en_lecture(10_000);
    ps.transition(Transition::EnchainementConfirme);
    assert_eq!(ps.etat, EtatDeLecture::Lecture);
    ps.coherent().unwrap();
}

/// E0 bis — l'invariant lui-même : chaque forme cohérente le passe, et
/// chaque drapeau qu'on fausse est nommé avec l'état.
#[test]
fn e0_l_invariant_nomme_l_etat_et_le_drapeau() {
    let mut ps = ZonePollState::new(1);
    ps.coherent().unwrap();
    ps.gapless_sent = true;
    assert_eq!(
        ps.coherent(),
        Err(drapeau("Neuve", "gapless_sent", "false", "true"))
    );
    ps.gapless_sent = false;
    ps.stopped_ticks = 2;
    assert_eq!(
        ps.coherent(),
        Err(drapeau("Neuve", "stopped_ticks", "0", "2"))
    );
    ps.stopped_ticks = 0;
    ps.wall_clock_end_fired = true;
    assert_eq!(
        ps.coherent(),
        Err(drapeau("Neuve", "wall_clock_end_fired", "false", "true"))
    );
    ps.wall_clock_end_fired = false;

    let mut arme = etat_arme_en_ombre(280_000);
    arme.gapless_armed = Some(ArmedNext {
        row_id: 43,
        position: 1,
    });
    assert!(matches!(
        arme.coherent(),
        Err(Incoherence::Drapeau {
            etat: "Armee",
            drapeau: "gapless_armed",
            ..
        })
    ));

    let mut pendante = etat_avance_pendante();
    pendante.gapless_advance_pending = false;
    assert_eq!(
        pendante.coherent(),
        Err(drapeau(
            "AvancePendante",
            "gapless_advance_pending",
            "true",
            "false"
        ))
    );

    // En panne de sonde, l'état d'avant doit encore décrire les drapeaux.
    let mut panne = etat_en_lecture(10_000);
    panne.consecutive_errors = 1;
    panne.transition(Transition::SondeEnEchec);
    panne.coherent().unwrap();
    panne.gapless_advance_pending = true;
    assert_eq!(
        panne.coherent(),
        Err(drapeau(
            "Lecture",
            "gapless_advance_pending",
            "false",
            "true"
        ))
    );

    // Le site unique de l'invariant dans `tick`, à la fin, sous debug.
    let site = TICK
        .find("poller_etat_incoherent zone_id=")
        .expect("le debug_assert! de fin de tour");
    assert_eq!(
        TICK.matches("poller_etat_incoherent").count(),
        1,
        "un seul site"
    );
    assert!(
        TICK.rfind("ps.transition(").unwrap() < site,
        "après le dernier appel de transition"
    );
    assert!(
        TICK[..site]
            .rfind("#[cfg(debug_assertions)]")
            .is_some_and(|c| site - c < 400)
    );
}

// ── E1 · → Neuve ──────────────────────────────────────────────────────

/// E1 — génération changée : depuis n'importe quel état, tout est rabattu
/// et l'état est `Neuve`. Sans l'appel, un état armé resterait dit armé sur
/// des drapeaux vierges.
#[test]
fn e1_nouvelle_piste_ramene_a_neuve() {
    let mut ps = etat_arme_en_ombre(280_000);
    // tick.rs:446-465
    ps.gapless_sent = false;
    ps.gapless_sent_at = None;
    ps.gapless_cooldown = 0;
    ps.stopped_ticks = 0;
    ps.track_generation = 2;
    ps.tenue_signalee = false;
    ps.gapless_advance_pending = false;
    ps.wall_clock_end_fired = false;
    ps.gapless_armed = None;
    let sans = ps.coherent().unwrap_err();
    assert_eq!(sans, drapeau("Armee", "gapless_sent", "true", "false"));
    ps.transition(Transition::NouvellePiste);
    assert_eq!(ps.etat, EtatDeLecture::Neuve);
    ps.coherent().unwrap();
    assert_eq!(
        ZonePollState::new(9).etat,
        EtatDeLecture::Neuve,
        "l'état neuf naît Neuve"
    );
    appel_suit_le_marqueur(
        "poller_track_generation_changed_resetting_state",
        "ps.transition(fsm::Transition::NouvellePiste);",
        34,
        Some(&sans),
    );
}

// ── E2 · Neuve → Lecture ──────────────────────────────────────────────

/// E2 — le premier échantillon plausible fait entrer en lecture ; un
/// échantillon périmé (`stale_start_position`) laisse `Neuve`. Aucun
/// drapeau ne sépare les deux états : l'ancrage seul garde l'appel.
#[test]
fn e2_premier_echantillon_plausible_neuve_vers_lecture() {
    let mut ps = ZonePollState::new(1);
    ps.track_started_at = Some(Instant::now());
    assert!(
        decisions::stale_start_position(0, 200_000),
        "position d'avant, horloge à zéro : périmé"
    );
    assert!(
        !decisions::stale_start_position(35, 20_000),
        "20 s de position pour 35 s d'horloge : plausible"
    );
    ps.coherent().unwrap();
    ps.transition(Transition::PremierEchantillonPlausible);
    assert_eq!(ps.etat, EtatDeLecture::Lecture);
    ps.coherent().unwrap();
    // Identité ensuite : chaque tour repasse par là.
    ps.transition(Transition::PremierEchantillonPlausible);
    assert_eq!(ps.etat, EtatDeLecture::Lecture);
    appel_suit_le_marqueur(
        "\"stale_start_position_ignored\"",
        "ps.transition(fsm::Transition::PremierEchantillonPlausible);",
        6,
        None,
    );
}

// ── E3 · Lecture → Armee ──────────────────────────────────────────────

/// E3 — `SetNext` accepté : `Armee { Accepte { ligne } }`, la ligne égale
/// à `gapless_armed`. Sortie exclusive : `Armee { Renonce }`, rien n'est
/// parti (décision du 12/09 : une variante distincte).
#[test]
fn e3_armement_accepte_ou_renonce() {
    let mut ps = etat_en_lecture(275_000);
    let decision = classify_playing(&PlayingInput {
        gapless_advance_pending: false,
        has_next: true,
        gapless_sent: ps.gapless_sent,
        track_duration_ms: 300_000,
        reported_duration_ms: 300_000,
        played_enough: true,
        position_ms: 275_000,
        past_end_ticks: 0,
        gapless_enabled: true,
        is_dlna: true,
        wall_elapsed_secs: 275,
    });
    assert!(decision.arm_gapless);
    // tick.rs, `GaplessPrep::Armed(arme)`
    let arme = LIGNE_42;
    ps.gapless_sent_at = Some(Instant::now());
    ps.gapless_sent = true;
    ps.gapless_armed = arme;
    let sans = ps.coherent().unwrap_err();
    assert_eq!(sans, drapeau("Lecture", "gapless_sent", "false", "true"));
    ps.transition(armement_accepte(arme));
    assert_eq!(
        ps.etat,
        EtatDeLecture::Armee {
            armement: Armement::Accepte { ligne: LIGNE_42 }
        }
    );
    ps.coherent().unwrap();
    appel_suit_le_marqueur(
        "GaplessPrep::Armed(arme) => {",
        "ps.transition(fsm::armement_accepte(arme));",
        9,
        Some(&sans),
    );

    // Sortie exclusive — tick.rs, `gapless_skipped_exclusive_output`.
    let mut ex = etat_en_lecture(275_000);
    ex.gapless_sent = true;
    ex.gapless_armed = None;
    let sans_ex = ex.coherent().unwrap_err();
    assert_eq!(sans_ex, drapeau("Lecture", "gapless_sent", "false", "true"));
    ex.transition(Transition::Armement {
        armement: Armement::Renonce,
    });
    assert_eq!(
        ex.etat,
        EtatDeLecture::Armee {
            armement: Armement::Renonce
        }
    );
    ex.coherent().unwrap();
    assert!(
        ex.gapless_sent_at.is_none(),
        "rien n'est parti : pas d'horodatage"
    );
    appel_suit_le_marqueur(
        "\"gapless_skipped_exclusive_output\"",
        "armement: Armement::Renonce,",
        8,
        Some(&sans_ex),
    );
}

// ── E4 · Armee → Lecture (transition détectée) ────────────────────────

/// E4 — durée changée + position confirmée (a), position remise à zéro
/// (b) : dans les deux cas l'état redevient `Lecture`, y compris depuis un
/// arrêt compté sous l'armement (la remise à zéro précède le bras
/// `Stopped`).
#[test]
fn e4_transition_detectee_arme_vers_lecture() {
    // (a) tick.rs, `gapless_transition_detected`
    let mut ps = etat_arme_en_ombre(295_000);
    ps.gapless_sent = false;
    ps.gapless_sent_at = None;
    let arme_avant = ps.gapless_armed.take();
    ps.peak_position_ms = 0;
    ps.last_position_ms = 0;
    ps.stopped_ticks = 0;
    ps.past_end_ticks = 0;
    ps.gapless_advance_pending = false;
    ps.gapless_stuck_ticks = 0;
    ps.gapless_arm_logged = None;
    ps.gapless_dsd_skip_pos = None;
    assert_eq!(arme_avant, LIGNE_42);
    let sans = ps.coherent().unwrap_err();
    assert_eq!(sans, drapeau("Armee", "gapless_sent", "true", "false"));
    ps.transition(Transition::TransitionDetectee);
    assert_eq!(ps.etat, EtatDeLecture::Lecture);
    ps.coherent().unwrap();
    appel_suit_le_marqueur(
        "\"gapless_transition_detected\"",
        "ps.transition(fsm::Transition::TransitionDetectee);",
        22,
        Some(&sans),
    );

    // (b) tick.rs, `gapless_position_reset_detected`, depuis un arrêt armé.
    let mut ps2 = etat_arme_en_ombre(280_000);
    ps2.stopped_ticks = 2;
    ps2.etat = EtatDeLecture::Arretee {
        depuis: Depuis::Armee(Armement::Accepte { ligne: LIGNE_42 }),
    };
    ps2.coherent().unwrap();
    assert!(decisions::position_reset(
        ps2.last_position_ms,
        1_000,
        ps2.gapless_sent
    ));
    ps2.gapless_sent = false;
    ps2.gapless_sent_at = None;
    ps2.gapless_armed.take();
    ps2.stopped_ticks = 0;
    ps2.gapless_advance_pending = false;
    let sans2 = ps2.coherent().unwrap_err();
    assert_eq!(sans2, drapeau("Arretee", "gapless_sent", "true", "false"));
    ps2.transition(Transition::TransitionDetectee);
    assert_eq!(ps2.etat, EtatDeLecture::Lecture);
    ps2.coherent().unwrap();
    appel_suit_le_marqueur(
        "\"gapless_position_reset_detected\"",
        "ps.transition(fsm::Transition::TransitionDetectee);",
        18,
        Some(&sans2),
    );
}

// ── E5 · Armee → Lecture (désarmement) ────────────────────────────────

/// E5 — armement expiré (a) ou file changée sous l'armement (b) : on
/// désarme sans que rien n'ait joué, et le même tour ré-arme.
#[test]
fn e5_desarmement_arme_vers_lecture() {
    let mut ps = etat_arme_en_ombre(280_000);
    assert!(decisions::gapless_stage_expired(
        ps.gapless_sent,
        Some(GAPLESS_STAGE_MAX_AGE_SECS + 1)
    ));
    ps.gapless_sent = false;
    ps.gapless_sent_at = None;
    ps.gapless_armed = None;
    let sans = ps.coherent().unwrap_err();
    assert_eq!(sans, drapeau("Armee", "gapless_sent", "true", "false"));
    ps.transition(Transition::Desarmement);
    assert_eq!(ps.etat, EtatDeLecture::Lecture);
    ps.coherent().unwrap();
    appel_suit_le_marqueur(
        "\"gapless_stage_expired_rearming\"",
        "ps.transition(fsm::Transition::Desarmement);",
        8,
        Some(&sans),
    );

    let mut ps2 = etat_arme_en_ombre(280_000);
    assert!(decisions::gapless_arm_outdated(
        ps2.gapless_armed.map(|a| a.row_id),
        Some(43)
    ));
    ps2.gapless_sent = false;
    ps2.gapless_sent_at = None;
    ps2.gapless_armed = None;
    ps2.gapless_arm_logged = None;
    let sans2 = ps2.coherent().unwrap_err();
    ps2.transition(Transition::Desarmement);
    assert_eq!(ps2.etat, EtatDeLecture::Lecture);
    ps2.coherent().unwrap();
    assert!(
        decisions::should_arm_gapless(ps2.gapless_sent, 300_000, 300_000, 280_000),
        "le même tour ré-arme"
    );
    appel_suit_le_marqueur(
        "\"gapless_rearm_queue_changed\"",
        "ps.transition(fsm::Transition::Desarmement);",
        18,
        Some(&sans2),
    );
}

// ── E6 · Armee → AvancePendante ───────────────────────────────────────

/// E6 — `Stopped` dans la garde, assez joué : `AvancePendante`. Depuis un
/// armement renoncé, la transition est inattendue (pas de garde sans
/// `SetNext`).
#[test]
fn e6_arret_dans_la_garde_arme_vers_avance_pendante() {
    let mut ps = etat_arme_en_ombre(290_000);
    let issue = classify_stopped(&entree_stopped(&ps, &observation(300_000, 290)));
    assert_eq!(issue, StoppedOutcome::GuardStoppedPending);
    appliquer_stopped(&mut ps, issue);
    let sans = ps.coherent().unwrap_err();
    assert_eq!(sans, drapeau("Armee", "gapless_sent", "true", "false"));
    ps.transition(Transition::ArretDansLaGarde);
    assert_eq!(ps.etat, EtatDeLecture::AvancePendante);
    ps.coherent().unwrap();
    assert!(
        suivant(
            &EtatDeLecture::Armee {
                armement: Armement::Renonce
            },
            Transition::ArretDansLaGarde
        )
        .is_err()
    );
    appel_suit_le_marqueur(
        "\"gapless_guard_stopped_pending_confirmation\"",
        "ps.transition(fsm::Transition::ArretDansLaGarde);",
        13,
        Some(&sans),
    );
}

// ── E7 · Lecture / Armee → Arretee ────────────────────────────────────

/// E7 — `Stopped` alors que Tune joue : un tick compté, l'état devient
/// `Arretee` et retient d'où il vient — l'armement survit à l'arrêt.
#[test]
fn e7_renderer_arrete_vers_arretee_qui_retient_l_armement() {
    let mut ps = etat_en_lecture(100_000);
    let issue = classify_stopped(&entree_stopped(&ps, &observation(300_000, 100)));
    assert_eq!(issue, StoppedOutcome::Waiting);
    appliquer_stopped(&mut ps, issue);
    let sans = ps.coherent().unwrap_err();
    assert_eq!(sans, drapeau("Lecture", "stopped_ticks", "0", "1"));
    ps.transition(Transition::RendererArrete);
    assert_eq!(
        ps.etat,
        EtatDeLecture::Arretee {
            depuis: Depuis::Lecture
        }
    );
    ps.coherent().unwrap();
    ps.stopped_ticks += 1;
    ps.transition(Transition::RendererArrete);
    assert_eq!(ps.stopped_ticks, 2);
    ps.coherent().unwrap();

    // Armé, garde expirée : l'arrêt compte sous l'armement.
    let mut arme = etat_arme_en_ombre(290_000);
    arme.gapless_sent_at = None;
    arme.stopped_ticks += 1;
    let sans_arme = arme.coherent().unwrap_err();
    assert_eq!(sans_arme, drapeau("Armee", "stopped_ticks", "0", "1"));
    arme.transition(Transition::RendererArrete);
    assert_eq!(
        arme.etat,
        EtatDeLecture::Arretee {
            depuis: Depuis::Armee(Armement::Accepte { ligne: LIGNE_42 })
        }
    );
    arme.coherent().unwrap();
    appel_suit_le_marqueur(
        "fsm_actual = Some(fsm::StoppedOutcome::Waiting);",
        "ps.transition(fsm::Transition::RendererArrete);",
        3,
        Some(&sans),
    );
}

// ── E8 · Arretee → Lecture / Armee ────────────────────────────────────

/// E8 — le compte d'arrêt est effacé : le renderer joue, ou est en pause,
/// ou Tune ne joue plus, ou une grâce masque l'arrêt. Six sites, un même
/// geste ; l'état revient d'où l'arrêt était parti.
#[test]
fn e8_arret_efface_rend_l_etat_d_avant() {
    let mut ps = etat_arrete(100_000, 3);
    ps.stopped_ticks = 0; // tick.rs, bras `Playing`
    let sans = ps.coherent().unwrap_err();
    assert_eq!(
        sans,
        drapeau("Arretee", "stopped_ticks > 0", "true", "false")
    );
    ps.transition(Transition::ArretEfface);
    assert_eq!(ps.etat, EtatDeLecture::Lecture);
    ps.coherent().unwrap();

    let mut arme = etat_arme_en_ombre(290_000);
    arme.gapless_sent_at = None;
    arme.stopped_ticks = 1;
    arme.transition(Transition::RendererArrete);
    arme.stopped_ticks = 0;
    arme.transition(Transition::ArretEfface);
    assert_eq!(
        arme.etat,
        EtatDeLecture::Armee {
            armement: Armement::Accepte { ligne: LIGNE_42 }
        }
    );
    arme.coherent().unwrap();

    // Identité hors arrêt : chaque tour `Playing` passe par là.
    let mut lecture = etat_en_lecture(10_000);
    lecture.transition(Transition::ArretEfface);
    assert_eq!(lecture.etat, EtatDeLecture::Lecture);

    for (marqueur, fenetre) in [
        (
            "TransportState::Playing | TransportState::Transitioning => {",
            3,
        ),
        ("TransportState::Paused => {", 3),
        (
            "TransportState::Stopped if !tune_is_playing || !tune_has_track => {",
            4,
        ),
        (
            "fsm_actual = Some(fsm::StoppedOutcome::SuppressSeekGrace);",
            3,
        ),
        (
            "fsm_actual = Some(fsm::StoppedOutcome::SuppressLoadGrace);",
            3,
        ),
        (
            "fsm_actual = Some(fsm::StoppedOutcome::SuppressCooldown);",
            4,
        ),
    ] {
        appel_suit_le_marqueur(
            marqueur,
            "ps.transition(fsm::Transition::ArretEfface);",
            fenetre,
            Some(&sans),
        );
    }
}

// ── E9 · Arretee (armé) → AvancePendante ──────────────────────────────

/// E9 — `Stopped` × 5, fin naturelle, armé, sortie capable : on attend
/// l'enchaînement. Depuis un arrêt non armé, inattendu.
#[test]
fn e9_fin_naturelle_en_attente_d_enchainement() {
    let mut ps = etat_arme_en_ombre(290_000);
    ps.gapless_sent_at = None;
    ps.stopped_ticks = STOPPED_TICKS_THRESHOLD - 1;
    ps.etat = EtatDeLecture::Arretee {
        depuis: Depuis::Armee(Armement::Accepte { ligne: LIGNE_42 }),
    };
    let issue = classify_stopped(&entree_stopped(&ps, &observation(300_000, 290)));
    assert_eq!(issue, StoppedOutcome::NaturalEndGaplessWaiting);
    appliquer_stopped(&mut ps, issue);
    let sans = ps.coherent().unwrap_err();
    assert_eq!(sans, drapeau("Arretee", "gapless_sent", "true", "false"));
    ps.transition(Transition::FinNaturelleEnAttenteDEnchainement);
    assert_eq!(ps.etat, EtatDeLecture::AvancePendante);
    ps.coherent().unwrap();
    assert!(
        suivant(
            &EtatDeLecture::Arretee {
                depuis: Depuis::Lecture
            },
            Transition::FinNaturelleEnAttenteDEnchainement
        )
        .is_err()
    );
    appel_suit_le_marqueur(
        "\"gapless_natural_end_waiting_for_transition\"",
        "fsm::Transition::FinNaturelleEnAttenteDEnchainement,",
        15,
        Some(&sans),
    );
}

// ── E10 · Arretee → Terminee(Finie) ───────────────────────────────────

/// E10 — `Stopped` × 5, fin naturelle, flux servi : la piste est finie.
/// L'état terminal porte le motif que `track_end_gap` journalise.
#[test]
fn e10_fin_naturelle_apres_arret_vers_finie() {
    let mut ps = etat_arrete(290_000, STOPPED_TICKS_THRESHOLD - 1);
    let issue = classify_stopped(&entree_stopped(&ps, &observation(300_000, 290)));
    assert_eq!(issue, StoppedOutcome::NaturalEndAdvance);
    assert_eq!(appliquer_stopped(&mut ps, issue), (true, false));
    ps.coherent()
        .unwrap_or_else(|e| panic!("l'arrêt reste cohérent jusqu'à la transition : {e}"));
    ps.transition(Transition::FinNaturelleApresArret);
    assert_eq!(
        ps.etat,
        EtatDeLecture::Terminee(Issue::Finie(MotifFin::FinNaturelleApresArret))
    );
    ps.coherent().unwrap();
    assert_eq!(
        MotifFin::FinNaturelleApresArret.etiquette(),
        decisions::motif_fin::FIN_NATURELLE_APRES_STOPPED
    );
    let sans = suivant(&EtatDeLecture::Lecture, Transition::FinNaturelleApresArret).unwrap_err();
    appel_suit_le_marqueur(
        "fsm_actual = Some(fsm::StoppedOutcome::NaturalEndAdvance);",
        "ps.transition(fsm::Transition::FinNaturelleApresArret);",
        7,
        Some(&sans),
    );
}

// ── E11 · Arretee → Terminee(Coupee) ──────────────────────────────────

/// E11 — la zone est coupée : compteur d'octets MESURÉ à sec après
/// `Stopped` × 30 (a), ou fin refusée dix fois sur un flux incomplet (b).
#[test]
fn e11_panne_de_lecture_vers_coupee() {
    let mut ps = etat_arrete(10_000, STOPPED_FAILURE_THRESHOLD - 1);
    let mut o = observation(300_000, 40);
    o.consommation = ConsommationFlux::ASec;
    let issue = classify_stopped(&entree_stopped(&ps, &o));
    assert_eq!(issue, StoppedOutcome::FailureStop);
    assert_eq!(appliquer_stopped(&mut ps, issue), (false, true));
    ps.transition(Transition::PanneDeLecture {
        cause: CauseDeCoupure::FluxASec,
    });
    assert_eq!(
        ps.etat,
        EtatDeLecture::Terminee(Issue::Coupee(CauseDeCoupure::FluxASec))
    );
    ps.coherent().unwrap();
    let sans = suivant(
        &EtatDeLecture::Lecture,
        Transition::PanneDeLecture {
            cause: CauseDeCoupure::FluxASec,
        },
    )
    .unwrap_err();
    appel_suit_le_marqueur(
        "\"playback_failure_stopping_zone\"",
        "cause: CauseDeCoupure::FluxASec,",
        7,
        Some(&sans),
    );

    let mut cale = etat_arrete(290_000, STOPPED_TICKS_THRESHOLD);
    cale.stall_declines = STALL_DECLINE_MAX_TICKS;
    cale.transition(Transition::PanneDeLecture {
        cause: CauseDeCoupure::RendererCale,
    });
    assert_eq!(
        cale.etat,
        EtatDeLecture::Terminee(Issue::Coupee(CauseDeCoupure::RendererCale))
    );
    appel_suit_le_marqueur(
        "\"renderer_stalled_not_advancing_stopping_zone\"",
        "cause: CauseDeCoupure::RendererCale,",
        6,
        Some(&sans),
    );
}

// ── E12 · Arretee → Arretee ───────────────────────────────────────────

/// E12 — `Stopped` × 30, octets consommés ou inconnus : on attend encore
/// (#2394). L'état ne bouge pas ; hors arrêt la transition est inattendue.
#[test]
fn e12_attente_prolongee_reste_arretee() {
    let mut ps = etat_arrete(10_000, STOPPED_FAILURE_THRESHOLD - 1);
    for consommation in [ConsommationFlux::Consomme, ConsommationFlux::Inconnue] {
        let mut o = observation(300_000, 40);
        o.consommation = consommation;
        let issue = classify_stopped(&entree_stopped(&ps, &o));
        assert!(!issue.is_force_stop());
        appliquer_stopped(&mut ps, issue);
        ps.transition(Transition::AttenteProlongee);
        assert_eq!(
            ps.etat,
            EtatDeLecture::Arretee {
                depuis: Depuis::Lecture
            }
        );
        ps.coherent().unwrap();
    }
    let sans = suivant(&EtatDeLecture::Lecture, Transition::AttenteProlongee).unwrap_err();
    for marqueur in [
        "fsm_actual = Some(fsm::StoppedOutcome::FailureWaitingConsuming);",
        "fsm_actual = Some(fsm::StoppedOutcome::FailureWaitingUnknown);",
    ] {
        appel_suit_le_marqueur(
            marqueur,
            "ps.transition(fsm::Transition::AttenteProlongee);",
            2,
            Some(&sans),
        );
    }
}

// ── E13 · AvancePendante → Lecture ────────────────────────────────────

/// E13 — le renderer rejoue alors qu'une avance est pendante : confirmé,
/// les métadonnées avancent, l'état redevient `Lecture`.
#[test]
fn e13_enchainement_confirme_vers_lecture() {
    let mut ps = etat_avance_pendante();
    ps.gapless_advance_pending = false;
    ps.gapless_stuck_ticks = 0;
    let sans = ps.coherent().unwrap_err();
    assert_eq!(
        sans,
        drapeau("AvancePendante", "gapless_advance_pending", "true", "false")
    );
    ps.transition(Transition::EnchainementConfirme);
    assert_eq!(ps.etat, EtatDeLecture::Lecture);
    ps.coherent().unwrap();
    appel_suit_le_marqueur(
        "// Renderer started playing — gapless transition confirmed.",
        "ps.transition(fsm::Transition::EnchainementConfirme);",
        8,
        Some(&sans),
    );
}

// ── E14 · AvancePendante → Terminee(Finie) ────────────────────────────

/// E14 — refroidissement écoulé, deux tours bloqués : fin forcée, motif
/// `gapless_advance_stuck`.
#[test]
fn e14_enchainement_bloque_vers_finie() {
    let mut ps = etat_avance_pendante();
    let o = observation(300_000, 290);
    let mut verdict = (false, false);
    for _ in 0..6 {
        let issue = classify_stopped(&entree_stopped(&ps, &o));
        verdict = appliquer_stopped(&mut ps, issue);
        if issue == StoppedOutcome::SuppressCooldown {
            ps.transition(Transition::ArretEfface);
        }
        if verdict.0 {
            break;
        }
        ps.coherent().unwrap();
    }
    assert_eq!(verdict, (true, false));
    let sans = ps.coherent().unwrap_err();
    assert_eq!(
        sans,
        drapeau("AvancePendante", "gapless_advance_pending", "true", "false")
    );
    ps.transition(Transition::EnchainementBloque);
    assert_eq!(
        ps.etat,
        EtatDeLecture::Terminee(Issue::Finie(MotifFin::AvanceGaplessBloquee))
    );
    ps.coherent().unwrap();
    assert_eq!(
        MotifFin::AvanceGaplessBloquee.etiquette(),
        decisions::motif_fin::AVANCE_GAPLESS_BLOQUEE
    );
    appel_suit_le_marqueur(
        "\"gapless_advance_stuck_forcing_play\"",
        "ps.transition(fsm::Transition::EnchainementBloque);",
        8,
        Some(&sans),
    );
}

// ── E15 · Lecture → Terminee(Finie) ───────────────────────────────────

/// E15 — position au-delà de la durée + 3 s pendant trois tours.
#[test]
fn e15_position_au_dela_de_la_fin_vers_finie() {
    let mut ps = etat_en_lecture(240_000);
    ps.past_end_ticks = POSITION_PAST_END_TICKS;
    ps.transition(Transition::PositionAuDelaDeLaFin);
    assert_eq!(
        ps.etat,
        EtatDeLecture::Terminee(Issue::Finie(MotifFin::PositionAuDelaDeLaFin))
    );
    ps.coherent().unwrap();
    assert_eq!(
        MotifFin::PositionAuDelaDeLaFin.etiquette(),
        decisions::motif_fin::POSITION_AU_DELA_DE_LA_FIN
    );
    let sans = suivant(
        &EtatDeLecture::AvancePendante,
        Transition::PositionAuDelaDeLaFin,
    )
    .unwrap_err();
    appel_suit_le_marqueur(
        "\"position_past_end_advancing\"",
        "ps.transition(fsm::Transition::PositionAuDelaDeLaFin);",
        6,
        Some(&sans),
    );
}

// ── E16 · Lecture → Terminee(Finie), avant le seuil ───────────────────

/// E16 — la sortie signale la fin avant les cinq tours : `ended_naturally`
/// plausible (locale), ou DSD sur DLNA au pic.
#[test]
fn e16_fin_constatee_avant_le_seuil_vers_finie() {
    let mut ps = etat_en_lecture(290_000);
    let mut o = observation(300_000, 290);
    o.ended_naturally = true;
    let issue = classify_stopped(&entree_stopped(&ps, &o));
    assert_eq!(issue, StoppedOutcome::LocalEndedNaturally);
    assert_eq!(appliquer_stopped(&mut ps, issue), (true, false));
    ps.transition(Transition::FinConstateeAvantLeSeuil {
        motif: MotifFin::FinNaturelleLocale,
    });
    assert_eq!(
        ps.etat,
        EtatDeLecture::Terminee(Issue::Finie(MotifFin::FinNaturelleLocale))
    );
    ps.coherent().unwrap();

    let mut dsd = etat_en_lecture(300_000);
    let mut o2 = observation(300_000, 300);
    o2.dlna_dsd_reached_end = true;
    let issue2 = classify_stopped(&entree_stopped(&dsd, &o2));
    assert_eq!(issue2, StoppedOutcome::DsdDlnaReachedEnd);
    assert_eq!(appliquer_stopped(&mut dsd, issue2), (true, false));
    dsd.transition(Transition::FinConstateeAvantLeSeuil {
        motif: MotifFin::DsdDlnaPicAtteint,
    });
    assert_eq!(
        dsd.etat,
        EtatDeLecture::Terminee(Issue::Finie(MotifFin::DsdDlnaPicAtteint))
    );
    assert_eq!(
        MotifFin::FinNaturelleLocale.etiquette(),
        decisions::motif_fin::FIN_NATURELLE_LOCALE
    );
    assert_eq!(
        MotifFin::DsdDlnaPicAtteint.etiquette(),
        decisions::motif_fin::DSD_DLNA_PIC_ATTEINT
    );
    let sans = suivant(
        &EtatDeLecture::AvancePendante,
        Transition::FinConstateeAvantLeSeuil {
            motif: MotifFin::FinNaturelleLocale,
        },
    )
    .unwrap_err();
    appel_suit_le_marqueur(
        "\"local_output_ended_naturally_advancing\"",
        "motif: MotifFin::FinNaturelleLocale,",
        7,
        Some(&sans),
    );
    appel_suit_le_marqueur(
        "\"dlna_dsd_reached_end_advancing\"",
        "motif: MotifFin::DsdDlnaPicAtteint,",
        8,
        Some(&sans),
    );
}

// ── E17 · Lecture → Terminee(Coupee) ──────────────────────────────────

/// E17 — `Playing` × 30 sans progrès ni octets : la zone est coupée.
#[test]
fn e17_lecture_sans_progres_vers_coupee() {
    let mut ps = etat_en_lecture(60_000);
    ps.playing_stall_ticks = PLAYING_STALL_THRESHOLD;
    ps.transition(Transition::LectureSansProgres);
    assert_eq!(
        ps.etat,
        EtatDeLecture::Terminee(Issue::Coupee(CauseDeCoupure::LectureSansProgres))
    );
    ps.coherent().unwrap();
    let sans = suivant(&EtatDeLecture::Radio, Transition::LectureSansProgres).unwrap_err();
    appel_suit_le_marqueur(
        "\"dlna_playing_without_progress_stopping_zone\"",
        "ps.transition(fsm::Transition::LectureSansProgres);",
        5,
        Some(&sans),
    );
}

// ── E18 · tout état → SondageEnEchec ──────────────────────────────────

/// E18 — la sonde rend `Err` : l'état d'avant est mis de côté, le recul
/// double. Une panne qui dure garde le même état d'avant.
#[test]
fn e18_sonde_en_echec_met_l_etat_de_cote() {
    let mut ps = etat_arme_en_ombre(280_000);
    ps.consecutive_errors = ps.consecutive_errors.saturating_add(1);
    ps.total_errors += 1;
    ps.backoff_remaining = 1u8 << ps.consecutive_errors.min(4);
    ps.coherent()
        .unwrap_or_else(|e| panic!("les drapeaux de lecture n'ont pas bougé : {e}"));
    ps.transition(Transition::SondeEnEchec);
    assert_eq!(
        ps.etat,
        EtatDeLecture::SondageEnEchec {
            precedent: Box::new(EtatDeLecture::Armee {
                armement: Armement::Accepte { ligne: LIGNE_42 }
            })
        }
    );
    ps.coherent().unwrap();
    ps.consecutive_errors += 1;
    ps.transition(Transition::SondeEnEchec);
    assert!(
        matches!(&ps.etat, EtatDeLecture::SondageEnEchec { precedent } if **precedent != EtatDeLecture::SondageEnEchec { precedent: Box::new(EtatDeLecture::Lecture) } && precedent.nom() == "Armee")
    );
    // Sans l'appel : `SondeRetablie` depuis `Armee` est une identité, donc
    // l'oubli ne se voit qu'ici, à l'entrée en panne.
    let mut oubli = etat_arme_en_ombre(280_000);
    oubli.consecutive_errors = 1;
    oubli.etat = EtatDeLecture::SondageEnEchec {
        precedent: Box::new(EtatDeLecture::Lecture),
    };
    let sans = oubli.coherent().unwrap_err();
    assert_eq!(sans, drapeau("Lecture", "gapless_sent", "false", "true"));
    appel_suit_le_marqueur(
        "ps.backoff_remaining = 1u8 << ps.consecutive_errors.min(4);",
        "ps.transition(fsm::Transition::SondeEnEchec);",
        2,
        Some(&sans),
    );
}

// ── E19 · SondageEnEchec → Terminee(Finie) ────────────────────────────

/// E19 — sonde en échec, DLNA, horloge écoulée : la fin est prononcée par
/// une transition nommée (décision du 12/09) ; l'état survit à l'emprunt
/// avec le verrou levé, et seul lui a le droit de le porter.
#[test]
fn e19_fin_par_horloge_murale_vers_finie_qui_survit() {
    let mut ps = etat_en_lecture(10_000);
    for _ in 0..2 {
        ps.consecutive_errors = ps.consecutive_errors.saturating_add(1);
        ps.backoff_remaining = 1u8 << ps.consecutive_errors.min(4);
        ps.transition(Transition::SondeEnEchec);
    }
    assert!(decisions::poll_failed_past_end(
        true,
        true,
        300_000,
        304,
        ps.consecutive_errors,
        ps.wall_clock_end_fired
    ));
    ps.wall_clock_end_fired = true; // tick.rs, `dlna_poll_failed_wall_clock_advancing`
    let sans = ps.coherent().unwrap_err();
    assert_eq!(
        sans,
        drapeau("SondageEnEchec", "wall_clock_end_fired", "false", "true")
    );
    ps.transition(Transition::FinParHorlogeMurale);
    assert_eq!(
        ps.etat,
        EtatDeLecture::Terminee(Issue::Finie(MotifFin::HorlogeMuraleSurSondeEnEchec))
    );
    ps.coherent().unwrap();
    // L'état survit : la sonde peut encore échouer ou se rétablir, il reste
    // terminal ; une nouvelle piste seule le ramène.
    ps.transition(Transition::SondeEnEchec);
    ps.transition(Transition::SondeRetablie);
    assert!(matches!(ps.etat, EtatDeLecture::Terminee(_)));
    ps.coherent().unwrap();
    ps.wall_clock_end_fired = false;
    ps.transition(Transition::NouvellePiste);
    assert_eq!(ps.etat, EtatDeLecture::Neuve);
    ps.coherent().unwrap();
    appel_suit_le_marqueur(
        "\"dlna_poll_failed_wall_clock_advancing\"",
        "ps.transition(fsm::Transition::FinParHorlogeMurale);",
        5,
        Some(&sans),
    );
}

// ── E20 · SondageEnEchec → état précédent ─────────────────────────────

/// E20 — la sonde rend `Ok` : l'état d'avant la panne est restitué, tel
/// quel. Hors panne, c'est une identité que chaque tour traverse.
#[test]
fn e20_sonde_retablie_restitue_l_etat_d_avant() {
    let mut ps = etat_arme_en_ombre(280_000);
    ps.consecutive_errors = 3;
    ps.transition(Transition::SondeEnEchec);
    ps.consecutive_errors = 0; // tick.rs, `Ok((s, …))`
    let sans = ps.coherent().unwrap_err();
    assert_eq!(
        sans,
        drapeau("SondageEnEchec", "consecutive_errors > 0", "true", "false")
    );
    ps.transition(Transition::SondeRetablie);
    assert_eq!(
        ps.etat,
        EtatDeLecture::Armee {
            armement: Armement::Accepte { ligne: LIGNE_42 }
        }
    );
    ps.coherent().unwrap();
    ps.transition(Transition::SondeRetablie);
    assert_eq!(ps.etat.nom(), "Armee", "identité hors panne");
    appel_suit_le_marqueur(
        "ps.consecutive_errors = 0;",
        "ps.transition(fsm::Transition::SondeRetablie);",
        2,
        Some(&sans),
    );
}

// ── E21 · Neuve → Radio ───────────────────────────────────────────────

/// E21 — `now_playing.source == "radio"` : l'état est `Radio`, depuis
/// `Neuve` seulement (une radio naît d'un `play`, donc d'une génération
/// neuve) ; depuis une lecture armée, la transition est inattendue.
#[test]
fn e21_source_radio_depuis_neuve() {
    let mut ps = ZonePollState::new(1);
    ps.last_radio_poll = Instant::now();
    ps.transition(Transition::SourceRadio);
    assert_eq!(ps.etat, EtatDeLecture::Radio);
    ps.coherent().unwrap();
    ps.transition(Transition::SourceRadio);
    assert_eq!(ps.etat, EtatDeLecture::Radio);
    let sans = suivant(
        &EtatDeLecture::Armee {
            armement: Armement::Renonce,
        },
        Transition::SourceRadio,
    )
    .unwrap_err();
    appel_suit_le_marqueur(
        "// Update last_radio_poll so the throttle gate works on next tick.",
        "ps.transition(fsm::Transition::SourceRadio);",
        5,
        Some(&sans),
    );
}

// ── E22 · Radio → Terminee(Coupee) ────────────────────────────────────

/// E22 — six ticks `Stopped` sans position, ou station refusée : la radio
/// est abandonnée, la zone coupée.
#[test]
fn e22_radio_abandonnee_vers_coupee() {
    let mut ps = ZonePollState::new(1);
    ps.transition(Transition::SourceRadio);
    ps.radio_stopped_ticks = 6;
    ps.transition(Transition::RadioAbandonnee);
    assert_eq!(
        ps.etat,
        EtatDeLecture::Terminee(Issue::Coupee(CauseDeCoupure::RadioAbandonnee))
    );
    ps.coherent().unwrap();
    let sans = suivant(&EtatDeLecture::Lecture, Transition::RadioAbandonnee).unwrap_err();
    appel_suit_le_marqueur(
        "\"radio_renderer_stopped_giving_up\"",
        "ps.transition(fsm::Transition::RadioAbandonnee);",
        5,
        Some(&sans),
    );
}

// ── E23 · le vrai `tick`, de bout en bout ─────────────────────────────

/// E23 — un renderer factice, une zone en lecture, et le `tick` de
/// production : `Neuve` → `Lecture` au premier échantillon, `Arretee` sur
/// `Stopped`, retour à `Lecture` quand il rejoue — l'invariant de fin de
/// tour tenu à chaque fois (sinon `tick` panique en debug). Les armements et
/// transitions gapless de bout en bout sont couverts par le banc
/// `lire_ensuite_dans_la_fenetre_gapless`, qui traverse le même
/// `debug_assert!`.
#[tokio::test]
async fn e23_le_tick_de_production_tient_l_invariant() {
    use crate::db::migrations::run_migrations;
    use crate::db::sqlite::SqliteDb;
    use crate::db::zone_repo::ZoneRepo;
    use crate::http::streamer::AudioStreamer;
    use crate::orchestrator::PlaybackOrchestrator;
    use crate::outputs::OutputRegistry;
    use crate::outputs::mock::MockOutput;
    use crate::playback::{NowPlaying, PlaybackManager};
    use crate::streaming::ServiceRegistry;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    const APPAREIL: &str = "dlna:temoin-ref9";
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("Salon", Some("dlna"), Some(APPAREIL))
        .unwrap();
    let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
    outputs.lock().await.register(Box::new(
        MockOutput::new(APPAREIL, "Témoin REF-9").with_type("dlna"),
    ));
    let playback = Arc::new(PlaybackManager::new());
    let orchestrator = Arc::new(PlaybackOrchestrator::new(
        db.clone(),
        playback.clone(),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        outputs.clone(),
        None,
    ));
    let poller = PositionPoller::new(
        orchestrator,
        playback.clone(),
        outputs.clone(),
        db.clone(),
        Arc::new(Mutex::new(HashMap::new())),
    );
    playback
        .play(
            zone_id,
            NowPlaying {
                track_id: Some(1),
                title: "Témoin".into(),
                source: "local".into(),
                duration_ms: 300_000,
                ..Default::default()
            },
        )
        .await;
    let generation = playback.get_state(zone_id).await.track_generation;
    let mut poll_states = HashMap::new();
    poll_states.insert(zone_id, ZonePollState::new(generation));
    let mut idle = HashMap::new();

    let poser = |outputs: Arc<Mutex<OutputRegistry>>, etat: TransportState, position_ms: u64| async move {
        let reg = outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.set_state(etat).await;
        mock.set_duration(300_000);
        mock.set_position(position_ms);
    };

    // Tour 1 : premier échantillon honnête (l'horloge de piste est datée
    // pour franchir `stale_start_position`, comme le banc #3026).
    {
        let ps = poll_states.get_mut(&zone_id).unwrap();
        ps.track_started_at = Some(Instant::now() - Duration::from_secs(320));
        assert_eq!(ps.etat, EtatDeLecture::Neuve);
    }
    poser(outputs.clone(), TransportState::Playing, 10_000).await;
    poller
        .tick(&mut poll_states, &mut idle, &Instant::now())
        .await;
    assert_eq!(poll_states[&zone_id].etat, EtatDeLecture::Lecture);

    // Tours 2-3 : le renderer dit `Stopped`, Tune joue : arrêt compté.
    poser(outputs.clone(), TransportState::Stopped, 10_000).await;
    poller
        .tick(&mut poll_states, &mut idle, &Instant::now())
        .await;
    poller
        .tick(&mut poll_states, &mut idle, &Instant::now())
        .await;
    let ps = &poll_states[&zone_id];
    assert_eq!(
        ps.etat,
        EtatDeLecture::Arretee {
            depuis: Depuis::Lecture
        }
    );
    assert_eq!(ps.stopped_ticks, 2);
    ps.coherent().unwrap();

    // Tour 4 : il rejoue.
    poser(outputs.clone(), TransportState::Playing, 12_000).await;
    poller
        .tick(&mut poll_states, &mut idle, &Instant::now())
        .await;
    let ps = &poll_states[&zone_id];
    assert_eq!(ps.etat, EtatDeLecture::Lecture);
    assert_eq!(ps.stopped_ticks, 0);
    ps.coherent().unwrap();
}
