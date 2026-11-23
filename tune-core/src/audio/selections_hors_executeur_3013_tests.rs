//! Fil du forum 3013 — les sélections des passes de fond (ReplayGain,
//! empreintes) ne figent plus un fil de l'exécuteur.
//!
//! Le rapport du testeur (Windows, 156 000 pistes) relève des « gels de
//! l'exécuteur » de 9 à 15 s. À chaque relevé, la seule lecture SQLite en
//! cours est l'une de ces deux sélections, 6 à 7 s, posée sur un fil
//! `tokio-rt-worker`. Ici, une base qui met 600 ms à répondre à ces
//! sélections, et un exécuteur à UN fil : un battement toutes les 5 ms ne
//! doit pas se taire pendant la lecture.

use super::*;
use crate::db::backend::SqlValue;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const LENTEUR: Duration = Duration::from_millis(600);

struct BaseLente {
    interne: Arc<dyn DbBackend>,
}

impl DbBackend for BaseLente {
    fn engine(&self) -> crate::db::engine::Engine {
        self.interne.engine()
    }
    fn execute(&self, sql: &str, p: &[&dyn ToSqlValue]) -> Result<usize, String> {
        self.interne.execute(sql, p)
    }
    fn last_insert_rowid(&self) -> i64 {
        self.interne.last_insert_rowid()
    }
    fn query_one(&self, sql: &str, p: &[&dyn ToSqlValue]) -> Result<Option<Vec<SqlValue>>, String> {
        self.interne.query_one(sql, p)
    }
    fn query_many(&self, sql: &str, p: &[&dyn ToSqlValue]) -> Result<Vec<Vec<SqlValue>>, String> {
        if sql.contains("FROM tracks t WHERE") {
            // La lecture longue de la base du rapport, en plus court.
            std::thread::sleep(LENTEUR);
        }
        self.interne.query_many(sql, p)
    }
    fn write_tx(
        &self,
        f: &mut dyn FnMut(&dyn DbTxHandle) -> Result<(), String>,
    ) -> Result<(), String> {
        self.interne.write_tx(f)
    }
    fn execute_batch(&self, sql: &str) -> Result<(), String> {
        self.interne.execute_batch(sql)
    }
}

/// Le plus long silence du battement pendant un lot de la passe.
async fn silence_pendant(passe: &'static str) -> (Duration, Duration) {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(BaseLente {
        interne: Arc::new(db),
    });
    let battements = Arc::new(Mutex::new(Vec::<Instant>::new()));
    let arret = Arc::new(AtomicBool::new(false));
    let coeur = {
        let (battements, arret) = (battements.clone(), arret.clone());
        tokio::spawn(async move {
            while !arret.load(Ordering::SeqCst) {
                battements.lock().unwrap().push(Instant::now());
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    };
    tokio::time::sleep(Duration::from_millis(30)).await;
    let debut = Instant::now();
    let lot = tokio::spawn(async move {
        match passe {
            "replaygain" => analyze_track_batch(&backend).await,
            _ => empreinter_un_lot(&backend).await,
        }
    });
    lot.await.unwrap();
    let duree = debut.elapsed();
    arret.store(true, Ordering::SeqCst);
    coeur.await.unwrap();
    let b = battements.lock().unwrap();
    let silence = b
        .windows(2)
        .filter(|w| w[1] >= debut)
        .map(|w| w[1] - w[0])
        .max()
        .unwrap_or_default();
    (duree, silence)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn les_selections_des_passes_de_fond_ne_figent_pas_l_executeur_3013() {
    for passe in ["replaygain", "empreintes"] {
        let (duree, silence) = silence_pendant(passe).await;
        assert!(duree >= LENTEUR, "{passe} : la sélection n'a pas été lue");
        assert!(
            silence < LENTEUR / 3,
            "{passe} : l'exécuteur s'est tu {silence:?} pendant une lecture de {LENTEUR:?}"
        );
    }
}
