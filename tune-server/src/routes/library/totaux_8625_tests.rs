//! #8625 (fil 2787, Pierre M, décision de Bertrand du 13/11) — même base,
//! mêmes totaux sur chaque route.
//!
//! Fixture de #5977 : 19 lignes de pistes, dont 8 montrées (3 copies de
//! moindre qualité, 6 pistes d'un album UPnP doublé par le local, 2 pistes
//! d'un album masqué) ; 4 albums, dont 2 montrés.
//!
//! Avant : `GET /library/stats` disait 19 / 4 (compte des tables) quand
//! Titres disait 8 et Albums 2.

use super::super::facets::socle_5977_tests::{bibliotheque, get};

#[tokio::test]
async fn la_meme_base_donne_les_memes_totaux_sur_chaque_route_8625() {
    let (app, _state) = bibliotheque();
    let stats = get(&app, "/api/v1/library/stats").await;
    let totaux = &stats["library_totals"];

    // Titres : les deux chemins de la liste (sans tri, et paginé trié).
    let titres = get(&app, "/api/v1/library/tracks?limit=100").await;
    let titres_tries = get(&app, "/api/v1/library/tracks?limit=100&order=asc").await;
    // Albums « Tout » : la grille paginée.
    let albums = get(&app, "/api/v1/library/albums?limit=100").await;

    assert_eq!(titres["total"], 8, "{titres}");
    assert_eq!(
        totaux["tracks"], titres["total"],
        "Statistiques ≠ Titres : {totaux}"
    );
    assert_eq!(
        totaux["tracks"], titres_tries["total"],
        "Titres trié : {titres_tries}"
    );
    assert_eq!(albums["total"], 2, "{albums}");
    assert_eq!(
        totaux["albums"], albums["total"],
        "Statistiques ≠ Albums : {totaux}"
    );

    // L'écart reste lisible : ce que les tables gardent sans le montrer.
    assert_eq!(totaux["stored_tracks"], 19, "{totaux}");
    assert_eq!(totaux["stored_albums"], 4, "{totaux}");
    assert_eq!(totaux["hidden_or_duplicate_tracks"], 11, "{totaux}");
    assert_eq!(totaux["hidden_or_duplicate_albums"], 2, "{totaux}");
    // #2147 : les clés historiques gardent le compte des tables.
    assert_eq!(stats["tracks"], 19);
    assert_eq!(stats["albums"], 4);
}

/// Répertoire (Pierre : 155 513 contre 155 539 en Titres) : un dossier ne
/// compte que les pistes dont `file_path` est sous lui. Une piste découpée
/// par un CUE a `file_path` NUL (son fichier est dans `cue_media_path`) : elle
/// est dans les titres et dans `library_totals`, pas dans les dossiers — ce
/// que dit l'infobulle de la facette côté web.
#[tokio::test]
async fn le_repertoire_ne_compte_pas_les_pistes_cue_8625() {
    let (app, state) = bibliotheque();
    state
        .backend
        .execute_batch(
            "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, \
             file_path, cue_media_path, duration_ms, format, sample_rate, bit_depth, source, year) \
             VALUES (900, 'Piste CUE', 13, 3, 1, 9, NULL, '/m/Jazzman/Autre/disque.flac', \
             300000, 'flac', 44100, 16, 'local', 2021);",
        )
        .expect("piste CUE");
    let stats = get(&app, "/api/v1/library/stats").await;
    let titres = get(&app, "/api/v1/library/tracks?limit=100").await;
    // Sous `/m` (la racine se déduit mal ici : l'album UPnP a des adresses
    // http), les dossiers d'artistes : Alda 6, Jazzman 2.
    let dossiers = get(&app, "/api/v1/library/folder-facet?path=/m").await;
    let somme: i64 = dossiers["children"]
        .as_array()
        .map(|c| c.iter().filter_map(|d| d["count"].as_i64()).sum())
        .unwrap_or(0);
    assert_eq!(titres["total"], 9, "{titres}");
    assert_eq!(stats["library_totals"]["tracks"], 9, "{stats}");
    assert_eq!(
        somme, 8,
        "la piste CUE n'est sous aucun dossier : {dossiers}"
    );
}
