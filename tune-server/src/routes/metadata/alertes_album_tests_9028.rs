//! #9028 — « Normal pour tout l'album » : l'aperçu liste les pistes de
//! l'album, un geste les couvre toutes, l'annulation les rend.

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

const IMAGE: &str = "/m/Coldplay/Everyday Life/Everyday Life.flac";

/// Album 1 (+ `decalage`) « Everyday Life », un fichier + feuille CUE : la
/// piste 1 dure 4 min, les pistes 2, 3 et 4 sont des pistes cachées de 3 s.
/// Album 2 « Autre » ailleurs : une piste de 2 s (jamais couverte).
/// Album 3 « Racine » à la racine (pas de dossier exploitable) : deux
/// pistes de 1 s.
fn poser(state: &AppState, decalage: i64) {
    let b = &state.backend;
    for (id, titre) in [(1, "Everyday Life"), (2, "Autre"), (3, "Racine")] {
        b.execute(
            "INSERT INTO albums (id, title) VALUES (?1, ?2)",
            &[&(id + decalage) as &dyn ToSqlValue, &titre],
        )
        .expect("album");
    }
    for (id, album, duree, debut) in [
        (1i64, 1i64, 240_000i64, 0i64),
        (2, 1, 3_000, 240_000),
        (3, 1, 3_000, 243_000),
        (4, 1, 3_000, 246_000),
    ] {
        b.execute(
            "INSERT INTO tracks (id, title, artist_id, album_id, duration_ms, source, \
             file_path, cue_media_path, cue_start_ms) \
             VALUES (?1, ?2, 1, ?3, ?4, 'local', NULL, ?5, ?6)",
            &[
                &(id + decalage) as &dyn ToSqlValue,
                &format!("p{id}"),
                &(album + decalage),
                &duree,
                &IMAGE,
                &debut,
            ],
        )
        .expect("piste cue");
    }
    for (id, album, duree, chemin) in [
        (5i64, 2i64, 2_000i64, "/m/Autre/Autre/01.flac"),
        (6, 3, 1_000, "/a.flac"),
        (7, 3, 1_000, "/b.flac"),
    ] {
        b.execute(
            "INSERT INTO tracks (id, title, artist_id, album_id, duration_ms, source, file_path) \
             VALUES (?1, ?2, 1, ?3, ?4, 'local', ?5)",
            &[
                &(id + decalage) as &dyn ToSqlValue,
                &format!("p{id}"),
                &(album + decalage),
                &duree,
                &chemin,
            ],
        )
        .expect("piste");
    }
}

fn etat() -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).expect("état");
    state
        .backend
        .execute("INSERT INTO artists (id, name) VALUES (1, 'Coldplay')", &[])
        .expect("artiste");
    poser(&state, 0);
    state
}

/// Les ids des pistes encore signalées « très courte ».
async fn courtes(state: &AppState) -> Vec<i64> {
    let (code, v) = route(state, "GET", "/doubtful?limit=100", "").await;
    assert_eq!(code, StatusCode::OK);
    let total = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone())
        .count_doubtful()
        .unwrap();
    assert_eq!(
        v["total"].as_i64(),
        Some(total),
        "la pastille compte comme la liste"
    );
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| {
            i["reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r == "very_short")
        })
        .map(|i| i["id"].as_i64().unwrap())
        .collect()
}

async fn marquer(state: &AppState, corps: &str) -> (StatusCode, Value) {
    route(state, "POST", "/doubtful/ignored/album", corps).await
}

async fn annuler(state: &AppState, corps: &str) -> (StatusCode, Value) {
    route(state, "DELETE", "/doubtful/ignored/album", corps).await
}

/// L'aperçu nomme l'album et liste ses pistes signalées, rien d'autre.
#[tokio::test]
async fn l_apercu_liste_les_pistes_de_l_album() {
    let state = etat();
    let (code, a) = route(
        &state,
        "GET",
        "/doubtful/album?album_id=1&reason=very_short",
        "",
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{a}");
    assert_eq!(a["album_title"], "Everyday Life");
    assert_eq!(
        a["scope"], "track",
        "le geste groupé n'écrit que des pistes"
    );
    assert_eq!(a["folder"], "/m/Coldplay/Everyday Life");
    assert_eq!(a["total"], 3, "{a}");
    let ids: Vec<i64> = a["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![2, 3, 4]);
    assert_eq!(
        a["tracks"][0]["file_path"], IMAGE,
        "le chemin ouvrable d'une piste CUE"
    );
    assert_eq!(a["ignored_count"], 0);
}

/// Une lecture en échec ne se maquille pas en « rien d'ignoré ».
#[tokio::test]
async fn l_apercu_ne_masque_pas_une_erreur_de_lecture() {
    let state = etat();
    state
        .backend
        .execute("DROP TABLE alertes_douteuses_ignorees", &[])
        .unwrap();
    let (code, a) = route(
        &state,
        "GET",
        "/doubtful/album?album_id=1&reason=very_short",
        "",
    )
    .await;
    assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR, "{a}");
}

/// 🔴 Le geste : UNE LIGNE PAR PISTE montrée ; rejoué, rien ; tient au
/// rescan complet (clé CUE `image#début`).
#[tokio::test]
async fn un_geste_ecrit_une_ligne_par_piste() {
    let state = etat();
    assert_eq!(courtes(&state).await, vec![2, 3, 4, 5, 6, 7]);
    let (code, r) = marquer(
        &state,
        r#"{"album_id":1,"reason":"very_short","track_ids":[2,3,4]}"#,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{r}");
    assert_eq!(r["scope"], "track");
    assert_eq!(r["track_count"], 3);
    assert_eq!(r["added"], 3);
    let cles: Vec<&str> = r["added_keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| {
            assert_eq!(k["scope"], "track", "{k}");
            k["key"].as_str().unwrap()
        })
        .collect();
    assert_eq!(
        cles,
        vec![
            format!("{IMAGE}#240000"),
            format!("{IMAGE}#243000"),
            format!("{IMAGE}#246000")
        ]
    );
    assert_eq!(
        courtes(&state).await,
        vec![5, 6, 7],
        "l'autre album reste signalé"
    );

    let (_, r) = marquer(
        &state,
        r#"{"album_id":1,"reason":"very_short","track_ids":[2,3,4]}"#,
    )
    .await;
    assert_eq!(r["added"], 0, "rejoué : rien de plus : {r}");
    assert_eq!(r["added_keys"], serde_json::json!([]));
    assert_eq!(r["track_count"], 3);
    let (_, l) = route(&state, "GET", "/doubtful/ignored", "").await;
    assert_eq!(l["total"], 3, "{l}");
    assert!(
        l["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["scope"] == "track")
    );

    let (_, a) = route(
        &state,
        "GET",
        "/doubtful/album?album_id=1&reason=very_short",
        "",
    )
    .await;
    assert_eq!(a["total"], 0, "{a}");
    assert_eq!(a["ignored_count"], 3, "{a}");

    state.backend.execute("DELETE FROM tracks", &[]).unwrap();
    state.backend.execute("DELETE FROM albums", &[]).unwrap();
    poser(&state, 100);
    assert_eq!(courtes(&state).await, vec![105, 106, 107]);
}

/// 🔴 Ne couvrir QUE les pistes montrées : une piste ajoutée ensuite au même
/// album, ou un autre album de même titre dans le même dossier (image CUE
/// sans TITLE, compilation à plat), restent signalées.
#[tokio::test]
async fn rien_au_dela_des_pistes_montrees() {
    let state = etat();
    marquer(
        &state,
        r#"{"album_id":1,"reason":"very_short","track_ids":[2,3,4]}"#,
    )
    .await;
    let b = &state.backend;
    b.execute(
        "INSERT INTO tracks (id, title, artist_id, album_id, duration_ms, source, \
         file_path, cue_media_path, cue_start_ms) \
         VALUES (8, 'ajoutée', 1, 1, 3000, 'local', NULL, ?1, 249000)",
        &[&IMAGE as &dyn ToSqlValue],
    )
    .unwrap();
    b.execute(
        "INSERT INTO albums (id, title) VALUES (4, 'Everyday Life')",
        &[],
    )
    .unwrap();
    b.execute(
        "INSERT INTO tracks (id, title, artist_id, album_id, duration_ms, source, file_path) \
         VALUES (9, 'homonyme', 1, 4, 2000, 'local', '/m/Coldplay/Everyday Life/bonus.flac')",
        &[],
    )
    .unwrap();
    assert_eq!(courtes(&state).await, vec![5, 6, 7, 8, 9]);
}

/// Une partie seulement des pistes : ces pistes-là.
#[tokio::test]
async fn une_partie_de_l_album() {
    let state = etat();
    let (code, r) = marquer(
        &state,
        r#"{"album_id":1,"reason":"very_short","track_ids":[2,3]}"#,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{r}");
    assert_eq!(r["added"], 2);
    assert_eq!(r["track_count"], 2);
    assert_eq!(courtes(&state).await, vec![4, 5, 6, 7]);
}

/// Album à la racine : même règle, pas de dossier à afficher.
#[tokio::test]
async fn album_a_la_racine() {
    let state = etat();
    let (_, a) = route(
        &state,
        "GET",
        "/doubtful/album?album_id=3&reason=very_short",
        "",
    )
    .await;
    assert_eq!(a["folder"], Value::Null, "{a}");
    let (code, r) = marquer(
        &state,
        r#"{"album_id":3,"reason":"very_short","track_ids":[6,7]}"#,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{r}");
    assert_eq!(r["added"], 2);
    assert_eq!(courtes(&state).await, vec![2, 3, 4, 5]);
}

/// Le serveur vérifie la liste : une piste d'un autre album, ou qui ne porte
/// pas l'alerte, fait tout refuser ; rien n'est écrit.
#[tokio::test]
async fn la_liste_de_pistes_est_verifiee() {
    let state = etat();
    for corps in [
        r#"{"album_id":1,"reason":"very_short","track_ids":[2,3,5]}"#,
        r#"{"album_id":1,"reason":"very_short","track_ids":[1,2]}"#,
        r#"{"album_id":1,"reason":"very_short","track_ids":[]}"#,
        r#"{"album_id":1,"reason":"missing_album","track_ids":[2]}"#,
        r#"{"album_id":1,"reason":"missing_artist","track_ids":[2]}"#,
        r#"{"album_id":1,"reason":"mauvaise","track_ids":[2]}"#,
    ] {
        let (code, r) = marquer(&state, corps).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{corps} : {r}");
    }
    let (_, r) = marquer(
        &state,
        r#"{"album_id":1,"reason":"very_short","track_ids":[2,3,5]}"#,
    )
    .await;
    assert_eq!(r["invalid_track_ids"], serde_json::json!([5]), "{r}");
    let (code, _) = marquer(&state, r#"{"album_id":1,"reason":"very_short"}"#).await;
    assert!(code.is_client_error(), "sans track_ids : {code}");
    let (code, _) = marquer(
        &state,
        r#"{"album_id":99,"reason":"very_short","track_ids":[2]}"#,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    let (_, l) = route(&state, "GET", "/doubtful/ignored", "").await;
    assert_eq!(l["total"], 0, "rien n'a été retenu : {l}");
    assert_eq!(courtes(&state).await, vec![2, 3, 4, 5, 6, 7]);
}

/// 🔴 L'annulation retire EXACTEMENT les lignes ajoutées par le geste : la
/// piste 2, marquée seule avant, reste normale — qu'elle ait été hors du
/// geste ou dedans (`added_keys` ne la contient alors pas).
#[tokio::test]
async fn annuler_ne_retire_que_les_lignes_du_geste() {
    for ids in ["[3,4]", "[2,3,4]"] {
        let state = etat();
        let (code, r) = route(
            &state,
            "POST",
            "/doubtful/ignored",
            r#"{"track_id":2,"reason":"very_short"}"#,
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{r}");
        let (_, r) = marquer(
            &state,
            &format!(r#"{{"album_id":1,"reason":"very_short","track_ids":{ids}}}"#),
        )
        .await;
        assert_eq!(r["added"], 2, "{ids} : {r}");
        assert_eq!(courtes(&state).await, vec![5, 6, 7]);

        let corps = serde_json::json!({"keys": r["added_keys"]}).to_string();
        let (code, a) = annuler(&state, &corps).await;
        assert_eq!(code, StatusCode::OK, "{a}");
        assert_eq!(a["removed"], 2, "{ids} : {a}");
        assert_eq!(
            courtes(&state).await,
            vec![3, 4, 5, 6, 7],
            "{ids} : la piste 2 reste normale"
        );
        let (_, a) = annuler(&state, &corps).await;
        assert_eq!(a["removed"], 0, "rejouée : {a}");
    }
}

/// Authentification active, sans jeton : le geste et l'annulation sont
/// refusés.
#[tokio::test]
async fn ecritures_reservees_a_l_administrateur() {
    let state = etat();
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set("auth_enabled", "true")
        .unwrap();
    for (m, corps) in [
        (
            "POST",
            r#"{"album_id":1,"reason":"very_short","track_ids":[2]}"#,
        ),
        ("DELETE", r#"{"keys":[]}"#),
    ] {
        let (code, _) = route(&state, m, "/doubtful/ignored/album", corps).await;
        assert!(
            code == StatusCode::UNAUTHORIZED || code == StatusCode::FORBIDDEN,
            "{m} : {code}"
        );
    }
    let (_, l) = route(&state, "GET", "/doubtful/ignored", "").await;
    assert_eq!(l["total"], 0);
}
