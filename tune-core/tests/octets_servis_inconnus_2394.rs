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
        // #4480 : aucune avance d'audio à opposer — l'état d'AVANT ce
        // correctif, celui où `ASec` coupe.
        avance_audio_couvre_l_arret: false,
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

// ──── 6. #4480 — une socket à sec n'est pas un RENDERER à sec ─────────────
//
// Le même bras, la cause de fond cette fois. `ConsommationFlux::ASec` ne dit
// qu'une chose : **le compteur d'octets de la socket n'a pas bougé depuis le
// tour précédent**. Sur une livraison HTTP à contre-pression, c'est l'état
// NORMAL d'un renderer qui a tamponné loin devant : il ne tire plus parce
// qu'il n'a plus de place, pas parce qu'il manque de son.
//
// Le journal du .18 (19/09/2026, Eversolo DMP-A8, zone 10) :
//
// ```text
// playback_failure_stopping_zone zone_id=10 peak_pos=23000 track_dur=558207
//                                wall_secs=18 bytes_sent=34406444
//                                consommation="a_sec"
// ```
//
// 34 406 444 octets de WAV 44,1 kHz / 16 bits / stéréo, c'est 176 400 o/s,
// donc **195 s d'audio livrées** pour une position annoncée de **23 s**. Le
// renderer avait 172 s de musique dans le ventre. Tune coupait la zone.
//
// Le sondeur n'avait pas le facteur qui rend ce calcul possible : le **débit
// nominal** de la session. `StreamInfo::debit_nominal_octets_par_seconde` le
// donne, `AudioStreamer::stream_audio_servi_ms` convertit, et la décision se
// prend enfin entre deux grandeurs de même nature.
//
// ⚠️ Décision de SÛRETÉ : une zone réellement morte ne doit pas rester
// ouverte indéfiniment. La patience vaut donc `min(avance, borne_haute)` et
// la borne haute est mesurée ici comme le reste — sans elle, un renderer qui
// aurait tamponné une heure garderait sa zone une heure.

use tune_core::http::streamer::{AudioStreamer, StreamInfo};
use tune_core::poller::fsm::{avance_audio_ms, famine_etablie_malgre_l_avance};

/// Le relevé du journal, chiffre pour chiffre.
const OCTETS_SERVIS_DU_JOURNAL: u64 = 34_406_444;
const POSITION_ANNONCEE_MS: u64 = 23_000;
const DUREE_DE_LA_PISTE_MS: u64 = 558_207;
/// 09:04:24.853 − 09:04:05.921 : les trente tours d'arrêt de l'incident.
const ARRET_DU_JOURNAL: std::time::Duration = std::time::Duration::from_millis(18_932);
/// La borne haute retenue par le sondeur (`AVANCE_AUDIO_BORNE_HAUTE_SECS`).
const BORNE_HAUTE_SECS: u64 = 120;

/// La session-canal WAV de l'incident : AAC local transcodé en 44,1/16 par le
/// relais DSP. `file_size: None` — c'est un canal, pas un fichier.
fn session_du_journal() -> StreamInfo {
    StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 44_100,
        bit_depth: 16,
        channels: 2,
        file_size: None,
        duration_ms: Some(DUREE_DE_LA_PISTE_MS),
        ..Default::default()
    }
}

#[test]
fn le_debit_nominal_de_la_session_du_journal_4480() {
    // 44 100 × 2 canaux × 2 octets = 176 400 o/s. La taille totale est
    // inconnue (`file_size: None`) : sans la formule PCM, il n'y aurait
    // AUCUN débit nominal pour cette session — et c'est justement celle de
    // l'incident.
    assert_eq!(
        session_du_journal().debit_nominal_octets_par_seconde(),
        Some(176_400)
    );
}

/// 🔴 Le témoin du cas de terrain : 195 s livrées, position 23 s, zone NON
/// coupée.
#[test]
fn le_cas_du_journal_ne_coupe_plus_la_zone_4480() {
    let nominal = session_du_journal()
        .debit_nominal_octets_par_seconde()
        .expect("le débit nominal d'une session WAV est calculable");
    let audio_servi_ms = OCTETS_SERVIS_DU_JOURNAL * 1_000 / nominal;
    assert_eq!(
        audio_servi_ms, 195_047,
        "195 s d'audio, pas 34 Mo abstraits"
    );

    let avance = avance_audio_ms(Some(audio_servi_ms), POSITION_ANNONCEE_MS);
    assert_eq!(avance, Some(172_047), "172 s d'audio devant la position");

    assert!(
        !famine_etablie_malgre_l_avance(avance, Some(ARRET_DU_JOURNAL), BORNE_HAUTE_SECS),
        "la zone du journal est coupée alors que 195 s d'audio ont été \
         livrées pour une position de 23 s (#4480)"
    );

    // Et la décision qui en découle, dans l'arbre du bras `Stopped`.
    let mut i = au_seuil_d_echec(ConsommationFlux::ASec);
    i.avance_audio_couvre_l_arret = true;
    let issue = classify_stopped(&i);
    assert_eq!(issue, StoppedOutcome::FailureWaitingAvance);
    assert!(
        !issue.is_force_stop(),
        "une socket à sec devant un renderer qui a de quoi jouer ne coupe pas"
    );
}

/// 🔴 LA CONTRE-PARTIE, sans laquelle le correctif serait dangereux : une
/// zone réellement à sec EST coupée, et dans la borne annoncée.
#[test]
fn une_zone_reellement_a_sec_est_coupee_4480() {
    let nominal = 176_400_u64;

    // (a) Le renderer a reçu exactement ce qu'il a joué — rien devant lui.
    let octets_servis = POSITION_ANNONCEE_MS * nominal / 1_000;
    let servi_ms = octets_servis * 1_000 / nominal;
    let avance = avance_audio_ms(Some(servi_ms), POSITION_ANNONCEE_MS);
    assert_eq!(avance, None, "aucune avance : la famine est réelle");
    assert!(
        famine_etablie_malgre_l_avance(avance, Some(ARRET_DU_JOURNAL), BORNE_HAUTE_SECS),
        "une zone sans une seconde d'audio d'avance doit rester coupée"
    );

    // (b) Le renderer est derrière ce qu'on lui a servi de moins d'une
    // seconde : pas de quoi surseoir.
    assert!(famine_etablie_malgre_l_avance(
        avance_audio_ms(Some(POSITION_ANNONCEE_MS + 999), POSITION_ANNONCEE_MS),
        Some(ARRET_DU_JOURNAL),
        BORNE_HAUTE_SECS,
    ));

    // (c) Débit nominal inconnu — on ne devine pas : comportement d'avant.
    assert!(famine_etablie_malgre_l_avance(
        avance_audio_ms(None, POSITION_ANNONCEE_MS),
        Some(ARRET_DU_JOURNAL),
        BORNE_HAUTE_SECS,
    ));

    // (d) L'horloge de la série d'arrêts n'a jamais été armée : idem.
    assert!(famine_etablie_malgre_l_avance(
        Some(172_047),
        None,
        BORNE_HAUTE_SECS,
    ));

    // (e) Et l'issue qui en découle coupe bien.
    let issue = classify_stopped(&au_seuil_d_echec(ConsommationFlux::ASec));
    assert_eq!(issue, StoppedOutcome::FailureStop);
    assert!(issue.is_force_stop());
}

/// 🔴 La BORNE HAUTE : même avec une avance énorme, la zone finit coupée.
#[test]
fn la_borne_haute_finit_par_couper_4480() {
    // Le cas du journal lui-même : 172 s d'avance, bornées à 120.
    let avance = Some(172_047);
    assert!(
        !famine_etablie_malgre_l_avance(
            avance,
            Some(std::time::Duration::from_secs(BORNE_HAUTE_SECS - 1)),
            BORNE_HAUTE_SECS,
        ),
        "avant la borne, on attend encore"
    );
    assert!(
        famine_etablie_malgre_l_avance(
            avance,
            Some(std::time::Duration::from_secs(BORNE_HAUTE_SECS)),
            BORNE_HAUTE_SECS,
        ),
        "à la borne haute, la zone est coupée : une zone morte ne reste pas \
         ouverte indéfiniment (#4480)"
    );

    // Une heure d'audio tamponnée ne donne pas une heure de sursis.
    assert!(famine_etablie_malgre_l_avance(
        Some(3_600_000),
        Some(std::time::Duration::from_secs(BORNE_HAUTE_SECS)),
        BORNE_HAUTE_SECS,
    ));

    // En deçà de la borne, c'est l'avance qui commande : 40 s d'avance ne
    // donnent pas 120 s de patience.
    assert!(famine_etablie_malgre_l_avance(
        Some(40_000),
        Some(std::time::Duration::from_secs(40)),
        BORNE_HAUTE_SECS,
    ));
    assert!(!famine_etablie_malgre_l_avance(
        Some(40_000),
        Some(std::time::Duration::from_secs(39)),
        BORNE_HAUTE_SECS,
    ));
}

/// Le débit nominal ne s'invente pas : un conteneur COMPRESSÉ ne passe jamais
/// par la formule PCM, qui donnerait le débit du flux décodé — bien plus
/// élevé — et surestimerait l'audio livrée. Il se lit
/// `octets_total / durée`, la définition du projet (#4645).
#[test]
fn le_debit_nominal_ne_s_invente_pas_4480() {
    let flac_mesurable = StreamInfo {
        format: "flac".into(),
        mime_type: "audio/flac".into(),
        sample_rate: 44_100,
        bit_depth: 16,
        channels: 2,
        // 100 s à 100 000 o/s : moitié moins que le PCM équivalent.
        file_size: Some(10_000_000),
        duration_ms: Some(100_000),
        ..Default::default()
    };
    assert_eq!(
        flac_mesurable.debit_nominal_octets_par_seconde(),
        Some(100_000),
        "un FLAC se lit à son nominal réel, jamais au débit du PCM décodé"
    );

    let flac_sans_taille = StreamInfo {
        file_size: None,
        ..flac_mesurable.clone()
    };
    assert_eq!(
        flac_sans_taille.debit_nominal_octets_par_seconde(),
        None,
        "sans taille ni conteneur PCM, on ne conclut RIEN — on ne devine pas"
    );

    let wav_sans_format = StreamInfo {
        format: String::new(),
        mime_type: String::new(),
        sample_rate: 0,
        ..flac_mesurable
    };
    assert_eq!(
        wav_sans_format.debit_nominal_octets_par_seconde(),
        Some(100_000),
        "une session muette sur son format retombe sur taille/durée"
    );
}

/// Le chemin réel, de bout en bout : une session vivante du gestionnaire de
/// flux, son compteur d'octets, et les millisecondes d'audio qu'il vaut.
#[tokio::test]
async fn le_gestionnaire_de_flux_convertit_ses_octets_en_audio_4480() {
    let streamer = AudioStreamer::new(0);
    let (id, _tx, _pret) = streamer
        .create_session(session_du_journal(), false, 8)
        .await;

    // Session neuve : rien de servi, donc zéro milliseconde — et surtout pas
    // `None`, qui voudrait dire « je ne sais pas ».
    assert_eq!(streamer.stream_audio_servi_ms(&id).await, Some(0));

    {
        let sessions = streamer.sessions_state();
        let sessions = sessions.lock().await;
        sessions
            .get(&id)
            .expect("la session vient d'être créée")
            .bytes_sent
            .store(
                OCTETS_SERVIS_DU_JOURNAL,
                std::sync::atomic::Ordering::Relaxed,
            );
    }

    assert_eq!(
        streamer.stream_audio_servi_ms(&id).await,
        Some(195_047),
        "34 406 444 octets de WAV 44,1/16 valent 195 s d'audio"
    );

    // Une session inconnue reste une ignorance, jamais un zéro (#2394).
    assert_eq!(
        streamer.stream_audio_servi_ms("session-fantome").await,
        None
    );
}

/// 🔴 « Écrit mais pas branché » : la règle ne vaut que si le bras de
/// production l'APPELLE — avec la bonne position, la bonne borne, et le débit
/// tiré du gestionnaire de flux.
#[test]
fn le_bras_de_production_compare_l_audio_livree_a_la_position_4480() {
    let branche = branche_du_seuil_d_echec();

    assert!(
        branche.contains("fsm::famine_etablie_malgre_l_avance("),
        "le bras coupe de nouveau sur le seul compteur d'octets, sans le \
         convertir en secondes d'audio (#4480)"
    );
    assert!(
        branche.contains("streamer_audio_servi_ms("),
        "sans le débit nominal remonté du flux, la décision reste aveugle"
    );
    assert!(
        branche.contains("fsm::avance_audio_ms(audio_servi_ms, ps.peak_position_ms)"),
        "l'avance se mesure contre le PIC de position : la plus grande des \
         positions, donc la plus petite des avances"
    );
    assert!(
        branche.contains("AVANCE_AUDIO_BORNE_HAUTE_SECS"),
        "la borne haute doit être la constante documentée, pas un littéral \
         posé sur place — c'est une décision de sûreté"
    );
    assert!(
        branche.contains("ps.premier_arret_a.map(|t| t.elapsed())"),
        "la patience se compte depuis le DÉBUT de la série d'arrêts"
    );
    assert!(
        branche.contains("fsm_in.avance_audio_couvre_l_arret = !famine_etablie;"),
        "le verdict doit atteindre l'entrée de `classify_stopped`, sinon \
         l'arbre en ombre diverge du bras à chaque épargne"
    );

    // 🔴 Calculer ne suffit pas : il faut BRANCHER. Un `famine_etablie`
    // calculé puis ignoré laisserait passer toutes les épreuves de décision
    // ci-dessus — c'est exactement le piège « écrit mais pas branché ». On
    // exige donc la branche d'épargne, et qu'elle précède celle qui coupe.
    let epargne = branche.find("} else if !famine_etablie {").expect(
        "le bras calcule la famine mais ne s'en sert pas : aucune branche \
             d'épargne ne vient avant la coupure (#4480)",
    );
    let attente = branche
        .find("fsm_actual = Some(fsm::StoppedOutcome::FailureWaitingAvance);")
        .expect("l'épargne doit porter son issue, pas se contenter d'un journal");
    let coupure_issue = branche
        .find("fsm_actual = Some(fsm::StoppedOutcome::FailureStop);")
        .expect("la branche qui coupe a disparu du bras");
    assert!(
        epargne < coupure_issue && attente < coupure_issue,
        "l'épargne doit passer AVANT la coupure : après elle, elle ne \
         garderait plus rien"
    );

    // Et la coupure elle-même dit désormais ce qu'elle a mesuré. Les deux
    // champs sont cherchés d'abord, PUIS situés : « A avant B » par
    // comparaison d'indices est vrai gratuitement quand A est absent.
    let marqueur = branche
        .find("\"playback_failure_stopping_zone\"")
        .expect("la ligne de coupure a disparu du bras");
    for champ in [
        "audio_servi_ms = audio_servi_ms.unwrap_or(0)",
        "avance_ms = avance_audio_ms.unwrap_or(0)",
    ] {
        let pose = branche.find(champ).unwrap_or_else(|| {
            panic!(
                "la ligne de coupure ne porte pas `{champ}` : sans l'audio \
                 livrée ni l'avance, l'incident suivant se relira aussi mal \
                 que celui-ci (#4480)"
            )
        });
        assert!(
            pose < marqueur,
            "`{champ}` doit être un champ de la ligne de coupure, pas du \
             texte posé après elle"
        );
    }
}
