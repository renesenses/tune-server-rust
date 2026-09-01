//! Contrats des plugins natifs partageant le même répertoire temporaire.
//!
//! # Un garde rangé dans un `static` ne s'exécute jamais (#3030)
//!
//! Rust **ne détruit pas** les variables statiques à la fin du processus :
//! `Drop` n'est pas appelé sur `PLUGIN_DATA_DIR`, et le `TempDir` qu'il tient
//! ne supprime donc rien. Chaque exécution de ce binaire laissait un dossier
//! anonyme `/tmp/.tmpXXXXXX`.
//!
//! Le recensement de #3030 ne l'avait pas vu : il comptait les entrées
//! `tune-*`, or ce dossier-ci porte le préfixe de `tempfile`. Mesuré sur la
//! machine de compilation le 01/09/2026 : **149 dossiers `.tmp*`**, tous
//! porteurs des mêmes quatre fichiers (`fails`, `injected`, `loads`,
//! `optin`) — c'est-à-dire tous nés ici.
//!
//! Le dossier ne peut pas être tenu par une portée : `TUNE_PLUGINS_DATA_DIR`
//! est lu par **tous** les tests du binaire, donc il doit vivre plus longtemps
//! que n'importe lequel d'entre eux. La seule fin de vie qui existe ici est
//! celle du processus, et c'est `atexit` qui la donne — il s'exécute quand
//! `libtest` sort, y compris après un test en échec.

// Le dossier doit survivre à tous les tests du binaire, donc à toute portée.
// tmp-autorise: repris par `menage_a_la_sortie_du_processus`, pas abandonné.
static PLUGIN_DATA_DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();

/// Fait supprimer `chemin` quand le processus se termine.
///
/// `Drop` ne peut rien ici — il ne s'exécute pas sur un `static`. `atexit`,
/// lui, se déclenche à la sortie de `libtest`, que la suite ait réussi ou
/// échoué. Restent hors de portée l'`abort` et le `SIGKILL` : un binaire de
/// test tué ne nettoie rien, et aucun mécanisme en processus ne le pourrait.
#[cfg(unix)]
fn menage_a_la_sortie_du_processus(chemin: std::path::PathBuf) {
    static CHEMIN: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

    /// Le gestionnaire ne prend pas d'argument : `atexit` impose la
    /// signature, d'où le passage par `CHEMIN`.
    extern "C" fn balayer() {
        if let Some(chemin) = CHEMIN.get() {
            // Résultat ignoré : rien à signaler à la sortie d'un processus,
            // et un dossier déjà disparu reste un succès.
            let _ = std::fs::remove_dir_all(chemin);
        }
    }

    if CHEMIN.set(chemin).is_ok() {
        // Safety: `atexit` n'est appelé qu'une fois — `OnceLock::set` ne rend
        // `Ok` qu'au premier passage — et `balayer` ne lit que `CHEMIN`, déjà
        // écrit, sans jamais toucher à un état que la sortie a démonté.
        unsafe {
            libc::atexit(balayer);
        }
    }
}

/// Sur les cibles sans `atexit` le dossier survit ; les machines concernées
/// sont les coureurs jetables de la CI, où le résidu meurt avec la machine.
#[cfg(not(unix))]
fn menage_a_la_sortie_du_processus(_chemin: std::path::PathBuf) {}

fn use_scratch_plugin_data_dir() {
    PLUGIN_DATA_DIR.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        menage_a_la_sortie_du_processus(dir.path().to_path_buf());
        // Safety: this OnceLock performs the only write in this process, before
        // the caller constructs an AppState that can read the variable.
        unsafe {
            std::env::set_var("TUNE_PLUGINS_DATA_DIR", dir.path());
        }
        dir
    });
}

#[path = "dj_plugin.rs"]
mod dj_plugin;
#[path = "plugin_routes.rs"]
mod plugin_routes;
