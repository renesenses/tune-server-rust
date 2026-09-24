//! Dossiers de collections (#4853, Gros Bidon, fil 1907).
//!
//! « Rock → Artiste 1, Artiste 2… ; Jazz → … » : ranger les collections — des
//! deux sortes — dans un arbre de dossiers. Décision de Bertrand du
//! 24/09/2026 : arbre, profondeur maximale 3.
//!
//! Ces témoins passent par les ROUTES réelles et relisent l'ARBRE servi
//! (`GET /library/collection-folders`) : c'est lui que l'écran affiche.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE: &str = "/api/v1/library/collection-folders";

fn make_app() -> axum::Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    tune_server::routes::router(state)
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    send(app, Request::get(path).body(Body::empty()).unwrap()).await
}

async fn with_json(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    send(
        app,
        Request::builder()
            .method(method)
            .uri(path)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

async fn delete(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    send(app, Request::delete(path).body(Body::empty()).unwrap()).await
}

async fn dossier(app: &axum::Router, nom: &str, parent: Option<i64>) -> i64 {
    let (st, body) = with_json(app, "POST", BASE, json!({"name": nom, "parent_id": parent})).await;
    assert_eq!(
        st,
        StatusCode::CREATED,
        "création du dossier « {nom} » : {body}"
    );
    body["id"].as_i64().unwrap()
}

async fn collection(app: &axum::Router, nom: &str) -> i64 {
    let (st, body) = with_json(
        app,
        "POST",
        "/api/v1/library/collections",
        json!({"name": nom, "icon": "disc", "color": "#123456"}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    body["id"].as_i64().unwrap()
}

async fn ranger(
    app: &axum::Router,
    kind: &str,
    id: i64,
    folder: Option<i64>,
) -> (StatusCode, Value) {
    with_json(
        app,
        "POST",
        &format!("{BASE}/items/{kind}/{id}"),
        json!({"folder_id": folder}),
    )
    .await
}

async fn arbre(app: &axum::Router) -> Value {
    let (st, body) = get(app, BASE).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    body
}

/// Le dossier `id`, cherché partout dans l'arbre.
fn trouver(noeuds: &Value, id: i64) -> Option<Value> {
    for n in noeuds.as_array()? {
        if n["id"].as_i64() == Some(id) {
            return Some(n.clone());
        }
        if let Some(t) = trouver(&n["folders"], id) {
            return Some(t);
        }
    }
    None
}

fn cles(collections: &Value) -> Vec<(String, i64)> {
    collections
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["kind"].as_str().unwrap().to_string(),
                c["id"].as_i64().unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn creer_renommer_imbriquer_et_lire_l_arbre() {
    let app = make_app();
    let rock = dossier(&app, "Rock", None).await;
    let jazz = dossier(&app, "Jazz", None).await;
    let hard = dossier(&app, "Hard", Some(rock)).await;
    let (st, body) = with_json(
        &app,
        "PATCH",
        &format!("{BASE}/{jazz}"),
        json!({"name": "Jazz & Blues"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");

    let c = collection(&app, "Deep Purple").await;
    let (st, body) = ranger(&app, "collection", c, Some(hard)).await;
    assert_eq!(st, StatusCode::OK, "{body}");

    let t = arbre(&app).await;
    assert_eq!(t["max_depth"], 3);
    let noms: Vec<&str> = t["folders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(noms, ["Rock", "Jazz & Blues"]);
    let h = trouver(&t["folders"], hard).unwrap();
    assert_eq!(h["depth"], 2);
    assert_eq!(h["parent_id"], rock);
    let rangee = &h["collections"][0];
    assert_eq!(rangee["kind"], "collection");
    assert_eq!(rangee["id"], c);
    assert_eq!(
        rangee["name"], "Deep Purple",
        "le client n'a pas à recroiser"
    );
    assert_eq!(rangee["color"], "#123456");
    assert!(
        !cles(&t["collections"]).contains(&("collection".into(), c)),
        "une collection rangée n'est plus à la racine"
    );

    // Les listes plates ne changent pas de forme.
    let (st, plate) = get(&app, "/api/v1/library/collections").await;
    assert_eq!(st, StatusCode::OK);
    assert!(plate.is_array(), "liste plate : {plate}");
    assert!(plate[0].get("folder_id").is_none(), "{plate}");
}

#[tokio::test]
async fn refus_du_cycle() {
    let app = make_app();
    let a = dossier(&app, "A", None).await;
    let b = dossier(&app, "B", Some(a)).await;
    let (st, body) = with_json(
        &app,
        "POST",
        &format!("{BASE}/{a}/move"),
        json!({"parent_id": b}),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("lui-même"),
        "{body}"
    );
    let (st, _) = with_json(
        &app,
        "POST",
        &format!("{BASE}/{a}/move"),
        json!({"parent_id": a}),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    // Rien n'a bougé.
    let t = arbre(&app).await;
    assert_eq!(trouver(&t["folders"], a).unwrap()["parent_id"], Value::Null);
}

#[tokio::test]
async fn refus_du_quatrieme_niveau() {
    let app = make_app();
    let n1 = dossier(&app, "1", None).await;
    let n2 = dossier(&app, "2", Some(n1)).await;
    let n3 = dossier(&app, "3", Some(n2)).await;
    let (st, body) = with_json(&app, "POST", BASE, json!({"name": "4", "parent_id": n3})).await;
    assert_eq!(st, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("profondeur"),
        "{body}"
    );
    // Déplacer un sous-arbre de hauteur 2 sous le niveau 2 : refusé aussi.
    let x = dossier(&app, "x", None).await;
    let _y = dossier(&app, "y", Some(x)).await;
    let (st, _) = with_json(
        &app,
        "POST",
        &format!("{BASE}/{x}/move"),
        json!({"parent_id": n2}),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    // Un dossier de niveau 3 range bien des collections.
    let c = collection(&app, "Au fond").await;
    assert_eq!(
        ranger(&app, "collection", c, Some(n3)).await.0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn supprimer_un_dossier_fait_remonter_son_contenu() {
    let app = make_app();
    let musique = dossier(&app, "Musique", None).await;
    let rock = dossier(&app, "Rock", Some(musique)).await;
    let punk = dossier(&app, "Punk", Some(rock)).await;
    let c = collection(&app, "Clash").await;
    ranger(&app, "collection", c, Some(rock)).await;

    let (st, _) = delete(&app, &format!("{BASE}/{rock}")).await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    let t = arbre(&app).await;
    assert!(trouver(&t["folders"], rock).is_none());
    let m = trouver(&t["folders"], musique).unwrap();
    assert_eq!(m["folders"][0]["id"], punk, "le sous-dossier remonte : {m}");
    assert_eq!(m["folders"][0]["depth"], 2);
    assert_eq!(
        cles(&m["collections"]),
        [("collection".to_string(), c)],
        "la collection remonte"
    );
    // Et la collection existe toujours.
    let (st, _) = get(&app, &format!("/api/v1/library/collections/{c}")).await;
    assert_eq!(st, StatusCode::OK);
}

/// L'id 1 est à la fois une collection simple et une intelligente : rangées
/// dans deux dossiers différents, chacune reste dans le sien.
#[tokio::test]
async fn meme_id_deux_sortes_deux_dossiers() {
    let app = make_app();
    let simple = collection(&app, "favorites").await;
    // Trouver (ou créer) une collection intelligente portant le MÊME id.
    let mut smart = None;
    for _ in 0..50 {
        let (_, liste) = get(&app, "/api/v1/library/smart-collections").await;
        if liste
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"].as_i64() == Some(simple))
        {
            smart = Some(simple);
            break;
        }
        let (st, b) = with_json(
            &app,
            "POST",
            "/api/v1/library/smart-collections",
            json!({"name": "💎 Audiophile", "rules": []}),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{b}");
    }
    let smart = smart.expect("une collection intelligente au même id que la simple");
    assert_eq!(smart, simple);

    let a = dossier(&app, "A", None).await;
    let b = dossier(&app, "B", None).await;
    assert_eq!(
        ranger(&app, "collection", simple, Some(a)).await.0,
        StatusCode::OK
    );
    assert_eq!(
        ranger(&app, "smart", smart, Some(b)).await.0,
        StatusCode::OK
    );

    let t = arbre(&app).await;
    let da = trouver(&t["folders"], a).unwrap();
    let db = trouver(&t["folders"], b).unwrap();
    assert_eq!(
        cles(&da["collections"]),
        [("collection".to_string(), simple)],
        "{da}"
    );
    assert_eq!(
        cles(&db["collections"]),
        [("smart".to_string(), smart)],
        "{db}"
    );
    assert_eq!(da["collections"][0]["name"], "favorites");
}

#[tokio::test]
async fn collection_supprimee_quitte_l_arbre_et_son_id_ne_s_herite_pas() {
    let app = make_app();
    let f = dossier(&app, "Rock", None).await;
    let c = collection(&app, "Éphémère").await;
    ranger(&app, "collection", c, Some(f)).await;
    let (st, _) = delete(&app, &format!("/api/v1/library/collections/{c}")).await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    let t = arbre(&app).await;
    assert_eq!(trouver(&t["folders"], f).unwrap()["collections"], json!([]));

    // L'id d'une collection simple est max + 1 : la suivante REPREND cet id.
    let nouvelle = collection(&app, "Nouvelle").await;
    assert_eq!(nouvelle, c, "prémisse : l'id est réutilisé");
    let t = arbre(&app).await;
    assert_eq!(
        trouver(&t["folders"], f).unwrap()["collections"],
        json!([]),
        "la nouvelle collection n'hérite pas du rangement de l'ancienne"
    );
    assert!(cles(&t["collections"]).contains(&("collection".into(), nouvelle)));
}

#[tokio::test]
async fn refus_types() {
    let app = make_app();
    let (st, body) = with_json(&app, "POST", BASE, json!({"name": "  "})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "nom vide : {body}");
    let (st, _) = with_json(&app, "POST", BASE, json!({"name": "x", "parent_id": 999})).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "parent inexistant");
    let f = dossier(&app, "F", None).await;
    let (st, body) = ranger(&app, "collection", 999, Some(f)).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "collection inexistante : {body}");
    let (st, _) = ranger(&app, "smart", 999_999, Some(f)).await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "collection intelligente inexistante"
    );
    let (st, body) = ranger(&app, "smart_collection", 1, Some(f)).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "sorte inconnue : {body}");
    let (st, _) = delete(&app, &format!("{BASE}/999")).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reordonner_et_retirer() {
    let app = make_app();
    let f = dossier(&app, "F", None).await;
    let c1 = collection(&app, "un").await;
    let c2 = collection(&app, "deux").await;
    ranger(&app, "collection", c1, Some(f)).await;
    ranger(&app, "collection", c2, Some(f)).await;
    let (st, _) = with_json(
        &app,
        "POST",
        &format!("{BASE}/items/collection/{c2}"),
        json!({"folder_id": f, "position": 0}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let t = arbre(&app).await;
    assert_eq!(
        cles(&trouver(&t["folders"], f).unwrap()["collections"]),
        [
            ("collection".to_string(), c2),
            ("collection".to_string(), c1)
        ]
    );
    let (st, _) = delete(&app, &format!("{BASE}/items/collection/{c2}")).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let t = arbre(&app).await;
    assert_eq!(
        cles(&trouver(&t["folders"], f).unwrap()["collections"]),
        [("collection".to_string(), c1)]
    );
    assert!(cles(&t["collections"]).contains(&("collection".into(), c2)));

    // Réordonner deux dossiers frères.
    let g = dossier(&app, "G", None).await;
    let (st, _) = with_json(
        &app,
        "POST",
        &format!("{BASE}/{g}/move"),
        json!({"parent_id": null, "position": 0}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let t = arbre(&app).await;
    let ids: Vec<i64> = t["folders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [g, f]);
}
