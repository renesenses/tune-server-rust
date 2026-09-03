//! #1360 — « Smart AI » promettait une IA que ce moteur n'a jamais eue.
//!
//! Fabien, forum : « Quelle est la différence entre les playlists Smart AI et
//! le bouton flottant Tune AI ? » La question n'avait pas de bonne réponse :
//! le bouton flottant (`POST /ai/query`) est un vrai assistant adossé à
//! Claude, tandis que `routes/smart_ai.rs` repère des mots-clés et construit
//! des conditions SQL. Aucune référence à `anthropic`, `AnthropicClient` ni
//! `api_key` dans ce module.
//!
//! Le libellé fautif ne vivait pas seulement dans les écrans : le serveur
//! **renvoyait lui-même** la promesse, dans le champ `name` de
//! `POST /smart-ai/generate` — `format!("AI: {}", prompt)`. `docs/contrat-web.json`
//! déclare `name` comme champ consommé par le client ; c'était donc bien une
//! chaîne d'affichage sortant de l'API, pas un détail interne.
//!
//! ## Ce que ce test garde, et ce qu'il change
//!
//! Le piège de ce genre de renommage est de corriger un chemin et de laisser
//! les autres nus. Ici les deux moitiés sont de nature différente :
//!
//! - `name` est un **LIBELLÉ** → il change (`ne_promet_plus_d_ia`).
//! - `/smart-ai/*` est un **IDENTIFIANT** comparé littéralement par trois
//!   bases de code clientes et figé dans un contrat publié → il ne change
//!   pas, et un témoin le prouve (`les_routes_historiques_repondent_encore`).
//!
//! Renommer la route aurait rendu muets les cinq écrans existants sans qu'un
//! seul utilisateur y gagne : personne ne lit une URL d'API.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

// --- socle -------------------------------------------------------------

fn appli() -> axum::Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("état en mémoire");
    tune_server::routes::router(state)
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
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

// --- rouge avant, vert après -------------------------------------------

/// Le champ `name` de `/smart-ai/generate` portait « AI: … ». Il rend
/// désormais l'invite telle quelle — la même chaîne que l'écran web affiche
/// déjà (`SmartAIView.svelte`, `playlistName = prompt`), pour qu'une même
/// génération ne porte pas deux noms selon le client.
#[tokio::test]
async fn ne_promet_plus_d_ia() {
    let app = appli();
    let invite = "relaxing jazz for the evening";

    let (status, body) = post(
        &app,
        "/api/v1/smart-ai/generate",
        json!({ "prompt": invite, "limit": 5 }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "corps: {body}");
    let name = body["name"].as_str().expect("champ `name` present");
    assert_eq!(
        name, invite,
        "le libellé doit être l'invite de l'utilisateur, sans préfixe"
    );
    assert!(
        !name.contains("AI"),
        "`name` promet encore une IA que ce moteur n'a pas : {name:?}"
    );
}

/// Les quatre autres générateurs ne doivent pas non plus invoquer l'IA dans
/// leur libellé. Ils ne le faisaient pas — ce test empêche qu'on la
/// réintroduise par symétrie mal comprise avec `generate`.
#[tokio::test]
async fn aucun_generateur_n_invoque_l_ia_dans_son_libelle() {
    let app = appli();
    let cas = [
        (
            "/api/v1/smart-ai/mood",
            json!({ "mood": "calm", "limit": 3 }),
        ),
        ("/api/v1/smart-ai/history-based", json!({ "limit": 3 })),
        ("/api/v1/smart-ai/discovery", json!({ "limit": 3 })),
        (
            "/api/v1/smart-ai/tempo-match",
            json!({ "target_bpm": 120.0, "limit": 3 }),
        ),
        ("/api/v1/smart-ai/similar-to", json!({ "track_id": 1 })),
    ];

    for (route, corps) in cas {
        let (status, body) = post(&app, route, corps).await;
        assert_eq!(status, StatusCode::OK, "{route} — corps: {body}");
        let name = body["name"].as_str().unwrap_or_default();
        let hurlant = name.to_uppercase();
        assert!(
            !hurlant.contains("AI") && !hurlant.contains("A.I."),
            "{route} rend un libellé qui promet une IA : {name:?}"
        );
    }
}

// --- témoin anti-régression --------------------------------------------

/// **Le témoin qui compte.** `/smart-ai` n'est pas un libellé : c'est le
/// chemin codé en dur dans `tune-web-client/src/lib/api.ts` (les cinq
/// appels), `tune-server-flutter/lib/services/tune_api_client.dart:1425` et
/// `tune-server-ipados/…/TuneAPIClient+SmartAutoPlay.swift:14`, et il est
/// publié dans `docs/contrat-web.json`.
///
/// Un client déjà installé — donc un réglage déjà pris — continue de taper
/// ces cinq URL. Si un renommage « de surface » les déplaçait, les écrans
/// tomberaient en 404 **sans message** : l'utilisateur verrait une liste vide
/// et aucune explication. Ce test échoue avant que ça n'arrive.
#[tokio::test]
async fn les_routes_historiques_repondent_encore() {
    let app = appli();
    let contrat = [
        ("/api/v1/smart-ai/generate", json!({ "prompt": "jazz" })),
        ("/api/v1/smart-ai/mood", json!({ "mood": "calm" })),
        ("/api/v1/smart-ai/similar-to", json!({ "track_id": 1 })),
        ("/api/v1/smart-ai/history-based", json!({})),
        (
            "/api/v1/smart-ai/tempo-match",
            json!({ "target_bpm": 120.0 }),
        ),
        ("/api/v1/smart-ai/discovery", json!({})),
    ];

    for (route, corps) in contrat {
        let (status, body) = post(&app, route, corps).await;
        assert_ne!(
            status,
            StatusCode::NOT_FOUND,
            "{route} a disparu : les clients installés tomberaient en 404 muet"
        );
        assert_eq!(status, StatusCode::OK, "{route} — corps: {body}");
        assert!(
            body.get("tracks").is_some(),
            "{route} ne rend plus `tracks`, champ obligatoire du contrat web"
        );
    }
}
