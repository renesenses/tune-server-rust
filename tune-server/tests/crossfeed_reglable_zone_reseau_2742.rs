//! #2742, 23/09 — TÉMOIN DE DISPONIBILITÉ : sur une zone réseau dont le flux
//! progressif n'est pas armé, `GET`/`PUT /zones/{id}/dsp` doivent laisser le
//! crossfeed RÉGLABLE.
//!
//! Tades (Tune OS 0.9.151, zone 29 DLNA vers un renderer Diretta, source
//! Qobuz, `dsp_progressif_reseau: false`) : « Aucun effet et même plus la
//! possibilité de régler intensité et retard ». La route répondait
//! `unavailable: true`, motif `network_progressive_off`, et le client
//! (`indisponibiliteCrossfeed`) désactive les curseurs sur ce seul champ.
//!
//! Or les bras streaming (Qobuz, Tidal, YouTube) appliquent le crossfeed de la
//! zone sans lire l'opt-in : la diaphonie est MESURÉE par
//! `une_zone_reseau_sans_opt_in_entend_le_crossfeed_sur_un_flux_2742`
//! (`tune-core/src/orchestrator/tests.rs`). Ce fichier garde la moitié qui
//! atteint l'écran, par le routeur réel.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! par sa cible `[[test]]` dans `tune-server/Cargo.toml`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::zone_repo::ZoneRepo;

/// Le renderer de la zone réseau annonce-t-il le LPCM ?
///
/// Absent du registre, il est présumé capable — la règle de
/// `dlna_accepte_lpcm`, la même à la résolution qu'à l'écran. Enregistré sous
/// une sortie factice sans Sink `GetProtocolInfo`, il n'annonce rien : c'est
/// le renderer sans LPCM, le seul cas qui garde la réserve depuis le 24/09.
#[derive(Clone, Copy)]
enum Renderer {
    AnnonceLeLpcm,
    SansLpcm,
}

async fn app(premium: bool) -> (axum::Router, i64, i64) {
    app_avec(premium, Renderer::SansLpcm).await
}

async fn app_avec(premium: bool, renderer: Renderer) -> (axum::Router, i64, i64) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    state.license.set_account_premium(premium, None).await;
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let locale = repo
        .create("Casque", Some("local"), Some("local:Realtek HD"))
        .unwrap();
    let reseau = repo
        .create("C19", Some("dlna"), Some("uuid:diretta-renderer-2742"))
        .unwrap();
    if let Renderer::SansLpcm = renderer {
        state.orchestrator.outputs.lock().await.register(Box::new(
            tune_core::outputs::mock::MockOutput::new("uuid:diretta-renderer-2742", "C19")
                .with_type("dlna"),
        ));
    }
    (tune_server::routes::router(state), locale, reseau)
}

async fn reponse(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

async fn lire(app: &axum::Router, zone: i64) -> Value {
    let (status, corps) = reponse(
        app,
        Request::get(format!("/api/v1/zones/{zone}/dsp"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    corps
}

async fn ecrire(app: &axum::Router, zone: i64) -> (StatusCode, Value) {
    let corps = json!({ "crossfeed": { "enabled": true, "amount": 0.4, "delay_ms": 0.7 } });
    reponse(
        app,
        Request::put(format!("/api/v1/zones/{zone}/dsp"))
            .header("Content-Type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await
}

/// ⭐ Le témoin : zone DLNA, opt-in désarmé, Premium. Avant comme après le
/// clic, l'intensité et le retard doivent rester réglables.
#[tokio::test]
async fn une_zone_reseau_sans_opt_in_laisse_regler_intensite_et_retard() {
    let (app, _, reseau) = app(true).await;

    let avant = lire(&app, reseau).await;
    let st = &avant["crossfeed_status"];
    assert_eq!(
        st["unavailable"].as_bool(),
        Some(false),
        "case décochée : le contrôle doit rester ACTIVABLE — {st}"
    );

    let (status, ecrit) = ecrire(&app, reseau).await;
    assert_eq!(status, StatusCode::OK, "{ecrit}");
    let st = &ecrit["crossfeed_status"];
    assert_eq!(
        st["unavailable"].as_bool(),
        Some(false),
        "les flux des services portent le crossfeed : verrouiller intensité et \
         retard est le symptôme de Tades — {st}"
    );
    assert_eq!(st["effective"].as_bool(), Some(true), "{st}");
    // La réserve reste dite : ce renderer n'annonce pas le LPCM, les pistes
    // de la bibliothèque que rien ne retraite partent telles quelles.
    assert_eq!(
        st["reason"].as_str(),
        Some("network_progressive_off"),
        "{st}"
    );
    assert!(
        st["detail"]
            .as_str()
            .is_some_and(|d| d.contains("Qobuz") && d.contains("PCM non compressé")),
        "{st}"
    );
    // Et le réglage envoyé est celui qui est relu.
    let relu = lire(&app, reseau).await;
    assert_eq!(relu["crossfeed"]["amount"].as_f64(), Some(0.4), "{relu}");
    assert_eq!(relu["crossfeed"]["delay_ms"].as_f64(), Some(0.7), "{relu}");
    assert_eq!(
        relu["crossfeed_status"]["unavailable"].as_bool(),
        Some(false),
        "{relu}"
    );
}

/// Contre-témoin : les DROITS verrouillent toujours, sur la même zone. Le
/// correctif ne lève que le verrou de l'opt-in, pas celui de la licence.
#[tokio::test]
async fn sans_premium_la_meme_zone_reste_verrouillee_par_les_droits() {
    let (app, _, reseau) = app(false).await;
    let corps = lire(&app, reseau).await;
    let st = &corps["crossfeed_status"];
    assert_eq!(st["unavailable"].as_bool(), Some(true), "{st}");
    assert_eq!(st["reason"].as_str(), Some("premium_required"), "{st}");
}

/// Contre-témoin : la sortie locale reste nominale, sans motif.
#[tokio::test]
async fn la_sortie_locale_reste_nominale() {
    let (app, locale, _) = app(true).await;
    let (status, ecrit) = ecrire(&app, locale).await;
    assert_eq!(status, StatusCode::OK, "{ecrit}");
    let st = &ecrit["crossfeed_status"];
    assert_eq!(st["unavailable"].as_bool(), Some(false), "{st}");
    assert_eq!(st["effective"].as_bool(), Some(true), "{st}");
    assert!(st["reason"].is_null(), "{st}");
}

/// #2742 (24/09) — le même écran devant un renderer qui ANNONCE le LPCM : les
/// pistes de la bibliothèque portent le crossfeed en WAV progressif, sans
/// l'opt-in (décision de Bertrand). Plus aucune réserve.
#[tokio::test]
async fn un_renderer_qui_lit_le_lpcm_n_a_plus_de_reserve() {
    let (app, _, reseau) = app_avec(true, Renderer::AnnonceLeLpcm).await;
    let (status, ecrit) = ecrire(&app, reseau).await;
    assert_eq!(status, StatusCode::OK, "{ecrit}");
    let st = &ecrit["crossfeed_status"];
    assert_eq!(st["unavailable"].as_bool(), Some(false), "{st}");
    assert_eq!(st["effective"].as_bool(), Some(true), "{st}");
    assert!(st["reason"].is_null(), "{st}");
}
