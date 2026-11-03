//! Export CSV des favoris radio (signalé le 14/10 : fichier VIDE).
//!
//! Le bouton « Exporter en CSV » de l'onglet Radio des favoris est un
//! `<a href="/api/v1/radio-favorites/export" download>`. La route n'existait
//! pas : le GET tombait sur `/{fav_id}` (DELETE seulement), 405 au corps vide,
//! et le navigateur enregistrait ce corps vide.
//!
//! Ces témoins passent par le ROUTEUR MONTÉ (`crate::routes::router`) : les
//! favoris sont créés par la vraie route d'enregistrement, puis lus par la
//! vraie route d'export.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::json;
use tower::ServiceExt;

fn app() -> axum::Router {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    crate::routes::router(state)
}

async fn enregistrer(app: &axum::Router, corps: serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/radio-favorites")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

/// (statut, content-type, content-disposition, corps)
async fn exporter(app: &axum::Router) -> (StatusCode, String, String, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::get("/api/v1/radio-favorites/export")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = resp.status();
    let entete = |nom| {
        resp.headers()
            .get(nom)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let ct = entete(header::CONTENT_TYPE);
    let cd = entete(header::CONTENT_DISPOSITION);
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let corps = String::from_utf8(octets.to_vec()).expect("CSV en UTF-8 valide");
    (statut, ct, cd, corps)
}

const ENTETE: &str = "title;artist;station;cover_url;stream_url;saved_at";

#[tokio::test]
async fn l_export_rend_une_ligne_par_favori_avec_accents_et_virgules() {
    let app = app();
    enregistrer(
        &app,
        json!({
            "title": "Café, crème et « chanson »",
            "artist": "Édith Piaf",
            "station_name": "FIP, Paris",
            "stream_url": "https://icecast.example.net/fip.mp3"
        }),
    )
    .await;
    enregistrer(
        &app,
        json!({
            "title": "Ne me quitte pas; live",
            "artist": "Jacques Brel",
            "station_name": "Radio \"Classique\"",
        }),
    )
    .await;

    let (statut, ct, cd, corps) = exporter(&app).await;
    assert_eq!(statut, StatusCode::OK, "corps : {corps:?}");
    assert_eq!(ct, "text/csv; charset=utf-8");
    assert!(cd.contains("attachment"), "{cd}");
    assert!(cd.contains("radio_favorites.csv"), "{cd}");

    // BOM UTF-8 pour Excel, comme les autres exports CSV destinés au tableur.
    let sans_bom = corps
        .strip_prefix('\u{FEFF}')
        .expect("le CSV commence par le BOM UTF-8");
    let lignes: Vec<&str> = sans_bom.lines().collect();
    assert_eq!(
        lignes.len(),
        3,
        "en-tête + une ligne par favori : {lignes:#?}"
    );
    assert_eq!(lignes[0], ENTETE);

    // Ligne avec accents et virgules : la virgule n'est pas le séparateur, le
    // champ reste entier et les accents passent intacts.
    let piaf = lignes
        .iter()
        .find(|l| l.contains("Piaf"))
        .expect("ligne Piaf");
    assert!(
        piaf.starts_with(
            "Café, crème et « chanson »;Édith Piaf;FIP, Paris;;https://icecast.example.net/fip.mp3;"
        ),
        "{piaf}"
    );
    // saved_at est rempli (horodatage ISO-8601 écrit par la route).
    assert!(piaf.rsplit(';').next().unwrap().contains('T'), "{piaf}");

    // Point-virgule et guillemets : le champ est cité, le guillemet doublé.
    let brel = lignes
        .iter()
        .find(|l| l.contains("Brel"))
        .expect("ligne Brel");
    assert!(
        brel.starts_with("\"Ne me quitte pas; live\";Jacques Brel;\"Radio \"\"Classique\"\"\";;;"),
        "{brel}"
    );
}

/// Contre-épreuve : sans favori, l'export répond quand même 200 avec le BOM et
/// l'en-tête, et AUCUNE ligne de données — le nombre de lignes suit bien la
/// table, il n'est pas fabriqué.
#[tokio::test]
async fn sans_favori_l_export_ne_rend_que_l_en_tete() {
    let app = app();
    let (statut, ct, _, corps) = exporter(&app).await;
    assert_eq!(statut, StatusCode::OK, "corps : {corps:?}");
    assert_eq!(ct, "text/csv; charset=utf-8");
    assert_eq!(corps, format!("\u{FEFF}{ENTETE}\n"));
}
