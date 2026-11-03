//! Témoins des « Albums manquants » (#6288) : MusicBrainz simulé, aucun appel
//! réseau. La forme des pages est celle de
//! `release-group?artist=…&type=album|ep&fmt=json`, relevée le 20/10/2026.

use std::sync::Mutex;

use super::*;
use crate::db::sqlite::SqliteDb;
use crate::test_scratch::{ScratchDir, scratch_dir};

const ARTISTE: &str = "83b9cbe7-9857-49e2-ab8e-b57b01038103";

fn groupe(id: u32, titre: &str, primaire: &str, secondaires: &[&str], date: &str) -> Value {
    json!({
        "id": format!("00000000-0000-0000-0000-{id:012}"),
        "title": titre,
        "primary-type": primaire,
        "secondary-types": secondaires,
        "first-release-date": date,
        "disambiguation": ""
    })
}

fn id(n: u32) -> String {
    format!("00000000-0000-0000-0000-{n:012}")
}

/// Cinq groupes, servis deux par page (`PAGE` simulé), total annoncé : 5.
fn page(offset: u32) -> Value {
    let tous = [
        groupe(
            1,
            "The Piper at the Gates of Dawn",
            "Album",
            &[],
            "1967-08-05",
        ),
        groupe(2, "Meddle", "Album", &[], "1971-10-30"),
        groupe(3, "Pulse", "Album", &["Live"], "1995-05-29"),
        groupe(
            4,
            "Echoes: The Best of Pink Floyd",
            "Album",
            &["Compilation"],
            "2001-11-05",
        ),
        groupe(5, "Atom Heart Mother", "Album", &[], "1970-10-02"),
    ];
    let debut = (offset / PAGE) as usize * 2;
    let items: Vec<Value> = tous.iter().skip(debut).take(2).cloned().collect();
    json!({"release-group-count": 5, "release-group-offset": offset, "release-groups": items})
}

fn base() -> (Arc<dyn DbBackend>, ScratchDir) {
    let dir = scratch_dir("albums-manquants-6288");
    let chemin = dir.path().join("tune.db");
    let db = SqliteDb::open(chemin.to_str().unwrap()).unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    (Arc::new(db), dir)
}

fn lue(l: Lecture) -> Discographie {
    match l {
        Lecture::Lue(d) => d,
        autre => panic!("une discographie était attendue : {autre:?}"),
    }
}

#[tokio::test]
async fn la_discographie_se_lit_page_par_page_jusqu_au_total_6288() {
    let appels: Mutex<Vec<(String, u32)>> = Mutex::new(Vec::new());
    let lire = |mbid: String, offset: u32| {
        appels.lock().unwrap().push((mbid, offset));
        std::future::ready(Ok(Some(page(offset))))
    };
    let d = lue(lire_discographie_par(ARTISTE, &lire, 10).await);
    assert_eq!(d.groupes.len(), 5);
    assert!(!d.tronquee);
    assert_eq!(
        *appels.lock().unwrap(),
        vec![
            (ARTISTE.to_string(), 0),
            (ARTISTE.to_string(), PAGE),
            (ARTISTE.to_string(), 2 * PAGE)
        ]
    );
}

#[tokio::test]
async fn compilations_et_live_sont_exclus_par_defaut_et_la_bibliotheque_soustraite_6288() {
    let lire = |_m: String, offset: u32| std::future::ready(Ok(Some(page(offset))));
    let d = lue(lire_discographie_par(ARTISTE, &lire, 10).await);
    let bibliotheque = vec![
        // Identifié : par le groupe de parutions.
        AlbumPossede {
            titre: "Meddle (2011 Remaster)".into(),
            groupe_mbid: Some(id(2)),
        },
        // Non identifié : par le titre normalisé (casse, accents, mentions).
        AlbumPossede {
            titre: "ATOM HEART MOTHER [Remastered]".into(),
            groupe_mbid: None,
        },
    ];
    let b = comparer(&d, &bibliotheque, false);
    assert_eq!(
        b.manquants
            .iter()
            .map(|g| g.titre.as_str())
            .collect::<Vec<_>>(),
        vec!["The Piper at the Gates of Dawn"],
        "seuls les albums studio non possédés doivent manquer"
    );
    assert_eq!((b.possedes, b.retenus), (2, 3));

    let b = comparer(&d, &bibliotheque, true);
    assert_eq!(
        b.manquants
            .iter()
            .map(|g| g.titre.as_str())
            .collect::<Vec<_>>(),
        vec![
            "The Piper at the Gates of Dawn",
            "Pulse",
            "Echoes: The Best of Pink Floyd"
        ],
        "avec les secondaires, live et compilation s'ajoutent, par date"
    );
}

#[test]
fn le_titre_normalise_ignore_casse_accents_ponctuation_et_mentions_6288() {
    assert_eq!(
        titre_normalise("Électrique  (Deluxe Édition)"),
        "electrique"
    );
    assert_eq!(titre_normalise("Wish You Were Here!"), "wish you were here");
    assert_eq!(titre_normalise("(Untitled)"), "untitled");
}

#[tokio::test]
async fn artiste_inconnu_panne_et_identifiant_forge_6288() {
    let inconnu = |_m: String, _o: u32| std::future::ready(Ok(None));
    assert_eq!(
        lire_discographie_par(ARTISTE, &inconnu, 1).await,
        Lecture::Inconnu
    );
    let panne = |_m: String, _o: u32| std::future::ready(Err(SansReponse));
    assert_eq!(
        lire_discographie_par(ARTISTE, &panne, 1).await,
        Lecture::Indisponible
    );
    // Panne à la deuxième page : rien n'est conclu.
    let a_moitie = |_m: String, o: u32| {
        std::future::ready(if o == 0 {
            Ok(Some(page(0)))
        } else {
            Err(SansReponse)
        })
    };
    assert_eq!(
        lire_discographie_par(ARTISTE, &a_moitie, 1).await,
        Lecture::Indisponible
    );
    let jamais = |_m: String, _o: u32| -> std::future::Ready<Page> {
        panic!("un identifiant forgé ne doit jamais partir en requête")
    };
    assert_eq!(
        lire_discographie_par("../x?inc=", &jamais, 1).await,
        Lecture::Inconnu
    );
}

#[tokio::test]
async fn un_artiste_tres_prolifique_est_tronque_et_le_dit_6288() {
    let sans_fin = |_m: String, offset: u32| {
        let items: Vec<Value> = (0..PAGE)
            .map(|i| {
                groupe(
                    offset + i + 1,
                    &format!("Album {}", offset + i),
                    "Album",
                    &[],
                    "2000",
                )
            })
            .collect();
        std::future::ready(Ok(Some(
            json!({"release-group-count": 100_000, "release-groups": items}),
        )))
    };
    let d = lue(lire_discographie_par(ARTISTE, &sans_fin, 1).await);
    assert!(d.tronquee);
    assert_eq!(d.groupes.len() as u32, PAGES_MAX * PAGE);
}

#[tokio::test]
async fn la_discographie_est_gardee_puis_relue_a_expiration_6288() {
    let (backend, _dir) = base();
    let appels: Mutex<u32> = Mutex::new(0);
    let lire = |_m: String, offset: u32| {
        *appels.lock().unwrap() += 1;
        std::future::ready(Ok(Some(page(offset))))
    };
    lue(discographie_par(&backend, ARTISTE, &lire, 1_000).await);
    assert_eq!(*appels.lock().unwrap(), 3);
    lue(discographie_par(&backend, ARTISTE, &lire, 1_000 + 86_400).await);
    assert_eq!(
        *appels.lock().unwrap(),
        3,
        "la discographie gardée a été relue"
    );
    lue(discographie_par(
        &backend,
        ARTISTE,
        &lire,
        1_000 + VALIDITE_JOURS * 86_400 + 1,
    )
    .await);
    assert_eq!(*appels.lock().unwrap(), 6);
    // Une panne n'est pas gardée.
    let (backend, _dir2) = base();
    let panne = |_m: String, _o: u32| std::future::ready(Err(SansReponse));
    discographie_par(&backend, ARTISTE, &panne, 1).await;
    assert!(gardee(&backend, ARTISTE, 1).is_none());
}
