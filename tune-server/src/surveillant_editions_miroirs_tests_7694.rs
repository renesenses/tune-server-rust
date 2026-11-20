//! #7694 (fil 2564) — la voie du SURVEILLANT de fichiers. Le correctif #7804
//! a appris la qualité à la règle des dossiers miroirs (#4907) pour le scan
//! par lots (`scan_import`), mais le surveillant (`build_track_from_metadata_opts`)
//! appelait encore `get_or_create_for_folder`, sans qualité : une édition CD
//! posée sous `…/CD/Artiste/Album` pendant que Tune tourne rejoignait l'album
//! de l'édition DSD déjà indexée sous `…/DSD/Artiste/Album`, l'autre racine,
//! malgré « Split albums by quality ».
use super::build_track_from_metadata_opts;
use std::sync::Arc;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::{DbBackend, ToSqlValue};
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_repo::TrackRepo;
use tune_core::metadata::TrackMetadata;
use tune_core::scanner::walker::ScannedFile;

const ARTISTE: &str = "Art Blakey & The Afro-Drum Ensemble";
const ALBUM: &str = "The African Beat";

/// Les sept pistes d'une édition sous `racine/Art Blakey/The African Beat`.
fn edition(racine: &str, ext: &str, cadence: u32, resolution: u16) -> Vec<ScannedFile> {
    (1..=7)
        .map(|n| {
            let chemin = format!("{racine}/Art Blakey/{ALBUM}/0{n}.{ext}");
            ScannedFile {
                path: chemin.clone(),
                metadata: Some(TrackMetadata {
                    title: Some(format!("Titre {n}")),
                    artist: Some(ARTISTE.to_string()),
                    album: Some(ALBUM.to_string()),
                    album_artist: Some(ARTISTE.to_string()),
                    track_number: Some(n),
                    disc_number: Some(1),
                    year: Some(1962),
                    format: Some(ext.to_string()),
                    sample_rate: Some(cadence),
                    bit_depth: Some(resolution),
                    ..Default::default()
                }),
                unsupported: None,
                audio_hash: Some(format!("hash-{chemin}")),
                file_size: 4096,
                mtime: 1_700_000_000.0,
            }
        })
        .collect()
}

/// Base en mémoire avec `racines` comme dossiers de musique.
fn base(racines: &[&str]) -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let json = serde_json::to_string(racines).unwrap();
    let params: [&dyn ToSqlValue; 1] = [&json];
    backend
        .execute(
            "INSERT INTO settings (key, value, updated_at) VALUES ('music_dirs', ?, '') \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            &params,
        )
        .unwrap();
    backend
}

/// Range chaque fichier par la voie du surveillant, piste écrite en base
/// aussitôt (comme `reimporter_fichier_surveillant`). Rend les albums vus.
fn surveiller(db: &Arc<dyn DbBackend>, fichiers: &[ScannedFile], quality_split: bool) -> Vec<i64> {
    let artistes = ArtistRepo::with_backend(db.clone());
    let albums = AlbumRepo::with_backend(db.clone());
    let pistes = TrackRepo::with_backend(db.clone());
    let mut ids = Vec::new();
    for f in fichiers {
        let (piste, album_id) =
            build_track_from_metadata_opts(f, &artistes, &albums, quality_split, None, None)
                .expect("une piste");
        pistes.create(&piste).unwrap();
        ids.push(album_id.expect("un album"));
    }
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// TÉMOIN #7694 — réglage actif : l'édition DSD256 indexée, puis l'édition CD
/// vue par le surveillant au même chemin sous l'autre racine, font deux albums.
#[test]
fn surveillant_edition_cd_sous_l_autre_racine_reste_un_album_a_part_7694() {
    let db = base(&["/m/DSD", "/m/CD"]);
    let dsd = surveiller(&db, &edition("/m/DSD", "dsf", 11_289_600, 1), true);
    let cd = surveiller(&db, &edition("/m/CD", "flac", 44_100, 16), true);
    assert_eq!(dsd.len(), 1, "l'édition DSD forme un album : {dsd:?}");
    assert_eq!(cd.len(), 1, "l'édition CD forme un album : {cd:?}");
    assert_ne!(
        dsd, cd,
        "le surveillant a réuni l'édition CD à l'album DSD de l'autre racine \
         malgré « Split albums by quality » (règle des dossiers miroirs sans qualité)"
    );
}

/// La règle #4907 reste entière pour une vraie copie : la MÊME qualité
/// recopiée sous l'autre racine rejoint l'album déjà indexé.
#[test]
fn surveillant_copie_de_meme_qualite_rejoint_son_miroir_4907() {
    let db = base(&["/m/NAS", "/m/Local"]);
    let nas = surveiller(&db, &edition("/m/NAS", "flac", 44_100, 16), true);
    let local = surveiller(&db, &edition("/m/Local", "flac", 44_100, 16), true);
    assert_eq!(nas, local, "une copie de même qualité est la même parution");
}

/// Réglage inactif : comme avant, une seule fiche pour les deux éditions.
#[test]
fn surveillant_reglage_inactif_un_seul_album_7694() {
    let db = base(&["/m/DSD", "/m/CD"]);
    let dsd = surveiller(&db, &edition("/m/DSD", "dsf", 11_289_600, 1), false);
    let cd = surveiller(&db, &edition("/m/CD", "flac", 44_100, 16), false);
    assert_eq!(dsd, cd);
}
