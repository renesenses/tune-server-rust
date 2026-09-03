//! Un morceau présent deux fois dans un album ne doit pas se jouer deux fois
//! (#1362).
//!
//! **Cyrille Moutia**, fil forum 1260 (01/08/2026) :
//!
//! > « J'ai un album correspondant à un CD rippé en AIFF ; j'ai un des morceaux
//! > de cet album d'une autre provenance en AAC. […] Le problème à l'écoute,
//! > c'est que je lance la lecture de l'album complet, il lira deux fois le
//! > morceau dans ses 2 résolutions différentes. »
//!
//! L'écran, lui, ne montrait qu'une ligne : la liste d'album passe par
//! `dedup_display_tracks`. C'est la FILE qui prenait `list_by_album` brut. Le
//! défaut n'est donc pas « l'album est mal regroupé » — c'est que l'écran et la
//! file ne répondaient pas la même chose, et que la zone recevait des pistes
//! que l'écran cache.
//!
//! Ce fichier attaque par la route publique, pas par la fonction interne : ce
//! qui doit être prouvé est le CÂBLAGE (`POST /zones/{id}/queue/add` avec un
//! `album_id`), pas le barème — celui-ci est testé dans
//! `tune_core::library::quality`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

fn app_et_etat() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn post_json(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn get_json(app: &axum::Router, path: &str) -> Value {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// L'album de Cyrille : trois fichiers, deux morceaux.
///
/// - piste 4 « Time » en **AIFF 44,1/16** (le rip du CD) ;
/// - piste 4 « Time » en **AAC 44,1/16** (l'autre provenance, posée dans le
///   même dossier) — même album, même numéro, même titre ;
/// - piste 5 « Money » en AIFF, le témoin qui ne doit jamais disparaître.
///
/// L'AAC est inséré **avant** l'AIFF pour que « garder le premier venu » ne
/// puisse pas passer par chance : le premier de la requête (`ORDER BY
/// disc, track, title`, départagé par l'ordre d'insertion) est le fichier
/// avec perte.
fn bibliotheque_de_cyrille(state: &tune_server::state::AppState) {
    state
        .backend
        .execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'Pink Floyd');\
             INSERT INTO albums (id, title, artist_id) VALUES (1, 'The Dark Side of the Moon', 1);\
             INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, \
                                 duration_ms, file_path, format, sample_rate, bit_depth, source) \
             VALUES (10, 'Time', 1, 1, 1, 4, 413000, '/musique/dsotm/04 Time.m4a', 'aac', 44100, 16, 'local'),\
                    (11, 'Time', 1, 1, 1, 4, 413000, '/musique/dsotm/04 Time.aiff', 'aiff', 44100, 16, 'local'),\
                    (12, 'Money', 1, 1, 1, 5, 382000, '/musique/dsotm/05 Money.aiff', 'aiff', 44100, 16, 'local');",
        )
        .unwrap();
}

/// Le défaut lui-même : la file d'attente ne doit pas recevoir les deux
/// copies. Sans le correctif, `added` vaut 3 et « Time » se joue deux fois.
#[tokio::test]
async fn ajouter_l_album_a_la_file_n_enfile_pas_le_meme_morceau_deux_fois() {
    let (app, state) = app_et_etat();
    bibliotheque_de_cyrille(&state);

    let (status, body) = post_json(&app, "/api/v1/zones/1/queue/add", json!({"album_id": 1})).await;
    assert_eq!(status, StatusCode::CREATED, "corps: {body}");

    assert_eq!(
        body["added"].as_i64(),
        Some(2),
        "trois fichiers, deux morceaux : la file doit en contenir DEUX. \
         À 3, « Time » se joue deux fois — le rapport de Cyrille."
    );

    let file = get_json(&app, "/api/v1/zones/1/queue").await;
    let titres: Vec<String> = file["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["title"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        titres,
        vec!["Time".to_string(), "Money".to_string()],
        "le témoin « Money » doit survivre au repli : on replie des copies, \
         on ne tronque pas l'album"
    );
}

/// Et c'est le **bon** fichier qui reste. Replier en gardant « le premier de
/// la requête » aurait remplacé un défaut par un autre : l'album entier joué
/// depuis la copie AAC, sans que rien ne le dise.
#[tokio::test]
async fn la_copie_retenue_est_la_meilleure_pas_la_premiere_venue() {
    let (app, state) = app_et_etat();
    bibliotheque_de_cyrille(&state);

    let (status, _) = post_json(&app, "/api/v1/zones/1/queue/add", json!({"album_id": 1})).await;
    assert_eq!(status, StatusCode::CREATED);

    let file = get_json(&app, "/api/v1/zones/1/queue").await;
    let premiere = &file["tracks"][0];
    assert_eq!(premiere["title"].as_str(), Some("Time"));
    assert_eq!(
        premiere["format"].as_str(),
        Some("aiff"),
        "l'AIFF est le rip du CD ; l'AAC est la copie. C'est le sans-perte \
         qui doit rester dans la file, quel que soit l'ordre des lignes."
    );
    assert_eq!(
        premiere["track_id"].as_i64(),
        Some(11),
        "la ligne retenue est bien le fichier AIFF"
    );
}

/// L'écran et la file doivent dire la même chose — c'est le désaccord entre
/// les deux qui a produit le défaut. La liste d'album répliait déjà ; on
/// vérifie ici qu'elle réplique sur la MÊME copie que la file, sinon la
/// pastille de qualité annoncerait un fichier que la zone n'ira pas chercher.
#[tokio::test]
async fn l_ecran_de_l_album_montre_la_copie_qui_sera_jouee() {
    let (app, state) = app_et_etat();
    bibliotheque_de_cyrille(&state);

    let vue = get_json(&app, "/api/v1/library/albums/1/tracks").await;
    let pistes = vue
        .get("tracks")
        .and_then(|v| v.as_array())
        .or_else(|| vue.as_array())
        .expect("la route rend une liste de pistes");

    assert_eq!(pistes.len(), 2, "l'écran repliait déjà : deux lignes");
    assert_eq!(pistes[0]["title"].as_str(), Some("Time"));
    assert_eq!(
        pistes[0]["format"].as_str(),
        Some("aiff"),
        "l'écran doit montrer la copie que la file jouera"
    );
}
