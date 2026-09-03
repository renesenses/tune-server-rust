//! `execute()` sur PostgreSQL : ` RETURNING id` n'est ajouté qu'aux tables qui
//! PORTENT une colonne `id` (#3256).
//!
//! # Le défaut
//!
//! Les deux implémentations PostgreSQL de `execute()`
//! (`PostgresBackend::execute` et `PgTxHandle::execute`, dans
//! `tune-core/src/db/backend.rs`) ajoutaient ` RETURNING id` à tout INSERT nu,
//! pour émuler le `last_insert_rowid()` que PostgreSQL n'a pas. La détection
//! était **syntaxique** : préfixe `INSERT INTO`, pas de `RETURNING` déjà là,
//! pas d'`ON CONFLICT` — et rien d'autre.
//!
//! Elle l'ajoutait donc aussi aux tables qui n'ont PAS de colonne `id`, et
//! PostgreSQL refusait la requête entière :
//!
//! ```text
//! ERROR: column "id" does not exist
//! ```
//!
//! Sur ces tables l'écriture échouait **systématiquement**, pas seulement
//! quand elle n'écrivait rien. Le correctif de #3248 (`fetch_all` au lieu de
//! `fetch_one`) n'y changeait rien : la requête est refusée par PostgreSQL
//! AVANT tout résultat.
//!
//! Deux sites de production le payaient :
//!
//! - `TaskRunRepo::ouvrir` (`db/task_run_repo.rs`) — la clef primaire de
//!   `task_runs` est `(boot_id, task, seq)`, PAS un `id`, et c'est un choix
//!   écrit dans la migration `040_task_runs.sql`. À CHAQUE ouverture de passe
//!   automatisée, l'insertion échouait, le témoin était rendu inerte et rien
//!   n'était journalisé : le registre des passes n'a JAMAIS fonctionné sur
//!   PostgreSQL depuis sa création.
//! - `ServicesManager::save_token` (`services_manager.rs`) — `streaming_auth`
//!   a `service` pour clef primaire. L'enregistrement du jeton d'un service de
//!   streaming remontait une erreur, donc n'était jamais persisté.
//!
//! # Ce que le correctif change, et ce qu'il NE change pas
//!
//! La clause n'est ajoutée que si la table porte réellement une colonne `id`,
//! question posée au SCHÉMA (`to_regclass` + `pg_attribute`) et mémorisée par
//! nom de table. Sur une table à `id` — donc pour TOUS les appelants de
//! `execute_returning_id` — la SQL produite est identique à l'octet près.
//!
//! ⚠️ Ce correctif réveille un chemin de SUCCÈS : là où l'appel rendait `Err`,
//! il rend maintenant `Ok(1)` et écrit vraiment. C'est l'objet du ticket, mais
//! c'est aussi ce qui rend l'épreuve centrale nécessaire — un `Ok` qui
//! n'écrirait rien serait un vert contre rien. Chaque écriture est donc RELUE.
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
//! [`garde_de_site_3256`] et [`sqlite_3256_contre_epreuve`] ne dépendent
//! d'aucune base et s'exécutent PARTOUT. Elles ne prouvent pas le
//! comportement PostgreSQL — elles empêchent une remise en arrière silencieuse
//! là où PostgreSQL n'est pas branché.

#[cfg(feature = "postgres")]
mod postgres {
    use std::sync::Arc;

    use tune_core::db::backend::{DbBackend, PostgresBackend, SqlValue, ToSqlValue};
    use tune_core::db::postgres::PostgresDb;

    /// Suffixe unique à cette épreuve : les tables sont partagées avec les
    /// autres bancs PostgreSQL du dépôt, donc on ne TRUNCATE rien et on ne
    /// compte que nos propres lignes.
    const MARQUE: &str = "a2c869";

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

    fn entier(db: &Arc<dyn DbBackend>, sql: &str) -> i64 {
        let row = db
            .query_one(sql, &[])
            .unwrap_or_else(|e| panic!("lecture « {sql} » : {e}"))
            .unwrap_or_else(|| panic!("« {sql} » n'a rendu aucune ligne"));
        match row[0] {
            SqlValue::Int(n) => n,
            ref autre => panic!("« {sql} » n'a pas rendu un entier : {autre:?}"),
        }
    }

    fn menage(db: &Arc<dyn DbBackend>, etiquette: &str) {
        for sql in [
            format!("DELETE FROM task_runs WHERE boot_id LIKE '{MARQUE}-{etiquette}-%'"),
            format!("DELETE FROM streaming_auth WHERE service LIKE '{MARQUE}-{etiquette}-%'"),
            format!("DELETE FROM artists WHERE name LIKE '{MARQUE}-{etiquette}-%'"),
        ] {
            db.execute(&sql, &[])
                .unwrap_or_else(|e| panic!("ménage {etiquette} : {e}"));
        }
    }

    /// Le décor du défaut : ces tables n'ont VRAIMENT pas de colonne `id`.
    ///
    /// Si un jour quelqu'un « répare » le schéma en ajoutant un `id` à
    /// `task_runs`, cette épreuve rougit — et c'est voulu : sa clef composite
    /// `(boot_id, task, seq)` est un choix documenté dans
    /// `040_task_runs.sql`, pas un oubli. Le défaut est dans le code, pas dans
    /// la table.
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_3256_les_tables_visees_n_ont_pas_de_colonne_id() {
        let Some(url) = url_pg() else {
            eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
            return;
        };
        let db = backend(&url).await;

        for table in ["task_runs", "streaming_auth"] {
            assert_eq!(
                entier(
                    &db,
                    &format!(
                        "SELECT COUNT(*) FROM information_schema.columns \
                         WHERE table_name = '{table}' AND column_name = 'id'"
                    )
                ),
                0,
                "{table} a gagné une colonne `id` : le schéma a été « réparé » \
                 à la place du code. La clef composite est un choix."
            );
        }

        // Le témoin du décor : `artists`, elle, en a une.
        assert_eq!(
            entier(
                &db,
                "SELECT COUNT(*) FROM information_schema.columns \
                 WHERE table_name = 'artists' AND column_name = 'id'"
            ),
            1,
            "témoin de décor : `artists` doit porter une colonne `id`, sinon \
             l'épreuve du témoin ne prouve rien"
        );
    }

    /// ⭐ L'ÉPREUVE CENTRALE — celle qui échoue sans le correctif.
    ///
    /// Un INSERT nu dans une table SANS colonne `id`, par le chemin du pool.
    /// Avant le correctif : `ERROR: column "id" does not exist`.
    ///
    /// L'`Ok` ne suffit pas : la ligne est RELUE. Un correctif qui rendrait
    /// `Ok` sans écrire serait pire que le défaut.
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_3256_insert_dans_une_table_sans_colonne_id_reussit() {
        let Some(url) = url_pg() else {
            eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
            return;
        };
        let db = backend(&url).await;
        menage(&db, "pool");

        // ── `task_runs` : la forme EXACTE de `TaskRunRepo::ouvrir`. ────────
        let boot = format!("{MARQUE}-pool-boot");
        let tache = format!("{MARQUE}-pool-tache");
        let seq = 1i64;
        let debut = "2026-09-03T04:00:00Z";
        let etat = "en_cours";
        let params: [&dyn ToSqlValue; 5] = [&boot, &tache, &seq, &debut, &etat];
        let ecrites = db
            .execute(
                "INSERT INTO task_runs (boot_id, task, seq, started_at, outcome) \
                 VALUES (?, ?, ?, ?, ?)",
                &params,
            )
            .unwrap_or_else(|e| {
                panic!(
                    "#3256 — l'INSERT dans `task_runs` a été REFUSÉ : {e}\n\
                     C'est le défaut : ` RETURNING id` est ajouté à une table \
                     dont la clef primaire est (boot_id, task, seq) et qui n'a \
                     aucune colonne `id`. Le registre des passes automatisées \
                     n'écrit alors JAMAIS rien sur PostgreSQL."
                )
            });
        assert_eq!(ecrites, 1, "une ligne écrite doit se dire 1");
        assert_eq!(
            entier(
                &db,
                &format!("SELECT COUNT(*) FROM task_runs WHERE boot_id = '{boot}'")
            ),
            1,
            "l'appel a rendu Ok mais la ligne n'est pas dans la table : \
             un succès qui n'écrit rien est pire que l'échec qu'il remplace"
        );

        // ── `streaming_auth` : la forme EXACTE de `save_token`. ────────────
        let service = format!("{MARQUE}-pool-service");
        let jeton = r#"{"fields":{}}"#;
        let params: [&dyn ToSqlValue; 2] = [&service, &jeton];
        db.execute(
            "INSERT INTO streaming_auth (service, token_data) VALUES (?, ?)",
            &params,
        )
        .unwrap_or_else(|e| {
            panic!(
                "#3256 — l'INSERT dans `streaming_auth` a été REFUSÉ : {e}\n\
                 Le jeton d'un service de streaming n'est alors jamais persisté."
            )
        });
        assert_eq!(
            entier(
                &db,
                &format!("SELECT COUNT(*) FROM streaming_auth WHERE service = '{service}'")
            ),
            1,
            "le jeton a été annoncé écrit et n'est pas là"
        );

        menage(&db, "pool");
    }

    /// Le même défaut par l'AUTRE chemin : `PgTxHandle::execute`, dans une
    /// transaction. C'est là que l'échec coûtait le plus cher — l'erreur
    /// remonte à `write_tx`, qui ROLLBACK, et emporte les écritures déjà
    /// faites dans la même transaction.
    ///
    /// Le témoin écrit AVANT prouve que la transaction a bien été validée.
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_3256_insert_sans_colonne_id_dans_une_transaction() {
        let Some(url) = url_pg() else {
            eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
            return;
        };
        let db = backend(&url).await;
        menage(&db, "tx");

        let temoin = format!("{MARQUE}-tx-temoin");
        let boot = format!("{MARQUE}-tx-boot");
        // `PgTxHandle::execute` ne traduit PAS les `?` : SQL littéral ici.
        let sql_temoin = format!("INSERT INTO artists (name) VALUES ('{temoin}')");
        let sql_registre = format!(
            "INSERT INTO task_runs (boot_id, task, seq, started_at, outcome) \
             VALUES ('{boot}', '{MARQUE}-tx-tache', 1, '2026-09-03T04:00:00Z', 'en_cours')"
        );

        let mut id_temoin = 0i64;
        let mut ecrites_registre = 0usize;
        let issue = db.write_tx(&mut |tx| {
            tx.execute(&sql_temoin, &[])?;
            // Le témoin garde son identifiant : la table `artists` A un `id`,
            // donc ` RETURNING id` doit toujours lui être ajouté.
            id_temoin = tx.last_insert_rowid();
            ecrites_registre = tx.execute(&sql_registre, &[])?;
            Ok(())
        });

        issue.unwrap_or_else(|e| {
            panic!(
                "#3256 — la transaction a été ANNULÉE par un INSERT dans une \
                 table sans colonne `id` : {e}\n\
                 L'erreur emporte les écritures précédentes de la même \
                 transaction."
            )
        });

        assert_eq!(ecrites_registre, 1, "une ligne écrite doit se dire 1");
        assert!(
            id_temoin > 0,
            "TÉMOIN — `artists` a une colonne `id` : `last_insert_rowid()` \
             doit toujours rendre l'identifiant du nouvel artiste. Un 0 ici \
             veut dire que le correctif a retiré la clause LÀ OÙ ELLE ÉTAIT \
             LÉGITIME, et tous les `execute_returning_id` du dépôt sont cassés."
        );
        assert_eq!(
            entier(
                &db,
                &format!("SELECT COUNT(*) FROM task_runs WHERE boot_id = '{boot}'")
            ),
            1,
            "la ligne de registre n'a pas survécu à la validation"
        );
        assert_eq!(
            entier(
                &db,
                &format!("SELECT COUNT(*) FROM artists WHERE id = {id_temoin}")
            ),
            1,
            "`last_insert_rowid()` a rendu un identifiant qui ne désigne \
             aucune ligne"
        );

        menage(&db, "tx");
    }

    /// ⭐ LE TÉMOIN — le chemin nominal n'a pas bougé d'un octet.
    ///
    /// Sur une table AVEC `id`, l'INSERT rend toujours son identifiant par
    /// `last_insert_rowid()`, et `execute_returning_id()` — le seul
    /// consommateur de production de cette émulation, partagé par une
    /// trentaine d'appelants — rend toujours l'identifiant de la ligne qu'il
    /// vient d'écrire.
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_3256_temoin_les_tables_a_id_rendent_toujours_leur_identifiant() {
        let Some(url) = url_pg() else {
            eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
            return;
        };
        let db = backend(&url).await;
        menage(&db, "temoin");

        // 1. `execute()` + `last_insert_rowid()` — le couple historique.
        let un = format!("{MARQUE}-temoin-un");
        let params: [&dyn ToSqlValue; 1] = [&un];
        assert_eq!(
            db.execute("INSERT INTO artists (name) VALUES (?)", &params)
                .expect("INSERT nominal sur une table à `id`"),
            1
        );
        let id = db.last_insert_rowid();
        assert!(
            id > 0,
            "le chemin nominal ne rend plus d'identifiant : régression sur \
             `RETURNING id` là où la colonne EXISTE"
        );
        assert_eq!(
            entier(
                &db,
                &format!("SELECT COUNT(*) FROM artists WHERE id = {id} AND name = '{un}'")
            ),
            1,
            "`last_insert_rowid()` ne désigne pas la ligne qu'on vient d'écrire"
        );

        // 2. `execute_returning_id()` — le chemin RÉELLEMENT emprunté par les
        //    `create()` de tous les dépôts (via `write_tx` + `PgTxHandle`).
        let deux = format!("{MARQUE}-temoin-deux");
        let sql = format!("INSERT INTO artists (name) VALUES ('{deux}')");
        let id2 = db
            .execute_returning_id(&sql, &[])
            .expect("execute_returning_id sur une table à `id`");
        assert!(id2 > 0, "`execute_returning_id` ne rend plus d'identifiant");
        assert_ne!(id2, id, "deux INSERT doivent rendre deux identifiants");
        assert_eq!(
            entier(
                &db,
                &format!("SELECT COUNT(*) FROM artists WHERE id = {id2} AND name = '{deux}'")
            ),
            1,
            "`execute_returning_id` a rendu un identifiant qui ne désigne pas \
             sa propre ligne"
        );

        // 3. Un INSERT conditionnel stérile reste à 0 et ne touche pas
        //    l'identifiant : le correctif de #3248 tient toujours.
        let sterile = format!(
            "INSERT INTO artists (name) SELECT '{MARQUE}-temoin-jamais' \
             WHERE EXISTS (SELECT 1 FROM artists WHERE name = '{MARQUE}-temoin-absent')"
        );
        assert_eq!(
            db.execute(&sterile, &[]).expect("INSERT stérile"),
            0,
            "#3248 — zéro ligne écrite doit se dire 0"
        );
        assert_eq!(
            db.last_insert_rowid(),
            id2,
            "#3248 — un INSERT qui n'écrit rien a écrasé `last_insert_rowid`"
        );

        menage(&db, "temoin");
    }
}

/// LA CONTRE-ÉPREUVE : le même appel se comporte pareil sur SQLite.
///
/// C'est la moitié qui s'exécute PARTOUT, y compris sans base PostgreSQL. Elle
/// fixe la référence que PostgreSQL doit imiter : un INSERT dans une table
/// sans colonne `id` réussit et écrit sa ligne, un INSERT dans une table à
/// `id` rend son identifiant.
///
/// Elle relit aussi le SCHÉMA SQLite pour prouver que `task_runs` y est
/// id-less de la même façon : les deux moteurs décrivent la même table, donc
/// la divergence était bien dans le code PostgreSQL et nulle part ailleurs.
#[test]
fn sqlite_3256_contre_epreuve() {
    use std::sync::Arc;

    use tune_core::db::backend::{DbBackend, SqlValue, ToSqlValue};
    use tune_core::db::migrations;
    use tune_core::db::sqlite::SqliteDb;

    let sqlite = SqliteDb::open_in_memory().expect("SQLite en mémoire");
    sqlite.init_schema().expect("schéma initial");
    migrations::run_migrations(&sqlite).expect("migrations SQLite");
    let db: Arc<dyn DbBackend> = Arc::new(sqlite);

    let entier = |sql: &str| -> i64 {
        let row = db
            .query_one(sql, &[])
            .unwrap_or_else(|e| panic!("lecture « {sql} » : {e}"))
            .unwrap_or_else(|| panic!("« {sql} » n'a rendu aucune ligne"));
        match row[0] {
            SqlValue::Int(n) => n,
            ref autre => panic!("« {sql} » n'a pas rendu un entier : {autre:?}"),
        }
    };

    // Le décor : côté SQLite AUSSI, `task_runs` n'a pas de colonne `id`.
    assert_eq!(
        entier("SELECT COUNT(*) FROM pragma_table_info('task_runs') WHERE name = 'id'"),
        0,
        "`task_runs` a gagné une colonne `id` côté SQLite : les deux moteurs \
         ne décrivent plus la même table, et la contre-épreuve ne compare plus \
         rien. La clef (boot_id, task, seq) est un choix documenté."
    );
    assert_eq!(
        entier("SELECT COUNT(*) FROM pragma_table_info('artists') WHERE name = 'id'"),
        1,
        "témoin de décor : `artists` doit porter une colonne `id`"
    );

    // 1. Table SANS `id` — réussit et écrit.
    let boot = "sqlite-a2c869-boot";
    let seq = 1i64;
    let params: [&dyn ToSqlValue; 3] = [&boot, &"tache-a2c869", &seq];
    assert_eq!(
        db.execute(
            "INSERT INTO task_runs (boot_id, task, seq, started_at, outcome) \
             VALUES (?, ?, ?, '2026-09-03T04:00:00Z', 'en_cours')",
            &params,
        )
        .expect("SQLite : INSERT dans une table sans colonne `id`"),
        1,
        "référence SQLite : une ligne écrite se dit 1"
    );
    assert_eq!(
        entier(&format!(
            "SELECT COUNT(*) FROM task_runs WHERE boot_id = '{boot}'"
        )),
        1,
        "référence SQLite : la ligne est bien dans la table"
    );

    // 2. Table AVEC `id` — rend son identifiant.
    let nom = "artiste-a2c869";
    let params: [&dyn ToSqlValue; 1] = [&nom];
    assert_eq!(
        db.execute("INSERT INTO artists (name) VALUES (?)", &params)
            .expect("SQLite : INSERT nominal"),
        1
    );
    let id = db.last_insert_rowid();
    assert!(id > 0, "SQLite : last_insert_rowid après INSERT nominal");
    assert_eq!(
        entier(&format!(
            "SELECT COUNT(*) FROM artists WHERE id = {id} AND name = '{nom}'"
        )),
        1,
        "SQLite : l'identifiant rendu désigne bien la ligne écrite"
    );

    // 3. Une seconde écriture dans la table sans `id` réussit aussi, et le
    //    couple (boot_id, task, seq) numérote les lignes tout seul.
    //
    //    ⚠️ MESURÉ, et les deux moteurs DIVERGENT ici : SQLite donne un
    //    `rowid` implicite à toute table qui n'est pas `WITHOUT ROWID`, donc
    //    `last_insert_rowid()` avance après cet INSERT (il vaut 2, le rowid de
    //    la ligne de registre) ; PostgreSQL, lui, n'a rien à rendre et le
    //    correctif laisse la valeur précédente en place. Aucune des deux
    //    valeurs ne désigne quoi que ce soit d'utile, et AUCUN appelant du
    //    dépôt ne lit `last_insert_rowid()` après avoir écrit dans une table
    //    sans colonne `id` — le recensement de #3256 le vérifie site par site.
    //    Ce qui doit être identique entre les moteurs, et qui l'est, c'est
    //    que l'écriture RÉUSSIT et que la ligne est là.
    let params: [&dyn ToSqlValue; 3] = [&boot, &"tache-a2c869", &2i64];
    assert_eq!(
        db.execute(
            "INSERT INTO task_runs (boot_id, task, seq, started_at, outcome) \
             VALUES (?, ?, ?, '2026-09-03T04:00:01Z', 'en_cours')",
            &params,
        )
        .expect("SQLite : seconde ligne de registre"),
        1
    );
    assert_eq!(
        entier(&format!(
            "SELECT COUNT(*) FROM task_runs WHERE boot_id = '{boot}'"
        )),
        2,
        "référence SQLite : deux passes journalisées, deux lignes"
    );
}

/// La garde de site : elle relit le source de PRODUCTION et refuse le retour
/// de l'ajout INCONDITIONNEL de ` RETURNING id`.
///
/// Elle ne prouve aucun comportement — elle empêche une remise en arrière
/// silencieuse là où PostgreSQL n'est pas branché (le `cargo test` ordinaire
/// de la CI, qui n'a pas de base). La preuve du comportement, ce sont les
/// épreuves du module `postgres` ci-dessus, sur base réelle.
#[test]
fn garde_de_site_3256() {
    const SOURCE: &str = include_str!("../src/db/backend.rs");

    // La sonde existe, une seule fois, et elle est GÉNÉRIQUE sur l'exécuteur :
    // le chemin du pool et celui de la transaction posent la même question par
    // le même code (`garde_de_site_3248` s'appuie sur ce découpage).
    assert_eq!(
        SOURCE.matches("async fn table_a_une_colonne_id").count(),
        1,
        "#3256 — la sonde de schéma `table_a_une_colonne_id` doit être définie \
         une fois et une seule dans `db/backend.rs`."
    );

    // Et les deux sites (`PostgresBackend::execute` et `PgTxHandle::execute`)
    // doivent l'APPELER avant de décider. `table_a_une_colonne_id(` ne compte
    // que les appels : la définition porte ses paramètres génériques
    // (`<'e, E>`) entre le nom et la parenthèse.
    let sondes = SOURCE.matches("table_a_une_colonne_id(").count();
    assert_eq!(
        sondes, 2,
        "#3256 — les deux `execute()` PostgreSQL doivent demander au SCHÉMA si \
         la table porte une colonne `id` avant d'ajouter ` RETURNING id` \
         (2 appels attendus : `PostgresBackend::execute` et \
         `PgTxHandle::execute`), trouvé {sondes}. Sans cette sonde, un INSERT \
         dans `task_runs` ou `streaming_auth` est refusé par PostgreSQL."
    );

    // Et l'ajout doit être SOUS la garde, pas à côté.
    let gardes = SOURCE.matches("if a_un_id {").count();
    assert_eq!(
        gardes, 2,
        "#3256 — l'ajout de ` RETURNING id` doit être gardé par `if a_un_id` \
         sur les deux sites, trouvé {gardes}. Un ajout inconditionnel casse \
         toute écriture vers une table sans colonne `id`."
    );

    // Ceinture : plus aucun `push_str(\" RETURNING id\")` hors de ces gardes.
    let ajouts = SOURCE.matches("push_str(\" RETURNING id\")").count();
    assert_eq!(
        ajouts, 2,
        "#3256 — {ajouts} ajout(s) de ` RETURNING id` dans `db/backend.rs` \
         alors que seuls les 2 sites gardés sont attendus. Un troisième site \
         ne serait couvert par aucune épreuve."
    );
}
