/// « Ces deux albums ne sont pas des doublons » (#1276) — paires arbitrées par
/// l'utilisateur, réconciliées sur le modèle des favoris et des masquages.
pub mod album_distinct_repo;
pub mod album_metadata_repo;
pub mod album_repo;
pub mod artist_repo;
pub mod backend;
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
pub mod metadata_proposal_repo;
pub mod metadata_report_repo;
pub mod migration_status;
pub mod migrations;
pub mod models;
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
pub mod settings_repo;
pub mod source_link_repo;
pub mod sqlite;
pub mod streaming_favorites_repo;
pub mod tag_repo;
/// Registre des executions automatisees (#2080).
pub mod task_run_repo;
pub mod track_metadata_repo;
pub mod track_repo;
pub mod tx_holder;
pub mod zone_repo;
