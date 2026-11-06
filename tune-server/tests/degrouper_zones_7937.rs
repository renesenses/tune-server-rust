//! #7937 (fil forum 2661) — un groupe de lecture ne se dissolvait pas : il
//! revenait.
//!
//! Deux chemins le ramenaient, tous deux par le réglage `zone_groups` :
//! - supprimer une zone n'est qu'un masquage ; `POST /zones` sur le même
//!   appareil la ressuscite sous le MÊME numéro, que rien n'avait retiré de
//!   `zone_groups` : la zone recréée retombait dans son groupe ;
//! - `POST /zones/group` empilait un nouveau groupe sans en retirer ses zones
//!   ailleurs (le client web regroupe sans dégrouper d'abord) : défaire le
//!   dernier laissait l'ancien en place, et il survivait au redémarrage.
//!
//! Le banc lit les groupes comme le client web (`GET /zones/groups/list`), et
//! « redémarre » en rouvrant un second `AppState` sur la même base.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! par son entrée `[[test]]` dans `Cargo.toml`.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::zone_repo::ZoneRepo;
use tune_server::state::AppState;

async fn appeler(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(methode).uri(chemin);
    if corps.is_some() {
        req = req.header("content-type", "application/json");
    }
    let body = corps
        .map(|c| Body::from(c.to_string()))
        .unwrap_or_else(Body::empty);
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

async fn ouvrir(chemin: &str) -> (AppState, axum::Router) {
    let state = AppState::new(chemin, 0, Default::default()).unwrap();
    // Le regroupement relève du droit « Multiroom Sync ».
    state.license.set_account_premium(true, None).await;
    let app = tune_server::routes::router(state.clone());
    (state, app)
}

/// Les groupes tels que le client web les lit : `(group_id, zone_ids)`.
async fn groupes(app: &axum::Router) -> Vec<(String, Vec<i64>)> {
    let (status, corps) = appeler(app, "GET", "/api/v1/zones/groups/list", None).await;
    assert_eq!(status, StatusCode::OK, "liste des groupes : {corps}");
    corps
        .as_array()
        .expect("une liste")
        .iter()
        .map(|g| {
            let ids = g["zone_ids"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_i64)
                .collect();
            (g["group_id"].as_str().unwrap_or_default().to_string(), ids)
        })
        .collect()
}

async fn grouper(app: &axum::Router, meneuse: i64, zones: &[i64]) -> String {
    let (status, corps) = appeler(
        app,
        "POST",
        "/api/v1/zones/group",
        Some(json!({ "leader_id": meneuse, "zone_ids": zones })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "groupement : {corps}");
    corps["group_id"].as_str().unwrap().to_string()
}

struct Banc {
    _dir: tempfile::TempDir,
    chemin: String,
    app: axum::Router,
    zones: Vec<i64>,
}

async fn banc(n: usize) -> Banc {
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("tune.db").to_string_lossy().into_owned();
    let (state, app) = ouvrir(&chemin).await;
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let zones = (0..n)
        .map(|i| {
            repo.create(
                &format!("AirPlay {i}"),
                Some("airplay"),
                Some(&format!("airplay-{i}")),
            )
            .unwrap()
        })
        .collect();
    Banc {
        _dir: dir,
        chemin,
        app,
        zones,
    }
}

/// Supprimer une zone groupée la sort du groupe ; la recréer (même appareil,
/// donc même numéro) ne la remet dans aucun groupe — redémarrage compris.
#[tokio::test]
async fn une_zone_supprimee_puis_recreee_n_herite_d_aucun_groupe() {
    let b = banc(2).await;
    let (salon, cuisine) = (b.zones[0], b.zones[1]);
    grouper(&b.app, salon, &[salon, cuisine]).await;
    assert_eq!(groupes(&b.app).await.len(), 1);

    let (status, _) = appeler(&b.app, "DELETE", &format!("/api/v1/zones/{cuisine}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        groupes(&b.app).await.is_empty(),
        "#7937 : la zone supprimée est restée dans son groupe"
    );

    let (status, recreee) = appeler(
        &b.app,
        "POST",
        "/api/v1/zones",
        Some(json!({ "name": "AirPlay 1", "output_type": "airplay", "output_device_id": "airplay-1" })),
    )
    .await;
    assert!(status.is_success(), "recréation : {status} {recreee}");
    assert_eq!(
        recreee["id"].as_i64(),
        Some(cuisine),
        "même appareil, même numéro"
    );
    assert!(
        groupes(&b.app).await.is_empty(),
        "#7937 : la zone recréée est revenue avec la mémoire du groupe"
    );

    let (_state, app) = ouvrir(&b.chemin).await;
    assert!(
        groupes(&app).await.is_empty(),
        "#7937 : le groupe est revenu au redémarrage"
    );
}

/// Le client web regroupe sans dégrouper d'abord : les zones du nouveau
/// groupe quittent l'ancien, et « Dégrouper » ne laisse rien derrière lui,
/// redémarrage compris.
#[tokio::test]
async fn regrouper_puis_degrouper_ne_laisse_aucun_groupe() {
    let b = banc(3).await;
    let (a, z2, z3) = (b.zones[0], b.zones[1], b.zones[2]);
    grouper(&b.app, a, &[a, z2]).await;
    let dernier = grouper(&b.app, a, &[a, z3]).await;

    let liste = groupes(&b.app).await;
    assert_eq!(
        liste.len(),
        1,
        "#7937 : l'ancien groupe a survécu au nouveau : {liste:?}"
    );

    let (status, _) = appeler(
        &b.app,
        "DELETE",
        &format!("/api/v1/zones/group/{dernier}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let liste = groupes(&b.app).await;
    assert!(
        liste.is_empty(),
        "#7937 : après « Dégrouper », un groupe reste : {liste:?}"
    );

    let (_state, app) = ouvrir(&b.chemin).await;
    assert!(
        groupes(&app).await.is_empty(),
        "#7937 : le groupe est revenu au redémarrage"
    );
}

/// Un nouveau groupe ne reprend pas le numéro d'un groupe encore là : sinon
/// « Dégrouper » l'un défait aussi l'autre, ou n'atteint pas le bon.
#[tokio::test]
async fn un_nouveau_groupe_ne_reprend_pas_le_numero_d_un_groupe_restant() {
    let b = banc(6).await;
    let z = &b.zones;
    let premier = grouper(&b.app, z[0], &[z[0], z[1]]).await;
    let second = grouper(&b.app, z[2], &[z[2], z[3]]).await;
    let (status, _) = appeler(
        &b.app,
        "DELETE",
        &format!("/api/v1/zones/group/{premier}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let troisieme = grouper(&b.app, z[4], &[z[4], z[5]]).await;
    assert_ne!(
        troisieme, second,
        "#7937 : le nouveau groupe a repris le numéro d'un groupe restant"
    );

    let (status, _) = appeler(
        &b.app,
        "DELETE",
        &format!("/api/v1/zones/group/{troisieme}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let liste = groupes(&b.app).await;
    assert_eq!(liste, vec![(second, vec![z[2], z[3]])]);
}
