//! Contrats serveur sans état global de processus.
//!
//! Ces modules utilisent des bases et routeurs isolés ; ils peuvent donc
//! partager un binaire de tests sans partager leurs données.

#[path = "annonce_forum_bornes.rs"]
mod annonce_forum_bornes;
#[path = "auth_security.rs"]
mod auth_security;
#[path = "bios_langue_album.rs"]
mod bios_langue_album;
#[path = "bump_natifs_android.rs"]
mod bump_natifs_android;
#[path = "collections_ordre_albums.rs"]
mod collections_ordre_albums;
#[path = "enrichissement_repertoire.rs"]
mod enrichissement_repertoire;
#[path = "etiquettes_types_et_playlists.rs"]
mod etiquettes_types_et_playlists;
#[path = "facettes_multivaleurs.rs"]
mod facettes_multivaleurs;
#[path = "favoris_facettes_routes.rs"]
mod favoris_facettes_routes;
#[path = "http_client_seam.rs"]
mod http_client_seam;
#[path = "integration.rs"]
mod integration;
#[path = "karaoke_plugin.rs"]
mod karaoke_plugin;
#[path = "licence_grace_visible.rs"]
mod licence_grace_visible;
#[path = "notarisation_bornes.rs"]
mod notarisation_bornes;
#[path = "output_provider_seam.rs"]
mod output_provider_seam;
#[path = "paroles_ecriture_fichiers.rs"]
mod paroles_ecriture_fichiers;
#[path = "paroles_source_lrclib.rs"]
mod paroles_source_lrclib;
#[path = "podcasts_radiofrance_cle.rs"]
mod podcasts_radiofrance_cle;
#[path = "radios_recherche_distinction.rs"]
mod radios_recherche_distinction;
#[path = "radios_validation_url.rs"]
mod radios_validation_url;
#[path = "rbac.rs"]
mod rbac;
#[path = "reidentification_album.rs"]
mod reidentification_album;
#[path = "rustsec_allowlists.rs"]
mod rustsec_allowlists;
#[path = "smb_dialect_seam.rs"]
mod smb_dialect_seam;
#[path = "support_relais_marquer_lu.rs"]
mod support_relais_marquer_lu;
#[path = "tests_orphelins.rs"]
mod tests_orphelins;
#[path = "uptime_process_scope.rs"]
mod uptime_process_scope;
#[path = "web_response_contracts.rs"]
mod web_response_contracts;
#[path = "workflows_bornes.rs"]
mod workflows_bornes;
#[path = "ws_auth.rs"]
mod ws_auth;
