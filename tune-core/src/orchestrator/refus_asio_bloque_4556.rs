//! #4556 — le refus de lecture ne doit plus accuser le câble quand c'est le
//! coupe-circuit ASIO qui a vidé le parc.
//!
//! Marco Polo (fil 1852) : DAC SMSL SU-1 refusé en `zone_output_unavailable`
//! « Vérifiez qu'elle est branchée et allumée », alors que Windows l'utilise
//! sans problème et que le témoin `blocked_after_crash` de sa machine (relevé
//! dans le `diagnostic.md` de #4412) interdisait tout balayage ASIO.

use super::transport::refus_de_zone_hors_ligne;
use crate::outputs::asio_blocage_4556::{
    BlocageAsio, CODE_APRES_PLANTAGE, CODE_PAR_ENVIRONNEMENT, MotifDeBlocage, depuis_sentinelle,
};

const TEMOIN: &str = r"C:\Users\Marco\AppData\Local\TuneServer\asio-warm.pending";

fn blocage_apres_plantage() -> BlocageAsio {
    BlocageAsio {
        motif: MotifDeBlocage::ApresPlantage,
        temoin: Some(TEMOIN.to_string()),
    }
}

/// 🔴 TÉMOIN — le refus ORDINAIRE, qui doit rester intact partout ailleurs.
///
/// Sans blocage mesuré, la phrase est celle de #4580 / #4601 : la cause
/// probable nommée, et surtout « inutile de supprimer la zone ». C'est la
/// bonne réponse quand l'absence a vraiment été mesurée (parc énuméré, DAC
/// débranché ou tenu par une autre application).
#[test]
fn sans_blocage_le_refus_reste_celui_de_4580() {
    let (msg, code) = refus_de_zone_hors_ligne("local:audio-gd USB audio", "Salon", &[], None);
    assert!(msg.starts_with("zone_output_unavailable:"), "{msg}");
    assert!(msg.contains("audio-gd USB audio"), "{msg}");
    assert!(
        msg.contains("Vérifiez qu'elle est branchée et allumée"),
        "{msg}"
    );
    assert!(
        msg.contains("Inutile de supprimer la zone"),
        "la phrase de #4580 doit survivre à la fusion des deux lots : {msg}"
    );
    assert_eq!(code, None, "aucun motif : il n'y a rien de plus à dire");
    assert_eq!(
        depuis_sentinelle(&msg),
        None,
        "l'ancienne sentinelle ne doit pas être relue comme un blocage ASIO"
    );

    // La troisième cause de #4580 : sortie réseau jamais revue.
    let (reseau, code) = refus_de_zone_hors_ligne("dlna-abcdef", "Salon", &[], None);
    assert!(reseau.starts_with("zone_output_unavailable:"), "{reseau}");
    assert!(
        reseau.contains("ne s'est pas annoncé sur le réseau"),
        "{reseau}"
    );
    assert!(reseau.contains("Inutile de supprimer la zone"), "{reseau}");
    assert_eq!(code, None);

    // Et la première : l'appareil est là, renommé par son pilote.
    let vivants = vec!["audio-gd USB audio (44,1 kHz)".to_string()];
    let (renomme, code) =
        refus_de_zone_hors_ligne("local:audio-gd USB audio", "Salon", &vivants, None);
    assert!(
        renomme.contains("audio-gd USB audio (44,1 kHz)"),
        "{renomme}"
    );
    assert!(
        renomme.contains("Inutile de supprimer la zone"),
        "{renomme}"
    );
    assert_eq!(code, None);
}

/// 🟢 LE CORRECTIF — le refus dit que le serveur n'a pas regardé, nomme le
/// témoin, donne le geste, et porte un code exploitable par le client.
#[test]
fn un_parc_de_repli_apres_plantage_nomme_le_temoin_et_le_geste() {
    let blocage = blocage_apres_plantage();
    // Un homonyme renommé est présent : la piste « appareil revenu sous un
    // autre nom » de #4580 serait servie SANS le coupe-circuit. Sous blocage,
    // elle serait un mensonge — aucune énumération ASIO n'a eu lieu.
    let vivants = vec!["USB DAC ASIO (44,1 kHz)".to_string()];
    let (msg, code) = refus_de_zone_hors_ligne(
        "local:USB DAC ASIO",
        "USB DAC ASIO",
        &vivants,
        Some(&blocage),
    );

    assert_eq!(code, Some(CODE_APRES_PLANTAGE));
    let (code_relu, phrase) = depuis_sentinelle(&msg).expect("sentinelle #4556 relisible");
    assert_eq!(code_relu, CODE_APRES_PLANTAGE);

    assert!(
        phrase.contains("USB DAC ASIO"),
        "l'appareil doit rester nommé (#3737) : {phrase}"
    );
    assert!(
        phrase.contains(TEMOIN),
        "le témoin doit être nommé, c'est la pièce qui répare à la main : {phrase}"
    );
    assert!(
        phrase.contains("Réarmez le balayage ASIO"),
        "le geste qui débloque doit être écrit : {phrase}"
    );
    assert!(
        !phrase.contains("branchée et allumée"),
        "le refus ne doit plus désigner le câble d'un DAC que Windows utilise : {phrase}"
    );
    assert!(
        !phrase.contains("Inutile de supprimer la zone"),
        "sous coupe-circuit, les causes de #4580 ne sont pas mesurées : ne pas les \
         servir (fusion des deux lots) : {phrase}"
    );
    assert!(
        !phrase.contains("(44,1 kHz)"),
        "nommer un candidat renommé sans avoir énuméré ASIO serait un pari : {phrase}"
    );
    assert!(
        !msg.starts_with("zone_output_unavailable:"),
        "la nouvelle sentinelle doit être DISJOINTE de l'ancienne, sinon la route \
         HTTP la traite comme un refus ordinaire et le code se perd : {msg}"
    );
}

/// La coupure par l'environnement n'est pas réarmable : pas de bouton.
#[test]
fn le_blocage_par_environnement_donne_un_autre_code() {
    let blocage = BlocageAsio {
        motif: MotifDeBlocage::ParEnvironnement,
        temoin: None,
    };
    // `local:` sans nom exploitable : la branche « sans appareil nommé » de
    // `message_fr`. On reste sur une zone LOCALE — c'est la seule famille pour
    // laquelle le site d'appel consulte le coupe-circuit.
    let (msg, code) = refus_de_zone_hors_ligne("local:   ", "Salon", &[], Some(&blocage));
    assert_eq!(code, Some(CODE_PAR_ENVIRONNEMENT));
    let (_, phrase) = depuis_sentinelle(&msg).unwrap();
    assert!(phrase.contains("TUNE_DISABLE_ASIO_SCAN"), "{phrase}");
    assert!(!phrase.contains("Réarmez"), "{phrase}");
}

/// 🔴 GARDE DU SITE D'APPEL — la fonction pure ci-dessus resterait verte si
/// `gate_or_rebind_offline_zone` cessait de la consulter, ou la consultait
/// pour une zone RÉSEAU, ou lisait le blocage SANS exiger le repli mesuré.
///
/// Les trois erreurs sont exactement celles qui feraient accuser ASIO à tort.
#[test]
fn le_garde_ne_consulte_le_blocage_que_pour_une_zone_locale_et_un_parc_de_repli() {
    let source = include_str!("transport.rs");
    let corps = source
        .split_once("pub(super) async fn gate_or_rebind_offline_zone(")
        .expect("le garde a changé de nom")
        .1
        .split_once("\n    pub(super) async fn play_inner(")
        .expect("fin du garde introuvable")
        .0;

    // La DÉCLARATION elle-même, pas le fichier entier : `starts_with("local:")`
    // apparaît aussi dans la garde du parc vide (#3737), bien plus haut.
    let declaration = corps
        .split_once("let blocage_asio =")
        .expect("le garde ne mesure plus le blocage ASIO (#4556)")
        .1
        .split_once(';')
        .expect("déclaration non terminée")
        .0;
    assert!(
        declaration.contains(r#"starts_with("local:")"#),
        "le blocage ASIO doit être consulté SOUS la condition `local:` — une zone \
         réseau ne doit jamais se voir accuser un pilote ASIO (#4556) : {declaration}"
    );
    assert!(
        declaration.contains("blocage_expliquant_un_parc_de_repli"),
        "le garde ne consulte plus le coupe-circuit ASIO (#4556) : {declaration}"
    );
    assert!(
        !corps.contains("asio_blocage_4556::blocage()"),
        "lire le blocage SEUL ferait accuser ASIO sur une machine réglée en WASAPI \
         dont le témoin traîne : il faut `blocage_expliquant_un_parc_de_repli` (#4556)"
    );
    assert!(
        corps.contains("refus_de_zone_hors_ligne("),
        "le garde doit composer son message par la fonction pure testée ici"
    );
    // Le code doit repartir sur le bus : c'est par LÀ que le refus atteint
    // l'écran (#3737), donc c'est là que le bouton s'accroche.
    for champ in ["\"reason\"", "\"can_rearm\"", "\"rearm_endpoint\""] {
        assert!(
            corps.contains(champ),
            "le champ {champ} a disparu de `zone.playback_error` : le client n'a plus \
             de quoi proposer « Réarmer ASIO » (#4556)"
        );
    }
    // Fusion des deux lots : `code` reste l'identifiant de FAMILLE posé par
    // #4580, et la phrase est retirée de la BONNE sentinelle — il y en a deux.
    assert!(
        corps.contains("\"code\": \"zone_output_unavailable\""),
        "le `code` stable de #4580 a disparu de la bulle"
    );
    assert!(
        corps.contains("phrase_sans_sentinelle(&msg)"),
        "la bulle doit retirer l'UNE OU L'AUTRE sentinelle, pas seulement la \
         première : sinon un refus ASIO s'affiche préfixé de son code"
    );
}
