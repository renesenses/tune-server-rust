//! #6459 — après une mise à jour, des albums perdaient leur artiste et
//! passaient sous « Unknown Artist ».
//!
//! Cause : un fichier relu avec un titre d'album mais sans balise d'artiste
//! calcule la clé « Unknown Artist ». La reprise de #5837
//! (`album_du_fichier_a_reprendre`) reprenait alors la ligne que ce fichier
//! occupe déjà et lui ÉCRIVAIT cet artiste inconnu, en contournant la garde
//! que la voie par dossier applique (`reclaim_unknown_artist`). Un scan
//! ordinaire saute ensuite les fichiers inchangés : rien ne réparait.
//!
//! Ces témoins jouent le trajet du scan (`begin_batch`, `import`, insertion
//! ou mise à jour selon le chemin, purge des orphelins), puis la passe de
//! réparation qui suit le scan et le démarrage. Données synthétiques.
use std::sync::Arc;

use tune_core::db::album_metadata_repo::AlbumMetadataRepo;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::{ArtistRepo, UNKNOWN_ARTIST_NAME};
use tune_core::db::backend::{DbBackend, ToSqlValue};
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_repo::TrackRepo;
use tune_core::metadata::TrackMetadata;
use tune_core::scanner::walker::ScannedFile;

use super::{PorteeDuScan, TrackImporter};

const ARTISTE: &str = "Richard Wagner";
const ALBUM: &str = "Ouvertures";

fn base() -> Arc<dyn DbBackend> {
    let sqlite = SqliteDb::open_in_memory().unwrap();
    sqlite.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&sqlite).unwrap();
    Arc::new(sqlite)
}

/// Les trois pistes de l'album, balisées (`avec_artiste`) ou relues avec un
/// titre d'album mais ni ARTIST ni ALBUMARTIST.
fn fichiers(racine: &std::path::Path, avec_artiste: bool) -> Vec<ScannedFile> {
    let dossier = racine.join(ARTISTE).join(ALBUM);
    std::fs::create_dir_all(&dossier).unwrap();
    (1..=3)
        .map(|n| {
            let chemin = dossier
                .join(format!("{n:02}.flac"))
                .to_string_lossy()
                .into_owned();
            std::fs::write(&chemin, b"pas-du-vrai-audio").unwrap();
            ScannedFile {
                path: chemin,
                metadata: Some(TrackMetadata {
                    title: Some(format!("Ouverture {n}")),
                    artist: avec_artiste.then(|| ARTISTE.to_string()),
                    album: Some(ALBUM.to_string()),
                    album_artist: avec_artiste.then(|| ARTISTE.to_string()),
                    track_number: Some(n),
                    disc_number: Some(1),
                    year: Some(1990),
                    format: Some("flac".into()),
                    sample_rate: Some(44_100),
                    bit_depth: Some(16),
                    ..Default::default()
                }),
                unsupported: None,
                audio_hash: Some(format!("hash-{n}")),
                file_size: 4096,
                mtime: 1_700_000_000.0,
            }
        })
        .collect()
}

/// Une passe de scan telle que `scan.rs` la joue.
fn scanner(db: &Arc<dyn DbBackend>, racine: &std::path::Path, lot: &[ScannedFile], qs: bool) {
    let pistes = TrackRepo::with_backend(db.clone());
    let mut imp = TrackImporter::new(db.clone(), qs, racine.join("cache"), PorteeDuScan::TOUT);
    imp.begin_batch(lot);
    let (mut a_inserer, mut a_mettre_a_jour) = (Vec::new(), Vec::new());
    for sf in lot {
        let (mut t, _) = imp.import(sf).expect("import");
        match pistes.get_by_path(&sf.path).unwrap().and_then(|x| x.id) {
            Some(id) => {
                t.id = Some(id);
                a_mettre_a_jour.push(t);
            }
            None => a_inserer.push(t),
        }
    }
    pistes.create_batch(&a_inserer).unwrap();
    pistes.update_batch(&a_mettre_a_jour).unwrap();
    AlbumRepo::with_backend(db.clone())
        .delete_orphans()
        .unwrap();
}

/// `(album_id, artiste de l'album)` de chaque piste.
fn rangement(db: &Arc<dyn DbBackend>, lot: &[ScannedFile]) -> Vec<(i64, Option<String>)> {
    let pistes = TrackRepo::with_backend(db.clone());
    let albums = AlbumRepo::with_backend(db.clone());
    lot.iter()
        .map(|f| {
            let id = pistes
                .get_by_path(&f.path)
                .unwrap()
                .unwrap()
                .album_id
                .unwrap();
            (id, albums.get(id).unwrap().unwrap().artist_name)
        })
        .collect()
}

fn une_relecture_sans_artiste_garde_l_artiste(quality_split: bool) {
    let tmp = tempfile::tempdir().unwrap();
    let db = base();
    let balises = fichiers(tmp.path(), true);
    scanner(&db, tmp.path(), &balises, quality_split);
    let avant = rangement(&db, &balises);
    assert!(
        avant.iter().all(|r| r == &avant[0]),
        "un seul album : {avant:?}"
    );
    assert_eq!(avant[0].1.as_deref(), Some(ARTISTE));

    // Relecture : même fichiers, plus aucune balise d'artiste.
    scanner(&db, tmp.path(), &fichiers(tmp.path(), false), quality_split);
    let apres = rangement(&db, &balises);
    assert!(
        apres.iter().all(|r| r == &avant[0]),
        "#6459 (quality_split={quality_split}) — une relecture sans balise d'artiste a \
         rétrogradé l'album : avant {avant:?}, après {apres:?}"
    );
    assert_eq!(
        AlbumRepo::with_backend(db.clone()).count().unwrap(),
        1,
        "aucune seconde ligne « Unknown Artist »"
    );
}

#[test]
fn une_relecture_sans_artiste_garde_l_artiste_avec_quality_split() {
    une_relecture_sans_artiste_garde_l_artiste(true);
}

#[test]
fn une_relecture_sans_artiste_garde_l_artiste_sans_quality_split() {
    une_relecture_sans_artiste_garde_l_artiste(false);
}

#[test]
fn le_realignement_ne_retrograde_pas_un_artiste_nomme() {
    let tmp = tempfile::tempdir().unwrap();
    let db = base();
    let lot = fichiers(tmp.path(), true);
    scanner(&db, tmp.path(), &lot, true);
    let id = rangement(&db, &lot)[0].0;
    let inconnu = ArtistRepo::with_backend(db.clone())
        .get_or_create(UNKNOWN_ARTIST_NAME, None, None)
        .unwrap()
        .id
        .unwrap();
    let albums = AlbumRepo::with_backend(db.clone());
    assert!(
        !albums
            .realigner_sur_les_balises(id, None, Some(inconnu))
            .unwrap()
    );
    assert_eq!(
        albums.get(id).unwrap().unwrap().artist_name.as_deref(),
        Some(ARTISTE),
        "#6459 — le réalignement a écrit « Unknown Artist » sur un album nommé"
    );
}

/// Un album déjà abîmé par une version antérieure : sa ligne est sous
/// « Unknown Artist » alors que ses pistes, en base, savent qui il est.
fn album_abime(db: &Arc<dyn DbBackend>, racine: &std::path::Path) -> i64 {
    let lot = fichiers(racine, true);
    scanner(db, racine, &lot, true);
    let id = rangement(db, &lot)[0].0;
    let inconnu = ArtistRepo::with_backend(db.clone())
        .get_or_create(UNKNOWN_ARTIST_NAME, None, None)
        .unwrap()
        .id
        .unwrap();
    let params: [&dyn ToSqlValue; 2] = [&inconnu, &id];
    db.execute("UPDATE albums SET artist_id = ? WHERE id = ?", &params)
        .unwrap();
    id
}

fn artiste_de(db: &Arc<dyn DbBackend>, id: i64) -> Option<String> {
    AlbumRepo::with_backend(db.clone())
        .get(id)
        .unwrap()
        .unwrap()
        .artist_name
}

#[test]
fn un_album_deja_abime_est_repare_sans_relire_ses_fichiers() {
    let tmp = tempfile::tempdir().unwrap();
    let db = base();
    let id = album_abime(&db, tmp.path());
    assert_eq!(artiste_de(&db, id).as_deref(), Some(UNKNOWN_ARTIST_NAME));

    // Le scan suivant saute les fichiers inchangés : seule la passe de
    // réparation qui le suit joue.
    let bilan = AlbumRepo::with_backend(db.clone())
        .rendre_leur_artiste_aux_albums_inconnus()
        .unwrap();
    assert_eq!(bilan.rendus, 1, "{bilan:?}");
    assert_eq!(
        artiste_de(&db, id).as_deref(),
        Some(ARTISTE),
        "#6459 — l'album abîmé est resté sous « Unknown Artist »"
    );
    let encore = AlbumRepo::with_backend(db.clone())
        .rendre_leur_artiste_aux_albums_inconnus()
        .unwrap();
    assert_eq!(encore.rendus, 0, "idempotente : {encore:?}");
}

#[test]
fn un_album_abime_sans_balise_d_album_reprend_l_artiste_unanime_de_ses_pistes() {
    let tmp = tempfile::tempdir().unwrap();
    let db = base();
    let id = album_abime(&db, tmp.path());
    db.execute("UPDATE tracks SET album_artist = NULL", &[])
        .unwrap();
    AlbumRepo::with_backend(db.clone())
        .rendre_leur_artiste_aux_albums_inconnus()
        .unwrap();
    assert_eq!(artiste_de(&db, id).as_deref(), Some(ARTISTE));
}

#[test]
fn un_artiste_tenu_a_la_main_ne_bouge_pas() {
    let tmp = tempfile::tempdir().unwrap();
    let db = base();
    let id = album_abime(&db, tmp.path());
    AlbumMetadataRepo::with_backend(db.clone())
        .marquer_edition_manuelle(id, &["artist"])
        .unwrap();
    let bilan = AlbumRepo::with_backend(db.clone())
        .rendre_leur_artiste_aux_albums_inconnus()
        .unwrap();
    assert_eq!(bilan.manuels, 1, "{bilan:?}");
    assert_eq!(artiste_de(&db, id).as_deref(), Some(UNKNOWN_ARTIST_NAME));
}

#[test]
fn une_compilation_et_un_album_sans_artiste_unique_ne_bougent_pas() {
    let tmp = tempfile::tempdir().unwrap();
    let db = base();
    let id = album_abime(&db, tmp.path());
    let albums = AlbumRepo::with_backend(db.clone());

    // Balises d'album contradictoires : indécis.
    db.execute(
        "UPDATE tracks SET album_artist = 'Autre chef' WHERE track_number = 2",
        &[],
    )
    .unwrap();
    assert_eq!(
        albums
            .rendre_leur_artiste_aux_albums_inconnus()
            .unwrap()
            .indecis,
        1
    );
    assert_eq!(artiste_de(&db, id).as_deref(), Some(UNKNOWN_ARTIST_NAME));

    // Balise « Various Artists » : la règle compilation décide, pas ici.
    db.execute("UPDATE tracks SET album_artist = 'Various Artists'", &[])
        .unwrap();
    albums.rendre_leur_artiste_aux_albums_inconnus().unwrap();
    assert_eq!(artiste_de(&db, id).as_deref(), Some(UNKNOWN_ARTIST_NAME));

    // Album marqué compilation : jamais lu par cette passe.
    db.execute(
        &format!("UPDATE tracks SET album_artist = '{ARTISTE}'"),
        &[],
    )
    .unwrap();
    let params: [&dyn ToSqlValue; 1] = [&id];
    db.execute("UPDATE albums SET is_compilation = 1 WHERE id = ?", &params)
        .unwrap();
    assert_eq!(
        albums
            .rendre_leur_artiste_aux_albums_inconnus()
            .unwrap()
            .examines,
        0
    );
    assert_eq!(artiste_de(&db, id).as_deref(), Some(UNKNOWN_ARTIST_NAME));
}
