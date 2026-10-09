//! Témoins du numéro de disque déduit du dossier.
use std::sync::Arc;

use super::*;
use crate::db::album_doublons::{Declencheur, FusionDesDoublons};
use crate::db::album_metadata_repo::AlbumMetadataRepo;
use crate::db::album_repo::AlbumRepo;
use crate::db::artist_repo::ArtistRepo;
use crate::db::models::{Album, Artist, Track};
use crate::db::sqlite::SqliteDb;
use crate::db::track_metadata_repo::TrackMetadataRepo;
use crate::db::track_repo::TrackRepo;

fn p(id: i64, chemin: &str, disque: i32, numero: i32, titre: &str) -> PisteADisposer {
    PisteADisposer {
        id,
        chemin: chemin.into(),
        disque,
        numero,
        titre: titre.into(),
    }
}

/// Deux dossiers frères aux noms libres, numérotés chacun à partir de 1.
fn jour_et_nuit() -> Vec<PisteADisposer> {
    vec![
        p(1, "/m/A/Nocturnes/Le jour/01.flac", 1, 1, "Aube"),
        p(2, "/m/A/Nocturnes/Le jour/02.flac", 1, 2, "Midi"),
        p(3, "/m/A/Nocturnes/La nuit/01.flac", 1, 1, "Crépuscule"),
        p(4, "/m/A/Nocturnes/La nuit/02.flac", 1, 2, "Minuit"),
    ]
}

#[test]
fn ordre_naturel_des_noms_de_dossier() {
    assert_eq!(ordre_naturel("Partie 2", "Partie 10"), Ordering::Less);
    assert_eq!(ordre_naturel("Partie 010", "Partie 9"), Ordering::Greater);
    assert_eq!(ordre_naturel("La nuit", "Le jour"), Ordering::Less);
    assert_eq!(ordre_naturel("été", "Hiver"), Ordering::Less);
    assert_eq!(ordre_naturel("a", "A"), "a".cmp("A"));
}

#[test]
fn un_disque_par_dossier_dans_l_ordre_naturel() {
    let mut r = deduire(&jour_et_nuit()).expect("règle tenue");
    r.sort();
    assert_eq!(r, vec![(1, 2), (2, 2), (3, 1), (4, 1)]);
    // L'ordre de lecture ne change rien.
    let mut inverse = jour_et_nuit();
    inverse.reverse();
    let mut r2 = deduire(&inverse).unwrap();
    r2.sort();
    assert_eq!(r, r2);
    // Une piste déjà rangée au disque de son dossier ne gêne pas.
    let mut deja = jour_et_nuit();
    deja[0].disque = 2;
    let mut r3 = deduire(&deja).unwrap();
    r3.sort();
    assert_eq!(r, r3);
}

#[test]
fn les_cas_qui_ne_sont_pas_touches() {
    // Un seul dossier.
    let seul: Vec<_> = jour_et_nuit()
        .into_iter()
        .filter(|x| x.chemin.contains("jour"))
        .collect();
    assert_eq!(deduire(&seul), None);
    // Un disque déjà au-delà de 1 (balise, `CD2`, coffret réuni).
    let mut tague = jour_et_nuit();
    tague[2].disque = 2;
    tague[3].disque = 2;
    assert_eq!(deduire(&tague), None);
    // Numérotation continue d'un dossier à l'autre : rien ne se répète.
    let continu = vec![
        p(1, "/m/A/X/Un/01.flac", 1, 1, "a"),
        p(2, "/m/A/X/Deux/02.flac", 1, 2, "b"),
    ];
    assert_eq!(deduire(&continu), None);
    // Deux copies du même disque, côte à côte.
    let copies = vec![
        p(1, "/m/A/X [16-44]/01.flac", 1, 1, "Aube"),
        p(2, "/m/A/X [24-96]/01.flac", 1, 1, "aube "),
    ];
    assert_eq!(deduire(&copies), None);
    // Un numéro en double DANS un même dossier.
    let mut desordre = jour_et_nuit();
    desordre.push(p(5, "/m/A/Nocturnes/Le jour/01b.flac", 1, 1, "Aube bis"));
    assert_eq!(deduire(&desordre), None);
    // Dossiers non frères.
    let epars = vec![
        p(1, "/m/A/X/Un/01.flac", 1, 1, "a"),
        p(2, "/m/B/X/Deux/01.flac", 1, 1, "b"),
    ];
    assert_eq!(deduire(&epars), None);
    // Dossiers posés à la racine.
    let racine = vec![
        p(1, "/Un/01.flac", 1, 1, "a"),
        p(2, "/Deux/01.flac", 1, 1, "b"),
    ];
    assert_eq!(deduire(&racine), None);
}

#[test]
fn chemins_windows() {
    let w = vec![
        p(1, r"D:\Musique\A\X\Le jour\01.flac", 1, 1, "a"),
        p(2, r"D:\Musique\A\X\La nuit\01.flac", 1, 1, "b"),
    ];
    let mut r = deduire(&w).unwrap();
    r.sort();
    assert_eq!(r, vec![(1, 2), (2, 1)]);
}

// ---------------------------------------------------------------------------
// Le scénario en base, sur `Arc<dyn DbBackend>`.
// ---------------------------------------------------------------------------

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

/// Un album d'un dossier, `n` pistes au disque `disque`, comme le scan le
/// laisse : un fichier sans DISCNUMBER est rangé au disque 1.
fn album_du_dossier(
    db: &Arc<dyn DbBackend>,
    titre: &str,
    artiste: i64,
    dossier: &str,
    n: i32,
    disque: i32,
) -> i64 {
    let mut a = Album::new(titre.into());
    a.artist_id = Some(artiste);
    let repo = AlbumRepo::with_backend(db.clone());
    let id = repo.create(&a).unwrap();
    repo.set_folder_path(id, dossier).unwrap();
    for k in 1..=n {
        let mut t = Track::new(format!("{dossier} {k}"));
        t.album_id = Some(id);
        t.artist_id = Some(artiste);
        t.track_number = k;
        t.disc_number = disque;
        t.file_path = Some(format!("{dossier}/{k:02}.flac"));
        TrackRepo::with_backend(db.clone()).create(&t).unwrap();
    }
    id
}

/// `(dossier, disque, numéro)` de chaque piste locale de la bibliothèque dont
/// le chemin commence par `prefixe`, triés.
fn disposition(db: &Arc<dyn DbBackend>, prefixe: &str) -> Vec<(String, i64, i64)> {
    let mut v: Vec<(String, i64, i64)> = db
        .query_many(
            "SELECT file_path, COALESCE(disc_number, 1), track_number FROM tracks",
            &[],
        )
        .unwrap()
        .into_iter()
        .filter_map(|r| {
            let c = r.first()?.as_string()?;
            if !c.starts_with(prefixe) {
                return None;
            }
            let dossier = c.rsplit_once('/')?.0.to_string();
            Some((dossier, r.get(1)?.as_i64()?, r.get(2)?.as_i64()?))
        })
        .collect();
    v.sort();
    v
}

fn albums_de(db: &Arc<dyn DbBackend>, prefixe: &str) -> Vec<i64> {
    let mut v: Vec<i64> = db
        .query_many("SELECT album_id, file_path FROM tracks", &[])
        .unwrap()
        .into_iter()
        .filter_map(|r| {
            let id = r.first()?.as_i64()?;
            r.get(1)?.as_string()?.starts_with(prefixe).then_some(id)
        })
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

fn disc_count(db: &Arc<dyn DbBackend>, id: i64) -> i64 {
    db.query_one(
        &format!("SELECT disc_count FROM albums WHERE id = {}", marque(db, 1)),
        &[&id as &dyn ToSqlValue],
    )
    .unwrap()
    .and_then(|r| r.first()?.as_i64())
    .unwrap_or(-1)
}

/// Le défaut, de bout en bout : deux dossiers frères aux noms libres, même
/// balise ALBUM, aucune DISCNUMBER. La fusion des doublons de fin de scan
/// les réunit ; chaque dossier doit devenir un disque, dans l'ordre naturel
/// des noms (« La nuit » avant « Le jour », bien que créé après).
pub(crate) fn scenario_dossiers_freres(db: &Arc<dyn DbBackend>) {
    let a = artiste(db, "Ensemble");
    album_du_dossier(db, "Nocturnes", a, "/m/Ensemble/Nocturnes/Le jour", 3, 1);
    album_du_dossier(db, "Nocturnes", a, "/m/Ensemble/Nocturnes/La nuit", 2, 1);

    // Les témoins qui ne doivent pas bouger.
    // Un album d'un seul dossier sans DISCNUMBER.
    album_du_dossier(db, "Solo", a, "/m/Ensemble/Solo", 3, 1);
    // Un coffret `CD01/CD02` : ses disques viennent du chemin.
    let c1 = album_du_dossier(db, "Coffret", a, "/m/Ensemble/Coffret/CD01", 2, 1);
    let c2 = album_du_dossier(db, "Coffret", a, "/m/Ensemble/Coffret/CD02", 2, 2);
    let repo = AlbumRepo::with_backend(db.clone());
    repo.absorber(c1, c2).unwrap();
    // Deux albums aux disques disposés à la main.
    album_du_dossier(db, "Tenu", a, "/m/Ensemble/Tenu/Face A", 2, 1);
    album_du_dossier(db, "Tenu", a, "/m/Ensemble/Tenu/Face B", 2, 1);
    // Un album dont une piste a son numéro de disque tenu à la main.
    album_du_dossier(db, "Garde", a, "/m/Ensemble/Garde/Recto", 2, 1);
    album_du_dossier(db, "Garde", a, "/m/Ensemble/Garde/Verso", 2, 1);

    let bilan = FusionDesDoublons::with_backend(db.clone())
        .fusionner(Declencheur::FinDeScan)
        .unwrap();
    assert!(bilan.fusionnes >= 3, "{bilan:?}");
    // La fusion elle-même range déjà les disques : pas besoin d'attendre le
    // scan suivant.
    let par_disque: Vec<(i64, i64)> = disposition(db, "/m/Ensemble/Nocturnes/")
        .into_iter()
        .map(|(_, d, n)| (d, n))
        .collect();
    assert_eq!(par_disque, vec![(1, 1), (1, 2), (2, 1), (2, 2), (2, 3)]);

    // Les gardes posées APRÈS la fusion, sur l'album conservé : c'est là que
    // l'utilisateur les pose. On rejoue alors la fusion de « Tenu » et
    // « Garde » à la main : on remet leurs disques à 1 puis on relance la passe.
    let tenu = albums_de(db, "/m/Ensemble/Tenu/");
    assert_eq!(tenu.len(), 1);
    let garde = albums_de(db, "/m/Ensemble/Garde/");
    assert_eq!(garde.len(), 1);
    db.execute(
        &format!(
            "UPDATE tracks SET disc_number = 1 WHERE album_id IN ({}, {})",
            marque(db, 1),
            marque(db, 2)
        ),
        &[&tenu[0] as &dyn ToSqlValue, &garde[0]],
    )
    .unwrap();
    AlbumMetadataRepo::with_backend(db.clone())
        .set(
            tenu[0],
            crate::db::edition_album::CLE_EDITION_PISTES,
            "{\"pistes\":[]}",
        )
        .unwrap();
    let piste_gardee = db
        .query_one(
            &format!(
                "SELECT id FROM tracks WHERE album_id = {} ORDER BY id LIMIT 1",
                marque(db, 1)
            ),
            &[&garde[0] as &dyn ToSqlValue],
        )
        .unwrap()
        .and_then(|r| r.first()?.as_i64())
        .unwrap();
    TrackMetadataRepo::with_backend(db.clone())
        .set(
            piste_gardee,
            crate::db::champs_tenus::CLE,
            "{\"chemin\":\"/m/Ensemble/Garde/Recto/01.flac\",\"disc_number\":1}",
        )
        .unwrap();
    passe(db).unwrap();

    // Le défaut corrigé.
    let noct = albums_de(db, "/m/Ensemble/Nocturnes/");
    assert_eq!(noct.len(), 1, "les deux dossiers forment UN album");
    let nuit = "/m/Ensemble/Nocturnes/La nuit".to_string();
    let jour = "/m/Ensemble/Nocturnes/Le jour".to_string();
    assert_eq!(
        disposition(db, "/m/Ensemble/Nocturnes/"),
        vec![
            (nuit.clone(), 1, 1),
            (nuit.clone(), 1, 2),
            (jour.clone(), 2, 1),
            (jour.clone(), 2, 2),
            (jour.clone(), 2, 3),
        ]
    );
    assert_eq!(disc_count(db, noct[0]), 2);

    // Les témoins.
    assert!(
        disposition(db, "/m/Ensemble/Solo/")
            .iter()
            .all(|(_, d, _)| *d == 1)
    );
    let coffret: Vec<i64> = disposition(db, "/m/Ensemble/Coffret/")
        .iter()
        .map(|(_, d, _)| *d)
        .collect();
    assert_eq!(coffret, vec![1, 1, 2, 2]);
    assert!(
        disposition(db, "/m/Ensemble/Tenu/")
            .iter()
            .all(|(_, d, _)| *d == 1),
        "disposition tenue à la main : intacte"
    );
    assert!(
        disposition(db, "/m/Ensemble/Garde/")
            .iter()
            .all(|(_, d, _)| *d == 1),
        "numéro de disque tenu à la main : intact"
    );

    // Stable : une piste relue au disque 1 par un scan suivant reprend le
    // sien, et une seconde passe ne change plus rien.
    db.execute(
        &format!(
            "UPDATE tracks SET disc_number = 1 WHERE file_path = {}",
            marque(db, 1)
        ),
        &[&"/m/Ensemble/Nocturnes/Le jour/02.flac" as &dyn ToSqlValue],
    )
    .unwrap();
    let b = passe(db).unwrap();
    assert_eq!((b.albums, b.pistes), (1, 1));
    assert_eq!(disposition(db, "/m/Ensemble/Nocturnes/")[3], (jour, 2, 2));
    let b = passe(db).unwrap();
    assert_eq!((b.albums, b.pistes), (0, 0));
}

#[test]
fn dossiers_freres_sans_discnumber_sur_sqlite() {
    scenario_dossiers_freres(&sqlite());
}
