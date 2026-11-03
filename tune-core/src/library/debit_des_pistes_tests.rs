//! #6332 — le débit moyen d'une piste : stocké, relu, complété au rescan.

use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::db::backend::DbBackend;
use crate::db::models::Track;
use crate::db::track_repo::TrackRepo;

fn base_sqlite() -> Arc<dyn DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn piste(db: &Arc<dyn DbBackend>, chemin: &str, format: &str, debit: Option<i32>) -> i64 {
    let mut t = Track::new(chemin.into());
    t.file_path = Some(chemin.into());
    t.format = Some(format.into());
    t.source = "local".into();
    t.sample_rate = Some(if format == "dsf" { 2_822_400 } else { 44_100 });
    t.channels = 2;
    t.bitrate_kbps = debit;
    TrackRepo::with_backend(db.clone()).create(&t).unwrap()
}

fn debit(db: &Arc<dyn DbBackend>, id: i64) -> Option<i32> {
    TrackRepo::with_backend(db.clone())
        .get(id)
        .unwrap()
        .unwrap()
        .bitrate_kbps
}

/// Le fichier de 44,1 kHz / 16 bits / stéréo : 1 411 kbit/s.
fn ecrire_wav(chemin: &std::path::Path) {
    let octets_data: u32 = 44_100 * 4;
    let mut v: Vec<u8> = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + octets_data).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&44_100u32.to_le_bytes());
    v.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&octets_data.to_le_bytes());
    v.extend(std::iter::repeat_n(0u8, octets_data as usize));
    std::fs::write(chemin, v).unwrap();
}

/// Le débit fait l'aller-retour par la base et sort dans le JSON de la piste
/// et dans `NowPlaying` — il était lu au scan puis jeté (`bitrate_kbps: None`).
#[test]
fn le_debit_fait_l_aller_retour_jusqu_a_lecture_en_cours_6332() {
    let db = base_sqlite();
    let id = piste(&db, "/m/diva.mp3", "mp3", Some(128));
    let t = TrackRepo::with_backend(db.clone())
        .get(id)
        .unwrap()
        .unwrap();
    assert_eq!(
        t.bitrate_kbps,
        Some(128),
        "le débit n'est pas écrit à l'insertion"
    );
    assert_eq!(t.to_json()["bitrate_kbps"], serde_json::json!(128));
    assert_eq!(
        crate::playback::NowPlaying::from_track(&t).bitrate_kbps,
        Some(128),
        "Lecture en cours ne reçoit pas le débit de la piste locale"
    );
    // Une écriture hors scan, ligne reconstruite sans débit : il est gardé.
    let mut sans = t.clone();
    sans.bitrate_kbps = None;
    TrackRepo::with_backend(db.clone()).update(&sans).unwrap();
    assert_eq!(
        debit(&db, id),
        Some(128),
        "une écriture hors scan a effacé le débit"
    );
    // Le « 0 = inconnu » ne s'affiche pas.
    let mut inconnu = t.clone();
    inconnu.bitrate_kbps = Some(0);
    assert_eq!(
        crate::playback::NowPlaying::from_track(&inconnu).bitrate_kbps,
        None
    );
}

/// LE besoin du rescan : les pistes déjà en base, inchangées, reçoivent leur
/// débit sans relecture complète — DSD par le calcul, le reste par une lecture
/// des propriétés ; une piste qui l'a déjà n'est pas touchée ; un flux non
/// local et une piste CUE virtuelle ne sont pas candidats.
#[test]
fn le_rescan_complete_les_pistes_existantes_6332() {
    let db = base_sqlite();
    let flac = piste(&db, "/m/a.flac", "flac", None);
    let dsf = piste(&db, "/m/b.dsf", "dsf", None);
    let deja = piste(&db, "/m/c.mp3", "mp3", Some(320));
    let muet = piste(&db, "/m/d.ogg", "ogg", None);
    let lus = std::sync::Mutex::new(Vec::new());
    let r = completer_avec(
        db.as_ref(),
        100,
        Duration::from_secs(60),
        || true,
        |p| {
            lus.lock().unwrap().push(p.to_string_lossy().to_string());
            Ok(match p.to_str() {
                Some("/m/a.flac") => Some(902),
                _ => None,
            })
        },
    );
    assert_eq!(debit(&db, flac), Some(902));
    assert_eq!(
        debit(&db, dsf),
        Some(5644),
        "DSD64 stéréo : 2 822 400 × 2 / 1000"
    );
    assert_eq!(debit(&db, deja), Some(320), "un débit déjà connu est gardé");
    assert_eq!(
        debit(&db, muet),
        Some(0),
        "illisible : marqué inconnu, pas relu à chaque scan"
    );
    assert_eq!(
        *lus.lock().unwrap(),
        vec!["/m/a.flac".to_string(), "/m/d.ogg".to_string()],
        "seuls les fichiers non DSD sans débit sont relus"
    );
    assert_eq!(
        r,
        RapportDebits {
            dsd_calcules: 1,
            lus: 1,
            inconnus: 1,
            interrompue: false
        }
    );
    // Un second rescan n'a plus rien à faire.
    let r2 = completer_avec(
        db.as_ref(),
        100,
        Duration::from_secs(60),
        || true,
        |_| panic!("rien ne doit être relu"),
    );
    assert_eq!(r2, RapportDebits::default());
}

/// Le lot et le stockage muet bornent la passe ; le scan suivant reprend.
#[test]
fn la_passe_est_bornee_et_reprend_au_scan_suivant_6332() {
    let db = base_sqlite();
    for i in 0..5 {
        piste(&db, &format!("/m/{i}.flac"), "flac", None);
    }
    let r = completer_avec(
        db.as_ref(),
        2,
        Duration::from_secs(60),
        || true,
        |_| Ok(Some(900)),
    );
    assert_eq!((r.lus, r.interrompue), (2, true));
    let r = completer_avec(
        db.as_ref(),
        10,
        Duration::from_secs(60),
        || true,
        |p| {
            if p.ends_with("3.flac") {
                Err(())
            } else {
                Ok(Some(900))
            }
        },
    );
    assert_eq!(
        (r.lus, r.interrompue),
        (1, true),
        "le stockage muet arrête la passe"
    );
}

/// La vraie lecture : un WAV de 44,1 kHz / 16 bits / stéréo donne 1 411.
#[test]
fn la_lecture_des_proprietes_rend_le_debit_d_un_vrai_fichier_6332() {
    let dir = crate::test_scratch::scratch_dir("debit-6332");
    let chemin = dir.path().join("piste.wav");
    ecrire_wav(&chemin);
    assert_eq!(lire_debit_kbps(&chemin), Some(1411));
    assert_eq!(lire_debit_kbps(&dir.path().join("absent.wav")), None);
}

/// La même chose sur PostgreSQL, migrations comprises (094). Saute sans
/// `TUNE_TEST_PG_URL`.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_le_debit_aller_retour_et_complement_6332() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT : TUNE_TEST_PG_URL non posée");
        return;
    };
    let pool = sqlx::PgPool::connect(&url).await.expect("connexion PG");
    for sql in crate::db::postgres::ENSURE_TABLES
        .iter()
        .chain(crate::db::postgres::ENSURE_COLUMNS.iter())
    {
        let _ = sqlx::raw_sql(*sql).execute(&pool).await;
    }
    crate::db::migrations::run_pg_migrations(&pool)
        .await
        .unwrap_or_else(|e| panic!("run_pg_migrations : {e}"));
    let db: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
    let db2 = db.clone();
    tokio::task::spawn_blocking(move || {
        let db = db2;
        db.execute("TRUNCATE TABLE tracks RESTART IDENTITY CASCADE", &[])
            .unwrap();
        let mp3 = piste(&db, "/m/diva.mp3", "mp3", Some(128));
        assert_eq!(debit(&db, mp3), Some(128), "PG : le débit n'est pas écrit");
        let flac = piste(&db, "/m/a.flac", "flac", None);
        let dsf = piste(&db, "/m/b.dsf", "dsf", None);
        let r = completer_avec(
            db.as_ref(),
            100,
            Duration::from_secs(60),
            || true,
            |_| Ok(Some(902)),
        );
        assert_eq!(debit(&db, flac), Some(902));
        assert_eq!(debit(&db, dsf), Some(5644));
        assert_eq!((r.dsd_calcules, r.lus), (1, 1));
    })
    .await
    .unwrap();
    let typ: String = sqlx::query_scalar(
        "SELECT data_type FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = 'tracks' \
         AND column_name = 'bitrate_kbps'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        typ, "bigint",
        "PG 094 : `tracks.bitrate_kbps` doit être BIGINT"
    );
}

/// Relecture rc7, point 5 : un fichier qui ne répond pas arrêtait la passe au
/// MÊME endroit à chaque scan, et les pistes suivantes n'étaient jamais
/// complétées. Il est désormais marqué inconnu avant l'arrêt : le scan suivant
/// passe au-delà.
#[test]
fn un_fichier_muet_ne_bloque_pas_les_scans_suivants_6332() {
    let db = base_sqlite();
    let muet = piste(&db, "/m/0-muet.flac", "flac", None);
    let a = piste(&db, "/m/1.flac", "flac", None);
    let b = piste(&db, "/m/2.flac", "flac", None);
    let lire = |p: &std::path::Path| {
        if p.ends_with("0-muet.flac") {
            Err(())
        } else {
            Ok(Some(900))
        }
    };
    let r1 = completer_avec(db.as_ref(), 10, Duration::from_secs(60), || true, lire);
    assert!(r1.interrompue, "le stockage muet arrête la passe");
    assert_eq!(
        debit(&db, muet),
        Some(0),
        "le fichier muet doit être marqué inconnu"
    );
    let r2 = completer_avec(db.as_ref(), 10, Duration::from_secs(60), || true, lire);
    assert_eq!(
        (debit(&db, a), debit(&db, b), r2.lus),
        (Some(900), Some(900), 2),
        "le scan suivant doit passer au-delà du fichier muet, pas buter dessus"
    );
}

/// Relecture rc7, point 4 : la passe cède la place à un nouveau scan.
#[test]
fn la_passe_cede_la_place_a_un_nouveau_scan_6332() {
    let db = base_sqlite();
    let id = piste(&db, "/m/a.flac", "flac", None);
    let r = completer_avec(
        db.as_ref(),
        10,
        Duration::from_secs(60),
        || false,
        |_| panic!("un scan tient le droit : rien ne doit être lu"),
    );
    assert!(r.interrompue);
    assert_eq!(debit(&db, id), None);
}

/// Le réglage `scan_bitrate_backfill = false` coupe la passe ; absent, elle
/// tourne.
#[test]
fn le_reglage_coupe_la_passe_6332() {
    let db = base_sqlite();
    assert!(active(&db), "absent : la passe tourne");
    crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .set(CLE_REGLAGE, "false")
        .unwrap();
    assert!(!active(&db));
    assert!(
        !lancer_en_fond(db.clone(), || false),
        "réglage coupé : aucune passe lancée"
    );
}
