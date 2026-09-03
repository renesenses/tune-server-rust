//! `execute()` sur PostgreSQL : un INSERT conditionnel qui n'écrit rien
//! n'est pas une erreur, et le compte rendu est le VRAI nombre de lignes
//! (#3248).
//!
//! # Le défaut
//!
//! Les deux implémentations PostgreSQL de `execute()`
//! (`PostgresBackend::execute` et `PgTxHandle::execute`, dans
//! `tune-core/src/db/backend.rs`) AJOUTENT ` RETURNING id` à tout INSERT nu,
//! pour émuler le `last_insert_rowid()` que PostgreSQL n'a pas. La détection
//! est **syntaxique** : préfixe `INSERT INTO`, pas de `RETURNING` déjà là,
//! pas d'`ON CONFLICT` — et rien d'autre. Un `INSERT … SELECT … WHERE NOT
//! EXISTS` la passe donc sans broncher et reçoit lui aussi son `RETURNING
//! id`.
//!
//! Deux mensonges en découlaient :
//!
//! 1. **`fetch_one` exigeait une ligne.** Quand la garde du `WHERE` mord,
//!    l'INSERT n'écrit rien et `RETURNING` ne rend aucune ligne :
//!    `fetch_one` en faisait une `RowNotFound`. L'erreur remontait, et dans
//!    une transaction elle ANNULAIT TOUT. Mesuré : un seul identifiant de
//!    piste manquant vidait la file d'attente en entier — précisément le
//!    dommage que la garde `WHERE EXISTS` existait pour empêcher, obtenu par
//!    une autre porte.
//! 2. **`Ok(1)` en dur.** La branche `returning` rendait toujours `1`, là où
//!    SQLite rend `rows_affected()`. Le même appel rendait un nombre vrai
//!    sur un moteur et un mensonge sur l'autre — et un `INSERT … SELECT` qui
//!    écrivait 3 lignes s'annonçait à 1.
//!
//! # Doctrine d'exécution — lire avant de croire un vert
//!
//! ⚠️ Ce fichier suit la doctrine de `tune-server/tests/pg_routes_serveur.rs`
//! et **NON** celle de `pg_or_skip!` (`tune-core/src/db/postgres_e2e.rs`) :
//! là-bas, une connexion qui ÉCHOUE rend `None` et le test se saute en
//! silence, si bien qu'un banc mal branché s'affiche vert. Ici :
//!
//! - `TUNE_TEST_PG_URL` **absente** ⇒ saut annoncé (le `cargo test` ordinaire
//!   n'a pas de base) ;
//! - `TUNE_TEST_PG_URL` **posée mais injoignable** ⇒ le test TOMBE.
//!
//! [`garde_de_site_3248`] ne dépend d'aucune base : elle relit le source de
//! production et rougit si `fetch_one` ou le `Ok(1)` en dur y reviennent.
//! C'est le filet quand PostgreSQL n'est pas branché — mais ce n'est qu'un
//! filet : la preuve du comportement, ce sont les épreuves sur base réelle.

#[cfg(feature = "postgres")]
mod postgres {
    use std::sync::Arc;

    use tune_core::db::backend::{DbBackend, PostgresBackend};
    use tune_core::db::postgres::PostgresDb;

    /// Suffixe unique à cette épreuve : les tables sont partagées avec les
    /// autres bancs PostgreSQL du dépôt, donc on ne TRUNCATE rien et on ne
    /// compte que nos propres lignes.
    const MARQUE: &str = "f7c250";

    fn url_pg() -> Option<String> {
        std::env::var("TUNE_TEST_PG_URL").ok()
    }

    /// Pas de `ok()?` : une variable POSÉE dont la connexion échoue doit
    /// ROUGIR, jamais sauter.
    async fn backend(url: &str) -> Arc<dyn DbBackend> {
        let db = PostgresDb::connect(url)
            .await
            .unwrap_or_else(|e| panic!("TUNE_TEST_PG_URL posée mais injoignable ({url}) : {e}"));
        Arc::new(PostgresBackend::new(db.pool().clone()))
    }

    fn menage(db: &Arc<dyn DbBackend>, etiquette: &str) {
        db.execute(
            &format!("DELETE FROM artists WHERE name LIKE '{MARQUE}-{etiquette}-%'"),
            &[],
        )
        .unwrap_or_else(|e| panic!("ménage {etiquette} : {e}"));
    }

    fn compte(db: &Arc<dyn DbBackend>, nom: &str) -> i64 {
        let row = db
            .query_one(
                &format!("SELECT COUNT(*) FROM artists WHERE name = '{nom}'"),
                &[],
            )
            .unwrap_or_else(|e| panic!("comptage de {nom} : {e}"))
            .unwrap_or_else(|| panic!("COUNT(*) sans ligne pour {nom}"));
        match row[0] {
            tune_core::db::backend::SqlValue::Int(n) => n,
            ref autre => panic!("COUNT(*) n'est pas un entier : {autre:?}"),
        }
    }

    /// ⭐ L'ÉPREUVE CENTRALE — celle qui décrit le dommage mesuré.
    ///
    /// Dans une transaction : une écriture nominale, PUIS un INSERT
    /// conditionnel dont la garde mord et qui n'écrit donc rien.
    ///
    /// Avant le correctif, le second appel rendait une `RowNotFound` qui
    /// remontait jusqu'à `write_tx` et faisait ROLLBACK : **le témoin écrit
    /// juste avant était perdu**. C'est la forme exacte du vidage de la file
    /// d'attente pour un seul identifiant manquant.
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_3248_insert_conditionnel_sterile_n_annule_pas_la_transaction() {
        let Some(url) = url_pg() else {
            eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
            return;
        };
        let db = backend(&url).await;
        menage(&db, "tx");

        let temoin = format!("{MARQUE}-tx-temoin");
        let jamais = format!("{MARQUE}-tx-jamais");
        let absent = format!("{MARQUE}-tx-absent-de-la-base");

        // `PgTxHandle::execute` ne traduit PAS les `?` : SQL littéral ici.
        let sql_temoin = format!("INSERT INTO artists (name) VALUES ('{temoin}')");
        // La garde MORD : `{absent}` n'est dans aucune ligne, donc le SELECT
        // ne rend rien et l'INSERT n'écrit rien. Zéro ligne est le résultat
        // ATTENDU, pas un incident.
        let sql_sterile = format!(
            "INSERT INTO artists (name) SELECT '{jamais}' \
             WHERE EXISTS (SELECT 1 FROM artists WHERE name = '{absent}')"
        );

        let mut compte_sterile: Option<usize> = None;
        let issue = db.write_tx(&mut |tx| {
            tx.execute(&sql_temoin, &[])?;
            compte_sterile = Some(tx.execute(&sql_sterile, &[])?);
            Ok(())
        });

        issue.unwrap_or_else(|e| {
            panic!(
                "#3248 — la transaction a été ANNULÉE par un INSERT conditionnel \
                 qui n'écrit rien : {e}\n\
                 C'est le défaut : `fetch_one` fait d'une garde qui mord une \
                 erreur, et l'erreur emporte les écritures précédentes."
            )
        });

        assert_eq!(
            compte_sterile,
            Some(0),
            "#3248 — un INSERT conditionnel qui n'écrit rien doit rendre 0, \
             pas le `Ok(1)` en dur"
        );
        assert_eq!(
            compte(&db, &temoin),
            1,
            "#3248 — la transaction a bien rendu Ok, mais le témoin écrit \
             AVANT l'INSERT stérile a disparu : les écritures précédentes ont \
             été annulées"
        );
        assert_eq!(
            compte(&db, &jamais),
            0,
            "l'INSERT gardé ne devait rien écrire — la garde ne mord plus, \
             l'épreuve ne prouve plus rien"
        );

        menage(&db, "tx");
    }

    /// Le compte rendu est le VRAI nombre de lignes écrites : 0, 1, puis N.
    ///
    /// Le cas `N` est celui que le `Ok(1)` en dur ratait le plus salement :
    /// un `INSERT … SELECT` qui écrit trois lignes s'annonçait à une seule.
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_3248_le_nombre_de_lignes_rendu_est_le_vrai() {
        let Some(url) = url_pg() else {
            eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
            return;
        };
        let db = backend(&url).await;
        menage(&db, "compte");

        let sterile = format!(
            "INSERT INTO artists (name) SELECT '{MARQUE}-compte-jamais' \
             WHERE EXISTS (SELECT 1 FROM artists WHERE name = '{MARQUE}-compte-absent')"
        );
        assert_eq!(
            db.execute(&sterile, &[])
                .expect("l'INSERT stérile ne doit pas ÉCHOUER"),
            0,
            "#3248 — zéro ligne écrite doit se dire 0"
        );

        let nominal = format!("INSERT INTO artists (name) VALUES ('{MARQUE}-compte-un')");
        assert_eq!(
            db.execute(&nominal, &[]).expect("INSERT nominal"),
            1,
            "une ligne écrite doit se dire 1"
        );

        // Trois lignes en un seul INSERT … SELECT.
        let multi = format!(
            "INSERT INTO artists (name) \
             SELECT '{MARQUE}-compte-multi-' || g FROM generate_series(1, 3) AS g"
        );
        assert_eq!(
            db.execute(&multi, &[])
                .expect("INSERT … SELECT multi-lignes"),
            3,
            "#3248 — trois lignes écrites doivent se dire 3, pas le `Ok(1)` en dur"
        );

        menage(&db, "compte");
    }

    /// LE TÉMOIN : le chemin nominal n'a pas bougé. Un INSERT ordinaire rend
    /// toujours son identifiant par `last_insert_rowid()`, et un INSERT qui
    /// n'écrit rien LAISSE cette valeur intacte au lieu de la remplacer par
    /// un 0 qui ne désigne aucune ligne.
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_3248_temoin_last_insert_rowid_intact() {
        let Some(url) = url_pg() else {
            eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
            return;
        };
        let db = backend(&url).await;
        menage(&db, "temoin");

        let nominal = format!("INSERT INTO artists (name) VALUES ('{MARQUE}-temoin-un')");
        db.execute(&nominal, &[]).expect("INSERT nominal");
        let id = db.last_insert_rowid();
        assert!(
            id > 0,
            "le chemin nominal ne rend plus d'identifiant : régression sur \
             `RETURNING id`"
        );

        // Un INSERT stérile ne doit pas écraser l'identifiant précédent.
        let sterile = format!(
            "INSERT INTO artists (name) SELECT '{MARQUE}-temoin-jamais' \
             WHERE EXISTS (SELECT 1 FROM artists WHERE name = '{MARQUE}-temoin-absent')"
        );
        assert_eq!(db.execute(&sterile, &[]).expect("INSERT stérile"), 0);
        assert_eq!(
            db.last_insert_rowid(),
            id,
            "#3248 — un INSERT qui n'écrit rien a écrasé `last_insert_rowid` : \
             la valeur doit être CONSERVÉE (un 0 désignerait une ligne qui \
             n'existe pas)"
        );

        menage(&db, "temoin");
    }
}

/// LA CONTRE-ÉPREUVE : le même appel doit rendre le même nombre sur SQLite.
///
/// C'est la moitié qui s'exécute PARTOUT, y compris sans base PostgreSQL.
/// Elle fixe la référence : `rows_affected()`. Si un jour PostgreSQL rend
/// autre chose que ces trois nombres-là, c'est PostgreSQL qui a tort.
#[test]
fn sqlite_3248_contre_epreuve_du_nombre_de_lignes() {
    use std::sync::Arc;

    use tune_core::db::backend::DbBackend;
    use tune_core::db::sqlite::SqliteDb;

    let sqlite = SqliteDb::open_in_memory().expect("SQLite en mémoire");
    sqlite
        .execute_batch("CREATE TABLE artists (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT);")
        .expect("schéma");
    let db: Arc<dyn DbBackend> = Arc::new(sqlite);

    let sterile = "INSERT INTO artists (name) SELECT 'jamais-f7c250' \
                   WHERE EXISTS (SELECT 1 FROM artists WHERE name = 'absent-f7c250')";
    assert_eq!(
        db.execute(sterile, &[]).expect("SQLite : INSERT stérile"),
        0,
        "référence SQLite : zéro ligne écrite se dit 0"
    );

    assert_eq!(
        db.execute("INSERT INTO artists (name) VALUES ('un-f7c250')", &[])
            .expect("SQLite : INSERT nominal"),
        1,
        "référence SQLite : une ligne écrite se dit 1"
    );

    let id = db.last_insert_rowid();
    assert!(id > 0, "SQLite : last_insert_rowid après INSERT nominal");
    assert_eq!(
        db.execute(sterile, &[]).expect("SQLite : INSERT stérile"),
        0
    );
    assert_eq!(
        db.last_insert_rowid(),
        id,
        "SQLite : un INSERT sans effet ne touche pas last_insert_rowid — \
         c'est la sémantique que PostgreSQL doit imiter"
    );

    // Trois lignes en un INSERT … SELECT (SQLite n'a pas generate_series
    // partout : UNION ALL, portable).
    let multi = "INSERT INTO artists (name) \
                 SELECT 'm1-f7c250' UNION ALL SELECT 'm2-f7c250' UNION ALL SELECT 'm3-f7c250'";
    assert_eq!(
        db.execute(multi, &[]).expect("SQLite : INSERT … SELECT"),
        3,
        "référence SQLite : trois lignes écrites se disent 3"
    );
}

/// La garde de site : elle relit le source de PRODUCTION et refuse le retour
/// de `fetch_one` ou du `Ok(1)` en dur dans les branches `returning` de
/// PostgreSQL.
///
/// Elle ne remplace pas les épreuves sur base réelle — elle ne prouve aucun
/// comportement, elle empêche seulement une remise en arrière silencieuse là
/// où PostgreSQL n'est pas branché (le `cargo test` ordinaire de la CI). Les
/// mentions de `fetch_one` en commentaire sont écrites entre accents graves
/// et ne ressemblent donc pas à un appel `.fetch_one(`.
///
/// # Évolution (#3256) — la garde est RESSERRÉE, pas relâchée
///
/// Elle exigeait ZÉRO `.fetch_one(` dans tout le fichier. #3256 y a introduit
/// une sonde de schéma (`table_a_une_colonne_id`, qui demande à PostgreSQL si
/// la table visée porte une colonne `id` avant d'ajouter ` RETURNING id`), et
/// cette sonde emploie `fetch_one` À BON DROIT : son `SELECT EXISTS (…)` rend
/// TOUJOURS exactement une ligne, donc la `RowNotFound` de #3248 y est
/// impossible. Le danger de #3248 est ailleurs — c'est un `fetch_one` sur un
/// `RETURNING`, qui lui peut ne rendre AUCUNE ligne.
///
/// Plutôt que de monter le compte autorisé de 0 à 1 — ce qui ouvrirait la
/// porte à n'importe quel `fetch_one`, y compris le mauvais —, la garde
/// DÉCOUPE le fichier : le corps de la sonde est retiré, et le reste doit
/// toujours être à ZÉRO. Elle interdit donc strictement plus qu'avant.
#[test]
fn garde_de_site_3248() {
    const SOURCE: &str = include_str!("../src/db/backend.rs");

    // Découper le corps de la sonde de schéma de #3256 : c'est le seul
    // endroit du fichier où `fetch_one` est légitime.
    let debut_sonde = SOURCE.find("async fn table_a_une_colonne_id").expect(
        "#3256 — la sonde de schéma `table_a_une_colonne_id` a disparu de \
             `db/backend.rs`. Sans elle, ` RETURNING id` redevient inconditionnel \
             et tout INSERT vers une table sans colonne `id` (`task_runs`, \
             `streaming_auth`) est refusé par PostgreSQL.",
    );
    // Fin de la fonction : la première accolade fermante en colonne 0.
    let fin_sonde = debut_sonde
        + SOURCE[debut_sonde..]
            .find("\n}\n")
            .expect("fin de `table_a_une_colonne_id` introuvable")
        + 3;
    let hors_sonde = format!("{}{}", &SOURCE[..debut_sonde], &SOURCE[fin_sonde..]);

    // La sonde elle-même n'a droit qu'à UN `fetch_one`, pas davantage.
    let dans_sonde = SOURCE[debut_sonde..fin_sonde]
        .matches(".fetch_one(")
        .count();
    assert_eq!(
        dans_sonde, 1,
        "#3256 — la sonde de schéma doit contenir exactement 1 `.fetch_one(` \
         (son `SELECT EXISTS` rend toujours une ligne), trouvé {dans_sonde}."
    );

    let appels_fetch_one = hors_sonde.matches(".fetch_one(").count();
    assert_eq!(
        appels_fetch_one, 0,
        "#3248 — `.fetch_one(` est revenu dans `db/backend.rs` HORS de la sonde de \
         schéma ({appels_fetch_one} appel(s)). \
         Sur un INSERT conditionnel qui n'écrit rien il lève `RowNotFound`, et dans une \
         transaction cette erreur annule TOUT. Utiliser `fetch_all` et compter les lignes."
    );

    // Les deux branches `returning` (pool et transaction) doivent rendre le
    // nombre de lignes RÉELLEMENT rendues par `RETURNING`.
    let comptes_reels = SOURCE.matches("Ok(ids.len())").count();
    assert_eq!(
        comptes_reels, 2,
        "#3248 — les branches `returning` de PostgreSQL doivent rendre `Ok(ids.len())` \
         (2 sites attendus : `PostgresBackend::execute` et `PgTxHandle::execute`), \
         trouvé {comptes_reels}. Un `Ok(1)` en dur rend un nombre vrai sur SQLite et \
         un mensonge sur PostgreSQL."
    );
}
