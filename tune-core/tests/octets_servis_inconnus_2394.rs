//! « Zéro octet servi » et « je ne sais pas » ne sont pas le même chiffre (#2394).
//!
//! ## Le défaut
//!
//! Le DMP-A8 de Bertrand (machine `.18`) cale à 5 s, le journal dit
//! `bytes_sent=0`, et la zone est coupée. Le sondeur (`tune-core/src/poller.rs`,
//! bras `TransportState::Stopped`) tenait le compte des octets servis ainsi :
//!
//! ```text
//! let current_bytes = if let Some(ref sid) = stream_id {
//!     self.orchestrator.streamer_bytes_sent(sid).await.unwrap_or(0)
//! } else {
//!     0
//! };
//! let stream_consuming = current_bytes > 0 && current_bytes > ps.last_bytes_sent;
//! ```
//!
//! Deux ignorances distinctes y rendaient `0` :
//!
//! 1. le now-playing n'a pas de `stream_id` — branche `else`,
//! 2. `streamer_bytes_sent` ne connaît pas la session — `unwrap_or(0)`.
//!
//! Or ce chiffre-là est celui qui arme `force_stop` trente ticks plus tard
//! (`STOPPED_FAILURE_THRESHOLD = 30`, un tick par seconde). Une zone qui joue
//! parfaitement mais dont le sondeur ignore le `stream_id` était donc
//! **indiscernable** d'une zone qui ne reçoit rien, et coupée au bout de trente
//! secondes. Pire : l'échappatoire qui existe exactement pour ces appareils —
//! « le renderer joue mais n'annonce pas son état » (DMP-A10, LHC, Shanling) —
//! exige `current_bytes > 0`, ce qu'un `0` d'ignorance ne produit jamais. Elle
//! était désarmée par la confusion même qu'elle devait rattraper.
//!
//! ## Pourquoi le `stream_id` manque, et pas seulement en panne
//!
//! `Orchestrator::advance_queue_metadata` — l'avance gapless, que le sondeur
//! s'appelle à LUI-MÊME quand un renderer DLNA enchaîne en interne — construit
//! un now-playing avec `stream_id: None` en dur. La reprise « le renderer joue,
//! Tune ne le croyait pas » en pose un aussi, la base ne mémorisant pas un
//! identifiant de session (cf. `decisions::stream_id_de_l_uri`, #2991). Une
//! écoute gapless sur DLNA passe donc STRUCTURELLEMENT en « inconnu » dès la
//! deuxième piste de l'album.
//!
//! ## Ce que ce fichier tient
//!
//! Quatre épreuves, dont une contre-épreuve et une garde de site :
//!
//! 1. **zéro réel** — compteur mesuré qui n'avance pas : la zone est coupée,
//!    comme avant. C'est la contre-épreuve : le correctif ne DÉSARME pas la
//!    garde, il lui retire seulement l'ignorance de son domaine ;
//! 2. **inconnu** — pas de `stream_id`, ou session inconnue du gestionnaire de
//!    flux : la zone n'est PAS coupée ;
//! 3. **nominal** — un flux qui délivre : inchangé ;
//! 4. **la garde de site** — les trois premières interrogent
//!    `fsm::consommation_flux` et `fsm::classify_stopped`, deux fonctions
//!    pures. Elles ne verraient pas un `unwrap_or(0)` réintroduit dans le bras
//!    de production, qui est un `async fn tick` de plusieurs centaines de
//!    lignes qu'aucune épreuve ne peut piloter sans un orchestrateur, une base
//!    et un appareil. La quatrième LIT donc le bras lui-même (`include_str!`,
//!    l'idiome déjà employé par `terminologie_eq.rs`) et vérifie que le
//!    `Option` y traverse intact jusqu'à la décision, que la branche
//!    « inconnue » existe et ne coupe pas, et que la branche mesurée coupe
//!    toujours.

use tune_core::poller::fsm::{
    ConsommationFlux, StoppedInput, StoppedOutcome, classify_stopped, consommation_flux,
};

/// Le source du sondeur, lu à la compilation — voir l'épreuve 4. `tick`, qui
/// porte le bras du seuil d'échec, vit dans son propre module depuis REF-1
/// (#2219) et se lit en premier, comme dans le fichier d'origine.
const SOURCE_POLLER: &str = concat!(
    include_str!("../src/poller/tick.rs"),
    include_str!("../src/poller.rs")
);

/// `STOPPED_FAILURE_THRESHOLD` vaut 30 et le bras applique son propre `+1` :
/// 29 ticks en entrée placent la décision exactement AU seuil.
const TICKS_AU_SEUIL: u8 = 29;

/// Une zone en lecture, arrivée au seuil d'échec, sans fin naturelle. Seule la
/// consommation du flux départage les issues.
fn au_seuil_d_echec(consommation: ConsommationFlux) -> StoppedInput {
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
        wall_elapsed: 40,
        track_duration_ms: 240_000,
        stopped_ticks: TICKS_AU_SEUIL,
        natural_end: false,
        gapless_sent: false,
        realtime: true,
        can_internal_gapless: true,
        consommation,
        dlna_dsd_reached_end: false,
    }
}

// ─────────────────────────── 1. Zéro réel ───────────────────────────
//
// LA CONTRE-ÉPREUVE. Une zone réellement à sec doit continuer d'être coupée :
// sinon l'utilisateur regarde un compteur qui avance sans un son, et le défaut
// qu'on prétend corriger est remplacé par un pire.

#[test]
fn un_flux_mesure_a_zero_est_a_sec() {
    // Le flux est enregistré, le gestionnaire répond : 0 octet servi.
    assert_eq!(consommation_flux(Some(0), 0), ConsommationFlux::ASec);
    assert!(consommation_flux(Some(0), 0).est_mesuree());
}

#[test]
fn un_flux_mesure_qui_n_avance_plus_est_a_sec() {
    // Décrochage EN COURS de lecture : le compteur est connu, non nul, et gelé.
    assert_eq!(
        consommation_flux(Some(1_048_576), 1_048_576),
        ConsommationFlux::ASec
    );
}

#[test]
fn zero_reel_coupe_toujours_la_zone() {
    let entree = au_seuil_d_echec(ConsommationFlux::ASec);
    assert_eq!(
        classify_stopped(&entree),
        StoppedOutcome::FailureStop,
        "un flux mesuré qui ne délivre rien pendant 30 s doit couper la zone"
    );
    assert!(
        classify_stopped(&entree).is_force_stop(),
        "la garde ne doit pas être désarmée"
    );
}

// ─────────────────────────── 2. Inconnu ───────────────────────────

#[test]
fn sans_stream_id_la_consommation_est_inconnue_pas_nulle() {
    assert_eq!(consommation_flux(None, 0), ConsommationFlux::Inconnue);
    assert_ne!(
        consommation_flux(None, 0),
        consommation_flux(Some(0), 0),
        "« je ne sais pas » ne doit plus être le même chiffre que « zéro servi »"
    );
    assert!(!consommation_flux(None, 0).est_mesuree());
}

#[test]
fn inconnu_ne_coupe_pas_la_zone() {
    let entree = au_seuil_d_echec(ConsommationFlux::Inconnue);
    assert_eq!(
        classify_stopped(&entree),
        StoppedOutcome::FailureWaitingUnknown,
        "sans mesure, le sondeur attend — il ne coupe pas une zone qui joue"
    );
    assert!(
        !classify_stopped(&entree).is_force_stop(),
        "couper une zone parce qu'on ne sait pas la mesurer est pire que le \
         défaut qu'on croit prévenir"
    );
}

#[test]
fn l_etat_inconnu_est_nommable_donc_observable() {
    // Un état qu'on ne peut pas nommer dans un journal se reconfond avec zéro à
    // la première occasion. Les trois étiquettes sont distinctes.
    assert_eq!(ConsommationFlux::Inconnue.etiquette(), "inconnue");
    assert_eq!(ConsommationFlux::ASec.etiquette(), "a_sec");
    assert_eq!(ConsommationFlux::Consomme.etiquette(), "consomme");
}

// ─────────────────────────── 3. Nominal ───────────────────────────

#[test]
fn un_flux_qui_delivre_ne_change_pas() {
    assert_eq!(consommation_flux(Some(4096), 0), ConsommationFlux::Consomme);
    let entree = au_seuil_d_echec(ConsommationFlux::Consomme);
    assert_eq!(
        classify_stopped(&entree),
        StoppedOutcome::FailureWaitingConsuming
    );
    assert!(!classify_stopped(&entree).is_force_stop());
}

// ──────────────────── 4. La garde du site de production ────────────────────

/// Le bras du seuil d'échec, découpé dans le source du sondeur.
fn branche_du_seuil_d_echec() -> &'static str {
    // Sans l'accolade : la condition du bras est multiligne depuis #4480 (le
    // seuil en ticks s'y double d'un plancher en secondes).
    let debut = SOURCE_POLLER
        .find("} else if ps.stopped_ticks >= STOPPED_FAILURE_THRESHOLD")
        .expect("le bras du seuil d'échec a disparu de poller.rs et poller/tick.rs");
    let fin = SOURCE_POLLER[debut..]
        .find("\"stopped_early_waiting\"")
        .expect(
            "la branche suivante (stopped_early_waiting) a disparu de poller.rs et poller/tick.rs",
        );
    &SOURCE_POLLER[debut..debut + fin]
}

#[test]
fn le_seuil_d_echec_reste_a_trente_ticks() {
    // Les épreuves ci-dessus placent 29 ticks en entrée pour tomber AU seuil.
    assert!(
        SOURCE_POLLER.contains("const STOPPED_FAILURE_THRESHOLD: u8 = 30;"),
        "le seuil a changé : TICKS_AU_SEUIL doit suivre"
    );
}

#[test]
fn le_compteur_de_production_garde_le_droit_de_ne_pas_savoir() {
    let branche = branche_du_seuil_d_echec();
    let avant_decision = &branche[..branche
        .find("fsm::consommation_flux(")
        .expect("le bras ne passe plus par fsm::consommation_flux")];

    assert!(
        avant_decision.contains("let octets_servis: Option<u64>"),
        "le compteur du bras de production doit rester un Option, pas un u64"
    );
    assert!(
        avant_decision.contains("None => None,"),
        "un now-playing sans stream_id doit donner None, jamais 0"
    );
    assert!(
        !avant_decision.contains("unwrap_or"),
        "le None de streamer_bytes_sent doit atteindre la décision intact : \
         un unwrap_or ici refait « inconnu » = « zéro »"
    );
}

#[test]
fn la_branche_inconnue_existe_et_ne_coupe_pas() {
    let branche = branche_du_seuil_d_echec();
    let debut = branche
        .find("} else if consommation == fsm::ConsommationFlux::Inconnue {")
        .expect("la branche « consommation inconnue » a disparu du bras de production");
    let inconnue = &branche[debut..];
    let fin = inconnue
        .find("\n                                } else {")
        .expect("la branche « inconnue » n'est plus suivie de la branche mesurée");
    let inconnue = &inconnue[..fin];

    assert!(
        !inconnue.contains("force_stop = true"),
        "une consommation inconnue ne doit pas couper la zone"
    );
    assert!(
        inconnue.contains("octets_servis_inconnus_zone_non_coupee"),
        "l'état inconnu doit être journalisé : invisible, il se reconfondra \
         avec zéro à la première occasion"
    );
}

#[test]
fn seule_la_branche_mesuree_coupe_la_zone() {
    let branche = branche_du_seuil_d_echec();
    assert_eq!(
        branche.matches("force_stop = true").count(),
        1,
        "une seule branche du seuil d'échec doit couper la zone"
    );
    assert!(
        branche.contains("decisions::demarrage_mort("),
        "le verdict « démarrage mort » doit rester armé pour un vrai zéro"
    );
    // Le `demarrage_mort` ne doit se lire QUE sous la branche mesurée : une
    // relance automatique Pause→Stop→Play déclenchée par un `0` d'ignorance
    // couperait le son d'une zone qui joue.
    let coupe = branche
        .find("force_stop = true")
        .expect("plus aucune coupure dans le bras du seuil d'échec");
    assert!(
        branche
            .find("decisions::demarrage_mort(")
            .is_some_and(|d| d > coupe),
        "demarrage_mort doit rester dans la branche qui coupe"
    );
}

// ─────────── 5. #4480 — trente TOURS ne font pas trente SECONDES ───────────
//
// Le même bras, une autre confusion : le seuil qui l'arme compte des tours de
// sondeur, et tout le code les commente « ~1 s ». Ils ne le sont pas.
//
// `poller.rs::spawn` attend
// `tokio::select! { ticker.tick(), TRACK_END_NOTIFY.notified() }` : chaque
// notification fait un tour hors cadence. Et `tokio::time::interval` rattrape
// par défaut les tours manqués EN RAFALE (`MissedTickBehavior::Burst`) : après
// un tour lent, plusieurs tombent coup sur coup.
//
// Le terrain le chiffre (Eversolo DMP-A8 du .18, 19/09/2026, journal cité dans
// l'issue) : `poller_track_generation_changed_resetting_state` à 09:04:05.921,
// `playback_failure_stopping_zone` à 09:04:24.853. Les trente tours tiennent
// donc dans **18,9 s au plus** — 0,63 s par tour. La ligne de coupure portait
// `wall_secs=18` et personne ne l'avait lue ainsi.
//
// ⛔ Ce que ces épreuves n'établissent PAS : que l'Eversolo se serait rétabli
// si on l'avait attendu jusqu'à trente secondes. Elles établissent que la
// patience annoncée par le seuil — « accommodate slow DLNA renderers […] that
// report Stopped/position=0 while buffering » — n'avait jamais lieu en entier.

#[test]
fn le_releve_du_terrain_ne_doit_plus_couper_4480() {
    use std::time::Duration;
    // 09:04:24.853 − 09:04:05.921 = 18,932 s pour trente tours.
    assert!(
        !tune_core::poller::fsm::arret_assez_long_pour_couper(
            Some(Duration::from_millis(18_932)),
            30,
        ),
        "dix-neuf secondes d'arrêt coupaient la zone alors que le seuil promet \
         une patience de trente (#4480)"
    );
}

#[test]
fn le_plancher_n_empeche_pas_une_vraie_coupure_4480() {
    use std::time::Duration;
    assert!(tune_core::poller::fsm::arret_assez_long_pour_couper(
        Some(Duration::from_secs(30)),
        30,
    ));
    assert!(tune_core::poller::fsm::arret_assez_long_pour_couper(
        Some(Duration::from_secs(120)),
        30,
    ));
    // Horloge jamais armée : on ne change rien au comportement d'avant plutôt
    // que de risquer une zone qui ne se couperait JAMAIS.
    assert!(tune_core::poller::fsm::arret_assez_long_pour_couper(
        None, 30
    ));
}

/// 🔴 La règle ci-dessus ne vaut que si le bras de production l'APPELLE.
///
/// « Écrit mais pas branché » : une décision juste que personne ne consulte
/// laisse le défaut entier. C'est la garde que le témoin d'origine (#2394)
/// posait déjà sur ce même bras, pour la même raison.
#[test]
fn le_bras_de_production_applique_le_plancher_de_temps_4480() {
    let branche = branche_du_seuil_d_echec();
    let entete = &branche[..branche
        .find("// Check if the stream is still being consumed")
        .unwrap_or(branche.len().min(600))];

    assert!(
        entete.contains("fsm::arret_assez_long_pour_couper("),
        "le seuil en ticks coupe de nouveau sans plancher de temps : trente \
         tours de sondeur peuvent tomber en dix-neuf secondes (#4480)"
    );
    assert!(
        entete.contains("STOPPED_FAILURE_MIN_SECS"),
        "le plancher doit être la constante, pas un littéral posé sur place"
    );
    assert!(
        entete.contains("ps.premier_arret_a"),
        "le plancher doit se mesurer depuis le DÉBUT de la série d'arrêts, \
         pas depuis le début de la piste (`wall_secs` inclut la lecture)"
    );
}

/// L'horloge de la série d'arrêts s'arme au passage de 0 à 1 tour, et nulle
/// part ailleurs : c'est ce qui permet à tout site qui repose
/// `stopped_ticks = 0` de la ré-armer sans le savoir.
#[test]
fn l_horloge_de_la_serie_s_arme_a_l_entree_4480() {
    let pose = SOURCE_POLLER
        .find("ps.premier_arret_a = Some(Instant::now());")
        .expect("l'horloge de la série d'arrêts n'est plus armée dans le sondeur");
    let avant = &SOURCE_POLLER[pose.saturating_sub(200)..pose];
    assert!(
        avant.contains("if ps.stopped_ticks == 0 {"),
        "l'horloge doit être posée à l'ENTRÉE de la série ; réarmée à chaque \
         tour, elle ne mesurerait jamais que le dernier"
    );
}
