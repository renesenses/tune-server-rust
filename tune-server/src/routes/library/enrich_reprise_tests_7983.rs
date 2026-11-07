//! #7983 — la passe « Enrichir les métadonnées » reprend où elle en était.
//!
//! Fil 2673, Tune OS, ≈ 540 000 pistes : le reste à faire remontait
//! (≈ 500 000 → 517 000) sans ajout de fichier, et après un redémarrage la
//! passe repartait des premiers `track_id`, les pistes en 503
//! (`mb_details_failed`) remises dans la file. La requête de sélection est
//! EXÉCUTÉE contre une vraie base SQLite migrée, et contre un vrai
//! PostgreSQL quand `TUNE_TEST_PG_URL` est posée (`pg_7983_*`, étape dédiée
//! de `test-postgres.yml`).
use super::*;
use std::sync::Arc;
use tune_core::db::backend::DbBackend;
use tune_core::db::enrich_tentatives::{self, Issue};
use tune_core::db::sqlite::SqliteDb;

const T0: i64 = 1_800_000_000;

/// Cinq pistes à qui il manque le genre, l'année, le label : toutes
/// candidates tant que rien n'est noté. Une instruction par appel : le même
/// remplissage sert à PostgreSQL.
fn remplir(backend: &Arc<dyn DbBackend>) {
    backend
        .execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Keith Jarrett')",
            &[],
        )
        .unwrap();
    backend
        .execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Live', 1)",
            &[],
        )
        .unwrap();
    for id in [5, 3, 1, 4, 2] {
        backend
            .execute(
                &format!(
                    "INSERT INTO tracks (id, title, album_id, artist_id, file_path, source, file_mtime) \
                     VALUES ({id}, 'Piste {id}', 1, 1, '/m/{id}.flac', 'local', 1700000000.25)"
                ),
                &[],
            )
            .unwrap();
    }
}

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    remplir(&backend);
    backend
}

/// La sélection, ligne à ligne : `(track_id, nouvelle tentative ?)`.
fn selection(backend: &Arc<dyn DbBackend>, maintenant: i64) -> Vec<(i64, bool)> {
    backend
        .query_many(&sql_candidats_enrichissement(backend, maintenant), &[])
        .expect("la sélection s'exécute")
        .iter()
        .map(|r| (r[0].as_i64().unwrap(), est_un_reessai(r)))
        .collect()
}

fn candidats(backend: &Arc<dyn DbBackend>, maintenant: i64) -> Vec<i64> {
    selection(backend, maintenant)
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

/// Les restantes : les pistes jamais tentées, celles que compte `total`.
fn restantes(backend: &Arc<dyn DbBackend>, maintenant: i64) -> Vec<i64> {
    selection(backend, maintenant)
        .into_iter()
        .filter(|(_, reessai)| !reessai)
        .map(|(id, _)| id)
        .collect()
}

/// 🔴 LE TÉMOIN DU REDÉMARRAGE. Une passe a traité les pistes 1 à 3 (une
/// faite, une introuvable, une en 503) puis le serveur s'arrête. La sélection
/// suivante ne repart pas des premiers identifiants : seules restent les
/// pistes 4 et 5, et le compte ne remonte pas au-dessus du reste d'avant.
#[test]
fn apres_un_redemarrage_la_passe_saute_les_pistes_deja_tentees() {
    let backend = base();
    assert_eq!(candidats(&backend, T0), vec![1, 2, 3, 4, 5], "ordre par id");
    enrich_tentatives::noter(&backend, 1, Issue::Fait, T0).unwrap();
    enrich_tentatives::noter(&backend, 2, Issue::Introuvable, T0).unwrap();
    enrich_tentatives::noter(&backend, 3, Issue::EchecTemporaire, T0).unwrap();
    let apres = candidats(&backend, T0 + 60);
    assert_eq!(
        apres,
        vec![4, 5],
        "les pistes déjà tentées (faite, introuvable, 503 en attente) ne reviennent pas"
    );
    assert!(apres.len() <= 5 - 3, "le reste à faire ne remonte pas");
}

/// 🔴 LE TÉMOIN DU REDÉMARRAGE APRÈS LE DÉLAI. Une passe dure des jours sur
/// 540 000 pistes ; le premier délai d'un 503 est d'une heure. Relancée deux
/// heures plus tard, la sélection ne doit ni remettre la piste en 503 dans les
/// restantes, ni la passer avant les pistes jamais tentées : elle vient en
/// queue, désignée comme nouvelle tentative.
#[test]
fn apres_le_delai_un_echec_revient_en_queue_hors_des_restantes() {
    let backend = base();
    enrich_tentatives::noter(&backend, 1, Issue::Fait, T0).unwrap();
    enrich_tentatives::noter(&backend, 2, Issue::Introuvable, T0).unwrap();
    enrich_tentatives::noter(&backend, 3, Issue::EchecTemporaire, T0).unwrap();
    let deux_heures_apres = T0 + 2 * 3600;
    assert_eq!(
        selection(&backend, deux_heures_apres),
        vec![(4, false), (5, false), (3, true)],
        "les pistes jamais tentées d'abord, la nouvelle tentative ensuite"
    );
    assert_eq!(
        restantes(&backend, deux_heures_apres),
        vec![4, 5],
        "le reste à faire ne remonte pas au-dessus de celui d'avant le redémarrage"
    );
}

/// La trace d'une piste supprimée part : un identifiant réutilisé n'hérite
/// pas de sa marque.
#[test]
fn la_trace_d_une_piste_supprimee_est_purgee() {
    let backend = base();
    enrich_tentatives::noter(&backend, 1, Issue::Fait, T0).unwrap();
    enrich_tentatives::noter(&backend, 2, Issue::Fait, T0).unwrap();
    backend
        .execute("DELETE FROM tracks WHERE id = 2", &[])
        .unwrap();
    assert_eq!(enrich_tentatives::purger_orphelines(&backend).unwrap(), 1);
    assert_eq!(enrich_tentatives::purger_orphelines(&backend).unwrap(), 0);
}

/// Un échec temporaire revient, mais seulement après son délai — et le
/// délai suivant est plus long : pas de nouvelle tentative en boucle.
#[test]
fn un_echec_temporaire_revient_apres_son_delai_seulement() {
    let backend = base();
    let reessai = enrich_tentatives::noter(&backend, 3, Issue::EchecTemporaire, T0).unwrap();
    assert!(
        !candidats(&backend, reessai - 1).contains(&3),
        "avant le délai"
    );
    assert!(candidats(&backend, reessai).contains(&3), "délai écoulé");
    let second = enrich_tentatives::noter(&backend, 3, Issue::EchecTemporaire, reessai).unwrap();
    assert!(second - reessai > reessai - T0, "le délai grandit");
    assert_eq!(
        enrich_tentatives::compter_a_retenter(&backend, reessai).unwrap(),
        1
    );
}

/// Une piste « faite » ou « introuvable » ne revient qu'avec un fichier
/// modifié (relu par un scan) : la marque vaut pour le fichier vu.
#[test]
fn un_fichier_modifie_redevient_candidat() {
    let backend = base();
    enrich_tentatives::noter(&backend, 1, Issue::Fait, T0).unwrap();
    enrich_tentatives::noter(&backend, 2, Issue::Introuvable, T0).unwrap();
    let loin = T0 + 365 * 24 * 3600;
    assert_eq!(candidats(&backend, loin), vec![3, 4, 5]);
    backend
        .execute(
            "UPDATE tracks SET file_mtime = 1700009999.5 WHERE id = 2",
            &[],
        )
        .unwrap();
    assert_eq!(candidats(&backend, loin), vec![2, 3, 4, 5]);
}

/// 🔴 LE TÉMOIN DU 503. Écrite avec seulement son MBID parce que la requête
/// de détails a échoué, la piste n'est pas « faite » : elle était comptée
/// enrichie, puis remise dans la file à chaque passe.
#[test]
fn des_details_en_echec_ne_valent_pas_une_piste_faite() {
    assert_eq!(
        issue_de_la_tentative(Etape::Ecrite {
            appel_en_echec: true
        }),
        Some(Issue::EchecTemporaire),
        "mb_details_failed (503) doit être un échec temporaire, pas un succès"
    );
    assert_eq!(
        issue_de_la_tentative(Etape::Ecrite {
            appel_en_echec: false
        }),
        Some(Issue::Fait)
    );
    assert_eq!(
        issue_de_la_tentative(Etape::RechercheEnEchec),
        Some(Issue::EchecTemporaire)
    );
    assert_eq!(
        issue_de_la_tentative(Etape::RechercheIntrouvable),
        Some(Issue::Introuvable)
    );
    assert_eq!(
        issue_de_la_tentative(Etape::EcritureEnEchec),
        Some(Issue::EchecTemporaire),
        "une écriture en échec est retentée après un délai, pas à chaque passe"
    );
}

/// Une recherche qui rend 503 ou 429 n'est pas un « introuvable » définitif.
#[test]
fn un_service_indisponible_n_est_pas_introuvable() {
    assert!(reponse_indisponible(StatusCode::SERVICE_UNAVAILABLE));
    assert!(reponse_indisponible(StatusCode::TOO_MANY_REQUESTS));
    assert!(reponse_indisponible(StatusCode::BAD_GATEWAY));
    assert!(!reponse_indisponible(StatusCode::NOT_FOUND));
    assert!(!reponse_indisponible(StatusCode::BAD_REQUEST));
}

/// L'état exposé distingue les restantes et les échecs à retenter, sans
/// changer `enriched` ni `total` (contrat du client web, #2870).
#[test]
fn le_releve_distingue_restantes_et_a_retenter() {
    let r = releve_enrichissement("running", "t", 7, 2, 100, 40, 3, 12);
    assert_eq!(r["enriched"], 7);
    assert_eq!(r["total"], 100);
    assert_eq!(r["examined"], 40);
    assert_eq!(r["remaining"], 60);
    assert_eq!(r["retry_pending"], 3);
    assert_eq!(r["retry_due"], 12);
    assert_eq!(
        releve_enrichissement("done", "t", 0, 0, 5, 9, 0, 0)["remaining"],
        0
    );
}

/// Le parcours complet sur un vrai PostgreSQL : la trace notée (insertion puis
/// mise à jour par `ON CONFLICT`), et la sélection avec une piste faite, une
/// introuvable, un 503 avant et après son délai, un fichier modifié. Sous
/// PostgreSQL, `file_mtime` est un DOUBLE PRECISION : la marque compare son
/// texte, écrit et relu par le même moteur.
#[cfg(feature = "postgres")]
fn eprouver_sur(backend: &Arc<dyn DbBackend>) {
    remplir(backend);
    assert_eq!(candidats(backend, T0), vec![1, 2, 3, 4, 5]);
    enrich_tentatives::noter(backend, 1, Issue::Fait, T0).unwrap();
    enrich_tentatives::noter(backend, 2, Issue::Introuvable, T0).unwrap();
    let reessai = enrich_tentatives::noter(backend, 3, Issue::EchecTemporaire, T0).unwrap();
    assert_eq!(reessai, T0 + 3600);
    assert_eq!(
        selection(backend, reessai - 1),
        vec![(4, false), (5, false)],
        "faite, introuvable et 503 en attente ne reviennent pas"
    );
    assert_eq!(
        enrich_tentatives::compter_a_retenter(backend, reessai - 1).unwrap(),
        1
    );
    assert_eq!(
        selection(backend, reessai),
        vec![(4, false), (5, false), (3, true)],
        "le 503 au délai écoulé revient en queue, hors des restantes"
    );
    // Mise à jour de la ligne : deuxième échec, délai doublé.
    let second = enrich_tentatives::noter(backend, 3, Issue::EchecTemporaire, reessai).unwrap();
    assert_eq!(second, reessai + 7200);
    let echecs = backend
        .query_one_strong(
            "SELECT echecs FROM enrich_tentatives WHERE track_id = 3",
            &[],
        )
        .unwrap()
        .unwrap()[0]
        .as_i64();
    assert_eq!(echecs, Some(2));
    assert_eq!(candidats(backend, second - 1), vec![4, 5]);
    // Un fichier modifié redevient une piste jamais tentée.
    backend
        .execute(
            "UPDATE tracks SET file_mtime = 1700009999.5 WHERE id = 2",
            &[],
        )
        .unwrap();
    assert_eq!(
        selection(backend, second - 1),
        vec![(2, false), (4, false), (5, false)]
    );
    backend
        .execute("DELETE FROM tracks WHERE id = 1", &[])
        .unwrap();
    assert_eq!(enrich_tentatives::purger_orphelines(backend).unwrap(), 1);
}

/// Variable ABSENTE ⇒ saut annoncé ; POSÉE ⇒ l'épreuve tourne dans un schéma
/// à elle, supprimé à la fin. `connect` y pose `ENSURE_TABLES`, donc la table
/// de trace telle que le serveur la crée ; les trois tables lues par la
/// sélection y sont créées aux types du schéma PostgreSQL.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_7983_la_passe_reprend_ou_elle_en_etait_sur_postgresql() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT: TUNE_TEST_PG_URL absent — épreuve PostgreSQL #7983 sautée");
        return;
    };
    const SCHEMA: &str = "essai_7983";
    let racine = tune_core::db::postgres::PostgresDb::connect(&url)
        .await
        .expect("connexion PostgreSQL");
    let admin: Arc<dyn DbBackend> = Arc::new(tune_core::db::backend::PostgresBackend::new(
        racine.pool().clone(),
    ));
    tokio::task::block_in_place(|| {
        admin
            .execute(&format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE"), &[])
            .unwrap();
        admin
            .execute(&format!("CREATE SCHEMA {SCHEMA}"), &[])
            .unwrap();
    });
    let sep = if url.contains('?') { '&' } else { '?' };
    let pg = tune_core::db::postgres::PostgresDb::connect(&format!(
        "{url}{sep}options=-csearch_path%3D{SCHEMA}"
    ))
    .await
    .expect("connexion au schéma d'essai");
    let db: Arc<dyn DbBackend> = Arc::new(tune_core::db::backend::PostgresBackend::new(
        pg.pool().clone(),
    ));
    tokio::task::block_in_place(|| {
        for ddl in [
            "CREATE TABLE artists (id BIGINT PRIMARY KEY, name TEXT, musicbrainz_id TEXT)",
            "CREATE TABLE albums (id BIGINT PRIMARY KEY, title TEXT, artist_id BIGINT)",
            "CREATE TABLE tracks (id BIGINT PRIMARY KEY, title TEXT, album_id BIGINT, \
             artist_id BIGINT, file_path TEXT, cue_media_path TEXT, source TEXT, \
             file_mtime DOUBLE PRECISION, musicbrainz_recording_id TEXT, genre TEXT, \
             year INTEGER, label TEXT, composer TEXT)",
        ] {
            db.execute(ddl, &[]).unwrap();
        }
        eprouver_sur(&db);
    });
    pg.pool().close().await;
    tokio::task::block_in_place(|| {
        admin
            .execute(&format!("DROP SCHEMA {SCHEMA} CASCADE"), &[])
            .unwrap();
    });
    racine.pool().close().await;
}
