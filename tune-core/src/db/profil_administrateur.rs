//! Le profil administrateur d'origine (profil n° 1) : `admin` / `Admin`.
//!
//! Décision de Bertrand du 26/11/2026 (rc14) : le profil créé d'office à
//! l'installation ne s'appelle plus `default` / `Default`, mais `admin` /
//! `Admin`. La migration 6 (SQLite) et la 005 (PostgreSQL) le sèment toujours
//! en `default` — elles sont appliquées chez tous les utilisateurs et ne se
//! modifient pas : c'est la migration suivante (SQLite 158, PG 124) qui le
//! renomme, sur une base neuve comme sur une base existante.
//!
//! Règles du renommage, communes aux deux moteurs :
//! - le nom ne change que s'il vaut encore EXACTEMENT `default` : un nom choisi
//!   par l'utilisateur est gardé ;
//! - le nom affiché ne change que s'il vaut encore exactement `Default`, et
//!   seulement avec le nom ;
//! - si un AUTRE profil s'appelle déjà `admin` (casse ignorée), rien ne change :
//!   le profil 1 reste `default` et le journal le dit.
//!
//! `default` reste accepté partout où un nom d'utilisateur est saisi : quand
//! plus aucun profil ne s'appelle `default`, ce nom désigne le profil 1
//! ([`SQL_PROFIL_PAR_NOM_DE_CONNEXION`]). Les scripts, les relais et les
//! habitudes qui saisissent `default` continuent donc de fonctionner.

/// Le nom du profil administrateur d'origine depuis la rc14.
pub const NOM_ADMINISTRATEUR: &str = "admin";
/// Son nom affiché.
pub const NOM_AFFICHE_ADMINISTRATEUR: &str = "Admin";
/// L'ancien nom, accepté comme alias du profil 1.
pub const ANCIEN_NOM_ADMINISTRATEUR: &str = "default";
/// L'ancien nom affiché.
pub const ANCIEN_NOM_AFFICHE_ADMINISTRATEUR: &str = "Default";
/// L'identifiant du profil semé à l'installation.
pub const ID_PROFIL_D_ORIGINE: i64 = 1;

/// Le renommage, même texte pour SQLite (migration 158) et PostgreSQL
/// (migration 124). Idempotent : une fois renommé, le profil 1 ne remplit plus
/// la condition.
pub const SQL_RENOMMER_LE_PROFIL_D_ORIGINE: &str = "UPDATE profiles \
     SET username = 'admin', \
         display_name = CASE WHEN display_name = 'Default' THEN 'Admin' ELSE display_name END \
     WHERE id = 1 AND username = 'default' \
       AND NOT EXISTS (SELECT 1 FROM profiles autre \
                        WHERE autre.id <> 1 AND LOWER(autre.username) = 'admin')";

/// 1 quand le renommage est empêché : le profil 1 s'appelle encore `default`
/// et un autre profil porte déjà le nom `admin`. Sert au journal.
pub const SQL_NOM_ADMIN_DEJA_PRIS: &str = "SELECT COUNT(*) FROM profiles \
     WHERE id = 1 AND username = 'default' \
       AND EXISTS (SELECT 1 FROM profiles autre \
                    WHERE autre.id <> 1 AND LOWER(autre.username) = 'admin')";

/// La condition qui retrouve un profil par le nom SAISI (connexion, API).
///
/// Deux paramètres, tous deux le nom saisi :
/// `… WHERE {SQL_PROFIL_PAR_NOM_DE_CONNEXION}` avec `[&nom, &nom]`.
///
/// Le nom exact d'abord ; à défaut, `default` désigne le profil 1 tant
/// qu'aucun profil ne porte plus ce nom. Jamais deux lignes : l'alias ne joue
/// que si personne ne s'appelle `default`.
pub const SQL_PROFIL_PAR_NOM_DE_CONNEXION: &str = "(username = ? \
     OR (? = 'default' AND id = 1 \
         AND NOT EXISTS (SELECT 1 FROM profiles alias_default \
                          WHERE alias_default.username = 'default')))";

/// Les noms réservés au profil administrateur d'origine, casse et espaces
/// ignorés : `admin` et `default` (son alias). Aucun AUTRE profil ne les prend
/// — ni à la création, ni au renommage, ni à l'inscription, ni par la liaison
/// au compte Mozaiklabs —, sinon il capterait l'alias `default`.
pub fn nom_reserve(nom: &str) -> bool {
    let nom = nom.trim();
    nom.eq_ignore_ascii_case(NOM_ADMINISTRATEUR)
        || nom.eq_ignore_ascii_case(ANCIEN_NOM_ADMINISTRATEUR)
}

/// Le nom `nom` est-il refusé pour le profil `id` (`None` : profil à créer) ?
/// Le profil 1 garde le droit de porter ses propres noms.
pub fn nom_refuse_pour(id: Option<i64>, nom: &str) -> bool {
    id != Some(ID_PROFIL_D_ORIGINE) && nom_reserve(nom)
}

/// Le message d'un refus de [`nom_refuse_pour`].
pub const ERREUR_NOM_RESERVE: &str = "nom de profil réservé au profil administrateur";

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::db::backend::{DbBackend, ToSqlValue};
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;

    fn base_migree() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    fn profil_1(db: &Arc<dyn DbBackend>) -> (String, Option<String>) {
        let r = db
            .query_one(
                "SELECT username, display_name FROM profiles WHERE id = 1",
                &[],
            )
            .unwrap()
            .unwrap();
        (
            r[0].as_string().unwrap(),
            r.get(1).and_then(|v| v.as_string()),
        )
    }

    fn id_par_nom_saisi(db: &Arc<dyn DbBackend>, nom: &str) -> Option<i64> {
        db.query_one(
            &format!("SELECT id FROM profiles WHERE {SQL_PROFIL_PAR_NOM_DE_CONNEXION}"),
            &[&nom as &dyn ToSqlValue, &nom as &dyn ToSqlValue],
        )
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
    }

    #[test]
    fn une_base_neuve_nait_avec_admin() {
        let db = base_migree();
        assert_eq!(profil_1(&db), ("admin".into(), Some("Admin".into())));
    }

    #[test]
    fn default_reste_un_alias_du_profil_1_renomme() {
        let db = base_migree();
        assert_eq!(id_par_nom_saisi(&db, "admin"), Some(1));
        assert_eq!(id_par_nom_saisi(&db, "default"), Some(1));
        assert_eq!(
            id_par_nom_saisi(&db, "Default"),
            None,
            "alias exact, comme le nom"
        );
        assert_eq!(id_par_nom_saisi(&db, "inconnu"), None);
    }

    #[test]
    fn un_profil_nomme_default_l_emporte_sur_l_alias() {
        let db = base_migree();
        db.execute(
            "INSERT INTO profiles (id, username, display_name) VALUES (7, 'default', 'X')",
            &[],
        )
        .unwrap();
        assert_eq!(id_par_nom_saisi(&db, "default"), Some(7));
        assert_eq!(id_par_nom_saisi(&db, "admin"), Some(1));
    }

    #[test]
    fn un_profil_1_renomme_par_l_utilisateur_garde_son_nom_et_l_alias() {
        let db = base_migree();
        db.execute(
            "UPDATE profiles SET username = 'bertrand' WHERE id = 1",
            &[],
        )
        .unwrap();
        assert_eq!(id_par_nom_saisi(&db, "default"), Some(1));
        assert_eq!(id_par_nom_saisi(&db, "admin"), None);
    }

    // ── Migration 158 sur une base EXISTANTE ────────────────────────────
    //
    // Une base d'avant la rc14 : migrée jusqu'à la 157, profil 1 en
    // `default` / `Default`. On la reconstitue en remettant l'état d'avant
    // puis en effaçant la marque de la 158 ; `run_migrations` rejoue alors la
    // 158 seule, comme au premier démarrage d'un binaire rc14.

    fn base_sqlite_migree() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        db
    }

    fn sql(db: &SqliteDb, texte: &str) {
        db.connection()
            .lock()
            .unwrap()
            .execute_batch(texte)
            .unwrap();
    }

    fn profils(db: &SqliteDb) -> Vec<(i64, String, Option<String>)> {
        let conn = db.connection().lock().unwrap();
        let mut st = conn
            .prepare("SELECT id, username, display_name FROM profiles ORDER BY id")
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn rejouer_la_158(db: &SqliteDb) {
        sql(db, "DELETE FROM _migrations WHERE version = 158");
        migrations::run_migrations(db).unwrap();
        let marquee: i64 = db
            .connection()
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM _migrations WHERE version = 158",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(marquee, 1, "la 158 est notée");
    }

    #[test]
    fn une_base_ancienne_restee_en_default_passe_en_admin() {
        let db = base_sqlite_migree();
        sql(
            &db,
            "UPDATE profiles SET username = 'default', display_name = 'Default' WHERE id = 1",
        );
        rejouer_la_158(&db);
        assert_eq!(
            profils(&db),
            vec![(1, "admin".into(), Some("Admin".into()))]
        );
    }

    #[test]
    fn un_nom_affiche_choisi_est_garde() {
        let db = base_sqlite_migree();
        sql(
            &db,
            "UPDATE profiles SET username = 'default', display_name = 'Bertrand' WHERE id = 1",
        );
        rejouer_la_158(&db);
        assert_eq!(
            profils(&db),
            vec![(1, "admin".into(), Some("Bertrand".into()))]
        );
    }

    #[test]
    fn un_profil_1_renomme_par_l_utilisateur_est_inchange() {
        let db = base_sqlite_migree();
        sql(
            &db,
            "UPDATE profiles SET username = 'bertrand', display_name = 'Default' WHERE id = 1",
        );
        rejouer_la_158(&db);
        assert_eq!(
            profils(&db),
            vec![(1, "bertrand".into(), Some("Default".into()))]
        );
    }

    #[test]
    fn un_autre_profil_deja_nomme_admin_n_entre_pas_en_collision() {
        let db = base_sqlite_migree();
        sql(
            &db,
            "UPDATE profiles SET username = 'default', display_name = 'Default' WHERE id = 1;
             INSERT INTO profiles (id, username, display_name) VALUES (5, 'admin', 'Admin du foyer');",
        );
        rejouer_la_158(&db);
        assert_eq!(
            profils(&db),
            vec![
                (1, "default".into(), Some("Default".into())),
                (5, "admin".into(), Some("Admin du foyer".into())),
            ]
        );
        // Le journal s'appuie sur ce constat.
        let pris: i64 = db
            .connection()
            .lock()
            .unwrap()
            .query_row(SQL_NOM_ADMIN_DEJA_PRIS, [], |r| r.get(0))
            .unwrap();
        assert_eq!(pris, 1);
        // Casse ignorée : `Admin` est aussi un nom pris.
        sql(&db, "UPDATE profiles SET username = 'Admin' WHERE id = 5");
        rejouer_la_158(&db);
        assert_eq!(profils(&db)[0].1, "default");
    }

    #[test]
    fn la_158_rejouee_ne_change_plus_rien() {
        let db = base_sqlite_migree();
        sql(
            &db,
            "UPDATE profiles SET display_name = 'Bertrand' WHERE id = 1",
        );
        rejouer_la_158(&db);
        rejouer_la_158(&db);
        assert_eq!(
            profils(&db),
            vec![(1, "admin".into(), Some("Bertrand".into()))]
        );
    }

    /// Le fichier PG 124 porte la même règle que la constante (même texte à
    /// l'espacement près).
    #[test]
    fn la_jumelle_pg_124_porte_le_meme_texte() {
        let pg = include_str!("../../migrations/postgres/124_profil_administrateur_admin.sql");
        let sans_commentaires: String = pg
            .lines()
            .filter(|l| !l.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join(" ");
        let plier = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            plier(&sans_commentaires).contains(&plier(SQL_RENOMMER_LE_PROFIL_D_ORIGINE)),
            "la 124 ne porte plus le texte de SQL_RENOMMER_LE_PROFIL_D_ORIGINE"
        );
    }

    #[test]
    fn les_noms_reserves() {
        assert!(nom_reserve("admin"));
        assert!(nom_reserve("Admin"));
        assert!(nom_reserve("default"));
        assert!(nom_reserve("DEFAULT"));
        assert!(nom_reserve(" Default "));
        assert!(!nom_reserve("léa"));
        assert!(!nom_reserve("administrateur"));
        assert!(nom_refuse_pour(None, "Admin"));
        assert!(nom_refuse_pour(Some(7), "default"));
        assert!(!nom_refuse_pour(Some(1), "default"));
        assert!(!nom_refuse_pour(Some(7), "léa"));
    }
}
