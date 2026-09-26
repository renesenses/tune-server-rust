/// Apparier un titre connu sur la bibliothèque LOCALE (#4716) — le pendant de
/// `streaming::matching` pour le sens service → bibliothèque.
pub mod appariement_bibliotheque;
pub mod artwork;
pub mod artwork_cache;
pub mod artwork_proxy;
pub mod audit;
pub mod cover_fetcher;
pub mod duplicate_detector;
/// Les exemplaires d'une piste dans plusieurs répertoires (#4907).
pub mod exemplaires;
pub mod export;
pub mod folder_playlists;
pub mod full_text_search;
pub mod genre_tree;
pub mod importer;
pub mod ingest;
pub mod local_path;
pub mod lyrics_pass;
pub mod m3u_parser;
pub mod playlist_scan;
/// La pochette d'un album face au disque : retrait et suivi (#5034).
pub mod pochette_disque;
pub mod pont_roon;
/// Appliquer un export du pont Roon (crédits, images) contre la base.
pub mod pont_roon_import;
pub mod quality;
pub mod smart_collections;
pub mod track_matcher;
