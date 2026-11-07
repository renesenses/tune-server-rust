//! #7694 (fil 2564) — « The African Beat » (Art Blakey & The Afro-Drum
//! Ensemble, 1962) existe en DSD et en CD, chaque édition dans son dossier,
//! sous deux racines de musique : `…/DSD/Art Blakey/The African Beat` et
//! `…/CD/Art Blakey/The African Beat`. Avec « Split albums by quality » actif,
//! les deux éditions sortaient en UN album, sous une seule pochette.
//!
//! Cause : la règle des dossiers miroirs (#4907) tient le même chemin relatif
//! sous une autre racine pour une copie de sauvegarde, donc pour le même
//! album, sans regarder la qualité. Elle ne rattache plus au miroir qu'un
//! fichier de même qualité.
//!
//! Ces témoins jouent le trajet de PRODUCTION : le vrai découpage en lots, le
//! vrai `begin_batch` / `import`, les pistes écrites en base à la fin de chaque
//! lot, comme le fait le scan.
use super::*;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_repo::TrackRepo;

const ARTISTE: &str = "Art Blakey & The Afro-Drum Ensemble";
const ALBUM: &str = "The African Beat";
const POCHETTE_DSD: &[u8] = b"POCHETTE-EDITION-DSD";
const POCHETTE_CD: &[u8] = b"POCHETTE-EDITION-CD";

/// Une qualité de fichier : `(extension, format, cadence, résolution)`.
type Qualite = (&'static str, &'static str, u32, u16);
const DSD64: Qualite = ("dsf", "dsf", 2_822_400, 1);
const CD: Qualite = ("flac", "flac", 44_100, 16);

/// Les sept pistes d'une édition, rangées sous `racine/Art Blakey/The
/// African Beat`, avec l'image `cover.jpg` du dossier.
fn edition(racine: &std::path::Path, qualite: Qualite, pochette: &[u8]) -> Vec<ScannedFile> {
    let (ext, format, cadence, resolution) = qualite;
    let dossier = racine.join("Art Blakey").join(ALBUM);
    std::fs::create_dir_all(&dossier).unwrap();
    std::fs::write(dossier.join("cover.jpg"), pochette).unwrap();
    (1..=7)
        .map(|n| {
            let chemin = dossier.join(format!("0{n}.{ext}"));
            ScannedFile {
                path: chemin.to_string_lossy().into_owned(),
                metadata: Some(TrackMetadata {
                    title: Some(format!("Titre {n}")),
                    artist: Some(ARTISTE.to_string()),
                    album: Some(ALBUM.to_string()),
                    album_artist: Some(ARTISTE.to_string()),
                    track_number: Some(n),
                    disc_number: Some(1),
                    year: Some(1962),
                    format: Some(format.to_string()),
                    sample_rate: Some(cadence),
                    bit_depth: Some(resolution),
                    ..Default::default()
                }),
                unsupported: None,
                audio_hash: Some(format!("hash-{}", chemin.display())),
                file_size: 4096,
                mtime: 1_700_000_000.0,
            }
        })
        .collect()
}

/// Ce que l'utilisateur voit d'un album : son identifiant et sa pochette.
#[derive(Debug, PartialEq)]
struct Vu {
    album_id: i64,
    pochette: Option<String>,
}

/// Scan de `fichiers` avec les racines `racines` déclarées, par lots de
/// `taille_de_lot` fichiers. Rend, pour chaque fichier, l'album où il est
/// rangé.
fn scanner(
    racines: &[&std::path::Path],
    fichiers: &[ScannedFile],
    taille_de_lot: usize,
    cache: &std::path::Path,
    quality_split: bool,
) -> std::collections::BTreeMap<String, Vu> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let racines: Vec<String> = racines
        .iter()
        .map(|r| r.to_string_lossy().into_owned())
        .collect();
    let json = serde_json::to_string(&racines).unwrap();
    let params: [&dyn ToSqlValue; 1] = [&json];
    backend
        .execute(
            "INSERT INTO settings (key, value, updated_at) VALUES ('music_dirs', ?, '') \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            &params,
        )
        .unwrap();
    let albums = AlbumRepo::with_backend(backend.clone());
    let pistes = TrackRepo::with_backend(backend.clone());
    let mut imp = TrackImporter::new(
        backend.clone(),
        quality_split,
        cache.to_path_buf(),
        PorteeDuScan::TOUT,
    );

    let chemins: Vec<std::path::PathBuf> = fichiers.iter().map(|f| f.path.clone().into()).collect();
    let par_chemin: std::collections::HashMap<&str, &ScannedFile> =
        fichiers.iter().map(|f| (f.path.as_str(), f)).collect();
    let mut rangement = Vec::new();
    for lot in tune_core::scanner::walker::lots_alignes_sur_les_dossiers(&chemins, taille_de_lot) {
        let lot: Vec<ScannedFile> = lot
            .iter()
            .map(|p| (*par_chemin[p.to_str().unwrap()]).clone())
            .collect();
        imp.begin_batch(&lot);
        let mut lignes = Vec::new();
        for f in &lot {
            let (piste, album_id) = imp.import(f).expect("import");
            rangement.push((f.path.clone(), album_id.expect("un album")));
            lignes.push(piste);
        }
        // Comme le scan : les pistes du lot sont écrites à sa fin.
        assert_eq!(pistes.create_batch(&lignes).unwrap(), lignes.len());
    }
    rangement
        .into_iter()
        .map(|(chemin, album_id)| {
            let pochette = albums.get(album_id).unwrap().unwrap().cover_path;
            (chemin, Vu { album_id, pochette })
        })
        .collect()
}

/// L'album unique de chaque édition, et ce qu'il montre.
fn album_de(rendu: &std::collections::BTreeMap<String, Vu>, edition: &[ScannedFile]) -> Vu {
    let vus: Vec<&Vu> = edition.iter().map(|f| &rendu[&f.path]).collect();
    assert!(
        vus.windows(2).all(|w| w[0] == w[1]),
        "toutes les pistes d'une édition sont dans un même album : {vus:?}"
    );
    Vu {
        album_id: vus[0].album_id,
        pochette: vus[0].pochette.clone(),
    }
}

/// TÉMOIN #7694 — réglage actif, l'édition DSD et l'édition CD rangées sous
/// le même chemin de deux racines restent deux albums, chacun avec la
/// pochette de son dossier. Joué deux fois : les deux éditions dans un même
/// lot (rien encore en base quand la seconde est rangée), puis un lot par
/// édition (la première est en base).
#[test]
fn les_editions_dsd_et_cd_sous_deux_racines_restent_deux_albums_7694() {
    for taille_de_lot in [500, 7] {
        let tmp = tempfile::tempdir().unwrap();
        let (r_dsd, r_cd) = (tmp.path().join("DSD"), tmp.path().join("CD"));
        let dsd = edition(&r_dsd, DSD64, POCHETTE_DSD);
        let cd = edition(&r_cd, CD, POCHETTE_CD);
        let tous: Vec<ScannedFile> = dsd.iter().chain(&cd).cloned().collect();

        let rendu = scanner(
            &[&r_dsd, &r_cd],
            &tous,
            taille_de_lot,
            &tmp.path().join("cache"),
            true,
        );
        let (a_dsd, a_cd) = (album_de(&rendu, &dsd), album_de(&rendu, &cd));

        assert_ne!(
            a_dsd.album_id, a_cd.album_id,
            "lots de {taille_de_lot} : l'édition DSD et l'édition CD ont été réunies en un \
             seul album malgré « Split albums by quality » (règle des dossiers miroirs)"
        );
        assert_eq!(
            a_dsd.pochette.as_deref(),
            Some(tune_core::library::artwork::content_hash(POCHETTE_DSD).as_str()),
            "lots de {taille_de_lot} : l'album DSD garde la pochette de son dossier"
        );
        assert_eq!(
            a_cd.pochette.as_deref(),
            Some(tune_core::library::artwork::content_hash(POCHETTE_CD).as_str()),
            "lots de {taille_de_lot} : l'album CD garde la pochette de son dossier"
        );
    }
}

/// Réglage INACTIF : rien ne change, les deux éditions forment un seul album,
/// comme avant le correctif (la clé d'album n'a pas de dossier).
#[test]
fn reglage_inactif_les_editions_dsd_et_cd_restent_un_seul_album_7694() {
    for taille_de_lot in [500, 7] {
        let tmp = tempfile::tempdir().unwrap();
        let (r_dsd, r_cd) = (tmp.path().join("DSD"), tmp.path().join("CD"));
        let dsd = edition(&r_dsd, DSD64, POCHETTE_DSD);
        let cd = edition(&r_cd, CD, POCHETTE_CD);
        let tous: Vec<ScannedFile> = dsd.iter().chain(&cd).cloned().collect();

        let rendu = scanner(
            &[&r_dsd, &r_cd],
            &tous,
            taille_de_lot,
            &tmp.path().join("cache"),
            false,
        );
        assert_eq!(
            album_de(&rendu, &dsd).album_id,
            album_de(&rendu, &cd).album_id,
            "lots de {taille_de_lot} : réglage inactif, les deux éditions restent un seul album"
        );
    }
}

/// Non-régression #4907 — réglage actif, une COPIE de même qualité sous une
/// autre racine (sauvegarde) reste le même album.
#[test]
fn une_copie_de_meme_qualite_sous_une_autre_racine_reste_le_meme_album_4907() {
    for taille_de_lot in [500, 7] {
        let tmp = tempfile::tempdir().unwrap();
        let (r_nas, r_sauvegarde) = (tmp.path().join("NAS"), tmp.path().join("Sauvegarde"));
        let nas = edition(&r_nas, CD, POCHETTE_CD);
        let sauvegarde = edition(&r_sauvegarde, CD, POCHETTE_CD);
        let tous: Vec<ScannedFile> = nas.iter().chain(&sauvegarde).cloned().collect();

        let rendu = scanner(
            &[&r_nas, &r_sauvegarde],
            &tous,
            taille_de_lot,
            &tmp.path().join("cache"),
            true,
        );
        assert_eq!(
            album_de(&rendu, &nas).album_id,
            album_de(&rendu, &sauvegarde).album_id,
            "lots de {taille_de_lot} : la sauvegarde de même qualité reste le même album"
        );
    }
}
