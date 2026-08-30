//! « Ces albums ne sont pas des doublons » — contrat de bout en bout (#1276).
//!
//! Megalo, forum-hifi.fr #41831 p.13 : « Tune me trouve des albums doublons
//! alors que ce sont des releases différentes ».
//!
//! Deux chemins rapprochent des albums, et l'issue vise les deux :
//!
//! * `GET /library/albums/grouped` — l'ALERTE ;
//! * `POST /library/albums/merge-duplicates` — la FUSION, qui déplace les
//!   pistes et **supprime** la ligne perdante. C'est le seul des deux qui ne
//!   se répare pas, donc celui qu'un simple filtre d'affichage aurait laissé
//!   détruire.
//!
//! Le jeu d'essai n'est pas théorique : `merge-duplicates` groupe par
//! `LOWER(title)` **sans regarder l'artiste**. Deux « Greatest Hits »
//! d'artistes différents sont donc, pour lui, un doublon à fusionner — la
//! forme la plus brutale du défaut décrit par l'issue.
//!
//! Chaque test porte sa contre-épreuve : on prouve d'abord que le
//! rapprochement a bien lieu SANS arbitrage, sans quoi un test vert ne dirait
//! rien d'autre que « la requête n'a rien trouvé ».

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;

type Etat = tune_server::state::AppState;

fn app_et_etat() -> (axum::Router, Etat) {
    let state = Etat::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn appel(app: &axum::Router, methode: &str, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(methode)
                .uri(path)
                .header("Content-Type", "application/json")
                .body(Body::empty())
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

fn artiste(state: &Etat, nom: &str) -> i64 {
    state
        .backend
        .execute(
            "INSERT INTO artists (name) VALUES (?)",
            &[&nom as &dyn ToSqlValue],
        )
        .unwrap();
    state.backend.last_insert_rowid()
}

fn album(state: &Etat, titre: &str, artist_id: i64) -> i64 {
    state
        .backend
        .execute(
            "INSERT INTO albums (title, artist_id, source, track_count) VALUES (?, ?, 'local', 0)",
            &[&titre as &dyn ToSqlValue, &artist_id],
        )
        .unwrap();
    state.backend.last_insert_rowid()
}

fn album_existe(state: &Etat, id: i64) -> bool {
    state
        .backend
        .query_one(
            "SELECT COUNT(*) FROM albums WHERE id = ?",
            &[&id as &dyn ToSqlValue],
        )
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
        > 0
}

/// Les deux « Greatest Hits » d'artistes DIFFÉRENTS : le pire cas du
/// rapprochement par `LOWER(title)` seul.
fn deux_homonymes(state: &Etat) -> (i64, i64) {
    let queen = artiste(state, "Queen");
    let abba = artiste(state, "ABBA");
    (
        album(state, "Greatest Hits", queen),
        album(state, "Greatest Hits", abba),
    )
}

// ───────────────────────────────────────────────────────────────────────────
// La porte HTTP
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn declarer_puis_revoquer_une_paire() {
    let (app, state) = app_et_etat();
    let (a, b) = deux_homonymes(&state);

    let (status, body) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{a}/distinct/{b}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "corps : {body}");
    assert_eq!(body["distinct"].as_bool(), Some(true));
    assert_eq!(body["album_a_id"].as_i64(), Some(a.min(b)));
    assert_eq!(body["album_b_id"].as_i64(), Some(a.max(b)));

    // Idempotent, et insensible à l'ordre : une seule ligne.
    let (status, _) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{b}/distinct/{a}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, liste) = appel(&app, "GET", "/api/v1/library/albums/distinct").await;
    assert_eq!(liste["total"].as_i64(), Some(1));
    assert_eq!(liste["items"][0]["a_title"].as_str(), Some("Greatest Hits"));
    assert_eq!(liste["items"][0]["resolved"].as_bool(), Some(true));

    let (status, body) = appel(
        &app,
        "DELETE",
        &format!("/api/v1/library/albums/{b}/distinct/{a}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["distinct"].as_bool(), Some(false));
    let (_, liste) = appel(&app, "GET", "/api/v1/library/albums/distinct").await;
    assert_eq!(liste["total"].as_i64(), Some(0));
}

/// Un 404 nu voudrait dire que le routeur ignore la route ; le corps doit
/// porter l'explication du handler.
#[tokio::test]
async fn ids_invalides_refuses_avec_explication() {
    let (app, state) = app_et_etat();
    let (a, _b) = deux_homonymes(&state);

    let (status, body) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{a}/distinct/999999"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["error"].as_str().is_some_and(|e| e.contains("999999")),
        "corps vide : la route n'est probablement pas montee — {body}"
    );

    let (status, body) = appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{a}/distinct/{a}"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "corps : {body}");
}

// ───────────────────────────────────────────────────────────────────────────
// L'ALERTE — GET /library/albums/grouped
// ───────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn l_alerte_disparait_pour_la_paire_arbitree() {
    let (app, state) = app_et_etat();
    let (a, b) = deux_homonymes(&state);

    // CONTRE-ÉPREUVE : sans arbitrage, la paire EST signalée.
    let (_, avant) = appel(&app, "GET", "/api/v1/library/albums/grouped").await;
    assert_eq!(
        avant["total_groups"].as_i64(),
        Some(1),
        "le rapprochement doit exister avant qu'on prouve qu'il disparaît — {avant}"
    );

    appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{a}/distinct/{b}"),
    )
    .await;

    let (_, apres) = appel(&app, "GET", "/api/v1/library/albums/grouped").await;
    assert_eq!(apres["total_groups"].as_i64(), Some(0), "{apres}");
}

#[tokio::test]
async fn une_autre_paire_n_est_pas_affectee_par_l_arbitrage() {
    let (app, state) = app_et_etat();
    let (a, b) = deux_homonymes(&state);
    let miles = artiste(&state, "Miles Davis");
    let coltrane = artiste(&state, "John Coltrane");
    let c = album(&state, "Blue Train", miles);
    let d = album(&state, "Blue Train", coltrane);

    let (_, avant) = appel(&app, "GET", "/api/v1/library/albums/grouped").await;
    assert_eq!(avant["total_groups"].as_i64(), Some(2), "{avant}");

    appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{a}/distinct/{b}"),
    )
    .await;

    let (_, apres) = appel(&app, "GET", "/api/v1/library/albums/grouped").await;
    assert_eq!(
        apres["total_groups"].as_i64(),
        Some(1),
        "seule la paire arbitrée sort — {apres}"
    );
    assert_eq!(apres["groups"][0]["group_id"].as_str(), Some("Blue Train"));

    // Et la fusion emporte toujours la paire NON arbitrée.
    let (_, fusion) = appel(&app, "POST", "/api/v1/library/albums/merge-duplicates").await;
    assert_eq!(fusion["merged"].as_i64(), Some(1), "{fusion}");
    assert_eq!(fusion["protected"].as_i64(), Some(1));
    assert!(album_existe(&state, a) && album_existe(&state, b));
    assert!(
        album_existe(&state, c) ^ album_existe(&state, d),
        "un seul des deux « Blue Train » doit rester"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// LA FUSION — POST /library/albums/merge-duplicates
// ───────────────────────────────────────────────────────────────────────────

/// La contre-épreuve du garde-fou : SANS arbitrage, la fusion supprime bel et
/// bien une des deux lignes. C'est ce que l'utilisateur subissait.
#[tokio::test]
async fn sans_arbitrage_la_fusion_supprime_une_ligne() {
    let (app, state) = app_et_etat();
    let (a, b) = deux_homonymes(&state);

    let (status, body) = appel(&app, "POST", "/api/v1/library/albums/merge-duplicates").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["merged"].as_i64(), Some(1), "{body}");
    assert_eq!(body["protected"].as_i64(), Some(0));
    assert!(
        album_existe(&state, a) ^ album_existe(&state, b),
        "exactement une des deux lignes doit avoir disparu"
    );
}

#[tokio::test]
async fn la_paire_arbitree_survit_a_la_fusion() {
    let (app, state) = app_et_etat();
    let (a, b) = deux_homonymes(&state);

    appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{a}/distinct/{b}"),
    )
    .await;

    let (_, body) = appel(&app, "POST", "/api/v1/library/albums/merge-duplicates").await;
    assert_eq!(body["merged"].as_i64(), Some(0), "{body}");
    assert_eq!(body["protected"].as_i64(), Some(1));
    assert!(album_existe(&state, a), "album {a} supprimé par la fusion");
    assert!(album_existe(&state, b), "album {b} supprimé par la fusion");

    // Rejouable : une seconde fusion ne grignote pas non plus.
    let (_, body) = appel(&app, "POST", "/api/v1/library/albums/merge-duplicates").await;
    assert_eq!(body["merged"].as_i64(), Some(0), "{body}");
    assert!(album_existe(&state, a) && album_existe(&state, b));
}

// ───────────────────────────────────────────────────────────────────────────
// Survie au renouvellement des rowids — LA raison d'être de la table
// ───────────────────────────────────────────────────────────────────────────

/// Racine music déplacée, « vider la bibliothèque », purge d'orphelines : les
/// lignes `albums` MEURENT et renaissent sous de nouveaux rowids. C'est le cas
/// où un simple couple d'ids aurait perdu l'arbitrage — et où la fusion
/// suivante aurait détruit ce que l'utilisateur avait protégé.
#[tokio::test]
async fn l_arbitrage_suit_la_mort_et_la_renaissance_des_lignes() {
    let (app, state) = app_et_etat();
    let (a, b) = deux_homonymes(&state);
    appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{a}/distinct/{b}"),
    )
    .await;

    // Rescan destructeur : les deux lignes disparaissent, les deux albums
    // reviennent (mêmes titre et artiste) sous de NOUVEAUX ids.
    let queen = state
        .backend
        .query_one(
            "SELECT artist_id FROM albums WHERE id = ?",
            &[&a as &dyn ToSqlValue],
        )
        .unwrap()
        .unwrap()[0]
        .as_i64()
        .unwrap();
    let abba = state
        .backend
        .query_one(
            "SELECT artist_id FROM albums WHERE id = ?",
            &[&b as &dyn ToSqlValue],
        )
        .unwrap()
        .unwrap()[0]
        .as_i64()
        .unwrap();
    state.backend.execute("DELETE FROM albums", &[]).unwrap();
    let neuf_a = album(&state, "Greatest Hits", queen);
    let neuf_b = album(&state, "Greatest Hits", abba);
    assert!(neuf_a != a && neuf_b != b, "les rowids doivent être neufs");

    // La réconciliation — celle que rejouent le démarrage, `scan.rs`,
    // `auto_scan.rs` et les deux portes de purge.
    let stats =
        tune_core::db::album_distinct_repo::AlbumDistinctRepo::with_backend(state.backend.clone())
            .reconcile(false)
            .unwrap();
    assert_eq!(stats.relinked, 1, "l'arbitrage doit être re-rattaché");

    // Preuve par le comportement, pas par la table : l'alerte reste muette et
    // la fusion ne détruit rien.
    let (_, groupes) = appel(&app, "GET", "/api/v1/library/albums/grouped").await;
    assert_eq!(groupes["total_groups"].as_i64(), Some(0), "{groupes}");
    let (_, fusion) = appel(&app, "POST", "/api/v1/library/albums/merge-duplicates").await;
    assert_eq!(fusion["merged"].as_i64(), Some(0), "{fusion}");
    assert!(album_existe(&state, neuf_a) && album_existe(&state, neuf_b));
}

/// Contre-épreuve du test précédent : sans la réconciliation, l'arbitrage
/// pointe des ids morts — la paire est listée « non résolue » et n'empêche
/// plus rien. C'est ce qui prouve que c'est bien la réconciliation qui
/// travaille, et pas un hasard de numérotation.
#[tokio::test]
async fn sans_reconciliation_l_arbitrage_orphelin_ne_protege_plus() {
    let (app, state) = app_et_etat();
    let (a, b) = deux_homonymes(&state);
    appel(
        &app,
        "POST",
        &format!("/api/v1/library/albums/{a}/distinct/{b}"),
    )
    .await;

    let queen = artiste(&state, "Queen (bis)");
    let abba = artiste(&state, "ABBA (bis)");
    state.backend.execute("DELETE FROM albums", &[]).unwrap();
    album(&state, "Greatest Hits", queen);
    album(&state, "Greatest Hits", abba);

    // La paire est toujours là, lisible par son instantané, mais orpheline.
    let (_, liste) = appel(&app, "GET", "/api/v1/library/albums/distinct").await;
    assert_eq!(liste["total"].as_i64(), Some(1));
    assert_eq!(liste["items"][0]["resolved"].as_bool(), Some(false));
    assert_eq!(
        liste["items"][0]["a_title"].as_str(),
        Some("Greatest Hits"),
        "l'instantané doit garder la paire lisible, donc révocable"
    );

    // Les artistes ayant changé de nom, l'identité ne retrouve rien : le
    // rapprochement est de retour, comme avant #1276.
    let (_, groupes) = appel(&app, "GET", "/api/v1/library/albums/grouped").await;
    assert_eq!(groupes["total_groups"].as_i64(), Some(1), "{groupes}");
}
