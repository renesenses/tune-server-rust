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

/// Le source du sondeur, lu à la compilation — voir l'épreuve 4.
const SOURCE_POLLER: &str = include_str!("../src/poller.rs");

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
    let debut = SOURCE_POLLER
        .find("} else if ps.stopped_ticks >= STOPPED_FAILURE_THRESHOLD {")
        .expect("le bras du seuil d'échec a disparu de poller.rs");
    let fin = SOURCE_POLLER[debut..]
        .find("\"stopped_early_waiting\"")
        .expect("la branche suivante (stopped_early_waiting) a disparu de poller.rs");
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
