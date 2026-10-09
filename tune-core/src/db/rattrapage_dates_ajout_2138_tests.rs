//! Fil 2138 — le rattrapage des dates d'ajout figées au premier scan, joué
//! sur SQLite et sur PostgreSQL (mêmes scénarios, même backend `dyn`).

use std::path::Path;
use std::sync::Arc;

use super::*;
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::home_queries::{DATE_D_AJOUT, JOINTURE_PREMIERE_VUE};
use crate::db::sqlite::SqliteDb;

/// La date du premier scan figée (05/09/2026, rc1).
const FIGEE: f64 = 1_788_000_000.0;
/// Un mois plus tard : des pistes ajoutées après le premier scan.
const APRES: f64 = FIGEE + 30.0 * 86_400.0;

fn sqlite() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn fichier(dir: &Path, nom: &str, mtime: u64) -> String {
    let chemin = dir.join(nom);
    let f = std::fs::File::create(&chemin).unwrap();
    f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime))
        .unwrap();
    chemin.to_str().unwrap().to_string()
}

/// Une piste locale, sa date d'ajout et son `file_mtime` en base.
fn piste(db: &Arc<dyn DbBackend>, chemin: &str, ajout: f64, mtime: Option<f64>) {
    let p: [&dyn ToSqlValue; 3] = [&"t", &chemin, &mtime];
    db.execute(
        "INSERT INTO tracks (title, file_path, file_mtime) VALUES (?, ?, ?)",
        &p,
    )
    .unwrap();
    let p: [&dyn ToSqlValue; 2] = [&chemin, &ajout];
    db.execute(
        "INSERT INTO file_first_seen (file_path, first_seen_at) VALUES (?, ?)",
        &p,
    )
    .unwrap();
}

/// La date d'ajout telle que la lit la bibliothèque (`DATE_D_AJOUT`).
fn date_d_ajout(db: &Arc<dyn DbBackend>, chemin: &str) -> f64 {
    let sql = format!(
        "SELECT {DATE_D_AJOUT} FROM tracks t {JOINTURE_PREMIERE_VUE} WHERE t.file_path = ?"
    );
    let p: [&dyn ToSqlValue; 1] = [&chemin];
    db.query_one_strong(&sql, &p)
        .unwrap()
        .and_then(|l| l.first().and_then(SqlValue::as_f64))
        .unwrap()
}

/// Base figée : 30 pistes, 27 (90 %) au premier scan, 3 ajoutées un mois
/// après. Parmi les 27 : une au fichier absent, une sans `file_mtime` en
/// base (le `stat` décide), une dont le fichier a été retouché APRÈS le scan.
fn scenario_base_figee(db: Arc<dyn DbBackend>) {
    let dir = tempfile::tempdir().unwrap();
    let mut au_scan = Vec::new();
    for i in 0..24u64 {
        let mtime = 1_500_000_000 + i * 86_400;
        let c = fichier(dir.path(), &format!("scan-{i:02}.flac"), mtime);
        // Le premier scan a posé une date par lot, à quelques secondes près.
        piste(&db, &c, FIGEE + (i / 8) as f64 * 40.0, Some(mtime as f64));
        au_scan.push((c, mtime as f64));
    }
    // Sans `file_mtime` en base : la date vient du disque.
    let sans_mtime = fichier(dir.path(), "scan-sans-mtime.flac", 1_400_000_000);
    piste(&db, &sans_mtime, FIGEE + 100.0, None);
    // Fichier absent du disque : sa date reste, même avec un `file_mtime`.
    let absent = dir
        .path()
        .join("scan-absent.flac")
        .to_str()
        .unwrap()
        .to_string();
    piste(&db, &absent, FIGEE + 100.0, Some(1_300_000_000.0));
    // Retouché après le scan : son mtime ne dit pas quand il est entré.
    let retouche_mtime = (FIGEE + 5_000.0) as u64;
    let retouche = fichier(dir.path(), "scan-retouche.flac", retouche_mtime);
    piste(&db, &retouche, FIGEE + 100.0, Some(retouche_mtime as f64));
    // Ajoutées après : jamais touchées, même avec un vieux mtime.
    let mut apres = Vec::new();
    for i in 0..3u64 {
        let c = fichier(dir.path(), &format!("apres-{i}.flac"), 1_200_000_000);
        piste(&db, &c, APRES + i as f64 * 86_400.0, Some(1_200_000_000.0));
        apres.push((c, APRES + i as f64 * 86_400.0));
    }

    let issue = rattraper_les_dates_d_ajout(&db).unwrap();
    let Issue::Corrigee(bilan) = issue else {
        panic!("base figée à 90 % non détectée : {issue:?}");
    };
    assert_eq!(
        (
            bilan.pistes,
            bilan.dans_le_bloc,
            bilan.corrigees,
            bilan.laissees,
            bilan.absentes
        ),
        (30, 27, 25, 1, 1),
        "{bilan:?}"
    );
    for (c, mtime) in &au_scan {
        assert_eq!(
            date_d_ajout(&db, c),
            *mtime,
            "une piste du premier scan doit être redatée par son fichier : {c}"
        );
    }
    assert_eq!(
        date_d_ajout(&db, &sans_mtime),
        1_400_000_000.0,
        "stat du fichier"
    );
    assert_eq!(
        date_d_ajout(&db, &absent),
        FIGEE + 100.0,
        "fichier absent : date gardée"
    );
    assert_eq!(
        date_d_ajout(&db, &retouche),
        FIGEE + 100.0,
        "un mtime postérieur au scan ne recule pas la date… ni ne l'avance"
    );
    for (c, ajout) in &apres {
        assert_eq!(
            date_d_ajout(&db, c),
            *ajout,
            "piste ajoutée après : intacte"
        );
    }
    let marqueur = crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .get(CLE_RATTRAPAGE_DATES_AJOUT_2138)
        .unwrap();
    assert_eq!(
        marqueur.as_deref(),
        Some("corrigees=25;laissees=1;absentes=1")
    );

    // Seconde exécution : rien n'est refait, même si une date redevient figée.
    let (premiere, _) = &au_scan[0];
    let p: [&dyn ToSqlValue; 2] = [&FIGEE, premiere];
    db.execute(
        "UPDATE file_first_seen SET first_seen_at = ? WHERE file_path = ?",
        &p,
    )
    .unwrap();
    assert_eq!(rattraper_les_dates_d_ajout(&db).unwrap(), Issue::DejaFaite);
    assert_eq!(
        date_d_ajout(&db, premiere),
        FIGEE,
        "la seconde passe a récrit"
    );
}

/// Base normale : des ajouts étalés sur des mois. Rien n'est touché, le
/// marqueur est posé.
fn scenario_base_normale(db: Arc<dyn DbBackend>) {
    let dir = tempfile::tempdir().unwrap();
    let mut toutes = Vec::new();
    for i in 0..30u64 {
        let c = fichier(dir.path(), &format!("n-{i:02}.flac"), 1_400_000_000);
        // Une piste par jour, sauf trois au même instant que la première.
        let ajout = if i < 4 {
            FIGEE
        } else {
            FIGEE + i as f64 * 86_400.0
        };
        piste(&db, &c, ajout, Some(1_400_000_000.0));
        toutes.push((c, ajout));
    }
    assert_eq!(
        rattraper_les_dates_d_ajout(&db).unwrap(),
        Issue::NonFigee {
            pistes: 30,
            dans_le_bloc: 4
        }
    );
    for (c, ajout) in &toutes {
        assert_eq!(date_d_ajout(&db, c), *ajout, "base normale touchée : {c}");
    }
    assert_eq!(rattraper_les_dates_d_ajout(&db).unwrap(), Issue::DejaFaite);
}

#[test]
fn sqlite_base_figee_redatee_par_les_fichiers_2138() {
    scenario_base_figee(sqlite());
}

#[test]
fn sqlite_base_normale_intacte_2138() {
    scenario_base_normale(sqlite());
}

/// Le seuil : 80 % des pistes locales, au moins 20 pistes ; une piste sans
/// date compte au dénominateur.
#[test]
fn seuil_de_detection_2138() {
    let bloc = |n: usize| vec![FIGEE; n];
    assert!(est_figee(16, 20));
    assert!(!est_figee(15, 20), "75 % : pas figée");
    assert!(!est_figee(19, 19), "moins de 20 pistes : pas de conclusion");
    assert_eq!(mesurer_le_bloc(&bloc(16)), Some((FIGEE, FIGEE, 16)));
    assert_eq!(mesurer_le_bloc(&[]), None);
}

/// Le bloc s'étend de proche en proche, par écarts de moins de dix minutes,
/// sans dépasser 72 h, et part de la date la PLUS ANCIENNE.
#[test]
fn bloc_du_premier_scan_2138() {
    // Un scan de cinq heures, un lot toutes les cinq minutes, puis un ajout
    // le lendemain.
    let mut dates: Vec<f64> = (0..60).map(|i| FIGEE + i as f64 * 300.0).collect();
    dates.push(FIGEE + 86_400.0);
    dates.reverse();
    assert_eq!(
        mesurer_le_bloc(&dates),
        Some((FIGEE, FIGEE + 59.0 * 300.0, 60))
    );
    // Un trou de plus de dix minutes ferme le bloc.
    assert_eq!(
        mesurer_le_bloc(&[FIGEE, FIGEE + 601.0]),
        Some((FIGEE, FIGEE, 1))
    );
    // Dates invalides ignorées.
    assert_eq!(
        mesurer_le_bloc(&[f64::NAN, 0.0, -5.0, FIGEE]),
        Some((FIGEE, FIGEE, 1))
    );
}

#[test]
fn nouvelle_date_2138() {
    // La base d'abord, le disque ensuite.
    assert_eq!(nouvelle_date(Some(10.0), Some(20.0), FIGEE), Some(10.0));
    assert_eq!(nouvelle_date(None, Some(20.0), FIGEE), Some(20.0));
    assert_eq!(nouvelle_date(Some(0.0), Some(20.0), FIGEE), Some(20.0));
    // Jamais plus tard que la date figée.
    assert_eq!(nouvelle_date(Some(FIGEE + 1.0), None, FIGEE), None);
    assert_eq!(nouvelle_date(Some(FIGEE), None, FIGEE), None);
    assert_eq!(nouvelle_date(None, None, FIGEE), None);
}

#[cfg(feature = "postgres")]
mod pg {
    use super::*;
    use sqlx::{Connection, PgConnection};

    /// Une base PostgreSQL jetable, au schéma complet (`PG_FULL_SCHEMA` puis
    /// les scripts numérotés), comme une base basculée puis mise à jour.
    async fn base_jetable(url: &str, nom: &str) -> (Arc<dyn DbBackend>, sqlx::PgPool) {
        let mut m = PgConnection::connect(url)
            .await
            .unwrap_or_else(|e| panic!("TUNE_TEST_PG_URL posée ({url}) mais injoignable : {e}"));
        for ordre in [
            format!("DROP DATABASE IF EXISTS {nom} WITH (FORCE)"),
            format!("CREATE DATABASE {nom}"),
        ] {
            sqlx::raw_sql(sqlx::AssertSqlSafe(ordre.clone()))
                .execute(&mut m)
                .await
                .unwrap_or_else(|e| panic!("{ordre} : {e}"));
        }
        m.close().await.ok();
        let (avant, requete) = match url.split_once('?') {
            Some((a, q)) => (a, format!("?{q}")),
            None => (url, String::new()),
        };
        let racine = avant.rsplit_once('/').map(|(r, _)| r).unwrap_or(avant);
        let cible = format!("{racine}/{nom}{requete}");
        let pool = sqlx::PgPool::connect(&cible).await.unwrap();
        sqlx::raw_sql("CREATE EXTENSION IF NOT EXISTS unaccent")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(crate::db::pg_migrate::PG_FULL_SCHEMA)
            .execute(&pool)
            .await
            .expect("PG_FULL_SCHEMA");
        crate::db::migrations::run_pg_migrations(&pool)
            .await
            .expect("run_pg_migrations");
        let db: Arc<dyn DbBackend> =
            Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
        (db, pool)
    }

    async fn supprimer(url: &str, nom: &str, pool: sqlx::PgPool) {
        pool.close().await;
        if let Ok(mut m) = PgConnection::connect(url).await {
            let _ = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "DROP DATABASE IF EXISTS {nom} WITH (FORCE)"
            )))
            .execute(&mut m)
            .await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pg_2138_base_figee_redatee_par_les_fichiers() {
        let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
            eprintln!("SAUT : TUNE_TEST_PG_URL absente");
            return;
        };
        const NOM: &str = "tune_dates_ajout_2138_figee";
        let (db, pool) = base_jetable(&url, NOM).await;
        scenario_base_figee(db);
        supprimer(&url, NOM, pool).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pg_2138_base_normale_intacte() {
        let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
            eprintln!("SAUT : TUNE_TEST_PG_URL absente");
            return;
        };
        const NOM: &str = "tune_dates_ajout_2138_normale";
        let (db, pool) = base_jetable(&url, NOM).await;
        scenario_base_normale(db);
        supprimer(&url, NOM, pool).await;
    }
}
