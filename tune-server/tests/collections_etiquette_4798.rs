//! #4798, second volet — un DOSSIER et une collection INTELLIGENTE portent des
//! étiquettes.
//!
//! Relevé pendant #4798 : l'écran Collections (`CollectionsV2.svelte`) montait
//! déjà le bouton Étiquette sur chaque pochette, avec `item_type =
//! "collection"` ou `"smart_collection"` — et `POST /tags/{id}/items` répondait
//! 400, « types admis : … ». Un bouton visible qui échoue à chaque clic.
//!
//! ## Le piège que ce fichier ferme
//!
//! Un dossier vit dans la liste JSON du réglage `collections` (identifiant =
//! max + 1) ; une collection intelligente dans la table `smart_collections`
//! (autoincrément). Les deux espaces se RECOUVRENT : l'id 1 est à la fois
//! « favorites » et « Audiophile » sur le serveur de Bertrand. Le type doit
//! donc rester distinct partout, jamais déduit — même leçon que les playlists
//! intelligentes (#4802). Le test qui compte est celui où les deux objets ont
//! le MÊME identifiant et ne se confondent pas.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! par sa cible `[[test]]` dans `Cargo.toml`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::tag_repo::TagRepo;

/// Un serveur en mémoire **et** son état, pour semer avant d'interroger.
fn app_avec_etat() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    (tune_server::routes::router(state.clone()), state)
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    lire(resp).await
}

async fn post(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
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
    lire(resp).await
}

async fn delete(app: &axum::Router, path: &str) -> StatusCode {
    app.clone()
        .oneshot(Request::delete(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

async fn lire(resp: axum::response::Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

/// Crée un DOSSIER par la route réelle et rend son identifiant.
async fn creer_dossier(app: &axum::Router, nom: &str) -> i64 {
    let (status, body) = post(app, "/api/v1/library/collections", json!({"name": nom})).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "création de « {nom} » : {body}"
    );
    body["id"].as_i64().expect("id du dossier")
}

/// Crée une collection INTELLIGENTE par la route réelle et rend son identifiant.
async fn creer_smart_collection(app: &axum::Router, nom: &str) -> i64 {
    let (status, body) = post(
        app,
        "/api/v1/library/smart-collections",
        json!({"name": nom, "rules": []}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "création de « {nom} » : {body}"
    );
    body["id"]
        .as_i64()
        .expect("id de la collection intelligente")
}

// --- 1. Un dossier ---

/// Le refus d'hier : `collection` était un `item_type` inconnu.
#[tokio::test]
async fn etiqueter_un_dossier_est_accepte_et_se_lit_dans_la_forme_servie() {
    let (app, state) = app_avec_etat();
    let cid = creer_dossier(&app, "Vinyles à réécouter").await;
    let tag = TagRepo::with_backend(state.backend.clone())
        .create("Rituels", None)
        .unwrap();

    let (status, body) = post(
        &app,
        &format!("/api/v1/tags/{tag}/items"),
        json!({"item_type": "collection", "item_id": cid}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "étiqueter un dossier doit passer, le serveur dit {status} : {body}"
    );

    // Lisible par son NOM, dans la forme SERVIE de `/library/collections` :
    // `album_count` compté, `orphan_album_ids` présent.
    let (status, body) = get(&app, &format!("/api/v1/tags/{tag}/collections")).await;
    assert_eq!(status, StatusCode::OK, "404 = route absente");
    assert_eq!(body["tag_id"], json!(tag));
    assert_eq!(body["count"], json!(1), "{body}");
    assert_eq!(body["collections"][0]["id"], json!(cid));
    assert_eq!(body["collections"][0]["name"], json!("Vinyles à réécouter"));
    assert_eq!(body["collections"][0]["album_count"], json!(0), "{body}");
    assert!(
        body["collections"][0]["orphan_album_ids"].is_number(),
        "{body}"
    );

    // Et la pose se relit depuis l'objet, comme pour un album.
    let (_, pour) = get(&app, &format!("/api/v1/tags/for/collection/{cid}")).await;
    assert_eq!(pour.as_array().map(|a| a.len()), Some(1), "{pour}");

    // Désétiqueter : la route générique, avec le type dans le chemin.
    let status = delete(&app, &format!("/api/v1/tags/{tag}/items/collection/{cid}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, body) = get(&app, &format!("/api/v1/tags/{tag}/collections")).await;
    assert_eq!(body["count"], json!(0), "{body}");
}

/// Un dossier étiqueté puis supprimé est omis, sans casser la lecture.
#[tokio::test]
async fn un_dossier_disparu_est_omis() {
    let (app, state) = app_avec_etat();
    let cid = creer_dossier(&app, "Éphémère").await;
    let tags = TagRepo::with_backend(state.backend.clone());
    let tag = tags.create("Fantôme", None).unwrap();
    tags.tag_item(tag, "collection", cid).unwrap();

    let status = delete(&app, &format!("/api/v1/library/collections/{cid}")).await;
    assert!(status.is_success(), "suppression du dossier : {status}");

    let (status, body) = get(&app, &format!("/api/v1/tags/{tag}/collections")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], json!(0), "{body}");
}

// --- 2. Une collection intelligente ---

/// Le refus d'hier : `smart_collection` était un `item_type` inconnu.
#[tokio::test]
async fn etiqueter_une_collection_intelligente_est_accepte_et_se_lit_par_son_nom() {
    let (app, state) = app_avec_etat();
    let sid = creer_smart_collection(&app, "Découvertes du mois").await;
    let tag = TagRepo::with_backend(state.backend.clone())
        .create("Rituels", None)
        .unwrap();

    let (status, body) = post(
        &app,
        &format!("/api/v1/tags/{tag}/items"),
        json!({"item_type": "smart_collection", "item_id": sid}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "étiqueter une collection intelligente doit passer, le serveur dit {status} : {body}"
    );

    // Lisible par son NOM, dans la forme de `/library/smart-collections`.
    let (status, body) = get(&app, &format!("/api/v1/tags/{tag}/smart-collections")).await;
    assert_eq!(status, StatusCode::OK, "404 = route absente");
    assert_eq!(body["tag_id"], json!(tag));
    assert_eq!(body["count"], json!(1), "{body}");
    assert_eq!(body["smart_collections"][0]["id"], json!(sid));
    assert_eq!(
        body["smart_collections"][0]["name"],
        json!("Découvertes du mois")
    );
    assert_eq!(body["smart_collections"][0]["match_mode"], json!("all"));
    assert!(body["smart_collections"][0]["rules"].is_array(), "{body}");

    let (_, pour) = get(&app, &format!("/api/v1/tags/for/smart_collection/{sid}")).await;
    assert_eq!(pour.as_array().map(|a| a.len()), Some(1), "{pour}");

    let status = delete(
        &app,
        &format!("/api/v1/tags/{tag}/items/smart_collection/{sid}"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, body) = get(&app, &format!("/api/v1/tags/{tag}/smart-collections")).await;
    assert_eq!(body["count"], json!(0), "{body}");
}

/// Les seize collections du semis portent une clé stable à côté de leur nom
/// (`name_key`) : la lecture par étiquette passe par le MÊME décodeur que
/// `/library/smart-collections`, le client les traduit donc ici aussi.
#[tokio::test]
async fn une_collection_du_semis_garde_sa_cle_de_traduction() {
    let (app, state) = app_avec_etat();
    let audiophile = state
        .backend
        .query_one(
            "SELECT id FROM smart_collections WHERE name LIKE '%Audiophile%'",
            &[],
        )
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .expect("le semis pose une collection « Audiophile »");
    let tags = TagRepo::with_backend(state.backend.clone());
    let tag = tags.create("Semis", None).unwrap();
    tags.tag_item(tag, "smart_collection", audiophile).unwrap();

    let (_, body) = get(&app, &format!("/api/v1/tags/{tag}/smart-collections")).await;
    assert_eq!(body["count"], json!(1), "{body}");
    assert_eq!(
        body["smart_collections"][0]["name_key"],
        json!("smartCollection.default.audiophile"),
        "{body}"
    );
}

/// Même enveloppe `{tag_id, <pluriel>, count}` que ses sœurs, à vide.
#[tokio::test]
async fn les_deux_routes_ont_la_meme_enveloppe_que_leurs_soeurs() {
    let (app, state) = app_avec_etat();
    let tag = TagRepo::with_backend(state.backend.clone())
        .create("Vide", None)
        .unwrap();
    for (route, pluriel) in [
        ("collections", "collections"),
        ("smart-collections", "smart_collections"),
    ] {
        let (status, body) = get(&app, &format!("/api/v1/tags/{tag}/{route}")).await;
        assert_eq!(status, StatusCode::OK, "{route}");
        assert_eq!(body["tag_id"], json!(tag));
        assert_eq!(body["count"], json!(0));
        assert!(body[pluriel].is_array(), "{route} : {body}");
    }
}

/// Une collection intelligente étiquetée puis supprimée est omise.
#[tokio::test]
async fn une_collection_intelligente_disparue_est_omise() {
    let (app, state) = app_avec_etat();
    let sid = creer_smart_collection(&app, "Éphémère").await;
    let tags = TagRepo::with_backend(state.backend.clone());
    let tag = tags.create("Fantôme", None).unwrap();
    tags.tag_item(tag, "smart_collection", sid).unwrap();

    let status = delete(&app, &format!("/api/v1/library/smart-collections/{sid}")).await;
    assert!(status.is_success(), "suppression : {status}");

    let (status, body) = get(&app, &format!("/api/v1/tags/{tag}/smart-collections")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], json!(0), "{body}");
}

/// #4678 reste : un `item_id` nul ou négatif est refusé en 400 pour les deux
/// nouveaux types comme pour les autres, et rien n'est écrit.
#[tokio::test]
async fn un_item_id_nul_ou_negatif_reste_refuse_pour_les_deux_types() {
    let (app, state) = app_avec_etat();
    let tag = TagRepo::with_backend(state.backend.clone())
        .create("Garde", None)
        .unwrap();

    for item_type in ["collection", "smart_collection"] {
        for faux in [0, -1] {
            let (status, _) = post(
                &app,
                &format!("/api/v1/tags/{tag}/items"),
                json!({"item_type": item_type, "item_id": faux}),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{item_type} : item_id = {faux} doit être refusé en 400, le serveur dit {status}"
            );
        }
        let (status, _) = post(
            &app,
            &format!("/api/v1/tags/{tag}/items/batch"),
            json!({"item_type": item_type, "item_ids": [3, 0]}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{item_type} : le lot doit être refusé entier"
        );
    }

    let (_, items) = get(&app, &format!("/api/v1/tags/{tag}/items")).await;
    assert_eq!(
        items["items"],
        json!([]),
        "un refus a quand même écrit : {items}"
    );
}

// --- 3. Le test qui compte : même identifiant, deux objets ---

/// Un dossier et une collection intelligente de MÊME id ne se confondent pas.
/// Le réglage `collections` est semé jusqu'à obtenir le même entier que la
/// collection intelligente — la situation réelle de l'id 1 sur le serveur de
/// Bertrand.
#[tokio::test]
async fn un_dossier_et_une_collection_intelligente_de_meme_id_ne_se_confondent_pas() {
    let (app, state) = app_avec_etat();
    let sid = creer_smart_collection(&app, "Intelligente").await;
    // `smart_collections` est semée par les migrations : l'intelligente ne
    // porte pas l'id 1. Les dossiers, eux, partent de 1 (max + 1) : on en
    // crée jusqu'à ce que l'un d'eux reçoive le MÊME entier — c'est lui, la
    // jumelle, et c'est son nom que la route des dossiers doit rendre.
    let mut nom_jumelle = String::from("Manuelle");
    let mut cid = creer_dossier(&app, &nom_jumelle).await;
    let mut essais = 0;
    while cid < sid && essais < 256 {
        nom_jumelle = format!("Manuelle {essais}");
        cid = creer_dossier(&app, &nom_jumelle).await;
        essais += 1;
    }
    assert_eq!(
        cid, sid,
        "le banc doit produire le MÊME identifiant dans les deux espaces"
    );

    // Étiquette : les deux, sous le même entier — deux lignes, deux routes.
    let tag = TagRepo::with_backend(state.backend.clone())
        .create("Jumeaux", None)
        .unwrap();
    for item_type in ["collection", "smart_collection"] {
        let (status, _) = post(
            &app,
            &format!("/api/v1/tags/{tag}/items"),
            json!({"item_type": item_type, "item_id": sid}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{item_type}");
    }
    let (_, items) = get(&app, &format!("/api/v1/tags/{tag}/items")).await;
    assert_eq!(
        items["items"].as_array().map(|a| a.len()),
        Some(2),
        "{items}"
    );

    // Chaque route ne rend que SON objet, sous SON nom.
    let (_, co) = get(&app, &format!("/api/v1/tags/{tag}/collections")).await;
    assert_eq!(co["count"], json!(1), "{co}");
    assert_eq!(co["collections"][0]["name"], json!(nom_jumelle));
    let (_, sc) = get(&app, &format!("/api/v1/tags/{tag}/smart-collections")).await;
    assert_eq!(sc["count"], json!(1), "{sc}");
    assert_eq!(sc["smart_collections"][0]["name"], json!("Intelligente"));

    // `/tags/for/…` distingue aussi par le type : un troisième objet du même
    // numéro, d'un type non étiqueté, ne voit rien.
    let (_, pour_pl) = get(&app, &format!("/api/v1/tags/for/playlist/{sid}")).await;
    assert_eq!(pour_pl.as_array().map(|a| a.len()), Some(0), "{pour_pl}");

    // Désétiqueter le dossier laisse l'intelligente étiquetée.
    let status = delete(&app, &format!("/api/v1/tags/{tag}/items/collection/{cid}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, co) = get(&app, &format!("/api/v1/tags/{tag}/collections")).await;
    assert_eq!(co["count"], json!(0), "{co}");
    let (_, sc) = get(&app, &format!("/api/v1/tags/{tag}/smart-collections")).await;
    assert_eq!(
        sc["count"],
        json!(1),
        "désétiqueter le dossier {cid} a emporté la collection intelligente {sid} : {sc}"
    );
}
