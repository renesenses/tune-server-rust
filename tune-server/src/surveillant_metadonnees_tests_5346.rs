//! Relecture réelle des balises étendues par le surveillant (#5346).
use super::reimporter_fichier_surveillant;
use super::surveillant_retouche_tests_4896::{baliser, coffret_indexe, flac_8_canaux};
use std::sync::Arc;
use tune_core::db::backend::DbBackend;
use tune_core::db::track_metadata_repo::TrackMetadataRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::scanner::watcher::{ChangeType, FileChange};

fn eprouver(genre: ChangeType, neuve: bool) {
    let dossier = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        "metadonnees-5346",
    );
    let db =
        tune_core::db::sqlite::SqliteDb::open(&dossier.join("tune.db").to_string_lossy()).unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let (_racine, pistes) = coffret_indexe(&db, "5346");
    let piste = if neuve {
        let p = pistes[0].with_file_name("03-nouvelle.flac");
        std::fs::write(&p, flac_8_canaux()).unwrap();
        p
    } else {
        pistes[0].clone()
    };
    let chemin = piste.to_string_lossy().into_owned();
    let tracks = TrackRepo::with_backend(db.clone());
    let meta = TrackMetadataRepo::with_backend(db.clone());
    let avant = tracks.get_by_path(&chemin).unwrap().and_then(|t| t.id);
    if let Some(id) = avant {
        meta.set_batch(
            id,
            &std::collections::HashMap::from([
                ("rg_track_gain".into(), "-7.1 dB".into()),
                ("composer".into(), "Ancien compositeur".into()),
                ("dr_track".into(), "12".into()),
            ]),
        )
        .unwrap();
    }
    baliser(
        &piste,
        &[
            ("TITLE", "Titre retouché"),
            ("ARTIST", "Pink Floyd"),
            ("REPLAYGAIN_TRACK_GAIN", "-3.5 dB"),
            ("REPLAYGAIN_TRACK_PEAK", "0.95"),
            ("COMPOSER", "Nouveau compositeur"),
        ],
        std::time::SystemTime::now(),
    );
    let change = FileChange {
        change_type: genre,
        path: chemin.clone(),
    };
    reimporter_fichier_surveillant(&db, &change, true);
    let track = tracks.get_by_path(&chemin).unwrap().unwrap();
    assert_eq!(
        track.title, "Titre retouché",
        "le surveillant a relu le fichier"
    );
    let id = track.id.unwrap();
    if let Some(avant) = avant {
        assert_eq!(id, avant, "identifiant conservé #5341");
    }
    let valeurs = meta.get_all(id).unwrap();
    assert_eq!(
        valeurs.get("rg_track_gain").map(String::as_str),
        Some("-3.5 dB"),
        "#5346 : le ReplayGain retouché doit arriver en base sans scan complet"
    );
    assert_eq!(
        valeurs.get("rg_track_peak").map(String::as_str),
        Some("0.95")
    );
    assert_eq!(
        valeurs.get("composer").map(String::as_str),
        Some("Nouveau compositeur")
    );
    if avant.is_some() {
        assert_eq!(
            valeurs.get("dr_track").map(String::as_str),
            Some("12"),
            "une mesure absente des balises reste conservée comme au scan"
        );
    }
    // Le même événement sans modification ne doit pas relire les balises
    // et écraser une mesure calculée entre-temps par Tune.
    meta.set_batch(
        id,
        &std::collections::HashMap::from([("rg_track_gain".into(), "-5.0 dB".into())]),
    )
    .unwrap();
    reimporter_fichier_surveillant(&db, &change, true);
    assert_eq!(
        meta.get_all(id)
            .unwrap()
            .get("rg_track_gain")
            .map(String::as_str),
        Some("-5.0 dB"),
        "le fichier inchangé ne doit pas être relu"
    );
}

#[test]
fn retouche_relit_metadonnees_etendues_5346() {
    eprouver(ChangeType::Modified, false);
}

#[test]
fn remplacement_relit_metadonnees_etendues_5346() {
    eprouver(ChangeType::Added, false);
}

#[test]
fn ajout_lit_metadonnees_etendues_5346() {
    eprouver(ChangeType::Added, true);
}
