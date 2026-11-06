//! #7922 — témoins des routes `GET`/`POST /zones/{id}/tune-tested`, avec un
//! faux site local qui note ce qu'il REÇOIT (corps et en-têtes).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tower::ServiceExt;
use tune_core::cloud::tune_tested_auto::FormatObserve;
use tune_core::cloud::tune_tested_proposition::{self as proposition, LectureRendue};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;

use crate::state::AppState;

const UDN: &str = "uuid:7922aaaa-bbbb-4ccc-8ddd-eeeeffff0000";

#[derive(Default)]
struct Recu {
    corps: Value,
    authorization: Option<String>,
    license_key: Option<String>,
    accept: Option<String>,
}

struct FauxSite {
    base: String,
    appels: Arc<AtomicUsize>,
    dernier: Arc<Mutex<Recu>>,
}

async fn faux_site() -> FauxSite {
    let appels = Arc::new(AtomicUsize::new(0));
    let dernier = Arc::new(Mutex::new(Recu::default()));
    let (a, d) = (appels.clone(), dernier.clone());
    let app = axum::Router::new().route(
        super::CHEMIN_DU_SITE,
        axum::routing::post(
            move |headers: axum::http::HeaderMap, corps: axum::body::Bytes| {
                let (a, d) = (a.clone(), d.clone());
                async move {
                    a.fetch_add(1, Ordering::SeqCst);
                    let lire = |nom: &str| {
                        headers
                            .get(nom)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string)
                    };
                    *d.lock().await = Recu {
                        corps: serde_json::from_slice(&corps).unwrap_or(Value::Null),
                        authorization: lire("authorization"),
                        license_key: lire("x-license-key"),
                        accept: lire("accept"),
                    };
                    axum::Json(json!({ "status": "pending", "proposal": { "id": 12 } }))
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    FauxSite {
        base: format!("http://127.0.0.1:{port}"),
        appels,
        dernier,
    }
}

struct Banc {
    app: axum::Router,
    settings: SettingsRepo,
    zone: i64,
}

fn banc(base: &str, reglages: &[(&str, &str)]) -> Banc {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("mozaik_base_url", base).unwrap();
    settings.set("instance_id", "instance-essai-7922").unwrap();
    for (cle, valeur) in reglages {
        settings.set(cle, valeur).unwrap();
    }
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("PRIVATE kitchen", Some("dlna"), Some(UDN))
        .unwrap();
    Banc {
        app: crate::routes::router(state),
        settings,
        zone,
    }
}

impl Banc {
    fn ecouter(&self, minutes: u64) {
        for n in 0..minutes / 4 {
            proposition::noter_lecture(
                &self.settings,
                self.zone,
                LectureRendue {
                    device_id: UDN,
                    protocole_brut: "dlna",
                    identite: Some(("Test Audio", "DAC 7922")),
                    millisecondes: 240_000,
                    format: Some(FormatObserve {
                        codec: "flac".into(),
                        sample_rate: Some(192_000),
                        bit_depth: Some(24),
                        dsd_mode: None,
                    }),
                    gapless: n > 0,
                    cle_piste: (1000 + n, 0),
                },
            )
            .unwrap();
        }
    }

    async fn appeler(&self, methode: &str) -> (StatusCode, Value) {
        self.appeler_avec(methode, Body::empty()).await
    }

    async fn appeler_avec(&self, methode: &str, corps: Body) -> (StatusCode, Value) {
        let resp = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(methode)
                    .uri(format!("/api/v1/zones/{}/tune-tested", self.zone))
                    .header("content-type", "application/json")
                    .body(corps)
                    .unwrap(),
            )
            .await
            .unwrap();
        let statut = resp.status();
        let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            statut,
            serde_json::from_slice(&octets).unwrap_or(Value::Null),
        )
    }
}

#[tokio::test]
async fn tune_tested_7922_get_dit_le_temps_restant_et_post_refuse_sous_le_seuil() {
    let site = faux_site().await;
    let banc = banc(&site.base, &[("license_key", "CLE-ESSAI-7922")]);
    banc.ecouter(40);
    let (statut, corps) = banc.appeler("GET").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["eligible"], false);
    assert_eq!(corps["seconds_without_error"], 2400);
    assert_eq!(corps["threshold_seconds"], 3600);
    assert_eq!(corps["remaining_seconds"], 1200);
    assert_eq!(corps["reason"], "below_threshold");
    assert_eq!(corps["already_proposed"], false);

    let (statut, corps) = banc.appeler("POST").await;
    assert_eq!(statut, StatusCode::CONFLICT, "{corps}");
    assert_eq!(
        corps["error"], "tune_tested_below_threshold",
        "sous une heure, l'envoi est refusé par une erreur nommée"
    );
    assert_eq!(corps["remaining_seconds"], 1200);
    assert_eq!(
        site.appels.load(Ordering::SeqCst),
        0,
        "rien ne part vers le site sous le seuil"
    );
}

#[tokio::test]
async fn tune_tested_7922_post_envoie_l_apercu_exact_sans_adresse_ni_nom_de_zone() {
    let site = faux_site().await;
    let banc = banc(&site.base, &[("license_key", "CLE-ESSAI-7922")]);
    banc.ecouter(60);
    let (statut, etat) = banc.appeler("GET").await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(etat["eligible"], true, "{etat}");
    let apercu = etat["preview"].clone();

    let (statut, corps) = banc.appeler("POST").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["status"], "ok");
    assert_eq!(site.appels.load(Ordering::SeqCst), 1);
    let recu = site.dernier.lock().await;
    assert_eq!(
        recu.corps, apercu,
        "le site reçoit exactement l'aperçu montré à l'utilisateur"
    );
    assert_eq!(recu.corps["manufacturer"], "Test Audio");
    assert_eq!(recu.corps["model"], "DAC 7922");
    assert_eq!(recu.corps["output_type"], "dlna");
    assert_eq!(recu.corps["error_free_seconds"], 3600);
    assert_eq!(recu.corps["gapless_observed"], true);
    assert_eq!(recu.corps["formats"][0]["sample_rate"], 192_000);
    assert_eq!(recu.corps["formats"][0]["bit_depth"], 24);
    assert_eq!(recu.corps["instance_id"], "instance-essai-7922");
    assert_eq!(recu.corps["tune_version"], tune_core::version());
    let texte = recu.corps.to_string();
    for interdit in ["PRIVATE kitchen", UDN, "7922aaaa", "127.0.0.1", "zone"] {
        assert!(
            !texte.contains(interdit),
            "valeur interdite envoyée au site : {interdit} — {texte}"
        );
    }
    drop(recu);

    // Une proposition par appareil : l'état le dit après l'envoi.
    let (_, etat) = banc.appeler("GET").await;
    assert_eq!(etat["already_proposed"], true);
}

#[tokio::test]
async fn tune_tested_7922_l_envoi_porte_l_identite_du_rapport_de_bogue() {
    // Licence seule : X-License-Key, sans Authorization.
    let site = faux_site().await;
    let banc_licence = banc(&site.base, &[("license_key", "CLE-ESSAI-7922")]);
    banc_licence.ecouter(60);
    assert_eq!(banc_licence.appeler("POST").await.0, StatusCode::OK);
    {
        let recu = site.dernier.lock().await;
        assert_eq!(recu.license_key.as_deref(), Some("CLE-ESSAI-7922"));
        assert!(recu.authorization.is_none());
        assert_eq!(recu.accept.as_deref(), Some("application/json"));
    }

    // Jeton de compte : Authorization: Bearer, comme le rapport de bogue.
    let banc_jeton = banc(&site.base, &[("mozaik_access_token", "jeton-essai-7922")]);
    banc_jeton.ecouter(60);
    assert_eq!(banc_jeton.appeler("POST").await.0, StatusCode::OK);
    assert_eq!(
        site.dernier.lock().await.authorization.as_deref(),
        Some("Bearer jeton-essai-7922")
    );

    // Sans aucune identité : refus nommé, rien ne part.
    let avant = site.appels.load(Ordering::SeqCst);
    let banc_anonyme = banc(&site.base, &[]);
    banc_anonyme.ecouter(60);
    let (statut, corps) = banc_anonyme.appeler("POST").await;
    assert_eq!(statut, StatusCode::PRECONDITION_FAILED, "{corps}");
    assert_eq!(corps["error"], "tune_tested_not_connected");
    assert_eq!(site.appels.load(Ordering::SeqCst), avant);
}

#[tokio::test]
async fn tune_tested_7922_un_apercu_perime_n_est_pas_envoye() {
    let site = faux_site().await;
    let banc = banc(&site.base, &[("license_key", "CLE-ESSAI-7922")]);
    banc.ecouter(60);
    let (_, etat) = banc.appeler("GET").await;
    let montre = etat["preview"].clone();
    // Une piste se termine entre l'aperçu et la confirmation.
    banc.ecouter(4);
    let confirmation = |apercu: &Value| Body::from(json!({ "preview": apercu }).to_string());
    let (statut, corps) = banc.appeler_avec("POST", confirmation(&montre)).await;
    assert_eq!(statut, StatusCode::CONFLICT, "{corps}");
    assert_eq!(
        corps["error"], "tune_tested_preview_changed",
        "un aperçu qui ne correspond plus à l'envoi est refusé par une erreur nommée"
    );
    assert_eq!(corps["preview"]["error_free_seconds"], 3840);
    assert_eq!(
        site.appels.load(Ordering::SeqCst),
        0,
        "rien ne part que l'utilisateur n'a pas vu"
    );

    // Le nouvel aperçu, confirmé, part tel quel.
    let nouveau = corps["preview"].clone();
    let (statut, corps) = banc.appeler_avec("POST", confirmation(&nouveau)).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(site.dernier.lock().await.corps, nouveau);
}

#[tokio::test]
async fn tune_tested_7922_zone_inconnue_404() {
    let site = faux_site().await;
    let banc = banc(&site.base, &[]);
    let resp = banc
        .app
        .clone()
        .oneshot(
            Request::get("/api/v1/zones/987654/tune-tested")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
