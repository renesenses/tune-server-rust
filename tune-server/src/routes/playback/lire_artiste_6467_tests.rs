//! #6467 — « Lire » un artiste de la bibliothèque, par la route publique.
//!
//! Contre-épreuve : sans la branche `artist_id`, la route ne trouve aucune
//! source de pistes et répond 400 « no track source specified » ; la file
//! reste vide et `lire_un_artiste_met_cinquante_de_ses_titres_en_file`
//! rougit sur sa longueur.

use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use tower::ServiceExt;
use tune_core::db::zone_repo::ZoneRepo;

const ARTISTE: i64 = 7;
const AUTRE: i64 = 8;

/// Un artiste de `n` titres (un album par tranche de 12), un autre artiste
/// d'un titre, et une zone navigateur.
fn banc(n: i64) -> (AppState, i64) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let mut sql = format!(
        "INSERT INTO artists (id, name) VALUES ({ARTISTE}, 'Alain Bashung'), ({AUTRE}, 'Autre');\n\
         INSERT INTO albums (id, title, artist_id, source) VALUES (900, 'Autre album', {AUTRE}, 'local');\n\
         INSERT INTO tracks (id, title, album_id, artist_id, track_number, file_path, format, duration_ms, source) \
         VALUES (9000, 'Intrus', 900, {AUTRE}, 1, '/banc/intrus.flac', 'flac', 200000, 'local');\n"
    );
    for i in 1..=n {
        let album = 100 + (i - 1) / 12;
        if (i - 1) % 12 == 0 {
            sql.push_str(&format!(
                "INSERT INTO albums (id, title, artist_id, source) VALUES ({album}, 'Album {album}', {ARTISTE}, 'local');\n"
            ));
        }
        sql.push_str(&format!(
            "INSERT INTO tracks (id, title, album_id, artist_id, track_number, file_path, format, duration_ms, source) \
             VALUES ({i}, 'Titre {i}', {album}, {ARTISTE}, {}, '/banc/{i}.flac', 'flac', 200000, 'local');\n",
            (i - 1) % 12 + 1
        ));
    }
    state.backend.execute_batch(&sql).unwrap();
    let zid = ZoneRepo::with_backend(state.backend.clone())
        .create("Navigateur", Some("browser"), None)
        .unwrap();
    (state, zid)
}

async fn lancer(state: &AppState, zid: i64, corps: serde_json::Value) -> StatusCode {
    let rep = router()
        .with_state(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/{zid}/play"))
                .header("Content-Type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = rep.status();
    let _ = to_bytes(rep.into_body(), 1024 * 1024).await;
    statut
}

fn file(state: &AppState, zid: i64) -> Vec<i64> {
    PlayQueueRepo::with_backend(state.backend.clone())
        .get_ordered(zid)
        .unwrap()
        .iter()
        .filter_map(|e| e.track_id)
        .collect()
}

/// LE besoin du fil 2302 : 60 titres en bibliothèque, « Lire » en met 50 en
/// file, tous de l'artiste, sans doublon, et pas un seul album.
#[tokio::test]
async fn lire_un_artiste_met_cinquante_de_ses_titres_en_file() {
    let (state, zid) = banc(60);
    lancer(&state, zid, serde_json::json!({ "artist_id": ARTISTE })).await;
    let ids = file(&state, zid);
    assert_eq!(
        ids.len(),
        50,
        "50 titres au plus, pas {} : {ids:?}",
        ids.len()
    );
    assert!(ids.iter().all(|id| (1..=60).contains(id)), "{ids:?}");
    let mut uniques = ids.clone();
    uniques.sort_unstable();
    uniques.dedup();
    assert_eq!(uniques.len(), 50, "aucun doublon");
    let albums: std::collections::HashSet<i64> = ids.iter().map(|i| (i - 1) / 12).collect();
    assert!(albums.len() > 1, "pas un seul album : {ids:?}");
}

/// Un artiste de 5 titres les joue tous ; `artist_limit` borne en dessous.
#[tokio::test]
async fn un_petit_artiste_joue_tout_et_la_limite_borne_6467() {
    let (state, zid) = banc(5);
    lancer(&state, zid, serde_json::json!({ "artist_id": ARTISTE })).await;
    let mut ids = file(&state, zid);
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2, 3, 4, 5]);

    let (state, zid) = banc(60);
    lancer(
        &state,
        zid,
        serde_json::json!({ "artist_id": ARTISTE, "artist_limit": 10 }),
    )
    .await;
    assert_eq!(file(&state, zid).len(), 10);
    // Plus que 50 n'est jamais accordé.
    let (state, zid) = banc(60);
    lancer(
        &state,
        zid,
        serde_json::json!({ "artist_id": ARTISTE, "artist_limit": 500 }),
    )
    .await;
    assert_eq!(file(&state, zid).len(), 50);
}

/// Les titres bannis du profil ne sont pas tirés.
#[tokio::test]
async fn les_titres_bannis_ne_sont_pas_tires_6467() {
    let (state, _zid) = banc(5);
    let profil = 1;
    tune_core::db::hidden_repo::HiddenRepo::with_backend(state.backend.clone())
        .ban_track(profil, 3)
        .unwrap();
    let mut ids = lire_artiste_6467::pistes_d_artiste_au_hasard(&state, ARTISTE, profil, None);
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2, 4, 5]);
}

/// Un artiste sans titre : refus, file intacte.
#[tokio::test]
async fn un_artiste_sans_titre_est_refuse_6467() {
    let (state, zid) = banc(0);
    let statut = lancer(&state, zid, serde_json::json!({ "artist_id": ARTISTE })).await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
    assert!(file(&state, zid).is_empty());
}

/// Le contexte de lecture est l'artiste, côté bibliothèque.
#[test]
fn le_contexte_est_l_artiste_6467() {
    let body = PlayRequest {
        artist_id: Some(ARTISTE),
        ..Default::default()
    };
    assert_eq!(
        contexte_de_lecture(&body),
        (
            Some("artist".into()),
            Some(ARTISTE.to_string()),
            Some("local".into())
        )
    );
}
