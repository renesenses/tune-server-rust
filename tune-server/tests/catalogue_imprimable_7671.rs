//! Catalogue imprimable de la bibliothèque (#7671).
//!
//! `GET /api/v1/export/catalogue.html` rend un fichier HTML autonome : albums
//! avec pochette, artiste, année et format, filtrés par genre, artiste et
//! qualité, triés. Les épreuves passent par la ROUTE MONTÉE, sur une petite
//! bibliothèque de quatre albums dont un porte une vraie pochette sur le
//! disque.
//!
//! La propriété qui ne se voit pas à l'écran : aucun chemin de fichier de
//! l'utilisateur ne sort dans le catalogue. La pochette y entre en
//! `data:image/…`, jamais par son chemin.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

struct Bibliotheque {
    _tmp: tempfile::TempDir,
    app: axum::Router,
    racine: String,
    pochette: String,
}

fn petite_bibliotheque() -> Bibliotheque {
    let tmp = tempfile::tempdir().expect("dossier temporaire");
    let racine = tmp.path().to_string_lossy().to_string();
    let dossier = tmp.path().join("Miles Davis").join("Kind of Blue");
    std::fs::create_dir_all(&dossier).expect("dossier de l'album");
    let pochette = dossier.join("cover.png");
    image::RgbImage::from_pixel(400, 400, image::Rgb([200, 30, 30]))
        .save(&pochette)
        .expect("pochette témoin");
    let pochette = pochette.to_string_lossy().to_string();

    let state = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("état serveur isolé");
    state
        .backend
        .execute_batch(&format!(
            "INSERT INTO artists (id, name) VALUES (1, 'Miles Davis'); \
             INSERT INTO artists (id, name) VALUES (2, 'Kraftwerk'); \
             INSERT INTO artists (id, name) VALUES (3, 'Pink Floyd'); \
             INSERT INTO albums (id, title, artist_id, year, genre, format, sample_rate, bit_depth, cover_path, source) \
               VALUES (1, 'Kind of Blue', 1, 1959, 'Jazz', 'flac', 44100, 16, '{pochette}', 'local'); \
             INSERT INTO albums (id, title, artist_id, year, genre, genres, format, sample_rate, bit_depth, source) \
               VALUES (2, 'Tutu', 1, 1986, 'Jazz', '[\"Jazz\",\"Fusion\"]', 'flac', 96000, 24, 'local'); \
             INSERT INTO albums (id, title, artist_id, year, genre, format, sample_rate, bit_depth, source) \
               VALUES (3, 'Autobahn', 2, 1974, 'Electronic', 'mp3', 44100, 16, 'local'); \
             INSERT INTO albums (id, title, artist_id, year, genre, format, sample_rate, bit_depth, source) \
               VALUES (4, 'Animals', 3, 1977, 'Rock', 'flac', 192000, 24, 'local'); \
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, source) \
               VALUES (11, 'So What', 1, 1, '{r}/Miles Davis/Kind of Blue/01.flac', 'flac', 'local'); \
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, source) \
               VALUES (12, 'Tutu', 2, 1, '{r}/Miles Davis/Tutu/01.flac', 'flac', 'local'); \
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, source) \
               VALUES (13, 'Autobahn', 3, 2, '{r}/Kraftwerk/Autobahn/01.mp3', 'mp3', 'local'); \
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, source) \
               VALUES (14, 'Dogs', 4, 3, '{r}/Pink Floyd/Animals/02.flac', 'flac', 'local');",
            r = racine
        ))
        .expect("bibliothèque témoin");

    Bibliotheque {
        _tmp: tmp,
        app: tune_server::routes::router(state),
        racine,
        pochette,
    }
}

async fn catalogue(app: &axum::Router, requete: &str) -> (StatusCode, String, String, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1/export/catalogue.html{requete}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("la route doit répondre");
    let status = resp.status();
    let entete = |n: &str| {
        resp.headers()
            .get(n)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let (ct, cd) = (entete("content-type"), entete("content-disposition"));
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("corps de la réponse");
    (status, ct, cd, String::from_utf8_lossy(&bytes).to_string())
}

/// Les titres d'album du catalogue, dans l'ordre du document.
fn titres(html: &str) -> Vec<String> {
    html.split("<td class=\"titre\">")
        .skip(1)
        .map(|s| s.split("</td>").next().unwrap_or("").to_string())
        .collect()
}

#[tokio::test]
async fn catalogue_est_un_fichier_html_a_telecharger() {
    let b = petite_bibliotheque();
    let (status, ct, cd, html) = catalogue(&b.app, "").await;
    assert_eq!(status, StatusCode::OK);
    assert!(ct.starts_with("text/html"), "type : {ct}");
    assert!(
        cd.contains("attachment") && cd.contains("catalogue.html"),
        "{cd}"
    );
    assert!(html.starts_with("<!DOCTYPE html>"));
    assert!(
        html.contains("@media print"),
        "la feuille d'impression manque"
    );
    assert_eq!(
        titres(&html),
        ["Animals", "Autobahn", "Kind of Blue", "Tutu"],
        "tous les albums, par titre"
    );
    assert!(html.contains("FLAC 24/96"), "format de Tutu absent");
    assert!(html.contains("1959"), "année absente");
    assert!(html.contains("Kraftwerk"), "artiste absent");
}

#[tokio::test]
async fn catalogue_filtre_par_genre_y_compris_les_genres_secondaires() {
    let b = petite_bibliotheque();
    let (_, _, _, html) = catalogue(&b.app, "?genre=jazz").await;
    assert_eq!(titres(&html), ["Kind of Blue", "Tutu"], "genre=jazz");
    let (_, _, _, html) = catalogue(&b.app, "?genre=Fusion").await;
    assert_eq!(titres(&html), ["Tutu"], "genre=Fusion (tableau genres)");
}

#[tokio::test]
async fn catalogue_filtre_par_artiste_et_haute_resolution() {
    let b = petite_bibliotheque();
    let (_, _, _, html) = catalogue(&b.app, "?quality=hires").await;
    assert_eq!(titres(&html), ["Animals", "Tutu"], "quality=hires");
    let (_, _, _, html) = catalogue(&b.app, "?artist=miles%20davis").await;
    assert_eq!(
        titres(&html),
        ["Kind of Blue", "Tutu"],
        "artist=miles davis"
    );
    let (_, _, _, html) = catalogue(&b.app, "?artist_id=1&quality=hires").await;
    assert_eq!(titres(&html), ["Tutu"], "artiste ET hi-res");
    let (_, _, _, html) = catalogue(&b.app, "?quality=hires,lossy").await;
    assert_eq!(
        titres(&html),
        ["Animals", "Autobahn", "Tutu"],
        "deux qualités : OU entre elles"
    );
}

#[tokio::test]
async fn catalogue_trie_par_annee_et_par_artiste() {
    let b = petite_bibliotheque();
    let (_, _, _, html) = catalogue(&b.app, "?sort=year&order=desc").await;
    assert_eq!(
        titres(&html),
        ["Tutu", "Animals", "Autobahn", "Kind of Blue"],
        "année décroissante"
    );
    let (_, _, _, html) = catalogue(&b.app, "?sort=artist").await;
    assert_eq!(
        titres(&html),
        ["Autobahn", "Kind of Blue", "Tutu", "Animals"],
        "artiste croissant"
    );
}

#[tokio::test]
async fn catalogue_integre_la_pochette_sans_aucun_chemin_de_fichier() {
    let b = petite_bibliotheque();
    let (_, _, _, html) = catalogue(&b.app, "").await;
    assert!(
        html.contains("<img src=\"data:image/"),
        "la pochette doit être intégrée au fichier"
    );
    assert!(
        !html.contains(&b.pochette),
        "le chemin de la pochette sort dans le catalogue"
    );
    assert!(
        !html.contains(&b.racine),
        "un chemin de la bibliothèque sort dans le catalogue"
    );
    assert!(
        !html.contains("cover.png"),
        "le nom du fichier de pochette sort"
    );
    assert!(!html.contains(".flac"), "un nom de fichier audio sort");
    // Sans pochettes : aucune image du tout.
    let (_, _, _, html) = catalogue(&b.app, "?covers=false").await;
    assert!(!html.contains("<img"), "covers=false garde des images");
}

#[tokio::test]
async fn catalogue_vide_et_libelles_en_francais() {
    let b = petite_bibliotheque();
    let (status, _, _, html) = catalogue(&b.app, "?genre=Baroque&lang=fr").await;
    assert_eq!(status, StatusCode::OK);
    assert!(titres(&html).is_empty());
    assert!(html.contains("Aucun album ne correspond"), "{html}");
    assert!(html.contains("lang=\"fr\""));
}
