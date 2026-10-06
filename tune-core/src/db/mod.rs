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
pub mod champs_tenus;
pub mod coffrets_auto;
pub mod collection_folder_repo;
/// Un disque par dossier pour un album sans DISCNUMBER réparti en dossiers frères.
pub mod disques_par_dossier;
/// Les dossiers « Collections » suivent leurs albums (#5527, #5528).
pub mod dossiers_des_collections;
pub mod edition_album;
pub mod engine;
pub mod facet_filter;
pub mod favorite_facets_repo;
pub mod favorites_reconcile;
/// #5314 — le genre posé sur un album vaut pour ses pistes.
pub mod genre_album_pistes;
/// Albums masqués (#1391) — marqueurs réconciliés, sur le modèle des favoris.
pub mod hidden_repo;
pub mod history_repo;
pub mod home_queries;
/// Appareils ignorés (#1280) — faire taire un appareil, pas ses zones.
pub mod ignored_device_repo;
/// Lectures SQLite en cours, pour le relevé d'un gel de l'exécuteur (#5677).
pub mod lectures_en_cours;
/// Registre DURABLE des serveurs multimédia (#2219, phase 1) — sur le modèle
/// de `network_mounts` : l'intention d'un côté, le constat de l'autre.
pub mod media_server_repo;
pub mod metadata_proposal_repo;
pub mod metadata_report_repo;
pub mod migration_status;
pub mod migrations;
pub mod models;
/// L'ordre alphabétique des listes paginées (#4956) : tri en Rust, découpe.
pub(crate) mod ordre_alphabetique;
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
/// Fil 2138 — rattrapage unique des dates d'ajout figées au premier scan.
pub mod rattrapage_dates_ajout_2138;
pub mod rattrapage_metadonnees_5043;
/// La référence d'album d'une piste Bandcamp, retrouvée dans la file, les
/// favoris ou l'historique pour resigner son URL de flux (fil 2121).
pub mod reference_d_album;
/// Verrou d'écriture SQLite surveillé, attente hors de l'exécuteur (#4924).
pub(crate) mod replieur_wal;
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
pub mod verrou_ecriture;
pub mod zone_motif_masquage;
pub mod zone_repo;

#[cfg(test)]
mod album_dr_provenance_tests;
#[cfg(test)]
mod ecrivains_pendant_un_lot_de_scan_tests;
#[cfg(test)]
mod lenteur_albums_4800_tests;
#[cfg(test)]
mod lenteur_pistes_5138_tests;
#[cfg(test)]
mod pochette_source_pg_tests_5034;
#[cfg(test)]
mod replieur_wal_tests;
/// Fil 2130 — « Reprendre l'écoute » : jointure en UNION ALL et index de
/// `listen_history.album_id`, preuves d'équivalence et de migration.
#[cfg(test)]
mod reprendre_l_ecoute_2130_tests;
pub mod upnp_revision;
