//! #6285 — la passe de fond : candidats, périmètre, largeur selon la chaleur, et un
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

/// 🔴 #6450 : sans température lisible, la passe garde la vitesse réglée
/// (le bridage à un fichier de #5410 est retiré), et le réglage de #6713
/// n'y change rien.
#[test]
fn sans_temperature_lisible_la_passe_garde_la_vitesse_reglee_6450() {
    let db = base();
    let reglages = SettingsRepo::with_backend(db.clone());
    reglages
        .set(vitesse::CLE_REGLAGE, vitesse::Vitesse::Rapide.id())
        .unwrap();
    let rapide = vitesse::Vitesse::Rapide.largeur(vitesse::coeurs());
    for leve in ["false", "true"] {
        reglages.set(vitesse::CLE_LEVER_LE_BRIDAGE, leve).unwrap();
        for motif in [MotifIllisible::AucunCapteur, MotifIllisible::Figee] {
            assert_eq!(
                largeur_de_la_passe(&db, &LectureThermique::Illisible(motif)),
                rapide,
                "température illisible ({motif:?}, réglage #6713 = {leve}) : \
                 la passe est bridée au lieu de garder la vitesse réglée"
            );
        }
    }
    assert_eq!(
        largeur_de_la_passe(&db, &LectureThermique::Lisible(45.0)),
        rapide,
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

/// 🔴 #7859 — de bout en bout, sur toute la durée : un 24/96 de 72 s dont
/// les 24 premières secondes sont coupées à 20 kHz et le reste monte à
/// 47 kHz. L'ancienne passe ne décodait que 30 s à partir de 30 s, en pleine
/// bande ; les tranches couvrent tout le morceau, et la ligne rangée garde
/// les colonnes de `hires_verifications` (verdict, raison, coupure, bits).
#[tokio::test]
async fn la_passe_mesure_toute_la_duree_et_range_dans_la_meme_table_7859() {
    let db = base();
    let dossier = crate::test_scratch::scratch_dir("faux-hires-7859");
    let pleine = quantifier(&rose(96_000, 47_000.0, 81), 24);
    let coupee = quantifier(&rose(96_000, 20_000.0, 82), 24);
    let total = 72 * 96_000;
    let bascule = 24 * 96_000;
    let pcm: Vec<i32> = (0..total)
        .map(|i| {
            let source = if i < bascule { &coupee } else { &pleine };
            source[i % source.len()]
        })
        .collect();
    let chemin = dossier.join("coupe_au_debut.wav");
    wav_24(&chemin, 96_000, &pcm);
    piste(&db, 1, &chemin.to_string_lossy(), "wav", "local");
    db.execute("UPDATE tracks SET duration_ms = 72000 WHERE id = 1", &[])
        .unwrap();

    let debut = std::time::Instant::now();
    assert_eq!(un_lot(&db, 1).await, 1);
    eprintln!(
        "faux_hires_7859 : 72 s en 24/96 mesurées en {:?}",
        debut.elapsed()
    );
    let r = db
        .query_one(
            "SELECT verdict, raison, coupure_hz, bits_effectifs, sample_rate, bits_declares \
             FROM hires_verifications WHERE track_id = 1",
            &[],
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        r[0].as_string().as_deref(),
        Some("douteux"),
        "les 24 premières secondes sont coupées à 20 kHz : la passe doit les voir — {r:?}"
    );
    assert_eq!(r[1].as_string().as_deref(), Some("bande_de_cd"), "{r:?}");
    let f = r[2].as_f64().unwrap();
    assert!((f - 20_000.0).abs() < 400.0, "coupure rangée {f}");
    assert_eq!(r[3].as_i64(), Some(24));
    assert_eq!(r[4].as_i64(), Some(96_000));
    assert_eq!(r[5].as_i64(), Some(24));

    // Le détail par fenêtres : les fenêtres en cause sont toutes avant 24 s.
    let m = mesurer_un_fichier(&chemin.to_string_lossy(), 72_000).unwrap();
    assert!(!m.fenetres_coupees.is_empty(), "{m:?}");
    assert!(m.fenetres_coupees.iter().all(|c| c.debut_s < 24.0), "{m:?}");
    let tranches = tranches_a_decoder(72_000);
    assert_eq!(tranches.len(), TRANCHES);
    assert!(
        tranches[0].0 < 6.0 && tranches[TRANCHES - 1].0 + TRANCHE_S > 66.0,
        "{tranches:?}"
    );
}

/// #7859 — MESURE, pas un témoin : le temps d'analyse d'un morceau de 5 min
/// en 24/96, avant (un extrait de 30 s, spectre moyen) et après (tranches sur
/// toute la durée, par fenêtres). À lancer à la main :
/// `cargo test -p tune-core --lib -- --ignored temps_d_analyse_7859 --nocapture`.
#[test]
#[ignore]
fn temps_d_analyse_7859() {
    let dossier = crate::test_scratch::scratch_dir("faux-hires-7859-temps");
    let segment = quantifier(&rose(96_000, 47_000.0, 91), 24);
    let pcm: Vec<i32> = (0..300 * 96_000)
        .map(|i| segment[i % segment.len()])
        .collect();
    let chemin = dossier.join("cinq_minutes.wav");
    wav_24(&chemin, 96_000, &pcm);
    let chemin = chemin.to_string_lossy().to_string();

    let debut = std::time::Instant::now();
    let pcm = crate::audio::decode::decode_to_pcm(&chemin, None, None, 30.0, 30.0).unwrap();
    let avant = super::super::faux_hires::coupure_du_spectre_moyen(
        &pcm.samples_i32,
        pcm.channels,
        pcm.sample_rate,
        24,
    );
    let duree_avant = debut.elapsed();

    let debut = std::time::Instant::now();
    let m = mesurer_un_fichier(&chemin, 300_000).unwrap();
    let duree_apres = debut.elapsed();
    eprintln!(
        "faux_hires_7859 temps : avant (30 s, spectre moyen) {duree_avant:?} -> {avant:?} ; \
         après ({} tranches de {} s, {} fenêtres) {duree_apres:?} -> {:?}",
        TRANCHES,
        TRANCHE_S,
        m.fenetres_jugees,
        juger(&m)
    );
}

/// #7859 — une piste courte ou de durée inconnue : une seule tranche, depuis
/// le début.
#[test]
fn une_piste_courte_se_decode_en_une_tranche_7859() {
    assert_eq!(
        tranches_a_decoder(0),
        vec![(0.0, TRANCHES as f64 * TRANCHE_S)]
    );
    assert_eq!(
        tranches_a_decoder(45_000),
        vec![(0.0, TRANCHES as f64 * TRANCHE_S)]
    );
    let longue = tranches_a_decoder(300_000);
    assert_eq!(longue.len(), TRANCHES);
    assert!(longue.iter().all(|&(d, l)| d >= 0.0 && d + l <= 300.0));
    assert!(longue.windows(2).all(|w| w[0].0 < w[1].0));
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
