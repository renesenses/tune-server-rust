//! #9265 — le nombre d'artistes des statistiques suit la grille Artistes :
//! un crédit composé relié à ses artistes (« A & B ») n'y compte plus, ses
//! artistes y comptent. Mêmes chiffres sur la grille, `GET /library/stats`,
//! `GET /library/stats/completeness` et `GET /system/database/status`.
use super::super::facets::socle_5977_tests::{bibliotheque, get};

#[tokio::test]
async fn les_statistiques_comptent_les_artistes_de_la_grille_9265() {
    let (app, state) = bibliotheque();
    // La fixture : 3 artistes d'album (Alda, Masqué, Jazzman). On y ajoute
    // deux musiciens sans album à eux et leur duo, qui en a un.
    state
        .backend
        .execute_batch(
            "INSERT INTO artists (id, name) VALUES (20, 'Pianiste Neuf'), \
             (21, 'Bassiste Neuf'), (22, 'Pianiste Neuf & Bassiste Neuf');\n\
             INSERT INTO albums (id, title, artist_id, source) VALUES (30, 'Duo', 22, 'local');\n\
             INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, \
             file_path, duration_ms, format, source) VALUES (930, 'Duo 1', 30, 22, 1, 1, \
             '/m/Duo/01.flac', 300000, 'flac', 'local');",
        )
        .expect("duo");
    let totaux = |grille: &serde_json::Value,
                  stats: &serde_json::Value,
                  completude: &serde_json::Value,
                  base: &serde_json::Value| {
        (
            grille["total"].as_i64().unwrap(),
            stats["artists"].as_i64().unwrap(),
            completude["total_artists"].as_i64().unwrap(),
            completude["artists_without_image"].as_i64().unwrap(),
            base["artists"].as_i64().unwrap(),
        )
    };
    let lire = || async {
        (
            get(&app, "/api/v1/library/artists?limit=100").await,
            get(&app, "/api/v1/library/stats").await,
            get(&app, "/api/v1/library/stats/completeness").await,
            get(&app, "/api/v1/system/database/status").await,
        )
    };
    // Sans liaison : le duo est un artiste d'album comme un autre.
    let (g, s, c, b) = lire().await;
    assert_eq!(totaux(&g, &s, &c, &b), (4, 4, 4, 4, 4), "{s} {c} {b}");

    let bilan =
        tune_core::db::artistes_des_credits::recalculer(&state.backend, &[]).expect("recalcul");
    assert_eq!(bilan.credits, 1, "{bilan:?}");
    // Le duo sort, ses deux musiciens entrent : 3 + 2.
    let (g, s, c, b) = lire().await;
    assert_eq!(totaux(&g, &s, &c, &b), (5, 5, 5, 5, 5), "{s} {c} {b}");
}
