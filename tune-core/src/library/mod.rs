/// Apparier un titre connu sur la bibliothèque LOCALE (#4716) — le pendant de
/// `streaming::matching` pour le sens service → bibliothèque.
pub mod appariement_bibliotheque;
/// Les artistes du même label : bibliothèque, puis catalogue Qobuz (#6782).
pub mod artistes_du_label;
pub mod artwork;
pub mod artwork_cache;
pub mod artwork_proxy;
pub mod audit;
pub mod cover_fetcher;
/// #6332 — compléter au rescan le débit des pistes déjà en base.
pub mod debit_des_pistes;
/// #6917 — ne dédoublonner que les copies d'une même racine de musique.
pub mod dedoublonnage_par_racine;
pub mod duplicate_detector;
/// Les exemplaires d'une piste dans plusieurs répertoires (#4907).
pub mod exemplaires;
pub mod export;
pub mod folder_playlists;
pub mod full_text_search;
pub mod genre_tree;
pub mod genres_equivalents;
/// Regrouper les exemplaires d'un même enregistrement entre sources, et
/// choisir celui qu'on joue (#2264).
pub mod groupes_versions;
pub mod importer;
pub mod ingest;
pub mod local_path;
/// Le logo d'un label de la bibliothèque, lu chez Qobuz (#6736).
pub mod logos_labels;
/// #7089 — les logos de labels pris sur le web ouvert (Commons, Wikipédia,
/// site officiel, Discogs), après un rapprochement MusicBrainz sûr.
pub mod logos_web;
pub mod lyrics_pass;
pub mod m3u_parser;
pub mod mosaique;
pub mod noms_de_labels;
pub mod playlist_scan;
/// La pochette d'un album face au disque : retrait et suivi (#5034).
pub mod pochette_disque;
pub mod pochette_visuelle;
pub mod pont_roon;
/// Appliquer un export du pont Roon (crédits, images) contre la base.
pub mod pont_roon_import;
pub mod quality;
/// LA règle « compilation » (25/09/2026) — un seul endroit, voir le module.
pub mod regle_compilation;
/// Où vit la règle de choix des versions : par profil, puis globale (#2264).
pub mod regle_de_version;
/// La révision de la bibliothèque, pour le cache du client (#7619, L5).
pub mod revision;
pub mod smart_collections;
pub mod track_matcher;
/// Les exemplaires d'un enregistrement qui sont en base (#2264).
pub mod versions_en_base;
