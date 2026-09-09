//! #3575 — « sortie locale ALSA imprenable pour toute la vie du processus ».
//!
//! Ce que le relevé de Belkadi Yacine établit, et que ces gardes tiennent :
//!
//! - le périphérique **était là** à chaque tour — dix `local_audio_devices_-
//!   enumerated count=6` en 43 minutes, DENAFRIPS retrouvé chaque fois. Ce
//!   n'est donc ni un renommage ni une identité perdue ;
//! - **13 ouvertures instrumentées, 13 fois la même corrélation, zéro
//!   contre-exemple** : les **dix** échecs portent `device_default_sr=None`,
//!   les **trois** réussites une cadence par défaut réellement lue. La sonde
//!   `default_output_config()` avait déjà pris le refus sur le MÊME PCM, et son
//!   erreur était jetée par un `.ok()` ;
//! - le processus SAIN rejoue l'échec à 12:14:38 — 2,7 s après avoir ouvert
//!   `alsa:hw:CARD=2,DEV=0` — puis réussit à 12:15:00. Un recouvrement, pas un
//!   appareil mort.
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{
    BUDGET_RELACHE_PERIPHERIQUE_MS, OpenFailure, PALIER_RELACHE_PERIPHERIQUE_MS,
    RelacheDuPeripherique, SentinelleDuFilDeLecture, classify_open_failure,
    decider_la_relache_du_peripherique,
};

/// La phrase EXACTE que porte le journal de Yacine, quatre fois.
///
/// `cpal-0.17.3/src/host/alsa/mod.rs:358-363` replie `ENOENT`, `EPERM`,
/// `ENODEV`, `ENOTSUPP`, `EBUSY` et `EAGAIN` sur ce seul message.
const PHRASE_DE_REPLI_DE_CPAL: &str =
    "The requested device is no longer available. For example, it has been unplugged.";

#[test]
fn la_phrase_que_cpal_substitue_a_l_errno_ne_tombe_plus_dans_unknown() {
    assert_eq!(
        classify_open_failure(PHRASE_DE_REPLI_DE_CPAL),
        OpenFailure::IndisponibleMotifPerdu,
        "c'est le message que portent les QUATRE échecs du relevé : classé \
         `Unknown`, il faisait dire à l'écran « le périphérique a refusé tous \
         les formats proposés » pour un périphérique jamais ouvert"
    );
}

#[test]
fn le_message_d_indisponibilite_n_accuse_pas_le_format() {
    let m = OpenFailure::IndisponibleMotifPerdu.user_message();
    assert!(
        !m.contains("format"),
        "changer de format ne rouvre pas un PCM tenu ou débranché — c'est \
         précisément le conseil qui a fait chercher Yacine pendant 1 h 27 : {m}"
    );
    assert!(
        m.contains("Vérifiez"),
        "le message doit dire quoi FAIRE, pas seulement ce qui a échoué : {m}"
    );
}

#[test]
fn le_renseignement_de_journal_nomme_les_deux_branches_possibles() {
    let h = OpenFailure::IndisponibleMotifPerdu.log_hint();
    assert!(
        h.contains("EBUSY") && h.contains("ENOENT"),
        "le journal doit dire que l'errno a été DÉTRUIT par cpal, sans quoi la \
         prochaine instruction repartira chercher un appareil débranché qui ne \
         l'était pas : {h}"
    );
}

// ---------------------------------------------------------------------------
// La décision d'attente
// ---------------------------------------------------------------------------

#[test]
fn un_peripherique_que_personne_ne_tient_s_ouvre_sans_attendre() {
    assert_eq!(
        decider_la_relache_du_peripherique(false, 0, BUDGET_RELACHE_PERIPHERIQUE_MS),
        RelacheDuPeripherique::Libre,
        "le cas NOMINAL — la quasi-totalité des lectures. Y ajouter une \
         attente retarderait chaque piste de tout le monde pour le défaut d'un \
         seul"
    );
}

#[test]
fn un_fil_precedent_encore_vivant_fait_attendre_par_paliers() {
    assert_eq!(
        decider_la_relache_du_peripherique(true, 0, BUDGET_RELACHE_PERIPHERIQUE_MS),
        RelacheDuPeripherique::Attendre {
            apres_ms: PALIER_RELACHE_PERIPHERIQUE_MS
        },
        "`stop()` vient peut-être de DÉTACHER ce fil sans qu'il ait rendu le \
         flux cpal : le PCM `hw:` est exclusif, ouvrir maintenant rend EBUSY"
    );
}

#[test]
fn le_dernier_palier_ne_deborde_pas_du_budget() {
    // 20 ms restants : le palier de 50 ms doit être rogné, sinon l'attente
    // dépasse le budget annoncé et le silence dure plus longtemps que promis.
    assert_eq!(
        decider_la_relache_du_peripherique(
            true,
            BUDGET_RELACHE_PERIPHERIQUE_MS - 20,
            BUDGET_RELACHE_PERIPHERIQUE_MS
        ),
        RelacheDuPeripherique::Attendre { apres_ms: 20 }
    );
}

#[test]
fn le_budget_epuise_ouvre_quand_meme_et_le_dit() {
    assert_eq!(
        decider_la_relache_du_peripherique(
            true,
            BUDGET_RELACHE_PERIPHERIQUE_MS,
            BUDGET_RELACHE_PERIPHERIQUE_MS
        ),
        RelacheDuPeripherique::ForcerEtLeDire,
        "attendre indéfiniment remplacerait une panne d'ouverture par une \
         panne d'attente : un fil bloqué sur une lecture réseau peut tenir des \
         minutes (`streaming_decode_send_timeout timeout_secs=300`)"
    );
    assert_eq!(
        decider_la_relache_du_peripherique(true, 99_000, BUDGET_RELACHE_PERIPHERIQUE_MS),
        RelacheDuPeripherique::ForcerEtLeDire
    );
}

#[test]
fn le_budget_reste_court_devant_l_attente_deja_consentie_par_stop() {
    // `stop()` accepte 2 000 ms avant de détacher. Le budget d'ici s'y AJOUTE :
    // au-delà d'une seconde et demie, l'auditeur qui vient d'appuyer sur
    // Lecture croit que Tune ne répond plus.
    assert!(
        BUDGET_RELACHE_PERIPHERIQUE_MS <= 2_000,
        "budget trop long : {BUDGET_RELACHE_PERIPHERIQUE_MS} ms s'ajoutent aux \
         2 000 ms de `stop()`"
    );
    assert!(BUDGET_RELACHE_PERIPHERIQUE_MS >= PALIER_RELACHE_PERIPHERIQUE_MS);
}

// ---------------------------------------------------------------------------
// La sentinelle — l'ORDRE est tout le contrat
// ---------------------------------------------------------------------------

/// Tient lieu du flux cpal : c'est SA destruction qui ferme le PCM.
struct FauxFluxCpal {
    temoin: Arc<Mutex<Vec<&'static str>>>,
    sentinelle: Arc<AtomicBool>,
}

impl Drop for FauxFluxCpal {
    fn drop(&mut self) {
        // Instant où le PCM se referme réellement. À cet instant la sentinelle
        // doit encore annoncer « vivant » : sinon un `play_url` concurrent
        // aurait déjà été autorisé à rouvrir un périphérique encore tenu.
        self.temoin
            .lock()
            .unwrap()
            .push(if self.sentinelle.load(Ordering::SeqCst) {
                "pcm_ferme_sentinelle_encore_vivante"
            } else {
                "pcm_ferme_sentinelle_DEJA_RETOMBEE"
            });
    }
}

#[test]
fn la_sentinelle_ne_retombe_qu_apres_la_fermeture_du_pcm() {
    let vivant = Arc::new(AtomicBool::new(true));
    let temoin = Arc::new(Mutex::new(Vec::new()));
    {
        // Déclarée EN PREMIER, comme dans le fil de lecture : Rust détruit en
        // ordre INVERSE de déclaration, donc elle part en DERNIER.
        let _sentinelle = SentinelleDuFilDeLecture(vivant.clone());
        let _flux = FauxFluxCpal {
            temoin: temoin.clone(),
            sentinelle: vivant.clone(),
        };
        assert!(
            vivant.load(Ordering::SeqCst),
            "tant que le fil vit, il tient le périphérique"
        );
    }
    assert_eq!(
        temoin.lock().unwrap().as_slice(),
        ["pcm_ferme_sentinelle_encore_vivante"],
        "la sentinelle a retombé AVANT la fermeture du PCM : elle annoncerait \
         libre un périphérique encore tenu, ce qui est exactement le défaut \
         qu'elle est censée empêcher"
    );
    assert!(
        !vivant.load(Ordering::SeqCst),
        "une fois le flux rendu, la sentinelle doit retomber — sinon toute \
         lecture suivante attendrait le budget entier pour rien"
    );
}

#[test]
fn la_sentinelle_retombe_aussi_quand_le_fil_sort_avant_d_ouvrir() {
    // Le fil rend la main sur `local_audio_http_fetch_failed` sans avoir
    // jamais ouvert de périphérique : la sentinelle doit retomber quand même,
    // sans quoi la piste suivante attend un flux qui n'a jamais existé.
    let vivant = Arc::new(AtomicBool::new(true));
    {
        let _sentinelle = SentinelleDuFilDeLecture(vivant.clone());
    }
    assert!(!vivant.load(Ordering::SeqCst));
}

// ---------------------------------------------------------------------------
// Le branchement de la trace qui manquait
// ---------------------------------------------------------------------------

/// La partie de `local.rs` qui est du code de PRODUCTION, modules d'épreuves
/// exclus — même découpe que `repli_format_compresse_i3618`.
fn code_de_production() -> &'static str {
    const TOUT: &str = include_str!("../local.rs");
    const BORNE: &str = "mod relache_peripherique_i3575";
    let fin = TOUT
        .find(BORNE)
        .unwrap_or_else(|| panic!("ce module a été renommé : la découpe ne protège plus rien"));
    &TOUT[..fin]
}

/// `device_default_sr=None` est la SEULE trace que le relevé de Yacine porte
/// sur les dix échecs, et elle ne disait rien : `.ok()` jetait l'erreur de
/// la sonde. Sur ALSA cette sonde ouvre le MÊME PCM que la lecture — son échec
/// EST le premier refus, quelques millisecondes avant celui qui coupe la zone.
///
/// Aucune API ne rend cette ligne : c'est un `warn!`. La garde porte donc sur
/// son BRANCHEMENT, comme celle de #3618 sur `open_failure.lock()`.
#[test]
fn la_sonde_de_cadence_par_defaut_ne_jette_plus_son_erreur() {
    let code = code_de_production();
    assert!(
        !code.contains("default_output_config().ok()"),
        "l'erreur de la sonde est de nouveau jetée par `.ok()` : \
         `device_default_sr=None` redevient une absence muette, et c'est \
         exactement ce qui a rendu #3575 inintelligible pendant deux jours"
    );
    assert_eq!(
        code.matches("\"local_audio_default_config_probe_failed\"")
            .count(),
        2,
        "les DEUX sondes — chemin PCM et chemin compressé — doivent nommer \
         leur échec : le relevé de Yacine porte les deux marqueurs \
         (`audio_stream_build_failed_all_formats` ET \
         `audio_stream_build_failed_compressed`)"
    );
}

/// La règle est branchée : le chemin qui ouvre le périphérique CONSULTE
/// réellement la décision, au lieu de dormir 50 ms en espérant.
///
/// ⚠️ Compter est indispensable, et la contre-épreuve l'a prouvé : la
/// DÉFINITION `pub(crate) fn decider_la_relache_du_peripherique(` contient
/// elle-même le motif cherché. Une garde qui se contentait de `contains`
/// restait VERTE alors que l'attente venait d'être entièrement débranchée —
/// elle attestait l'existence du code, pas son branchement, c'est-à-dire
/// exactement le défaut « écrit mais pas branché » qu'elle prétendait tenir.
#[test]
fn l_ouverture_consulte_reellement_la_decision_de_relache() {
    let code = code_de_production();
    assert!(
        code.matches("decider_la_relache_du_peripherique(").count() >= 2,
        "une seule occurrence = la définition seule : plus personne n'APPELLE \
         la décision, et `play_url` rouvre un PCM exclusif que son propre fil \
         précédent tient peut-être encore (#3575)"
    );
    assert!(
        code.matches("SentinelleDuFilDeLecture(").count() >= 2,
        "une seule occurrence = la définition seule : la sentinelle n'est plus \
         ARMÉE dans le fil de lecture, la décision n'a donc aucune observation \
         à lire et répond toujours `Libre`"
    );
    // Ces deux littéraux ne vivent QU'au point de branchement : ils survivent
    // à la compilation et discriminent donc deux binaires, contrairement à un
    // nom de fonction que l'édition de liens efface.
    for marqueur in [
        "\"local_audio_peripherique_encore_tenu_par_le_fil_precedent\"",
        "\"local_audio_ouverture_forcee_le_fil_precedent_tient_encore\"",
    ] {
        assert!(
            code.contains(marqueur),
            "le point de branchement a disparu : {marqueur} n'est plus émis, \
             donc le prochain relevé de terrain ne pourra pas dire si Tune a \
             attendu son propre fil ou non"
        );
    }
}

/// Le détachement du fil précédent doit se DIRE, au niveau que les relevés de
/// terrain rapportent.
///
/// Les exports de journaux de Belkadi Yacine (tickets 87 et 92, 1 402 lignes)
/// ne portent que de l'INFO et au-dessus : 854 INFO, 59 WARN, 2 ERROR, **zéro
/// DEBUG**. `local_audio_stop_thread_detached` était en `debug!` — il ne
/// pouvait donc PAS y figurer, et son absence ne prouvait rien. C'est pourtant
/// la ligne qui départage « le PCM est tenu par notre propre fil » de « le DAC
/// a disparu ».
#[test]
fn le_detachement_du_fil_precedent_est_dit_a_voix_haute() {
    let code = code_de_production();
    // `rfind` sur le LITTÉRAL entre guillemets : le nom apparaît aussi dans
    // la documentation de `BUDGET_RELACHE_PERIPHERIQUE_MS`, et un `find` nu
    // mesurait ce commentaire-là au lieu du point d'émission — la garde
    // échouait alors sur du code juste, ce qui est le pire des deux défauts.
    let i = code
        .rfind("\"local_audio_stop_thread_detached")
        .expect("le marqueur du détachement a disparu : plus rien ne dit que `stop()` a renoncé");
    let avant = &code[i.saturating_sub(300)..i];
    assert!(
        avant.contains("warn!("),
        "le détachement est redescendu sous `warn!` : il redevient invisible \
         de tout export de terrain, et la prochaine instruction repartira \
         d'une absence qu'elle prendra pour une preuve"
    );
}
