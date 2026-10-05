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

// ---------------------------------------------------------------------------
// « Bibliothèque LOCALE » — Bertrand, 27/09/2026
// ---------------------------------------------------------------------------

/// Un disque dont les pistes ET l'album portent une source donnée, avec de
/// vrais chemins de fichier.
///
/// 🔴 C'est le cas que la base de Bertrand ne contient PAS aujourd'hui — sur
/// le .18, aucune des 49 440 pistes `source = 'upnp'` ne porte de chemin. Il
/// faut donc le fabriquer ici, sinon on ne prouve rien : le filtre par chemin
/// de fichier et le filtre par source rendraient le même résultat.
fn disque_de_source(
    db: &Arc<dyn DbBackend>,
    titre: &str,
    artiste: i64,
    dossier: &str,
    n: i32,
    disque_tague: i32,
    source: &str,
) -> i64 {
    let id = disque(db, titre, artiste, dossier, n, disque_tague);
    let (p1, p2) = placeholders(db);
    db.execute(
        &format!("UPDATE tracks SET source = {p1} WHERE album_id = {p2}"),
        &[&source.to_string() as &dyn ToSqlValue, &id],
    )
    .unwrap();
    db.execute(
        &format!("UPDATE albums SET source = {p1} WHERE id = {p2}"),
        &[&source.to_string() as &dyn ToSqlValue, &id],
    )
    .unwrap();
    id
}

/// Deux coffrets éclatés de la même forme, l'un LOCAL, l'autre `upnp`, tous
/// deux avec des chemins de fichier.
struct BancDeuxSources {
    local_1: i64,
    local_2: i64,
    distant_1: i64,
    distant_2: i64,
}

fn poser_deux_sources(db: &Arc<dyn DbBackend>) -> BancDeuxSources {
    let ar = artiste(db, "Depeche Mode");
    BancDeuxSources {
        local_1: disque_de_source(db, "101, Disc 1", ar, "/m/101/Disc 1", 3, 1, "local"),
        local_2: disque_de_source(db, "101, Disc 2", ar, "/m/101/Disc 2", 3, 1, "local"),
        distant_1: disque_de_source(db, "Violator, Disc 1", ar, "/u/V/Disc 1", 3, 1, "upnp"),
        distant_2: disque_de_source(db, "Violator, Disc 2", ar, "/u/V/Disc 2", 3, 1, "upnp"),
    }
}

/// 🔴 L'inventaire — celui que lit l'écran des coffrets éclatés, le geste de
/// regroupement ET la passe automatique — ne voit que la bibliothèque LOCALE.
#[test]
fn l_inventaire_ecarte_un_album_non_local_qui_porte_des_chemins() {
    let db = sqlite();
    let b = poser_deux_sources(&db);
    let inv = inventaire(&db).unwrap();
    let vus: Vec<i64> = inv.albums.iter().map(|a| a.id).collect();
    for id in [b.distant_1, b.distant_2] {
        assert!(
            !vus.contains(&id),
            "l'album {id} est `source = upnp` : il ne doit PAS entrer dans \
             l'inventaire, même avec des chemins de fichier — vus {vus:?}"
        );
    }
}

/// L'AUTRE sens, sans quoi un filtre qui rejette tout serait vert.
#[test]
fn l_inventaire_garde_les_albums_locaux() {
    let db = sqlite();
    let b = poser_deux_sources(&db);
    let inv = inventaire(&db).unwrap();
    let vus: Vec<i64> = inv.albums.iter().map(|a| a.id).collect();
    for id in [b.local_1, b.local_2] {
        assert!(
            vus.contains(&id),
            "l'album LOCAL {id} doit entrer dans l'inventaire — vus {vus:?}"
        );
    }
    // Et le coffret local est bien DÉTECTÉ : la sélection n'a pas seulement
    // laissé passer les lignes, elle laisse la passe faire son travail.
    let trouves = coffrets(&inv.albums);
    assert_eq!(
        trouves.len(),
        1,
        "un seul coffret éclaté, le local — trouvés {:?}",
        trouves.iter().map(|c| &c.titre).collect::<Vec<_>>()
    );
}

/// 🔴 La liste de l'onglet « Coffrets » (`GET /library/coffrets`) : même règle.
#[test]
fn lister_ecarte_un_coffret_non_local_qui_porte_des_chemins() {
    let db = sqlite();
    let ar = artiste(&db, "Depeche Mode");
    // Un coffret RÉUNI : un seul album, deux numéros de disque, deux dossiers.
    let local = disque_de_source(&db, "Box locale", ar, "/m/Box/CD1", 2, 1, "local");
    let distant = disque_de_source(&db, "Box distante", ar, "/u/Box/CD1", 2, 1, "upnp");
    for (id, dossier) in [(local, "/m/Box/CD2"), (distant, "/u/Box/CD2")] {
        let (p1, p2) = placeholders(&db);
        db.execute(
            &format!(
                "UPDATE tracks SET disc_number = 2, file_path = {p1} WHERE album_id = {p2} AND track_number = 2"
            ),
            &[&format!("{dossier}/02.flac") as &dyn ToSqlValue, &id],
        )
        .unwrap();
    }
    let vus: Vec<i64> = lister(&db)
        .unwrap()
        .into_iter()
        .map(|c| c.album_id)
        .collect();
    assert!(
        !vus.contains(&distant),
        "le coffret {distant} est `source = upnp` : il ne doit PAS être listé — vus {vus:?}"
    );
    assert!(
        vus.contains(&local),
        "le coffret LOCAL {local} doit être listé — vus {vus:?}"
    );
}

// ---------------------------------------------------------------------------
// #5317 — les albums nés d'une feuille CUE ; #5357 — le marqueur EN TÊTE.
// GO de Bertrand du 28/09/2026 (Marco Polo, fil 2009).
// ---------------------------------------------------------------------------

/// Vide ce que `postgres_e2e::reset_schema` ne vide pas.
fn nettoyer_les_magasins(db: &Arc<dyn DbBackend>) {
    let _ = db.execute("DELETE FROM album_metadata", &[]);
    let _ = db.execute("DELETE FROM album_distinct_pairs", &[]);
    let _ = SettingsRepo::with_backend(db.clone()).delete(CLE_REFUS);
}

/// Un vrai WAV court : le plan CUE écarte une image qu'aucun décodeur ne lit.
fn ecrire_wav(chemin: &std::path::Path, millisecondes: u32) {
    const TAUX: u32 = 8_000;
    let trames = TAUX * millisecondes / 1000;
    let octets = trames * 4;
    let mut f = Vec::new();
    f.extend_from_slice(b"RIFF");
    f.extend_from_slice(&(36 + octets).to_le_bytes());
    f.extend_from_slice(b"WAVEfmt ");
    f.extend_from_slice(&16u32.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&2u16.to_le_bytes());
    f.extend_from_slice(&TAUX.to_le_bytes());
    f.extend_from_slice(&(TAUX * 4).to_le_bytes());
    f.extend_from_slice(&4u16.to_le_bytes());
    f.extend_from_slice(&16u16.to_le_bytes());
    f.extend_from_slice(b"data");
    f.extend_from_slice(&octets.to_le_bytes());
    for n in 0..trames {
        let v = ((n as f32 / 40.0).sin() * 8000.0) as i16;
        f.extend_from_slice(&v.to_le_bytes());
        f.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(chemin, f).unwrap();
}

/// Un disque en image + feuille, rangé comme chez le testeur :
/// `<parent>/CDn/CDImagen.wav` + `CDImagen.cue`, deux pistes.
fn disque_cue(
    parent: &std::path::Path,
    n: u32,
    interprete: &str,
    titre: &str,
) -> std::path::PathBuf {
    let d = parent.join(format!("CD{n}"));
    std::fs::create_dir_all(&d).unwrap();
    ecrire_wav(&d.join(format!("CDImage{n}.wav")), 4_000);
    std::fs::write(
        d.join(format!("CDImage{n}.cue")),
        format!(
            "PERFORMER \"{interprete}\"\nTITLE \"{titre}\"\nFILE \"CDImage{n}.wav\" WAVE\n  \
             TRACK 01 AUDIO\n    TITLE \"Piste 1\"\n    INDEX 01 00:00:00\n  \
             TRACK 02 AUDIO\n    TITLE \"Piste 2\"\n    INDEX 01 00:02:00\n"
        ),
    )
    .unwrap();
    d
}

/// Les albums qui portent au moins une piste, et le nombre de leurs pistes.
fn albums_avec_pistes(db: &Arc<dyn DbBackend>) -> Vec<(i64, i64)> {
    let mut v: Vec<(i64, i64)> = db
        .query_many(
            "SELECT album_id, COUNT(*) FROM tracks WHERE album_id IS NOT NULL GROUP BY album_id",
            &[],
        )
        .unwrap()
        .into_iter()
        .filter_map(|r| Some((r.first()?.as_i64()?, r.get(1)?.as_i64()?)))
        .collect();
    v.sort();
    v
}

/// 🔴 #5317 — LE *MESSIAH* DE GARDINER, sur une VRAIE arborescence :
/// `Handel - Messiah, Gardiner (Philips 2CD)/CD1/CDImage1.{wav,cue}` et
/// `…/CD2/CDImage2.{wav,cue}`, titres de feuille « Messiah - Gardiner - CD1 »
/// et « … - CD2 », passés par le VRAI écrivain CUE du scan.
///
/// À côté, le cas à NE PAS réunir : deux disques CUE frères, même socle,
/// mais deux interprètes réels différents.
pub(crate) fn scenario_cue_deux_disques(db: &Arc<dyn DbBackend>) {
    use crate::scanner::cue_bibliotheque::inventorier_et_ecrire;
    nettoyer_les_magasins(db);
    let d = tempfile::TempDir::new().unwrap();
    let messiah = d.path().join("Handel - Messiah, Gardiner (Philips 2CD)");
    let cd1 = disque_cue(&messiah, 1, "G. F. Handel", "Messiah - Gardiner - CD1");
    let cd2 = disque_cue(&messiah, 2, "G. F. Handel", "Messiah - Gardiner - CD2");
    let autre = d.path().join("Cantates (2CD)");
    let ca1 = disque_cue(&autre, 1, "J. S. Bach", "Cantates - CD1");
    let ca2 = disque_cue(&autre, 2, "Gardiner", "Cantates - CD2");
    let racines = vec![d.path().to_string_lossy().into_owned()];
    let scanner = || {
        inventorier_et_ecrire(
            db.clone(),
            &[cd1.clone(), cd2.clone(), ca1.clone(), ca2.clone()],
            &racines,
        )
    };

    let (_, bilan, _) = scanner();
    assert_eq!(bilan.pistes_creees, 8, "{bilan:?}");
    // Le fait qui cachait ces albums à la passe : AUCUNE piste n'a de
    // `file_path`, toutes vivent dans `cue_media_path`.
    assert_eq!(
        compte(
            db,
            "SELECT COUNT(*) FROM tracks WHERE file_path IS NULL AND cue_media_path IS NOT NULL AND id > {p1}",
            0
        ),
        8
    );
    assert_eq!(
        albums_avec_pistes(db).len(),
        4,
        "quatre disques, quatre albums"
    );

    let r = passe(db).unwrap();
    assert_eq!(r.reunis, 1, "le Messiah, et lui seul : {r:?}");
    assert_eq!(r.disques_absorbes, 1, "{r:?}");
    assert_eq!(r.echecs, 0, "{r:?}");
    let apres = albums_avec_pistes(db);
    assert_eq!(
        apres.len(),
        3,
        "Messiah réuni, Cantates intactes : {apres:?}"
    );
    let coffret = apres
        .iter()
        .map(|(id, _)| *id)
        .find(|id| titre(db, *id) == "Messiah - Gardiner")
        .expect("un album « Messiah - Gardiner »");
    assert_eq!(pistes_du_disque(db, coffret, 1), 2);
    assert_eq!(pistes_du_disque(db, coffret, 2), 2);
    for t in ["Cantates - CD1", "Cantates - CD2"] {
        assert!(
            apres.iter().any(|(id, _)| titre(db, *id) == t),
            "« {t} » réuni à tort : deux interprètes réels différents"
        );
    }
    assert!(
        lister(db).unwrap().iter().any(|c| c.album_id == coffret),
        "le coffret CUE doit figurer dans l'onglet"
    );

    // RESCAN : l'écrivain CUE réécrit chaque feuille et rend le disque 2 à
    // un album de son dossier ; la passe qui suit le scan le réunit de nouveau.
    scanner();
    passe(db).unwrap();
    let apres_rescan = albums_avec_pistes(db);
    assert_eq!(apres_rescan.len(), 3, "{apres_rescan:?}");
    assert!(apres_rescan.contains(&(coffret, 4)), "{apres_rescan:?}");
    assert_eq!(titre(db, coffret), "Messiah - Gardiner");
    assert_eq!(pistes_du_disque(db, coffret, 2), 2);

    // DÉFAIRE : le disque 2 retrouve ses pistes — qui n'ont pas de
    // `file_path` — et le coffret ne revient pas.
    let recrees = defaire(db, coffret).unwrap();
    assert_eq!(recrees.len(), 1, "{recrees:?}");
    assert_eq!(
        compte(
            db,
            "SELECT COUNT(*) FROM tracks WHERE album_id = {p1}",
            coffret
        ),
        2
    );
    assert_eq!(
        compte(
            db,
            "SELECT COUNT(*) FROM tracks WHERE album_id = {p1}",
            recrees[0]
        ),
        2
    );
    assert_eq!(titre(db, coffret), "Messiah - Gardiner - CD1");
    assert_eq!(titre(db, recrees[0]), "Messiah - Gardiner - CD2");
    let r2 = passe(db).unwrap();
    assert_eq!(r2.reunis, 0, "{r2:?}");
    assert_eq!(r2.laisses_refuses, 1, "{r2:?}");
}

#[test]
fn cue_deux_disques_sur_sqlite() {
    scenario_cue_deux_disques(&sqlite());
}

/// 🔴 #5357 — le marqueur EN TÊTE, en pistes séparées. `CD1 - Messiah` /
/// `CD2 - Messiah` sont réunis ; la *Philips Original Jackets Collection*
/// de la capture — un titre DIFFÉRENT par disque, sous un même parent et un
/// même artiste d'album — ne l'est pas.
pub(crate) fn scenario_marqueur_de_tete(db: &Arc<dyn DbBackend>) {
    nettoyer_les_magasins(db);
    let handel = artiste(db, "G. F. Handel");
    let philips = artiste(db, "Philips");
    let m = "/m/Classique/Handel - Messiah (Gardiner)";
    let cd1 = disque(
        db,
        "CD1 - Messiah",
        handel,
        &format!("{m}/CD1 - Messiah"),
        2,
        1,
    );
    let cd2 = disque(
        db,
        "CD2 - Messiah",
        handel,
        &format!("{m}/CD2 - Messiah"),
        3,
        1,
    );
    let p = "/m/Coffrets/Philips Original Jackets Collection (55 CDs)";
    let collection: Vec<i64> = [
        "CD01 - Bruch Violin Concertos Nos. 1 & 2; Scottish Fantasy",
        "CD02 - Brahms Wolf Lieder",
        "CD03 - Brahms - Piano Concerto No.2",
        "CD09 - Rossini - Stabat Mater",
    ]
    .iter()
    .map(|n| disque(db, n, philips, &format!("{p}/{n}"), 1, 1))
    .collect();

    let r = passe(db).unwrap();
    assert_eq!(r.reunis, 1, "{r:?}");
    assert!(!existe(db, cd2));
    assert_eq!(titre(db, cd1), "Messiah");
    assert_eq!(pistes_du_disque(db, cd1, 1), 2);
    assert_eq!(
        pistes_du_disque(db, cd1, 2),
        3,
        "numéroté d'après le marqueur"
    );
    for id in collection {
        assert!(
            existe(db, id),
            "disque {id} de la collection absorbé à tort"
        );
    }
}

#[test]
fn marqueur_de_tete_sur_sqlite() {
    scenario_marqueur_de_tete(&sqlite());
}

// ---------------------------------------------------------------------------
// #5318 — serveur Windows : `tracks.file_path` porte des antislashs.
// ---------------------------------------------------------------------------

/// Un disque aux chemins Windows : `n` pistes `{dossier}\NN.flac`.
fn disque_windows(
    db: &Arc<dyn DbBackend>,
    titre: &str,
    artiste: i64,
    dossier: &str,
    n: i32,
) -> i64 {
    let id = album(db, titre, artiste, dossier);
    for k in 1..=n {
        piste(db, id, artiste, k, 1, &format!("{dossier}\\{k:02}.flac"));
    }
    id
}

/// 🔴 #5318 — la passe, l'onglet et « défaire » sur des chemins Windows
/// réels : lettre de lecteur, partage réseau, casse du lecteur mêlée.
///
/// Avant le correctif, `dossier_de` coupait sur `/` seul : aucun de ces
/// chemins n'avait de dossier, la passe sautait TOUT, et l'onglet ne voyait
/// pas l'album rangé disque par disque.
pub(crate) fn scenario_windows(db: &Arc<dyn DbBackend>) {
    nettoyer_les_magasins(db);
    let handel = artiste(db, "G. F. Handel");
    let bach = artiste(db, "J. S. Bach");
    let floyd = artiste(db, "Pink Floyd");

    // Lettre de lecteur — le rangement du testeur (fil 2009).
    let m = r"D:\Musique\Handel - Messiah, Gardiner (Philips 2CD)";
    let cd1 = disque_windows(db, "Messiah, CD1", handel, &format!(r"{m}\CD1"), 2);
    let cd2 = disque_windows(db, "Messiah, CD2", handel, &format!(r"{m}\CD2"), 3);
    // Partage réseau UNC.
    let g = r"\\NAS\Musique\Bach - Gardiner Vol 21";
    let g1 = disque_windows(
        db,
        "Gardiner Vol 21, Disc 1",
        bach,
        &format!(r"{g}\Disc 1"),
        2,
    );
    let g2 = disque_windows(
        db,
        "Gardiner Vol 21, Disc 2",
        bach,
        &format!(r"{g}\Disc 2"),
        2,
    );

    // Un ancien coffret SANS marqueur, rangé disque par disque : l'onglet le
    // doit par son second critère (deux disques, deux dossiers).
    let a = r"E:\Musique\Pink Floyd\Pulse";
    let ancien = album(db, "Pulse", floyd, &format!(r"{a}\CD1"));
    piste(db, ancien, floyd, 1, 1, &format!(r"{a}\CD1\01.flac"));
    piste(db, ancien, floyd, 1, 2, &format!(r"{a}\CD2\01.flac"));
    // Un double album dans UN dossier, dont une piste porte le lecteur en
    // minuscule (`c:`) : ce n'est PAS un coffret, et la casse ne doit pas en
    // fabriquer deux dossiers.
    let d = r"C:\Musique\Pink Floyd\The Wall";
    let wall = album(db, "The Wall", floyd, d);
    piste(db, wall, floyd, 1, 1, &format!(r"{d}\1-01.flac"));
    piste(
        db,
        wall,
        floyd,
        1,
        2,
        r"c:\Musique\Pink Floyd\The Wall\2-01.flac",
    );

    let r = passe(db).unwrap();
    assert_eq!(r.reunis, 2, "Messiah (D:) et Gardiner (UNC) : {r:?}");
    assert!(!existe(db, cd2), "le disque 2 du Messiah n'est pas absorbé");
    assert!(!existe(db, g2), "le disque 2 du partage n'est pas absorbé");
    assert_eq!(titre(db, cd1), "Messiah");
    assert_eq!(titre(db, g1), "Gardiner Vol 21");
    assert_eq!(pistes_du_disque(db, cd1, 2), 3);
    assert!(existe(db, ancien) && existe(db, wall));

    // L'ONGLET.
    let l: Vec<i64> = lister(db).unwrap().iter().map(|c| c.album_id).collect();
    for id in [cd1, g1, ancien] {
        assert!(l.contains(&id), "coffret {id} absent de la liste {l:?}");
    }
    assert!(
        !l.contains(&wall),
        "un double album d'UN dossier (casse du lecteur mêlée) listé : {l:?}"
    );

    // DÉFAIRE : le disque 2 retrouve ses pistes `D:\…\CD2\…`.
    let recrees = defaire(db, cd1).unwrap();
    assert_eq!(recrees.len(), 1, "{recrees:?}");
    assert_eq!(titre(db, recrees[0]), "Messiah, CD2");
    assert_eq!(
        compte(
            db,
            "SELECT COUNT(*) FROM tracks WHERE album_id = {p1}",
            recrees[0]
        ),
        3,
        "les pistes du disque 2 ne sont pas rendues à leur album"
    );
    assert_eq!(
        compte(db, "SELECT COUNT(*) FROM tracks WHERE album_id = {p1}", cd1),
        2
    );
}

#[test]
fn chemins_windows_sur_sqlite() {
    scenario_windows(&sqlite());
}

/// `(numéro de disque, sous-titre)` des pistes d'un album, distincts, triés.
fn sous_titres_de(db: &Arc<dyn DbBackend>, album: i64) -> Vec<(i64, Option<String>)> {
    let (p1, _) = placeholders(db);
    let mut v: Vec<(i64, Option<String>)> = db
        .query_many(
            &format!(
                "SELECT DISTINCT COALESCE(disc_number, 1), disc_subtitle FROM tracks \
                 WHERE album_id = {p1}"
            ),
            &[&album as &dyn ToSqlValue],
        )
        .unwrap()
        .into_iter()
        .map(|r| {
            (
                r.first().and_then(|v| v.as_i64()).unwrap_or(-1),
                r.get(1).and_then(|v| v.as_string()),
            )
        })
        .collect();
    v.sort();
    v
}

/// Fil 2094 (décision de Bertrand du 05/10/2026) — réunir un coffret fait du
/// titre d'ORIGINE de chaque disque son sous-titre de disque, sauf quand il
/// est celui du coffret ; un nom déjà porté par un disque reste le sien ;
/// « Défaire » retire ce que la réunion a posé, et rien d'autre.
///
/// Avant : la fiche affichait « Disque 1 », « Disque 2 » — les titres
/// d'origine ne vivaient que dans le marqueur `coffret`.
pub(crate) fn scenario_sous_titres_2094(db: &Arc<dyn DbBackend>) {
    let _ = db.execute("DELETE FROM album_metadata", &[]);
    let _ = db.execute("DELETE FROM album_distinct_pairs", &[]);
    let _ = SettingsRepo::with_backend(db.clone()).delete(CLE_REFUS);
    let bach = artiste(db, "Jean-Sébastien Bach");
    let r = "/m/CLASSIQUE/Bach";
    let d1 = disque(
        db,
        "Cantates, Disc 1",
        bach,
        &format!("{r}/Cantates, Disc 1"),
        2,
        1,
    );
    let d2 = disque(
        db,
        "Cantates, Disc 2",
        bach,
        &format!("{r}/Cantates, Disc 2"),
        2,
        2,
    );
    // Le disque 3 porte DÉJÀ un nom (balise DISCSUBTITLE) sur une piste.
    let d3 = disque(
        db,
        "Cantates, Disc 3",
        bach,
        &format!("{r}/Cantates, Disc 3"),
        2,
        3,
    );
    let (p1, p2) = placeholders(db);
    db.execute(
        &format!(
            "UPDATE tracks SET disc_subtitle = {p1} WHERE id = \
             (SELECT MIN(id) FROM tracks WHERE album_id = {p2})"
        ),
        &[&"BWV 140" as &dyn ToSqlValue, &d3],
    )
    .unwrap();
    assert_eq!(sous_titres_de(db, d1), vec![(1, None)], "avant : aucun nom");

    let rapport = passe(db).unwrap();
    assert_eq!(rapport.reunis, 1, "{rapport:?}");
    assert_eq!(titre(db, d1), "Cantates");
    assert!(!existe(db, d2) && !existe(db, d3));
    assert_eq!(
        sous_titres_de(db, d1),
        vec![
            (1, Some("Cantates, Disc 1".into())),
            (2, Some("Cantates, Disc 2".into())),
            (3, None),
            (3, Some("BWV 140".into())),
        ],
        "chaque disque prend son titre d'origine ; le disque 3 garde le sien, entier"
    );
    let m = marqueurs(db).unwrap();
    assert_eq!(m[&d1].sous_titres, vec![1, 2]);

    // DÉFAIRE : les sous-titres posés partent, le nom du disque 3 reste.
    let recrees = defaire(db, d1).unwrap();
    assert_eq!(sous_titres_de(db, d1), vec![(1, None)]);
    let mut restes: Vec<(i64, Option<String>)> = recrees
        .iter()
        .flat_map(|id| sous_titres_de(db, *id))
        .collect();
    restes.sort();
    assert_eq!(
        restes,
        vec![(2, None), (3, None), (3, Some("BWV 140".into()))],
        "rien de ce que la réunion avait posé ne reste"
    );

    // TÉMOIN — des disques au titre IDENTIQUE à celui du coffret : aucun
    // sous-titre (27 « Arkhangelsk »). Composés à la main faute de marqueur
    // chiffré dans le titre : la fonction est appelée comme la route le fait.
    let ark = artiste(db, "Arkhangelsk");
    let a1 = disque(db, "Arkhangelsk", ark, "/m/Arkhangelsk/CD01", 1, 1);
    let a2 = disque(db, "Arkhangelsk", ark, "/m/Arkhangelsk/CD02", 1, 1);
    let (p1, p2) = placeholders(db);
    db.execute(
        &format!("UPDATE tracks SET disc_number = 2 WHERE album_id = {p1}"),
        &[&a2 as &dyn ToSqlValue],
    )
    .unwrap();
    db.execute(
        &format!("UPDATE tracks SET album_id = {p1} WHERE album_id = {p2}"),
        &[&a1 as &dyn ToSqlValue, &a2],
    )
    .unwrap();
    let retenus = |n, d: &str| DisqueRetenu {
        n,
        titre: "Arkhangelsk".into(),
        dossier: d.into(),
        artiste_id: Some(ark),
    };
    let poses = poser_les_sous_titres(
        db,
        a1,
        &[
            retenus(1, "/m/Arkhangelsk/CD01"),
            retenus(2, "/m/Arkhangelsk/CD02"),
        ],
        "ARKHANGELSK ",
    )
    .unwrap();
    assert!(
        poses.is_empty(),
        "titre identique (casse, espaces) : {poses:?}"
    );
    assert_eq!(sous_titres_de(db, a1), vec![(1, None), (2, None)]);
}

#[test]
fn sous_titres_2094_sur_sqlite() {
    scenario_sous_titres_2094(&sqlite());
}

/// Un coffret composé À LA MAIN, comme le fait `POST /library/albums/coffret`
/// AVANT le fil 2094 : disques renumérotés dans l'ordre, absorbés dans le
/// premier, titre posé, marqueur `manuel` qui retient les disques, et
/// disposition tenue — mais AUCUN sous-titre.
fn coffret_manuel_d_avant_2094(
    db: &Arc<dyn DbBackend>,
    titre_du_coffret: &str,
    ids: &[i64],
    disques: Vec<DisqueRetenu>,
) {
    let (p1, p2) = placeholders(db);
    for (rang, id) in ids.iter().enumerate() {
        db.execute(
            &format!("UPDATE tracks SET disc_number = {p1} WHERE album_id = {p2}"),
            &[&((rang + 1) as i64) as &dyn ToSqlValue, id],
        )
        .unwrap();
    }
    let repo = AlbumRepo::with_backend(db.clone());
    for &id in &ids[1..] {
        repo.absorber(ids[0], id).unwrap();
    }
    repo.force_update_title(ids[0], titre_du_coffret).unwrap();
    let m = Marqueur {
        disques,
        titre_compose: Some(titre_du_coffret.to_string()),
        ..Marqueur::manuel()
    };
    AlbumMetadataRepo::with_backend(db.clone())
        .set(ids[0], CLE_COFFRET, &serde_json::to_string(&m).unwrap())
        .unwrap();
    crate::db::edition_album::tenir_la_disposition(db, ids[0], &[]).unwrap();
}

fn retenu(n: u32, titre: &str, dossier: &str) -> DisqueRetenu {
    DisqueRetenu {
        n,
        titre: titre.into(),
        dossier: dossier.into(),
        artiste_id: None,
    }
}

/// Fil 2094 — le RATTRAPAGE des coffrets composés avant que la composition
/// pose les sous-titres : même règle (pas le titre du coffret, jamais sur un
/// nom existant), marqueur mis à jour pour que « Défaire » reste exact,
/// disposition tenue retenue à nouveau, une seule fois.
pub(crate) fn scenario_rattrapage_sous_titres_2094(db: &Arc<dyn DbBackend>) {
    let _ = db.execute("DELETE FROM album_metadata", &[]);
    let _ = db.execute("DELETE FROM album_distinct_pairs", &[]);
    let reglages = SettingsRepo::with_backend(db.clone());
    let _ = reglages.delete(CLE_REFUS);
    let _ = reglages.delete(CLE_RATTRAPAGE_SOUS_TITRES_2094);
    let meta = AlbumMetadataRepo::with_backend(db.clone());
    let (p1, p2) = placeholders(db);

    // A. Un coffret AUTOMATIQUE réuni avant le fil 2094. Le disque 3 porte
    //    déjà un nom (balise DISCSUBTITLE) sur une piste.
    let bach = artiste(db, "Jean-Sébastien Bach");
    let r = "/m/CLASSIQUE/Oratorio";
    let d1 = disque(
        db,
        "Oratorio, Disc 1",
        bach,
        &format!("{r}/Oratorio, Disc 1"),
        2,
        1,
    );
    disque(
        db,
        "Oratorio, Disc 2",
        bach,
        &format!("{r}/Oratorio, Disc 2"),
        2,
        2,
    );
    let d3 = disque(
        db,
        "Oratorio, Disc 3",
        bach,
        &format!("{r}/Oratorio, Disc 3"),
        2,
        3,
    );
    db.execute(
        &format!(
            "UPDATE tracks SET disc_subtitle = {p1} WHERE id = \
             (SELECT MIN(id) FROM tracks WHERE album_id = {p2})"
        ),
        &[&"BWV 248" as &dyn ToSqlValue, &d3],
    )
    .unwrap();
    assert_eq!(passe(db).unwrap().reunis, 1);
    // Revenir à l'état d'une base d'avant 2094 : ni sous-titre posé par la
    // réunion, ni `sous_titres` dans le marqueur.
    let mut avant = marqueurs(db).unwrap()[&d1].clone();
    retirer_les_sous_titres(db, &[d1], &avant).unwrap();
    avant.sous_titres.clear();
    meta.set(d1, CLE_COFFRET, &serde_json::to_string(&avant).unwrap())
        .unwrap();
    assert_eq!(
        sous_titres_de(db, d1),
        vec![(1, None), (2, None), (3, None), (3, Some("BWV 248".into()))],
        "base d'avant 2094"
    );

    // B. Un coffret MANUEL, disposition tenue. Son disque 3 a quitté le
    //    dossier que le marqueur retient : il est laissé.
    let x = artiste(db, "Xenakis");
    let m1 = disque(db, "101 - Disc A", x, "/m/Manuel/101 A", 2, 1);
    let m2 = disque(db, "101 - Disc B", x, "/m/Manuel/101 B", 2, 1);
    let m3 = disque(db, "101 - Disc C", x, "/m/Ailleurs", 1, 1);
    coffret_manuel_d_avant_2094(
        db,
        "101",
        &[m1, m2, m3],
        vec![
            retenu(1, "101 - Disc A", "/m/Manuel/101 A"),
            retenu(2, "101 - Disc B", "/m/Manuel/101 B"),
            retenu(3, "101 - Disc C", "/m/Manuel/101 C"),
        ],
    );

    // C. Un coffret manuel SANS disques retenus (avant #5319, ou « attacher ») :
    //    les titres d'origine sont perdus.
    let y = artiste(db, "Yes");
    let s1 = disque(db, "Relayer", y, "/m/Yes/Relayer", 1, 1);
    let s2 = disque(db, "Fragile", y, "/m/Yes/Fragile", 1, 1);
    coffret_manuel_d_avant_2094(db, "Yes Box", &[s1, s2], vec![]);

    // D. TÉMOIN — des disques au titre identique à celui du coffret.
    let ark = artiste(db, "Arkhangelsk");
    let a1 = disque(db, "Arkhangelsk", ark, "/m/Arkhangelsk/CD01", 1, 1);
    let a2 = disque(db, "Arkhangelsk", ark, "/m/Arkhangelsk/CD02", 1, 1);
    coffret_manuel_d_avant_2094(
        db,
        "Arkhangelsk",
        &[a1, a2],
        vec![
            retenu(1, "ARKHANGELSK ", "/m/Arkhangelsk/CD01"),
            retenu(2, "Arkhangelsk", "/m/Arkhangelsk/CD02"),
        ],
    );

    let rapport = rattraper_les_sous_titres(db).unwrap();
    assert_eq!(
        rapport,
        RattrapageSousTitres {
            deja_fait: false,
            coffrets: 4,
            coffrets_rattrapes: 2,
            disques_sous_titres: 4,
            titres_inconnus: 1,
            disques_sous_titres_par_balise: 0,
            // Le coffret « Yes Box » : ses fichiers n'existent pas.
            fichiers_illisibles: 2,
            disques_deplaces: 1,
            echecs: 0,
        }
    );
    assert_eq!(
        sous_titres_de(db, d1),
        vec![
            (1, Some("Oratorio, Disc 1".into())),
            (2, Some("Oratorio, Disc 2".into())),
            (3, None),
            (3, Some("BWV 248".into())),
        ],
        "coffret auto : chaque disque prend son titre d'origine, le disque 3 garde le sien"
    );
    assert_eq!(
        sous_titres_de(db, m1),
        vec![
            (1, Some("101 - Disc A".into())),
            (2, Some("101 - Disc B".into())),
            (3, None),
        ],
        "coffret manuel : le disque 3, déplacé, est laissé"
    );
    assert_eq!(sous_titres_de(db, s1), vec![(1, None), (2, None)]);
    assert_eq!(sous_titres_de(db, a1), vec![(1, None), (2, None)]);
    let m = marqueurs(db).unwrap();
    assert_eq!(m[&d1].sous_titres, vec![1, 2]);
    assert_eq!(m[&m1].sous_titres, vec![1, 2]);
    assert_eq!(
        m[&m1].titre_compose.as_deref(),
        Some("101"),
        "le reste du marqueur est gardé"
    );
    assert!(m[&s1].sous_titres.is_empty() && m[&a1].sous_titres.is_empty());
    // La disposition tenue du coffret manuel retient le nouveau nom : une
    // relecture des fichiers ne l'effacera pas.
    let tenue = &crate::db::edition_album::editions_tenues(db, &[m1]).unwrap()[0];
    assert!(tenue.disposition);
    let noms: BTreeSet<(i32, Option<String>)> = tenue
        .pistes
        .iter()
        .map(|p| (p.disque, p.nom_disque.clone()))
        .collect();
    assert_eq!(
        noms,
        BTreeSet::from([
            (1, Some("101 - Disc A".to_string())),
            (2, Some("101 - Disc B".to_string())),
            (3, None),
        ])
    );
    assert!(
        reglages
            .get(CLE_RATTRAPAGE_SOUS_TITRES_2094)
            .unwrap()
            .is_some()
    );

    // UNE SEULE FOIS : un nom retiré par l'utilisateur depuis ne revient pas.
    db.execute(
        &format!(
            "UPDATE tracks SET disc_subtitle = NULL WHERE album_id = {p1} \
             AND COALESCE(disc_number, 1) = {p2}"
        ),
        &[&d1 as &dyn ToSqlValue, &1i64],
    )
    .unwrap();
    assert_eq!(
        rattraper_les_sous_titres(db).unwrap(),
        RattrapageSousTitres {
            deja_fait: true,
            ..Default::default()
        }
    );
    assert_eq!(sous_titres_de(db, d1)[0], (1, None));

    // DÉFAIRE reste exact : ce que le rattrapage a posé part, le reste reste.
    let recrees = defaire(db, d1).unwrap();
    let mut restes: Vec<(i64, Option<String>)> = std::iter::once(d1)
        .chain(recrees.iter().copied())
        .flat_map(|id| sous_titres_de(db, id))
        .collect();
    restes.sort();
    restes.dedup();
    assert_eq!(
        restes,
        vec![(1, None), (2, None), (3, None), (3, Some("BWV 248".into()))]
    );
    let recrees = crate::db::edition_album::defaire_coffret_manuel(db, m1).unwrap();
    let restes: BTreeSet<Option<String>> = std::iter::once(m1)
        .chain(recrees.iter().copied())
        .flat_map(|id| sous_titres_de(db, id))
        .map(|(_, s)| s)
        .collect();
    assert_eq!(restes, BTreeSet::from([None]), "coffret manuel défait");
}

#[test]
fn rattrapage_sous_titres_2094_sur_sqlite() {
    scenario_rattrapage_sous_titres_2094(&sqlite());
}

/// Fil 2094 — un coffret automatique réuni auquel s'ajoute un disque arrivé
/// plus tard : le marqueur garde les sous-titres de la première réunion, et
/// « Défaire » les retire tous.
pub(crate) fn scenario_disque_tardif_2094(db: &Arc<dyn DbBackend>) {
    let _ = db.execute("DELETE FROM album_metadata", &[]);
    let _ = db.execute("DELETE FROM album_distinct_pairs", &[]);
    let _ = SettingsRepo::with_backend(db.clone()).delete(CLE_REFUS);
    let h = artiste(db, "Haendel");
    let r = "/m/CLASSIQUE/Messiah";
    let d1 = disque(
        db,
        "Messiah, Disc 1",
        h,
        &format!("{r}/Messiah, Disc 1"),
        2,
        1,
    );
    disque(
        db,
        "Messiah, Disc 2",
        h,
        &format!("{r}/Messiah, Disc 2"),
        2,
        2,
    );
    assert_eq!(passe(db).unwrap().reunis, 1);
    assert_eq!(marqueurs(db).unwrap()[&d1].sous_titres, vec![1, 2]);

    disque(
        db,
        "Messiah, Disc 3",
        h,
        &format!("{r}/Messiah, Disc 3"),
        2,
        3,
    );
    let rapport = passe(db).unwrap();
    assert_eq!(
        rapport.reunis, 1,
        "le disque 3 rejoint le coffret : {rapport:?}"
    );
    assert_eq!(
        sous_titres_de(db, d1),
        vec![
            (1, Some("Messiah, Disc 1".into())),
            (2, Some("Messiah, Disc 2".into())),
            (3, Some("Messiah, Disc 3".into())),
        ]
    );
    assert_eq!(
        marqueurs(db).unwrap()[&d1].sous_titres,
        vec![1, 2, 3],
        "les sous-titres de la première réunion restent au marqueur"
    );
    let recrees = defaire(db, d1).unwrap();
    let restes: BTreeSet<Option<String>> = std::iter::once(d1)
        .chain(recrees.iter().copied())
        .flat_map(|id| sous_titres_de(db, id))
        .map(|(_, s)| s)
        .collect();
    assert_eq!(restes, BTreeSet::from([None]));
}

#[test]
fn disque_tardif_2094_sur_sqlite() {
    scenario_disque_tardif_2094(&sqlite());
}

/// Une copie d'un vrai fichier du dépôt (`tests/fixtures/<gabarit>`), sa
/// balise ALBUM posée à `album`.
fn fichier_tague(dossier: &std::path::Path, nom: &str, gabarit: &str, album: &str) -> String {
    use lofty::config::WriteOptions;
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::tag::{Accessor, Tag};
    std::fs::create_dir_all(dossier).unwrap();
    let chemin = dossier.join(nom);
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(gabarit),
        &chemin,
    )
    .unwrap();
    let mut f = lofty::read_from_path(&chemin).unwrap();
    if f.primary_tag().is_none() {
        let genre = f.primary_tag_type();
        f.insert_tag(Tag::new(genre));
    }
    f.primary_tag_mut().unwrap().set_album(album.to_string());
    f.save_to_path(&chemin, WriteOptions::default()).unwrap();
    chemin.to_string_lossy().into_owned()
}

/// Fil 2094 (décision de Bertrand du 05/10/2026) — un coffret dont le
/// marqueur ne retient AUCUN titre d'origine : le rattrapage relit la balise
/// ALBUM de la PREMIÈRE piste de chaque disque, sur de vrais FLAC et MP3, et
/// applique la même règle. Le marqueur est complété : « Défaire » retire ce
/// qui a été posé.
pub(crate) fn scenario_rattrapage_par_les_balises_2094(db: &Arc<dyn DbBackend>) {
    let _ = db.execute("DELETE FROM album_metadata", &[]);
    let _ = db.execute("DELETE FROM album_distinct_pairs", &[]);
    let reglages = SettingsRepo::with_backend(db.clone());
    let _ = reglages.delete(CLE_REFUS);
    let _ = reglages.delete(CLE_RATTRAPAGE_SOUS_TITRES_2094);
    let racine = tempfile::tempdir().unwrap();
    let r = racine.path();
    let y = artiste(db, "Yes");
    let un_disque = |titre: &str, dossier: &std::path::Path, pistes: &[(i32, String)]| {
        let id = album(db, titre, y, &dossier.to_string_lossy());
        for (n, chemin) in pistes {
            piste(db, id, y, *n, 1, chemin);
        }
        id
    };
    // Disque 1 (FLAC) : la piste 2 est ENREGISTRÉE avant la piste 1, avec
    // une autre balise — c'est la PREMIÈRE piste qui fait foi.
    let dossier1 = r.join("Yes Box/CD1");
    let p2 = fichier_tague(&dossier1, "02.flac", "test.flac", "Relayer (bonus)");
    let p1 = fichier_tague(&dossier1, "01.flac", "test.flac", "Relayer");
    let b1 = un_disque("Yes Box", &dossier1, &[(2, p2), (1, p1)]);
    // Disque 2 (MP3, ID3v2).
    let dossier2 = r.join("Yes Box/CD2");
    let b2 = un_disque(
        "Fragile",
        &dossier2,
        &[(1, fichier_tague(&dossier2, "01.mp3", "test.mp3", "Fragile"))],
    );
    // Disque 3 : balise identique au titre du coffret — rien.
    let dossier3 = r.join("Yes Box/CD3");
    let b3 = un_disque(
        "Yes Box",
        &dossier3,
        &[(
            1,
            fichier_tague(&dossier3, "01.flac", "test.flac", "YES box"),
        )],
    );
    // Disque 4 : fichier ABSENT (partage démonté) — laissé, compté.
    let dossier4 = r.join("Yes Box/CD4");
    let b4 = un_disque(
        "Yes Box",
        &dossier4,
        &[(1, dossier4.join("01.flac").to_string_lossy().into_owned())],
    );
    // Disque 5 : porte DÉJÀ un nom — jamais remplacé.
    let dossier5 = r.join("Yes Box/CD5");
    let b5 = un_disque(
        "Yes Box",
        &dossier5,
        &[(1, fichier_tague(&dossier5, "01.flac", "test.flac", "90125"))],
    );
    let (p1, p2) = placeholders(db);
    db.execute(
        &format!("UPDATE tracks SET disc_subtitle = {p1} WHERE album_id = {p2}"),
        &[&"Bonus" as &dyn ToSqlValue, &b5],
    )
    .unwrap();
    coffret_manuel_d_avant_2094(db, "Yes Box", &[b1, b2, b3, b4, b5], vec![]);
    // Le marqueur d'un coffret d'avant #5319 : ni disques, ni titre composé.
    AlbumMetadataRepo::with_backend(db.clone())
        .set(
            b1,
            CLE_COFFRET,
            &serde_json::to_string(&Marqueur::manuel()).unwrap(),
        )
        .unwrap();

    let rapport = rattraper_les_sous_titres(db).unwrap();
    assert_eq!(
        rapport,
        RattrapageSousTitres {
            deja_fait: false,
            coffrets: 1,
            coffrets_rattrapes: 1,
            disques_sous_titres: 2,
            titres_inconnus: 0,
            disques_sous_titres_par_balise: 2,
            fichiers_illisibles: 1,
            disques_deplaces: 0,
            echecs: 0,
        }
    );
    assert_eq!(
        sous_titres_de(db, b1),
        vec![
            (1, Some("Relayer".into())),
            (2, Some("Fragile".into())),
            (3, None),
            (4, None),
            (5, Some("Bonus".into())),
        ],
        "balise de la première piste ; titre du coffret, fichier absent et nom existant laissés"
    );
    let m = marqueurs(db).unwrap();
    assert_eq!(m[&b1].sous_titres, vec![1, 2]);
    assert_eq!(
        m[&b1]
            .disques
            .iter()
            .map(|d| (d.n, d.titre.as_str()))
            .collect::<Vec<_>>(),
        vec![(1, "Relayer"), (2, "Fragile")],
        "les titres relus rejoignent le marqueur"
    );
    let tenue = &crate::db::edition_album::editions_tenues(db, &[b1]).unwrap()[0];
    assert!(
        tenue
            .pistes
            .iter()
            .filter(|p| p.disque == 2)
            .all(|p| p.nom_disque.as_deref() == Some("Fragile")),
        "la disposition tenue retient le nom relu"
    );

    // DÉFAIRE : ce qui a été posé part, « Bonus » reste.
    let recrees = crate::db::edition_album::defaire_coffret_manuel(db, b1).unwrap();
    let restes: BTreeSet<Option<String>> = std::iter::once(b1)
        .chain(recrees.iter().copied())
        .flat_map(|id| sous_titres_de(db, id))
        .map(|(_, s)| s)
        .collect();
    assert_eq!(restes, BTreeSet::from([None, Some("Bonus".to_string())]));
}

#[test]
fn rattrapage_par_les_balises_2094_sur_sqlite() {
    scenario_rattrapage_par_les_balises_2094(&sqlite());
}
