//! Couleur d'un dossier « Collections » (#3044, Lulu/JLuc, fil forum 1631).
//!
//! Le client web posait déjà un `<input type="color">` dans le formulaire de
//! création et envoyait `{name, description, icon, color}` ; le serveur
//! désérialisait dans un `CreateCollectionBody` qui ne connaissait ni `color`
//! ni `icon`. Sans `deny_unknown_fields`, serde jetait les deux champs en
//! silence et la route répondait `201 Created` — le « 200 pour rien » : la
//! requête réussit, rien n'est fait. La pastille `{#if col.color}` du client
//! ne pouvait donc jamais être rendue.
//!
//! Les dossiers manuels ne vivent PAS dans une table : ils sont un tableau
//! JSON dans le réglage `collections` (`SettingsRepo`). Il n'y a donc aucune
//! colonne à ajouter, et aucune dérive de schéma SQLite/PostgreSQL possible —
//! le même document JSON est relu par les deux moteurs.
//!
//! Ces tests portent sur le FAIT relu, pas sur le code HTTP : la collection
//! rendue par `GET` porte bien `#A1B2C3`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

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

async fn post_json(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    send(
        app,
        Request::post(path)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

async fn put_json(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    send(
        app,
        Request::put(path)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

/// Le point même du ticket : la couleur choisie au formulaire de création
/// survit à l'aller-retour. Relue par la LISTE — c'est elle qui alimente les
/// cartes de l'onglet Collections.
#[tokio::test]
async fn couleur_de_creation_relue_dans_la_liste() {
    let app = make_app();
    let (st, cree) = post_json(
        &app,
        "/api/v1/library/collections",
        json!({"name": "Jazz du soir", "color": "#A1B2C3"}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "création: {cree}");

    let (st, liste) = get(&app, "/api/v1/library/collections").await;
    assert_eq!(st, StatusCode::OK, "liste: {liste}");
    let dossier = liste
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == json!("Jazz du soir"))
        .unwrap_or_else(|| panic!("dossier absent de la liste: {liste}"));
    assert_eq!(
        dossier["color"],
        json!("#A1B2C3"),
        "la collection relue ne porte pas la couleur envoyée: {dossier}"
    );
}

/// Relue par le DÉTAIL, et déjà présente dans la réponse de création : le
/// client se sert des deux.
#[tokio::test]
async fn couleur_relue_dans_le_detail_et_dans_la_reponse_de_creation() {
    let app = make_app();
    let (st, cree) = post_json(
        &app,
        "/api/v1/library/collections",
        json!({"name": "Coffret", "color": "#A1B2C3"}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "création: {cree}");
    assert_eq!(
        cree["color"],
        json!("#A1B2C3"),
        "la réponse de création ne rend pas la couleur: {cree}"
    );

    let id = cree["id"].as_i64().unwrap();
    let (st, detail) = get(&app, &format!("/api/v1/library/collections/{id}")).await;
    assert_eq!(st, StatusCode::OK, "détail: {detail}");
    assert_eq!(
        detail["color"],
        json!("#A1B2C3"),
        "le détail ne rend pas la couleur: {detail}"
    );
}

/// L'icône empruntait exactement le même chemin mort — le client l'envoie
/// depuis `createCollection(name, description, icon, color)`.
#[tokio::test]
async fn icone_de_creation_relue() {
    let app = make_app();
    let (st, cree) = post_json(
        &app,
        "/api/v1/library/collections",
        json!({"name": "Classique", "icon": "music-note"}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "création: {cree}");
    let id = cree["id"].as_i64().unwrap();
    let (_, detail) = get(&app, &format!("/api/v1/library/collections/{id}")).await;
    assert_eq!(detail["icon"], json!("music-note"), "détail: {detail}");
}

/// Sans couleur, la clé reste `null` : la pastille `{#if col.color}` du client
/// ne doit pas s'allumer sur une chaîne vide.
#[tokio::test]
async fn sans_couleur_la_cle_reste_nulle() {
    let app = make_app();
    let (st, cree) = post_json(
        &app,
        "/api/v1/library/collections",
        json!({"name": "Sans couleur"}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "création: {cree}");
    assert_eq!(cree["color"], json!(null), "création: {cree}");
}

/// Les dossiers créés AVANT ce correctif n'ont pas de couleur, et aucun écran
/// d'édition n'existait : `PUT /library/collections/{id}` est le maillon qui
/// leur en donne une. Le client a déjà `api.updateCollection(id, data)` —
/// écrite, jamais appelable faute de route.
#[tokio::test]
async fn put_pose_une_couleur_sur_un_dossier_existant() {
    let app = make_app();
    let (st, cree) = post_json(
        &app,
        "/api/v1/library/collections",
        json!({"name": "Ancien dossier"}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "création: {cree}");
    let id = cree["id"].as_i64().unwrap();

    let (st, maj) = put_json(
        &app,
        &format!("/api/v1/library/collections/{id}"),
        json!({"color": "#A1B2C3"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "PUT: {maj}");

    let (_, detail) = get(&app, &format!("/api/v1/library/collections/{id}")).await;
    assert_eq!(
        detail["color"],
        json!("#A1B2C3"),
        "la couleur posée par PUT n'est pas relue: {detail}"
    );
    assert_eq!(
        detail["name"],
        json!("Ancien dossier"),
        "le PUT partiel a écrasé le nom: {detail}"
    );
}

/// Un PUT ne doit pas perdre les albums déjà rangés dans le dossier.
#[tokio::test]
async fn put_ne_perd_pas_les_albums() {
    let app = make_app();
    let (_, cree) = post_json(
        &app,
        "/api/v1/library/collections",
        json!({"name": "Avec albums"}),
    )
    .await;
    let id = cree["id"].as_i64().unwrap();
    let (st, _) = post_json(
        &app,
        &format!("/api/v1/library/collections/{id}/albums/1"),
        json!({}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "ajout d'album");

    let (st, _) = put_json(
        &app,
        &format!("/api/v1/library/collections/{id}"),
        json!({"color": "#A1B2C3"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let (_, detail) = get(&app, &format!("/api/v1/library/collections/{id}")).await;
    assert_eq!(detail["album_ids"], json!([1]), "détail: {detail}");
}

/// `col.color` est injecté tel quel dans un attribut `style` du client
/// (`style="background:{col.color}"`). Une valeur qui n'est pas un
/// `#RRGGBB` est refusée à la porte plutôt que stockée.
#[tokio::test]
async fn couleur_hors_format_refusee() {
    let app = make_app();
    for mauvaise in [
        json!("red; background-image:url(x)"),
        json!("#GGGGGG"),
        json!("A1B2C3"),
        json!("#A1B2C"),
        json!(""),
    ] {
        let (st, corps) = post_json(
            &app,
            "/api/v1/library/collections",
            json!({"name": "Bancale", "color": mauvaise}),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::BAD_REQUEST,
            "couleur {mauvaise} acceptée: {corps}"
        );
    }

    let (st, liste) = get(&app, "/api/v1/library/collections").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        liste.as_array().unwrap().len(),
        0,
        "un dossier bancal a tout de même été créé: {liste}"
    );
}

/// La forme courte `#RGB` est un `<input type="color">` valide côté navigateur
/// et doit passer.
#[tokio::test]
async fn couleur_courte_acceptee() {
    let app = make_app();
    let (st, cree) = post_json(
        &app,
        "/api/v1/library/collections",
        json!({"name": "Courte", "color": "#abc"}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "création: {cree}");
    assert_eq!(cree["color"], json!("#abc"), "création: {cree}");
}

/// TÉMOIN — vert avant comme après le correctif : le nom et la description
/// étaient déjà persistés et relus. Si celui-ci rougit, c'est le correctif qui
/// a cassé l'existant, pas le champ `color` qui manquait.
#[tokio::test]
async fn temoin_nom_et_description_toujours_persistes() {
    let app = make_app();
    let (st, cree) = post_json(
        &app,
        "/api/v1/library/collections",
        json!({"name": "Témoin", "description": "un dossier ordinaire"}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "création: {cree}");
    let id = cree["id"].as_i64().unwrap();

    let (st, detail) = get(&app, &format!("/api/v1/library/collections/{id}")).await;
    assert_eq!(st, StatusCode::OK, "détail: {detail}");
    assert_eq!(detail["name"], json!("Témoin"), "détail: {detail}");
    assert_eq!(
        detail["description"],
        json!("un dossier ordinaire"),
        "détail: {detail}"
    );
    assert_eq!(detail["album_ids"], json!([]), "détail: {detail}");
}
