//! #6702 — la synchronisation communautaire ne bloque plus le sondeur.
//!
//! Sur le .18, `community_enrichments_pulled pulled=1000 applied=1000`
//! appliquait 1000 `UPDATE` d'affilée, chacun balayant toute la table
//! `tracks` (aucun index sur la colonne nue), sur un fil de l'exécuteur. Le
//! sondeur, qui enregistre la position des zones par le même verrou
//! d'écriture, attendait la fin des 1000 écritures (23 à 52 s), puis
//! rattrapait ses tours en rafale.
//!
//! ⚠️ Base de FICHIER : le banc mesure le verrou d'écriture réel.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::*;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;

/// Banc : assez de pistes pour qu'un balayage complet coûte, comme sur le .18.
const PISTES_DU_BANC: i64 = 60_000;

/// Contenu : la boucle d'avant balaie la table 1000 fois, on la garde petite.
const PISTES_DU_CONTENU: i64 = 5_000;

/// Les épreuves de ce module passent l'une après l'autre : le banc mesure des
/// délais, une épreuve voisine qui écrase le processeur le fausserait.
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

/// Le banc : un sondeur factice (intervalle de 500 ms, rafale comme le vrai)
/// enregistre la position d'une zone par le verrou d'écriture pendant que la
/// synchronisation, lancée par `tokio::spawn` comme en production, applique
/// 1000 enrichissements. Rend le pire écart entre deux tours du sondeur, la
/// durée de l'application et le nombre de tours.
async fn banc_du_sondeur() -> (Duration, Duration, i64) {
    let (_dossier, db) = base("banc", PISTES_DU_BANC);
    let arret = Arc::new(AtomicBool::new(false));
    let sondeur = {
        let db = db.clone();
        let arret = arret.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_millis(500));
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
            iv.tick().await;
            let mut dernier = Instant::now();
            let mut pire = Duration::ZERO;
            let mut tours = 0i64;
            while !arret.load(Ordering::Acquire) {
                iv.tick().await;
                ZoneRepo::with_backend(db.clone())
                    .save_playback_position(1, tours * 1000, None, None, None)
                    .ok();
                let maintenant = Instant::now();
                pire = pire.max(maintenant - dernier);
                dernier = maintenant;
                tours += 1;
            }
            (pire, tours)
        })
    };
    tokio::time::sleep(Duration::from_millis(700)).await;

    let debut = Instant::now();
    let synchro = {
        let db = db.clone();
        tokio::spawn(async move { appliquer_les_enrichissements(&db, enrichissements()).await })
    };
    let applied = synchro.await.unwrap();
    let duree = debut.elapsed();

    tokio::time::sleep(Duration::from_millis(700)).await;
    arret.store(true, Ordering::Release);
    let (pire, tours) = sondeur.await.unwrap();
    assert_eq!(applied, 1000, "1000 enrichissements appliqués sans erreur");
    (pire, duree, tours)
}

/// Un seul fil d'exécuteur : le travail synchrone de la synchronisation ne
/// doit pas le tenir. Le sondeur ne reste jamais bloqué plus d'une seconde.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn le_sondeur_n_est_jamais_bloque_plus_d_une_seconde_6702() {
    let _seul = UNE_A_LA_FOIS.lock().await;
    let (pire, duree, tours) = banc_du_sondeur().await;
    assert!(
        pire < Duration::from_secs(1),
        "sondeur bloqué {pire:?} pendant l'application de 1000 enrichissements \
         (application {duree:?}, {tours} tours)"
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
