//! #6300 (Lulu, fil 2205) — retirer un album en double en entier.
//!
//! Le geste masque l'album retiré (#1391) : fichiers intacts, lignes
//! intactes, réversible. Sans `confirm: true`, rien n'est écrit.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::hidden_repo::HiddenRepo;
use tune_core::test_scratch::{ScratchDir, scratch_dir};

type Etat = crate::state::AppState;

/// Album 10 (gardé) : « Blue » et « River ». Album 20 (à retirer) : les deux
/// mêmes titres, plus « Bonus » qui n'a pas d'équivalent. Album 30 : un
/// album de SERVICE. Chaque piste locale a un vrai fichier sur disque.
fn banc() -> (axum::Router, Etat, ScratchDir) {
    let state = Etat::new(":memory:", 0, Default::default()).unwrap();
    let dossier = scratch_dir("retrait-album-6300");
    let b = &state.backend;
    b.execute(
        "INSERT INTO artists (id, name) VALUES (1, 'Joni Mitchell')",
        &[],
    )
    .unwrap();
    for (id, titre, source) in [
        (10i64, "Blue", "local"),
        (20, "Blue (copie)", "local"),
        (30, "Blue", "qobuz"),
    ] {
        b.execute(
            "INSERT INTO albums (id, title, artist_id, source) VALUES (?, ?, 1, ?)",
            &[&id as &dyn ToSqlValue, &titre, &source],
        )
        .unwrap();
    }
    for (id, album, titre, duree) in [
        (1i64, 10i64, "Blue", 180_000i64),
        (2, 10, "River", 240_000),
        (3, 20, "Blue", 180_000),
        (4, 20, "River", 240_000),
        (5, 20, "Bonus", 100_000),
    ] {
        let chemin = dossier.join(format!("{id}.flac"));
        std::fs::write(&chemin, b"fLaC").unwrap();
        let chemin = chemin.to_string_lossy().to_string();
        b.execute(
            "INSERT INTO tracks (id, title, artist_id, album_id, file_path, duration_ms, format) \
             VALUES (?, ?, 1, ?, ?, ?, 'flac')",
            &[&id as &dyn ToSqlValue, &titre, &album, &chemin, &duree],
        )
        .unwrap();
    }
    let routeur = crate::routes::router(state.clone());
    (routeur, state, dossier)
}

async fn appel(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let reponse = app.clone().oneshot(req).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

async fn retirer(app: &axum::Router, corps: Value) -> (StatusCode, Value) {
    appel(
        app,
        Request::post("/api/v1/library/duplicates/resolve-album")
            .header("content-type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await
}

async fn paires(app: &axum::Router) -> Vec<Value> {
    let (statut, corps) = appel(
        app,
        Request::get("/api/v1/library/duplicates?critere=etiquettes_identiques")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    corps["paires"].as_array().cloned().unwrap_or_default()
}

fn masque(state: &Etat, album: i64) -> bool {
    HiddenRepo::with_backend(state.backend.clone())
        .is_album_hidden(album)
        .unwrap()
}

/// 🔴 Sans confirmation, RIEN n'est écrit : la route rend le plan.
#[tokio::test]
async fn sans_confirmation_rien_n_est_retire_et_le_plan_est_rendu_6300() {
    let (app, state, _d) = banc();
    let (statut, corps) =
        retirer(&app, json!({ "keep_album_id": 10, "remove_album_id": 20 })).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert!(
        !masque(&state, 20),
        "sans `confirm: true`, l'album 20 ne doit PAS être retiré — {corps}"
    );
    assert_eq!(
        corps["applique"], false,
        "sans `confirm: true`, la route ne doit rien appliquer — {corps}"
    );
    assert_eq!(corps["confirmation_requise"], true, "{corps}");
    let plan = &corps["plan"];
    assert_eq!(plan["retire"]["pistes"], 3, "{corps}");
    assert_eq!(
        plan["retire"]["pistes_sans_equivalent"], 1,
        "« Bonus » n'a pas d'équivalent dans l'album gardé — {corps}"
    );
    assert_eq!(plan["garde"]["pistes"], 2, "{corps}");
    assert_eq!(plan["fichiers_supprimes"], 0, "{corps}");
}

/// 🔴 Confirmé : l'album retiré est masqué, l'album gardé ne l'est pas, les
/// pistes restent en base et les FICHIERS restent sur le disque.
#[tokio::test]
async fn confirme_l_album_est_retire_sans_toucher_aux_fichiers_6300() {
    let (app, state, dossier) = banc();
    let (statut, corps) = retirer(
        &app,
        json!({ "keep_album_id": 10, "remove_album_id": 20, "confirm": true }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["applique"], true, "{corps}");
    assert!(masque(&state, 20), "l'album 20 doit être retiré — {corps}");
    assert!(!masque(&state, 10), "l'album gardé ne doit pas l'être");
    let restantes = state
        .backend
        .query_one("SELECT COUNT(*) FROM tracks", &[])
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap();
    assert_eq!(
        restantes, 5,
        "aucune piste ne doit être supprimée de la base"
    );
    for id in 1..=5 {
        assert!(
            dossier.join(format!("{id}.flac")).exists(),
            "le fichier {id}.flac doit rester sur le disque"
        );
    }
}

/// 🔴 Les paires portent leur album, et celles de l'album retiré ne sont plus
/// proposées ; le démasquer les ramène (geste réversible).
#[tokio::test]
async fn les_paires_de_l_album_retire_disparaissent_puis_reviennent_6300() {
    let (app, state, _d) = banc();
    let avant = paires(&app).await;
    assert_eq!(avant.len(), 2, "Blue et River en double — {avant:?}");
    for p in &avant {
        let albums = [p["a"]["album_id"].as_i64(), p["b"]["album_id"].as_i64()];
        assert_eq!(
            albums,
            [Some(10), Some(20)],
            "chaque copie doit porter son album — {p}"
        );
        assert_eq!(p["b"]["album_title"], "Blue (copie)", "{p}");
    }

    let (statut, corps) = retirer(
        &app,
        json!({ "keep_album_id": 10, "remove_album_id": 20, "confirm": true }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let apres = paires(&app).await;
    assert!(
        apres.is_empty(),
        "les paires de l'album retiré ne doivent plus être proposées — {apres:?}"
    );

    HiddenRepo::with_backend(state.backend.clone())
        .unhide_album(20)
        .unwrap();
    assert_eq!(
        paires(&app).await.len(),
        2,
        "remettre l'album doit ramener ses paires"
    );
}

/// Les refus : même album, album inconnu, album de service.
#[tokio::test]
async fn les_refus_du_retrait_6300() {
    let (app, state, _d) = banc();
    let (s, c) = retirer(
        &app,
        json!({ "keep_album_id": 10, "remove_album_id": 10, "confirm": true }),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{c}");
    let (s, c) = retirer(
        &app,
        json!({ "keep_album_id": 10, "remove_album_id": 99, "confirm": true }),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{c}");
    let (s, c) = retirer(
        &app,
        json!({ "keep_album_id": 10, "remove_album_id": 30, "confirm": true }),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{c}");
    assert_eq!(c["error"], "source_non_locale", "{c}");
    assert!(!masque(&state, 30) && !masque(&state, 10));
}
