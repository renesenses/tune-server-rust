//! `POST /api/v1/podcasts/subscriptions` : l'identifiant, et la différence
//! entre créer et retrouver (#3542).
//!
//! Le défaut : la route rendait **201 dans les deux cas** — création réelle et
//! abonnement déjà présent — avec un corps qui ne portait pas l'identifiant de
//! la ligne. Un client ne pouvait donc ni enchaîner (il lui fallait relire la
//! liste entière et rapprocher par URL de flux) ni dire honnêtement à
//! l'utilisateur s'il venait d'ajouter quelque chose. Le client web s'en est
//! gardé le 07/09 en cessant de se fier à la réponse ; la route, elle, était
//! restée intacte.
//!
//! Ces essais tiennent le SITE D'APPEL — la route, par sa vraie URL, à travers
//! le routeur complet — et non une fonction pure qui la doublerait :
//!
//!   1. le premier abonnement rend **201**, `created: true`, et un `id` qui est
//!      celui de la ligne réellement écrite ;
//!   2. le même flux renvoyé une seconde fois rend **200**, `created: false`,
//!      et **le même** `id` ;
//!   3. rien n'a été dupliqué en base entre les deux.
//!
//! Le troisième point est ce qui empêche de « réussir » la garde en rendant 200
//! parce qu'on aurait cassé l'insertion.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

const FLUX: &str = "https://feeds.example.test/le-podcast.xml";

fn app() -> axum::Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    tune_server::routes::router(state)
}

async fn envoyer(app: &axum::Router, requete: Request<Body>) -> (StatusCode, Value) {
    let reponse = app.clone().oneshot(requete).await.unwrap();
    let status = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let corps: Value = serde_json::from_slice(&octets).unwrap_or(json!(null));
    (status, corps)
}

async fn abonner(app: &axum::Router, titre: &str) -> (StatusCode, Value) {
    envoyer(
        app,
        Request::post("/api/v1/podcasts/subscriptions")
            .header("Content-Type", "application/json")
            .body(Body::from(
                json!({"feed_url": FLUX, "title": titre}).to_string(),
            ))
            .unwrap(),
    )
    .await
}

async fn lister(app: &axum::Router) -> Vec<Value> {
    let (status, corps) = envoyer(
        app,
        Request::get("/api/v1/podcasts/subscriptions")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "la liste doit répondre : {corps}");
    corps.as_array().cloned().unwrap_or_default()
}

#[tokio::test]
async fn le_premier_abonnement_rend_201_et_l_identifiant_de_la_ligne_ecrite() {
    let app = app();

    let (status, corps) = abonner(&app, "Le Podcast").await;

    assert_eq!(
        status,
        StatusCode::CREATED,
        "une création rend 201 : {corps}"
    );
    assert_eq!(
        corps["created"], true,
        "le corps doit DIRE que c'est une création : {corps}"
    );
    let id = corps["id"]
        .as_i64()
        .unwrap_or_else(|| panic!("l'identifiant manque dans la réponse : {corps}"));

    // L'identifiant rendu est celui de la vraie ligne, pas un numéro inventé.
    let liste = lister(&app).await;
    assert_eq!(liste.len(), 1, "une seule ligne attendue : {liste:?}");
    assert_eq!(
        liste[0]["id"].as_i64(),
        Some(id),
        "l'identifiant rendu par la route doit être celui que la liste montre"
    );
    assert_eq!(liste[0]["feed_url"], FLUX);
}

#[tokio::test]
async fn un_abonnement_deja_present_rend_200_le_meme_identifiant_et_ne_duplique_rien() {
    let app = app();

    let (premier_status, premier) = abonner(&app, "Le Podcast").await;
    assert_eq!(premier_status, StatusCode::CREATED, "{premier}");
    let id = premier["id"]
        .as_i64()
        .expect("identifiant du premier appel");

    // Second clic sur le même podcast — exactement le geste que la route ne
    // savait pas distinguer.
    let (second_status, second) = abonner(&app, "Le Podcast").await;

    assert_eq!(
        second_status,
        StatusCode::OK,
        "un abonnement déjà présent n'est pas une création : {second}"
    );
    assert_eq!(
        second["created"], false,
        "le corps doit DIRE qu'il n'y a rien eu à créer : {second}"
    );
    assert_eq!(
        second["id"].as_i64(),
        Some(id),
        "c'est la MÊME ligne : le même identifiant doit revenir"
    );

    // Contre-poids de la garde : si 200 venait d'une insertion cassée plutôt
    // que d'un conflit, la base serait vide ou double. Elle porte une ligne.
    let liste = lister(&app).await;
    assert_eq!(
        liste.len(),
        1,
        "le second appel ne doit RIEN écrire de plus : {liste:?}"
    );
    assert_eq!(liste[0]["id"].as_i64(), Some(id));
}
