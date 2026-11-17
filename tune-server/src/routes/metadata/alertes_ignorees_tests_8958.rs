//! #8958 — « Marquer comme normale » : l'alerte ne revient plus, la liste des
//! ignorées la montre, l'annulation la rend.

use crate::state::AppState;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;

async fn route(state: &AppState, methode: &str, uri: &str, corps: &str) -> (StatusCode, Value) {
    let r = super::router()
        .with_state(state.clone())
        .oneshot(
            Request::builder()
                .method(methode)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(corps.to_string()))
                .expect("requête"),
        )
        .await
        .expect("réponse");
    let code = r.status();
    let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .expect("corps");
    (code, serde_json::from_slice(&octets).unwrap_or(Value::Null))
}

/// Album 1 « Sound Effects » sans artiste : pistes 1 (normale) et 2 (2 s).
/// Piste 3 : artiste connu, très courte, sans album.
fn etat() -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).expect("état");
    let b = &state.backend;
    b.execute(
        "INSERT INTO artists (id, name) VALUES (1, 'Unknown Artist')",
        &[],
    )
    .expect("artiste inconnu");
    b.execute("INSERT INTO artists (id, name) VALUES (2, 'Didier')", &[])
        .expect("artiste");
    b.execute(
        "INSERT INTO albums (id, title) VALUES (1, 'Sound Effects')",
        &[],
    )
    .expect("album");
    for (id, artiste, album, duree, chemin) in [
        (
            1i64,
            1i64,
            Some(1i64),
            180_000i64,
            "/data/recordings/Tidal/Prince/Ultimate/01.flac",
        ),
        (
            2,
            1,
            Some(1),
            2_000,
            "/data/recordings/Tidal/Prince/Ultimate/02.flac",
        ),
        (3, 2, None, 3_000, "/m/divers/bip.flac"),
    ] {
        b.execute(
            "INSERT INTO tracks (id, title, artist_id, album_id, duration_ms, source, file_path) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'local', ?6)",
            &[
                &id as &dyn ToSqlValue,
                &format!("p{id}"),
                &artiste,
                &album,
                &duree,
                &chemin,
            ],
        )
        .expect("piste");
    }
    state
}

async fn douteuses(state: &AppState) -> Value {
    let (code, v) = route(state, "GET", "/doubtful?limit=100", "").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        v["total"].as_i64(),
        Some(v["items"].as_array().map(|a| a.len()).unwrap_or(0) as i64),
        "le total doit valoir la liste : {v}"
    );
    v
}

fn raisons(v: &Value, id: i64) -> Option<Vec<String>> {
    v["items"]
        .as_array()?
        .iter()
        .find(|i| i["id"] == id)
        .map(|i| {
            i["reasons"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r.as_str().unwrap().to_string())
                .collect()
        })
}

#[tokio::test]
async fn la_liste_annonce_la_portee_de_chaque_alerte() {
    let state = etat();
    let v = douteuses(&state).await;
    let p2 = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == 2)
        .unwrap();
    assert_eq!(p2["ignore_scopes"]["missing_artist"], "album", "{p2}");
    assert_eq!(p2["ignore_scopes"]["very_short"], "track", "{p2}");
    let p3 = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == 3)
        .unwrap();
    assert_eq!(p3["ignore_scopes"]["missing_album"], "track", "{p3}");
}

/// Une piste courte voulue : ignorée pour ELLE seule ; elle reste listée tant
/// que son autre alerte (artiste) tient, mais sans la raison ignorée.
#[tokio::test]
async fn piste_tres_courte_ignoree_garde_son_autre_alerte() {
    let state = etat();
    assert_eq!(douteuses(&state).await["total"], 3);
    let (code, r) = route(
        &state,
        "POST",
        "/doubtful/ignored",
        r#"{"track_id":2,"reason":"very_short"}"#,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{r}");
    assert_eq!(r["scope"], "track");
    assert_eq!(r["target_id"], 2);
    let v = douteuses(&state).await;
    assert_eq!(v["total"], 3, "la piste 2 manque toujours d'artiste : {v}");
    assert_eq!(raisons(&v, 2), Some(vec!["missing_artist".to_string()]));

    // Rejouée : rien de plus.
    let (_, r) = route(
        &state,
        "POST",
        "/doubtful/ignored",
        r#"{"track_id":2,"reason":"very_short"}"#,
    )
    .await;
    assert_eq!(r["added"], false);
}

/// L'artiste manquant se mémorise pour l'ALBUM : ses deux pistes sortent.
#[tokio::test]
async fn artiste_manquant_ignore_pour_tout_l_album() {
    let state = etat();
    // L'autre sens d'abord : sans le choix, l'aperçu les prend bien.
    let (_, avant) = route(
        &state,
        "POST",
        "/doubtful/artist-from-folder",
        r#"{"dry_run":true}"#,
    )
    .await;
    assert_eq!(avant["tracks"], 2, "aperçu avant : {avant}");
    let (code, r) = route(
        &state,
        "POST",
        "/doubtful/ignored",
        r#"{"track_id":1,"reason":"missing_artist"}"#,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{r}");
    assert_eq!(r["scope"], "album");
    assert_eq!(r["target_id"], 1);
    let v = douteuses(&state).await;
    assert_eq!(raisons(&v, 1), None, "piste 1 : plus aucune alerte : {v}");
    assert_eq!(
        raisons(&v, 2),
        Some(vec!["very_short".to_string()]),
        "piste 2 : l'album couvre son artiste, pas sa durée"
    );
    // La pastille du tableau de bord compte pareil.
    let compte = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone())
        .count_doubtful()
        .unwrap();
    assert_eq!(compte, 2);

    // Et l'écriture de l'artiste du dossier ne la touche plus.
    let (_, apercu) = route(
        &state,
        "POST",
        "/doubtful/artist-from-folder",
        r#"{"dry_run":true}"#,
    )
    .await;
    assert_eq!(apercu["tracks"], 0, "aperçu : {apercu}");
}

/// La liste des ignorées la montre ; l'annulation la rend.
#[tokio::test]
async fn revoir_puis_annuler() {
    let state = etat();
    route(
        &state,
        "POST",
        "/doubtful/ignored",
        r#"{"track_id":3,"reason":"very_short"}"#,
    )
    .await;
    route(
        &state,
        "POST",
        "/doubtful/ignored",
        r#"{"track_id":3,"reason":"missing_album"}"#,
    )
    .await;
    let v = douteuses(&state).await;
    assert_eq!(
        raisons(&v, 3),
        None,
        "les deux alertes de la piste 3 sont ignorées : {v}"
    );

    let (code, l) = route(&state, "GET", "/doubtful/ignored", "").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(l["total"], 2, "{l}");
    assert_eq!(l["items"][0]["title"], "p3", "{l}");
    assert_eq!(l["items"][0]["file_path"], "/m/divers/bip.flac", "{l}");

    let (code, r) = route(&state, "DELETE", "/doubtful/ignored/track/3/very_short", "").await;
    assert_eq!(code, StatusCode::OK, "{r}");
    assert_eq!(
        raisons(&douteuses(&state).await, 3),
        Some(vec!["very_short".to_string()])
    );
    let (_, l) = route(&state, "GET", "/doubtful/ignored", "").await;
    assert_eq!(l["total"], 1);

    let (code, _) = route(&state, "DELETE", "/doubtful/ignored/track/3/very_short", "").await;
    assert_eq!(code, StatusCode::NOT_FOUND, "déjà annulée");
}

#[tokio::test]
async fn demandes_refusees() {
    let state = etat();
    let (code, _) = route(
        &state,
        "POST",
        "/doubtful/ignored",
        r#"{"track_id":2,"reason":"mauvaise"}"#,
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) = route(
        &state,
        "POST",
        "/doubtful/ignored",
        r#"{"track_id":99,"reason":"very_short"}"#,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    let (code, _) = route(
        &state,
        "DELETE",
        "/doubtful/ignored/disque/1/very_short",
        "",
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    // Rien n'a été retenu.
    assert_eq!(douteuses(&state).await["total"], 3);
}

/// Un choix sur une piste supprimée disparaît de la liste des ignorées.
#[tokio::test]
async fn choix_orphelin_purge() {
    let state = etat();
    route(
        &state,
        "POST",
        "/doubtful/ignored",
        r#"{"track_id":3,"reason":"very_short"}"#,
    )
    .await;
    state
        .backend
        .execute("DELETE FROM tracks WHERE id = 3", &[])
        .unwrap();
    let (_, l) = route(&state, "GET", "/doubtful/ignored", "").await;
    assert_eq!(l["total"], 0, "{l}");
}
