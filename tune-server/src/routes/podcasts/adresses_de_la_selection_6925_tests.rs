//! #6925 — un abonnement à « Banzzaï » pris depuis la sélection éditoriale
//! gardait l'adresse fausse `rss_11408.xml` (le flux de « Les polars de
//! l'histoire », réduit au message d'accueil de l'appli Radio France). La
//! liste des abonnements doit rendre la vraie adresse, sans doublon.
//!
//! Aucun réseau : seule la base est lue et écrite.
use crate::state::AppState;
use axum::{body::Body, http::Request};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;

const FAUSSE: &str = "https://radiofrance-podcast.net/podcast09/rss_11408.xml";
const VRAIE: &str =
    "https://radiofrance-podcast.net/podcast09/podcast_62a715a5-7397-450a-ad80-0f84bbc6f3ad.xml";

fn abonner(state: &AppState, feed_url: &str, title: &str) {
    state
        .backend
        .execute(
            "INSERT INTO podcast_subscriptions (feed_url, title, source_id) VALUES (?, ?, ?)",
            &[
                &feed_url as &dyn ToSqlValue,
                &title as &dyn ToSqlValue,
                &"rf-banzzai" as &dyn ToSqlValue,
            ],
        )
        .unwrap();
}

async fn abonnements(state: &AppState) -> Vec<Value> {
    let rep = crate::routes::router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/v1/podcasts/subscriptions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(rep.status().is_success(), "statut {}", rep.status());
    let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice::<Value>(&octets)
        .unwrap()
        .as_array()
        .cloned()
        .unwrap()
}

fn adresses(liste: &[Value]) -> Vec<String> {
    liste
        .iter()
        .filter_map(|a| a["feed_url"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn un_abonnement_a_banzzai_retrouve_son_vrai_flux_6925() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    abonner(&state, FAUSSE, "Banzzaï");
    let liste = abonnements(&state).await;
    assert_eq!(
        adresses(&liste),
        vec![VRAIE.to_string()],
        "l'abonnement doit ouvrir le flux de Banzzaï, pas celui des polars (#6925)"
    );
    assert_eq!(liste[0]["title"], "Banzzaï");
    // Une seconde lecture ne change plus rien.
    assert_eq!(
        adresses(&abonnements(&state).await),
        vec![VRAIE.to_string()]
    );
}

#[tokio::test]
async fn la_vraie_adresse_deja_suivie_efface_le_doublon_faux_6925() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    abonner(&state, FAUSSE, "Banzzaï");
    abonner(&state, VRAIE, "Banzzaï");
    assert_eq!(
        adresses(&abonnements(&state).await),
        vec![VRAIE.to_string()],
        "un seul abonnement, sur la vraie adresse (#6925)"
    );
}

#[tokio::test]
async fn une_autre_adresse_n_est_pas_touchee_6925() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    abonner(&state, "https://example.org/flux.xml", "Autre");
    assert_eq!(
        adresses(&abonnements(&state).await),
        vec!["https://example.org/flux.xml".to_string()]
    );
}
