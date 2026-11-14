//! rc12 (décision du 13/11/2026) — le catalogue imprimable suit le filtre
//! « Hi-Res vérifié / douteux » de la Bibliothèque : `hires_verdict=` sur
//! `GET /api/v1/export/catalogue.html`, par la route montée.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

/// 1 vérifié, 2 douteux, 3 jamais mesuré (CD), 4 jamais mesuré (192 kHz).
fn bibliotheque(douteux_visible: bool) -> axum::Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("état serveur isolé");
    state
        .backend
        .execute_batch(&format!(
            "INSERT INTO albums (id, title, format, sample_rate, bit_depth, source) VALUES \
               (1, 'Vrai', 'flac', 96000, 24, 'local'), \
               (2, 'Faux', 'flac', 96000, 24, 'local'), \
               (3, 'Disque', 'flac', 44100, 16, 'local'), \
               (4, 'Studio', 'flac', 192000, 24, 'local'); \
             INSERT INTO tracks (id, title, album_id, file_path, format, source, duration_ms) VALUES \
               (11, 'a', 1, '/m/11.flac', 'flac', 'local', 60000), \
               (21, 'b', 2, '/m/21.flac', 'flac', 'local', 60000), \
               (31, 'c', 3, '/m/31.flac', 'flac', 'local', 60000), \
               (41, 'd', 4, '/m/41.flac', 'flac', 'local', 60000); \
             INSERT INTO hires_verifications (track_id, verdict, raison) VALUES \
               (11, 'verifie', NULL), (21, 'douteux', 'bande_de_cd'); \
             INSERT INTO settings (key, value) VALUES ('faux_hires_douteux_visible', '{douteux_visible}');"
        ))
        .expect("bibliothèque témoin");
    tune_server::routes::router(state)
}

async fn titres(app: &axum::Router, requete: &str) -> (StatusCode, Vec<String>, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!(
                "/api/v1/export/catalogue.html?covers=false&sort=title{requete}"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .expect("la route doit répondre");
    let status = resp.status();
    let html = String::from_utf8_lossy(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("corps"),
    )
    .to_string();
    let t = html
        .split("<td class=\"titre\">")
        .skip(1)
        .map(|s| s.split("</td>").next().unwrap_or("").to_string())
        .collect();
    (status, t, html)
}

#[tokio::test]
async fn le_catalogue_filtre_sur_le_verdict_hires_rc12() {
    let app = bibliotheque(true);
    let (s, t, html) = titres(&app, "&hires_verdict=verifie&lang=fr").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(t, vec!["Vrai"], "{html}");
    assert!(
        html.contains("Hi-Res vérifié"),
        "le filtre est nommé : {html}"
    );
    let (_, t, _) = titres(&app, "&hires_verdict=douteux").await;
    assert_eq!(t, vec!["Faux"]);
    let (_, t, _) = titres(&app, "&hires_verdict=verifie,douteux").await;
    assert_eq!(t, vec!["Faux", "Vrai"]);
    // OU avec la qualité, comme les pastilles : CD ou vérifié.
    let (_, t, _) = titres(&app, "&quality=cd&hires_verdict=verifie").await;
    assert_eq!(t, vec!["Disque", "Vrai"]);
    // Une seule qualité seule passe encore par la base, comme avant.
    let (_, t, _) = titres(&app, "&quality=cd").await;
    assert_eq!(t, vec!["Disque"]);
    // Un mot inconnu ne rend rien, jamais toute la bibliothèque.
    let (_, t, _) = titres(&app, "&hires_verdict=n_importe").await;
    assert!(t.is_empty(), "{t:?}");
    // Sans le paramètre : toute la bibliothèque.
    let (_, t, _) = titres(&app, "").await;
    assert_eq!(t.len(), 4);
}

#[tokio::test]
async fn douteux_masque_ne_sort_pas_du_catalogue_rc12() {
    let app = bibliotheque(false);
    let (_, t, _) = titres(&app, "&hires_verdict=douteux").await;
    assert!(t.is_empty(), "« douteux » masqué : rien — {t:?}");
}
