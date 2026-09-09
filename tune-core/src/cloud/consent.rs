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

/// Cle du reglage qui gouverne la synchronisation communautaire AUTOMATIQUE
/// de la bibliotheque (`community_sync::spawn`, toutes les 30 minutes).
///
/// C'est la cle que le client web ecrit par `PATCH /system/config` depuis la
/// bascule « Partage communautaire des metadonnees ».
pub const SYNC_SETTING_KEY: &str = "community_sync_enabled";

/// Valeur par defaut : **desactive**. Opt-in strict, comme la contribution.
pub const SYNC_DEFAULT: bool = false;

/// La boucle de synchronisation communautaire a-t-elle le droit de tourner ?
///
/// #3383 — c'etait la DERNIERE famille d'envois automatiques a ignorer le
/// refus de telemetrie. Elle lisait `community_sync_enabled` en dur, sans
/// jamais consulter le verrou, pendant que sa jumelle
/// [`contribution_autorisee`] le respectait depuis le debut. Un utilisateur
/// qui decochait la telemetrie continuait donc de voir partir, toutes les
/// trente minutes, le titre, l'artiste, l'album, le genre, l'annee, l'ISRC et
/// le format de ses pistes.
///
/// **Ce verrou ne couvre que l'AUTOMATIQUE.** Le signalement de metadonnee
/// (`routes/library/reports.rs`) reste gouverne par le seul
/// `community_sync_enabled` : il part parce que l'utilisateur vient de cliquer
/// « signaler ». Couper en silence une action qu'on vient de demander serait
/// une seconde facon de mentir a l'ecran, symetrique de celle que corrige
/// cette issue.
pub fn sync_communautaire_autorise(settings: &SettingsRepo) -> bool {
    if !crate::cloud::telemetry::TelemetryReporter::is_enabled_for(settings) {
        return false;
    }
    settings
        .get(SYNC_SETTING_KEY)
        .ok()
        .flatten()
        .map(|v| est_vrai(&v))
        .unwrap_or(SYNC_DEFAULT)
}

/// L'utilisateur a-t-il explicitement autorise la contribution de metadonnees
/// au cloud communautaire ?
///
/// Faux quand le reglage est absent (installation neuve), illisible, ou pose a
/// autre chose que vrai. Faux aussi quand la telemetrie est refusee — par
/// l'environnement OU par l'interface (#3383) : un refus, d'ou qu'il vienne,
/// reste souverain, et on ne peut pas re-autoriser d'un cote ce qui a ete
/// interdit de l'autre.
pub fn contribution_autorisee(settings: &SettingsRepo) -> bool {
    if !crate::cloud::telemetry::TelemetryReporter::is_enabled_for(settings) {
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

    /// #3383 — refuser la telemetrie arrete AUSSI la boucle de
    /// synchronisation, meme quand elle est explicitement cochee.
    #[test]
    fn le_refus_de_telemetrie_arrete_la_synchronisation_cochee() {
        let settings = SettingsRepo::with_backend(base_neuve());
        settings.set(SYNC_SETTING_KEY, "true").unwrap();
        assert!(
            sync_communautaire_autorise(&settings),
            "cochee, telemetrie acceptee : la boucle a le droit de tourner"
        );

        settings
            .set(crate::cloud::telemetry::TELEMETRY_SETTING_KEY, "false")
            .unwrap();
        assert!(
            !sync_communautaire_autorise(&settings),
            "un refus de telemetrie doit arreter la boucle, meme cochee"
        );
    }

    /// Le defaut ne bouge pas : une installation qui n'a rien coche
    /// n'envoyait rien, et n'envoie toujours rien.
    #[test]
    fn la_synchronisation_reste_un_opt_in() {
        let settings = SettingsRepo::with_backend(base_neuve());
        assert!(!SYNC_DEFAULT);
        assert!(!sync_communautaire_autorise(&settings));
    }

    /// La boucle de production doit APPELER ce verrou, pas relire la cle pour
    /// son compte — c'est exactement ce qu'elle faisait avant #3383, et un
    /// verrou que personne n'appelle ne coupe rien.
    #[test]
    fn la_boucle_de_synchronisation_appelle_bien_ce_verrou() {
        let source = include_str!("community_sync.rs");
        let production = source
            .split(&format!("#[cfg({})]", "test"))
            .next()
            .expect("source vide");
        assert!(
            production.contains(&format!("consent::{}(", "sync_communautaire_autorise")),
            "community_sync::spawn doit passer par consent::sync_communautaire_autorise"
        );
        assert!(
            !production.contains("get(\"community_sync_enabled\")"),
            "plus aucune lecture en dur de la cle dans la boucle : elle contournerait le verrou"
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
