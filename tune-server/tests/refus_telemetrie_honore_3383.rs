//! #3383 — la bascule « désactiver la télémétrie » doit réellement éteindre.
//!
//! ## Ce qui était cassé
//!
//! L'interface appelait `POST /api/v1/cloud/telemetry/disable`. La route ne
//! liait même pas son `State` (`State(_state)`), n'écrivait rien, et répondait
//! `enabled: true` — la valeur de `TelemetryReporter::is_enabled()`, c'est-à-dire
//! la seule variable d'environnement `TUNE_TELEMETRY` — juste après que
//! l'utilisateur eut demandé l'inverse. Le client basculait son booléen
//! localement, et le rafraîchissement suivant (`GET /cloud/telemetry/status`)
//! remettait la case en place tout seul.
//!
//! Ce n'est pas un défaut de confort : le produit proposait un refus et ne
//! l'appliquait pas. La charge utile du battement porte version, plateforme,
//! nombre de pistes, **nom d'hôte**, services authentifiés et liste des
//! appareils.
//!
//! ## Pourquoi ce fichier passe par les ROUTES MONTÉES
//!
//! Un témoin qui appellerait `TelemetryReporter::is_enabled_for` sur un réglage
//! qu'il aurait posé lui-même resterait VERT le jour où quelqu'un débranche la
//! route — c'est-à-dire exactement le défaut d'origine, « écrit mais pas
//! branché ». Ici, on monte `tune_server::routes::router(state)` et on envoie
//! de vraies requêtes HTTP : le refus doit traverser le handler, le
//! `SettingsRepo`, puis être relu par les gardes d'envoi réelles.
//!
//! ## Les trois propriétés gardées
//!
//! 1. le refus posé par la route ÉTEINT le battement descriptif — vérifié en
//!    appelant `background::plan_du_tour`, l'unique expression que la boucle
//!    de production évalue à chaque tour ;
//! 2. il éteint AUSSI la contribution communautaire, la seconde famille
//!    d'envois, par `consent::contribution_autorisee` ;
//! 3. la **licence continue d'être validée** — `revalidate_key` reste vrai.
//!    C'est le point que l'issue nomme « le POST est à double usage » : un
//!    opt-out qui couperait le POST entier ferait retomber un Premium à clé en
//!    gratuit à la fin de la grâce hors-ligne de 14 jours (LIC-1). Un test qui
//!    vérifierait l'absence de tout POST garderait donc la MAUVAISE propriété.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::cloud::telemetry::TelemetryReporter;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

fn banc() -> (axum::Router, AppState) {
    let state = AppState::new(":memory:", 0, Default::default()).expect("AppState sur SQLite");
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

fn reglages(state: &AppState) -> SettingsRepo {
    SettingsRepo::with_backend(state.backend.clone())
}

async fn poster(app: &axum::Router, chemin: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(chemin)
                .header("Content-Type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&octets).unwrap_or(json!(null)),
    )
}

async fn lire(app: &axum::Router, chemin: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&octets).unwrap_or(json!(null)),
    )
}

/// Le cœur de l'issue : après le clic, les gardes d'envoi voient le refus.
#[tokio::test]
async fn le_refus_pose_par_la_route_eteint_reellement_les_envois() {
    let (app, state) = banc();
    let reglages = reglages(&state);

    // Avant : rien n'a été décoché, tout part comme avant #3383.
    assert!(
        tune_server::background::plan_du_tour(&reglages).send_heartbeat,
        "une installation neuve doit continuer d'émettre — le défaut ne change pas"
    );

    let (status, corps) = poster(&app, "/api/v1/cloud/telemetry/disable").await;
    assert_eq!(status, StatusCode::OK);

    // 1. La réponse dit la vérité, tout de suite. Avant #3383 elle répondait
    //    `true` juste après le refus.
    assert_eq!(
        corps["enabled"], false,
        "la route doit répondre l'état EFFECTIF, pas la variable d'environnement"
    );

    // 2. Le battement descriptif est coupé — décidé par le code de production.
    let plan = tune_server::background::plan_du_tour(&reglages);
    assert!(
        !plan.send_heartbeat,
        "le refus doit couper la charge utile descriptive du battement"
    );

    // 3. …et la licence continue de vivre. C'est la moitié de l'issue qu'un
    //    « aucun POST ne part » aurait cassée (LIC-1, J+15).
    assert!(
        plan.revalidate_key,
        "un opt-out ne doit jamais faire perdre une licence à clé"
    );
    assert!(
        plan.refresh_account,
        "un opt-out ne doit jamais dégrader un compte premium"
    );

    // 4. La seconde famille d'envois — la contribution communautaire — est
    //    coupée par le même refus.
    reglages
        .set(tune_core::cloud::consent::CONTRIBUTION_SETTING_KEY, "true")
        .expect("écriture du consentement de contribution");
    assert!(
        !tune_core::cloud::consent::contribution_autorisee(&reglages),
        "refuser la télémétrie doit fermer la contribution, même explicitement cochée"
    );

    // 5. Et le verrou lui-même, celui que les sept gardes appellent.
    assert!(
        !TelemetryReporter::is_enabled_for(&reglages),
        "le verrou partagé doit voir le refus posé par la route"
    );
}

/// Le refus n'est pas un aller sans retour : re-cocher rallume.
#[tokio::test]
async fn re_cocher_rallume_reellement() {
    let (app, state) = banc();
    let reglages = reglages(&state);

    poster(&app, "/api/v1/cloud/telemetry/disable").await;
    assert!(!TelemetryReporter::is_enabled_for(&reglages));

    let (status, corps) = poster(&app, "/api/v1/cloud/telemetry/enable").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(corps["enabled"], true);
    assert!(
        TelemetryReporter::is_enabled_for(&reglages),
        "re-cocher doit rallumer les gardes, pas seulement la case"
    );
    assert!(
        tune_server::background::plan_du_tour(&reglages).send_heartbeat,
        "re-cocher doit faire repartir le battement"
    );
}

/// `GET /cloud/telemetry/status` doit republier le refus, sinon le
/// rafraîchissement de l'écran remet la case en place tout seul — c'est
/// littéralement ce que voyait l'utilisateur avant #3383.
#[tokio::test]
async fn le_statut_republie_le_refus_au_rafraichissement() {
    let (app, _state) = banc();

    let (_, avant) = lire(&app, "/api/v1/cloud/telemetry/status").await;
    assert_eq!(avant["enabled"], true, "défaut inchangé : actif");

    poster(&app, "/api/v1/cloud/telemetry/disable").await;

    let (status, apres) = lire(&app, "/api/v1/cloud/telemetry/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        apres["enabled"], false,
        "le rafraîchissement ne doit pas ressusciter la télémétrie"
    );
    // La question que l'issue laissait ouverte : dire à l'écran quand
    // l'exploitant lui retire la main. Faux ici, `TUNE_TELEMETRY` n'étant pas
    // posé dans la suite de tests.
    assert_eq!(
        apres["env_override"], false,
        "aucun TUNE_TELEMETRY n'est posé pendant les tests"
    );
}

/// Les trois chemins de l'issue — `/cloud/telemetry/*`, `/system/telemetry` et
/// `TUNE_TELEMETRY` — doivent décrire UN SEUL interrupteur.
///
/// `POST /system/telemetry` écrivait déjà la bonne clé, mais aucune garde ne
/// la lisait : un aller-retour fermé sur lui-même. Ce témoin exige qu'un refus
/// posé par ce chemin-là éteigne pour de bon, et que l'autre chemin le voie.
#[tokio::test]
async fn les_trois_chemins_sont_le_meme_interrupteur() {
    let (app, state) = banc();
    let reglages = reglages(&state);

    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/system/telemetry")
                .header("Content-Type", "application/json")
                .body(Body::from(json!({"enabled": false}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    assert!(
        !tune_server::background::plan_du_tour(&reglages).send_heartbeat,
        "/system/telemetry doit éteindre pour de bon, pas se relire lui-même"
    );

    let (_, statut) = lire(&app, "/api/v1/cloud/telemetry/status").await;
    assert_eq!(
        statut["enabled"], false,
        "les deux routes doivent décrire le même interrupteur"
    );
}

/// Le ping de démarrage — version, OS, arch, liste des services — partait
/// « always regardless of TUNE_TELEMETRY setting ». Il porte quatre des champs
/// descriptifs que le refus est censé éteindre ; il doit donc s'abstenir.
///
/// Aucun réseau n'est touché : la fonction rend `Refuse` avant de construire
/// son client HTTP.
#[tokio::test]
async fn le_ping_de_demarrage_respecte_le_refus() {
    let (app, state) = banc();
    poster(&app, "/api/v1/cloud/telemetry/disable").await;

    let verdict =
        tune_core::cloud::telemetry::ping_de_demarrage(&state.backend, &state.services).await;
    assert_eq!(
        verdict,
        tune_core::cloud::telemetry::PingDemarrage::Refuse,
        "le ping de démarrage doit se taire quand la télémétrie est refusée"
    );
}
