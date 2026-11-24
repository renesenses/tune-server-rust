//! Fil du forum 3013 — les lectures longues des passes de fond (ReplayGain,
//! empreintes, plage dynamique) ne figent plus un fil de l'exécuteur.
//!
//! Le rapport du testeur (Windows, 156 000 pistes) relève des « gels de
//! l'exécuteur » de 9 à 15 s. À chaque relevé, la seule lecture SQLite en
//! cours est une sélection de passe de fond, 6 à 7 s, posée sur un fil
//! `tokio-rt-worker`. Les sélections ET les `COUNT(*)` d'ouverture de
//! campagne passent désormais par `taches_de_fond::priorite::hors_du_fil_async`.
//!
//! Ici, une base qui met 600 ms à répondre à la lecture visée, et un exécuteur
//! à UN fil : un battement toutes les 5 ms ne doit pas se taire plus de
//! 150 ms. Le battement note aussi l'instant où il s'arrête, et ne s'arrête
//! que 50 ms après la passe : le silence d'un fil bloqué est toujours mesuré
//! (sinon, quand l'arrêt est vu avant le battement suivant, le témoin
//! pouvait rester vert sans correctif).

use super::*;
use crate::db::backend::SqlValue;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const LENTEUR: Duration = Duration::from_millis(600);
const SILENCE_MAX: Duration = Duration::from_millis(150);

/// Quelle lecture la base fait attendre.
#[derive(Clone, Copy)]
enum Visee {
    /// La sélection des candidats (`SELECT … FROM tracks t WHERE …`).
    Selection,
    /// Le `COUNT(*)` d'ouverture de campagne.
    Comptage,
}

impl Visee {
    fn vise(self, sql: &str) -> bool {
        let compte = sql.contains("SELECT COUNT(*) FROM tracks t WHERE");
        match self {
            Visee::Selection => sql.contains("FROM tracks t WHERE") && !compte,
            Visee::Comptage => compte,
        }
    }
}

struct BaseLente {
    interne: Arc<dyn DbBackend>,
    visee: Visee,
    ralenties: AtomicUsize,
}

impl BaseLente {
    fn peut_etre_attendre(&self, sql: &str) {
        if self.visee.vise(sql) {
            self.ralenties.fetch_add(1, Ordering::SeqCst);
            // La lecture longue de la base du rapport, en plus court.
            std::thread::sleep(LENTEUR);
        }
    }
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
        self.peut_etre_attendre(sql);
        self.interne.query_one(sql, p)
    }
    fn query_many(&self, sql: &str, p: &[&dyn ToSqlValue]) -> Result<Vec<Vec<SqlValue>>, String> {
        self.peut_etre_attendre(sql);
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

#[derive(Clone, Copy)]
enum Passe {
    ReplayGain,
    Empreintes,
    PlageDynamique,
}

/// Une base où chaque passe trouve un candidat : la piste 1 n'est pas
/// analysée (ReplayGain), la piste 2 l'est (empreintes, plage dynamique).
/// Les fichiers n'existent pas : la passe les reporte, sans rien décoder.
fn base_avec_candidats() -> Arc<dyn DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    db.execute_batch(
        "INSERT INTO tracks (id, title, file_path, format, duration_ms, sample_rate, channels) \
         VALUES (1, 'Une', '/banc-3013-inexistant/1.flac', 'flac', 300000, 44100, 2), \
                (2, 'Deux', '/banc-3013-inexistant/2.flac', 'flac', 300000, 44100, 2); \
         INSERT INTO track_metadata (track_id, key, value) VALUES (2, 'rg_analyzed', '1');",
    )
    .unwrap();
    Arc::new(db)
}

/// Le plus long silence du battement pendant un lot de la passe, et combien
/// de lectures visées ont été ralenties.
async fn silence_pendant(passe: Passe, visee: Visee) -> (Duration, usize) {
    // Une campagne ouverte par un autre témoin sauterait le comptage.
    progression::au_repos();
    crate::taches_de_fond::avancement::clore(crate::taches_de_fond::Tache::Empreintes);
    crate::taches_de_fond::avancement::clore(crate::taches_de_fond::Tache::PlageDynamique);
    let base = Arc::new(BaseLente {
        interne: base_avec_candidats(),
        visee,
        ralenties: AtomicUsize::new(0),
    });
    let backend: Arc<dyn DbBackend> = base.clone();
    let battements = Arc::new(Mutex::new(Vec::<Instant>::new()));
    let arret = Arc::new(AtomicBool::new(false));
    let coeur = {
        let (battements, arret) = (battements.clone(), arret.clone());
        tokio::spawn(async move {
            loop {
                battements.lock().unwrap().push(Instant::now());
                if arret.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    };
    tokio::time::sleep(Duration::from_millis(30)).await;
    let debut = Instant::now();
    let lot = tokio::spawn(async move {
        match passe {
            Passe::ReplayGain => analyze_track_batch(&backend).await,
            Passe::Empreintes => empreinter_un_lot(&backend).await,
            Passe::PlageDynamique => rattraper_un_lot_de_dr(&backend).await,
        }
    });
    lot.await.unwrap();
    // Le battement repart et se note avant de voir l'arrêt.
    tokio::time::sleep(Duration::from_millis(50)).await;
    arret.store(true, Ordering::SeqCst);
    coeur.await.unwrap();
    let b = battements.lock().unwrap();
    let silence = b
        .windows(2)
        .filter(|w| w[1] >= debut)
        .map(|w| w[1] - w[0])
        .max()
        .unwrap_or_default();
    (silence, base.ralenties.load(Ordering::SeqCst))
}

async fn exiger_un_executeur_vivant(nom: &str, passe: Passe, visee: Visee) {
    // L'état des campagnes est global au processus : un témoin voisin
    // pouvait en rouvrir une entre la remise au repos et le comptage, qui
    // était alors sauté (#9057, sous charge, malgré trois essais). Tous les
    // témoins qui ouvrent une campagne prennent désormais ce verrou. Les
    // trois essais restent : jamais un vert sans lecture.
    let _campagnes = crate::taches_de_fond::avancement::campagnes_des_essais().await;
    for _ in 0..3 {
        let (silence, ralenties) = silence_pendant(passe, visee).await;
        if ralenties == 0 {
            continue;
        }
        assert!(
            silence < SILENCE_MAX,
            "{nom} : l'exécuteur s'est tu {silence:?} pendant une lecture de {LENTEUR:?}"
        );
        return;
    }
    panic!("{nom} : la lecture visée n'a pas eu lieu");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn la_selection_du_replaygain_ne_fige_pas_l_executeur_3013() {
    exiger_un_executeur_vivant("replaygain", Passe::ReplayGain, Visee::Selection).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn la_selection_des_empreintes_ne_fige_pas_l_executeur_3013() {
    exiger_un_executeur_vivant("empreintes", Passe::Empreintes, Visee::Selection).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn la_selection_de_la_plage_dynamique_ne_fige_pas_l_executeur_3013() {
    exiger_un_executeur_vivant("plage dynamique", Passe::PlageDynamique, Visee::Selection).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn le_comptage_d_ouverture_du_replaygain_ne_fige_pas_l_executeur_3013() {
    exiger_un_executeur_vivant("comptage replaygain", Passe::ReplayGain, Visee::Comptage).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn le_comptage_d_ouverture_des_empreintes_ne_fige_pas_l_executeur_3013() {
    exiger_un_executeur_vivant("comptage empreintes", Passe::Empreintes, Visee::Comptage).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn le_comptage_d_ouverture_de_la_plage_dynamique_ne_fige_pas_l_executeur_3013() {
    exiger_un_executeur_vivant(
        "comptage plage dynamique",
        Passe::PlageDynamique,
        Visee::Comptage,
    )
    .await;
}
