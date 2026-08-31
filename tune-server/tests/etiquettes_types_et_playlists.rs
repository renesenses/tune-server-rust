//! Étiquettes : la route des playlists, et le refus d'un `item_type` inconnu.
//!
//! #2256. Deux défauts distincts, tous deux invisibles avant ce fichier.
//!
//! 1. **Il manquait des routes de lecture.** `tags::router()` résolvait les
//!    objets pour `albums`, `tracks` et `artists` seulement. Une étiquette
//!    posée sur une playlist n'était lisible que par `/{id}/items`, qui rend
//!    des paires `{item_type, item_id}` — des identifiants BRUTS, sans nom :
//!    de quoi afficher une liste de numéros.
//!
//! 2. **`item_type` n'était validé nulle part.** `TagRepo::tag_item` partait
//!    droit dans l'`INSERT`. Un `"albums"` au pluriel créait un type parallèle
//!    en silence : compté par `/tags` (le `COUNT(*)` ne filtre pas), servi par
//!    aucune route de lecture, donc perdu pour toujours.
//!
//! Ce que ce fichier ne fait PAS : traiter le label. Un label n'a pas
//! d'identité numérique dans ce dépôt — pas de table `labels`, pas de
//! `label_id` ; l'onglet Labels lit la colonne libre `tracks.label` en facette
//! et sélectionne par CHAÎNE, ce que `favorite_facets_repo` a déjà dû
//! constater pour les favoris. Or `item_tags.item_id` est `INTEGER NOT NULL`.
//! `/tags/{id}/labels` ne peut pas être écrite tant que ce point de modèle
//! n'est pas tranché.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`. Voir
//! `tests_orphelins.rs`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::playlist_repo::PlaylistRepo;
use tune_core::db::tag_repo::TagRepo;

/// Un serveur en mémoire **et** ses dépôts, pour semer avant d'interroger.
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

// --- 1. La route de lecture des playlists ---

/// Le défaut du ticket : l'étiquette est posée sur une playlist, et rien ne
/// sait la rendre autrement qu'en numéro.
///
/// Le test exige le **nom**, pas seulement le compte : une route qui
/// renverrait `{"playlists": [3]}` passerait un test sur `count` et
/// n'afficherait toujours rien d'utilisable.
#[tokio::test]
async fn la_route_des_playlists_rend_des_objets_resolus_et_pas_des_numeros() {
    let (app, state) = app_avec_etat();

    let playlists = PlaylistRepo::with_backend(state.backend.clone());
    let pid = playlists
        .create("Dimanche matin", Some("café et ECM"), 1)
        .unwrap();

    let tags = TagRepo::with_backend(state.backend.clone());
    let tag = tags.create("Rituels", None).unwrap();
    tags.tag_item(tag, "playlist", pid).unwrap();

    let (status, body) = get(&app, &format!("/api/v1/tags/{tag}/playlists")).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "GET /api/v1/tags/{{id}}/playlists rend {status} — 404 = route absente"
    );
    assert_eq!(body["tag_id"], json!(tag));
    assert_eq!(body["count"], json!(1));
    assert_eq!(
        body["playlists"][0]["name"],
        json!("Dimanche matin"),
        "la route doit résoudre la playlist, pas rendre son identifiant nu : {body}"
    );
    assert_eq!(body["playlists"][0]["id"], json!(pid));
    assert_eq!(body["playlists"][0]["description"], json!("café et ECM"));
}

/// La forme de réponse est celle des trois routes existantes — même enveloppe
/// `{tag_id, <pluriel>, count}`. Une quatrième route qui inventerait sa propre
/// forme obligerait le client à deux chemins de lecture.
#[tokio::test]
async fn la_route_des_playlists_a_la_meme_enveloppe_que_ses_trois_soeurs() {
    let (app, state) = app_avec_etat();
    let tags = TagRepo::with_backend(state.backend.clone());
    let tag = tags.create("Vide", None).unwrap();

    for (chemin, pluriel) in [
        ("albums", "albums"),
        ("tracks", "tracks"),
        ("artists", "artists"),
        ("playlists", "playlists"),
    ] {
        let (status, body) = get(&app, &format!("/api/v1/tags/{tag}/{chemin}")).await;
        assert_eq!(status, StatusCode::OK, "/{chemin} rend {status}");
        assert_eq!(body["tag_id"], json!(tag), "/{chemin} : tag_id");
        assert_eq!(body["count"], json!(0), "/{chemin} : count");
        assert!(
            body[pluriel].is_array(),
            "/{chemin} : la clé « {pluriel} » doit être un tableau, corps = {body}"
        );
    }
}

/// Une playlist étiquetée puis supprimée ne doit pas faire tomber la route :
/// elle est simplement omise, comme le font déjà `albums` et `tracks`.
#[tokio::test]
async fn une_playlist_disparue_est_omise_et_ne_casse_pas_la_lecture() {
    let (app, state) = app_avec_etat();
    let tags = TagRepo::with_backend(state.backend.clone());
    let tag = tags.create("Fantome", None).unwrap();
    tags.tag_item(tag, "playlist", 4242).unwrap();

    let (status, body) = get(&app, &format!("/api/v1/tags/{tag}/playlists")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], json!(0), "corps = {body}");
}

// --- 2. Le refus d'un `item_type` inconnu ---

/// La faute de frappe qui créait un type parallèle en silence.
///
/// Deux exigences : le code (400, pas 201 et pas 500 — le client doit pouvoir
/// distinguer sa faute d'une panne serveur) **et** l'absence d'écriture,
/// vérifiée par la route générique qui, elle, voit TOUS les types.
#[tokio::test]
async fn un_item_type_inconnu_est_refuse_en_400_et_n_ecrit_rien() {
    let (app, state) = app_avec_etat();
    let tags = TagRepo::with_backend(state.backend.clone());
    let tag = tags.create("Jazz", None).unwrap();

    let (status, _) = post(
        &app,
        &format!("/api/v1/tags/{tag}/items"),
        json!({"item_type": "albums", "item_id": 1}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "« albums » au pluriel doit être refusé en 400, il rend {status}"
    );

    let (_, items) = get(&app, &format!("/api/v1/tags/{tag}/items")).await;
    assert_eq!(
        items["items"],
        json!([]),
        "le type refusé a quand même été écrit : {items}"
    );
}

/// Le lot est l'autre chemin d'écriture — celui qui écrit par centaines. Il
/// ne passe pas par `tag_item` : sans vérification propre, il resterait le
/// trou du garde-fou.
#[tokio::test]
async fn le_lot_refuse_un_item_type_inconnu_sans_ecriture_partielle() {
    let (app, state) = app_avec_etat();
    let tags = TagRepo::with_backend(state.backend.clone());
    let tag = tags.create("Lot", None).unwrap();

    let (status, _) = post(
        &app,
        &format!("/api/v1/tags/{tag}/items/batch"),
        json!({"item_type": "Album", "item_ids": [1, 2, 3]}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "« Album » avec une majuscule doit être refusé, il rend {status}"
    );

    let (_, items) = get(&app, &format!("/api/v1/tags/{tag}/items")).await;
    assert_eq!(
        items["items"],
        json!([]),
        "un lot refusé ne doit rien laisser derrière lui : {items}"
    );
}

/// Le refus doit **nommer les types admis**. Un 400 nu laisse le client
/// deviner — et c'est en devinant qu'on a écrit « albums ».
#[tokio::test]
async fn le_refus_nomme_les_types_admis() {
    let (app, state) = app_avec_etat();
    let tags = TagRepo::with_backend(state.backend.clone());
    let tag = tags.create("Message", None).unwrap();

    let resp = app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/tags/{tag}/items"))
                .header("Content-Type", "application/json")
                .body(Body::from(
                    json!({"item_type": "albums", "item_id": 1}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let message = String::from_utf8_lossy(&bytes);

    for attendu in ["album", "artist", "playlist", "track"] {
        assert!(
            message.contains(attendu),
            "le message de refus ne cite pas « {attendu} » : {message}"
        );
    }
}

/// Les quatre types à identifiant passent — le garde-fou ne doit pas fermer la
/// porte à ce qui marchait.
#[tokio::test]
async fn les_quatre_types_a_identifiant_sont_acceptes() {
    let (app, state) = app_avec_etat();
    let tags = TagRepo::with_backend(state.backend.clone());
    let tag = tags.create("Tous", None).unwrap();

    for (n, t) in ["album", "artist", "playlist", "track"].iter().enumerate() {
        let (status, _) = post(
            &app,
            &format!("/api/v1/tags/{tag}/items"),
            json!({"item_type": t, "item_id": n + 1}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "« {t} » doit être accepté");
    }

    let (_, items) = get(&app, &format!("/api/v1/tags/{tag}/items")).await;
    assert_eq!(items["items"].as_array().unwrap().len(), 4);
}

/// `label` est refusé — non par oubli, mais parce qu'un label n'a pas
/// d'identifiant numérique et que `item_tags.item_id` est un entier. Ce test
/// est là pour que la décision soit VISIBLE : le jour où les labels reçoivent
/// une identité, il rougira et forcera à rouvrir la question.
#[tokio::test]
async fn label_reste_refuse_faute_d_identifiant_numerique() {
    let (app, state) = app_avec_etat();
    let tags = TagRepo::with_backend(state.backend.clone());
    let tag = tags.create("Labels", None).unwrap();

    let (status, _) = post(
        &app,
        &format!("/api/v1/tags/{tag}/items"),
        json!({"item_type": "label", "item_id": 1}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "tant qu'un label n'est qu'une chaîne, item_id INTEGER ne peut pas le porter"
    );
}

/// La suppression ne doit PAS être verrouillée par la même liste : une ligne
/// écrite avant ce garde-fou doit rester déracinable.
#[tokio::test]
async fn la_suppression_atteint_encore_une_ligne_de_type_hors_liste() {
    let (app, state) = app_avec_etat();
    let tags = TagRepo::with_backend(state.backend.clone());
    let tag = tags.create("Heritage", None).unwrap();

    // Écrite comme avant le garde-fou, par l'INSERT.
    state
        .backend
        .execute(
            &format!(
                "INSERT INTO item_tags (tag_id, item_type, item_id) VALUES ({tag}, 'albums', 7)"
            ),
            &[],
        )
        .unwrap();

    let resp = app
        .clone()
        .oneshot(
            Request::delete(format!("/api/v1/tags/{tag}/items/albums/7"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let (_, items) = get(&app, &format!("/api/v1/tags/{tag}/items")).await;
    assert_eq!(
        items["items"],
        json!([]),
        "une ligne héritée doit rester supprimable : {items}"
    );
}
