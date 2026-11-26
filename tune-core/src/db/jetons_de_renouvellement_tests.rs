//! Témoins du dépôt des jetons de renouvellement (rc14), sur SQLite. Les
//! mêmes épreuves tournent sur PostgreSQL dans
//! `jetons_de_renouvellement_pg_tests` (feature `postgres`).

use std::sync::Arc;

use super::*;
use crate::db::backend::DbBackend;
use crate::db::migrations;
use crate::db::sqlite::SqliteDb;

const T0: i64 = 1_800_000_000;

fn base() -> (Arc<dyn DbBackend>, i64) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let id = db
        .execute_returning_id(
            "INSERT INTO profiles (username, display_name, password_hash_v2) VALUES (?, ?, ?)",
            &[&"ana" as &dyn ToSqlValue, &"ana", &"$argon2id$hachage-1"],
        )
        .unwrap();
    (db, id)
}

#[test]
fn epreuves_communes_sqlite() {
    let (db, id) = base();
    epreuves_communes(db, id);
}

/// Les épreuves du dépôt, communes aux deux moteurs.
pub(crate) fn epreuves_communes(db: Arc<dyn DbBackend>, id: i64) {
    rotation(&db, id);
    reutilisation_revoque_la_famille(&db, id);
    course_dans_le_delai_de_grace(&db, id);
    expiration_a_30_jours_sans_usage(&db, id);
    fenetre_glissante(&db, id);
    mot_de_passe_change(&db, id);
    revocations(&db, id);
    jamais_de_jeton_en_clair(&db, id);
}

fn rotation(db: &Arc<dyn DbBackend>, id: i64) {
    let repo = JetonsDeRenouvellementRepo::with_backend(db.clone());
    let a = repo.ouvrir(id, Some("Tune iOS"), T0).unwrap();
    assert!(a.jeton.starts_with(PREFIXE_JETON));
    assert_eq!(a.expire_le, T0 + DUREE_RENOUVELLEMENT);
    let b = repo.renouveler(&a.jeton, T0 + 3600).unwrap();
    assert_ne!(a.jeton, b.jeton, "le jeton tourne");
    assert_eq!(a.famille, b.famille, "même famille");
    assert_eq!(b.profile_id, id);
    // L'ancien ne sert plus (dans le délai de grâce : refusé sans révoquer).
    assert_eq!(
        repo.renouveler(&a.jeton, T0 + 3601).unwrap_err(),
        Refus::DejaTourne
    );
    assert!(repo.famille_active(&b.famille, T0 + 3601));
    // Le nouveau, lui, tourne à son tour.
    let c = repo.renouveler(&b.jeton, T0 + 7200).unwrap();
    assert_eq!(c.famille, a.famille);
}

fn reutilisation_revoque_la_famille(db: &Arc<dyn DbBackend>, id: i64) {
    let repo = JetonsDeRenouvellementRepo::with_backend(db.clone());
    let vole = repo.ouvrir(id, None, T0).unwrap();
    let legitime = repo.renouveler(&vole.jeton, T0 + 10).unwrap();
    // Le voleur rejoue l'ancien jeton une minute plus tard.
    assert_eq!(
        repo.renouveler(&vole.jeton, T0 + 10 + DELAI_DE_GRACE + 1)
            .unwrap_err(),
        Refus::Reutilise
    );
    assert!(!repo.famille_active(&vole.famille, T0 + 100));
    // Le jeton légitime de la famille est mort aussi.
    assert_eq!(
        repo.renouveler(&legitime.jeton, T0 + 120).unwrap_err(),
        Refus::Revoque
    );
}

fn course_dans_le_delai_de_grace(db: &Arc<dyn DbBackend>, id: i64) {
    let repo = JetonsDeRenouvellementRepo::with_backend(db.clone());
    let a = repo.ouvrir(id, None, T0).unwrap();
    let b = repo.renouveler(&a.jeton, T0 + 5).unwrap();
    assert_eq!(
        repo.renouveler(&a.jeton, T0 + 5 + DELAI_DE_GRACE)
            .unwrap_err(),
        Refus::DejaTourne
    );
    // Rien n'est révoqué : le jeton tourné sert toujours.
    assert!(repo.renouveler(&b.jeton, T0 + 60).is_ok());
}

fn expiration_a_30_jours_sans_usage(db: &Arc<dyn DbBackend>, id: i64) {
    let repo = JetonsDeRenouvellementRepo::with_backend(db.clone());
    let a = repo.ouvrir(id, None, T0).unwrap();
    assert!(repo.famille_active(&a.famille, T0 + DUREE_RENOUVELLEMENT - 1));
    assert_eq!(
        repo.renouveler(&a.jeton, T0 + DUREE_RENOUVELLEMENT)
            .unwrap_err(),
        Refus::Expire
    );
    assert!(!repo.famille_active(&a.famille, T0 + DUREE_RENOUVELLEMENT));
}

fn fenetre_glissante(db: &Arc<dyn DbBackend>, id: i64) {
    let repo = JetonsDeRenouvellementRepo::with_backend(db.clone());
    // Un renouvellement tous les 20 jours : la session dure bien au-delà de
    // 30 jours.
    let mut j = repo.ouvrir(id, None, T0).unwrap();
    for n in 1..=4 {
        j = repo.renouveler(&j.jeton, T0 + n * 20 * 86400).unwrap();
    }
    assert_eq!(j.expire_le, T0 + 80 * 86400 + DUREE_RENOUVELLEMENT);
}

fn mot_de_passe_change(db: &Arc<dyn DbBackend>, id: i64) {
    let repo = JetonsDeRenouvellementRepo::with_backend(db.clone());
    let a = repo.ouvrir(id, None, T0).unwrap();
    db.execute(
        "UPDATE profiles SET password_hash_v2 = ? WHERE id = ?",
        &[&"$argon2id$hachage-2" as &dyn ToSqlValue, &id],
    )
    .unwrap();
    assert_eq!(
        repo.renouveler(&a.jeton, T0 + 60).unwrap_err(),
        Refus::Revoque
    );
    assert!(!repo.famille_active(&a.famille, T0 + 60));
    // Une connexion avec le nouveau mot de passe ouvre une famille neuve.
    let b = repo.ouvrir(id, None, T0 + 120).unwrap();
    assert!(repo.renouveler(&b.jeton, T0 + 180).is_ok());
}

fn revocations(db: &Arc<dyn DbBackend>, id: i64) {
    let repo = JetonsDeRenouvellementRepo::with_backend(db.clone());
    // Déconnexion : la famille du jeton présenté.
    let a = repo.ouvrir(id, None, T0).unwrap();
    let autre = repo.ouvrir(id, None, T0).unwrap();
    assert_eq!(
        repo.session_du_jeton(&a.jeton),
        Some((a.famille.clone(), id))
    );
    assert!(repo.revoquer_famille(&a.famille, T0 + 1) >= 1);
    assert_eq!(
        repo.renouveler(&a.jeton, T0 + 2).unwrap_err(),
        Refus::Revoque
    );
    assert!(
        repo.famille_active(&autre.famille, T0 + 2),
        "l'autre appareil reste"
    );
    // Se déconnecter partout.
    repo.revoquer_profil(id, T0 + 3);
    assert_eq!(
        repo.renouveler(&autre.jeton, T0 + 4).unwrap_err(),
        Refus::Revoque
    );
    // Secrets renouvelés : tout le monde.
    let c = repo.ouvrir(id, None, T0 + 5).unwrap();
    repo.revoquer_tout(T0 + 6);
    assert_eq!(
        repo.renouveler(&c.jeton, T0 + 7).unwrap_err(),
        Refus::Revoque
    );
    // Jeton inconnu ou mal formé.
    assert_eq!(
        repo.renouveler("tune_rt_0000", T0).unwrap_err(),
        Refus::Inconnu
    );
    assert_eq!(repo.renouveler("", T0).unwrap_err(), Refus::Inconnu);
    // Purge des expirés.
    assert!(repo.purger(T0 + 2 * DUREE_RENOUVELLEMENT) >= 1);
}

fn jamais_de_jeton_en_clair(db: &Arc<dyn DbBackend>, id: i64) {
    let repo = JetonsDeRenouvellementRepo::with_backend(db.clone());
    let a = repo.ouvrir(id, None, T0).unwrap();
    let b = repo.renouveler(&a.jeton, T0 + 1).unwrap();
    let lignes = db
        .query_many("SELECT * FROM jetons_de_renouvellement", &[])
        .unwrap();
    assert!(!lignes.is_empty());
    for ligne in &lignes {
        for valeur in ligne {
            if let Some(texte) = valeur.as_string() {
                for secret in [&a.jeton, &b.jeton] {
                    assert!(
                        !texte.contains(secret.as_str())
                            && !texte.contains(&secret[PREFIXE_JETON.len()..]),
                        "un jeton en clair est stocké : {texte}"
                    );
                }
            }
        }
    }
    let stockee = db
        .query_one(
            "SELECT empreinte_sha256 FROM jetons_de_renouvellement WHERE famille = ? AND remplace_le IS NULL",
            &[&b.famille as &dyn ToSqlValue],
        )
        .unwrap()
        .unwrap();
    assert_eq!(stockee[0].as_string(), Some(empreinte(&b.jeton)));
}

/// Les mêmes épreuves sur PostgreSQL, table créée par le script 125.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn epreuves_communes_postgres() {
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
    let nom = format!("rt-{}", uuid::Uuid::new_v4().simple());
    let id = db
        .execute_returning_id(
            "INSERT INTO profiles (username, display_name, password_hash_v2) VALUES (?, ?, ?)",
            &[&nom as &dyn ToSqlValue, &nom, &"$argon2id$hachage-1"],
        )
        .unwrap();
    epreuves_communes(db.clone(), id);
    db.execute(
        "DELETE FROM jetons_de_renouvellement WHERE profile_id = ?",
        &[&id as &dyn ToSqlValue],
    )
    .unwrap();
    db.execute(
        "DELETE FROM profiles WHERE id = ?",
        &[&id as &dyn ToSqlValue],
    )
    .unwrap();
}
