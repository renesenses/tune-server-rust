//! BIB-C1 (phase 1) : l'absorption d'un artiste homographe par la route.
//!
//! Les cas sont ceux du relevé du .18 : « Etienne Daho » / « Étienne Daho »
//! (une graphie accentuée, un MBID d'un seul côté), et les refus — clés
//! différentes, deux MBID distincts, l'artiste inconnu.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;

type Etat = crate::state::AppState;

fn serveur() -> (axum::Router, Etat) {
    let state = Etat::new(":memory:", 0, Default::default()).unwrap();
    let routeur = crate::routes::router(state.clone());
    (routeur, state)
}

async fn appel(app: &axum::Router, methode: &str, chemin: &str) -> (StatusCode, Value) {
    let requete = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("Content-Type", "application/json")
        .body(Body::empty())
        .unwrap();
    let reponse = app.clone().oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn inserer(state: &Etat, sql: &str, params: &[&dyn ToSqlValue]) -> i64 {
    state.backend.execute(sql, params).unwrap();
    state.backend.last_insert_rowid()
}

fn artiste(state: &Etat, nom: &str, mbid: Option<&str>) -> i64 {
    let mbid = mbid
        .map(|m| format!("'{m}'"))
        .unwrap_or_else(|| "NULL".to_string());
    state
        .backend
        .execute_batch(&format!(
            "INSERT INTO artists (name, musicbrainz_id) VALUES ('{nom}', {mbid});"
        ))
        .unwrap();
    state.backend.last_insert_rowid()
}

fn compte(state: &Etat, sql: &str, id: i64) -> i64 {
    state
        .backend
        .query_one(sql, &[&id as &dyn ToSqlValue])
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
}

/// Deux fiches, un artiste : un album et une piste chacune, un favori et
/// une étiquette sur la graphie accentuée, le MBID aussi.
fn daho_en_double(state: &Etat) -> (i64, i64) {
    let cible = artiste(state, "Etienne Daho", None);
    let doublon = artiste(state, "Étienne Daho", Some("mbid-daho"));
    let a1 = inserer(
        state,
        "INSERT INTO albums (title, artist_id, source) VALUES ('Pop Satori', ?, 'local')",
        &[&cible as &dyn ToSqlValue],
    );
    let a2 = inserer(
        state,
        "INSERT INTO albums (title, artist_id, source) VALUES ('Eden', ?, 'local')",
        &[&doublon as &dyn ToSqlValue],
    );
    inserer(
        state,
        "INSERT INTO tracks (title, album_id, artist_id, album_artist, file_path) VALUES ('Duel au soleil', ?, ?, 'Etienne Daho', '/m/pop/01.flac')",
        &[&a1 as &dyn ToSqlValue, &cible],
    );
    inserer(
        state,
        "INSERT INTO tracks (title, album_id, artist_id, album_artist, file_path) VALUES ('Des attractions désastre', ?, ?, 'Étienne Daho', '/m/eden/01.flac')",
        &[&a2 as &dyn ToSqlValue, &doublon],
    );
    inserer(
        state,
        "INSERT INTO favorites (profile_id, item_type, item_id) VALUES (1, 'artist', ?)",
        &[&doublon as &dyn ToSqlValue],
    );
    let tag = inserer(state, "INSERT INTO tags (name) VALUES ('chanson')", &[]);
    inserer(
        state,
        "INSERT INTO item_tags (tag_id, item_type, item_id) VALUES (?, 'artist', ?)",
        &[&tag as &dyn ToSqlValue, &doublon],
    );
    (cible, doublon)
}

#[tokio::test]
async fn la_graphie_accentuee_est_absorbee_avec_ses_marqueurs_et_ne_renait_pas() {
    let (app, state) = serveur();
    let (cible, doublon) = daho_en_double(&state);

    let (_, avant) = appel(&app, "GET", "/api/v1/library/artists/doublons").await;
    assert_eq!(avant["count"].as_u64(), Some(1), "avant : {avant}");

    let (statut, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/artists/{cible}/absorber/{doublon}"),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "corps : {corps}");
    assert_eq!(corps["albums"].as_u64(), Some(1));
    assert_eq!(corps["pistes"].as_u64(), Some(1));
    assert_eq!(
        corps["textes_recales"].as_u64(),
        Some(1),
        "album_artist recalé : {corps}"
    );

    let (_, apres) = appel(&app, "GET", "/api/v1/library/artists/doublons").await;
    assert_eq!(apres["count"].as_u64(), Some(0), "après : {apres}");
    assert_eq!(
        compte(&state, "SELECT COUNT(*) FROM artists WHERE id = ?", doublon),
        0
    );
    assert_eq!(
        compte(
            &state,
            "SELECT COUNT(*) FROM albums WHERE artist_id = ?",
            cible
        ),
        2
    );
    assert_eq!(
        compte(
            &state,
            "SELECT COUNT(*) FROM tracks WHERE artist_id = ? AND album_artist = 'Etienne Daho'",
            cible
        ),
        2
    );
    assert_eq!(
        compte(
            &state,
            "SELECT COUNT(*) FROM favorites WHERE item_type = 'artist' AND item_id = ?",
            cible
        ),
        1,
        "le favori suit"
    );
    assert_eq!(
        compte(
            &state,
            "SELECT COUNT(*) FROM item_tags WHERE item_type = 'artist' AND item_id = ?",
            cible
        ),
        1,
        "l'étiquette suit"
    );
    let mbid = state
        .backend
        .query_one(
            "SELECT musicbrainz_id FROM artists WHERE id = ?",
            &[&cible as &dyn ToSqlValue],
        )
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_string()));
    assert_eq!(
        mbid.as_deref(),
        Some("mbid-daho"),
        "le MBID du doublon est repris"
    );

    // Pas de renaissance : la graphie perdante retombe sur le survivant.
    let repo = tune_core::db::artist_repo::ArtistRepo::with_backend(state.backend.clone());
    let retrouve = repo.get_or_create("Étienne Daho", None, None).unwrap();
    assert_eq!(retrouve.id, Some(cible));
    assert_eq!(
        compte(&state, "SELECT COUNT(*) FROM artists WHERE 1 = ?", 1),
        1
    );

    let (statut, _) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/artists/{cible}/absorber/{doublon}"),
    )
    .await;
    assert_eq!(
        statut,
        StatusCode::NOT_FOUND,
        "second appel : la fiche n'existe plus"
    );
}

#[tokio::test]
async fn cles_differentes_mbid_distincts_et_artiste_inconnu_sont_refuses() {
    let (app, state) = serveur();
    let ayo = artiste(&state, "Ayo", Some("mbid-ayo"));
    let ayo_accent = artiste(&state, "Ayọ", Some("mbid-autre"));
    let bilou = artiste(&state, "Bilou", None);
    let inconnu = artiste(
        &state,
        tune_core::db::artist_repo::UNKNOWN_ARTIST_NAME,
        None,
    );

    let (statut, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/artists/{ayo}/absorber/{bilou}"),
    )
    .await;
    assert_eq!(
        (statut, corps["error"].as_str()),
        (StatusCode::CONFLICT, Some("cles_differentes")),
        "{corps}"
    );

    let (statut, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/artists/{ayo}/absorber/{ayo_accent}"),
    )
    .await;
    assert_eq!(
        (statut, corps["error"].as_str()),
        (StatusCode::CONFLICT, Some("mbid_distincts")),
        "{corps}"
    );

    let (statut, corps) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/artists/{bilou}/absorber/{inconnu}"),
    )
    .await;
    assert_eq!(
        (statut, corps["error"].as_str()),
        (StatusCode::CONFLICT, Some("artiste_inconnu_non_absorbable")),
        "{corps}"
    );

    let (statut, _) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/artists/{ayo}/absorber/{ayo}"),
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
    let (statut, _) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/artists/{ayo}/absorber/424242"),
    )
    .await;
    assert_eq!(statut, StatusCode::NOT_FOUND);
    assert_eq!(
        compte(&state, "SELECT COUNT(*) FROM artists WHERE 1 = ?", 1),
        4,
        "rien n'a bougé"
    );
}
