//! « Écrire les modifications dans les fichiers audio » — le réglage UNIQUE
//! qui autorise Tune à réécrire les fichiers de l'utilisateur.
//!
//! Bertrand, 05/10/2026 : « Écrire les tags dans les fichiers : inactif par
//! défaut ! »
//!
//! Désactivé (le défaut), une modification de métadonnées va **seulement en
//! base** : les fichiers audio ne sont ni ouverts en écriture, ni renommés, ni
//! retouchés. Les routes qui n'ont pas d'autre effet que d'écrire dans les
//! fichiers (« Écrire dans les fichiers », gravure DR, gravure du drapeau
//! compilation, nettoyage des balises…) refusent alors en `409` avec le code
//! [`CODE_REFUS`], pour que l'interface puisse dire où se trouve le réglage.
//!
//! ## Installations existantes
//!
//! Le réglage n'existait pas avant. **Une clé absente vaut « désactivé »**
//! ([`DEFAUT`]) : toute installation qui se met à jour cesse d'écrire dans les
//! fichiers, jusqu'à ce que l'utilisateur coche la case. Les réglages propres
//! à une fonction (`ingest_write_tags`, `lyrics_write_files_enabled`) gardent
//! leur valeur, mais ne suffisent plus : ils s'ajoutent à celui-ci, ils ne le
//! remplacent pas.
//!
//! ## La garde
//!
//! Tout chemin du dépôt qui écrit dans un fichier audio consulte
//! [`autorisee`]. Le test `garde_ecriture_fichiers` (tune-server) recense les
//! appels aux écrivains et échoue si un nouveau apparaît sans passer par ici.
use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Clé du réglage, telle que `GET/PATCH /system/config` la publie.
pub const CLE: &str = "library_write_files_enabled";

/// Valeur quand la clé est absente : **désactivé**.
pub const DEFAUT: bool = false;

/// Code stable d'un refus, dans le corps `{"error": …, "code": …}` d'un 409.
pub const CODE_REFUS: &str = "file_writes_disabled";

/// Phrase de journal et de repli (le client traduit à partir de
/// [`CODE_REFUS`], il n'affiche pas cette phrase).
pub const MOTIF_REFUS: &str = "Écriture dans les fichiers audio désactivée \
     (Réglages › Bibliothèque) : rien n'a été écrit.";

/// Lit une valeur stockée. Seules les formes « vraies » usuelles activent :
/// toute autre valeur — y compris illisible — laisse les fichiers tranquilles.
pub fn depuis_valeur(valeur: Option<&str>) -> bool {
    match valeur {
        None => DEFAUT,
        Some(v) => matches!(
            v.trim().trim_matches('"').to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        ),
    }
}

/// Tune a-t-il le droit d'écrire dans les fichiers audio de l'utilisateur ?
///
/// Une base illisible répond « non » : dans le doute, on ne touche pas aux
/// fichiers.
pub fn autorisee(db: &Arc<dyn DbBackend>) -> bool {
    match SettingsRepo::with_backend(db.clone()).get(CLE) {
        Ok(v) => depuis_valeur(v.as_deref()),
        Err(e) => {
            tracing::warn!(erreur = %e, "ecriture_fichiers_reglage_illisible");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Arc<dyn DbBackend> {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().expect("base en mémoire");
        db.init_schema().expect("schéma");
        crate::db::migrations::run_migrations(&db).expect("migrations");
        Arc::new(db)
    }

    /// 🔴 Le cœur de la demande : une base qui n'a jamais vu le réglage —
    /// toute installation existante au moment de la mise à jour — n'écrit pas
    /// dans les fichiers.
    #[test]
    fn une_base_sans_le_reglage_n_ecrit_pas_dans_les_fichiers() {
        let db = base();
        assert!(
            !autorisee(&db),
            "clé absente : l'écriture doit être désactivée"
        );
    }

    #[test]
    fn seul_un_oui_explicite_active() {
        let db = base();
        let repo = SettingsRepo::with_backend(db.clone());
        repo.set(CLE, "true").unwrap();
        assert!(autorisee(&db));
        repo.set(CLE, "false").unwrap();
        assert!(!autorisee(&db));
        repo.set(CLE, "n'importe quoi").unwrap();
        assert!(!autorisee(&db));
    }

    #[test]
    fn formes_de_la_valeur() {
        assert!(!depuis_valeur(None));
        assert!(depuis_valeur(Some("true")));
        assert!(depuis_valeur(Some("\"true\"")));
        assert!(depuis_valeur(Some(" TRUE ")));
        assert!(depuis_valeur(Some("1")));
        assert!(!depuis_valeur(Some("")));
        assert!(!depuis_valeur(Some("0")));
        assert!(!depuis_valeur(Some("false")));
    }
}
