//! Volume fixe (bit-perfect) : mémoire du volume d'avant l'armement (#2395).
//!
//! Cocher « Volume fixe » porte la zone à 100 %. Jusqu'ici, décocher la case
//! ne rendait rien : le volume d'origine était perdu, et l'utilisateur devait
//! le retrouver à l'oreille — sur un renderer qui porte son propre ampli, en
//! partant de 100 %.
//!
//! Ce module tient la seule pièce qui manquait : la valeur d'avant. Elle est
//! écrite à l'armement, relue et effacée au désarmement.
//!
//! ## Pourquoi un réglage et pas une colonne
//!
//! Aucune colonne de `zones` ne porte un volume *précédent*, et en ajouter une
//! imposerait une migration sur les deux moteurs (SQLite et Postgres) pour une
//! donnée qui ne vit que le temps d'un mode. La table `settings` porte déjà
//! l'état par zone du mode PURE (`zone_{id}_audiophile`) : la mémoire du volume
//! suit la même clef par zone, et un serveur qui redémarre en mode bit-perfect
//! sait toujours quoi rendre — ce qu'une mémoire en RAM ne saurait pas.
//!
//! ## Échelle
//!
//! Le pour-cent de la colonne `zones.volume` (0..100, décimales comprises
//! depuis #2886), et non le linéaire 0..1 : la valeur mémorisée se compare
//! directement à ce que la base contient, sans conversion à l'écriture.

use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Clef du volume mémorisé pour une zone.
pub fn setting_key(zone_id: i64) -> String {
    format!("zone_{zone_id}_volume_before_fixed")
}

/// Mémorise le volume (en pour-cent) d'avant l'armement du mode.
///
/// Écrase une mémoire précédente : la dernière transition `false → true` est
/// celle qui fait foi. Une écriture qui échoue n'empêche pas l'armement —
/// elle coûte la restauration, pas le mode.
pub fn remember(db: &Arc<dyn DbBackend>, zone_id: i64, volume_percent: f64) -> Result<(), String> {
    SettingsRepo::with_backend(db.clone()).set(&setting_key(zone_id), &volume_percent.to_string())
}

/// Lit le volume mémorisé sans l'effacer.
///
/// `None` quand rien n'a été mémorisé, ou quand la valeur stockée n'est pas un
/// nombre exploitable : on préfère ne rien rendre plutôt que de commander un
/// volume deviné.
pub fn peek(db: &Arc<dyn DbBackend>, zone_id: i64) -> Option<f64> {
    SettingsRepo::with_backend(db.clone())
        .get(&setting_key(zone_id))
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && (0.0..=100.0).contains(v))
}

/// Efface la mémoire. Sans effet si elle est déjà vide.
pub fn forget(db: &Arc<dyn DbBackend>, zone_id: i64) {
    let _ = SettingsRepo::with_backend(db.clone()).delete(&setting_key(zone_id));
}

/// Lit **et** efface : la valeur à rendre au désarmement.
///
/// L'effacement est inconditionnel, y compris quand la lecture ne rend rien :
/// une valeur illisible ne doit pas rester à traîner pour le prochain cycle.
pub fn take(db: &Arc<dyn DbBackend>, zone_id: i64) -> Option<f64> {
    let value = peek(db, zone_id);
    forget(db, zone_id);
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;

    fn mem_db() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().expect("base memoire");
        db.init_schema().expect("schema");
        migrations::run_migrations(&db).expect("migrations");
        Arc::new(db)
    }

    #[test]
    fn sans_memoire_il_n_y_a_rien_a_rendre() {
        let db = mem_db();
        assert_eq!(peek(&db, 1), None);
        assert_eq!(take(&db, 1), None);
    }

    #[test]
    fn ce_qui_est_memorise_se_relit_a_l_identique() {
        let db = mem_db();
        // Une valeur à virgule : la colonne en porte depuis #2886, la mémoire
        // ne doit pas la rogner au passage.
        remember(&db, 7, 42.5).expect("memoriser");
        assert_eq!(peek(&db, 7), Some(42.5));
        // `peek` ne consomme pas.
        assert_eq!(peek(&db, 7), Some(42.5));
    }

    #[test]
    fn prendre_rend_la_valeur_puis_vide_la_memoire() {
        let db = mem_db();
        remember(&db, 7, 30.0).expect("memoriser");
        assert_eq!(take(&db, 7), Some(30.0));
        // Deuxième désarmement sans armement entre les deux : plus rien.
        assert_eq!(take(&db, 7), None);
    }

    #[test]
    fn les_zones_ne_se_melangent_pas() {
        let db = mem_db();
        remember(&db, 7, 30.0).expect("memoriser");
        remember(&db, 8, 80.0).expect("memoriser");
        assert_eq!(take(&db, 7), Some(30.0));
        assert_eq!(peek(&db, 8), Some(80.0));
    }

    #[test]
    fn un_second_armement_ecrase_la_memoire() {
        let db = mem_db();
        remember(&db, 7, 30.0).expect("memoriser");
        remember(&db, 7, 55.0).expect("memoriser");
        assert_eq!(take(&db, 7), Some(55.0));
    }

    #[test]
    fn une_valeur_illisible_ou_hors_bornes_ne_commande_rien() {
        let db = mem_db();
        let repo = SettingsRepo::with_backend(db.clone());
        for value in ["", "beaucoup", "NaN", "inf", "-1", "101", "1e400"] {
            repo.set(&setting_key(9), value).expect("ecrire");
            assert_eq!(
                peek(&db, 9),
                None,
                "« {value} » ne doit pas devenir une consigne de volume"
            );
        }
        // Et elle est bien évacuée, pour ne pas polluer le cycle suivant.
        repo.set(&setting_key(9), "beaucoup").expect("ecrire");
        assert_eq!(take(&db, 9), None);
        assert_eq!(
            repo.get(&setting_key(9)).ok().flatten(),
            None,
            "la valeur illisible doit etre effacee"
        );
    }

    #[test]
    fn les_bornes_elles_memes_restent_valides() {
        let db = mem_db();
        remember(&db, 1, 0.0).expect("memoriser");
        assert_eq!(peek(&db, 1), Some(0.0));
        remember(&db, 1, 100.0).expect("memoriser");
        assert_eq!(peek(&db, 1), Some(100.0));
    }
}
