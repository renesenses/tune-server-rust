//! Nom (`username`) d'un profil créé par la liaison au compte Mozaiklabs.
//!
//! Le nom d'un profil se lit partout (`GET /profiles`, sélecteur de profil) :
//! il ne porte jamais l'adresse du compte. La liaison nomme le profil d'après
//! le nom affiché du compte, sinon d'après la partie de l'adresse avant `@`.
//! L'adresse reste dans la colonne `email`, qui sert au rapprochement et ne
//! se publie pas.
//!
//! Les profils créés avant cette règle (nom = adresse) sont renommés une fois,
//! au démarrage, par [`renommer_les_profils_nommes_par_leur_adresse`] — passe
//! idempotente, identique sur SQLite et PostgreSQL. Un profil qui a un mot de
//! passe n'est jamais renommé : son nom sert à la connexion par mot de passe.

use std::sync::Arc;

use crate::db::backend::{DbBackend, ToSqlValue};

/// Le nom de base, sans garantie d'unicité.
pub fn nom_neutre(nom_affiche: &str, courriel: &str) -> String {
    let affiche = nom_affiche.trim();
    let base = if !affiche.is_empty() {
        affiche.split('@').next().unwrap_or("").trim().to_string()
    } else {
        String::new()
    };
    let base = if base.is_empty() {
        courriel.split('@').next().unwrap_or("").trim().to_string()
    } else {
        base
    };
    // `default` est le profil d'origine de toute installation : jamais pris.
    if base.is_empty() || base.eq_ignore_ascii_case("default") {
        "compte".to_string()
    } else {
        base
    }
}

/// Le nom AFFICHÉ d'un profil lié : celui du compte, sauf s'il est une
/// adresse (alors sa partie avant `@`). `GET /profiles` le publie.
pub fn nom_affiche_neutre(nom_affiche: &str) -> String {
    let n = nom_affiche.trim();
    if n.contains('@') {
        n.split('@').next().unwrap_or("").trim().to_string()
    } else {
        n.to_string()
    }
}

fn nom_pris(backend: &Arc<dyn DbBackend>, nom: &str, sauf: Option<i64>) -> bool {
    let sauf = sauf.unwrap_or(-1);
    backend
        .query_one(
            "SELECT COUNT(*) FROM profiles WHERE username = ? AND id <> ?",
            &[&nom as &dyn ToSqlValue, &sauf as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|row| row.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
        > 0
}

/// Un nom libre dérivé de [`nom_neutre`] : `léa`, sinon `léa-2`, `léa-3`…
/// `sauf` : le profil à renommer lui-même (son nom actuel ne compte pas).
pub fn nom_libre(
    backend: &Arc<dyn DbBackend>,
    nom_affiche: &str,
    courriel: &str,
    sauf: Option<i64>,
) -> String {
    let base = nom_neutre(nom_affiche, courriel);
    if !nom_pris(backend, &base, sauf) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|nom| !nom_pris(backend, nom, sauf))
        .expect("suite infinie")
}

/// Renomme les profils créés par la liaison sous leur adresse (`username` =
/// `email`, contient `@`), sans mot de passe. Rend le nombre de profils
/// renommés. Idempotente : un profil renommé ne remplit plus la condition.
pub fn renommer_les_profils_nommes_par_leur_adresse(backend: &Arc<dyn DbBackend>) -> usize {
    let lignes = backend
        .query_many(
            "SELECT id, username, display_name FROM profiles \
             WHERE LOWER(username) = LOWER(email) AND username LIKE '%@%' \
             AND (password_hash IS NULL OR password_hash = '') \
             AND (password_hash_v2 IS NULL OR password_hash_v2 = '')",
            &[],
        )
        .unwrap_or_default();
    let mut renommes = 0;
    for ligne in lignes {
        let Some(id) = ligne.first().and_then(|v| v.as_i64()) else {
            continue;
        };
        let adresse = ligne.get(1).and_then(|v| v.as_string()).unwrap_or_default();
        let affiche = ligne.get(2).and_then(|v| v.as_string()).unwrap_or_default();
        let nom = nom_libre(backend, &affiche, &adresse, Some(id));
        let affiche = nom_affiche_neutre(&affiche);
        if backend
            .execute(
                "UPDATE profiles SET username = ?, display_name = ? WHERE id = ?",
                &[
                    &nom as &dyn ToSqlValue,
                    &affiche as &dyn ToSqlValue,
                    &id as &dyn ToSqlValue,
                ],
            )
            .is_ok()
        {
            renommes += 1;
        }
    }
    if renommes > 0 {
        tracing::info!(renommes, "profils_renommes_hors_adresse");
    }
    renommes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    fn profil(db: &Arc<dyn DbBackend>, nom: &str, affiche: &str, courriel: &str) -> i64 {
        let zero: i64 = 0;
        db.execute(
            "INSERT INTO profiles (username, display_name, email, avatar_path, is_admin) \
             VALUES (?, ?, ?, ?, ?)",
            &[
                &nom as &dyn ToSqlValue,
                &affiche as &dyn ToSqlValue,
                &courriel as &dyn ToSqlValue,
                &"#6366f1" as &dyn ToSqlValue,
                &zero as &dyn ToSqlValue,
            ],
        )
        .unwrap();
        db.last_insert_rowid()
    }

    fn nom(db: &Arc<dyn DbBackend>, id: i64) -> String {
        db.query_one(
            "SELECT username FROM profiles WHERE id = ?",
            &[&id as &dyn ToSqlValue],
        )
        .unwrap()
        .unwrap()[0]
            .as_string()
            .unwrap()
    }

    #[test]
    fn le_nom_neutre_ne_porte_jamais_l_adresse() {
        assert_eq!(nom_neutre("Léa Martin", "lea@exemple.test"), "Léa Martin");
        assert_eq!(nom_neutre("", "lea@exemple.test"), "lea");
        assert_eq!(nom_neutre("  ", "lea@exemple.test"), "lea");
        // Un nom affiché qui EST une adresse : sa partie avant @.
        assert_eq!(nom_neutre("lea@exemple.test", "lea@exemple.test"), "lea");
        assert_eq!(nom_neutre("", ""), "compte");
        assert_eq!(nom_neutre("Default", "x@y.z"), "compte");
        assert_eq!(nom_affiche_neutre("lea@exemple.test"), "lea");
        assert_eq!(nom_affiche_neutre("Léa"), "Léa");
        assert!(!nom_neutre("x@y.z", "x@y.z").contains('@'));
    }

    #[test]
    fn un_nom_pris_recoit_un_suffixe() {
        let db = base();
        profil(&db, "lea", "Léa", "autre@exemple.test");
        assert_eq!(nom_libre(&db, "", "lea@exemple.test", None), "lea-2");
    }

    #[test]
    fn les_profils_nommes_par_leur_adresse_sont_renommes_une_fois() {
        let db = base();
        let a = profil(&db, "lea@exemple.test", "Léa", "lea@exemple.test");
        let b = profil(
            &db,
            "tom@exemple.test",
            "tom@exemple.test",
            "tom@exemple.test",
        );
        // Un nom choisi à la main (≠ adresse du compte) reste tel quel.
        let c = profil(&db, "moi@maison", "Moi", "autre@exemple.test");
        // Un profil à mot de passe garde son nom de connexion.
        let d = profil(&db, "pat@exemple.test", "Pat", "pat@exemple.test");
        db.execute(
            "UPDATE profiles SET password_hash = 'x' WHERE id = ?",
            &[&d as &dyn ToSqlValue],
        )
        .unwrap();

        assert_eq!(renommer_les_profils_nommes_par_leur_adresse(&db), 2);
        assert_eq!(nom(&db, a), "Léa");
        assert_eq!(nom(&db, b), "tom");
        // Le nom affiché qui était l'adresse devient sa partie avant @.
        let affiche_b = db
            .query_one(
                "SELECT display_name FROM profiles WHERE id = ?",
                &[&b as &dyn ToSqlValue],
            )
            .unwrap()
            .unwrap()[0]
            .as_string()
            .unwrap();
        assert_eq!(affiche_b, "tom");
        assert_eq!(nom(&db, c), "moi@maison");
        assert_eq!(nom(&db, d), "pat@exemple.test");
        // L'adresse reste dans `email` : le rapprochement de la liaison tient.
        let courriel = db
            .query_one(
                "SELECT email FROM profiles WHERE id = ?",
                &[&a as &dyn ToSqlValue],
            )
            .unwrap()
            .unwrap()[0]
            .as_string()
            .unwrap();
        assert_eq!(courriel, "lea@exemple.test");
        // Idempotente.
        assert_eq!(renommer_les_profils_nommes_par_leur_adresse(&db), 0);
    }
}
