//! #7894 (point 9 de #7778) — le type annoncé à un appareil DLNA pour un
//! épisode de podcast est celui de l'audio réellement servi, pas celui que
//! suggère (ou non) l'extension de l'adresse.
//!
//! Un faux hébergeur sert un M4A sous une adresse SANS extension, avec
//! `Content-Type: audio/mp4`. La zone est une zone DLNA factice
//! (`MockOutput` de type `dlna`) : aucun son n'est produit.
//!
//! Contre-épreuve : faire rendre à `type_de_l_episode` le seul
//! `guess_audio_mime(url)` (l'ancienne déduction par l'extension) fait tomber
//! `un_m4a_sans_extension_est_annonce_audio_mp4_a_l_appareil_dlna` et
//! `le_type_declare_par_le_flux_prime_sur_l_adresse` sur « audio/mpeg ».

use crate::state::AppState;
use axum::{Router, body::Body, http::Request, routing::get};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;

struct Banc {
    app: Router,
    state: AppState,
    zone: i64,
    base: String,
    amont: tokio::task::JoinHandle<()>,
}
impl Drop for Banc {
    fn drop(&mut self) {
        self.amont.abort();
        tune_core::taches_de_fond::priorite::oublier_la_zone(self.zone);
    }
}

async fn banc() -> Banc {
    // Quelques octets suffisent : l'appareil factice ne lit pas le flux.
    let amont = Router::new()
        .route(
            "/media/42",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "audio/mp4")],
                    vec![0u8; 64],
                )
            }),
        )
        .route(
            "/media/inconnu",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
                    vec![0u8; 64],
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let amont = tokio::spawn(async move {
        axum::serve(listener, amont).await.unwrap();
    });
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let did = "uuid:salon-7894";
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon — témoin", Some("dlna"), Some(did))
        .unwrap();
    state
        .outputs
        .lock()
        .await
        .register(Box::new(MockOutput::new(did, "Salon").with_type("dlna")));
    let app = crate::routes::router(state.clone());
    Banc {
        app,
        state,
        zone,
        base,
        amont,
    }
}

/// Les types que la sortie factice a reçus de `play_media`, dans l'ordre.
async fn types_recus(b: &Banc) -> Vec<String> {
    let registre = b.state.outputs.lock().await;
    let sortie = registre.get("uuid:salon-7894").expect("sortie enregistrée");
    let garde = sortie.lock().await;
    garde
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("la sortie factice")
        .play_mime_types()
        .await
}

async fn lancer(b: &Banc, corps: Value) -> Value {
    let req = Request::builder()
        .uri(format!("/api/v1/podcasts/play/{}", b.zone))
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(corps.to_string()))
        .unwrap();
    let response = b.app.clone().oneshot(req).await.unwrap();
    let statut = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(statut.is_success(), "{statut} {v}");
    v
}

/// Le cas de l'issue : adresse sans extension, l'hébergeur dit `audio/mp4`.
#[tokio::test]
async fn un_m4a_sans_extension_est_annonce_audio_mp4_a_l_appareil_dlna() {
    let b = banc().await;
    let r = lancer(
        &b,
        json!({"audio_url": format!("{}/media/42", b.base), "title": "Épisode"}),
    )
    .await;
    assert_eq!(r["output_sent"], true, "{r}");
    assert_eq!(
        types_recus(&b).await,
        vec!["audio/mp4".to_string()],
        "l'appareil doit recevoir le type de l'audio servi, pas audio/mpeg : {r}"
    );
    assert_eq!(r["mime_type"], "audio/mp4", "{r}");
}

/// Le type déclaré par le flux (`enclosure@type`) passe avant l'adresse.
#[tokio::test]
async fn le_type_declare_par_le_flux_prime_sur_l_adresse() {
    let b = banc().await;
    let r = lancer(
        &b,
        json!({
            "audio_url": format!("{}/media/inconnu", b.base),
            "title": "Épisode",
            "mime_type": "audio/x-m4a",
        }),
    )
    .await;
    assert_eq!(types_recus(&b).await, vec!["audio/mp4".to_string()], "{r}");
    assert_eq!(r["mime_type"], "audio/mp4", "{r}");
}

/// Ce qui marchait reste : l'extension connue, et le repli `audio/mpeg`
/// quand ni le flux ni l'hébergeur ne disent de l'audio.
#[tokio::test]
async fn l_extension_et_le_repli_restent_inchanges() {
    let b = banc().await;
    let r = lancer(
        &b,
        json!({"audio_url": format!("{}/media/ep.mp3", b.base), "title": "Épisode"}),
    )
    .await;
    assert_eq!(r["mime_type"], "audio/mpeg", "{r}");
    let r = lancer(
        &b,
        json!({"audio_url": format!("{}/media/inconnu", b.base), "title": "Épisode"}),
    )
    .await;
    assert_eq!(r["mime_type"], "audio/mpeg", "{r}");
}

#[test]
fn les_orthographes_courantes_sont_ramenees_a_celle_du_dlna() {
    use super::normaliser_type_audio as n;
    assert_eq!(n("audio/x-m4a").as_deref(), Some("audio/mp4"));
    assert_eq!(
        n("Audio/MPEG; charset=binary").as_deref(),
        Some("audio/mpeg")
    );
    assert_eq!(n("audio/mp3").as_deref(), Some("audio/mpeg"));
    assert_eq!(n("audio/ogg").as_deref(), Some("audio/ogg"));
    assert_eq!(n("application/octet-stream"), None);
    assert_eq!(n("audio/"), None);
    assert_eq!(n(""), None);
}
