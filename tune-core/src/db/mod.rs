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
pub mod cles_clients;
pub mod coffrets_auto;
pub mod collection_folder_repo;
/// Le conteneur de chaque élément de file (#6466).
pub mod contexte_de_file;
/// Un disque par dossier pour un album sans DISCNUMBER réparti en dossiers frères.
pub mod disques_par_dossier;
/// Les dossiers « Collections » suivent leurs albums (#5527, #5528).
pub mod dossiers_des_collections;
/// #7763 — les écoutes importées (Roon, Plex, m3u), à part de l'historique.
pub mod ecoutes_importees;
pub mod edition_album;
pub mod engine;
/// #7983 — la trace des tentatives de la passe d'enrichissement des métadonnées.
pub mod enrich_tentatives;
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
pub mod listening_resume;
/// Registre DURABLE des serveurs multimédia (#2219, phase 1) — sur le modèle
/// de `network_mounts` : l'intention d'un côté, le constat de l'autre.
pub mod media_server_repo;
pub mod metadata_proposal_repo;
pub mod metadata_report_repo;
pub mod migration_status;
pub mod migrations;
pub mod models;
/// #7849 — les notes personnelles en Markdown (artiste, album, piste).
pub mod notes_personnelles;
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
/// #7695 — les photos et vidéos indexées comme pistes par l'UPnP, retirées.
pub mod photos_indexees_upnp_7695;
pub mod play_queue_repo;
pub mod playlist_repo;
pub mod playlists_reconcile;
/// Contrôle parental, étape 1 (#7856) : politique d'un profil supervisé.
pub mod politique_parentale;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(all(test, feature = "postgres"))]
mod postgres_e2e;
pub mod profile_repo;
/// #6682 — le temps d'écoute radio par station (« Mon écoute »).
pub mod radio_ecoute;
pub mod radio_repo;
/// #6500 — les stations de radio indexées comme pistes par l'UPnP, retirées.
pub mod radios_indexees_upnp_6500;
pub mod rating_repo;
/// Fil 2138 — rattrapage unique des dates d'ajout figées au premier scan.
pub mod rattrapage_dates_ajout_2138;
pub mod rattrapage_metadonnees_5043;
/// La référence d'album d'une piste Bandcamp, retrouvée dans la file, les
/// favoris ou l'historique pour resigner son URL de flux (fil 2121).
pub mod reference_d_album;
/// #7849 — répartitions de la bibliothèque (codec, fréquence, décennie, rôle).
pub mod repartitions_bibliotheque;
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
mod filtre_dr_190_tests;
/// #7979 — la graphie accentuée des balises reprend la fiche artiste.
#[cfg(test)]
mod graphie_artiste_tests_7979;
#[cfg(test)]
mod lenteur_albums_4800_tests;
#[cfg(test)]
mod lenteur_pistes_5138_tests;
#[cfg(test)]
mod liste_par_curseur_7619_tests;
/// #7535 — un MBID trouvé par le nom n'est plus posé s'il est déjà porté.
#[cfg(test)]
mod mbid_deja_porte_7535_tests;
/// #7694 — la règle des dossiers miroirs ne fond pas deux qualités.
#[cfg(test)]
mod miroirs_de_qualite_7694_tests;
/// #7849 — notes personnelles et répartitions, SQLite et PostgreSQL.
#[cfg(test)]
mod notes_et_repartitions_7849_tests;
#[cfg(test)]
mod pochette_source_pg_tests_5034;
#[cfg(test)]
mod replieur_wal_tests;
/// Fil 2130 — « Reprendre l'écoute » : jointure en UNION ALL et index de
/// `listen_history.album_id`, preuves d'équivalence et de migration.
#[cfg(test)]
mod reprendre_l_ecoute_2130_tests;
#[cfg(test)]
mod tableau_de_bord_7634_tests;
#[cfg(test)]
mod top_titres_index_tests;
pub mod upnp_revision;
/// #5974 — un vidage qui échoue ne s'annonce plus réussi.
#[cfg(test)]
mod vidage_5974_tests;
