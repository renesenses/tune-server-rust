use super::PlaybackOrchestrator as O;

/// La parole de l'utilisateur prime sur le Sink — le terrain l'a exige.
///
/// L'Eversolo DMP-A8 annonce 392 formats dans son GetProtocolInfo, aucun
/// DSD — et JOUE le .dsf brut. La version precedente de cette regle cedait
/// devant ce « non » apparent et convertissait en PCM un flux que le
/// renderer savait lire. Un Sink qui omet un format n'est pas un refus.
#[test]
fn la_parole_de_lutilisateur_prime_sur_le_sink() {
    assert!(O::decider_passthrough_dsd("native", Some(false)));
}

/// La faute symetrique, celle qu'on ne veut PAS commettre en corrigeant :
/// un sondage muet n'est pas un refus. Le reglage explicite tient.
#[test]
fn native_tient_quand_le_sondage_ne_repond_pas() {
    assert!(O::decider_passthrough_dsd("native", None));
}

#[test]
fn native_tient_quand_le_renderer_confirme() {
    assert!(O::decider_passthrough_dsd("native", Some(true)));
}

/// `pcm` est un refus de l'utilisateur : aucune reponse du renderer ne le
/// renverse, pas meme un oui franc.
#[test]
fn pcm_refuse_quoi_que_reponde_le_renderer() {
    for annonce in [Some(true), Some(false), None] {
        assert!(
            !O::decider_passthrough_dsd("pcm", annonce),
            "pcm a laisse passer du DSD avec {annonce:?}"
        );
    }
}

/// `dop` non plus n'est pas du passthrough : le renderer doit recevoir le
/// DSD emballe en trames PCM, donc le fichier passe par la conversion.
#[test]
fn dop_n_est_pas_du_passthrough() {
    for annonce in [Some(true), Some(false), None] {
        assert!(
            !O::decider_passthrough_dsd("dop", annonce),
            "dop a laisse passer du DSD brut avec {annonce:?}"
        );
    }
}

/// `auto` suit le renderer, et sans reponse prend le chemin sur.
#[test]
fn auto_suit_le_sondage_et_se_replie_dans_le_doute() {
    assert!(O::decider_passthrough_dsd("auto", Some(true)));
    assert!(!O::decider_passthrough_dsd("auto", Some(false)));
    assert!(!O::decider_passthrough_dsd("auto", None));
}

/// Un mode inconnu en base (valeur ecrite par une version future, champ
/// vide) doit se comporter comme `auto`, pas envoyer du DSD au hasard.
#[test]
fn un_mode_inconnu_se_comporte_comme_auto() {
    assert!(!O::decider_passthrough_dsd("", None));
    assert!(
        !O::decider_passthrough_dsd("Native", None),
        "la casse compte"
    );
}

// ── Le message d'échec doit nommer le RÉGLAGE (#2396) ─────────────────
//
// Le choix d'envoyer quand même n'est pas remis en cause : « natif » est un
// réglage explicite et des renderers lisent le DSD sans l'annoncer. C'est
// le message d'ÉCHEC qui était faux. Il disait « Le renderer a acquitté
// Play mais joue toujours une autre source » — il accusait l'appareil,
// alors que le serveur savait AVANT d'envoyer que le Sink annonçait
// `Some(false)` et que la zone était en « natif ». L'utilisateur cherchait
// du côté du matériel ; l'un d'eux a réinstallé son système entier.

/// Le message tel que le renderer le renvoie — celui de #2396, mot pour mot.
const ECHEC_DLNA: &str = "Le renderer a acquitté Play mais joue toujours une \
     autre source (URI non appliquée après relance)";

#[test]
fn le_message_nomme_le_reglage_et_l_action_pas_l_appareil() {
    let msg = O::message_echec_sortie(ECHEC_DLNA, "native", Some(false), "application/x-dsd");

    assert!(
        msg.contains("natif"),
        "le message doit NOMMER le réglage en cause : {msg}"
    );
    assert!(
        msg.contains("DoP") && msg.contains("PCM"),
        "le message doit nommer l'action qui corrige (DoP ou PCM) : {msg}"
    );
    assert!(
        msg.contains("Output device error"),
        "le marqueur qui pilote le 503 côté route ne doit pas disparaître : {msg}"
    );
}

/// Zéro régression : hors de ce cas précis, le message ne change pas d'un
/// caractère. Accuser un réglage à tort serait la faute symétrique.
#[test]
fn les_autres_echecs_gardent_leur_message_mot_pour_mot() {
    let intact = format!("Output device error: {ECHEC_DLNA}");

    // Le renderer a dit OUI : l'échec ne vient pas du réglage.
    assert_eq!(
        O::message_echec_sortie(ECHEC_DLNA, "native", Some(true), "application/x-dsd"),
        intact
    );
    // Sondage muet : une absence n'est pas une preuve, on n'accuse rien.
    assert_eq!(
        O::message_echec_sortie(ECHEC_DLNA, "native", None, "application/x-dsd"),
        intact
    );
    // `auto` n'a forcé personne : le passthrough a suivi le renderer.
    assert_eq!(
        O::message_echec_sortie(ECHEC_DLNA, "auto", Some(false), "application/x-dsd"),
        intact
    );
    // Même zone, même appareil, un FLAC : le DSD n'y est pour rien. C'est
    // la preuve par contraste du ticket (FLAC OK / DSF muet à 3 min).
    assert_eq!(
        O::message_echec_sortie(ECHEC_DLNA, "native", Some(false), "audio/flac"),
        intact
    );
}

/// Les MIME que prend un DSD brut sur le fil : `application/x-dsd` par
/// défaut, ou celui que le renderer a annoncé (`audio/x-dsf`, `audio/dff`).
#[test]
fn tous_les_mime_dsd_bruts_declenchent_l_explication() {
    for mime in ["application/x-dsd", "audio/x-dsf", "audio/dff", "audio/dsf"] {
        let msg = O::message_echec_sortie(ECHEC_DLNA, "native", Some(false), mime);
        assert!(
            msg.contains("natif"),
            "le MIME {mime} est du DSD brut et n'a pas déclenché l'explication : {msg}"
        );
    }
}
