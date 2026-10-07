//! #4741 — le greffon devient le SEUL moteur de transfert du serveur.
//!
//! Ce qu'il fallait lui ajouter pour remplacer les deux moteurs retirés :
//!
//! * la bibliothèque locale comme **cible** (l'ancienne route
//!   `/playlist-manager/transfer` savait importer une playlist de service dans
//!   la bibliothèque ; le greffon le refusait) ;
//! * l'appariement **par ISRC d'abord**, puis titre + artiste + durée sur tout
//!   le classement de l'hôte — et pas seulement sur son premier candidat ;
//! * le nom de la playlist créée, choisi par l'utilisateur (`nom_cible`).
//!
//! Tout se joue contre l'hôte de banc : aucun vrai service n'est touché.

use crate::appariement::Raison;
use crate::banc::{HoteDeBanc, Piste, Verdict};
use crate::modele::{Demande, etat};
use crate::moteur::Convertisseur;

fn demande(source: &str, cible: &str, playlists: &[&str]) -> Demande {
    Demande {
        source_service: source.into(),
        cible_service: cible.into(),
        playlists: playlists.iter().map(|s| s.to_string()).collect(),
        suffixe_nom: None,
        nom_cible: None,
    }
}

// ---------------------------------------------------------------------------
// La bibliothèque locale, des deux côtés
// ---------------------------------------------------------------------------

/// Une playlist TIDAL de deux titres : l'un est dans la bibliothèque (piste
/// locale 501), l'autre non.
fn banc_vers_la_bibliotheque() -> HoteDeBanc {
    HoteDeBanc::new()
        .avec_playlist(
            "pl-t",
            "Du service",
            vec![
                Piste::new("t-1", "Imagine", "John Lennon", 183_000),
                Piste::new("t-2", "Obscure", "Inconnu", 200_000),
            ],
        )
        .avec_verdict_chez(
            "local",
            "Imagine",
            Verdict::exact(Piste::new("501", "Imagine", "John Lennon", 183_500)),
        )
        .avec_verdict_chez("local", "Obscure", Verdict::Rien)
}

/// 🔴 Le témoin de la cible locale. Avant #4741, l'aperçu rendait
/// `cible_locale_non_supportee` et rien n'était créé.
#[test]
fn la_bibliotheque_est_une_cible() {
    let hote = banc_vers_la_bibliotheque();
    let moteur = Convertisseur::new(&hote);

    let lot = moteur
        .apercu(&demande("tidal", "local", &["pl-t"]))
        .expect("la bibliothèque doit être acceptée comme cible");
    assert!(
        hote.creations().is_empty(),
        "l'aperçu a écrit : {:?}",
        hote.creations()
    );
    let pl = &lot.playlists[0];
    assert_eq!(pl.appariees.len(), 1);
    // L'identifiant retenu est l'ENTIER de la bibliothèque, pas l'origine
    // streaming que la fiche locale porte sous `source_id`.
    assert_eq!(pl.appariees[0].cible_id, "501");
    assert_eq!(pl.introuvables.len(), 1);
    assert_eq!(pl.introuvables[0].raison, Raison::AucunResultat);

    let lot = moteur.transferer(&lot.lot_id, true).unwrap();
    assert_eq!(lot.etat, etat::TERMINE);
    // Le banc numérote les playlists locales créées à partir de 1001.
    assert_eq!(hote.creations(), vec![("1001".into(), "Du service".into())]);
    assert_eq!(hote.ajouts(), vec![("1001".into(), vec!["501".into()])]);
    assert_eq!(lot.playlists[0].cible_playlist_id.as_deref(), Some("1001"));
    assert!(
        lot.playlists[0].snapshot_avant.is_some(),
        "une copie datée doit précéder le premier titre versé, en local aussi"
    );
}

/// Le mode par lot vers la bibliothèque : une playlist locale par playlist
/// source, et l'aperçu reste sans écriture même banc verrouillé.
#[test]
fn un_lot_vers_la_bibliotheque_cree_une_playlist_par_source() {
    let hote = banc_vers_la_bibliotheque().avec_playlist(
        "pl-u",
        "Seconde",
        vec![Piste::new("t-3", "Imagine", "John Lennon", 183_000)],
    );
    let moteur = Convertisseur::new(&hote);
    let lot = moteur
        .apercu(&demande("tidal", "local", &["pl-t", "pl-u"]))
        .unwrap();
    let lot = moteur.transferer(&lot.lot_id, true).unwrap();
    assert_eq!(lot.etat, etat::TERMINE);
    assert_eq!(
        hote.creations(),
        vec![
            ("1001".into(), "Du service".into()),
            ("1002".into(), "Seconde".into()),
        ]
    );
}

#[test]
fn un_apercu_vers_la_bibliotheque_n_ecrit_rien() {
    let hote = banc_vers_la_bibliotheque().ecriture_interdite();
    let lot = Convertisseur::new(&hote)
        .apercu(&demande("tidal", "local", &["pl-t"]))
        .expect("l'aperçu n'a demandé aucune écriture : il doit réussir");
    assert_eq!(lot.etat, etat::APERCU);
    assert!(hote.creations().is_empty());
    assert!(hote.ajouts().is_empty());
}

/// La bibliothèque comme SOURCE vers un service : la playlist locale 7 se lit
/// par son identifiant entier.
#[test]
fn la_bibliotheque_est_une_source() {
    let hote = HoteDeBanc::new()
        .avec_playlist(
            "7",
            "Ma locale",
            vec![Piste::new("11", "Imagine", "John Lennon", 183_000)],
        )
        .avec_verdict(
            "Imagine",
            Verdict::exact(Piste::new("q-1", "Imagine", "John Lennon", 184_000)),
        );
    let moteur = Convertisseur::new(&hote);
    let lot = moteur.apercu(&demande("local", "qobuz", &["7"])).unwrap();
    assert_eq!(lot.playlists[0].source_nom, "Ma locale");
    let lot = moteur.transferer(&lot.lot_id, true).unwrap();
    assert_eq!(lot.etat, etat::TERMINE);
    assert_eq!(hote.ajouts(), vec![("cible-1".into(), vec!["q-1".into()])]);
}

#[test]
fn la_bibliotheque_vers_elle_meme_reste_refusee() {
    let hote = banc_vers_la_bibliotheque();
    let erreur = Convertisseur::new(&hote)
        .apercu(&demande("local", "local", &["7"]))
        .unwrap_err();
    assert!(erreur.contains("même service"), "{erreur}");
}

// ---------------------------------------------------------------------------
// Le nom choisi
// ---------------------------------------------------------------------------

#[test]
fn le_nom_choisi_prime_sur_le_nom_source() {
    let hote = banc_vers_la_bibliotheque();
    let moteur = Convertisseur::new(&hote);
    let mut d = demande("tidal", "local", &["pl-t"]);
    d.nom_cible = Some("  Mon import  ".into());
    d.suffixe_nom = Some(" (copie)".into());
    let lot = moteur.apercu(&d).unwrap();
    assert_eq!(lot.playlists[0].cible_nom, "Mon import");
    moteur.transferer(&lot.lot_id, true).unwrap();
    assert_eq!(hote.creations(), vec![("1001".into(), "Mon import".into())]);
}

#[test]
fn un_nom_choisi_sur_un_lot_est_refuse() {
    let hote = banc_vers_la_bibliotheque();
    let mut d = demande("tidal", "local", &["pl-t", "pl-u"]);
    d.nom_cible = Some("Un seul nom pour deux".into());
    let erreur = Convertisseur::new(&hote).apercu(&d).unwrap_err();
    assert!(erreur.starts_with("demande_invalide"), "{erreur}");
    assert!(hote.creations().is_empty());
}

// ---------------------------------------------------------------------------
// L'appariement : ISRC d'abord, puis le classement
// ---------------------------------------------------------------------------

fn banc_un_titre(source: Piste, verdict: Verdict) -> HoteDeBanc {
    let titre = source.titre.clone();
    HoteDeBanc::new()
        .avec_playlist("pl-1", "Une", vec![source])
        .avec_verdict(&titre, verdict)
}

fn appariees_et_introuvables(hote: &HoteDeBanc) -> crate::modele::PlaylistDuLot {
    Convertisseur::new(hote)
        .apercu(&demande("tidal", "qobuz", &["pl-1"]))
        .unwrap()
        .playlists
        .remove(0)
}

/// 🔴 Le titre est écrit autrement chez la cible (le flou le juge
/// approximatif), mais l'ISRC est le même enregistrement et la durée concorde :
/// il est transféré. Sans l'étape ISRC, il sortait `appariement_approximatif`.
#[test]
fn l_isrc_l_emporte_sur_le_flou_du_titre() {
    let hote = banc_un_titre(
        Piste::new("s-1", "Hymne à l'amour", "Édith Piaf", 200_000).avec_isrc("FR-Z03-50-00001"),
        Verdict::Classement(vec![(
            Piste::new("q-9", "Hymn to Love", "Edith Piaf", 201_000).avec_isrc("frz035000001"),
            0.62,
            true,
        )]),
    );
    let pl = appariees_et_introuvables(&hote);
    assert_eq!(
        pl.appariees.len(),
        1,
        "l'ISRC identique devait l'emporter : {:?}",
        pl.introuvables
    );
    assert_eq!(pl.appariees[0].cible_id, "q-9");
}

/// L'ISRC ne dispense pas de la durée : neuf secondes d'écart, c'est
/// introuvable, avec l'écart mesuré.
#[test]
fn un_isrc_identique_ne_dispense_pas_de_la_duree() {
    let hote = banc_un_titre(
        Piste::new("s-1", "Come Together", "The Beatles", 259_000).avec_isrc("GBAYE6900001"),
        Verdict::Classement(vec![(
            Piste::new("q-2", "Come Together", "The Beatles", 268_000).avec_isrc("GBAYE6900001"),
            1.0,
            false,
        )]),
    );
    let pl = appariees_et_introuvables(&hote);
    assert!(pl.appariees.is_empty());
    assert_eq!(pl.introuvables[0].raison.code(), "duree_hors_tolerance");
}

/// L'ISRC passe AVANT le classement : un candidat de tête qui tient les trois
/// critères mais porte un autre ISRC cède la place à celui qui porte le bon.
#[test]
fn l_isrc_passe_avant_la_tete_du_classement() {
    let hote = banc_un_titre(
        Piste::new("s-1", "Imagine", "John Lennon", 183_000).avec_isrc("USAAA7100001"),
        Verdict::Classement(vec![
            (
                Piste::new("q-live", "Imagine", "John Lennon", 184_000).avec_isrc("USBBB0000009"),
                0.95,
                false,
            ),
            (
                Piste::new("q-studio", "Imagine", "John Lennon", 183_200).avec_isrc("USAAA7100001"),
                0.9,
                false,
            ),
        ]),
    );
    let pl = appariees_et_introuvables(&hote);
    assert_eq!(pl.appariees[0].cible_id, "q-studio");
}

/// 🔴 Le verdict de tête est un remaster (9 s de trop) ; le deuxième candidat
/// est la bonne édition. Le greffon ne lisait que la tête : introuvable.
#[test]
fn le_candidat_suivant_est_pris_quand_la_tete_rate_la_duree() {
    let hote = banc_un_titre(
        Piste::new("s-1", "Come Together", "The Beatles", 259_000),
        Verdict::Classement(vec![
            (
                Piste::new(
                    "q-rem",
                    "Come Together (Remastered 2009)",
                    "The Beatles",
                    268_000,
                ),
                0.95,
                false,
            ),
            (
                Piste::new("q-orig", "Come Together", "The Beatles", 259_500),
                0.95,
                false,
            ),
        ]),
    );
    let pl = appariees_et_introuvables(&hote);
    assert_eq!(
        pl.appariees.len(),
        1,
        "le deuxième candidat tenait les trois critères : {:?}",
        pl.introuvables
    );
    assert_eq!(pl.appariees[0].cible_id, "q-orig");
}

/// Aucun candidat ne tient : la raison rapportée est celle de la TÊTE, celle
/// que l'utilisateur reconnaîtra.
#[test]
fn sans_candidat_valable_la_raison_est_celle_de_la_tete() {
    let hote = banc_un_titre(
        Piste::new("s-1", "Come Together", "The Beatles", 259_000),
        Verdict::Classement(vec![
            (
                Piste::new(
                    "q-rem",
                    "Come Together (Remastered)",
                    "The Beatles",
                    268_000,
                ),
                0.95,
                false,
            ),
            (
                Piste::new("q-cover", "Come Together", "Tribute Band", 259_000),
                0.65,
                true,
            ),
        ]),
    );
    let pl = appariees_et_introuvables(&hote);
    assert!(pl.appariees.is_empty());
    match &pl.introuvables[0].raison {
        Raison::DureeHorsTolerance { ecart_ms, .. } => assert_eq!(*ecart_ms, 9_000),
        autre => panic!("raison inattendue : {autre:?}"),
    }
}

/// Un approximatif plus bas dans le classement ne passe pas parce que sa durée
/// concorde : titre et artiste restent exigés.
#[test]
fn un_approximatif_du_classement_ne_passe_pas_sur_sa_seule_duree() {
    let hote = banc_un_titre(
        Piste::new("s-1", "Imagine", "John Lennon", 183_000),
        Verdict::Classement(vec![(
            Piste::new("q-cover", "Imagine", "A Tribute Band", 183_000),
            0.65,
            true,
        )]),
    );
    let pl = appariees_et_introuvables(&hote);
    assert!(pl.appariees.is_empty());
    assert_eq!(pl.introuvables[0].raison.code(), "appariement_approximatif");
}
