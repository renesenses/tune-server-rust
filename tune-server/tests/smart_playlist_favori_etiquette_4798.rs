//! #4798 — une playlist INTELLIGENTE se met en favori et porte des étiquettes.
//!
//! Bertrand, 23/09/2026, onglet Smart playlists : « il manque 2 des 5 CTA sur
//! les pochettes (favori et tag icons) ». Le client les avait omis à dessein
//! (web #1455) parce que le serveur ne savait pas les servir :
//!
//! - `TAGGABLE_ITEM_TYPES` ne connaissait pas `smart_playlist` — `POST
//!   /tags/{id}/items` répondait 400 ;
//! - aucune route ne rendait les playlists intelligentes d'une étiquette
//!   autrement qu'en numéro ;
//! - le favori, lui, passait déjà (`favorites.item_type` est libre, comme pour
//!   `smart_collection`) — mais rien ne le prouvait.
//!
//! ## Le piège que ce fichier ferme
//!
//! `playlists.id` et `smart_playlists.id` se RECOUVRENT : l'id 1 existe dans
//! les deux tables. Le type doit donc être distinct partout, jamais déduit —
//! c'est la leçon des collections, où l'id 1 est à la fois « favorites » et
//! « Audiophile ». Le test qui compte est celui où les deux objets ont le MÊME
//! identifiant et ne se confondent ni en favori ni en étiquette.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! par sa cible `[[test]]` dans `Cargo.toml`.

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

/// Crée une playlist intelligente par la route réelle et rend son identifiant.
async fn creer_smart_playlist(app: &axum::Router, nom: &str) -> i64 {
    let (status, body) = post(
        app,
        "/api/v1/library/smart-playlists",
        json!({"name": nom, "rules": []}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "création de « {nom} » : {body}"
    );
    body["id"].as_i64().expect("id de la playlist intelligente")
}

/// Les favoris du profil 1 pour un type — les identifiants seuls.
async fn favoris(app: &axum::Router, item_type: &str) -> Vec<i64> {
    let (status, body) = get(
        app,
        &format!("/api/v1/profiles/1/favorites?item_type={item_type}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body.as_array()
        .expect("tableau de favoris")
        .iter()
        .map(|r| r["item_id"].as_i64().unwrap())
        .collect()
}

// --- 1. Étiqueter / désétiqueter ---

/// Le refus d'hier : `smart_playlist` était un `item_type` inconnu.
#[tokio::test]
async fn etiqueter_une_playlist_intelligente_est_accepte_et_se_lit_par_son_nom() {
    let (app, state) = app_avec_etat();
    let sid = creer_smart_playlist(&app, "Découvertes du mois").await;
    let tag = TagRepo::with_backend(state.backend.clone())
        .create("Rituels", None)
        .unwrap();

    let (status, body) = post(
        &app,
        &format!("/api/v1/tags/{tag}/items"),
        json!({"item_type": "smart_playlist", "item_id": sid}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "étiqueter une playlist intelligente doit passer, le serveur dit {status} : {body}"
    );

    // Lisible par son NOM, dans la forme de `/library/smart-playlists` —
    // pas un numéro nu.
    let (status, body) = get(&app, &format!("/api/v1/tags/{tag}/smart-playlists")).await;
    assert_eq!(status, StatusCode::OK, "404 = route absente");
    assert_eq!(body["tag_id"], json!(tag));
    assert_eq!(body["count"], json!(1), "{body}");
    assert_eq!(body["smart_playlists"][0]["id"], json!(sid));
    assert_eq!(
        body["smart_playlists"][0]["name"],
        json!("Découvertes du mois")
    );
    assert_eq!(body["smart_playlists"][0]["match_mode"], json!("all"));
    assert!(body["smart_playlists"][0]["rules"].is_array(), "{body}");

    // Et la pose se relit depuis l'objet, comme pour un album.
    let (_, pour) = get(&app, &format!("/api/v1/tags/for/smart_playlist/{sid}")).await;
    assert_eq!(pour.as_array().map(|a| a.len()), Some(1), "{pour}");

    // Désétiqueter : la route générique, avec le type dans le chemin.
    let status = delete(
        &app,
        &format!("/api/v1/tags/{tag}/items/smart_playlist/{sid}"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, body) = get(&app, &format!("/api/v1/tags/{tag}/smart-playlists")).await;
    assert_eq!(body["count"], json!(0), "{body}");
}

/// Même enveloppe `{tag_id, <pluriel>, count}` que ses quatre sœurs.
#[tokio::test]
async fn la_route_des_smart_playlists_a_la_meme_enveloppe_que_ses_soeurs() {
    let (app, state) = app_avec_etat();
    let tag = TagRepo::with_backend(state.backend.clone())
        .create("Vide", None)
        .unwrap();
    let (status, body) = get(&app, &format!("/api/v1/tags/{tag}/smart-playlists")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tag_id"], json!(tag));
    assert_eq!(body["count"], json!(0));
    assert!(body["smart_playlists"].is_array(), "{body}");
}

/// Une playlist intelligente étiquetée puis supprimée est omise, sans casser
/// la lecture — la règle déjà tenue par `albums`, `tracks` et `playlists`.
#[tokio::test]
async fn une_playlist_intelligente_disparue_est_omise() {
    let (app, state) = app_avec_etat();
    let sid = creer_smart_playlist(&app, "Éphémère").await;
    let tags = TagRepo::with_backend(state.backend.clone());
    let tag = tags.create("Fantôme", None).unwrap();
    tags.tag_item(tag, "smart_playlist", sid).unwrap();

    let status = delete(&app, &format!("/api/v1/library/smart-playlists/{sid}")).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = get(&app, &format!("/api/v1/tags/{tag}/smart-playlists")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["count"], json!(0), "{body}");
}

/// #4678 reste : un `item_id` nul ou négatif est refusé en 400 pour le
/// nouveau type comme pour les autres, et rien n'est écrit.
#[tokio::test]
async fn un_item_id_nul_ou_negatif_reste_refuse_pour_une_playlist_intelligente() {
    let (app, state) = app_avec_etat();
    let tag = TagRepo::with_backend(state.backend.clone())
        .create("Garde", None)
        .unwrap();

    for faux in [0, -1] {
        let (status, _) = post(
            &app,
            &format!("/api/v1/tags/{tag}/items"),
            json!({"item_type": "smart_playlist", "item_id": faux}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "item_id = {faux} doit être refusé en 400, le serveur dit {status}"
        );
    }
    let (status, _) = post(
        &app,
        &format!("/api/v1/tags/{tag}/items/batch"),
        json!({"item_type": "smart_playlist", "item_ids": [3, 0]}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "le lot doit être refusé entier"
    );

    let (_, items) = get(&app, &format!("/api/v1/tags/{tag}/items")).await;
    assert_eq!(
        items["items"],
        json!([]),
        "un refus a quand même écrit : {items}"
    );
}

// --- 2. Favori / défavori ---

/// Sur le modèle exact du favori de collection intelligente : une ligne
/// `favorites` typée `smart_playlist`, relue par le filtre `item_type`.
#[tokio::test]
async fn une_playlist_intelligente_se_met_en_favori_et_s_en_retire() {
    let (app, _) = app_avec_etat();
    let sid = creer_smart_playlist(&app, "Soirée").await;

    let (status, body) = post(
        &app,
        "/api/v1/profiles/1/favorites/add",
        json!({"item_type": "smart_playlist", "item_id": sid}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(favoris(&app, "smart_playlist").await, vec![sid]);

    // La liste SANS filtre porte la ligne avec son type — c'est ce que le
    // client web lit pour remplir ses magasins.
    let (_, tous) = get(&app, "/api/v1/profiles/1/favorites").await;
    assert!(
        tous.as_array()
            .unwrap()
            .iter()
            .any(|r| r["item_type"] == "smart_playlist" && r["item_id"] == sid),
        "{tous}"
    );

    // `check` sait répondre pour ce type.
    let (_, check) = post(
        &app,
        "/api/v1/profiles/1/favorites/check",
        json!({"item_type": "smart_playlist", "item_ids": [sid]}),
    )
    .await;
    assert_eq!(check[0]["is_favorite"], json!(true), "{check}");

    let (status, _) = post(
        &app,
        "/api/v1/profiles/1/favorites/remove",
        json!({"item_type": "smart_playlist", "item_id": sid}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(favoris(&app, "smart_playlist").await.is_empty());
}

// --- 3. Le test qui compte : même identifiant, deux objets ---

/// Une playlist et une playlist intelligente de MÊME id ne se confondent ni
/// en favori ni en étiquette. Les deux tables sont semées jusqu'à obtenir le
/// même entier des deux côtés — la situation réelle de l'id 1 sur le .18.
#[tokio::test]
async fn une_playlist_et_une_playlist_intelligente_de_meme_id_ne_se_confondent_pas() {
    let (app, state) = app_avec_etat();
    let sid = creer_smart_playlist(&app, "Intelligente").await;
    let playlists = PlaylistRepo::with_backend(state.backend.clone());
    // `smart_playlists` est semée par les migrations : l'intelligente ne
    // porte pas l'id 1. On crée des playlists jusqu'à ce que l'une d'elles
    // reçoive le MÊME entier — c'est elle, la jumelle, et c'est son nom que la
    // route des playlists doit rendre.
    let mut nom_jumelle = String::from("Manuelle");
    let mut pid = playlists.create(&nom_jumelle, None, 1).unwrap();
    let mut essais = 0;
    while pid < sid && essais < 64 {
        nom_jumelle = format!("Manuelle {essais}");
        pid = playlists.create(&nom_jumelle, None, 1).unwrap();
        essais += 1;
    }
    assert_eq!(
        pid, sid,
        "le banc doit produire le MÊME identifiant dans les deux tables"
    );

    // Favori : l'intelligente seule.
    let (status, _) = post(
        &app,
        "/api/v1/profiles/1/favorites/add",
        json!({"item_type": "smart_playlist", "item_id": sid}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(favoris(&app, "smart_playlist").await, vec![sid]);
    assert!(
        favoris(&app, "playlist").await.is_empty(),
        "mettre la playlist intelligente {sid} en favori a allumé la playlist {pid}"
    );
    let (_, check) = post(
        &app,
        "/api/v1/profiles/1/favorites/check",
        json!({"item_type": "playlist", "item_ids": [pid]}),
    )
    .await;
    assert_eq!(check[0]["is_favorite"], json!(false), "{check}");

    // Retirer le favori de la PLAYLIST (qui n'en a pas) ne touche pas l'autre.
    post(
        &app,
        "/api/v1/profiles/1/favorites/remove",
        json!({"item_type": "playlist", "item_id": pid}),
    )
    .await;
    assert_eq!(favoris(&app, "smart_playlist").await, vec![sid]);

    // Étiquette : les deux, sous le même entier — deux lignes, deux routes.
    let tag = TagRepo::with_backend(state.backend.clone())
        .create("Jumeaux", None)
        .unwrap();
    for item_type in ["playlist", "smart_playlist"] {
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

    let (_, pl) = get(&app, &format!("/api/v1/tags/{tag}/playlists")).await;
    assert_eq!(pl["count"], json!(1), "{pl}");
    assert_eq!(pl["playlists"][0]["name"], json!(nom_jumelle));
    let (_, sp) = get(&app, &format!("/api/v1/tags/{tag}/smart-playlists")).await;
    assert_eq!(sp["count"], json!(1), "{sp}");
    assert_eq!(sp["smart_playlists"][0]["name"], json!("Intelligente"));

    // Désétiqueter la playlist laisse l'intelligente étiquetée.
    let status = delete(&app, &format!("/api/v1/tags/{tag}/items/playlist/{pid}")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, pl) = get(&app, &format!("/api/v1/tags/{tag}/playlists")).await;
    assert_eq!(pl["count"], json!(0), "{pl}");
    let (_, sp) = get(&app, &format!("/api/v1/tags/{tag}/smart-playlists")).await;
    assert_eq!(
        sp["count"],
        json!(1),
        "désétiqueter la playlist {pid} a emporté la playlist intelligente {sid} : {sp}"
    );
}
