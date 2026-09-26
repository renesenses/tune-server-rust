/// « Ces deux albums ne sont pas des doublons » (#1276) — paires arbitrées par
/// l'utilisateur, réconciliées sur le modèle des favoris et des masquages.
pub(crate) mod absorption;
pub mod album_distinct_repo;
/// La fusion des albums en double, commune au manuel, au scan et au nettoyage.
pub mod album_doublons;
pub mod album_metadata_repo;
pub mod album_repo;
pub mod artist_repo;
pub mod backend;
pub mod collection_folder_repo;
pub mod engine;
pub mod facet_filter;
pub mod favorite_facets_repo;
pub mod favorites_reconcile;
/// Albums masqués (#1391) — marqueurs réconciliés, sur le modèle des favoris.
pub mod hidden_repo;
pub mod history_repo;
pub mod home_queries;
/// Appareils ignorés (#1280) — faire taire un appareil, pas ses zones.
pub mod ignored_device_repo;
/// Registre DURABLE des serveurs multimédia (#2219, phase 1) — sur le modèle
/// de `network_mounts` : l'intention d'un côté, le constat de l'autre.
pub mod media_server_repo;
pub mod metadata_proposal_repo;
pub mod metadata_report_repo;
pub mod migration_status;
pub mod migrations;
pub mod models;
#[cfg(all(test, feature = "postgres"))]
mod pg_ensure_schema_parity;
#[cfg(all(test, feature = "postgres"))]
mod pg_gardes_schema_5003;
#[cfg(feature = "postgres")]
pub mod pg_migrate;
#[cfg(all(test, feature = "postgres"))]
mod pg_schema_parity;
#[cfg(all(test, feature = "postgres"))]
mod pg_sqlite_type_parity;
pub mod play_queue_repo;
pub mod playlist_repo;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(all(test, feature = "postgres"))]
mod postgres_e2e;
pub mod profile_repo;
pub mod radio_repo;
pub mod rating_repo;
pub mod rattrapage_metadonnees_5043;
pub mod settings_repo;
pub mod source_link_repo;
pub mod sqlite;
pub mod streaming_favorites_repo;
pub mod tag_repo;
/// Registre des executions automatisees (#2080).
pub mod task_run_repo;
pub mod track_metadata_repo;
pub mod track_repo;
/// Qui tient la transaction ouverte sur la connexion d'écriture, et qui
/// attend qu'elle se ferme.
pub(crate) mod transaction_du_lot;
pub mod tx_holder;
/// Verrou d'écriture SQLite surveillé, attente hors de l'exécuteur (#4924).
pub mod verrou_ecriture;
pub mod zone_repo;

#[cfg(test)]
mod album_dr_provenance_tests;
#[cfg(test)]
mod ecrivains_pendant_un_lot_de_scan_tests;
#[cfg(test)]
mod lenteur_albums_4800_tests;
#[cfg(test)]
mod pochette_source_pg_tests_5034;
pub mod upnp_revision;
