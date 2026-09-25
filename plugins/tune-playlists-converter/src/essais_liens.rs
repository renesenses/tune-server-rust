//! Les essais des liens auto-sync (#4719), joués contre l'hôte de banc.
//!
//! Chacun garde une règle du ticket : ajouts seulement, disparition signalée
//! et jamais propagée, aperçu accepté avant la première synchronisation,
//! snapshot avant chaque synchronisation, journal, pause, suppression du lien
//! sans toucher aux playlists.

use serde_json::json;

use crate::banc::{HoteDeBanc, Piste, Verdict};
use crate::dispatch::{repondre, sur_evenement};
use crate::liens::{DemandeLien, Extremite, Liens, declencheur, etat_lien, sens};

const MINUTE: u64 = 60_000;

fn imagine_a() -> Piste {
    Piste::new("s-1", "Imagine", "John Lennon", 183_000)
}
fn fragile_a() -> Piste {
    Piste::new("s-2", "Fragile", "Sting", 232_000)
}
fn roxanne_a() -> Piste {
    Piste::new("s-4", "Roxanne", "The Police", 192_000)
}
fn imagine_b() -> Piste {
    Piste::new("q-1", "Imagine", "John Lennon", 184_000)
}

/// A (tidal) : Imagine, Fragile. B (qobuz) : Imagine. Les verdicts sont
/// posés PAR SERVICE, pour que le double sens ait deux espaces d'identifiants.
fn banc() -> HoteDeBanc {
    HoteDeBanc::new()
        .avec_playlist("pl-a", "Route 66", vec![imagine_a(), fragile_a()])
        .avec_playlist("pl-b", "Route 66 (Qobuz)", vec![imagine_b()])
        .avec_verdict_chez("qobuz", "Imagine", Verdict::exact(imagine_b()))
        .avec_verdict_chez(
            "qobuz",
            "Fragile",
            Verdict::exact(Piste::new("q-2", "Fragile", "Sting", 232_500)),
        )
        .avec_verdict_chez(
            "qobuz",
            "Roxanne",
            Verdict::exact(Piste::new("q-4", "Roxanne", "The Police", 192_000)),
        )
        .avec_verdict_chez("qobuz", "Obscure", Verdict::Rien)
        .avec_verdict_chez("tidal", "Imagine", Verdict::exact(imagine_a()))
        .avec_verdict_chez("tidal", "Fragile", Verdict::exact(fragile_a()))
        .avec_verdict_chez(
            "tidal",
            "Hotel",
            Verdict::exact(Piste::new("s-5", "Hotel", "Eagles", 390_000)),
        )
}

fn demande(sens_lien: &str, cadence: u64) -> DemandeLien {
    DemandeLien {
        a: Extremite {
            service: "tidal".into(),
            playlist_id: "pl-a".into(),
            nom: String::new(),
        },
        b: Extremite {
            service: "qobuz".into(),
            playlist_id: "pl-b".into(),
            nom: String::new(),
        },
        sens: Some(sens_lien.into()),
        cadence_minutes: Some(cadence),
    }
}

/// Un lien créé, aperçu, première synchronisation acceptée.
fn lien_actif(hote: &HoteDeBanc, sens_lien: &str, cadence: u64) -> String {
    let liens = Liens::new(hote);
    let id = liens.creer(&demande(sens_lien, cadence)).unwrap().lien_id;
    liens.apercu(&id).unwrap();
    liens.synchroniser(&id, true, declencheur::DEMANDE).unwrap();
    id
}

// ---------------------------------------------------------------------------
// Créer
// ---------------------------------------------------------------------------

#[test]
fn un_lien_relie_deux_services_differents_et_attend_un_apercu() {
    let hote = banc();
    let liens = Liens::new(&hote);
    let lien = liens.creer(&demande(sens::A_VERS_B, 60)).unwrap();
    assert_eq!(lien.etat, etat_lien::ATTENTE_APERCU);
    assert_eq!(lien.a.nom, "Route 66", "nom lu chez le service");
    assert!(!lien.premiere_synchro_faite);

    let mut meme = demande(sens::A_VERS_B, 60);
    meme.b.service = "tidal".into();
    assert!(
        liens
            .creer(&meme)
            .unwrap_err()
            .starts_with("demande_invalide")
    );

    let err = liens.creer(&demande(sens::A_VERS_B, 5)).unwrap_err();
    assert!(
        err.starts_with("demande_invalide"),
        "cadence trop courte : {err}"
    );
    assert!(hote.ajouts().is_empty() && hote.creations().is_empty());
}

// ---------------------------------------------------------------------------
// Aperçu et première synchronisation
// ---------------------------------------------------------------------------

/// 🔴 L'aperçu n'écrit rien chez un service : banc en refus d'écriture.
#[test]
fn l_apercu_d_un_lien_n_ecrit_rien() {
    let mut hote = banc();
    hote.ecriture_interdite = true;
    let liens = Liens::new(&hote);
    let id = liens.creer(&demande(sens::A_VERS_B, 60)).unwrap().lien_id;
    let plan = liens.apercu(&id).unwrap();
    assert_eq!(plan.ajouts.len(), 1);
    assert_eq!(plan.ajouts[0].cible_id, "q-2");
    assert_eq!(plan.ajouts[0].vers, "b");
    assert_eq!(plan.deja_presentes, 1);
    assert!(hote.ajouts().is_empty());
}

/// 🔴 La première synchronisation exige un aperçu, puis un accord ; le
/// minuteur ne touche pas un lien qui n'a jamais été accepté.
#[test]
fn la_premiere_synchro_exige_un_apercu_accepte() {
    let hote = banc();
    let liens = Liens::new(&hote);
    let id = liens.creer(&demande(sens::A_VERS_B, 15)).unwrap().lien_id;

    let err = liens
        .synchroniser(&id, true, declencheur::DEMANDE)
        .unwrap_err();
    assert!(err.starts_with("apercu_requis"), "{err}");

    liens.apercu(&id).unwrap();
    let err = liens
        .synchroniser(&id, false, declencheur::DEMANDE)
        .unwrap_err();
    assert!(err.starts_with("accord_requis"), "{err}");

    hote.avancer(60 * MINUTE);
    assert!(liens.tic().unwrap().is_none(), "le minuteur n'y touche pas");
    assert!(hote.ajouts().is_empty());

    let (lien, entree) = liens.synchroniser(&id, true, declencheur::DEMANDE).unwrap();
    assert_eq!(lien.etat, etat_lien::ACTIF);
    assert_eq!(entree.declencheur, declencheur::PREMIERE);
    assert_eq!(entree.ajoutees, 1);
    assert_eq!(hote.ajouts(), vec![("pl-b".into(), vec!["q-2".into()])]);
}

/// L'accord porte sur l'aperçu : ce qui est apparu depuis attend la suivante.
#[test]
fn la_premiere_synchro_n_ecrit_pas_plus_que_l_apercu() {
    let hote = banc();
    let liens = Liens::new(&hote);
    let id = liens.creer(&demande(sens::A_VERS_B, 15)).unwrap().lien_id;
    liens.apercu(&id).unwrap();
    hote.remplacer_pistes("pl-a", vec![imagine_a(), fragile_a(), roxanne_a()]);
    liens.synchroniser(&id, true, declencheur::DEMANDE).unwrap();
    assert_eq!(hote.ajouts(), vec![("pl-b".into(), vec!["q-2".into()])]);
}

/// 🔴 Un snapshot des deux côtés est écrit AVANT le premier ajout.
#[test]
fn un_snapshot_precede_chaque_synchro() {
    let hote = banc();
    let id = lien_actif(&hote, sens::A_VERS_B, 15);
    let ops = hote.operations();
    let snapshot = ops
        .iter()
        .position(|o| o.starts_with("kv:snap:"))
        .expect("un snapshot");
    let ajout = ops
        .iter()
        .position(|o| o.starts_with("ajout:"))
        .expect("un ajout");
    assert!(snapshot < ajout, "{ops:?}");

    let journal = Liens::new(&hote).journal(&id).unwrap();
    assert_eq!(journal[0].snapshots.len(), 2, "les deux côtés");
}

// ---------------------------------------------------------------------------
// Minuteur : des AJOUTS, jamais de suppression
// ---------------------------------------------------------------------------

#[test]
fn le_minuteur_synchronise_un_lien_du_et_n_ecrit_que_des_ajouts() {
    let hote = banc();
    let id = lien_actif(&hote, sens::A_VERS_B, 15);
    let liens = Liens::new(&hote);

    hote.remplacer_pistes("pl-a", vec![imagine_a(), fragile_a(), roxanne_a()]);
    hote.avancer(MINUTE);
    assert!(liens.tic().unwrap().is_none(), "pas encore dû");

    hote.avancer(15 * MINUTE);
    let (_, entree) = liens.tic().unwrap().expect("dû");
    assert_eq!(entree.declencheur, declencheur::MINUTEUR);
    assert_eq!(entree.ajoutees, 1);
    assert_eq!(
        hote.ajouts().last().unwrap(),
        &("pl-b".into(), vec!["q-4".into()])
    );
    assert_eq!(liens.journal(&id).unwrap().len(), 2);
}

/// 🔴 Une piste retirée de A n'est PAS retirée de B : elle est signalée, une
/// seule fois, et reste en place.
#[test]
fn une_piste_disparue_est_signalee_jamais_retiree() {
    let hote = banc();
    lien_actif(&hote, sens::A_VERS_B, 15);
    let liens = Liens::new(&hote);
    let ajouts_avant = hote.ajouts().len();

    hote.remplacer_pistes("pl-a", vec![imagine_a()]);
    hote.avancer(16 * MINUTE);
    let (_, entree) = liens.tic().unwrap().expect("dû");
    assert_eq!(entree.ajoutees, 0);
    assert_eq!(entree.statut, "rien_a_faire");
    assert_eq!(entree.disparues_signalees.len(), 1);
    assert_eq!(entree.disparues_signalees[0].disparue_de, "a");
    assert_eq!(entree.disparues_signalees[0].id_restant, "q-2");
    assert_eq!(
        hote.ids_de("pl-b"),
        vec!["q-1", "q-2"],
        "rien n'est retiré de B"
    );

    hote.avancer(16 * MINUTE);
    let (_, entree) = liens.tic().unwrap().expect("dû");
    assert!(
        entree.disparues_signalees.is_empty(),
        "signalée une fois, pas à chaque passage"
    );
    assert_eq!(hote.ajouts().len(), ajouts_avant);
    assert_eq!(hote.ids_de("pl-b"), vec!["q-1", "q-2"]);
}

/// 🔴 Une piste que l'utilisateur a retirée de B n'y est pas REMISE.
#[test]
fn une_piste_retiree_de_la_cible_n_y_est_pas_remise() {
    let hote = banc();
    lien_actif(&hote, sens::A_VERS_B, 15);
    hote.remplacer_pistes("pl-b", vec![imagine_b()]);
    hote.avancer(16 * MINUTE);
    let (_, entree) = Liens::new(&hote).tic().unwrap().expect("dû");
    assert_eq!(entree.ajoutees, 0, "q-2 n'est pas remise");
    assert_eq!(entree.disparues_signalees[0].disparue_de, "b");
    assert_eq!(hote.ids_de("pl-b"), vec!["q-1"]);
}

/// Les deux sens : ce qui arrive dans B va dans A, sans ping-pong ensuite.
#[test]
fn le_double_sens_ajoute_de_chaque_cote_sans_ping_pong() {
    let hote = banc();
    lien_actif(&hote, sens::DEUX_SENS, 15);
    let liens = Liens::new(&hote);
    let mut b = hote.contenu("pl-b").unwrap().1;
    b.push(Piste::new("q-5", "Hotel", "Eagles", 391_000));
    hote.remplacer_pistes("pl-b", b);

    hote.avancer(16 * MINUTE);
    let (_, entree) = liens.tic().unwrap().expect("dû");
    assert_eq!(entree.ajoutees, 1);
    assert_eq!(
        hote.ajouts().last().unwrap(),
        &("pl-a".into(), vec!["s-5".into()])
    );

    hote.avancer(16 * MINUTE);
    let (_, entree) = liens.tic().unwrap().expect("dû");
    assert_eq!(entree.statut, "rien_a_faire", "{entree:?}");
}

// ---------------------------------------------------------------------------
// Pause, suppression du lien
// ---------------------------------------------------------------------------

#[test]
fn un_lien_en_pause_n_ecrit_rien() {
    let hote = banc();
    let id = lien_actif(&hote, sens::A_VERS_B, 15);
    let liens = Liens::new(&hote);
    let avant = hote.ajouts().len();
    liens.mettre_en_pause(&id, true).unwrap();
    hote.remplacer_pistes("pl-a", vec![imagine_a(), fragile_a(), roxanne_a()]);
    hote.avancer(60 * MINUTE);
    assert!(liens.tic().unwrap().is_none());
    let err = liens
        .synchroniser(&id, false, declencheur::DEMANDE)
        .unwrap_err();
    assert!(err.starts_with("lien_en_pause"), "{err}");
    assert_eq!(hote.ajouts().len(), avant);

    let repris = liens.mettre_en_pause(&id, false).unwrap();
    assert_eq!(repris.etat, etat_lien::ACTIF);
    let (_, entree) = liens
        .synchroniser(&id, false, declencheur::DEMANDE)
        .unwrap();
    assert_eq!(entree.ajoutees, 1);
}

#[test]
fn supprimer_un_lien_ne_touche_pas_aux_playlists() {
    let hote = banc();
    let id = lien_actif(&hote, sens::A_VERS_B, 15);
    let liens = Liens::new(&hote);
    let a = hote.ids_de("pl-a");
    let b = hote.ids_de("pl-b");
    let ajouts = hote.ajouts().len();

    let r = liens.supprimer(&id).unwrap();
    assert_eq!(r["playlists_touchees"], false);
    assert!(liens.lister().unwrap().is_empty());
    assert!(liens.lire(&id).unwrap_err().starts_with("lien_inconnu"));
    hote.avancer(60 * MINUTE);
    assert!(liens.tic().unwrap().is_none());
    assert_eq!(hote.ids_de("pl-a"), a);
    assert_eq!(hote.ids_de("pl-b"), b);
    assert_eq!(hote.ajouts().len(), ajouts);
}

// ---------------------------------------------------------------------------
// Bibliothèque locale, journal, routes, minuteur par l'événement
// ---------------------------------------------------------------------------

/// Un lien service ↔ bibliothèque : l'ajout local passe par `track_id`, pas
/// par le `source_id` d'origine que porte une piste locale venue d'un service.
#[test]
fn un_lien_vers_la_bibliotheque_ajoute_par_track_id() {
    let hote = banc()
        .avec_playlist(
            "7",
            "Ma sélection",
            vec![Piste::new("12", "Imagine", "John Lennon", 183_000)],
        )
        .avec_verdict_chez(
            "local",
            "Fragile",
            Verdict::exact(Piste::new("13", "Fragile", "Sting", 232_000)),
        );
    let liens = Liens::new(&hote);
    let mut d = demande(sens::A_VERS_B, 0);
    d.b = Extremite {
        service: "local".into(),
        playlist_id: "7".into(),
        nom: String::new(),
    };
    let id = liens.creer(&d).unwrap().lien_id;
    let plan = liens.apercu(&id).unwrap();
    // Imagine n'a pas de verdict local : introuvable. Fragile → 13.
    assert_eq!(plan.ajouts.len(), 1);
    assert_eq!(plan.ajouts[0].cible_id, "13");
    assert_eq!(plan.introuvables.len(), 1);
    liens.synchroniser(&id, true, declencheur::DEMANDE).unwrap();
    assert_eq!(hote.ids_de("7"), vec!["12", "13"]);
}

#[test]
fn le_journal_dit_quand_quoi_combien_et_ce_qui_a_echoue() {
    let hote = banc();
    let id = lien_actif(&hote, sens::A_VERS_B, 15);
    let liens = Liens::new(&hote);
    let journal = liens.journal(&id).unwrap();
    assert_eq!(journal.len(), 1);
    let e = &journal[0];
    assert!(e.quand_ms > 0);
    assert_eq!(e.ajoutees, 1);
    assert_eq!(e.ajouts[0].titre, "Fragile");
    assert!(e.echecs.is_empty());

    // Le service tombe : l'échec est au journal, pas perdu.
    hote.remplacer_pistes("pl-a", vec![imagine_a(), fragile_a(), roxanne_a()]);
    *hote.tomber_apres_ajouts.borrow_mut() = Some(0);
    hote.avancer(16 * MINUTE);
    let (lien, entree) = liens.tic().unwrap().expect("dû");
    assert_eq!(entree.statut, "partiel");
    assert_eq!(entree.ajoutees, 0);
    assert!(
        entree.echecs[0].contains("service_indisponible"),
        "{entree:?}"
    );
    assert!(lien.derniere_erreur.is_some());
    assert_eq!(liens.journal(&id).unwrap().len(), 2);
}

#[test]
fn les_routes_des_liens_et_l_evenement_minuteur() {
    let hote = banc();
    let route = |methode: &str, chemin: &str, query: &str, corps: serde_json::Value| {
        repondre(
            &hote,
            &json!({ "method": methode, "path": chemin, "query": query, "body": corps }),
        )
    };
    let r = route(
        "POST",
        "/liens",
        "",
        json!({
            "a": { "service": "tidal", "playlist_id": "pl-a" },
            "b": { "service": "qobuz", "playlist_id": "pl-b" },
            "sens": "a_vers_b",
            "cadence_minutes": 15,
        }),
    );
    assert_eq!(r["status"], 200, "{r}");
    let id = r["body"]["lien"]["lien_id"].as_str().unwrap().to_string();

    let r = route(
        "POST",
        "/lien/synchroniser",
        "",
        json!({ "lien_id": id, "accord": true }),
    );
    assert_eq!(r["status"], 409, "aperçu requis : {r}");

    let r = route("POST", "/lien/apercu", "", json!({ "lien_id": id }));
    assert_eq!(r["status"], 200);
    assert_eq!(r["body"]["plan"]["ajouts"][0]["cible_id"], "q-2");

    let r = route(
        "POST",
        "/lien/synchroniser",
        "",
        json!({ "lien_id": id, "accord": true }),
    );
    assert_eq!(r["status"], 200, "{r}");
    assert_eq!(r["body"]["entree"]["ajoutees"], 1);

    let r = route(
        "POST",
        "/lien/reglages",
        "",
        json!({ "lien_id": id, "cadence_minutes": 3 }),
    );
    assert_eq!(r["status"], 400);
    let r = route(
        "POST",
        "/lien/reglages",
        "",
        json!({ "lien_id": id, "cadence_minutes": 30 }),
    );
    assert_eq!(r["body"]["lien"]["cadence_minutes"], 30);

    // Le minuteur, par l'événement que l'hôte envoie.
    hote.remplacer_pistes("pl-a", vec![imagine_a(), fragile_a(), roxanne_a()]);
    hote.avancer(31 * MINUTE);
    sur_evenement(&hote, &json!({ "name": "autre", "payload": {} }));
    assert_eq!(hote.ajouts().len(), 1, "un autre événement ne fait rien");
    sur_evenement(&hote, &json!({ "name": "minuteur", "payload": {} }));
    assert_eq!(hote.ajouts().len(), 2);

    let r = route("GET", "/lien/journal", &format!("id={id}"), json!(null));
    assert_eq!(r["body"]["count"], 2);
    assert_eq!(r["body"]["entrees"][0]["declencheur"], "minuteur");

    let r = route(
        "POST",
        "/lien/pause",
        "",
        json!({ "lien_id": id, "pause": true }),
    );
    assert_eq!(r["body"]["lien"]["etat"], "en_pause");
    let r = route("POST", "/lien/synchroniser", "", json!({ "lien_id": id }));
    assert_eq!(r["status"], 409);

    let r = route("POST", "/lien/supprimer", "", json!({ "lien_id": id }));
    assert_eq!(r["status"], 200);
    let r = route("GET", "/lien", &format!("id={id}"), json!(null));
    assert_eq!(r["status"], 404);
    let r = route("GET", "/liens", "", json!(null));
    assert_eq!(r["body"]["count"], 0);
}
