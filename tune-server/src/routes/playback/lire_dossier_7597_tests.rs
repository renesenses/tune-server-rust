//! #7597 — lire ou enfiler un dossier avec tout son sous-arbre, dans l'ordre,
//! par les routes publiques `POST /zones/{id}/play` et `.../queue/add`.
//!
//! Contre-épreuve : sans le champ `folder`, serde l'ignore, la route ne trouve
//! aucune source de pistes et répond 400 sans rien mettre en file ; chaque
//! témoin rougit sur la file ou sur le code du refus.

use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use tower::ServiceExt;
use tune_core::db::zone_repo::ZoneRepo;

/// Une bibliothèque de dossiers sous `/banc`, une piste hors bibliothèque, et
/// une zone navigateur (aucun son).
fn banc() -> (AppState, i64) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    // (id, chemin, disque, piste, album)
    let pistes: [(i64, &str, &str, &str, i64); 10] = [
        (1, "/banc/Coffret/CD2/02.flac", "2", "2", 100),
        (2, "/banc/Coffret/CD2/01.flac", "2", "1", 100),
        (3, "/banc/Coffret/CD10/01.flac", "10", "1", 101),
        (4, "/banc/Coffret/CD1/piste 10.flac", "NULL", "NULL", 100),
        (5, "/banc/Coffret/CD1/piste 9.flac", "NULL", "NULL", 100),
        (6, "/banc/Coffret/CD1/Bonus/01.flac", "NULL", "1", 100),
        (7, "/banc/Coffret/a-propos.flac", "NULL", "NULL", 100),
        // Voisins qui ne sont PAS dans le sous-arbre de « Coffret ».
        (8, "/banc/Autre/x.flac", "1", "1", 100),
        (9, "/banc/Coffret 2/y.flac", "1", "1", 100),
        // Hors de tout dossier musical configuré.
        (10, "/ailleurs/z.flac", "1", "1", 100),
    ];
    let mut sql = String::from(
        "INSERT INTO artists (id, name) VALUES (1, 'Artiste');\n\
         INSERT INTO albums (id, title, artist_id, source) VALUES (100, 'A', 1, 'local'), (101, 'B', 1, 'local');\n",
    );
    for (id, chemin, disque, piste, album) in pistes {
        sql.push_str(&format!(
            "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, file_path, format, duration_ms, source) \
             VALUES ({id}, 'Titre {id}', {album}, 1, {disque}, {piste}, '{chemin}', 'flac', 200000, 'local');\n"
        ));
    }
    state.backend.execute_batch(&sql).unwrap();
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set("music_dirs", r#"["/banc"]"#)
        .unwrap();
    let zid = ZoneRepo::with_backend(state.backend.clone())
        .create("Navigateur", Some("browser"), None)
        .unwrap();
    (state, zid)
}

async fn poster(
    state: &AppState,
    uri: String,
    corps: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let rep = router()
        .with_state(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("Content-Type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = rep.status();
    let octets = to_bytes(rep.into_body(), 4 * 1024 * 1024).await.unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(serde_json::Value::Null),
    )
}

async fn lire(state: &AppState, zid: i64, dossier: &str) -> (StatusCode, serde_json::Value) {
    poster(
        state,
        format!("/{zid}/play"),
        serde_json::json!({ "folder": dossier }),
    )
    .await
}

fn file(state: &AppState, zid: i64) -> Vec<i64> {
    PlayQueueRepo::with_backend(state.backend.clone())
        .get_ordered(zid)
        .unwrap()
        .iter()
        .filter_map(|e| e.track_id)
        .collect()
}

/// L'ordre attendu de « Coffret » : le dossier lui-même, puis ses
/// sous-dossiers en tri naturel (CD1, son Bonus, CD2, CD10), et dans chacun
/// disque, piste, puis nom de fichier (« piste 9 » avant « piste 10 »).
const ORDRE_COFFRET: [i64; 7] = [7, 5, 4, 6, 2, 1, 3];

/// LE besoin du fil 2577 : « Tout lire » un dossier joue tout son sous-arbre,
/// dans l'ordre, et rien d'autre — ni le voisin, ni « Coffret 2 ».
#[tokio::test]
async fn lire_un_dossier_joue_tout_le_sous_arbre_dans_l_ordre_7597() {
    let (state, zid) = banc();
    let (statut, corps) = lire(&state, zid, "/banc/Coffret").await;
    assert!(statut.is_success(), "{statut} {corps}");
    assert_eq!(file(&state, zid), ORDRE_COFFRET.to_vec());
}

/// Un séparateur final ne change rien.
#[tokio::test]
async fn le_separateur_final_est_ignore_7597() {
    let (state, zid) = banc();
    let (statut, corps) = lire(&state, zid, "/banc/Coffret/").await;
    assert!(statut.is_success(), "{statut} {corps}");
    assert_eq!(file(&state, zid), ORDRE_COFFRET.to_vec());
}

/// « Ajouter à la file » met le sous-arbre À LA FIN, dans l'ordre ; « Lire
/// ensuite » (`position`) l'insère à la place demandée.
#[tokio::test]
async fn ajouter_un_dossier_a_la_file_a_la_fin_ou_ensuite_7597() {
    let (state, zid) = banc();
    let (statut, corps) = poster(
        &state,
        format!("/{zid}/queue/add"),
        serde_json::json!({ "track_ids": [8, 9] }),
    )
    .await;
    assert!(statut.is_success(), "{statut} {corps}");

    let (statut, corps) = poster(
        &state,
        format!("/{zid}/queue/add"),
        serde_json::json!({ "folder": "/banc/Coffret/CD2" }),
    )
    .await;
    assert!(statut.is_success(), "{statut} {corps}");
    assert_eq!(
        file(&state, zid),
        vec![8, 9, 2, 1],
        "à la fin, dans l'ordre"
    );

    let (statut, corps) = poster(
        &state,
        format!("/{zid}/queue/add"),
        serde_json::json!({ "folder": "/banc/Coffret/CD10", "position": 1 }),
    )
    .await;
    assert!(statut.is_success(), "{statut} {corps}");
    assert_eq!(file(&state, zid), vec![8, 3, 9, 2, 1], "lire ensuite");
}

/// Un dossier sans piste : refus nommé, file intacte.
#[tokio::test]
async fn un_dossier_vide_est_refuse_7597() {
    let (state, zid) = banc();
    let (statut, corps) = lire(&state, zid, "/banc/Vide").await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
    assert_eq!(corps["error"], "folder_no_tracks", "{corps}");
    let (statut, corps) = poster(
        &state,
        format!("/{zid}/queue/add"),
        serde_json::json!({ "folder": "/banc/Vide" }),
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
    assert_eq!(corps["error"], "folder_no_tracks", "{corps}");
    assert!(file(&state, zid).is_empty());
}

/// Traversée et chemins hors bibliothèque : refusés, rien n'est lu — même
/// quand la base contient des pistes sous le chemin demandé.
#[tokio::test]
async fn un_chemin_hors_bibliotheque_est_refuse_7597() {
    let (state, zid) = banc();
    for chemin in [
        "/banc/../ailleurs",
        "/banc/Coffret/./CD2",
        "banc/Coffret",
        "",
    ] {
        let (statut, corps) = lire(&state, zid, chemin).await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "{chemin}");
        assert_eq!(corps["error"], "folder_invalid", "{chemin} : {corps}");
    }
    let (statut, corps) = lire(&state, zid, "/ailleurs").await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
    assert_eq!(corps["error"], "folder_outside_library", "{corps}");
    let (statut, corps) = poster(
        &state,
        format!("/{zid}/queue/add"),
        serde_json::json!({ "folder": "/ailleurs" }),
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
    assert_eq!(corps["error"], "folder_outside_library", "{corps}");
    assert!(file(&state, zid).is_empty(), "rien n'est lu");
}

/// Les titres bannis du profil et les albums masqués ne sont pas lus.
#[tokio::test]
async fn bannis_et_albums_masques_sont_ecartes_7597() {
    let (state, zid) = banc();
    let caches = tune_core::db::hidden_repo::HiddenRepo::with_backend(state.backend.clone());
    caches.ban_track(1, 5).unwrap();
    caches.hide_album(101).unwrap();
    let (statut, corps) = lire(&state, zid, "/banc/Coffret").await;
    assert!(statut.is_success(), "{statut} {corps}");
    assert_eq!(file(&state, zid), vec![7, 4, 6, 2, 1]);
}

/// Le plafond : jusqu'à `PLAFOND_DOSSIER` pistes, tout est lu ; une de plus,
/// et c'est un refus qui dit combien — sans rien lire en partie, la file
/// d'avant reste telle quelle.
#[tokio::test]
async fn le_plafond_refuse_sans_rien_lire_en_partie_7597() {
    let (state, zid) = banc();
    let plafond = lire_dossier_7597::PLAFOND_DOSSIER;
    state
        .backend
        .execute_batch(&format!(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {plafond}) \
             INSERT INTO tracks (id, title, album_id, artist_id, track_number, file_path, format, duration_ms, source) \
             SELECT 100000 + i, 'Grand ' || i, 100, 1, i, '/banc/Grand/' || i || '.flac', 'flac', 1000, 'local' FROM n;"
        ))
        .unwrap();
    let (statut, corps) = lire(&state, zid, "/banc/Grand").await;
    assert!(statut.is_success(), "{statut} {corps}");
    let jouees = file(&state, zid);
    assert_eq!(jouees.len() as i64, plafond, "tout, sans troncature");
    assert_eq!(jouees.first(), Some(&100_001));
    assert_eq!(jouees.last(), Some(&(100_000 + plafond)));

    state
        .backend
        .execute_batch(
            "INSERT INTO tracks (id, title, album_id, artist_id, track_number, file_path, format, duration_ms, source) \
             VALUES (200000, 'De trop', 100, 1, 1, '/banc/Grand/Sous/1.flac', 'flac', 1000, 'local');",
        )
        .unwrap();
    for (uri, corps) in [
        (
            format!("/{zid}/play"),
            serde_json::json!({ "folder": "/banc/Grand" }),
        ),
        (
            format!("/{zid}/queue/add"),
            serde_json::json!({ "folder": "/banc/Grand" }),
        ),
    ] {
        let (statut, corps) = poster(&state, uri, corps).await;
        assert_eq!(statut, StatusCode::BAD_REQUEST);
        assert_eq!(corps["error"], "folder_too_large", "{corps}");
        assert_eq!(
            corps["params"]["count"],
            (plafond + 1).to_string(),
            "{corps}"
        );
        assert_eq!(corps["params"]["max"], plafond.to_string(), "{corps}");
    }
    assert_eq!(file(&state, zid), jouees, "la file d'avant est intacte");
}
