//! [rc14] Le scan lit le code-barres de la balise et remplit `albums.barcode`
//! s'il est VIDE. Idées signalées par Xavier Joly (testeur).
//!
//! Trajet de production : de VRAIS FLAC copiés de la fixture, relus par
//! `read_metadata`, puis le vrai `begin_batch` / `import` sur une base neuve.
use super::*;
use std::sync::Arc;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::sqlite::SqliteDb;

/// Un FLAC réel du dossier `dossier`, piste `n`, aux commentaires Vorbis
/// donnés en plus du titre, de l'album et des artistes.
fn flac(racine: &std::path::Path, dossier: &str, n: u32, extra: &[(&str, &str)]) -> ScannedFile {
    use lofty::config::WriteOptions;
    use lofty::tag::TagExt;
    let d = racine.join(dossier);
    std::fs::create_dir_all(&d).unwrap();
    let chemin = d.join(format!("0{n}.flac"));
    let fixture = format!(
        "{}/../tune-core/tests/fixtures/test.flac",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::copy(&fixture, &chemin).unwrap();
    let mut vc = lofty::ogg::VorbisComments::new();
    let titre = format!("{dossier} {n}");
    let numero = n.to_string();
    for (k, v) in [
        ("TITLE", titre.as_str()),
        ("ALBUM", dossier),
        ("ARTIST", "Anne Gastinel"),
        ("ALBUMARTIST", "Anne Gastinel"),
        ("TRACKNUMBER", numero.as_str()),
    ]
    .iter()
    .chain(extra.iter())
    {
        vc.push((*k).into(), (*v).into());
    }
    vc.save_to_path(&chemin, WriteOptions::default()).unwrap();
    let meta = tune_core::metadata::read_metadata(&chemin).expect("FLAC lisible");
    ScannedFile {
        path: chemin.to_string_lossy().into_owned(),
        metadata: Some(meta),
        unsupported: None,
        audio_hash: Some(format!("hash-{dossier}-{n}")),
        file_size: 4096,
        mtime: 1_700_000_000.0,
    }
}

fn base() -> (Arc<dyn tune_core::db::backend::DbBackend>, AlbumRepo) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    let backend: Arc<dyn tune_core::db::backend::DbBackend> = Arc::new(db);
    let albums = AlbumRepo::with_backend(backend.clone());
    (backend, albums)
}

fn importer(
    backend: &Arc<dyn tune_core::db::backend::DbBackend>,
    cache: &std::path::Path,
    lot: &[ScannedFile],
) -> Vec<i64> {
    let mut imp = TrackImporter::new(
        backend.clone(),
        true,
        cache.to_path_buf(),
        PorteeDuScan::TOUT,
    );
    imp.begin_batch(lot);
    lot.iter()
        .map(|f| imp.import(f).expect("import").1.expect("un album"))
        .collect()
}

#[test]
fn le_code_barres_de_la_balise_remplit_la_colonne_vide() {
    let tmp = tempfile::tempdir().unwrap();
    let (backend, albums) = base();
    let lot = vec![
        // La 1re piste n'en porte pas : la 2e remplit quand même l'album.
        flac(tmp.path(), "Bach Suites", 1, &[]),
        flac(
            tmp.path(),
            "Bach Suites",
            2,
            &[("BARCODE", "0 028947-75841 9")],
        ),
        flac(tmp.path(), "Variantes UPC", 1, &[("UPC", "602547924865")]),
        flac(tmp.path(), "Variantes EAN", 1, &[("EAN", "40085423")]),
        flac(tmp.path(), "Sans balise", 1, &[]),
    ];
    let ids = importer(&backend, tmp.path(), &lot);
    let code = |id: i64| albums.get(id).unwrap().unwrap().barcode;
    assert_eq!(code(ids[0]).as_deref(), Some("0028947758419"));
    assert_eq!(ids[0], ids[1], "les deux pistes forment un album");
    assert_eq!(code(ids[2]).as_deref(), Some("602547924865"));
    assert_eq!(code(ids[3]).as_deref(), Some("40085423"));
    assert_eq!(code(ids[4]), None, "sans balise, rien n'est inventé");
}

#[test]
fn une_valeur_deja_connue_n_est_jamais_ecrasee() {
    let tmp = tempfile::tempdir().unwrap();
    let (backend, albums) = base();
    let lot = vec![flac(
        tmp.path(),
        "Bach Suites",
        1,
        &[("BARCODE", "0028947758419")],
    )];
    let ids = importer(&backend, tmp.path(), &lot);
    // Une valeur posée ailleurs (MusicBrainz, édition) : le scan suivant,
    // même avec un AUTRE code dans la balise, la garde.
    albums
        .force_update_barcode(ids[0], "5099902987620")
        .unwrap();
    let relu = vec![flac(
        tmp.path(),
        "Bach Suites",
        1,
        &[("BARCODE", "0028947758419")],
    )];
    importer(&backend, tmp.path(), &relu);
    assert_eq!(
        albums.get(ids[0]).unwrap().unwrap().barcode.as_deref(),
        Some("5099902987620")
    );
}

#[test]
fn une_valeur_invalide_est_ignoree() {
    let tmp = tempfile::tempdir().unwrap();
    let (backend, albums) = base();
    let lot = vec![
        flac(tmp.path(), "Lettres", 1, &[("BARCODE", "ABC123456789")]),
        flac(tmp.path(), "Trop court", 1, &[("BARCODE", "1234567")]),
        flac(
            tmp.path(),
            "Onze chiffres",
            1,
            &[("BARCODE", "12345678901")],
        ),
        flac(
            tmp.path(),
            "Quinze chiffres",
            1,
            &[("BARCODE", "123456789012345")],
        ),
    ];
    let ids = importer(&backend, tmp.path(), &lot);
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(
            albums.get(*id).unwrap().unwrap().barcode,
            None,
            "fichier {i} : un code invalide ne doit pas atteindre la base"
        );
    }
}
