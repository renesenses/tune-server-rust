//! La borne d'une collection intelligente : un seul nom, et une valeur que le
//! serveur sait honorer (#2732).
//!
//! Le ticket demandait trois choses. Deux sont acquises et livrées : le serveur
//! persiste et rend la borne sous le SEUL nom `max_limit` (`71e8e004`, ancêtre
//! de la v0.9.127), et le client la relit sous ce nom
//! (`SmartCollectionEditor.svelte`, `tune-web-client@main`). Restaient deux
//! trous côté serveur, tous deux sur la même valeur :
//!
//! 1. **rien ne gardait le NOM.** Le banc de contrats n'exige que la PRÉSENCE
//!    des champs cartographiés (`exige_champs`) : réintroduire `max_albums`
//!    dans la charge utile laisserait toutes les portes vertes, et le
//!    formulaire se retrouverait de nouveau devant deux noms pour une seule
//!    borne — le défaut d'origine ;
//! 2. **rien ne gardait la VALEUR.** `max_limit` partait tel quel dans le SQL
//!    (`build_album_query` : `format!("LIMIT {n}")`). `0` rendait la collection
//!    VIDE sans message ; un négatif faisait diverger les moteurs — « pas de
//!    limite » sur SQLite, requête refusée et 500 sur PostgreSQL.
//!
//! Les épreuves passent par les ROUTES MONTÉES du serveur
//! (`/api/v1/library/smart-collections`), pas par `build_album_query` : c'est
//! le point d'entrée qui laissait passer la valeur.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

const BASE: &str = "/api/v1/library/smart-collections";

/// Trois albums, et une collection dont les règles vides les prennent tous.
fn app_et_trois_albums() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("état serveur isolé");
    state
        .backend
        .execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'Miles Davis'); \
             INSERT INTO albums (id, title, artist_id) VALUES (1, 'Aaa', 1); \
             INSERT INTO albums (id, title, artist_id) VALUES (2, 'Bbb', 1); \
             INSERT INTO albums (id, title, artist_id) VALUES (3, 'Ccc', 1);",
        )
        .expect("albums témoins");
    let app = tune_server::routes::router(state.clone());
    (app, state)
}

async fn envoyer(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let requete = match corps {
        Some(c) => Request::builder()
            .method(methode)
            .uri(chemin)
            .header("Content-Type", "application/json")
            .body(Body::from(c.to_string()))
            .unwrap(),
        None => Request::builder()
            .method(methode)
            .uri(chemin)
            .body(Body::empty())
            .unwrap(),
    };
    let reponse = app.clone().oneshot(requete).await.expect("routeur");
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .expect("corps");
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn creer(borne: Option<i64>) -> Value {
    json!({
        "name": "Borne 2732",
        "rules": [],
        "match_mode": "all",
        "sort_by": "title",
        "sort_order": "asc",
        "max_limit": borne,
    })
}

/// 🔴 CONTRE-ÉPREUVE #2732 — une borne à zéro est REFUSÉE, au lieu d'être
/// enregistrée et de vider la collection en silence.
#[tokio::test]
async fn une_borne_a_zero_est_refusee_au_lieu_de_vider_la_collection() {
    let (app, _state) = app_et_trois_albums();

    let (statut, corps) = envoyer(&app, "POST", BASE, Some(creer(Some(0)))).await;
    assert_eq!(
        statut,
        StatusCode::BAD_REQUEST,
        "une borne « aucun album » doit être refusée, pas enregistrée ; corps={corps}"
    );

    // Et la prévisualisation refuse la même valeur : sinon l'écran montrerait
    // un aperçu que l'enregistrement ne saurait pas reproduire.
    let (statut, _) = envoyer(
        &app,
        "POST",
        &format!("{BASE}/preview"),
        Some(json!({"rules": [], "max_limit": 0})),
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
}

/// 🔴 CONTRE-ÉPREUVE #2732 — une borne négative est REFUSÉE.
///
/// SQLite lit un `LIMIT` négatif comme « pas de limite » : la borne enregistrée
/// ne bornait alors RIEN, tout en s'affichant dans le formulaire. PostgreSQL,
/// lui, refuse la requête — même donnée, deux comportements (#1752).
#[tokio::test]
async fn une_borne_negative_est_refusee_au_lieu_de_ne_rien_borner() {
    let (app, _state) = app_et_trois_albums();

    let (statut, corps) = envoyer(&app, "POST", BASE, Some(creer(Some(-1)))).await;
    assert_eq!(
        statut,
        StatusCode::BAD_REQUEST,
        "une borne négative doit être refusée ; corps={corps}"
    );

    // La mise à jour la refuse AVANT d'écrire quoi que ce soit.
    let (statut, collection) = envoyer(&app, "POST", BASE, Some(creer(Some(2)))).await;
    assert_eq!(statut, StatusCode::CREATED, "collection témoin");
    let id = collection["id"].as_i64().expect("id");

    let (statut, _) = envoyer(
        &app,
        "PUT",
        &format!("{BASE}/{id}"),
        Some(json!({"name": "Renommee", "max_limit": -5})),
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);

    let (_, relue) = envoyer(&app, "GET", &format!("{BASE}/{id}"), None).await;
    assert_eq!(
        relue["max_limit"], 2,
        "la borne valide d'origine survit au refus"
    );
    assert_eq!(
        relue["name"], "Borne 2732",
        "le refus précède TOUTE écriture : le nom n'a pas changé non plus"
    );
}

/// 🔴 CONTRE-ÉPREUVE #2732 — une borne déjà enregistrée à zéro ne vide plus la
/// collection.
///
/// La moitié LECTURE de la garde. Les lignes écrites avant elle existent ;
/// `LIMIT 0` les rendait toutes vides, sans message ni journal. Elles se lisent
/// désormais comme « pas de borne » — le seul repli qui ne perde aucun album.
#[tokio::test]
async fn une_borne_deja_enregistree_a_zero_ne_vide_plus_la_collection() {
    let (app, state) = app_et_trois_albums();

    let (statut, collection) = envoyer(&app, "POST", BASE, Some(creer(None))).await;
    assert_eq!(statut, StatusCode::CREATED);
    let id = collection["id"].as_i64().expect("id");

    // Ce qu'une version antérieure du serveur a pu écrire.
    state
        .backend
        .execute_batch(&format!(
            "UPDATE smart_collections SET max_limit = 0 WHERE id = {id}"
        ))
        .expect("borne héritée");

    let (statut, albums) = envoyer(&app, "GET", &format!("{BASE}/{id}/albums"), None).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(
        albums.as_array().map(Vec::len),
        Some(3),
        "une borne héritée inapplicable ne doit pas faire disparaître la collection ; \
         rendu={albums}"
    );
}

/// 🟢 TÉMOIN #2732 — une borne positive borne TOUJOURS.
///
/// L'autre moitié de la contre-épreuve : refuser les valeurs inapplicables ne
/// doit pas revenir à ne plus borner du tout. Ce témoin est vert avant comme
/// après le correctif.
#[tokio::test]
async fn temoin_une_borne_positive_borne_toujours() {
    let (app, _state) = app_et_trois_albums();

    let (statut, collection) = envoyer(&app, "POST", BASE, Some(creer(Some(2)))).await;
    assert_eq!(statut, StatusCode::CREATED);
    let id = collection["id"].as_i64().expect("id");
    assert_eq!(collection["max_limit"], 2);

    let (statut, albums) = envoyer(&app, "GET", &format!("{BASE}/{id}/albums"), None).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(
        albums.as_array().map(Vec::len),
        Some(2),
        "la borne enregistrée doit encore couper la liste ; rendu={albums}"
    );

    let (statut, apercu) = envoyer(
        &app,
        "POST",
        &format!("{BASE}/preview"),
        Some(json!({"rules": [], "max_limit": 1})),
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(apercu["total"], 1, "l'aperçu borné aussi");
}

/// 🔴 CONTRE-ÉPREUVE #2732 — UN SEUL nom de borne, de la réponse au formulaire.
///
/// C'est le premier critère du ticket, et rien ne le gardait : le banc de
/// contrats n'exige que la PRÉSENCE des champs cartographiés, jamais l'absence
/// des autres. Réintroduire `max_albums` dans `decode_collection_row` — le
/// défaut d'origine — laissait donc toutes les portes vertes.
///
/// Les trois noms fantômes du ticket sont nommés ici un par un : `max_albums`,
/// `auto_refresh`, `updated_at`.
#[tokio::test]
async fn les_charges_utiles_ne_portent_qu_un_seul_nom_de_borne() {
    let (app, _state) = app_et_trois_albums();
    const FANTOMES: [&str; 3] = ["max_albums", "auto_refresh", "updated_at"];

    let verifier = |ou: &str, charge: &Value| {
        let objet = charge
            .as_object()
            .unwrap_or_else(|| panic!("{ou} : objet JSON attendu, reçu {charge}"));
        assert!(
            objet.contains_key("max_limit"),
            "{ou} : la borne doit être rendue sous `max_limit` ; reçu {charge}"
        );
        for fantome in FANTOMES {
            assert!(
                !objet.contains_key(fantome),
                "{ou} : le nom fantôme `{fantome}` est de retour dans la charge utile — \
                 le formulaire aurait de nouveau deux noms pour une seule borne ; reçu {charge}"
            );
        }
    };

    let (statut, creee) = envoyer(&app, "POST", BASE, Some(creer(Some(7)))).await;
    assert_eq!(statut, StatusCode::CREATED);
    verifier("POST /library/smart-collections", &creee);
    let id = creee["id"].as_i64().expect("id");

    let (_, lue) = envoyer(&app, "GET", &format!("{BASE}/{id}"), None).await;
    verifier("GET /library/smart-collections/{id}", &lue);

    let (statut, modifiee) = envoyer(
        &app,
        "PUT",
        &format!("{BASE}/{id}"),
        Some(json!({"max_limit": 3})),
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(modifiee["max_limit"], 3);
    verifier("PUT /library/smart-collections/{id}", &modifiee);

    let (_, liste) = envoyer(&app, "GET", BASE, None).await;
    let premier = liste
        .as_array()
        .and_then(|v| v.first())
        .cloned()
        .expect("la liste doit porter la collection créée");
    verifier("GET /library/smart-collections", &premier);
}
