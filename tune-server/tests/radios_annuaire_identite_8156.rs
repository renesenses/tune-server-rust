//! #8156 — la relève de l'annuaire des radios s'identifie auprès du site.
//!
//! Décision de Bertrand du 08/11/2026 (temps 1, rc12) : le relevé de
//! `/api/v2/radios` porte l'identifiant d'installation (`X-Tune-Server-Id`,
//! le `server_id` que le battement de cœur fait déjà connaître au site) et la
//! version de Tune ; une installation qui a refusé la télémétrie reste
//! anonyme ; un refus 401/403 du site laisse servir le relevé d'avant, et
//! l'écran reçoit `refus` pour dire pourquoi il n'est pas rafraîchi. Aucun
//! essai ne touche au réseau : le site est un serveur monté ici.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::cloud::telemetry::{TELEMETRY_SETTING_KEY, TelemetryReporter};
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::routes::radios_annuaire::{self, ENTETE_INSTALLATION, Releve};

const EXTRAIT: &str = include_str!("fixtures/annuaire_v2_extrait_20261019.json");

/// Ce que le faux site a vu d'une requête : `X-Tune-Server-Id`, `User-Agent`.
type Vus = Arc<Mutex<Vec<(Option<String>, Option<String>)>>>;

fn etat() -> tune_server::state::AppState {
    tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap()
}

/// Un faux site : rend `statut` (200 avec l'extrait, ou un refus) et note les
/// en-têtes d'identité de chaque requête.
async fn faux_site(statut: Arc<Mutex<u16>>) -> (String, Vus) {
    let vus: Vus = Arc::default();
    let v = vus.clone();
    let app = axum::Router::new().route(
        "/api/v2/radios",
        axum::routing::get(move |headers: HeaderMap| {
            let (v, statut) = (v.clone(), statut.clone());
            async move {
                let lire = |nom: &str| {
                    headers
                        .get(nom)
                        .and_then(|x| x.to_str().ok())
                        .map(str::to_string)
                };
                v.lock()
                    .unwrap()
                    .push((lire("x-tune-server-id"), lire("user-agent")));
                let s = *statut.lock().unwrap();
                let corps = if s == 200 {
                    EXTRAIT.to_string()
                } else {
                    r#"{"error":"unauthenticated"}"#.to_string()
                };
                axum::response::Response::builder()
                    .status(s)
                    .header("content-type", "application/json")
                    .body(Body::from(corps))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://127.0.0.1:{port}/api/v2/radios"), vus)
}

async fn annuaire_vu_par_l_ecran(state: &tune_server::state::AppState) -> Value {
    let app = tune_server::routes::router(state.clone());
    let rep = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/radios/directory")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rep.status(), StatusCode::OK);
    let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&octets).unwrap()
}

#[tokio::test]
async fn le_releve_porte_l_identifiant_d_installation_et_la_version_8156() {
    if !TelemetryReporter::is_enabled() {
        // `TUNE_TELEMETRY=false` dans l'environnement du banc : l'installation
        // est réputée l'avoir refusée, le relevé est anonyme par construction.
        eprintln!("TUNE_TELEMETRY refusée par l'environnement : témoin sans objet");
        return;
    }
    let state = etat();
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    let server_id = TelemetryReporter::get_or_create_server_id(&reglages);
    let dossier = tempfile::tempdir().unwrap();
    let (url, vus) = faux_site(Arc::new(Mutex::new(200))).await;

    let r = radios_annuaire::rafraichir_depuis(&state, &url, &dossier.path().join("a.json")).await;
    assert_eq!(r, Releve::Nouveau(4));

    let vus = vus.lock().unwrap();
    assert_eq!(vus.len(), 1);
    assert_eq!(
        vus[0].0.as_deref(),
        Some(server_id.as_str()),
        "le relevé doit présenter le server_id dans {ENTETE_INSTALLATION}"
    );
    let ua = vus[0].1.as_deref().unwrap_or_default();
    assert_eq!(
        ua,
        format!("Tune/{} (https://mozaiklabs.fr)", tune_core::version()),
        "le site doit pouvoir lire la version de Tune"
    );
}

#[tokio::test]
async fn telemetrie_refusee_le_releve_reste_anonyme_8156() {
    let state = etat();
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    reglages.set(TELEMETRY_SETTING_KEY, "false").unwrap();
    let dossier = tempfile::tempdir().unwrap();
    let (url, vus) = faux_site(Arc::new(Mutex::new(200))).await;

    let r = radios_annuaire::rafraichir_depuis(&state, &url, &dossier.path().join("a.json")).await;
    assert_eq!(r, Releve::Nouveau(4));
    let vus = vus.lock().unwrap();
    assert_eq!(vus.len(), 1);
    assert_eq!(
        vus[0].0, None,
        "télémétrie refusée : aucun identifiant ne doit sortir"
    );
    assert!(
        vus[0]
            .1
            .as_deref()
            .is_some_and(|ua| ua.starts_with("Tune/")),
        "la version reste annoncée : {:?}",
        vus[0].1
    );
}

#[tokio::test]
async fn un_refus_du_site_garde_l_annuaire_et_le_dit_a_l_ecran_8156() {
    let state = etat();
    let dossier = tempfile::tempdir().unwrap();
    let cache = dossier.path().join("radios").join("annuaire-v2.json");
    let statut = Arc::new(Mutex::new(200));
    let (url, _vus) = faux_site(statut.clone()).await;

    assert_eq!(
        radios_annuaire::rafraichir_depuis(&state, &url, &cache).await,
        Releve::Nouveau(4)
    );
    let avant = annuaire_vu_par_l_ecran(&state).await;
    assert_eq!(avant["refus"], Value::Null, "{avant}");

    for refus in [401u16, 403] {
        *statut.lock().unwrap() = refus;
        let r = radios_annuaire::rafraichir_depuis(&state, &url, &cache).await;
        assert_eq!(r, Releve::Refuse(refus));
        assert!(!r.a_jour());
        assert_eq!(
            state.annuaire_radios.read().unwrap().len(),
            4,
            "un refus ne vide jamais l'annuaire"
        );
        let corps = annuaire_vu_par_l_ecran(&state).await;
        assert_eq!(corps["total"], 4, "{corps}");
        assert_eq!(corps["refus"]["status"], refus, "{corps}");
        assert_eq!(
            corps["refus"]["code"],
            radios_annuaire::CODE_REFUS,
            "{corps}"
        );
    }

    // Le site accepte de nouveau : le refus s'efface.
    *statut.lock().unwrap() = 200;
    assert_eq!(
        radios_annuaire::rafraichir_depuis(&state, &url, &cache).await,
        Releve::Nouveau(4)
    );
    let apres = annuaire_vu_par_l_ecran(&state).await;
    assert_eq!(apres["refus"], Value::Null, "{apres}");
}

#[tokio::test]
async fn un_serveur_qui_demarre_refuse_sert_le_cache_8156() {
    let dossier = tempfile::tempdir().unwrap();
    let cache = dossier.path().join("radios").join("annuaire-v2.json");
    let statut = Arc::new(Mutex::new(200));
    let (url, _vus) = faux_site(statut.clone()).await;
    assert_eq!(
        radios_annuaire::rafraichir_depuis(&etat(), &url, &cache).await,
        Releve::Nouveau(4)
    );

    *statut.lock().unwrap() = 401;
    let redemarre = etat();
    assert_eq!(
        radios_annuaire::rafraichir_depuis(&redemarre, &url, &cache).await,
        Releve::Refuse(401)
    );
    assert_eq!(redemarre.annuaire_radios.read().unwrap().len(), 4);
    assert_eq!(redemarre.annuaire_etat.read().unwrap().origine, "cache");
}
