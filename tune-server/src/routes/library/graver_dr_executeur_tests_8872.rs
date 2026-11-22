//! #8872 — GET et POST gardent leurs vrais accès SQLite hors du worker async.
//!
//! Un même observateur délègue à la vraie base de fichier et vérifie le fil
//! des requêtes coûteuses : inventaire pour POST, inventaire et compte pour
//! GET. Aucun métronome mural ne confond charge du runner et blocage.
use crate::state::AppState;
use axum::extract::State;
use axum::response::IntoResponse;
use std::sync::Arc;
use std::sync::atomic::Ordering;

fn base() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("tune.db");
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
    crate::routes::ecriture_fichiers::activer_pour_test(&state.backend);
    (dir, state)
}

/// Observe le vrai accès SQLite sans confondre charge du runner et blocage.
struct BaseHorsExecuteur {
    reel: Arc<dyn tune_core::db::backend::DbBackend>,
    executeur: std::thread::ThreadId,
    route: &'static str,
    lectures: Arc<std::sync::atomic::AtomicUsize>,
}

impl tune_core::db::backend::DbBackend for BaseHorsExecuteur {
    fn engine(&self) -> tune_core::db::engine::Engine {
        self.reel.engine()
    }
    fn execute(
        &self,
        sql: &str,
        params: &[&dyn tune_core::db::backend::ToSqlValue],
    ) -> Result<usize, String> {
        self.reel.execute(sql, params)
    }
    fn last_insert_rowid(&self) -> i64 {
        self.reel.last_insert_rowid()
    }
    fn query_one(
        &self,
        sql: &str,
        params: &[&dyn tune_core::db::backend::ToSqlValue],
    ) -> Result<Option<Vec<tune_core::db::backend::SqlValue>>, String> {
        if sql == "SELECT COUNT(*) FROM track_metadata WHERE key = 'dr_source' AND value = 'tag'" {
            self.lectures.fetch_add(1, Ordering::SeqCst);
            assert_ne!(
                std::thread::current().id(),
                self.executeur,
                "#8872 : le comptage {} occupe le fil de l'exécuteur",
                self.route
            );
        }
        self.reel.query_one(sql, params)
    }
    fn query_many(
        &self,
        sql: &str,
        params: &[&dyn tune_core::db::backend::ToSqlValue],
    ) -> Result<Vec<Vec<tune_core::db::backend::SqlValue>>, String> {
        if sql == super::sql_candidates() {
            self.lectures.fetch_add(1, Ordering::SeqCst);
            assert_ne!(
                std::thread::current().id(),
                self.executeur,
                "#8872 : l'inventaire {} occupe le fil de l'exécuteur",
                self.route
            );
        }
        self.reel.query_many(sql, params)
    }
    fn write_tx(
        &self,
        f: &mut dyn FnMut(&dyn tune_core::db::backend::DbTxHandle) -> Result<(), String>,
    ) -> Result<(), String> {
        self.reel.write_tx(f)
    }
    fn execute_batch(&self, sql: &str) -> Result<(), String> {
        self.reel.execute_batch(sql)
    }
}

/// TÉMOIN #8872 — les deux requêtes coûteuses quittent le worker async unique.
#[tokio::test(flavor = "current_thread")]
async fn le_statut_de_la_gravure_ne_fige_pas_l_executeur_8872() {
    let (_dir, mut state) = base();
    let lectures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    state.backend = Arc::new(BaseHorsExecuteur {
        reel: state.backend.clone(),
        executeur: std::thread::current().id(),
        route: "GET",
        lectures: lectures.clone(),
    });
    let v = super::statut(State(state)).await.0;
    assert_eq!(
        lectures.load(Ordering::SeqCst),
        2,
        "inventaire et comptage réels doivent être observés"
    );
    assert_eq!(v["status"], "idle", "{v}");
    assert_eq!(v["a_graver"], 0, "{v}");
    assert_eq!(v["hors_format"], 0, "{v}");
    assert_eq!(v["dans_les_fichiers"], 0, "{v}");
}

/// TÉMOIN #8872 — l'inventaire réel doit quitter le worker async unique.
#[tokio::test(flavor = "current_thread")]
async fn lancer_la_gravure_ne_fige_pas_l_executeur_8872() {
    let (_dir, mut state) = base();
    let lectures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    state.backend = Arc::new(BaseHorsExecuteur {
        reel: state.backend.clone(),
        executeur: std::thread::current().id(),
        route: "POST",
        lectures: lectures.clone(),
    });
    let statut = super::lancer(State(state.clone()))
        .await
        .into_response()
        .status();
    assert_eq!(statut, axum::http::StatusCode::ACCEPTED);
    assert_eq!(
        lectures.load(Ordering::SeqCst),
        1,
        "l'inventaire réel doit être observé"
    );
    // Attendre la garde de la passe vide avant de fermer la base et son
    // dossier, notamment sous Windows où les connexions retiennent le fichier.
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while state
            .background_tasks
            .snapshot()
            .iter()
            .any(|t| t.id == super::TACHE_GRAVER_DR)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("#8872 : la passe ne libère pas sa garde de fin");
}
