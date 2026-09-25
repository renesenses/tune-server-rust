//! Témoins de la passe des coffrets automatiques.
//!
//! Le scénario est écrit UNE fois, sur `Arc<dyn DbBackend>` : SQLite le joue
//! ici, PostgreSQL le rejoue dans `postgres_e2e.rs` (`pg_coffrets_auto_…`).
//! Deux copies divergeraient à la première correction.
use std::sync::Arc;

use super::*;
use crate::db::artist_repo::ArtistRepo;
use crate::db::models::{Album, Artist, Track};
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;

fn sqlite() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn artiste(db: &Arc<dyn DbBackend>, nom: &str) -> i64 {
    ArtistRepo::with_backend(db.clone())
        .create(&Artist::new(nom.into()))
        .unwrap()
}

fn album(db: &Arc<dyn DbBackend>, titre: &str, artiste: i64, dossier: &str) -> i64 {
    let mut a = Album::new(titre.into());
    a.artist_id = Some(artiste);
    let repo = AlbumRepo::with_backend(db.clone());
    let id = repo.create(&a).unwrap();
    repo.set_folder_path(id, dossier).unwrap();
    id
}

fn piste(db: &Arc<dyn DbBackend>, album: i64, artiste: i64, n: i32, disque: i32, chemin: &str) {
    let mut t = Track::new(format!("piste {n}"));
    t.album_id = Some(album);
    t.artist_id = Some(artiste);
    t.track_number = n;
    t.disc_number = disque;
    t.file_path = Some(chemin.to_string());
    TrackRepo::with_backend(db.clone()).create(&t).unwrap();
}

/// Un disque : un album et `n` pistes dans SON dossier, toutes au disque
/// `disque_tague` — souvent 1 pour chaque disque d'un coffret éclaté.
fn disque(
    db: &Arc<dyn DbBackend>,
    titre: &str,
    artiste: i64,
    dossier: &str,
    n: i32,
    disque_tague: i32,
) -> i64 {
    let id = album(db, titre, artiste, dossier);
    for k in 1..=n {
        piste(
            db,
            id,
            artiste,
            k,
            disque_tague,
            &format!("{dossier}/{k:02}.flac"),
        );
    }
    id
}

fn compte(db: &Arc<dyn DbBackend>, sql: &str, id: i64) -> i64 {
    let (p1, _) = placeholders(db);
    db.query_one(&sql.replace("{p1}", &p1), &[&id as &dyn ToSqlValue])
        .unwrap()
        .and_then(|r| r.first()?.as_i64())
        .unwrap_or(-1)
}

fn existe(db: &Arc<dyn DbBackend>, id: i64) -> bool {
    AlbumRepo::with_backend(db.clone())
        .get(id)
        .unwrap()
        .is_some()
}

fn titre(db: &Arc<dyn DbBackend>, id: i64) -> String {
    AlbumRepo::with_backend(db.clone())
        .get(id)
        .unwrap()
        .unwrap()
        .title
}

fn pistes_du_disque(db: &Arc<dyn DbBackend>, album: i64, n: i64) -> i64 {
    let (p1, p2) = placeholders(db);
    db.query_one(
        &format!("SELECT COUNT(*) FROM tracks WHERE album_id = {p1} AND disc_number = {p2}"),
        &[&album as &dyn ToSqlValue, &n],
    )
    .unwrap()
    .and_then(|r| r.first()?.as_i64())
    .unwrap_or(-1)
}

/// Les identifiants du banc — ce que chaque témoin regarde.
pub(crate) struct Banc {
    pub ew1: i64,
    pub ew2: i64,
    pub ls1: i64,
    pub ls2: i64,
    pub tonic: i64,
    pub gh1: i64,
    pub gh2: i64,
    pub manuel: i64,
    pub box3: i64,
    pub ancien: i64,
    pub boite3: i64,
    pub duo1: i64,
    pub duo2: i64,
}

/// La bibliothèque du banc, à la forme des cas mesurés sur le .18.
pub(crate) fn poser_le_banc(db: &Arc<dyn DbBackend>) -> Banc {
    // Nettoyage des magasins que `postgres_e2e::reset_schema` ne vide pas.
    let _ = db.execute("DELETE FROM album_metadata", &[]);
    let _ = db.execute("DELETE FROM album_distinct_pairs", &[]);
    let _ = SettingsRepo::with_backend(db.clone()).delete(CLE_REFUS);

    let garnier = artiste(db, "Laurent Garnier");
    let va = artiste(db, "Various Artists");
    let coltrane = artiste(db, "John Coltrane");
    let a = artiste(db, "Artiste A");
    let b = artiste(db, "Artiste B");
    let mcbride = artiste(db, "Christian McBride");

    // Early Works : dossiers aux années DIFFÉRENTES, disque 2 tagué « 1 ».
    let lg = "/m/ELECTRO/Laurent Garnier";
    let ew1 = disque(
        db,
        "Early Works, Disc 1",
        garnier,
        &format!("{lg}/1999-Early Works, Disc 1"),
        2,
        1,
    );
    let ew2 = disque(
        db,
        "Early Works, Disc 2",
        garnier,
        &format!("{lg}/2001-Early Works, Disc 2"),
        3,
        1,
    );
    // A Love Supreme : disque 1 classé « Various Artists ».
    let jc = "/m/JAZZ/John Coltrane";
    let ls1 = disque(
        db,
        "A Love Supreme, Disc 1",
        va,
        &format!("{jc}/1965-A Love Supreme, Disc 1"),
        2,
        1,
    );
    let ls2 = disque(
        db,
        "A Love Supreme, Disc 2",
        coltrane,
        &format!("{jc}/1965-A Love Supreme, Disc 2"),
        2,
        2,
    );
    // Un disque 2 SEUL.
    let tonic = disque(
        db,
        "Live at Tonic, Disc 2",
        mcbride,
        "/m/JAZZ/McBride/2006-Live at Tonic, Disc 2",
        2,
        2,
    );
    // Deux artistes réels sous un même parent : pas un coffret.
    let gh1 = disque(
        db,
        "Greatest Hits, Disc 1",
        a,
        "/m/Compilations/Greatest Hits, Disc 1",
        1,
        1,
    );
    let gh2 = disque(
        db,
        "Greatest Hits, Disc 2",
        b,
        "/m/Compilations/Greatest Hits, Disc 2",
        1,
        1,
    );
    // Un coffret composé À LA MAIN (marqueur `manuel`) et un disque 3 frère.
    let x = "/m/X";
    let manuel = album(db, "Box", a, &format!("{x}/Box, Disc 1"));
    piste(db, manuel, a, 1, 1, &format!("{x}/Box, Disc 1/01.flac"));
    piste(db, manuel, a, 1, 2, &format!("{x}/Box, Disc 2/01.flac"));
    AlbumMetadataRepo::with_backend(db.clone())
        .set(
            manuel,
            CLE_COFFRET,
            &serde_json::to_string(&Marqueur::manuel()).unwrap(),
        )
        .unwrap();
    let box3 = disque(db, "Box, Disc 3", a, &format!("{x}/Box, Disc 3"), 1, 3);
    // Un coffret ANCIEN, réparti sur deux dossiers SANS marqueur (composé
    // avant le marqueur), et un disque 3 frère.
    let y = "/m/Y";
    let ancien = album(db, "Boite", b, &format!("{y}/Boite, Disc 1"));
    piste(db, ancien, b, 1, 1, &format!("{y}/Boite, Disc 1/01.flac"));
    piste(db, ancien, b, 1, 2, &format!("{y}/Boite, Disc 2/01.flac"));
    let boite3 = disque(db, "Boite, Disc 3", b, &format!("{y}/Boite, Disc 3"), 1, 3);
    // Deux disques que l'utilisateur a déclarés DISTINCTS (#1276).
    let duo1 = disque(db, "Duo, Disc 1", a, "/m/Z/Duo, Disc 1", 1, 1);
    let duo2 = disque(db, "Duo, Disc 2", a, "/m/Z/Duo, Disc 2", 1, 1);
    AlbumDistinctRepo::with_backend(db.clone())
        .declarer_distincts(duo1, duo2)
        .unwrap();
    Banc {
        ew1,
        ew2,
        ls1,
        ls2,
        tonic,
        gh1,
        gh2,
        manuel,
        box3,
        ancien,
        boite3,
        duo1,
        duo2,
    }
}

/// LE scénario, sur n'importe quel moteur.
pub(crate) fn scenario_complet(db: &Arc<dyn DbBackend>) {
    let b = poser_le_banc(db);

    // 🔴 CONTRE-ÉPREUVE D'ABORD : avant la passe, rien n'est réuni.
    assert!(existe(db, b.ew2) && existe(db, b.ls2));
    assert_eq!(pistes_du_disque(db, b.ew1, 2), 0);

    let r = passe(db).unwrap();
    assert_eq!(r.reunis, 2, "{r:?}");
    assert_eq!(r.disques_absorbes, 2, "{r:?}");
    assert_eq!(
        r.laisses_manuels, 2,
        "le manuel ET l'ancien sans marqueur : {r:?}"
    );
    assert_eq!(r.laisses_distincts, 1, "{r:?}");
    assert_eq!(r.echecs, 0, "{r:?}");

    // Early Works : un album, deux disques, numérotés d'après le MARQUEUR —
    // le disque 2 était tagué « 1 ».
    assert!(!existe(db, b.ew2));
    assert_eq!(titre(db, b.ew1), "Early Works");
    assert_eq!(pistes_du_disque(db, b.ew1, 1), 2);
    assert_eq!(pistes_du_disque(db, b.ew1, 2), 3);
    // A Love Supreme : le disque « Various Artists » ne départage pas.
    assert!(!existe(db, b.ls2));
    assert_eq!(titre(db, b.ls1), "A Love Supreme");
    // Intacts : disque isolé, artistes différents, manuel, ancien, distincts.
    for id in [b.tonic, b.gh1, b.gh2, b.box3, b.boite3, b.duo1, b.duo2] {
        assert!(existe(db, id), "album {id} absorbé à tort");
    }
    assert_eq!(titre(db, b.tonic), "Live at Tonic, Disc 2");
    assert_eq!(
        compte(
            db,
            "SELECT COUNT(*) FROM tracks WHERE album_id = {p1}",
            b.manuel
        ),
        2
    );
    assert_eq!(
        compte(
            db,
            "SELECT COUNT(*) FROM tracks WHERE album_id = {p1}",
            b.ancien
        ),
        2
    );
    let m = marqueurs(db).unwrap();
    assert_eq!(
        m.get(&b.ew1).map(|m| m.origine.as_str()),
        Some(ORIGINE_AUTO)
    );
    assert_eq!(
        m.get(&b.manuel).map(|m| m.origine.as_str()),
        Some(ORIGINE_MANUEL)
    );

    // IDEMPOTENTE : la seconde passe ne trouve rien à réunir.
    let r2 = passe(db).unwrap();
    assert_eq!(r2.reunis, 0, "{r2:?}");

    // Un disque 3 arrivé PLUS TARD rejoint le coffret automatique.
    let lg = "/m/ELECTRO/Laurent Garnier";
    let garnier = AlbumRepo::with_backend(db.clone())
        .get(b.ew1)
        .unwrap()
        .and_then(|a| a.artist_id)
        .unwrap();
    let ew3 = disque(
        db,
        "Early Works, Disc 3",
        garnier,
        &format!("{lg}/2003-Early Works, Disc 3"),
        1,
        1,
    );
    let r3 = passe(db).unwrap();
    assert_eq!(r3.reunis, 1, "{r3:?}");
    assert!(!existe(db, ew3));
    assert_eq!(pistes_du_disque(db, b.ew1, 3), 1);
    let m = marqueurs(db).unwrap();
    assert_eq!(m[&b.ew1].disques.len(), 3, "{:?}", m[&b.ew1]);

    // L'ONGLET : les coffrets auto, le manuel, l'ancien — pas les autres.
    let l: Vec<i64> = lister(db).unwrap().iter().map(|c| c.album_id).collect();
    for id in [b.ew1, b.ls1, b.manuel, b.ancien] {
        assert!(l.contains(&id), "coffret {id} absent de la liste {l:?}");
    }
    for id in [b.tonic, b.gh1, b.box3, b.duo1] {
        assert!(!l.contains(&id), "album {id} listé comme coffret {l:?}");
    }

    // DÉFAIRE : trois albums de nouveau, sous leurs titres d'origine.
    assert_eq!(defaire(db, b.manuel), Err(RefusDefaire::PasUnCoffretAuto));
    let recrees = defaire(db, b.ew1).unwrap();
    assert_eq!(recrees.len(), 2);
    assert_eq!(titre(db, b.ew1), "Early Works, Disc 1");
    assert_eq!(
        compte(
            db,
            "SELECT COUNT(*) FROM tracks WHERE album_id = {p1}",
            b.ew1
        ),
        2
    );
    let titres: Vec<String> = recrees.iter().map(|id| titre(db, *id)).collect();
    assert_eq!(titres, vec!["Early Works, Disc 2", "Early Works, Disc 3"]);
    assert_eq!(
        compte(
            db,
            "SELECT COUNT(*) FROM tracks WHERE album_id = {p1}",
            recrees[0]
        ),
        3
    );

    // …et le coffret défait NE REVIENT PAS.
    let r4 = passe(db).unwrap();
    assert_eq!(r4.reunis, 0, "{r4:?}");
    assert_eq!(r4.laisses_refuses, 1, "{r4:?}");
    assert!(existe(db, recrees[0]) && existe(db, recrees[1]));

    // 🔴 CONTRE-ÉPREUVE du refus : oublié, la passe le reforme. Sans elle, le
    // témoin précédent serait vert contre une passe qui ne reforme plus rien.
    let cle = refus(db).into_iter().next().expect("un refus retenu");
    oublier_refus(db, &cle).unwrap();
    let r5 = passe(db).unwrap();
    assert_eq!(r5.reunis, 1, "{r5:?}");
    assert!(!existe(db, recrees[0]));
}

#[test]
fn scenario_complet_sur_sqlite() {
    scenario_complet(&sqlite());
}

/// 🔴 CONTRE-ÉPREUVE de la garde « manuel » : le MÊME banc, marqueur manuel
/// retiré et pistes ramenées dans un seul dossier — le disque 3 est alors
/// absorbé. Sans ce témoin, `laisses_manuels == 2` pourrait tenir à une autre
/// cause (un coffret que la détection ne verrait pas).
#[test]
fn sans_la_garde_le_disque_3_serait_absorbe() {
    let db = sqlite();
    let b = poser_le_banc(&db);
    AlbumMetadataRepo::with_backend(db.clone())
        .delete(b.manuel, CLE_COFFRET)
        .unwrap();
    db.execute(
        "UPDATE tracks SET file_path = '/m/X/Box, Disc 1/02.flac' WHERE album_id = ? AND disc_number = 2",
        &[&b.manuel as &dyn ToSqlValue],
    )
    .unwrap();
    passe(&db).unwrap();
    assert!(!existe(&db, b.box3), "la garde n'était pas la seule raison");
}
