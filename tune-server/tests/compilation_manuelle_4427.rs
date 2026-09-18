//! #4427 — le drapeau « compilation » posé À LA MAIN, et qui tient.
//!
//! Bertrand, 18/09/2026 : *Coco María Presents* éclaté en douze vignettes, une
//! par artiste de piste. Trois choses étaient vraies avant ce lot :
//!
//! 1. aucune route n'écrivait `albums.is_compilation` — seul le scan le faisait ;
//! 2. `mark_compilation` ne savait que le **lever**, jamais le baisser ;
//! 3. rien ne survivait au re-scan.
//!
//! Ce fichier cloue les trois gardes demandées par le ticket. Il passe par la
//! ROUTE, pas par le dépôt : ce qui doit être prouvé est le câblage.
//!
//! ⚠️ `tune-server` porte `autotests = false` — déclaré dans `server_contracts.rs`.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::album_repo::AlbumRepo;
use tune_server::state::AppState;

fn banc() -> (axum::Router, AppState) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state.clone());
    state
        .backend
        .execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'Coco María'), (2, 'Un autre');\
             INSERT INTO albums (id, title, artist_id) VALUES \
               (1, 'Coco María Presents', 1), (2, 'Coco María Presents', 2), \
               (3, 'Un vrai album solo', 2);",
        )
        .unwrap();
    (app, state)
}

async fn poser(app: &axum::Router, corps: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/library/albums/compilation")
                .header("content-type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// ⭐ La garde principale : une décision manuelle SURVIT au scan, dans les
/// DEUX sens.
///
/// - posée sur un album que le scan ne voit pas comme une compilation : le
///   scan ne la défait pas ;
/// - retirée d'un album que le scan y voyait : `mark_compilation` ne la relève
///   pas.
///
/// Sans la garde `compilation_manuelle IS NULL` de `mark_compilation`, la
/// seconde moitié de ce test tombe — et c'est exactement ce que le ticket
/// appelle « l'écran ment ».
#[tokio::test]
async fn une_decision_manuelle_survit_au_scan_dans_les_deux_sens() {
    let (app, state) = banc();
    let repo = AlbumRepo::with_backend(state.backend.clone());

    // 1. Posée à la main sur l'album 3, que le scan n'aurait jamais levé.
    let (statut, corps) = poser(&app, json!({"album_ids": [3], "valeur": true})).await;
    assert_eq!(statut, StatusCode::OK, "corps: {corps}");
    assert_eq!(corps["poses"].as_i64(), Some(1));
    assert_eq!(repo.compilation_manuelle(3).unwrap(), Some(true));

    // 2. Retirée à la main sur l'album 1. Le scan repasse : il ne la relève pas.
    let (statut, _) = poser(&app, json!({"album_ids": [1], "valeur": false})).await;
    assert_eq!(statut, StatusCode::OK);
    repo.mark_compilation(1).unwrap();
    assert_eq!(
        repo.compilation_manuelle(1).unwrap(),
        Some(false),
        "le scan a défait un geste de l'utilisateur"
    );
    assert!(
        !repo.get(1).unwrap().unwrap().is_compilation,
        "is_compilation doit rester aligné sur la décision manuelle"
    );

    // 3. Le témoin : un album sur lequel PERSONNE n'a tranché reste au scan.
    assert_eq!(repo.compilation_manuelle(2).unwrap(), None);
    repo.mark_compilation(2).unwrap();
    assert!(
        repo.get(2).unwrap().unwrap().is_compilation,
        "sans décision manuelle, le scan décide comme avant"
    );
}

/// Poser le drapeau sur plusieurs albums avec `fusionner` les réunit en un
/// seul disque — le geste que Bertrand veut sur ses douze vignettes.
#[tokio::test]
async fn poser_avec_fusion_reunit_les_eclats_en_un_album() {
    let (app, state) = banc();
    let repo = AlbumRepo::with_backend(state.backend.clone());

    let (statut, corps) = poser(
        &app,
        json!({"album_ids": [1, 2], "valeur": true, "fusionner": true}),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "corps: {corps}");
    assert_eq!(corps["poses"].as_i64(), Some(2));
    assert_eq!(corps["fusionnes"].as_i64(), Some(1), "corps: {corps}");
    assert_eq!(corps["album_cible"].as_i64(), Some(1));
    assert!(repo.get(2).unwrap().is_none(), "l'éclat doit avoir disparu");
    assert_eq!(repo.compilation_manuelle(1).unwrap(), Some(true));
}

/// ⚠️ Retirer le drapeau NE DÉFAIT PAS une fusion. Les deux gestes sont
/// distincts, et confondre un décochage avec une commande destructrice serait
/// le pire des malentendus sur cet écran.
#[tokio::test]
async fn retirer_le_drapeau_ne_defait_aucune_fusion() {
    let (app, state) = banc();
    let repo = AlbumRepo::with_backend(state.backend.clone());
    poser(
        &app,
        json!({"album_ids": [1, 2], "valeur": true, "fusionner": true}),
    )
    .await;

    let (statut, corps) = poser(
        &app,
        json!({"album_ids": [1], "valeur": false, "fusionner": true}),
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(
        corps["fusionnes"].as_i64(),
        Some(0),
        "aucune fusion ne doit être tentée en retirant : {corps}"
    );
    assert!(
        repo.get(2).unwrap().is_none(),
        "l'album absorbé ne revient pas, et c'est voulu"
    );
    assert_eq!(repo.compilation_manuelle(1).unwrap(), Some(false));
}

/// Une demande vide est refusée plutôt que comptée comme un succès à zéro.
#[tokio::test]
async fn une_demande_sans_album_est_refusee() {
    let (app, _state) = banc();
    let (statut, corps) = poser(&app, json!({"album_ids": [], "valeur": true})).await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
    assert_eq!(corps["motif"], "album_ids_vide");
}

/// La gravure est une SECONDE action, et elle grave la décision manuelle — pas
/// une déduction du scan. Un album sur lequel personne n'a tranché est nommé
/// dans la réponse plutôt que gravé en silence.
#[tokio::test]
async fn la_gravure_refuse_un_album_sans_decision_manuelle() {
    let (app, _state) = banc();
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/library/albums/compilation/graver")
                .header("content-type", "application/json")
                .body(Body::from(json!({"album_ids": [3]}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let corps: Value = serde_json::from_slice(&octets).unwrap();
    assert_eq!(corps["fichiers_ecrits"].as_i64(), Some(0));
    assert_eq!(
        corps["sans_decision"].as_array().map(|a| a.len()),
        Some(1),
        "l'album sans décision doit être NOMMÉ, pas ignoré : {corps}"
    );
}

/// Une gravure sans album est refusée, comme la pose.
#[tokio::test]
async fn une_gravure_sans_album_est_refusee() {
    let (app, _state) = banc();
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/library/albums/compilation/graver")
                .header("content-type", "application/json")
                .body(Body::from(json!({"album_ids": []}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
