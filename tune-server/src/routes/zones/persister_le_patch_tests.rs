//! REF-4 phase 2 (#2219) — témoins des familles de `persister_le_patch` que
//! AUCUN test ne traversait par la route avant sa découpe.
//!
//! Relevé du 12/09/2026 sur `batch/bugs-11` (72fbdf86), une famille = un bloc
//! contigu d'écritures de `PATCH /zones/{id}` :
//!
//! | famille | témoin existant |
//! |---|---|
//! | zone et sortie (`name`, `output_*`, `gapless`, `sync_delay`, `max_sample_rate`) | `mono_downmix_dit_son_indisponibilite.rs` (`output_type` + `output_device_id`) |
//! | volume fixe | `volume_fixe_2395.rs` |
//! | lecture (`autoplay_*`, `dsd_mode`, `lyrics_offset_ms`) | `integration.rs` (`autoplay_enabled`, Sandro 0.9.70) |
//! | réseau (`dlna_*`, `alac_passthrough`, `aac_passthrough`) | **aucun par la route** — ci-dessous |
//! | marque et modèle (`brand`, `model`, `identite_appareil_effacee`) | `identite_appareil_tests.rs` |
//! | UPnP (`upnp_renderer`, `upnp_silence`) | **aucun par la route** — ci-dessous |
//! | son (`mono_downmix`, `gain_trim_db`) | `mono_downmix_dit_son_indisponibilite.rs` |
//!
//! `aac_passthrough_tests.rs` et `preconfiguration.rs` écrivent par le dépôt,
//! pas par la route : ils ne prouvent pas que le PATCH atteint la famille.
//! Ces deux témoins passent par le VRAI routeur et relisent la fiche que
//! `GET /zones/{id}` rend — la même surface que les clients.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;

fn serveur() -> (axum::Router, crate::state::AppState) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let routeur = crate::routes::router(state.clone());
    (routeur, state)
}

async fn envoyer(app: &axum::Router, requete: Request<Body>) -> (StatusCode, Value) {
    let reponse = app.clone().oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(json!(null)),
    )
}

/// Une zone DLNA — le type de sortie que les deux familles visent.
async fn zone_dlna(app: &axum::Router) -> i64 {
    let corps = json!({
        "name": "Salon refe4",
        "output_type": "dlna",
        "output_device_id": "uuid:refe4-f70496",
    });
    let (statut, fiche) = envoyer(
        app,
        Request::builder()
            .method("POST")
            .uri("/api/v1/zones")
            .header("Content-Type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(statut, StatusCode::CREATED, "création de zone : {fiche}");
    fiche["id"].as_i64().expect("un id de zone")
}

async fn patch(app: &axum::Router, id: i64, corps: Value) -> (StatusCode, Value) {
    envoyer(
        app,
        Request::builder()
            .method("PATCH")
            .uri(format!("/api/v1/zones/{id}"))
            .header("Content-Type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await
}

async fn fiche(app: &axum::Router, id: i64) -> Value {
    let (statut, fiche) = envoyer(
        app,
        Request::get(format!("/api/v1/zones/{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{fiche}");
    fiche
}

/// Réseau : les sept clés DLNA d'un seul PATCH arrivent toutes en base, et la
/// fiche les rend telles quelles. Le retard de lecture est borné à zéro par
/// la route (`delay.max(0)`) : la contre-épreuve négative le prouve.
#[tokio::test]
async fn la_famille_reseau_est_persistee_par_la_route() {
    let (app, _state) = serveur();
    let id = zone_dlna(&app).await;

    let avant = fiche(&app, id).await;
    for cle in [
        "dlna_native_flac",
        "alac_passthrough",
        "aac_passthrough",
        "dlna_lpcm",
        "dlna_cap_16bit",
        "dlna_wav24",
    ] {
        assert_eq!(
            avant[cle],
            json!(false),
            "`{cle}` armé avant tout PATCH : {avant}"
        );
    }

    let (statut, reponse) = patch(
        &app,
        id,
        json!({
            "dlna_native_flac": true,
            "alac_passthrough": true,
            "aac_passthrough": true,
            "dlna_lpcm": true,
            "dlna_cap_16bit": true,
            "dlna_wav24": true,
            "dlna_play_delay_ms": 750,
        }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "PATCH refusé : {reponse}");

    let apres = fiche(&app, id).await;
    for cle in [
        "dlna_native_flac",
        "alac_passthrough",
        "aac_passthrough",
        "dlna_lpcm",
        "dlna_cap_16bit",
        "dlna_wav24",
    ] {
        assert_eq!(
            apres[cle],
            json!(true),
            "`{cle}` accepté par la route mais absent de la fiche : {apres}"
        );
    }
    assert_eq!(apres["dlna_play_delay_ms"], json!(750), "{apres}");

    // Contre-épreuve : un retard négatif est ramené à zéro, pas refusé.
    let (statut, _) = patch(&app, id, json!({ "dlna_play_delay_ms": -40 })).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(fiche(&app, id).await["dlna_play_delay_ms"], json!(0));
}

/// UPnP : les deux interrupteurs suivent la convention « clé supprimée à la
/// désactivation » — l'absence de clé et le défaut désarmé sont un seul et
/// même état. Les deux sens sont exercés, par la fiche ET par la table des
/// réglages.
#[tokio::test]
async fn la_famille_upnp_est_persistee_par_la_route() {
    let (app, state) = serveur();
    let id = zone_dlna(&app).await;
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    let cle_renderer = format!("zone_{id}_upnp_renderer");
    let cle_silence = crate::config::cle_silence_upnp(id);

    let avant = fiche(&app, id).await;
    assert_eq!(avant["upnp_renderer"], json!(false), "{avant}");
    assert_eq!(avant["upnp_silence"], json!(false), "{avant}");

    let (statut, reponse) = patch(
        &app,
        id,
        json!({ "upnp_renderer": true, "upnp_silence": true }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "PATCH refusé : {reponse}");
    let armee = fiche(&app, id).await;
    assert_eq!(armee["upnp_renderer"], json!(true), "{armee}");
    assert_eq!(armee["upnp_silence"], json!(true), "{armee}");
    assert_eq!(
        reglages.get(&cle_renderer).unwrap().as_deref(),
        Some("true")
    );
    assert_eq!(reglages.get(&cle_silence).unwrap().as_deref(), Some("true"));

    let (statut, reponse) = patch(
        &app,
        id,
        json!({ "upnp_renderer": false, "upnp_silence": false }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "PATCH refusé : {reponse}");
    let desarmee = fiche(&app, id).await;
    assert_eq!(desarmee["upnp_renderer"], json!(false), "{desarmee}");
    assert_eq!(desarmee["upnp_silence"], json!(false), "{desarmee}");
    assert!(
        reglages.get(&cle_renderer).unwrap().is_none(),
        "désactiver doit SUPPRIMER la clé, pas écrire « false »"
    );
    assert!(
        reglages.get(&cle_silence).unwrap().is_none(),
        "désactiver doit SUPPRIMER la clé, pas écrire « false »"
    );
}
