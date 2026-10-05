#![recursion_limit = "256"]
// Les gestionnaires axum et leurs gardes (`require_premium`, `require_admin`…)
// rendent `Result<_, axum::response::Response>` : c'est l'idiome d'axum, et
// `Response` pèse 128 octets. Emballer l'erreur dans une `Box` changerait la
// signature de dizaines de routes pour un gain nul sur un chemin qui n'est pas
// chaud (clippy 1.98, `result_large_err`).
#![allow(clippy::result_large_err)]

mod adresse_d_accueil;
pub mod auth;
pub mod auto_resume;
pub mod auto_scan;
pub mod background;
pub mod background_tasks;
#[cfg(feature = "bandcamp")]
pub mod bandcamp_sweep;
/// #5461 — le binaire installé, distinct de celui qui tourne (relance après
/// mise à jour, lancement depuis `<exe>.old`).
pub mod binaire_installe;
pub mod boot_status;
pub mod bootstrap;
/// L'adresse de première connexion, imprimée au démarrage (#1272).
pub mod catalogue_services;
/// Pourquoi un dossier configuré est-il injoignable, et que peut y faire
/// l'utilisateur. Voir [`chemin_inaccessible`] pour le cas Windows.
pub mod chemin_inaccessible;
/// #5513 — les chemins de données retenus au démarrage.
pub mod chemins_de_donnees;
pub mod config;
pub mod discovery_setup;
/// Plancher de fils de travail du moteur tokio (fil 2124, #5677).
pub mod fils_de_travail;
/// Détecteur de gel de l'exécuteur et relevé automatique (#4924).
pub mod gel_executeur;
mod instance_existante;
pub use tune_http_types::error;
pub mod i18n;
pub mod journal;
mod lecture_bornee;
pub mod lien_de_partage;
/// Rendre au système, à froid, la mémoire libre que garde l'allocateur.
pub mod memoire_a_froid;
/// #4677 — relevé, au démarrage, des règles du pare-feu Windows pour
/// `tune-server.exe` (lecture seule, une ligne de journal).
pub mod pare_feu_windows;
pub mod plugins;
/// P2 of the plugin ABI: AppState-backed [`HostContext`] plus the registry of
/// loaded wasm plugins. Gated behind `plugins-wasm`; absent from default builds.
#[cfg(feature = "plugins-wasm")]
pub mod plugins_host;
pub mod premium_guard;
pub mod reprise_des_passes;
pub mod routes;
pub mod scan_import;
/// L'echelle de dialectes CIFS, partagee par la route de montage et par le
/// remontage au demarrage. Voir [`smb`] pour ce que leur divergence coutait.
pub mod smb;
mod sqlite_write_gate;
pub mod startup;
pub mod state;
#[cfg(target_os = "linux")]
mod tune_os_password;
pub mod windows_migrate;

#[cfg(test)]
mod aleatoire_selection_5526_tests;
// #5512 : compilé partout, actif seulement en build de test (lib ET
// intégration) — voir `isolement_disque_tests_5467::actif`.
mod isolement_disque_tests_5467;
#[cfg(test)]
mod labels_albums_4836_tests;

/// The whole server startup, so out-of-tree binaries can compose it with their
/// own plugins. See [`bootstrap::run`].
pub use bootstrap::run;

mod premium_audio_plugins;

mod audio_job_journal;
mod catalogue_greffons_audio;
mod native_audio;
