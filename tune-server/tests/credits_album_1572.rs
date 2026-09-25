//! #1572 — `GET /library/albums/{id}/credits` : les crédits de toutes les
//! pistes d'un album, en un aller-retour (demande de FabienM, fil forum 1921,
//! sur le modèle de la fiche « Crédits » de Roon).
//!
//! Épreuves contre le VRAI routeur et une base SQLite en mémoire :
//!
//! - les crédits de l'album viennent de SES pistes, et d'elles seules (un
//!   crédit d'une piste d'un autre album n'y figure pas) ;
//! - chaque ligne porte la piste concernée (`track_id`, `track_title`,
//!   `track_number`, `disc_number`) ;
//! - l'ordre est disque, piste, position du crédit ;
//! - un album sans crédit rend un tableau VIDE (200), pas une erreur ;
//! - la route des crédits d'une piste reste inchangée.
//!
//! Cible `[[test]]` propre (`autotests = false`).
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_server::state::AppState;

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn exec(state: &AppState, sql: &str) {
    state
        .backend
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql} : {e}"));
}

/// Album 1 « The Meaning of Flowers » (capture Roon du fil 1921), deux
/// disques ; album 2 sans aucun crédit ; album 3 porte un crédit qui ne doit
/// pas fuir vers l'album 1.
fn bibliotheque() -> axum::Router {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    exec(
        &state,
        "INSERT INTO artists (id, name) VALUES (1, 'Agnes Obel')",
    );
    exec(
        &state,
        "INSERT INTO artists (id, name) VALUES (2, 'Charlotte Danhier')",
    );
    for (id, titre) in [
        (1, "The Meaning of Flowers"),
        (2, "Sans credits"),
        (3, "Autre"),
    ] {
        exec(
            &state,
            &format!(
                "INSERT INTO albums (id, title, artist_id, source) VALUES ({id}, '{titre}', 1, 'local')"
            ),
        );
    }
    // (id, titre, album, disque, numéro)
    let pistes: [(i64, &str, i64, i64, i64); 5] = [
        (10, "Face B", 1, 2, 1),
        (11, "Ouverture", 1, 1, 2),
        (12, "Premier", 1, 1, 1),
        (20, "Muette", 2, 1, 1),
        (30, "Ailleurs", 3, 1, 1),
    ];
    for (id, titre, album, disque, numero) in pistes {
        exec(
            &state,
            &format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, file_path, duration_ms, format) \
                 VALUES ({id}, '{titre}', {album}, 1, {disque}, {numero}, '/m/{id}.flac', 200000, 'flac')"
            ),
        );
    }
    // (piste, artiste id, nom, rôle, instrument, position)
    let credits: [(i64, &str, &str, &str, &str, i64); 6] = [
        (10, "1", "Agnes Obel", "producer", "NULL", 0),
        (11, "2", "Charlotte Danhier", "performer", "'cello'", 1),
        (11, "1", "Agnes Obel", "composer", "NULL", 0),
        (12, "1", "Agnes Obel", "composer", "NULL", 0),
        (12, "NULL", "John Corban", "performer", "'violin'", 1),
        (30, "1", "Agnes Obel", "mixer", "NULL", 0),
    ];
    for (piste, artiste, nom, role, instrument, position) in credits {
        exec(
            &state,
            &format!(
                "INSERT INTO track_credits (track_id, artist_id, artist_name, role, instrument, position) \
                 VALUES ({piste}, {artiste}, '{nom}', '{role}', {instrument}, {position})"
            ),
        );
    }
    tune_server::routes::router(state)
}

fn tableau(body: &Value) -> &Vec<Value> {
    body.as_array()
        .unwrap_or_else(|| panic!("la réponse n'est pas un tableau : {body}"))
}

#[tokio::test]
async fn les_credits_d_un_album_viennent_de_ses_pistes_1572() {
    let app = bibliotheque();
    let (status, body) = get(&app, "/api/v1/library/albums/1/credits").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let lignes = tableau(&body);
    let vus: Vec<(i64, String, String)> = lignes
        .iter()
        .map(|l| {
            (
                l["track_id"].as_i64().unwrap(),
                l["role"].as_str().unwrap().to_string(),
                l["artist_name"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    // Disque 1 piste 1 (12), disque 1 piste 2 (11), disque 2 piste 1 (10) ;
    // à l'intérieur d'une piste, la position.
    assert_eq!(
        vus,
        vec![
            (12, "composer".into(), "Agnes Obel".into()),
            (12, "performer".into(), "John Corban".into()),
            (11, "composer".into(), "Agnes Obel".into()),
            (11, "performer".into(), "Charlotte Danhier".into()),
            (10, "producer".into(), "Agnes Obel".into()),
        ],
        "ordre ou périmètre faux : {body}"
    );
    // Le mixage de l'album 3 ne fuit pas.
    assert!(
        !lignes.iter().any(|l| l["role"] == "mixer"),
        "un crédit d'un autre album a fui : {body}"
    );
}

#[tokio::test]
async fn chaque_ligne_porte_sa_piste_et_son_artiste_1572() {
    let app = bibliotheque();
    let (_, body) = get(&app, "/api/v1/library/albums/1/credits").await;
    let lignes = tableau(&body);
    let cello = lignes
        .iter()
        .find(|l| l["instrument"] == "cello")
        .unwrap_or_else(|| panic!("violoncelle absent : {body}"));
    assert_eq!(cello["track_title"], "Ouverture");
    assert_eq!(cello["track_number"], 2);
    assert_eq!(cello["disc_number"], 1);
    assert_eq!(cello["artist_id"], 2);
    let violon = lignes
        .iter()
        .find(|l| l["instrument"] == "violin")
        .unwrap_or_else(|| panic!("violon absent : {body}"));
    assert!(
        violon["artist_id"].is_null(),
        "sans fiche, pas d'id : {body}"
    );
    assert_eq!(violon["track_title"], "Premier");
}

#[tokio::test]
async fn un_album_sans_credit_rend_un_tableau_vide_1572() {
    let app = bibliotheque();
    for chemin in [
        "/api/v1/library/albums/2/credits",
        "/api/v1/library/albums/999/credits",
    ] {
        let (status, body) = get(&app, chemin).await;
        assert_eq!(status, StatusCode::OK, "{chemin} : {body}");
        assert_eq!(tableau(&body).len(), 0, "{chemin} : {body}");
    }
}

#[tokio::test]
async fn les_credits_d_une_piste_restent_inchanges_1572() {
    let app = bibliotheque();
    let (status, body) = get(&app, "/api/v1/library/tracks/11/credits").await;
    assert_eq!(status, StatusCode::OK);
    let roles: Vec<&str> = tableau(&body)
        .iter()
        .map(|l| l["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, vec!["composer", "performer"], "{body}");
}
