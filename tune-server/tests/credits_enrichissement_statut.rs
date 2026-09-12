//! Contrat de `POST /library/enrich-credits` et de son statut (#2799).
//!
//! **Hermétique : aucun appel réseau.** La bibliothèque de test ne contient
//! aucune piste portant un `musicbrainz_recording_id`, ou seulement des pistes
//! déjà créditées avec `only_missing` : la passe de fond n'a alors AUCUN
//! candidat et ne sort jamais. Ce qui est vérifié ici, c'est le contrat de la
//! route et la persistance du statut — la sélection des candidats est
//! verrouillée par les tests unitaires de `requete_candidats`, hors réseau eux
//! aussi.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;

fn app_et_etat() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn get_json(app: &axum::Router, chemin: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
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

async fn post_enrich(app: &axum::Router, corps: Option<&str>) -> (StatusCode, Value) {
    let req = match corps {
        Some(b) => Request::post("/api/v1/library/enrich-credits")
            .header("Content-Type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => Request::post("/api/v1/library/enrich-credits")
            .body(Body::empty())
            .unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Bibliothèque SANS aucun `musicbrainz_recording_id` : rien à demander à
/// MusicBrainz, donc rien ne sort de la machine.
fn peupler_sans_mbid(state: &tune_server::state::AppState) {
    state
        .backend
        .execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'Bill Evans'); \
             INSERT INTO albums (id, title, artist_id) VALUES (1, 'Waltz for Debby', 1); \
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, source) \
               VALUES (10, 'My Foolish Heart', 1, 1, '/m-n2799/a/01.flac', 'local');",
        )
        .unwrap();
}

/// 🔴 LE MANQUE DE L'ISSUE : la route de statut n'existait pas. Sans correctif,
/// `GET /library/enrich-credits/status` rend `404 Not Found`.
#[tokio::test]
async fn la_route_de_statut_existe_et_rend_les_compteurs_au_repos() {
    let (app, _state) = app_et_etat();
    let (status, body) = get_json(&app, "/api/v1/library/enrich-credits/status").await;
    assert_eq!(status, StatusCode::OK, "corps = {body}");
    assert_eq!(body["status"], "idle");
    // Les compteurs sont rendus dans TOUS les états, y compris au repos : une
    // réponse partielle force chaque appelant à rattraper les champs manquants.
    for champ in ["enriched", "errors", "skipped", "total"] {
        assert_eq!(body[champ], 0, "champ {champ} absent ou faux : {body}");
    }
}

/// L'avancement est PERSISTÉ : après un 202, le statut est déjà `running` en
/// base, avant même le premier aller-retour MusicBrainz. C'est ce qui permet à
/// l'écran de retrouver la passe après une navigation.
#[tokio::test]
async fn le_statut_est_persiste_des_le_202() {
    let (app, state) = app_et_etat();
    peupler_sans_mbid(&state);

    let (status, body) = post_enrich(&app, None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "corps = {body}");
    let task_id = body["task_id"].as_str().unwrap_or_default().to_string();
    assert!(!task_id.is_empty(), "task_id absent : {body}");

    // Écrit AVANT le spawn : la lecture ne dépend d'aucun ordonnancement.
    let brut = SettingsRepo::with_backend(state.backend.clone())
        .get("enrich_credits_status")
        .unwrap()
        .expect("le statut doit etre en base des le 202");
    let persiste: Value = serde_json::from_str(&brut).unwrap();
    assert_eq!(persiste["task_id"], task_id.as_str());
    assert_eq!(persiste["status"], "running");

    // Et la route de statut rend bien CE task_id, pas un statut inventé.
    let (_, vu) = get_json(&app, "/api/v1/library/enrich-credits/status").await;
    assert_eq!(vu["task_id"], task_id.as_str());
}

/// `only_missing` est accepté et ANNONCÉ dans la réponse : l'écran peut dire
/// laquelle des deux passes il a lancée sans deviner.
#[tokio::test]
async fn only_missing_est_accepte_et_annonce() {
    let (app, state) = app_et_etat();
    peupler_sans_mbid(&state);

    let (status, body) = post_enrich(&app, Some(r#"{"only_missing": true}"#)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "corps = {body}");
    assert_eq!(body["only_missing"], true, "{body}");
}

/// TÉMOIN ANTI-RÉGRESSION : sans corps, le contrat historique est intact —
/// `202`, un `task_id`, et `only_missing` à `false`. Une route qui exigerait
/// désormais un corps casserait le client déjà livré.
#[tokio::test]
async fn temoin_sans_corps_le_contrat_historique_est_intact() {
    let (app, state) = app_et_etat();
    peupler_sans_mbid(&state);

    let (status, body) = post_enrich(&app, None).await;
    assert_eq!(status, StatusCode::ACCEPTED, "corps = {body}");
    assert!(body["task_id"].is_string(), "{body}");
    assert_eq!(body["only_missing"], false, "{body}");
}

/// TÉMOIN ANTI-RÉGRESSION : la route de statut des MÉTADONNÉES est une autre
/// route, avec sa propre clé. Les deux passes tournent en parallèle ; si elles
/// partageaient leur avancement, l'une écraserait la barre de l'autre.
#[tokio::test]
async fn temoin_le_statut_des_metadonnees_reste_distinct() {
    let (app, state) = app_et_etat();
    peupler_sans_mbid(&state);
    post_enrich(&app, None).await;

    let (status, body) = get_json(&app, "/api/v1/library/enrich-all/status").await;
    assert_eq!(status, StatusCode::OK);
    // La passe crédits vient d'écrire `running` sous SA clé ; celle des
    // métadonnées n'a jamais tourné et doit rester au repos.
    assert_eq!(body["status"], "idle", "{body}");
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    assert!(reglages.get("enrich_all_status").unwrap().is_none());
    assert!(reglages.get("enrich_credits_status").unwrap().is_some());
}
