//! Contrat de `POST /library/identify-all` et `GET /library/identify-all/status`
//! (#4805) — le pilote de lot de l'identification.
//!
//! **Hermétique : aucun appel réel à MusicBrainz.** Les chemins couverts ici
//! sont exactement ceux qui rendent une réponse *avant* la moindre requête
//! réseau — le refus Premium et le relevé au repos. Ce n'est pas une demi-
//! mesure : ce sont les deux endroits où la passe pourrait mentir à
//! l'utilisateur sans rien avoir à y gagner. La sélection du lot (décisions 2
//! et 3 de Bertrand) est couverte par les tests unitaires du module, qui
//! l'exécutent contre une vraie base en mémoire et ne touchent pas non plus
//! au réseau.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

fn app() -> axum::Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    tune_server::routes::router(state)
}

async fn appeler(app: &axum::Router, methode: &str, path: &str) -> (StatusCode, Value) {
    let requete = Request::builder()
        .method(methode)
        .uri(path)
        .header("Content-Type", "application/json")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(requete).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// 🔴 Décision 4 de Bertrand : le lot est Premium, **et le refus est
/// explicite**.
///
/// C'est la propriété qui compte, pas le code HTTP. Un serveur qui refuserait
/// en rendant `200 {"total": 0}` — une liste vide, un « rien à faire » — serait
/// le pire des cas : l'utilisateur en conclurait que sa bibliothèque est déjà
/// identifiée, alors qu'elle ne l'est pas à 99,7 %. Le corps doit donc porter
/// un motif lisible, et **nommer le geste resté gratuit**.
#[tokio::test]
async fn sans_premium_le_lot_refuse_franchement_et_nomme_la_route_gratuite() {
    let app = app();

    let (status, body) = appeler(&app, "POST", "/api/v1/library/identify-all").await;

    assert_eq!(
        status,
        StatusCode::PAYMENT_REQUIRED,
        "un refus de droit ne doit ni passer pour un succès, ni pour une panne : {body}"
    );
    assert_eq!(body["code"].as_str(), Some("premium_required"));
    assert_eq!(
        body["gratuit"]["route"].as_str(),
        Some("POST /library/albums/{id}/reidentify"),
        "le refus doit nommer l'identification à la main, restée gratuite : {body}"
    );
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|m| !m.trim().is_empty()),
        "un refus muet est pire qu'un refus : {body}"
    );
    assert!(
        body["total"].is_null(),
        "un refus ne doit surtout pas ressembler à un lot vide : {body}"
    );
}

/// Le relevé porte TOUS ses compteurs, y compris quand aucune passe n'a jamais
/// tourné. Rendre `{"status":"idle"}` seul rendrait la réponse typée fausse et
/// obligerait le client à combler les manques (#1897).
#[tokio::test]
async fn le_releve_au_repos_porte_tous_ses_compteurs() {
    let app = app();

    let (status, body) = appeler(&app, "GET", "/api/v1/library/identify-all/status").await;

    assert_eq!(
        status,
        StatusCode::OK,
        "corps vide : la route n'est probablement pas montée — {body}"
    );
    assert_eq!(body["status"].as_str(), Some("idle"));
    for cle in [
        "total",
        "traites",
        "identifies",
        "sans_correspondance",
        "pistes_identifiees",
    ] {
        assert_eq!(
            body[cle].as_i64(),
            Some(0),
            "compteur `{cle}` absent du relevé au repos : {body}"
        );
    }
    assert_eq!(
        body["en_pause"].as_bool(),
        Some(false),
        "l'écran doit pouvoir afficher « Reprendre » sans interroger une seconde route : {body}"
    );
}

/// L'identification d'UN album reste servie, et ne passe par aucun droit.
/// C'est l'autre moitié de la décision 4 : le pilote de lot ne doit pas avoir
/// rendu payant le geste qui ne l'était pas.
#[tokio::test]
async fn identifier_un_album_a_la_main_ne_demande_aucun_droit() {
    let app = app();

    let (status, body) = appeler(&app, "POST", "/api/v1/library/albums/999999/reidentify").await;

    assert_ne!(
        status,
        StatusCode::PAYMENT_REQUIRED,
        "la route par album doit rester gratuite : {body}"
    );
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"].as_str(), Some("album introuvable"));
}
