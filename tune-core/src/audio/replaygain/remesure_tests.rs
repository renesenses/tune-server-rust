//! Témoins de la remesure des mesures d'avant #5882.

use super::*;
use crate::db::migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::track_metadata_repo::TrackMetadataRepo;
use std::collections::HashMap;

/// Une base, un album de cinq pistes dont les fichiers existent (sauf la 5),
/// et une sixième piste hors album dans une racine à part.
fn base(dir: &std::path::Path) -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    db.execute("INSERT INTO artists (id, name) VALUES (1, 'Bjork')", &[])
        .unwrap();
    db.execute(
        "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Homogenic', 1)",
        &[],
    )
    .unwrap();
    for id in 1..=6 {
        let chemin = if id == 6 {
            dir.join("exclue").join("6.flac")
        } else {
            dir.join(format!("{id}.flac"))
        };
        if id != 5 {
            std::fs::create_dir_all(chemin.parent().unwrap()).unwrap();
            std::fs::write(&chemin, b"x").unwrap();
        }
        let album = if id == 6 { "NULL" } else { "1" };
        db.execute(
            &format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, \
                 sample_rate, channels) VALUES ({id}, 'Piste {id}', {album}, 1, '{}', 300000, \
                 44100, 2)",
                chemin.display()
            ),
            &[],
        )
        .unwrap();
    }
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let repo = TrackMetadataRepo::with_backend(backend.clone());
    // 1 : mesurée avant #5594 (sans version). 2 : `v1`. 3 : version courante.
    // 4 : gain lu dans les tags. 5 : mesurée, fichier absent. 6 : mesurée,
    // sans version, dans la racine qui sera exclue.
    for id in [1, 2, 3, 5, 6] {
        mesure_de_tune(&repo, id);
    }
    repo.set(2, RG_ALGO_KEY, "bs1770-tp4x-v1").unwrap();
    repo.set(3, RG_ALGO_KEY, RG_ALGO).unwrap();
    repo.set(4, "rg_track_gain", "-3.00 dB").unwrap();
    repo.set(4, "rg_track_peak", "0.900000").unwrap();
    // L'album : de Tune sur 1, 2, 3 et 5, des tags sur 4.
    for id in [1, 2, 3, 5] {
        repo.set(id, "rg_album_gain", "-7.00 dB").unwrap();
        repo.set(id, "rg_album_peak", "0.950000").unwrap();
        repo.set(id, "rg_album_true_peak", "1.010000").unwrap();
        repo.set(id, "rg_album_source", SOURCE_ANALYSIS).unwrap();
    }
    repo.set(4, "rg_album_gain", "-4.00 dB").unwrap();
    backend
}

fn mesure_de_tune(repo: &TrackMetadataRepo, id: i64) {
    repo.set(id, "rg_track_gain", "-6.50 dB").unwrap();
    repo.set(id, "rg_track_peak", "0.455000").unwrap();
    repo.set(id, "rg_track_true_peak", "0.507412").unwrap();
    repo.set(
        id,
        super::super::TRUE_PEAK_ALGO_KEY,
        super::super::TRUE_PEAK_ALGO,
    )
    .unwrap();
    repo.set(id, TRACK_SOURCE_KEY, SOURCE_ANALYSIS).unwrap();
    repo.set(id, "rg_analyzed", "1700000000").unwrap();
    repo.set(id, "dr_track", "10").unwrap();
    repo.set(id, "dr_source", "analysis").unwrap();
}

fn meta(backend: &Arc<dyn DbBackend>, id: i64) -> HashMap<String, String> {
    TrackMetadataRepo::with_backend(backend.clone())
        .get_all(id)
        .unwrap()
}

/// Ce qui est rendu à la passe, et ce qui ne l'est pas.
#[test]
fn le_lot_rend_a_la_passe_les_seules_mesures_d_avant_le_correctif() {
    let tmp = tempfile::TempDir::new().unwrap();
    let backend = base(tmp.path());
    // 1, 2, 5, 6 : sans la version courante. 3 l'a, 4 vient des tags.
    assert_eq!(compter_les_mesures_perimees(&backend), Some(4));
    let avant = super::super::compter_les_candidats_replaygain(&backend);

    let lot = remettre_un_lot(&backend, 0, LOT).unwrap();
    assert_eq!(
        lot,
        Lot {
            examinees: 4,
            remises: 3,
            curseur: 6
        }
    );

    for id in [1, 2, 6] {
        let m = meta(&backend, id);
        for cle in [
            "rg_track_gain",
            "rg_track_peak",
            "rg_track_true_peak",
            TRACK_SOURCE_KEY,
            RG_ALGO_KEY,
            super::super::TRUE_PEAK_ALGO_KEY,
            "rg_analyzed",
        ] {
            assert!(!m.contains_key(cle), "piste {id} garde {cle} : {m:?}");
        }
        // La plage dynamique n'est pas l'objet de la remesure.
        assert_eq!(m.get("dr_track").map(String::as_str), Some("10"), "{m:?}");
    }
    // Version courante : intacte.
    assert_eq!(
        meta(&backend, 3)
            .get("rg_track_true_peak")
            .map(String::as_str),
        Some("0.507412")
    );
    // Tags du fichier : intacts, gain d'album compris.
    let m4 = meta(&backend, 4);
    assert_eq!(
        m4.get("rg_track_gain").map(String::as_str),
        Some("-3.00 dB")
    );
    assert_eq!(
        m4.get("rg_album_gain").map(String::as_str),
        Some("-4.00 dB")
    );
    // Fichier absent : la mesure reste, la passe ne pourrait pas la refaire.
    assert!(meta(&backend, 5).contains_key("rg_track_gain"));
    // L'album de Tune est effacé sur TOUTES ses pistes, 3 et 5 comprises :
    // il se recalculera sur les nouveaux gains de piste.
    for id in [1, 2, 3, 5] {
        let m = meta(&backend, id);
        assert!(!m.contains_key("rg_album_gain"), "piste {id} : {m:?}");
        assert!(!m.contains_key("rg_album_source"), "piste {id} : {m:?}");
    }

    // Les pistes rendues redeviennent candidates de la passe nominale.
    assert_eq!(
        super::super::compter_les_candidats_replaygain(&backend),
        avant + 3
    );
    // Le curseur : 5 n'est pas réexaminée, le tour est fini.
    let suivant = remettre_un_lot(&backend, lot.curseur, LOT).unwrap();
    assert_eq!(suivant.examinees, 0);
}

/// Le lot est borné, et le curseur fait avancer la campagne.
#[test]
fn le_lot_est_borne_et_le_curseur_avance() {
    let tmp = tempfile::TempDir::new().unwrap();
    let backend = base(tmp.path());
    let l1 = remettre_un_lot(&backend, 0, 2).unwrap();
    assert_eq!((l1.examinees, l1.remises, l1.curseur), (2, 2, 2));
    let l2 = remettre_un_lot(&backend, l1.curseur, 2).unwrap();
    // 5 (absente) puis 6.
    assert_eq!((l2.examinees, l2.remises, l2.curseur), (2, 1, 6));
}

/// Une racine exclue des analyses (#5593) garde ses mesures : la passe ne les
/// referait pas.
#[test]
fn la_racine_exclue_garde_ses_mesures() {
    let tmp = tempfile::TempDir::new().unwrap();
    let backend = base(tmp.path());
    let exclue = tmp.path().join("exclue");
    SettingsRepo::with_backend(backend.clone())
        .set(
            crate::taches_de_fond::perimetre::CLE_RACINES_EXCLUES,
            &serde_json::json!([exclue.display().to_string()]).to_string(),
        )
        .unwrap();
    assert_eq!(compter_les_mesures_perimees(&backend), Some(3));
    remettre_un_lot(&backend, 0, LOT).unwrap();
    assert!(meta(&backend, 6).contains_key("rg_track_gain"));
}

/// La campagne n'efface rien tant que la passe ne pourrait pas suivre.
#[test]
fn la_campagne_attend_la_passe() {
    assert!(doit_attendre(false, false, Some(0)), "analyse coupée");
    assert!(doit_attendre(true, true, Some(0)), "passe en pause");
    assert!(doit_attendre(true, false, None), "comptage en échec");
    assert!(doit_attendre(true, false, Some(SEUIL_DE_RELANCE + 1)));
    assert!(!doit_attendre(true, false, Some(SEUIL_DE_RELANCE)));
    assert!(!doit_attendre(true, false, Some(0)));
}

/// La version de la mesure ReplayGain n'est plus `v1` : une `v1` a pu être
/// écrite sans #5882.
#[test]
fn la_version_courante_n_est_pas_v1() {
    assert_ne!(RG_ALGO, "bs1770-tp4x-v1");
}
