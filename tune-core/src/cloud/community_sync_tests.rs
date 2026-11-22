//! #6702 — la synchronisation communautaire ne bloque plus le sondeur.
//!
//! Sur le .18, `community_enrichments_pulled pulled=1000 applied=1000`
//! appliquait 1000 `UPDATE` d'affilée, chacun balayant toute la table
//! `tracks` (aucun index sur la colonne nue), sur un fil de l'exécuteur. Le
//! sondeur, qui enregistre la position des zones par le même verrou
//! d'écriture, attendait la fin des 1000 écritures (23 à 52 s), puis
//! rattrapait ses tours en rafale.
//!
//! ⚠️ Base de FICHIER : le banc observe les transactions réelles.

use std::cell::Cell;
use std::sync::Mutex;
use std::thread::{self, ThreadId};

use super::*;
use crate::db::backend::{DbTxHandle, SqlValue};
use crate::db::engine::Engine;
use crate::db::sqlite::SqliteDb;

/// Contenu : la boucle d'avant balaie la table 1000 fois, on la garde petite.
const PISTES_DU_CONTENU: i64 = 5_000;

/// Les épreuves historiques de contenu restent sérialisées entre elles.
/// La preuve de déport des écritures ne dépend plus de délais muraux.
static UNE_A_LA_FOIS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn base(epreuve: &str, pistes: i64) -> (crate::test_scratch::ScratchDir, Arc<dyn DbBackend>) {
    let dossier = crate::test_scratch::scratch_dir(&format!("community-sync-{epreuve}"));
    let chemin = dossier.join("tune-epreuve.db");
    let db = SqliteDb::open(&chemin.to_string_lossy()).expect("base de fichier");
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    peupler(&db, pistes);
    (dossier, db)
}

/// `pistes` pistes ; les 1200 premières portent un MBID ; des champs déjà
/// remplis çà et là (jamais écrasés) ; un doublon de MBID ; une piste au MBID
/// en casse différente (jamais touchée, comme avant).
fn peupler(db: &Arc<dyn DbBackend>, pistes: i64) {
    db.execute(
        "INSERT INTO tracks (title, file_path, musicbrainz_recording_id, genre, year, composer) \
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?) \
         SELECT 'piste ' || i, '/banc/' || i || '.flac', \
                CASE WHEN i <= 1200 THEN printf('mbid-%05d', i) ELSE NULL END, \
                CASE WHEN i % 7 = 0 THEN 'Jazz' END, \
                CASE WHEN i % 5 = 0 THEN 1999 END, \
                CASE WHEN i % 11 = 0 THEN 'Bach' END \
         FROM n",
        &[&pistes as &dyn ToSqlValue],
    )
    .expect("pistes du banc");
    db.execute(
        "INSERT INTO tracks (title, file_path, musicbrainz_recording_id) VALUES \
         ('doublon', '/banc/doublon.flac', 'mbid-00001'), \
         ('casse', '/banc/casse.flac', 'MBID-00002')",
        &[],
    )
    .expect("doublon et casse");
}

/// 1000 enrichissements : 990 MBID connus, 10 inconnus.
fn enrichissements() -> Vec<Enrichissement> {
    (1..=1000)
        .map(|i: i32| Enrichissement {
            mb_id: if i <= 990 {
                format!("mbid-{i:05}")
            } else {
                format!("inconnu-{i}")
            },
            genre: (i % 2 == 0).then(|| "Rock".to_string()),
            year: Some(2000 + i % 20),
            composer: (i % 3 != 0).then(|| format!("Compositeur {i}")),
            label: Some(format!("Label {i}")),
            isrc: (i % 4 != 0).then(|| format!("FRXXX{i:07}")),
        })
        .collect()
}

/// La boucle d'AVANT #6702, en un seul bloc : la référence du contenu.
fn appliquer_en_un_bloc(backend: &Arc<dyn DbBackend>, items: &[Enrichissement]) -> usize {
    let mut applied = 0usize;
    for e in items {
        let result = backend.execute(
            "UPDATE tracks SET \
             genre = COALESCE(genre, ?), \
             year = COALESCE(year, ?), \
             composer = COALESCE(composer, ?), \
             label = COALESCE(label, ?), \
             isrc = COALESCE(isrc, ?) \
             WHERE musicbrainz_recording_id = ? \
             AND (genre IS NULL OR year IS NULL OR composer IS NULL)",
            &[
                &e.genre as &dyn ToSqlValue,
                &e.year as &dyn ToSqlValue,
                &e.composer as &dyn ToSqlValue,
                &e.label as &dyn ToSqlValue,
                &e.isrc as &dyn ToSqlValue,
                &e.mb_id as &dyn ToSqlValue,
            ],
        );
        if result.is_ok() {
            applied += 1;
        }
    }
    applied
}

fn contenu(db: &Arc<dyn DbBackend>) -> Vec<String> {
    db.query_many(
        "SELECT id, musicbrainz_recording_id, genre, year, composer, label, isrc \
         FROM tracks WHERE musicbrainz_recording_id IS NOT NULL ORDER BY id",
        &[],
    )
    .unwrap()
    .into_iter()
    .map(|l| format!("{l:?}"))
    .collect()
}

/// Observe les vraies écritures sans remplacer SQLite ni la boucle de production.
/// Un délai mural mélangeait le blocage du sondeur avec la charge du runner.
/// Ici, chaque accès SQL vérifie qu'il a quitté l'unique fil d'exécution async.
struct BaseObservee {
    db: Arc<dyn DbBackend>,
    fil_async: ThreadId,
    tailles: Arc<Mutex<Vec<usize>>>,
}
impl BaseObservee {
    fn verifier_fil(&self) {
        assert_ne!(
            thread::current().id(),
            self.fil_async,
            "#6702 : une écriture communautaire occupe le fil du sondeur"
        );
    }
}
impl DbBackend for BaseObservee {
    fn engine(&self) -> Engine {
        self.db.engine()
    }
    fn execute(&self, sql: &str, params: &[&dyn ToSqlValue]) -> Result<usize, String> {
        self.verifier_fil();
        self.db.execute(sql, params)
    }
    fn last_insert_rowid(&self) -> i64 {
        self.db.last_insert_rowid()
    }
    fn query_one(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Option<Vec<SqlValue>>, String> {
        self.verifier_fil();
        self.db.query_one(sql, params)
    }
    fn query_many(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Vec<Vec<SqlValue>>, String> {
        self.verifier_fil();
        self.db.query_many(sql, params)
    }
    fn execute_batch(&self, sql: &str) -> Result<(), String> {
        self.verifier_fil();
        self.db.execute_batch(sql)
    }
    fn write_tx(
        &self,
        f: &mut dyn FnMut(&dyn DbTxHandle) -> Result<(), String>,
    ) -> Result<(), String> {
        self.verifier_fil();
        self.db.write_tx(&mut |tx| {
            let observe = TransactionObservee {
                tx,
                ecritures: Cell::new(0),
            };
            f(&observe)?;
            self.tailles.lock().unwrap().push(observe.ecritures.get());
            Ok(())
        })
    }
}
struct TransactionObservee<'a> {
    tx: &'a dyn DbTxHandle,
    ecritures: Cell<usize>,
}
impl DbTxHandle for TransactionObservee<'_> {
    fn execute(&self, sql: &str, params: &[&dyn ToSqlValue]) -> Result<usize, String> {
        let result = self.tx.execute(sql, params)?;
        self.ecritures.set(self.ecritures.get() + 1);
        Ok(result)
    }
    fn query_one(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Option<Vec<SqlValue>>, String> {
        self.tx.query_one(sql, params)
    }
    fn query_many(
        &self,
        sql: &str,
        params: &[&dyn ToSqlValue],
    ) -> Result<Vec<Vec<SqlValue>>, String> {
        self.tx.query_many(sql, params)
    }
    fn last_insert_rowid(&self) -> i64 {
        self.tx.last_insert_rowid()
    }
}

/// Un seul worker : les requêtes doivent se faire hors de ce fil, par lots
/// de cinquante au plus. La charge de la machine ne change pas ces propriétés.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn les_lots_ne_tiennent_pas_le_fil_du_sondeur_6702() {
    let (_dossier, db) = base("fil-du-sondeur", PISTES_DU_CONTENU);
    let tailles = Arc::new(Mutex::new(Vec::new()));
    let observees = tailles.clone();
    let applied = tokio::spawn(async move {
        let backend: Arc<dyn DbBackend> = Arc::new(BaseObservee {
            db,
            // Capturé sur le worker, pas sur le fil qui pilote #[tokio::test].
            fil_async: thread::current().id(),
            tailles: observees,
        });
        appliquer_les_enrichissements(&backend, enrichissements()).await
    })
    .await
    .expect("la synchronisation ne doit pas écrire sur le fil du sondeur");
    assert_eq!(applied, 1000, "1000 enrichissements appliqués sans erreur");
    let tailles = tailles.lock().unwrap();
    assert!(
        tailles.len() > 1,
        "les enrichissements tiennent une seule transaction"
    );
    assert!(
        tailles.iter().all(|&n| n > 0 && n <= 50),
        "transaction trop longue : {tailles:?}"
    );
    assert_eq!(
        tailles.iter().sum::<usize>(),
        1000,
        "écritures sorties des transactions courtes"
    );
}

/// Contenu identique à la boucle d'avant, et même compte `applied`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contenu_identique_a_l_application_en_un_bloc_6702() {
    let _seul = UNE_A_LA_FOIS.lock().await;
    let (_a, avant) = base("contenu-avant", PISTES_DU_CONTENU);
    let (_b, apres) = base("contenu-apres", PISTES_DU_CONTENU);
    let items = enrichissements();

    let attendu = appliquer_en_un_bloc(&avant, &items);
    let obtenu = appliquer_les_enrichissements(&apres, items).await;

    assert_eq!(obtenu, attendu, "même compte applied");
    let (c_avant, c_apres) = (contenu(&avant), contenu(&apres));
    assert_eq!(c_avant.len(), c_apres.len());
    for (a, b) in c_avant.iter().zip(&c_apres) {
        assert_eq!(a, b, "même contenu ligne à ligne");
    }
    // Le banc remplit bien quelque chose, et n'écrase rien.
    assert!(c_apres.iter().any(|l| l.contains("Label 1\"")));
}

/// Rejouer la même synchronisation ne change plus rien.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejouer_la_synchronisation_ne_change_rien_6702() {
    let _seul = UNE_A_LA_FOIS.lock().await;
    let (_d, db) = base("idempotence", PISTES_DU_CONTENU);
    let premier = appliquer_les_enrichissements(&db, enrichissements()).await;
    let apres_un = contenu(&db);
    let second = appliquer_les_enrichissements(&db, enrichissements()).await;
    assert_eq!(premier, second);
    assert_eq!(apres_un, contenu(&db));
}

/// La mise à jour va droit aux lignes par l'index plié du MBID, au lieu de
/// balayer `tracks` à chaque enrichissement.
#[test]
fn la_mise_a_jour_passe_par_l_index_du_mbid_6702() {
    let _seul = UNE_A_LA_FOIS.blocking_lock();
    let (_d, db) = base("plan", PISTES_DU_CONTENU);
    let e = &enrichissements()[0];
    let sql = format!("EXPLAIN QUERY PLAN {SQL_APPLIQUER_ENRICHISSEMENT}");
    let plan: Vec<String> = db
        .query_many(&sql, &parametres(e))
        .unwrap()
        .into_iter()
        .map(|l| format!("{l:?}"))
        .collect();
    assert!(
        plan.iter()
            .any(|l| l.contains("idx_tracks_mbid_recording_norm")),
        "plan sans l'index du MBID : {plan:?}"
    );
}

/// Une entrée sans MBID est sautée, comme avant.
#[test]
fn lire_les_enrichissements_saute_les_entrees_sans_mbid() {
    let data = serde_json::json!({ "tracks": [
        { "musicbrainz_recording_id": "a", "genre": "Rock", "year": 1990 },
        { "genre": "Jazz" },
        { "musicbrainz_recording_id": "b", "composer": "Bach", "isrc": "X", "label": "L" },
    ]});
    let lus = lire_les_enrichissements(&data);
    assert_eq!(lus.len(), 2);
    assert_eq!(lus[0].year, Some(1990));
    assert_eq!(lus[1].composer.as_deref(), Some("Bach"));
}
