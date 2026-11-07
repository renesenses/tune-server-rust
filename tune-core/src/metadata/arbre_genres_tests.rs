//! Témoins de l'arbre des genres (#6001) : MusicBrainz et Wikidata simulés,
//! aucun appel réseau. Les formes de réponse sont celles relevées le
//! 20/10/2026 (`genre/all?fmt=json`, SPARQL `format=json`).

use std::sync::Mutex;

use super::*;
use crate::db::sqlite::SqliteDb;
use crate::test_scratch::{ScratchDir, scratch_dir};

const SKA: &str = "65877068-2116-4af6-89a2-838179a7e103";
const DEUX_TONE: &str = "18797864-41b0-4602-8241-adbee761774c";
const THIRD_WAVE: &str = "18d6ad2c-83d2-402f-972c-dd75b4b2f34b";
const ROCK: &str = "0e3fc579-2d24-4f20-9dae-736e1ec78798";
const PROG: &str = "dbd0aa1c-3b06-4b37-ab3b-4bd1a2a5e1d2";

fn genre(nom: &str, id: &str) -> Value {
    json!({"name": nom, "id": id, "disambiguation": ""})
}

/// Trois pages de `genre/all` (`limit=2` simulé), total annoncé : 5.
fn page(offset: u32) -> Value {
    let tous = [
        genre("2 tone", DEUX_TONE),
        genre("progressive rock", PROG),
        genre("rock", ROCK),
        genre("ska", SKA),
        genre("third wave ska", THIRD_WAVE),
    ];
    let debut = (offset / PAGE_GENRES) as usize * 2;
    let items: Vec<Value> = tous.iter().skip(debut).take(2).cloned().collect();
    json!({"genre-count": 5, "genre-offset": offset, "genres": items})
}

fn ligne(enfant: &str, parent: &str) -> Value {
    json!({"enfant": {"type": "literal", "value": enfant},
           "parent": {"type": "literal", "value": parent}})
}

/// Wikidata simulé : ska ⊂ rock ; 2 tone, third wave ska ⊂ ska ;
/// progressive rock ⊂ rock. Rend les arêtes qui touchent un identifiant
/// demandé, comme la requête réelle.
fn sparql(url: &str) -> Value {
    let toutes = [
        (SKA, ROCK),
        (DEUX_TONE, SKA),
        (THIRD_WAVE, SKA),
        (PROG, ROCK),
    ];
    let lignes: Vec<Value> = toutes
        .iter()
        .filter(|(e, p)| url.contains(e) || url.contains(p))
        .map(|(e, p)| ligne(e, p))
        .collect();
    json!({"head": {"vars": ["enfant", "parent"]}, "results": {"bindings": lignes}})
}

fn base() -> (Arc<dyn DbBackend>, ScratchDir) {
    let dir = scratch_dir("arbre-genres-6001");
    let chemin = dir.path().join("tune.db");
    let db = SqliteDb::open(chemin.to_str().unwrap()).unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    (Arc::new(db), dir)
}

#[tokio::test]
async fn la_liste_se_lit_page_par_page_jusqu_au_total_6001() {
    let offsets: Mutex<Vec<u32>> = Mutex::new(Vec::new());
    let lire = |offset: u32| {
        offsets.lock().unwrap().push(offset);
        std::future::ready(Ok(page(offset)))
    };
    let index = lire_index_par(&lire).await.unwrap();
    assert_eq!(index.len(), 5);
    assert_eq!(
        *offsets.lock().unwrap(),
        vec![0, PAGE_GENRES, 2 * PAGE_GENRES],
        "la lecture doit s'arrêter au total annoncé, pas avant ni après"
    );
}

#[tokio::test]
async fn une_page_manquante_ne_donne_pas_une_liste_partielle_6001() {
    let lire = |offset: u32| {
        std::future::ready(if offset == 0 {
            Ok(page(0))
        } else {
            Err(SansReponse)
        })
    };
    assert!(lire_index_par(&lire).await.is_err());
}

#[test]
fn la_requete_sparql_ne_porte_que_des_identifiants_plausibles_6001() {
    let q = requete_sparql(&[SKA.to_string(), "\" } ; DROP".to_string()]);
    assert!(q.contains(&format!("\"{SKA}\"")));
    assert!(!q.contains("DROP"), "{q}");
    assert!(q.contains("wdt:P8052") && q.contains("wdt:P279"));
}

#[tokio::test]
async fn l_arbre_rapproche_la_bibliotheque_et_nomme_parents_et_sous_genres_6001() {
    let lire_page = |offset: u32| std::future::ready(Ok(page(offset)));
    let index = lire_index_par(&lire_page).await.unwrap();
    let locaux = vec![
        "Ska".to_string(),
        "Progressive-Rock".to_string(),
        "Chanson française".to_string(),
    ];
    let (trouves, autres) = rapprocher(&locaux, &index);
    assert_eq!(autres, vec!["Chanson française"]);

    let (backend, _dir) = base();
    let demandes: Mutex<u32> = Mutex::new(0);
    let lire = |url: String| {
        *demandes.lock().unwrap() += 1;
        std::future::ready(Some(sparql(&url)))
    };
    let (voisins, complet) = voisins_par(&backend, &trouves, &lire, 1_000).await;
    assert!(complet);
    let noeuds = assembler(&trouves, &index, &voisins);
    assert_eq!(
        noeuds,
        vec![
            Noeud {
                genre: "Ska".into(),
                mbid: SKA.into(),
                nom_musicbrainz: "ska".into(),
                parents: vec!["rock".into()],
                sous_genres: vec!["2 tone".into(), "third wave ska".into()],
            },
            Noeud {
                genre: "Progressive-Rock".into(),
                mbid: PROG.into(),
                nom_musicbrainz: "progressive rock".into(),
                parents: vec!["rock".into()],
                sous_genres: vec![],
            },
        ]
    );

    // Gardé : un second passage n'interroge pas Wikidata.
    let (voisins2, _) = voisins_par(&backend, &trouves, &lire, 2_000).await;
    assert_eq!(
        *demandes.lock().unwrap(),
        1,
        "la hiérarchie gardée a été redemandée"
    );
    assert_eq!(assembler(&trouves, &index, &voisins2), noeuds);

    // Expiré : redemandé.
    let tard = 1_000 + VALIDITE_HIERARCHIE_JOURS * 86_400 + 1;
    voisins_par(&backend, &trouves, &lire, tard).await;
    assert_eq!(*demandes.lock().unwrap(), 2);
}

#[tokio::test]
async fn une_panne_de_wikidata_ne_garde_rien_et_le_dit_6001() {
    let lire_page = |offset: u32| std::future::ready(Ok(page(offset)));
    let index = lire_index_par(&lire_page).await.unwrap();
    let (trouves, _) = rapprocher(&["ska".to_string()], &index);
    let (backend, _dir) = base();
    let panne = |_url: String| std::future::ready(None::<Value>);
    let (voisins, complet) = voisins_par(&backend, &trouves, &panne, 1_000).await;
    assert!(!complet);
    assert!(voisins.is_empty());
    // Wikidata revient : il est interrogé, rien de vide n'a été gardé.
    let lire = |url: String| std::future::ready(Some(sparql(&url)));
    let (voisins, complet) = voisins_par(&backend, &trouves, &lire, 1_001).await;
    assert!(complet);
    assert_eq!(voisins[SKA].parents.len(), 1);
}

#[test]
fn la_liste_gardee_expire_6001() {
    let (backend, _dir) = base();
    let index = vec![GenreMb {
        nom: "ska".into(),
        mbid: SKA.into(),
    }];
    assert!(index_garde(&backend, 0).is_none());
    garder_index(&backend, &index, 100);
    assert_eq!(index_garde(&backend, 100 + 86_400), Some(index));
    assert!(index_garde(&backend, 100 + VALIDITE_INDEX_JOURS * 86_400 + 1).is_none());
}
