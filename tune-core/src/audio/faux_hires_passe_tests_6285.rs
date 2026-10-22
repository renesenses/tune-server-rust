//! #6285 — la passe de fond : candidats, périmètre, bridage thermique, et un
//! lot de bout en bout sur de VRAIS fichiers WAV écrits par le test à partir
//! de signaux synthétiques. Aucun son joué.

use std::sync::Arc;

use super::*;
use crate::audio::faux_hires::tests::{quantifier, rose};
use crate::audio::thermal::{LectureThermique, MotifIllisible};
use crate::db::backend::DbBackend;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    Arc::new(db)
}

fn piste(db: &Arc<dyn DbBackend>, id: i64, chemin: &str, format: &str, source: &str) {
    let chemin = chemin.to_string();
    let format = format.to_string();
    let source = source.to_string();
    db.execute(
        "INSERT INTO tracks (id, title, file_path, format, source, duration_ms) \
         VALUES (?1, 't', ?2, ?3, ?4, 60000)",
        &[&id as &dyn ToSqlValue, &chemin, &format, &source],
    )
    .unwrap();
}

/// Un WAV PCM mono 24 bits.
fn wav_24(chemin: &std::path::Path, sample_rate: u32, echantillons: &[i32]) {
    let data: Vec<u8> = echantillons
        .iter()
        .flat_map(|s| {
            let b = s.to_le_bytes();
            [b[0], b[1], b[2]]
        })
        .collect();
    let mut f = Vec::new();
    f.extend_from_slice(b"RIFF");
    f.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    f.extend_from_slice(b"WAVEfmt ");
    f.extend_from_slice(&16u32.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&sample_rate.to_le_bytes());
    f.extend_from_slice(&(sample_rate * 3).to_le_bytes());
    f.extend_from_slice(&3u16.to_le_bytes());
    f.extend_from_slice(&24u16.to_le_bytes());
    f.extend_from_slice(b"data");
    f.extend_from_slice(&(data.len() as u32).to_le_bytes());
    f.extend_from_slice(&data);
    std::fs::write(chemin, f).unwrap();
}

/// 🔴 #6450 (décision de Bertrand du 20/10/2026) : sans température lisible,
/// la passe décode UN fichier à la fois, quelle que soit la vitesse réglée.
#[test]
fn sans_temperature_lisible_la_passe_est_bridee_a_un_fichier_6285() {
    let db = base();
    SettingsRepo::with_backend(db.clone())
        .set(vitesse::CLE_REGLAGE, vitesse::Vitesse::Rapide.id())
        .unwrap();
    for motif in [MotifIllisible::AucunCapteur, MotifIllisible::Figee] {
        assert_eq!(
            largeur_de_la_passe(&db, &LectureThermique::Illisible(motif)),
            1,
            "température illisible ({motif:?}) : un fichier à la fois, même en vitesse rapide"
        );
    }
    assert_eq!(
        largeur_de_la_passe(&db, &LectureThermique::Lisible(45.0)),
        vitesse::Vitesse::Rapide.largeur(vitesse::coeurs()),
        "température lisible : la vitesse réglée"
    );
}

/// Les candidats : locaux, sans perte, avec un chemin, pas encore mesurés,
/// dans le périmètre des analyses.
#[test]
fn les_candidats_respectent_le_perimetre_et_la_source_6285() {
    let db = base();
    piste(&db, 1, "/m/a/1.flac", "flac", "local");
    piste(&db, 2, "/m/a/2.mp3", "mp3", "local");
    piste(&db, 3, "/u/a/3.flac", "flac", "upnp");
    piste(&db, 4, "/nas/4.flac", "flac", "local");
    piste(&db, 5, "/m/a/5.wav", "WAV", "local");
    SettingsRepo::with_backend(db.clone())
        .set(
            crate::taches_de_fond::perimetre::CLE_RACINES_EXCLUES,
            "[\"/nas\"]",
        )
        .unwrap();
    let ids: Vec<i64> = prochains_candidats(&db, 10)
        .iter()
        .map(|c| c.track_id)
        .collect();
    assert_eq!(ids, vec![1, 5], "seuls les sans-perte locaux du périmètre");
    assert_eq!(compter_les_candidats(&db), 2);
    enregistrer(&db, 1, &Err("x".into())).unwrap();
    assert_eq!(
        compter_les_candidats(&db),
        1,
        "une piste rangée n'est plus candidate"
    );
}

/// La passe est désactivée par défaut.
#[test]
fn la_passe_est_desactivee_par_defaut_6285() {
    let db = base();
    assert!(!est_active(&db));
    activer(&db, true).unwrap();
    assert!(est_active(&db));
}

/// 🔴 De bout en bout : un vrai 24/96, un CD gonflé en 24/96, et un fichier
/// illisible, rangés par UN lot.
#[tokio::test]
async fn un_lot_range_les_verdicts_des_vrais_fichiers_6285() {
    let db = base();
    let dossier = crate::test_scratch::scratch_dir("faux-hires-6285");
    let vrai = dossier.join("vrai.wav");
    wav_24(&vrai, 96_000, &quantifier(&rose(96_000, 47_000.0, 11), 24));
    let gonfle = dossier.join("gonfle.wav");
    let cd: Vec<i32> = quantifier(&rose(96_000, 20_000.0, 12), 16)
        .into_iter()
        .map(|s| s << 8)
        .collect();
    wav_24(&gonfle, 96_000, &cd);
    let casse = dossier.join("casse.wav");
    std::fs::write(&casse, b"pas un wav").unwrap();
    piste(&db, 1, &vrai.to_string_lossy(), "wav", "local");
    piste(&db, 2, &gonfle.to_string_lossy(), "wav", "local");
    piste(&db, 3, &casse.to_string_lossy(), "wav", "local");

    assert_eq!(un_lot(&db, 3).await, 3);
    let lignes: Vec<(i64, String, Option<String>, Option<i64>)> = db
        .query_many(
            "SELECT track_id, verdict, raison, bits_effectifs FROM hires_verifications ORDER BY track_id",
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| {
            (
                r[0].as_i64().unwrap(),
                r[1].as_string().unwrap(),
                r[2].as_string(),
                r[3].as_i64(),
            )
        })
        .collect();
    assert_eq!(
        lignes,
        vec![
            (1, "verifie".into(), None, Some(24)),
            (
                2,
                "douteux".into(),
                Some("bits_de_remplissage".into()),
                Some(16)
            ),
            (
                3,
                "non_concluant".into(),
                Some("decodage_impossible".into()),
                None
            ),
        ],
        "le vrai 24/96 vérifié, le CD gonflé douteux, l'illisible rangé sans verdict"
    );
    assert_eq!(compter_les_candidats(&db), 0);
}

#[test]
fn le_verdict_d_album_6285() {
    let p = |v: &str, r: Option<&str>| (v.to_string(), r.map(str::to_string));
    assert_eq!(verdict_d_album(&[]), ("non_analyse".into(), None));
    assert_eq!(
        verdict_d_album(&[p("verifie", None), p("verifie", None)]),
        ("verifie".into(), None)
    );
    assert_eq!(
        verdict_d_album(&[
            p("verifie", None),
            p("douteux", Some("bande_de_cd")),
            p("douteux", Some("bande_de_cd")),
            p("douteux", Some("bits_de_remplissage")),
        ]),
        ("douteux".into(), Some("bande_de_cd".into()))
    );
    assert_eq!(
        verdict_d_album(&[p("non_concluant", None)]),
        ("non_analyse".into(), None)
    );
    assert_eq!(
        verdict_d_album(&[p("conforme", None), p("verifie", None)]),
        ("conforme".into(), None)
    );
}
