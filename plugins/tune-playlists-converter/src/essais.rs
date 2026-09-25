//! Les essais du moteur, joués contre l'hôte de banc.
//!
//! Chacun répond à une exigence écrite du ticket #4717 : aperçu avant écriture,
//! rapport avec la raison, reprise sans recréer, mode par lot.

use serde_json::json;

use crate::banc::{HoteDeBanc, Piste, Verdict};
use crate::dispatch::repondre;
use crate::modele::{Demande, etat};
use crate::moteur::{Convertisseur, TAILLE_PAQUET};

fn demande(playlists: &[&str]) -> Demande {
    Demande {
        source_service: "tidal".into(),
        cible_service: "qobuz".into(),
        playlists: playlists.iter().map(|s| s.to_string()).collect(),
        suffixe_nom: None,
    }
}

/// Un banc avec une playlist de trois titres : un qui concorde, un remaster
/// trop long, un absent de la cible.
fn banc_trois_titres() -> HoteDeBanc {
    HoteDeBanc::new()
        .avec_playlist(
            "pl-1",
            "Nuit blanche",
            vec![
                Piste::new("s-1", "Imagine", "John Lennon", 183_000),
                Piste::new("s-2", "Come Together", "The Beatles", 259_000),
                Piste::new("s-3", "Obscure", "Inconnu", 200_000),
            ],
        )
        .avec_verdict(
            "Imagine",
            Verdict::exact(Piste::new("q-1", "Imagine", "John Lennon", 184_000)),
        )
        .avec_verdict(
            "Come Together",
            // Même titre une fois « (Remastered) » retiré, mais 9 s de plus.
            Verdict::exact(Piste::new(
                "q-2",
                "Come Together (Remastered 2009)",
                "The Beatles",
                268_000,
            )),
        )
        .avec_verdict("Obscure", Verdict::Rien)
}

// ---------------------------------------------------------------------------
// Aperçu
// ---------------------------------------------------------------------------

/// 🔴 LA garantie du ticket. Le banc est en **refus d'écriture** : toute
/// création ou tout ajout chez le service y échoue. L'aperçu réussit quand
/// même, donc il n'en a demandé aucun. Le retirer du moteur ferait rougir cet
/// essai, ce qui est exactement le rôle qu'on lui demande.
#[test]
fn apercu_n_ecrit_rien_chez_le_service() {
    let hote = banc_trois_titres().ecriture_interdite();
    let lot = Convertisseur::new(&hote)
        .apercu(&demande(&["pl-1"]))
        .expect("l'aperçu doit réussir sans écrire");

    assert_eq!(lot.etat, etat::APERCU);
    assert!(hote.creations().is_empty(), "aucune playlist créée");
    assert!(hote.ajouts().is_empty(), "aucun titre versé");
    assert_eq!(lot.playlists[0].cible_playlist_id, None);
}

/// L'aperçu annonce ce qui sera créé, combien de titres sont appariés et
/// lesquels manquent — le contenu exact que le ticket exige.
#[test]
fn l_apercu_annonce_ce_qui_sera_cree_et_ce_qui_manque() {
    let hote = banc_trois_titres();
    let lot = Convertisseur::new(&hote)
        .apercu(&demande(&["pl-1"]))
        .unwrap();

    let pl = &lot.playlists[0];
    assert_eq!(pl.cible_nom, "Nuit blanche", "nom repris à l'identique");
    assert_eq!(pl.total, 3);
    assert_eq!(pl.appariees.len(), 1);
    assert_eq!(pl.appariees[0].cible_id, "q-1");
    assert_eq!(pl.introuvables.len(), 2);

    let raisons: Vec<&str> = pl.introuvables.iter().map(|i| i.raison.code()).collect();
    assert!(raisons.contains(&"duree_hors_tolerance"));
    assert!(raisons.contains(&"aucun_resultat"));

    let resume = lot.resume();
    assert_eq!(resume["titres"], 3);
    assert_eq!(resume["appariees"], 1);
    assert_eq!(resume["introuvables"], 2);
    assert_eq!(resume["versees"], 0);
}

/// Le rapport dit POURQUOI, pas seulement « introuvable ».
#[test]
fn le_rapport_porte_la_raison_et_l_ecart_mesure() {
    let hote = banc_trois_titres();
    let lot = Convertisseur::new(&hote)
        .apercu(&demande(&["pl-1"]))
        .unwrap();
    let json = serde_json::to_value(&lot.playlists[0].introuvables).unwrap();
    let remaster = json
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["source_titre"] == "Come Together")
        .expect("le remaster est dans les introuvables");
    assert_eq!(remaster["raison"]["code"], "duree_hors_tolerance");
    assert_eq!(remaster["raison"]["ecart_ms"], 9_000);
}

/// Une erreur de service sur UN titre ne fait pas échouer la playlist : elle
/// devient la raison de ce titre-là.
#[test]
fn une_erreur_de_service_sur_un_titre_devient_une_raison() {
    let hote = HoteDeBanc::new()
        .avec_playlist(
            "pl-1",
            "Deux",
            vec![
                Piste::new("s-1", "Imagine", "John Lennon", 183_000),
                Piste::new("s-2", "Fragile", "Sting", 232_000),
            ],
        )
        .avec_verdict(
            "Imagine",
            Verdict::exact(Piste::new("q-1", "Imagine", "John Lennon", 183_000)),
        )
        .avec_verdict("Fragile", Verdict::Erreur("503 jeton expiré".into()));

    let lot = Convertisseur::new(&hote)
        .apercu(&demande(&["pl-1"]))
        .unwrap();
    assert_eq!(lot.playlists[0].appariees.len(), 1);
    assert_eq!(lot.playlists[0].introuvables.len(), 1);
    assert_eq!(
        lot.playlists[0].introuvables[0].raison.code(),
        "service_en_erreur"
    );
}

/// Un appariement seulement approximatif ne se transfère pas : deux critères
/// sur trois ne suffisent pas.
#[test]
fn un_appariement_approximatif_ressort_en_introuvable() {
    let hote = HoteDeBanc::new()
        .avec_playlist(
            "pl-1",
            "Un",
            vec![Piste::new("s-1", "Imagine", "John Lennon", 183_000)],
        )
        .avec_verdict(
            "Imagine",
            Verdict::flou(
                Piste::new("q-9", "Imagine (Live)", "Various Artists", 183_000),
                0.64,
            ),
        );
    let lot = Convertisseur::new(&hote)
        .apercu(&demande(&["pl-1"]))
        .unwrap();
    assert!(lot.playlists[0].appariees.is_empty());
    assert_eq!(
        lot.playlists[0].introuvables[0].raison.code(),
        "appariement_approximatif"
    );
}

// ---------------------------------------------------------------------------
// Accord explicite
// ---------------------------------------------------------------------------

/// Sans accord, rien n'est écrit — et le refus le dit.
#[test]
fn un_transfert_sans_accord_n_ecrit_rien() {
    let hote = banc_trois_titres();
    let moteur = Convertisseur::new(&hote);
    let lot = moteur.apercu(&demande(&["pl-1"])).unwrap();

    let erreur = moteur.transferer(&lot.lot_id, false).unwrap_err();
    assert!(erreur.starts_with("accord_requis"), "{erreur}");
    assert!(hote.creations().is_empty());
    assert!(hote.ajouts().is_empty());
}

/// Un lot qui n'existe pas n'écrit rien non plus — même avec l'accord. Écrire
/// exige un aperçu, pas seulement une case cochée.
#[test]
fn un_lot_inconnu_n_ecrit_rien_meme_avec_accord() {
    let hote = banc_trois_titres();
    let erreur = Convertisseur::new(&hote)
        .transferer("lot-404", true)
        .unwrap_err();
    assert!(erreur.starts_with("lot_inconnu"), "{erreur}");
    assert!(hote.creations().is_empty());
}

/// La bibliothèque locale comme CIBLE est refusée explicitement, et la raison
/// nomme la capacité qui manque.
#[test]
fn la_bibliotheque_comme_cible_est_refusee_explicitement() {
    let hote = banc_trois_titres();
    let mut d = demande(&["pl-1"]);
    d.cible_service = "local".into();
    let erreur = Convertisseur::new(&hote).apercu(&d).unwrap_err();
    assert!(erreur.starts_with("cible_locale_non_supportee"), "{erreur}");
}

// ---------------------------------------------------------------------------
// Transfert
// ---------------------------------------------------------------------------

#[test]
fn avec_accord_la_playlist_est_creee_et_les_apparies_verses() {
    let hote = banc_trois_titres();
    let moteur = Convertisseur::new(&hote);
    let lot = moteur.apercu(&demande(&["pl-1"])).unwrap();
    let lot = moteur.transferer(&lot.lot_id, true).unwrap();

    assert_eq!(lot.etat, etat::TERMINE);
    assert_eq!(
        hote.creations(),
        vec![("cible-1".into(), "Nuit blanche".into())]
    );
    assert_eq!(hote.ajouts(), vec![("cible-1".into(), vec!["q-1".into()])]);
    assert_eq!(lot.playlists[0].versees, vec!["q-1".to_string()]);
}

/// On ne crée pas une playlist vide chez un service : rien n'y serait versé,
/// et aucune capacité ne saurait l'effacer ensuite.
#[test]
fn une_playlist_sans_aucun_apparie_ne_cree_rien() {
    let hote = HoteDeBanc::new()
        .avec_playlist(
            "pl-1",
            "Rien",
            vec![Piste::new("s-1", "Obscure", "Inconnu", 200_000)],
        )
        .avec_verdict("Obscure", Verdict::Rien);
    let moteur = Convertisseur::new(&hote);
    let lot = moteur.apercu(&demande(&["pl-1"])).unwrap();
    let lot = moteur.transferer(&lot.lot_id, true).unwrap();

    assert!(hote.creations().is_empty());
    assert_eq!(lot.playlists[0].etat, etat::RIEN_A_TRANSFERER);
    assert_eq!(lot.etat, etat::TERMINE);
}

/// Le mode par lot : plusieurs playlists en une passe, un rapport par playlist.
#[test]
fn le_mode_par_lot_traite_plusieurs_playlists_en_une_passe() {
    let hote = banc_trois_titres()
        .avec_playlist(
            "pl-2",
            "Deuxième",
            vec![Piste::new("s-9", "Fragile", "Sting", 232_000)],
        )
        .avec_verdict(
            "Fragile",
            Verdict::exact(Piste::new("q-9", "Fragile", "Sting", 232_500)),
        );
    let moteur = Convertisseur::new(&hote);
    let lot = moteur.apercu(&demande(&["pl-1", "pl-2"])).unwrap();
    let lot = moteur.transferer(&lot.lot_id, true).unwrap();

    assert_eq!(lot.playlists.len(), 2);
    assert_eq!(hote.creations().len(), 2);
    assert_eq!(lot.resume()["playlists"], 2);
    assert_eq!(lot.resume()["appariees"], 2);
    // Le rapport reste PAR playlist, pas agrégé.
    assert_eq!(lot.playlists[0].introuvables.len(), 2);
    assert_eq!(lot.playlists[1].introuvables.len(), 0);
}

/// Les titres partent par paquets de cent : c'est le lot d'ajout de TIDAL, et
/// le grain de la reprise.
#[test]
fn les_titres_partent_par_paquets_de_cent() {
    let mut pistes = Vec::new();
    let mut hote = HoteDeBanc::new();
    for i in 0..250 {
        let titre = format!("Titre {i}");
        pistes.push(Piste::new(&format!("s-{i}"), &titre, "Artiste", 200_000));
        hote = hote.avec_verdict(
            &titre,
            Verdict::exact(Piste::new(&format!("q-{i}"), &titre, "Artiste", 200_000)),
        );
    }
    let hote = hote.avec_playlist("pl-1", "Longue", pistes);
    let moteur = Convertisseur::new(&hote);
    let lot = moteur.apercu(&demande(&["pl-1"])).unwrap();
    let lot = moteur.transferer(&lot.lot_id, true).unwrap();

    let ajouts = hote.ajouts();
    assert_eq!(ajouts.len(), 3, "250 titres = 100 + 100 + 50");
    assert_eq!(ajouts[0].1.len(), TAILLE_PAQUET);
    assert_eq!(ajouts[2].1.len(), 50);
    assert_eq!(lot.playlists[0].versees.len(), 250);
}

// ---------------------------------------------------------------------------
// Reprise
// ---------------------------------------------------------------------------

/// 🔴 Le lot tombe après le premier paquet ; la reprise ne recrée **pas** la
/// playlist et ne reverse **pas** les cent premiers titres.
#[test]
fn une_reprise_ne_recree_ni_la_playlist_ni_les_titres_deja_verses() {
    let mut pistes = Vec::new();
    let mut hote = HoteDeBanc::new();
    for i in 0..250 {
        let titre = format!("Titre {i}");
        pistes.push(Piste::new(&format!("s-{i}"), &titre, "Artiste", 200_000));
        hote = hote.avec_verdict(
            &titre,
            Verdict::exact(Piste::new(&format!("q-{i}"), &titre, "Artiste", 200_000)),
        );
    }
    let hote = hote.avec_playlist("pl-1", "Longue", pistes);
    // Un seul ajout passe, le suivant échoue.
    *hote.tomber_apres_ajouts.borrow_mut() = Some(1);

    let moteur = Convertisseur::new(&hote);
    let lot = moteur.apercu(&demande(&["pl-1"])).unwrap();
    let interrompu = moteur.transferer(&lot.lot_id, true).unwrap();

    assert_eq!(interrompu.etat, etat::INTERROMPU);
    assert_eq!(interrompu.playlists[0].versees.len(), TAILLE_PAQUET);
    assert_eq!(hote.creations().len(), 1);
    assert_eq!(hote.ajouts().len(), 1);

    // Le service revient.
    *hote.tomber_apres_ajouts.borrow_mut() = None;
    let repris = moteur.reprendre(&lot.lot_id).unwrap();

    assert_eq!(repris.etat, etat::TERMINE);
    assert_eq!(
        hote.creations().len(),
        1,
        "la playlist cible n'est PAS recréée"
    );
    let ajouts = hote.ajouts();
    assert_eq!(
        ajouts.len(),
        3,
        "100 versés + 100 + 50, pas 100 + 100 + 100 + 50"
    );
    assert_eq!(repris.playlists[0].versees.len(), 250);
    // Aucun identifiant versé deux fois.
    let mut tous: Vec<String> = ajouts.iter().flat_map(|(_, ids)| ids.clone()).collect();
    let avant = tous.len();
    tous.sort();
    tous.dedup();
    assert_eq!(tous.len(), avant, "aucun doublon versé");
}

/// Une reprise sur un lot jamais accepté est refusée : la reprise n'est pas une
/// porte dérobée autour de l'accord.
#[test]
fn une_reprise_ne_remplace_pas_l_accord() {
    let hote = banc_trois_titres();
    let moteur = Convertisseur::new(&hote);
    let lot = moteur.apercu(&demande(&["pl-1"])).unwrap();
    let erreur = moteur.reprendre(&lot.lot_id).unwrap_err();
    assert!(erreur.starts_with("accord_requis"), "{erreur}");
    assert!(hote.creations().is_empty());
}

/// Un lot déjà engagé ne se rejoue pas par `/transfert` : la seconde passe
/// recréerait la playlist. Le refus renvoie vers `/reprise`.
#[test]
fn un_lot_deja_engage_ne_se_retransfere_pas() {
    let hote = banc_trois_titres();
    let moteur = Convertisseur::new(&hote);
    let lot = moteur.apercu(&demande(&["pl-1"])).unwrap();
    moteur.transferer(&lot.lot_id, true).unwrap();
    let erreur = moteur.transferer(&lot.lot_id, true).unwrap_err();
    assert!(erreur.starts_with("lot_deja_engage"), "{erreur}");
    assert_eq!(hote.creations().len(), 1);
}

/// Une reprise d'un lot terminé ne réécrit rien.
#[test]
fn reprendre_un_lot_termine_n_ecrit_rien_de_plus() {
    let hote = banc_trois_titres();
    let moteur = Convertisseur::new(&hote);
    let lot = moteur.apercu(&demande(&["pl-1"])).unwrap();
    moteur.transferer(&lot.lot_id, true).unwrap();
    let avant = hote.ajouts().len();
    moteur.reprendre(&lot.lot_id).unwrap();
    assert_eq!(hote.ajouts().len(), avant);
    assert_eq!(hote.creations().len(), 1);
}

// ---------------------------------------------------------------------------
// Persistance et routes
// ---------------------------------------------------------------------------

/// L'état survit au moteur : un nouveau `Convertisseur` relit le même lot.
/// C'est ce qui rend la reprise possible après un redémarrage du serveur.
#[test]
fn le_lot_se_relit_depuis_le_stockage() {
    let hote = banc_trois_titres();
    let lot_id = Convertisseur::new(&hote)
        .apercu(&demande(&["pl-1"]))
        .unwrap()
        .lot_id;

    let relu = Convertisseur::new(&hote).lire_le_lot(&lot_id).unwrap();
    assert_eq!(relu.lot_id, lot_id);
    assert_eq!(relu.playlists[0].appariees.len(), 1);
    assert_eq!(relu.playlists[0].introuvables.len(), 2);
}

/// `kv_list` ne doit pas confondre les clés de détail avec les en-têtes.
#[test]
fn la_liste_des_lots_ne_montre_que_les_en_tetes() {
    let hote = banc_trois_titres();
    let moteur = Convertisseur::new(&hote);
    moteur.apercu(&demande(&["pl-1"])).unwrap();
    moteur.apercu(&demande(&["pl-1"])).unwrap();
    let lots = moteur.lots().unwrap();
    assert_eq!(lots.len(), 2, "deux en-têtes, pas les détails par playlist");
    assert!(lots.iter().all(|l| l["lot_id"].is_string()));
}

#[test]
fn les_identifiants_de_lot_ne_se_repetent_pas() {
    let hote = banc_trois_titres();
    let moteur = Convertisseur::new(&hote);
    let a = moteur.apercu(&demande(&["pl-1"])).unwrap().lot_id;
    let b = moteur.apercu(&demande(&["pl-1"])).unwrap().lot_id;
    assert_ne!(a, b);
}

/// La route `/apercu` rend 200 et un résumé ; `/transfert` sans accord rend
/// 409 et n'écrit rien.
#[test]
fn les_routes_rendent_les_codes_attendus() {
    let hote = banc_trois_titres();

    let r = repondre(
        &hote,
        &json!({
            "method": "POST",
            "path": "/apercu",
            "query": "",
            "body": { "source_service": "tidal", "cible_service": "qobuz", "playlists": ["pl-1"] },
        }),
    );
    assert_eq!(r["status"], 200);
    assert_eq!(r["body"]["resume"]["appariees"], 1);
    let lot_id = r["body"]["lot"]["lot_id"].as_str().unwrap().to_string();

    let r = repondre(
        &hote,
        &json!({
            "method": "POST",
            "path": "/transfert",
            "query": "",
            "body": { "lot_id": lot_id, "accord": false },
        }),
    );
    assert_eq!(r["status"], 409);
    assert!(hote.creations().is_empty());

    let r = repondre(
        &hote,
        &json!({
            "method": "POST",
            "path": "/transfert",
            "query": "",
            "body": { "lot_id": lot_id, "accord": true },
        }),
    );
    assert_eq!(r["status"], 200);
    assert_eq!(r["body"]["resume"]["versees"], 1);

    let r = repondre(
        &hote,
        &json!({ "method": "GET", "path": "/lot", "query": format!("id={lot_id}"), "body": null }),
    );
    assert_eq!(r["status"], 200);
    assert_eq!(r["body"]["lot"]["lot_id"], lot_id);

    let r = repondre(
        &hote,
        &json!({ "method": "GET", "path": "/inconnue", "query": "", "body": null }),
    );
    assert_eq!(r["status"], 404);
}

/// Une source LOCALE vers un service : le sens que la tranche 1 rend possible.
#[test]
fn une_playlist_locale_se_transfere_vers_un_service() {
    let hote = HoteDeBanc::new()
        .avec_playlist(
            "7",
            "Ma sélection",
            vec![Piste::new("12", "Imagine", "John Lennon", 183_000)],
        )
        .avec_verdict(
            "Imagine",
            Verdict::exact(Piste::new("q-1", "Imagine", "John Lennon", 183_000)),
        );
    let moteur = Convertisseur::new(&hote);
    let mut d = demande(&["7"]);
    d.source_service = "local".into();
    let lot = moteur.apercu(&d).unwrap();
    assert_eq!(lot.playlists[0].source_nom, "Ma sélection");
    let lot = moteur.transferer(&lot.lot_id, true).unwrap();
    assert_eq!(
        hote.creations(),
        vec![("cible-1".into(), "Ma sélection".into())]
    );
    assert_eq!(lot.playlists[0].versees, vec!["q-1".to_string()]);
}

/// Le suffixe est facultatif, et son absence est le cas « à l'identique ».
#[test]
fn le_suffixe_de_nom_est_facultatif() {
    let hote = banc_trois_titres();
    let mut d = demande(&["pl-1"]);
    d.suffixe_nom = Some(" (Tune)".into());
    let lot = Convertisseur::new(&hote).apercu(&d).unwrap();
    assert_eq!(lot.playlists[0].cible_nom, "Nuit blanche (Tune)");
}
