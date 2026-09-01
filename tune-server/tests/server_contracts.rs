//! Contrats serveur sans état global de processus.
//!
//! Ces modules utilisent des bases et routeurs isolés ; ils peuvent donc
//! partager un binaire de tests sans partager leurs données.

#[path = "albums_pas_des_doublons.rs"]
mod albums_pas_des_doublons;
#[path = "annonce_forum_bornes.rs"]
mod annonce_forum_bornes;
#[path = "auth_security.rs"]
mod auth_security;
#[path = "bios_bilan_visible.rs"]
mod bios_bilan_visible;
#[path = "bios_langue_album.rs"]
mod bios_langue_album;
#[path = "bios_langue_entete.rs"]
mod bios_langue_entete;
#[path = "bump_natifs_android.rs"]
mod bump_natifs_android;
#[path = "cles_developpeur_persistance.rs"]
mod cles_developpeur_persistance;
#[path = "collections_couleur_persistee.rs"]
mod collections_couleur_persistee;
#[path = "collections_ordre_albums.rs"]
mod collections_ordre_albums;
#[path = "config_secrets_et_roles.rs"]
mod config_secrets_et_roles;
#[path = "contexte_de_session.rs"]
mod contexte_de_session;
#[path = "credits_enrichissement_statut.rs"]
mod credits_enrichissement_statut;
#[path = "enrich_artiste_langue_entete.rs"]
mod enrich_artiste_langue_entete;
#[path = "enrichissement_repertoire.rs"]
mod enrichissement_repertoire;
#[path = "etiquettes_types_et_playlists.rs"]
mod etiquettes_types_et_playlists;
#[path = "explorateur_dossiers.rs"]
mod explorateur_dossiers;
#[path = "facettes_multivaleurs.rs"]
mod facettes_multivaleurs;
#[path = "favoris_cloisonnement_par_profil.rs"]
mod favoris_cloisonnement_par_profil;
#[path = "favoris_facettes_routes.rs"]
mod favoris_facettes_routes;
#[path = "generateur_playlists_sans_ia.rs"]
mod generateur_playlists_sans_ia;
#[path = "http_client_seam.rs"]
mod http_client_seam;
#[path = "integration.rs"]
mod integration;
#[path = "karaoke_plugin.rs"]
mod karaoke_plugin;
#[path = "licence_activation_immediate.rs"]
mod licence_activation_immediate;
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
#[path = "piste_artiste_par_id.rs"]
mod piste_artiste_par_id;
#[path = "playlist_manager_cloisonnement_par_profil.rs"]
mod playlist_manager_cloisonnement_par_profil;
#[path = "playlists_cloisonnement_par_profil.rs"]
mod playlists_cloisonnement_par_profil;
#[path = "playlists_ecritures_partielles.rs"]
mod playlists_ecritures_partielles;
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
#[path = "reprise_position_au_demarrage.rs"]
mod reprise_position_au_demarrage;
#[path = "rustsec_allowlists.rs"]
mod rustsec_allowlists;
#[path = "serveur_media_criteres_canoniques.rs"]
mod serveur_media_criteres_canoniques;
#[path = "smb_dialect_seam.rs"]
mod smb_dialect_seam;
#[path = "support_relais_marquer_lu.rs"]
mod support_relais_marquer_lu;
#[path = "tests_orphelins.rs"]
mod tests_orphelins;
#[path = "tranches_dynamic_range.rs"]
mod tranches_dynamic_range;
#[path = "tri_aleatoire_albums.rs"]
mod tri_aleatoire_albums;
#[path = "uptime_process_scope.rs"]
mod uptime_process_scope;
#[path = "volume_db_contrat.rs"]
mod volume_db_contrat;
#[path = "web_response_contracts.rs"]
mod web_response_contracts;
#[path = "workflows_bornes.rs"]
mod workflows_bornes;
#[path = "ws_auth.rs"]
mod ws_auth;
