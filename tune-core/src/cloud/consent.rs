//! Consentement explicite pour la contribution de metadonnees au cloud
//! communautaire.
//!
//! Deux chemins remontaient des metadonnees enrichies vers mozaiklabs.fr sans
//! que l'utilisateur ait jamais eu a dire oui :
//!
//! 1. `cloud::bio_sync::upload_bios` — les biographies d'artistes et d'albums,
//!    gouvernees par le seul `TUNE_TELEMETRY`, **actif par defaut** : c'etait
//!    donc un opt-out, et un opt-out invisible (aucun reglage dans l'UI, une
//!    variable d'environnement pour toute porte de sortie) ;
//! 2. `library::artwork` — les images d'artistes recuperees automatiquement,
//!    qui ne connaissaient meme pas `TUNE_TELEMETRY` : le seul garde-fou etait
//!    « avoir un `instance_id` », or celui-ci est genere tout seul au demarrage.
//!    Cet envoi-la etait donc inconditionnel.
//!
//! Ce module porte l'unique verrou que les deux chemins consultent desormais.
//! Le defaut est NON : rien ne part tant que l'utilisateur n'a pas coche.
//!
//! Ce verrou ne concerne QUE la contribution — ce que la machine *envoie*. Le
//! telechargement de bios communautaires, lui, ne fait sortir aucune donnee
//! personnelle et garde ses propres regles.

use crate::db::settings_repo::SettingsRepo;

/// Cle du reglage en base `settings`. C'est aussi le nom que le client web
/// lit dans `GET /api/v1/system/config` et reecrit par `PATCH`.
pub const CONTRIBUTION_SETTING_KEY: &str = "community_contribution_enabled";

/// Valeur par defaut : **desactive**. Opt-in strict, jamais opt-out.
pub const CONTRIBUTION_DEFAULT: bool = false;

/// Lit une valeur de `settings` comme un booleen. Le reglage est ecrit tantot
/// par `PATCH /system/config` (qui serialise le JSON `true` en `"true"`),
/// tantot a la main ; on accepte les formes usuelles du vrai et **rien
/// d'autre** : toute valeur inconnue vaut « non », parce que le doute doit
/// toujours pencher du cote qui n'envoie rien.
pub fn est_vrai(brut: &str) -> bool {
    matches!(
        brut.trim().trim_matches('"').to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    )
}

/// L'utilisateur a-t-il explicitement autorise la contribution de metadonnees
/// au cloud communautaire ?
///
/// Faux quand le reglage est absent (installation neuve), illisible, ou pose a
/// autre chose que vrai. Faux aussi quand `TUNE_TELEMETRY` est explicitement
/// coupe : un refus pose a l'echelle de la machine reste souverain sur un
/// reglage d'application — on ne peut pas re-autoriser par l'UI ce que
/// l'exploitant a interdit par l'environnement.
pub fn contribution_autorisee(settings: &SettingsRepo) -> bool {
    if !crate::cloud::telemetry::TelemetryReporter::is_enabled() {
        return false;
    }
    settings
        .get(CONTRIBUTION_SETTING_KEY)
        .ok()
        .flatten()
        .map(|v| est_vrai(&v))
        .unwrap_or(CONTRIBUTION_DEFAULT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::backend::DbBackend;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;
    use std::sync::Arc;

    fn base_neuve() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    #[test]
    fn le_defaut_d_une_installation_neuve_est_non() {
        let settings = SettingsRepo::with_backend(base_neuve());
        assert!(!CONTRIBUTION_DEFAULT);
        assert!(
            !contribution_autorisee(&settings),
            "une base neuve ne porte aucun consentement : rien ne doit partir"
        );
    }

    #[test]
    fn seul_un_oui_explicite_ouvre_la_porte() {
        let settings = SettingsRepo::with_backend(base_neuve());

        for refus in ["false", "0", "no", "off", "", "peut-etre", "TRUE ish"] {
            settings.set(CONTRIBUTION_SETTING_KEY, refus).unwrap();
            assert!(
                !contribution_autorisee(&settings),
                "{refus:?} ne vaut pas un consentement"
            );
        }

        // Les formes du oui, y compris le "true" guillemete que produit un
        // PATCH ayant serialise le JSON booleen en chaine.
        for accord in ["true", "TRUE", " true ", "1", "yes", "on", "\"true\""] {
            settings.set(CONTRIBUTION_SETTING_KEY, accord).unwrap();
            assert!(
                contribution_autorisee(&settings),
                "{accord:?} vaut un consentement"
            );
        }
    }
}
