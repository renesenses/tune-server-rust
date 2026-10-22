//! #6687 — retirer une piste d'une playlist locale retire LA ligne désignée.
//!
//! Bertrand, 21/10 (rc6) : « la suppression de pistes n'est pas fiable ». Le
//! client envoie le RANG affiché à `POST /playlists/{id}/tracks/remove`, le
//! serveur effaçait `WHERE position = ?` (la colonne), et rien ne recompactait
//! les positions : après un premier retrait, les suivants n'effaçaient rien
//! (en répondant `204`) ou retiraient la piste voisine.
//!
//! Ce banc garde, route par route :
//!
//! - `GET /tracks` rend `entry_id` sur CHAQUE ligne (locale, service,
//!   introuvable) ;
//! - `POST /tracks/remove {position}` lit un RANG, `{entry_id}` une ligne ;
//! - `DELETE /tracks {positions}` lit des rangs ;
//! - un retrait qui n'efface rien répond `404`, jamais `204`.
//!
//! Les assertions relisent la playlist par `GET /tracks` : le code de retour
//! seul ne prouve rien (le défaut répondait `204` pour rien).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

const P1: &str = "1";
const P2: &str = "2";

fn etat() -> tune_server::state::AppState {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let profils = tune_core::db::profile_repo::ProfileRepo::with_backend(state.backend.clone());
    let id = profils
        .create("voisin", Some("Le voisin"), None)
        .expect("create profile");
    assert_eq!(id, 2, "le second profil doit porter l'id 2");
    state
}

fn piste(state: &tune_server::state::AppState, titre: &str) -> i64 {
    let repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    let mut t = tune_core::db::models::Track::new(titre.into());
    t.file_path = Some(format!("/6687/{titre}.flac"));
    repo.create(&t).expect("insert track")
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    path: &str,
    profil: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(methode)
        .uri(path)
        .header("X-Profile-Id", profil);
    let body = match corps {
        Some(v) => {
            req = req.header("Content-Type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes).to_string()));
    (status, json)
}

/// Une playlist du profil 1 portant les pistes `titres`, dans l'ordre.
async fn playlist(
    app: &axum::Router,
    state: &tune_server::state::AppState,
    titres: &[&str],
) -> i64 {
    let (st, v) = appel(
        app,
        "POST",
        "/api/v1/playlists",
        P1,
        Some(json!({"name": "6687"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let id = v["id"].as_i64().unwrap();
    let ids: Vec<i64> = titres.iter().map(|t| piste(state, t)).collect();
    let (st, v) = appel(
        app,
        "POST",
        &format!("/api/v1/playlists/{id}/tracks"),
        P1,
        Some(json!({"track_ids": ids})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    id
}

async fn lignes(app: &axum::Router, id: i64) -> Vec<Value> {
    let (st, v) = appel(
        app,
        "GET",
        &format!("/api/v1/playlists/{id}/tracks"),
        P1,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v.as_array().cloned().unwrap()
}

async fn titres(app: &axum::Router, id: i64) -> Vec<String> {
    lignes(app, id)
        .await
        .iter()
        .map(|l| l["title"].as_str().unwrap_or("?").to_string())
        .collect()
}

/// Les positions STOCKÉES, dans l'ordre d'affichage : la route ne les publie
/// pas, on les lit en base.
fn positions(state: &tune_server::state::AppState, id: i64) -> Vec<i64> {
    tune_core::db::playlist_repo::PlaylistRepo::with_backend(state.backend.clone())
        .get_entries(id)
        .unwrap()
        .iter()
        .map(|e| e.position)
        .collect()
}

fn contigues(state: &tune_server::state::AppState, id: i64) {
    let p = positions(state, id);
    assert_eq!(
        p,
        (0..p.len() as i64).collect::<Vec<_>>(),
        "#6687 — positions non contiguës après un retrait : {p:?}"
    );
}

async fn retire(app: &axum::Router, id: i64, profil: &str, corps: Value) -> StatusCode {
    appel(
        app,
        "POST",
        &format!("/api/v1/playlists/{id}/tracks/remove"),
        profil,
        Some(corps),
    )
    .await
    .0
}

#[tokio::test]
async fn retirer_deux_fois_le_rang_1_laisse_a_d_6687() {
    let state = etat();
    let app = tune_server::routes::router(state.clone());
    let id = playlist(&app, &state, &["A", "B", "C", "D"]).await;

    assert_eq!(
        retire(&app, id, P1, json!({"position": 1})).await,
        StatusCode::NO_CONTENT
    );
    contigues(&state, id);
    assert_eq!(
        retire(&app, id, P1, json!({"position": 1})).await,
        StatusCode::NO_CONTENT
    );
    contigues(&state, id);
    assert_eq!(
        titres(&app, id).await,
        ["A", "D"],
        "#6687 — deux retraits au rang 1 sur A B C D doivent laisser A D"
    );
}

#[tokio::test]
async fn retirer_rang_1_puis_rang_2_laisse_a_c_e_6687() {
    let state = etat();
    let app = tune_server::routes::router(state.clone());
    let id = playlist(&app, &state, &["A", "B", "C", "D", "E"]).await;

    assert_eq!(
        retire(&app, id, P1, json!({"position": 1})).await,
        StatusCode::NO_CONTENT
    );
    contigues(&state, id);
    assert_eq!(
        retire(&app, id, P1, json!({"position": 2})).await,
        StatusCode::NO_CONTENT
    );
    contigues(&state, id);
    assert_eq!(
        titres(&app, id).await,
        ["A", "C", "E"],
        "#6687 — rang 1 puis rang 2 sur A B C D E doit laisser A C E (et pas A D E)"
    );
}

#[tokio::test]
async fn le_retrait_en_lot_lit_des_rangs_6687() {
    let state = etat();
    let app = tune_server::routes::router(state.clone());
    let id = playlist(&app, &state, &["A", "B", "C", "D", "E"]).await;

    assert_eq!(
        retire(&app, id, P1, json!({"position": 1})).await,
        StatusCode::NO_CONTENT
    );
    let (st, v) = appel(
        &app,
        "DELETE",
        &format!("/api/v1/playlists/{id}/tracks"),
        P1,
        Some(json!({"positions": [1, 2]})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["removed"], 2);
    contigues(&state, id);
    assert_eq!(titres(&app, id).await, ["A", "E"]);

    // Un rang hors bornes dans le lot : rien n'est effacé, 404.
    let (st, _) = appel(
        &app,
        "DELETE",
        &format!("/api/v1/playlists/{id}/tracks"),
        P1,
        Some(json!({"positions": [0, 9]})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(
        titres(&app, id).await,
        ["A", "E"],
        "le lot refusé a effacé quand même"
    );

    // Une liste vide ne désigne rien : `{"removed": 0}`, comme avant #6687.
    let (st, v) = appel(
        &app,
        "DELETE",
        &format!("/api/v1/playlists/{id}/tracks"),
        P1,
        Some(json!({"positions": []})),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "une liste de rangs vide n'est pas un retrait manqué : {v}"
    );
    assert_eq!(v["removed"], 0);
    assert_eq!(titres(&app, id).await, ["A", "E"]);
}

/// Chaque ligne — locale, de service, introuvable — porte `entry_id`, et la
/// ligne de service se retire par lui.
#[tokio::test]
async fn playlist_mixte_entry_id_sur_chaque_ligne_et_retrait_d_une_ligne_de_service_6687() {
    let state = etat();
    let app = tune_server::routes::router(state.clone());
    let id = playlist(&app, &state, &["A", "B"]).await;
    let (st, v) = appel(
        &app,
        "POST",
        &format!("/api/v1/playlists/{id}/tracks"),
        P1,
        Some(json!({
            "track_ids": [],
            "streaming_tracks": [{"source": "qobuz", "source_id": "q-6687", "title": "Qobuz"}],
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    // B devient INTROUVABLE : sa piste quitte la bibliothèque.
    tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone())
        .delete_by_path("/6687/B.flac")
        .unwrap();

    let avant = lignes(&app, id).await;
    assert_eq!(avant.len(), 3, "{avant:?}");
    assert_eq!(
        avant[1]["missing"], true,
        "B doit être introuvable : {avant:?}"
    );
    assert_eq!(avant[2]["source"], "qobuz");
    for l in &avant {
        assert!(
            l["entry_id"].as_i64().is_some(),
            "ligne sans entry_id : {l}"
        );
    }

    let service = avant[2]["entry_id"].as_i64().unwrap();
    assert_eq!(
        retire(&app, id, P1, json!({"entry_id": service})).await,
        StatusCode::NO_CONTENT
    );
    contigues(&state, id);
    let apres = lignes(&app, id).await;
    assert_eq!(apres.len(), 2);
    assert_eq!(apres[0]["title"], "A");
    assert_eq!(apres[1]["missing"], true);
    assert_eq!(
        apres[0]["entry_id"], avant[0]["entry_id"],
        "les autres lignes gardent leur id"
    );

    // La même ligne une seconde fois : elle n'existe plus, 404.
    assert_eq!(
        retire(&app, id, P1, json!({"entry_id": service})).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn un_retrait_qui_n_efface_rien_repond_404_6687() {
    let state = etat();
    let app = tune_server::routes::router(state.clone());
    let id = playlist(&app, &state, &["A", "B"]).await;
    let autre = playlist(&app, &state, &["X"]).await;
    let etrangere = lignes(&app, autre).await[0]["entry_id"].as_i64().unwrap();

    // Rang hors bornes.
    assert_eq!(
        retire(&app, id, P1, json!({"position": 2})).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        retire(&app, id, P1, json!({"position": -1})).await,
        StatusCode::NOT_FOUND
    );
    // entry_id d'une autre playlist : refusé, et rien ne part de l'autre.
    assert_eq!(
        retire(&app, id, P1, json!({"entry_id": etrangere})).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(titres(&app, autre).await, ["X"]);
    // entry_id inconnu.
    assert_eq!(
        retire(&app, id, P1, json!({"entry_id": 987_654})).await,
        StatusCode::NOT_FOUND
    );
    // Playlist d'un autre profil : 404, et la ligne reste.
    let mienne = lignes(&app, id).await[0]["entry_id"].as_i64().unwrap();
    assert_eq!(
        retire(&app, id, P2, json!({"entry_id": mienne})).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        retire(&app, id, P2, json!({"position": 0})).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        titres(&app, id).await,
        ["A", "B"],
        "un refus a effacé quand même"
    );

    // Témoin : le propriétaire, lui, retire par entry_id.
    assert_eq!(
        retire(&app, id, P1, json!({"entry_id": mienne})).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(titres(&app, id).await, ["B"]);
}
