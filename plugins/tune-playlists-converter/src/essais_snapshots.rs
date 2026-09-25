//! Les essais des snapshots (#4718), joués contre l'hôte de banc.
//!
//! Chacun répond à une exigence du ticket : copie datée AVANT tout transfert,
//! lister / consulter, retour en arrière qui ne supprime jamais rien et ne se
//! fait jamais sans accord, rétention bornée.

use serde_json::json;

use crate::banc::{HEURE_DU_BANC_MS, HoteDeBanc, Piste, Verdict};
use crate::dispatch::repondre;
use crate::modele::Demande;
use crate::moteur::Convertisseur;
use crate::snapshots::{RETENTION_PAR_PLAYLIST, Snapshots, mode};

fn trois() -> Vec<Piste> {
    vec![
        Piste::new("s-1", "Imagine", "John Lennon", 183_000),
        Piste::new("s-2", "Come Together", "The Beatles", 259_000),
        Piste::new("s-3", "Fragile", "Sting", 232_000),
    ]
}

fn banc() -> HoteDeBanc {
    HoteDeBanc::new()
        .avec_playlist("pl-1", "Nuit blanche", trois())
        .avec_verdict(
            "Imagine",
            Verdict::exact(Piste::new("q-1", "Imagine", "John Lennon", 184_000)),
        )
}

// ---------------------------------------------------------------------------
// Avant tout transfert
// ---------------------------------------------------------------------------

/// 🔴 Le snapshot de la playlist visée est écrit AVANT le premier titre versé.
/// On ne lit pas le code : on regarde l'ordre des opérations reçues par l'hôte.
#[test]
fn le_transfert_prend_un_snapshot_avant_le_premier_ajout() {
    let hote = banc();
    let moteur = Convertisseur::new(&hote);
    let lot = moteur
        .apercu(&Demande {
            source_service: "tidal".into(),
            cible_service: "qobuz".into(),
            playlists: vec!["pl-1".into()],
            suffixe_nom: None,
        })
        .unwrap();
    let lot = moteur.transferer(&lot.lot_id, true).unwrap();

    let ops = hote.operations();
    let premier_snapshot = ops
        .iter()
        .position(|o| o.starts_with("kv:snap:"))
        .expect("un snapshot est écrit");
    let premier_ajout = ops
        .iter()
        .position(|o| o.starts_with("ajout:"))
        .expect("un titre est versé");
    assert!(
        premier_snapshot < premier_ajout,
        "snapshot AVANT le versement — vu {ops:?}"
    );

    let id = lot.playlists[0]
        .snapshot_avant
        .clone()
        .expect("le lot garde l'identifiant du snapshot");
    let snap = Snapshots::new(&hote).lire(&id).unwrap();
    assert_eq!(snap.entete.service, "qobuz");
    assert_eq!(snap.entete.playlist_id, "cible-1");
    assert_eq!(snap.entete.motif, format!("avant_transfert:{}", lot.lot_id));
    assert_eq!(snap.entete.pris_le_ms, HEURE_DU_BANC_MS, "daté par l'hôte");
    assert!(snap.pistes.is_empty(), "la playlist vient d'être créée");
}

/// Sans snapshot possible, pas d'écriture : le stockage qui refuse arrête la
/// playlist avant le premier titre.
#[test]
fn sans_snapshot_rien_n_est_verse() {
    // Une page de snapshot trop grosse pour le stockage : impossible ici avec
    // une cible vide, donc on force le cas par une playlist visée existante
    // introuvable au moment de la relire (reprise d'un lot ancien).
    let hote = banc();
    let moteur = Convertisseur::new(&hote);
    let lot = moteur
        .apercu(&Demande {
            source_service: "tidal".into(),
            cible_service: "qobuz".into(),
            playlists: vec!["pl-1".into()],
            suffixe_nom: None,
        })
        .unwrap();
    // Un lot écrit AVANT #4718 : cible déjà créée, aucun snapshot, et la
    // playlist visée n'est plus lisible chez le service.
    let mut pl = moteur.lire_le_lot(&lot.lot_id).unwrap().playlists[0].clone();
    pl.cible_playlist_id = Some("disparue".into());
    pl.etat = "interrompu".into();
    use crate::hote::Hote;
    hote.kv_set(
        &format!("lot:{}:pl:0", lot.lot_id),
        &serde_json::to_value(&pl).unwrap(),
    )
    .unwrap();
    let mut entete: serde_json::Value =
        hote.kv_get(&format!("lot:{}", lot.lot_id)).unwrap()["value"].clone();
    entete["etat"] = json!("interrompu");
    hote.kv_set(&format!("lot:{}", lot.lot_id), &entete)
        .unwrap();

    let repris = moteur.reprendre(&lot.lot_id).unwrap();
    assert_eq!(repris.playlists[0].etat, "interrompu");
    assert!(
        hote.ajouts().is_empty(),
        "aucun titre versé sans copie préalable"
    );
}

// ---------------------------------------------------------------------------
// Prendre, lister, lire, rétention
// ---------------------------------------------------------------------------

#[test]
fn un_snapshot_manuel_se_liste_et_se_relit() {
    let hote = banc();
    let s = Snapshots::new(&hote);
    let e = s.prendre("tidal", "pl-1", None, "manuel").unwrap();
    assert_eq!(e.nom, "Nuit blanche", "nom lu chez le service");
    assert_eq!(e.total, 3);

    let liste = s.lister("tidal", "pl-1").unwrap();
    assert_eq!(liste.len(), 1);
    assert_eq!(liste[0].snapshot_id, e.snapshot_id);

    let complet = s.lire(&e.snapshot_id).unwrap();
    let ids: Vec<&str> = complet.pistes.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, vec!["s-1", "s-2", "s-3"]);
    assert_eq!(complet.pistes[1].titre, "Come Together");

    let playlists = s.playlists().unwrap();
    assert_eq!(playlists.len(), 1);
    assert_eq!(playlists[0]["snapshots"], 1);
    assert!(hote.creations().is_empty() && hote.ajouts().is_empty());
}

/// Une copie identique à la précédente n'occupe pas d'emplacement.
#[test]
fn une_copie_identique_n_occupe_pas_d_emplacement() {
    let hote = banc();
    let s = Snapshots::new(&hote);
    let a = s.prendre("tidal", "pl-1", None, "manuel").unwrap();
    hote.avancer(60_000);
    let b = s.prendre("tidal", "pl-1", None, "manuel").unwrap();
    assert_eq!(a.snapshot_id, b.snapshot_id);
    assert_eq!(s.lister("tidal", "pl-1").unwrap().len(), 1);
}

/// La rétention est un anneau : au-delà de dix, le plus ancien est réécrit,
/// et le demander le dit clairement.
#[test]
fn la_retention_garde_les_dix_derniers() {
    let hote = banc();
    let s = Snapshots::new(&hote);
    let mut ids = Vec::new();
    for i in 0..12 {
        let mut pistes = trois();
        pistes.push(Piste::new(&format!("x-{i}"), "Varie", "X", 100_000));
        hote.remplacer_pistes("pl-1", pistes);
        hote.avancer(1_000);
        ids.push(
            s.prendre("tidal", "pl-1", None, "manuel")
                .unwrap()
                .snapshot_id,
        );
    }
    let liste = s.lister("tidal", "pl-1").unwrap();
    assert_eq!(liste.len() as u64, RETENTION_PAR_PLAYLIST);
    assert_eq!(liste[0].snapshot_id, ids[11], "le plus récent d'abord");
    let err = s.lire(&ids[0]).unwrap_err();
    assert!(err.starts_with("snapshot_expire"), "{err}");
    assert!(s.lire(&ids[2]).is_ok());
}

/// Une grande playlist se range en pages : la borne de 256 Kio par valeur ne
/// doit pas empêcher de la garder.
#[test]
fn une_grande_playlist_se_garde_en_pages() {
    let pistes: Vec<Piste> = (0..950)
        .map(|i| {
            Piste::new(
                &format!("s-{i}"),
                &format!("Un titre assez long pour peser {i}"),
                "Artiste",
                200_000,
            )
        })
        .collect();
    let hote = HoteDeBanc::new().avec_playlist("pl-9", "Longue", pistes);
    let s = Snapshots::new(&hote);
    let e = s.prendre("tidal", "pl-9", None, "manuel").unwrap();
    assert_eq!(e.pages, 3);
    let relu = s.lire(&e.snapshot_id).unwrap();
    assert_eq!(relu.pistes.len(), 950);
    assert_eq!(relu.pistes[949].id, "s-949");
}

// ---------------------------------------------------------------------------
// Retour en arrière — sans suppression, jamais sans accord
// ---------------------------------------------------------------------------

/// L'utilisateur a retiré `s-2` et ajouté `s-4` après le snapshot.
fn banc_retouche() -> (HoteDeBanc, String) {
    let hote = banc();
    let id = Snapshots::new(&hote)
        .prendre("tidal", "pl-1", None, "manuel")
        .unwrap()
        .snapshot_id;
    hote.remplacer_pistes(
        "pl-1",
        vec![
            Piste::new("s-1", "Imagine", "John Lennon", 183_000),
            Piste::new("s-3", "Fragile", "Sting", 232_000),
            Piste::new("s-4", "Roxanne", "The Police", 192_000),
        ],
    );
    (hote, id)
}

/// 🔴 L'aperçu dit ce qui sera rajouté et ce que l'UTILISATEUR devra retirer
/// lui-même — et il n'écrit rien : le banc est en refus d'écriture.
#[test]
fn l_apercu_de_restauration_n_ecrit_rien_et_liste_ce_qui_est_en_trop() {
    let (mut hote, id) = banc_retouche();
    hote.ecriture_interdite = true;
    let (plan, a_rajouter, a_retirer) = Snapshots::new(&hote)
        .apercu_restauration(&id, mode::COMPLETER)
        .unwrap();
    assert_eq!(plan.a_rajouter_ids, vec!["s-2"]);
    assert_eq!(plan.a_retirer_par_vous_ids, vec!["s-4"]);
    assert_eq!(plan.deja_presentes, 2);
    assert_eq!(a_rajouter[0].titre, "Come Together");
    assert_eq!(a_retirer[0].titre, "Roxanne");
    assert!(plan.avertissement.contains("retirer vous-même"));
    assert!(hote.ajouts().is_empty() && hote.creations().is_empty());
}

/// 🔴 Sans accord, rien. Avec accord : la piste manquante est RAJOUTÉE, la
/// piste en trop est TOUJOURS là — Tune ne l'a pas retirée, il n'en a pas le
/// moyen.
#[test]
fn le_retour_en_arriere_rajoute_et_ne_supprime_rien() {
    let (hote, id) = banc_retouche();
    let s = Snapshots::new(&hote);
    let (plan, _, _) = s.apercu_restauration(&id, mode::COMPLETER).unwrap();

    let err = s.restaurer(&plan.plan_id, false).unwrap_err();
    assert!(err.starts_with("accord_requis"), "{err}");
    assert!(hote.ajouts().is_empty());

    let (fait, a_retirer) = s.restaurer(&plan.plan_id, true).unwrap();
    assert_eq!(fait.etat, "termine");
    assert_eq!(hote.ajouts(), vec![("pl-1".into(), vec!["s-2".into()])]);
    assert!(hote.creations().is_empty());
    assert_eq!(
        hote.ids_de("pl-1"),
        vec!["s-1", "s-3", "s-4", "s-2"],
        "s-4 reste : rien n'est supprimé ; s-2 revient en fin de playlist"
    );
    assert_eq!(a_retirer.len(), 1);
    assert_eq!(a_retirer[0].id, "s-4");

    // Le retour en arrière a lui-même gardé l'état d'avant.
    let avant = fait
        .snapshot_avant_restauration
        .expect("copie de l'état courant avant d'écrire");
    let ids: Vec<String> = s
        .lire(&avant)
        .unwrap()
        .pistes
        .into_iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(ids, vec!["s-1", "s-3", "s-4"]);

    let err = s.restaurer(&plan.plan_id, true).unwrap_err();
    assert!(err.starts_with("plan_deja_engage"), "{err}");
}

/// L'accord porte sur l'aperçu, pas sur ce qui a changé depuis : une piste
/// retirée APRÈS l'aperçu n'est pas rajoutée.
#[test]
fn la_restauration_ne_fait_jamais_plus_que_l_apercu() {
    let (hote, id) = banc_retouche();
    let s = Snapshots::new(&hote);
    let (plan, _, _) = s.apercu_restauration(&id, mode::COMPLETER).unwrap();
    hote.remplacer_pistes(
        "pl-1",
        vec![Piste::new("s-1", "Imagine", "John Lennon", 183_000)],
    );
    s.restaurer(&plan.plan_id, true).unwrap();
    assert_eq!(hote.ajouts(), vec![("pl-1".into(), vec!["s-2".into()])]);
}

/// Le mode « recréer » crée une NOUVELLE playlist et laisse l'ancienne.
#[test]
fn recreer_cree_une_nouvelle_playlist_et_laisse_l_ancienne() {
    let (hote, id) = banc_retouche();
    let s = Snapshots::new(&hote);
    let (plan, _, a_retirer) = s.apercu_restauration(&id, mode::RECREER).unwrap();
    assert!(a_retirer.is_empty());
    let (fait, _) = s.restaurer(&plan.plan_id, true).unwrap();
    assert_eq!(
        hote.creations(),
        vec![("cible-1".into(), "Nuit blanche".into())]
    );
    assert_eq!(fait.playlist_recreee_id.as_deref(), Some("cible-1"));
    assert_eq!(hote.ids_de("cible-1"), vec!["s-1", "s-2", "s-3"]);
    assert_eq!(hote.ids_de("pl-1"), vec!["s-1", "s-3", "s-4"], "intacte");
}

/// Une playlist LOCALE se garde et se complète par ses identifiants entiers.
#[test]
fn une_playlist_locale_se_garde_et_se_complete() {
    let hote = HoteDeBanc::new().avec_playlist(
        "7",
        "Ma sélection",
        vec![
            Piste::new("12", "Imagine", "John Lennon", 183_000),
            Piste::new("13", "Fragile", "Sting", 232_000),
        ],
    );
    let s = Snapshots::new(&hote);
    let id = s.prendre("local", "7", None, "manuel").unwrap().snapshot_id;
    hote.remplacer_pistes(
        "7",
        vec![Piste::new("12", "Imagine", "John Lennon", 183_000)],
    );
    let (plan, _, _) = s.apercu_restauration(&id, mode::COMPLETER).unwrap();
    assert_eq!(plan.a_rajouter_ids, vec!["13"]);
    s.restaurer(&plan.plan_id, true).unwrap();
    assert_eq!(hote.ids_de("7"), vec!["12", "13"]);
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

fn route(
    hote: &HoteDeBanc,
    methode: &str,
    chemin: &str,
    query: &str,
    corps: serde_json::Value,
) -> serde_json::Value {
    repondre(
        hote,
        &json!({ "method": methode, "path": chemin, "query": query, "body": corps }),
    )
}

#[test]
fn les_routes_des_snapshots_rendent_les_codes_attendus() {
    let (hote, _) = banc_retouche();

    let r = route(
        &hote,
        "POST",
        "/snapshot",
        "",
        json!({ "service": "tidal", "playlist_id": "pl-1" }),
    );
    assert_eq!(r["status"], 200, "{r}");
    let id = r["body"]["snapshot"]["snapshot_id"]
        .as_str()
        .unwrap()
        .to_string();

    let r = route(
        &hote,
        "GET",
        "/snapshots",
        "service=tidal&playlist_id=pl%2D1",
        json!(null),
    );
    assert_eq!(r["status"], 200, "{r}");
    assert_eq!(r["body"]["count"], 2, "le décodage %XX retrouve pl-1");
    assert_eq!(r["body"]["retention_par_playlist"], 10);

    let r = route(&hote, "GET", "/snapshots", "", json!(null));
    assert_eq!(r["body"]["count"], 1);

    let r = route(&hote, "GET", "/snapshot", &format!("id={id}"), json!(null));
    assert_eq!(r["status"], 200);
    assert_eq!(r["body"]["snapshot"]["pistes"].as_array().unwrap().len(), 3);

    let r = route(&hote, "GET", "/snapshot", "id=snap-99-1", json!(null));
    assert_eq!(r["status"], 404);

    let r = route(
        &hote,
        "POST",
        "/snapshot/restauration/apercu",
        "",
        json!({ "snapshot_id": "snap-1-1", "mode": "completer" }),
    );
    assert_eq!(r["status"], 200, "{r}");
    assert_eq!(r["body"]["a_rajouter"][0]["id"], "s-2");
    assert_eq!(r["body"]["a_retirer_par_vous"][0]["id"], "s-4");
    let plan_id = r["body"]["plan"]["plan_id"].as_str().unwrap().to_string();

    let r = route(
        &hote,
        "POST",
        "/snapshot/restauration",
        "",
        json!({ "plan_id": plan_id }),
    );
    assert_eq!(r["status"], 409, "sans accord");
    assert!(hote.ajouts().is_empty());

    let r = route(
        &hote,
        "POST",
        "/snapshot/restauration",
        "",
        json!({ "plan_id": plan_id, "accord": true }),
    );
    assert_eq!(r["status"], 200, "{r}");
    assert_eq!(r["body"]["plan"]["etat"], "termine");

    let r = route(
        &hote,
        "POST",
        "/snapshot/restauration/apercu",
        "",
        json!({ "snapshot_id": "snap-1-1", "mode": "effacer" }),
    );
    assert_eq!(r["status"], 400, "aucun mode destructeur");
}
