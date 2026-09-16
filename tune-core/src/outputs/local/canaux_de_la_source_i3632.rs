//! #3632 — « FLAC multicanal vers un ampli HDMI : Tune ne DEMANDE jamais N
//! canaux au périphérique ».
//!
//! Didier (fil 1717) : le moteur multicanal existe, le décodage accepte
//! 1..=32 voies, et pourtant un 5.1 sort en stéréo — parce que le flux cpal
//! était construit avec le nombre de canaux PAR DÉFAUT du périphérique, et
//! qu'une sortie HDMI annonce presque toujours 2 par défaut.
//!
//! La décision est pure et compilée sur toutes les cibles : elle ne voit que
//! (canaux de la source, canaux par défaut, largeurs annoncées). Shrek n'a pas
//! de carte son ; rien ici n'affirme une écoute.
use super::{ChoixDeCanaux, choisir_les_canaux_de_sortie};

/// Ce qu'annonce, à peu de chose près, une sortie HDMI vers un ampli 7.1 :
/// le défaut est stéréo, mais 6 et 8 voies sont là pour qui les demande.
const HDMI_7_1: &[u16] = &[2, 6, 8];

#[test]
fn une_source_stereo_ne_change_rien_au_defaut() {
    for annonces in [&[][..], &[2][..], HDMI_7_1] {
        assert_eq!(
            choisir_les_canaux_de_sortie(2, 2, annonces),
            ChoixDeCanaux::Defaut(2),
            "la quasi-totalité des lectures : le comportement d'avant, strictement"
        );
        assert_eq!(
            choisir_les_canaux_de_sortie(1, 2, annonces),
            ChoixDeCanaux::Defaut(2),
            "le mono se dédouble sur le défaut stéréo, comme avant"
        );
    }
}

#[test]
fn un_5_1_sur_une_sortie_qui_annonce_six_voies_les_demande() {
    assert_eq!(
        choisir_les_canaux_de_sortie(6, 2, HDMI_7_1),
        ChoixDeCanaux::Multicanal { canaux: 6 },
        "le périphérique annonce 6 : on ouvre 6, pas le défaut stéréo — c'est \
         tout le sujet de #3632"
    );
    assert_eq!(
        choisir_les_canaux_de_sortie(8, 2, HDMI_7_1),
        ChoixDeCanaux::Multicanal { canaux: 8 }
    );
}

#[test]
fn un_5_1_sur_une_sortie_qui_n_annonce_que_huit_voies_ouvre_huit() {
    let choix = choisir_les_canaux_de_sortie(6, 2, &[2, 8]);
    assert_eq!(
        choix,
        ChoixDeCanaux::Multicanal { canaux: 8 },
        "6 n'est pas annoncé mais 8 l'est : les deux voies absentes reçoivent du \
         silence (`adapt_channels_f32`), aucun mixage vers la stéréo"
    );
    assert_eq!(choix.canaux(), 8);
}

#[test]
fn un_5_1_sur_une_sortie_stereo_se_replie_et_le_dit() {
    let choix = choisir_les_canaux_de_sortie(6, 2, &[2]);
    assert_eq!(
        choix,
        ChoixDeCanaux::RepliStereo {
            defaut: 2,
            demande: 6,
            accepte: 2,
        },
        "le périphérique n'accepte pas six voies : le repli ITU d'avant reste, mais \
         il porte `demande` et `accepte` pour la ligne de journal"
    );
    assert_eq!(
        choix.canaux(),
        2,
        "le flux s'ouvre au défaut — jamais à une largeur que le périphérique n'annonce pas"
    );
}

#[test]
fn une_enumeration_echouee_n_autorise_rien() {
    let choix = choisir_les_canaux_de_sortie(6, 2, &[]);
    assert_eq!(
        choix,
        ChoixDeCanaux::RepliStereo {
            defaut: 2,
            demande: 6,
            accepte: 2,
        },
        "aucune largeur annoncée (PipeWire en compatibilité ALSA) : une absence \
         n'est pas une preuve, on garde le défaut et on le dit"
    );
}

#[test]
fn un_defaut_deja_plus_large_que_la_source_reste_le_defaut() {
    assert_eq!(
        choisir_les_canaux_de_sortie(6, 8, &[8]),
        ChoixDeCanaux::Defaut(8),
        "le défaut couvre déjà la source : remplissage de silence, comme avant — \
         ce n'est ni un repli ni une demande"
    );
    assert_eq!(
        choisir_les_canaux_de_sortie(6, 8, &[]),
        ChoixDeCanaux::Defaut(8),
        "même sans énumération : un défaut à 8 n'est pas un repli stéréo, et la \
         ligne `local_multicanal_replie_stereo` ne doit pas mentir"
    );
}

/// Toute la famille 3..=32, contre toute liste annoncée : ce qui est ouvert ne
/// dépasse JAMAIS la plus grande largeur annoncée (ou le défaut, à défaut
/// d'annonce), et `Multicanal` ne sort que de la liste annoncée.
#[test]
fn la_largeur_ouverte_ne_depasse_jamais_ce_que_le_peripherique_annonce() {
    let listes: [&[u16]; 6] = [&[], &[2], &[2, 6], &[2, 8], HDMI_7_1, &[2, 6, 8, 16, 32]];
    for &annonces in &listes {
        for defaut in [1u16, 2, 6, 8] {
            for source in 3u16..=32 {
                let choix = choisir_les_canaux_de_sortie(source, defaut, annonces);
                let plafond = annonces.iter().copied().max().unwrap_or(0).max(defaut);
                assert!(
                    choix.canaux() <= plafond,
                    "source={source} defaut={defaut} annonces={annonces:?} : {choix:?} \
                     ouvre plus large que ce que le périphérique annonce"
                );
                if let ChoixDeCanaux::Multicanal { canaux } = choix {
                    assert!(
                        annonces.contains(&canaux),
                        "source={source} : {canaux} voies demandées sans être annoncées"
                    );
                    assert!(canaux >= source, "une demande multicanal ne mixe jamais");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// La garde de site : la décision est-elle BRANCHÉE, sur les DEUX chemins ?
// ---------------------------------------------------------------------------

/// `local.rs` sans ses modules d'épreuves — même découpe que
/// `relache_peripherique_i3575`.
fn code_de_production_du_chemin_compresse() -> &'static str {
    const TOUT: &str = include_str!("../local.rs");
    const BORNE: &str = "mod relache_peripherique_i3575";
    let fin = TOUT
        .find(BORNE)
        .unwrap_or_else(|| panic!("ce module a été renommé : la découpe ne protège plus rien"));
    &TOUT[..fin]
}

/// Les épreuves ci-dessus exercent la règle ; aucune ne peut voir la seule
/// chose qui reste : que les deux ouvertures cpal — le chemin PCM
/// (`BackendCpal::ouvrir`) et le chemin compressé (`play_url`) — l'APPELLENT
/// à la place du défaut cpal. La définition vit dans `resolution.rs`, qui
/// n'est pas lu ici : chaque occurrence comptée est un appel.
#[test]
fn les_deux_chemins_cpal_demandent_les_canaux_de_la_source() {
    const APPEL: &str = "ouvrir_les_canaux_de_la_source(";
    let backend = include_str!("backend.rs");
    assert_eq!(
        backend.matches(APPEL).count(),
        1,
        "le chemin PCM (`BackendCpal::ouvrir`) n'applique plus la décision de \
         canaux : un FLAC 5.1 rouvre le défaut stéréo de cpal (#3632)"
    );
    let compresse = code_de_production_du_chemin_compresse();
    assert_eq!(
        compresse.matches(APPEL).count(),
        1,
        "le chemin compressé (`play_url`, flux servi tel quel par un serveur \
         multimédia) n'applique plus la décision de canaux (#3632)"
    );
}
