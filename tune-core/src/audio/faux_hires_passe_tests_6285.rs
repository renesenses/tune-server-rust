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

/// #9035 — l'ancien `coupure_de_codec` (rangé sans signature mesurée) se
/// publie `coupure_spectrale`, et compte avec elle ; le compte des pistes en
/// cause dit « 2 sur 3 » (la non concluante n'est pas mesurée).
#[test]
fn l_album_dit_sa_coupure_et_combien_de_pistes_9035() {
    let p = |v: &str, r: Option<&str>| (v.to_string(), r.map(str::to_string));
    let pistes = [
        p("douteux", Some("coupure_de_codec")),
        p("douteux", Some("coupure_spectrale")),
        p("douteux", Some("bande_de_cd")),
        p("verifie", None),
        p("non_concluant", None),
    ];
    assert_eq!(
        verdict_d_album(&pistes),
        ("douteux".into(), Some("coupure_spectrale".into()))
    );
    assert_eq!(pistes_en_cause(&pistes), (3, 4));
    assert_eq!(pistes_en_cause(&[]), (0, 0));
}

/// #9035 (décision du 18/11, fil 2902) — un album n'est douteux qu'à partir
/// de la MOITIÉ de ses pistes mesurées. Deux bonus douteux sur dix : l'album
/// garde le verdict de ses autres pistes ; six sur dix : douteux.
#[test]
fn un_album_n_est_douteux_qu_a_partir_de_la_moitie_9035() {
    let album = |douteuses: usize, autre: &str| -> Vec<(String, Option<String>)> {
        (0..10)
            .map(|i| {
                if i < douteuses {
                    ("douteux".to_string(), Some("coupure_spectrale".to_string()))
                } else {
                    (autre.to_string(), None)
                }
            })
            .collect()
    };
    assert_eq!(
        verdict_d_album(&album(2, "verifie")),
        ("verifie".into(), None)
    );
    assert_eq!(pistes_en_cause(&album(2, "verifie")), (2, 10));
    assert_eq!(
        verdict_d_album(&album(2, "conforme")),
        ("conforme".into(), None),
        "#9067 (19/11) — source altérée, tous formats : la moitié se compte sur \
         toutes les pistes mesurées, CD conformes compris : 2 sur 10"
    );
    assert_eq!(pistes_en_cause(&album(2, "conforme")), (2, 10));
    assert_eq!(
        verdict_d_album(&album(4, "verifie")),
        ("verifie".into(), None)
    );
    assert_eq!(
        verdict_d_album(&album(5, "verifie")),
        ("douteux".into(), Some("coupure_spectrale".into())),
        "la moitié pile suffit"
    );
    assert_eq!(
        verdict_d_album(&album(6, "verifie")),
        ("douteux".into(), Some("coupure_spectrale".into()))
    );
    assert_eq!(
        raison_des_douteuses(&album(2, "verifie")),
        Some("coupure_spectrale".into())
    );
    assert_eq!(raison_des_douteuses(&album(0, "verifie")), None);
    // Les non concluantes ne comptent pas : 1 douteuse sur 2 mesurées.
    let mut p = album(1, "non_concluant");
    p[1] = ("verifie".into(), None);
    assert_eq!(verdict_d_album(&p).0, "douteux");
}

/// #9035 (décision du 18/11) — REMESURE CIBLÉE : une piste dont la mesure
/// porte l'ancien `coupure_de_codec` redevient candidate, APRÈS les pistes
/// jamais mesurées ; les autres mesures ne sont pas touchées ; une fois
/// remesurée (nouveau code), elle n'est plus candidate.
#[test]
fn l_ancien_code_codec_est_remesure_une_fois_9035() {
    let db = base();
    for id in 1..=4 {
        piste(&db, id, &format!("/m/a/{id}.flac"), "flac", "local");
    }
    for (id, raison) in [
        (1i64, "coupure_de_codec"),
        (2, "coupure_spectrale"),
        (3, "bande_de_cd"),
    ] {
        let raison = raison.to_string();
        db.execute(
            "INSERT INTO hires_verifications (track_id, verdict, raison, coupure_hz, sample_rate, bits_declares) \
             VALUES (?1, 'douteux', ?2, 16000.0, 96000, 24)",
            &[&id as &dyn ToSqlValue, &raison],
        )
        .unwrap();
    }
    let ids: Vec<i64> = prochains_candidats(&db, 10)
        .iter()
        .map(|c| c.track_id)
        .collect();
    assert_eq!(
        ids,
        vec![4, 1],
        "la jamais mesurée d'abord, puis la remesure"
    );
    assert_eq!(compter_les_candidats(&db), 2);
    // La remesure range un nouveau code : la piste sort des candidates.
    // Un 24/44,1 (il se dit hi-res : #9067) coupé à 16 kHz.
    let mesure = Mesure {
        sample_rate: 44_100,
        bits_declares: 24,
        bits_effectifs: Some(24),
        coupure_hz: Some(16_000.0),
        ..Default::default()
    };
    enregistrer(&db, 1, &Ok(mesure.clone())).unwrap();
    enregistrer(&db, 4, &Ok(mesure)).unwrap();
    assert_eq!(compter_les_candidats(&db), 0);
    let r = db
        .query_one(
            "SELECT raison FROM hires_verifications WHERE track_id = 1",
            &[],
        )
        .unwrap()
        .unwrap();
    assert_eq!(r[0].as_string().as_deref(), Some("coupure_spectrale"));
}

/// #9035 — une REMESURE qui ne peut pas lire le fichier (NAS en veille) ne
/// perd pas la mesure : la ligne reste `douteux` avec sa coupure, seule la
/// raison passe de `coupure_de_codec` à `coupure_spectrale` ; jamais
/// `non_concluant`. Elle n'est plus candidate. Une piste jamais mesurée et
/// illisible, elle, reste rangée `non_concluant` comme avant.
#[test]
fn une_remesure_illisible_garde_le_verdict_douteux_9035() {
    let db = base();
    piste(&db, 1, "/m/a/1.flac", "flac", "local");
    piste(&db, 2, "/m/a/2.flac", "flac", "local");
    db.execute(
        "INSERT INTO hires_verifications (track_id, verdict, raison, coupure_hz, bits_effectifs, sample_rate, bits_declares) \
         VALUES (1, 'douteux', 'coupure_de_codec', 16000.0, 24, 96000, 24)",
        &[],
    )
    .unwrap();
    assert_eq!(compter_les_candidats(&db), 2);
    enregistrer(&db, 1, &Err("fichier injoignable".into())).unwrap();
    enregistrer(&db, 2, &Err("fichier injoignable".into())).unwrap();
    let r = db
        .query_one(
            "SELECT verdict, raison, coupure_hz, bits_effectifs FROM hires_verifications WHERE track_id = 1",
            &[],
        )
        .unwrap()
        .unwrap();
    assert_eq!(r[0].as_string().as_deref(), Some("douteux"), "{r:?}");
    assert_eq!(
        r[1].as_string().as_deref(),
        Some("coupure_spectrale"),
        "{r:?}"
    );
    assert_eq!(r[2].as_f64(), Some(16000.0), "{r:?}");
    assert_eq!(r[3].as_i64(), Some(24), "{r:?}");
    let r = db
        .query_one(
            "SELECT verdict, raison FROM hires_verifications WHERE track_id = 2",
            &[],
        )
        .unwrap()
        .unwrap();
    assert_eq!(r[0].as_string().as_deref(), Some("non_concluant"));
    assert_eq!(r[1].as_string().as_deref(), Some("decodage_impossible"));
    assert_eq!(compter_les_candidats(&db), 0);
}

/// #9067 (décision du 19/11, « Source altérée ») — les lignes d'un CD que la
/// nouvelle règle (mur de codec) peut changer sont REMESURÉES une fois : un CD
/// rangé `douteux` sur la seule coupure (avant #9067), un CD rangé `conforme`
/// avec une coupure suspecte sans trace de la règle (du 18 au 19/11). Celles
/// qui portent déjà le jugement du 19/11 (`mur_de_codec`, `coupure_sans_mur`),
/// un CD conforme sans coupure suspecte et un hi-res ne le sont pas.
#[test]
fn les_lignes_d_un_cd_a_rejuger_sont_remesurees_9067() {
    let db = base();
    for id in 1..=8 {
        piste(&db, id, &format!("/m/a/{id}.flac"), "flac", "local");
    }
    let lignes: [(i64, &str, Option<&str>, Option<f64>, i64, i64); 8] = [
        (
            1,
            "douteux",
            Some("coupure_de_codec"),
            Some(16000.0),
            44_100,
            16,
        ),
        (
            2,
            "douteux",
            Some("coupure_spectrale"),
            Some(17000.0),
            48_000,
            16,
        ),
        (3, "conforme", None, Some(16000.0), 44_100, 16),
        (
            4,
            "douteux",
            Some("mur_de_codec"),
            Some(16000.0),
            44_100,
            16,
        ),
        (
            5,
            "conforme",
            Some("coupure_sans_mur"),
            Some(18000.0),
            44_100,
            16,
        ),
        (6, "conforme", None, Some(21000.0), 44_100, 16),
        (7, "conforme", None, None, 44_100, 16),
        (
            8,
            "douteux",
            Some("coupure_spectrale"),
            Some(16000.0),
            96_000,
            24,
        ),
    ];
    for (id, verdict, raison, coupure, rate, bits) in lignes {
        let verdict = verdict.to_string();
        let raison = raison.map(str::to_string);
        db.execute(
            "INSERT INTO hires_verifications (track_id, verdict, raison, coupure_hz, sample_rate, bits_declares) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            &[&id as &dyn ToSqlValue, &verdict, &raison, &coupure, &rate, &bits],
        )
        .unwrap();
    }
    let ids: Vec<i64> = prochains_candidats(&db, 20)
        .iter()
        .map(|c| c.track_id)
        .collect();
    assert_eq!(ids, vec![1, 2, 3], "seules les lignes CD à rejuger");
    assert_eq!(compter_les_candidats(&db), 3);

    // Remesurées : un mur → `mur_de_codec` ; pas de mur → conforme marqué ;
    // illisible → conforme marqué. Aucune ne redevient candidate.
    let cd = |en_mur: bool| Mesure {
        sample_rate: 44_100,
        bits_declares: 16,
        bits_effectifs: Some(16),
        coupure_hz: Some(16_000.0),
        fenetres_jugees: 4,
        fenetres_coupees: vec![
            crate::audio::faux_hires::FenetreCoupee {
                debut_s: 0.0,
                fin_s: 1.0,
                coupure_hz: 16_000.0,
            };
            4
        ],
        coupure_en_mur: en_mur,
        ..Default::default()
    };
    enregistrer(&db, 1, &Ok(cd(true))).unwrap();
    enregistrer(&db, 2, &Ok(cd(false))).unwrap();
    enregistrer(&db, 3, &Err("NAS en veille".into())).unwrap();
    assert_eq!(compter_les_candidats(&db), 0);
    let lire = |id: i64| {
        let r = db
            .query_one(
                &format!("SELECT verdict, raison FROM hires_verifications WHERE track_id = {id}"),
                &[],
            )
            .unwrap()
            .unwrap();
        (r[0].as_string().unwrap(), r[1].as_string())
    };
    assert_eq!(lire(1), ("douteux".into(), Some("mur_de_codec".into())));
    assert_eq!(
        lire(2),
        ("conforme".into(), Some("coupure_sans_mur".into()))
    );
    assert_eq!(
        lire(3),
        ("conforme".into(), Some("coupure_sans_mur".into()))
    );
}
