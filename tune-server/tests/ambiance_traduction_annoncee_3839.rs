//! #3839 — l'écran Ambiance doit pouvoir dire si le serveur sait traduire.
//!
//! JeromeQ, forum fil 1751, 10/09/2026 : « Progressive rock » et « Rock
//! progressif » rendent deux listes différentes, et la seconde sort du Led
//! Zeppelin sans Genesis ni Yes. Ce n'est pas un défaut de tri : la tour texte
//! du CLAP est entraînée en ANGLAIS, et `routes/library/search.rs` ne traduit
//! une requête française que si l'utilisateur a configuré une clé IA
//! (`tune_core::ai::translate`). Sans clé, les deux libellés sont deux vecteurs
//! différents — le comportement du code, pas un bug.
//!
//! ## Pourquoi cette information ne pouvait venir que du serveur
//!
//! Mesuré sur `renesenses/tune-web-client` `origin/main` (dd3c1a8b) :
//! `git grep anthropic_api_key -- src` ne rend RIEN. Les clés API ne sortent
//! d'aucune route, et le client n'a donc aucun moyen de distinguer « requête
//! déjà en anglais » de « pas de clé, requête envoyée brute ». L'écran se
//! taisait faute de savoir, pas faute de place.
//!
//! ## Pourquoi ce test passe par la ROUTE MONTÉE
//!
//! `cle_disponible` est déjà gardée par son propre témoin, dans le fichier où
//! elle vit. Ce qui manquerait sans ce banc-ci, c'est le fil : que la valeur
//! traverse `acoustic_status` et atteigne le corps JSON. Un test qui
//! rappellerait `cle_disponible` resterait vert si l'on retirait le champ de
//! la réponse — c'est-à-dire dans le seul cas qui intéresse le client. On monte
//! donc le vrai routeur et on lit le vrai corps.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! parce que `tune-server/Cargo.toml` déclare la cible `[[test]]`
//! `ambiance_traduction_annoncee_3839`. Sans elle, il ne serait jamais bâti :
//! un faux vert. Il ne dépend d'aucune fonctionnalité optionnelle (surtout pas
//! de `local-audio`, que les portes `Test` et `Test (PostgreSQL)` n'activent
//! PAS) : il tourne donc bien dans les deux.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_server::state::AppState;

const ROUTE: &str = "/api/v1/library/search/acoustic/status";

fn etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).expect("AppState sur SQLite")
}

async fn statut(routeur: Router) -> Value {
    let reponse = routeur
        .oneshot(
            Request::builder()
                .uri(ROUTE)
                .body(Body::empty())
                .expect("requête"),
        )
        .await
        .expect("réponse");
    assert_eq!(
        reponse.status(),
        StatusCode::OK,
        "{ROUTE} doit répondre 200"
    );
    let octets = axum::body::to_bytes(reponse.into_body(), 1 << 20)
        .await
        .expect("corps");
    serde_json::from_slice(&octets).expect("corps JSON")
}

/// Le champ existe, et il dit NON quand aucune clé n'est posée — c'est le cas
/// de JeromeQ, et celui où l'écran doit conseiller l'anglais.
#[tokio::test]
async fn sans_cle_le_statut_annonce_qu_il_ne_traduit_pas() {
    let state = etat();
    let v = statut(tune_server::routes::router(state)).await;
    assert_eq!(
        v.get("translation_available"),
        Some(&Value::Bool(false)),
        "le corps doit PORTER le champ et le dire faux ; reçu : {v}"
    );
}

/// Une clé posée bascule l'annonce. Le témoin part du réglage RÉEL que
/// `tune_core::ai::translate` consulte, pas d'une donnée qu'il fabriquerait.
#[tokio::test]
async fn une_cle_posee_fait_basculer_l_annonce() {
    let state = etat();
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set("openai_api_key", "sk-temoin-3839")
        .expect("réglage écrit");
    let v = statut(tune_server::routes::router(state)).await;
    assert_eq!(
        v.get("translation_available"),
        Some(&Value::Bool(true)),
        "une clé configurée doit s'annoncer ; reçu : {v}"
    );
}
