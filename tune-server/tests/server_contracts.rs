//! Contrats serveur sans état global de processus.
//!
//! Ces modules utilisent des bases et routeurs isolés ; ils peuvent donc
//! partager un binaire de tests sans partager leurs données.

#[path = "auth_security.rs"]
mod auth_security;
#[path = "bump_natifs_android.rs"]
mod bump_natifs_android;
#[path = "http_client_seam.rs"]
mod http_client_seam;
#[path = "integration.rs"]
mod integration;
#[path = "karaoke_plugin.rs"]
mod karaoke_plugin;
#[path = "notarisation_bornes.rs"]
mod notarisation_bornes;
#[path = "output_provider_seam.rs"]
mod output_provider_seam;
#[path = "rbac.rs"]
mod rbac;
#[path = "smb_dialect_seam.rs"]
mod smb_dialect_seam;
#[path = "tests_orphelins.rs"]
mod tests_orphelins;
#[path = "workflows_bornes.rs"]
mod workflows_bornes;
#[path = "ws_auth.rs"]
mod ws_auth;
