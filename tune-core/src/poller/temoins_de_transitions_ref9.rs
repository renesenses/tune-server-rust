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
