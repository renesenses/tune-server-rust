//! #6773 — témoins du rattrapage du nom de tri MusicBrainz et de la
//! déduction « Nom, Prénom ». Réponses MusicBrainz FACTICES, aucune requête.

use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::db::artist_repo::{ArtistRepo, SOURCE_TRI_MUSICBRAINZ};
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::migrations;
use crate::db::sqlite::SqliteDb;

const MB_CECCARELLI: &str = "0b5b9b6c-6c1a-4c55-9a0b-7a3f4a6e0001";
const MB_KIDJO: &str = "0b5b9b6c-6c1a-4c55-9a0b-7a3f4a6e0002";
const MB_ANDRE: &str = "0b5b9b6c-6c1a-4c55-9a0b-7a3f4a6e0003";
const MB_SAISI: &str = "0b5b9b6c-6c1a-4c55-9a0b-7a3f4a6e0004";
const MB_AUTRE: &str = "0b5b9b6c-6c1a-4c55-9a0b-7a3f4a6e0005";

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn artiste(b: &Arc<dyn DbBackend>, nom: &str, mbid: Option<&str>, tri: Option<&str>) -> i64 {
    b.execute_returning_id(
        "INSERT INTO artists (name, musicbrainz_id, sort_name) VALUES (?, ?, ?)",
        &[
            &nom as &dyn ToSqlValue,
            &mbid as &dyn ToSqlValue,
            &tri as &dyn ToSqlValue,
        ],
    )
    .unwrap()
}

fn tri_et_source(b: &Arc<dyn DbBackend>, id: i64) -> (Option<String>, Option<String>) {
    let l = b
        .query_one(
            "SELECT sort_name, sort_name_source FROM artists WHERE id = ?",
            &[&id as &dyn ToSqlValue],
        )
        .unwrap()
        .unwrap();
    (
        l.first().and_then(|v| v.as_string()),
        l.get(1).and_then(|v| v.as_string()),
    )
}

/// La fiche `GET /artist/{mbid}?inc=aliases` telle que MusicBrainz la rend.
fn fiche_mb(mbid: &str) -> Value {
    match mbid {
        MB_CECCARELLI => json!({"id": mbid, "name": "André Ceccarelli",
            "sort-name": "Ceccarelli, André", "type": "Person"}),
        MB_KIDJO => json!({"id": mbid, "name": "Angélique Kidjo",
            "sort-name": "Kidjo, Angélique", "type": "Person"}),
        MB_ANDRE => json!({"id": mbid, "name": "Maurice André",
            "sort-name": "André, Maurice", "type": "Person"}),
        MB_SAISI => json!({"id": mbid, "name": "Michel Petrucciani",
            "sort-name": "Petrucciani, Michel", "type": "Person"}),
        // Un MBID mal rattaché : la fiche ne nomme pas l'artiste local.
        _ => json!({"id": mbid, "name": "Quelqu'un d'autre",
            "sort-name": "Autre, Quelqu'un", "type": "Person"}),
    }
}

/// #6773, point 1 — les trois artistes du signalement, déjà identifiés
/// (MBID connu, nom de tri vide) : le rattrapage pose le nom de tri
/// MusicBrainz. Une saisie n'est jamais écrasée, une fiche MusicBrainz qui ne
/// nomme pas l'artiste local ne prête rien.
#[tokio::test]
async fn le_rattrapage_pose_le_nom_de_tri_musicbrainz_des_artistes_deja_identifies_6773() {
    let b = base();
    let ceccarelli = artiste(&b, "André Ceccarelli", Some(MB_CECCARELLI), None);
    let kidjo = artiste(&b, "Angélique Kidjo", Some(MB_KIDJO), Some(""));
    let andre = artiste(&b, "Maurice André", Some(MB_ANDRE), None);
    let saisi = artiste(&b, "Michel Petrucciani", Some(MB_SAISI), Some("Petru"));
    let mal_rattache = artiste(&b, "Jean Durand", Some(MB_AUTRE), None);

    let requetes = std::sync::Mutex::new(Vec::new());
    let bilan = rattraper_par_musicbrainz(&b, 0, 100, |mbid: String| {
        requetes.lock().unwrap().push(mbid.clone());
        let f = fiche_mb(&mbid);
        async move { Ok(f) }
    })
    .await
    .unwrap();

    let attendu = |id, tri: &str| {
        assert_eq!(
            tri_et_source(&b, id),
            (
                Some(tri.to_string()),
                Some(SOURCE_TRI_MUSICBRAINZ.to_string())
            ),
            "artiste {id} : le nom de tri MusicBrainz doit être rattrapé"
        );
    };
    attendu(ceccarelli, "Ceccarelli, André");
    attendu(kidjo, "Kidjo, Angélique");
    attendu(andre, "André, Maurice");
    assert_eq!(
        tri_et_source(&b, saisi),
        (Some("Petru".to_string()), None),
        "une saisie n'est jamais écrasée"
    );
    assert!(
        !requetes.lock().unwrap().iter().any(|m| m == MB_SAISI),
        "un artiste qui a déjà un nom de tri n'est même pas interrogé"
    );
    assert_eq!(
        tri_et_source(&b, mal_rattache).0,
        None,
        "une fiche MusicBrainz qui ne nomme pas l'artiste ne prête pas son nom de tri"
    );
    assert_eq!(bilan.poses, 3);
    assert!(bilan.fin_de_liste);

    // Idempotent : un second tour n'a plus rien à rattraper que la fiche
    // écartée.
    let second = rattraper_par_musicbrainz(&b, 0, 100, |mbid: String| {
        let f = fiche_mb(&mbid);
        async move { Ok(f) }
    })
    .await
    .unwrap();
    assert_eq!((second.traitees, second.poses), (1, 0));
}

/// Une panne MusicBrainz n'écrit rien et arrête le tour SUR la fiche en
/// panne : le curseur ne la saute pas.
#[tokio::test]
async fn une_panne_arrete_le_tour_sans_sauter_la_fiche_6773() {
    let b = base();
    let premier = artiste(&b, "André Ceccarelli", Some(MB_CECCARELLI), None);
    let _second = artiste(&b, "Angélique Kidjo", Some(MB_KIDJO), None);
    let bilan = rattraper_par_musicbrainz(&b, 0, 100, |mbid: String| async move {
        if mbid == MB_KIDJO {
            Err(RefusMusicBrainz::Statut(503))
        } else {
            Ok(fiche_mb(&mbid))
        }
    })
    .await
    .unwrap();
    assert_eq!(
        bilan.dernier_id, premier,
        "le curseur s'arrête avant la panne"
    );
    assert_eq!(bilan.pannes, 1);
    assert!(!bilan.fin_de_liste);
}

/// #6773, point 2 (go de Bertrand du 22/10) — la déduction « Nom, Prénom »
/// pour une personne, jamais pour un groupe.
#[test]
fn la_deduction_range_une_personne_a_son_nom_de_famille_et_jamais_un_groupe_6773() {
    for (nom, tri) in [
        ("André Ceccarelli", "Ceccarelli, André"),
        ("Maurice André", "André, Maurice"),
        ("Angélique Kidjo", "Kidjo, Angélique"),
        ("Jean-Jacques Goldman", "Goldman, Jean-Jacques"),
        ("Ludwig van Beethoven", "Beethoven, Ludwig van"),
        ("Jean Michel Jarre", "Jarre, Jean Michel"),
    ] {
        assert_eq!(nom_de_tri_deduit(nom).as_deref(), Some(tri), "{nom}");
    }
    for groupe in [
        "Pink Floyd",
        "Daft Punk",
        "Iron Maiden",
        "The Beatles",
        "Simon & Garfunkel",
        "Earth, Wind & Fire",
        "Orchestre National de France",
        "Quatuor Ébène",
        "Michel Portal Trio",
        "Jacques Loussier Trio",
        "Miles Davis Quintet",
        "Bob Marley and the Wailers",
        "Various Artists",
        "Sting",
        "U2",
        "AC/DC",
        "Gabriel García Márquez Ensemble",
        "J.J. Cale",
    ] {
        assert_eq!(
            nom_de_tri_deduit(groupe),
            None,
            "{groupe} ne doit rien déduire"
        );
    }
}

/// La déduction ne touche qu'un artiste SANS MBID et SANS nom de tri ; une
/// balise, MusicBrainz ou une saisie la remplacent ensuite, et une saisie
/// n'est jamais remplacée par elle.
#[tokio::test]
async fn la_deduction_cede_a_la_balise_a_musicbrainz_et_a_la_saisie_6773() {
    let b = base();
    let sans_rien = artiste(&b, "Angélique Kidjo", None, None);
    let saisi = artiste(&b, "André Ceccarelli", None, Some("Dédé"));
    let identifie = artiste(&b, "Maurice André", Some(MB_ANDRE), None);
    let groupe = artiste(&b, "Pink Floyd", None, None);

    assert_eq!(deduire_les_noms_de_tri(&b).unwrap(), 1);
    assert_eq!(
        tri_et_source(&b, sans_rien),
        (
            Some("Kidjo, Angélique".to_string()),
            Some(SOURCE_TRI_DEDUIT.to_string())
        )
    );
    assert_eq!(tri_et_source(&b, saisi).0.as_deref(), Some("Dédé"));
    assert_eq!(
        tri_et_source(&b, identifie).0,
        None,
        "un artiste identifié attend le nom de tri MusicBrainz"
    );
    assert_eq!(tri_et_source(&b, groupe).0, None);
    assert_eq!(deduire_les_noms_de_tri(&b).unwrap(), 0, "idempotente");

    // MusicBrainz remplace une déduction.
    let repo = ArtistRepo::with_backend(b.clone());
    assert!(
        repo.poser_nom_de_tri_musicbrainz(sans_rien, "Kidjo, Angélique (MB)")
            .unwrap()
    );
    assert_eq!(
        tri_et_source(&b, sans_rien).1.as_deref(),
        Some(SOURCE_TRI_MUSICBRAINZ)
    );

    // Une balise remplace une déduction (scan d'un fichier qui porte
    // ARTISTSORT).
    let autre = artiste(&b, "Salif Keita", None, None);
    deduire_les_noms_de_tri(&b).unwrap();
    assert_eq!(tri_et_source(&b, autre).0.as_deref(), Some("Keita, Salif"));
    repo.get_or_create("Salif Keita", None, Some("Keïta, Salif"))
        .unwrap();
    assert_eq!(
        tri_et_source(&b, autre),
        (Some("Keïta, Salif".to_string()), None),
        "la balise prend la place de la déduction"
    );
}
