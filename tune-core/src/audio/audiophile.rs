//! Mode PURE (audiophile) : lecture partagée du réglage, et politique de
//! volume qui l'accompagne.
//!
//! Le drapeau `zone_{id}_audiophile` était lu à l'identique en trois endroits
//! (orchestrateur, route `/zones`, chemin du signal). Une quatrième copie
//! arrivait avec le verrou de volume : le moment de n'en garder qu'une.
//!
//! ## Le verrou de volume
//!
//! En PURE, aucun traitement ne touche les échantillons — sauf le volume, qui
//! reste un multiplicateur appliqué à chacun d'eux. Un curseur à 20 % dans une
//! zone « bit-perfect », c'est une contradiction : le chemin n'est plus
//! bit-perfect, et rien ne le dit.
//!
//! Le réglage `audiophile_lock_volume` force alors le volume à 100 % et gèle
//! le curseur. Il est **inactif par défaut**, décision de Bertrand : couper le
//! volume d'un utilisateur au moment où il coche « Audiophile » serait une
//! mauvaise surprise, et tout le monde n'écoute pas avec un préampli en aval.

use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Réglage global par défaut : le mode PURE impose-t-il le volume à 100 % ?
pub const SETTING_LOCK_VOLUME: &str = "audiophile_lock_volume";

/// La zone est-elle en mode PURE (audiophile) ?
///
/// Valeur stockée sous forme d'objet JSON `{"enabled": true}` — un réglage
/// absent, illisible ou sans le champ vaut « inactif ».
pub fn zone_enabled(db: &Arc<dyn DbBackend>, zone_id: i64) -> bool {
    SettingsRepo::with_backend(db.clone())
        .get(&format!("zone_{zone_id}_audiophile"))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("enabled").and_then(|e| e.as_bool()))
        .unwrap_or(false)
}

/// Le verrou de volume global est-il armé ? **Faux par défaut** : seule la chaîne
/// « true » l'active, pour qu'une clé absente ou illisible ne modifie jamais
/// le volume de personne.
pub fn global_volume_lock_enabled(db: &Arc<dyn DbBackend>) -> bool {
    SettingsRepo::with_backend(db.clone())
        .get(SETTING_LOCK_VOLUME)
        .ok()
        .flatten()
        .as_deref()
        == Some("true")
}

/// Surcharge du verrou propre à une zone.
///
/// Elle vit dans le même objet que le mode PURE, sous `lock_volume`. L'absence
/// du champ signifie « hériter du réglage global » ; une valeur illisible est
/// traitée de la même façon pour préserver le comportement historique.
pub fn volume_lock_override(db: &Arc<dyn DbBackend>, zone_id: i64) -> Option<bool> {
    SettingsRepo::with_backend(db.clone())
        .get(&format!("zone_{zone_id}_audiophile"))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("lock_volume").and_then(|e| e.as_bool()))
}

/// Verrou effectif pour une zone : sa surcharge explicite, ou à défaut le
/// réglage global historique. Une installation existante sans surcharge garde
/// donc strictement son comportement.
pub fn volume_lock_enabled(db: &Arc<dyn DbBackend>, zone_id: i64) -> bool {
    volume_lock_override(db, zone_id).unwrap_or_else(|| global_volume_lock_enabled(db))
}

/// Volume réellement appliqué pour cette zone.
///
/// Renvoie `1.0` quand la zone est en PURE **et** que le verrou est armé ;
/// sinon la valeur demandée, bornée à `[0, 1]`. Le point d'entrée unique :
/// la route de volume s'en sert pour répondre la valeur *effective*, ce qui
/// fait remonter le curseur de l'interface au lieu de le laisser mentir.
/// `f64` et non `f32` : tout le reste de la chaîne de volume est en double
/// précision (`Orchestrator::set_volume`, `PlaybackState::volume`, la colonne
/// et les charges utiles JSON). Cette fonction était le seul rétrécissement du
/// parcours, et il se voyait : un réglage à −20 dB ressortait à
/// −19,99999987 dB, parce que 0,1 n'est pas représentable en `f32`. Inaudible,
/// mais c'est exactement la réversibilité que #1274 promet — et un aller-retour
/// lecture/écriture faisait dériver le curseur d'un pouième à chaque passage.
pub fn effective_volume(db: &Arc<dyn DbBackend>, zone_id: i64, requested: f64) -> f64 {
    if volume_lock_enabled(db, zone_id) && zone_enabled(db, zone_id) {
        return 1.0;
    }
    requested.clamp(0.0, 1.0)
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

    fn set(db: &Arc<dyn DbBackend>, key: &str, value: &str) {
        SettingsRepo::with_backend(db.clone())
            .set(key, value)
            .expect("ecrire le reglage");
    }

    #[test]
    fn a_zone_without_setting_is_not_in_pure_mode() {
        assert!(!zone_enabled(&mem_db(), 1));
    }

    #[test]
    fn pure_mode_reads_the_enabled_field() {
        let db = mem_db();
        set(&db, "zone_7_audiophile", r#"{"enabled":true}"#);
        assert!(zone_enabled(&db, 7));

        set(&db, "zone_7_audiophile", r#"{"enabled":false}"#);
        assert!(!zone_enabled(&db, 7));

        // Ni un JSON sans le champ, ni du texte libre ne doivent activer PURE.
        set(&db, "zone_7_audiophile", "{}");
        assert!(!zone_enabled(&db, 7));
        set(&db, "zone_7_audiophile", "oui");
        assert!(!zone_enabled(&db, 7));
    }

    #[test]
    fn the_volume_lock_is_off_until_explicitly_armed() {
        let db = mem_db();
        assert!(!global_volume_lock_enabled(&db));

        // Rien d'autre que « true » ne l'arme : on ne coupe pas le volume de
        // quelqu'un sur une valeur douteuse.
        for value in ["", "1", "oui", "yes", "TRUE"] {
            set(&db, SETTING_LOCK_VOLUME, value);
            assert!(
                !global_volume_lock_enabled(&db),
                "« {value} » ne doit pas armer"
            );
        }

        set(&db, SETTING_LOCK_VOLUME, "true");
        assert!(global_volume_lock_enabled(&db));
    }

    #[test]
    fn a_zone_inherits_the_global_lock_until_it_has_an_override() {
        let db = mem_db();
        assert_eq!(volume_lock_override(&db, 7), None);
        assert!(!volume_lock_enabled(&db, 7));

        set(&db, SETTING_LOCK_VOLUME, "true");
        assert!(volume_lock_enabled(&db, 7));

        set(
            &db,
            "zone_7_audiophile",
            r#"{"enabled":true,"lock_volume":false}"#,
        );
        assert_eq!(volume_lock_override(&db, 7), Some(false));
        assert!(!volume_lock_enabled(&db, 7));

        set(
            &db,
            "zone_8_audiophile",
            r#"{"enabled":true,"lock_volume":true}"#,
        );
        assert_eq!(volume_lock_override(&db, 8), Some(true));
        assert!(volume_lock_enabled(&db, 8));
    }

    #[test]
    fn malformed_zone_override_falls_back_to_the_global_setting() {
        let db = mem_db();
        set(&db, SETTING_LOCK_VOLUME, "true");
        for value in [
            r#"{"enabled":true}"#,
            r#"{"enabled":true,"lock_volume":"oui"}"#,
            "pas du json",
        ] {
            set(&db, "zone_9_audiophile", value);
            assert_eq!(volume_lock_override(&db, 9), None);
            assert!(volume_lock_enabled(&db, 9));
        }
    }

    #[test]
    fn volume_passes_through_unless_pure_and_locked_are_both_true() {
        let db = mem_db();

        // Ni l'un ni l'autre.
        assert_eq!(effective_volume(&db, 1, 0.2), 0.2);

        // PURE seul : le volume reste celui de l'utilisateur — c'est le
        // comportement par défaut, et il ne change pas.
        set(&db, "zone_1_audiophile", r#"{"enabled":true}"#);
        assert_eq!(effective_volume(&db, 1, 0.2), 0.2);

        // Verrou seul, zone ordinaire : rien ne change non plus.
        set(&db, SETTING_LOCK_VOLUME, "true");
        assert_eq!(effective_volume(&db, 2, 0.2), 0.2);

        // Les deux : plein volume.
        assert_eq!(effective_volume(&db, 1, 0.2), 1.0);

        // Une exclusion explicite protège cette zone malgré le verrou global.
        set(
            &db,
            "zone_1_audiophile",
            r#"{"enabled":true,"lock_volume":false}"#,
        );
        assert_eq!(effective_volume(&db, 1, 0.2), 0.2);

        // Et l'inverse permet de verrouiller un transport seulement.
        set(&db, SETTING_LOCK_VOLUME, "false");
        set(
            &db,
            "zone_1_audiophile",
            r#"{"enabled":true,"lock_volume":true}"#,
        );
        assert_eq!(effective_volume(&db, 1, 0.2), 1.0);
    }

    #[test]
    fn a_volume_out_of_range_is_clamped_not_rejected() {
        let db = mem_db();
        assert_eq!(effective_volume(&db, 1, 1.8), 1.0);
        assert_eq!(effective_volume(&db, 1, -0.5), 0.0);
    }
}
