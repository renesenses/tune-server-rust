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

/// #3973 — « bit-perfect strict » : présent sur la fiche dès le départ (c'est
/// sa PRÉSENCE qui fait apparaître l'interrupteur côté client), armé par la
/// route, persisté sous `zone_{id}_strict_bitperfect`, désarmé en SUPPRIMANT
/// la clé.
#[tokio::test]
async fn bitperfect_strict_3973_est_persiste_par_la_route() {
    let (app, state) = serveur();
    let id = zone_dlna(&app).await;
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    let cle = tune_core::audio::bitperfect_strict::cle_de_zone(id);

    let avant = fiche(&app, id).await;
    assert_eq!(
        avant["strict_bitperfect"],
        json!(false),
        "le champ doit être publié, désarmé par défaut : {avant}"
    );

    let (statut, reponse) = patch(&app, id, json!({ "strict_bitperfect": true })).await;
    assert_eq!(statut, StatusCode::OK, "PATCH refusé : {reponse}");
    assert_eq!(fiche(&app, id).await["strict_bitperfect"], json!(true));
    assert_eq!(reglages.get(&cle).unwrap().as_deref(), Some("true"));

    let (statut, reponse) = patch(&app, id, json!({ "strict_bitperfect": false })).await;
    assert_eq!(statut, StatusCode::OK, "PATCH refusé : {reponse}");
    assert_eq!(fiche(&app, id).await["strict_bitperfect"], json!(false));
    assert!(
        reglages.get(&cle).unwrap().is_none(),
        "désactiver SUPPRIME la clé"
    );
}

/// #2991 — « Envoyer le titre radio par DLNA » : absent = `off`, `next` et
/// `reload` se persistent et se relisent, `off` supprime la clé, une voie
/// inconnue est refusée sans rien écrire.
#[tokio::test]
async fn le_titre_radio_par_didl_se_regle_par_la_route() {
    let (app, state) = serveur();
    let id = zone_dlna(&app).await;
    let cle = tune_core::outputs::dlna::titre_radio_2991::cle_de_zone(id);
    let settings = SettingsRepo::with_backend(state.backend.clone());

    assert_eq!(fiche(&app, id).await["dlna_radio_title"], json!("off"));

    for voie in ["next", "reload"] {
        let (statut, corps) = patch(&app, id, json!({ "dlna_radio_title": voie })).await;
        assert_eq!(statut, StatusCode::OK, "{corps}");
        assert_eq!(fiche(&app, id).await["dlna_radio_title"], json!(voie));
        assert_eq!(settings.get(&cle).unwrap().as_deref(), Some(voie));
    }

    let (statut, corps) = patch(&app, id, json!({ "dlna_radio_title": "toujours" })).await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
    assert_eq!(
        settings.get(&cle).unwrap().as_deref(),
        Some("reload"),
        "une voie refusée ne doit rien écrire"
    );

    let (statut, corps) = patch(&app, id, json!({ "dlna_radio_title": "off" })).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(fiche(&app, id).await["dlna_radio_title"], json!("off"));
    assert_eq!(settings.get(&cle).unwrap(), None, "« off » supprime la clé");
}

#[tokio::test]
async fn frequence_fixe_8428_route_persiste_efface_et_valide() {
    let (app, _) = serveur();
    let id = zone_dlna(&app).await;
    assert_eq!(fiche(&app, id).await["fixed_sample_rate"], Value::Null);
    let (statut, reponse) = patch(&app, id, json!({"fixed_sample_rate": 48000})).await;
    assert_eq!(statut, StatusCode::OK, "{reponse}");
    assert_eq!(
        fiche(&app, id).await["fixed_sample_rate"],
        json!(48000),
        "la cadence fixe doit être persistée par le PATCH"
    );
    let (statut, _) = patch(&app, id, json!({"fixed_sample_rate": 0})).await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
    assert_eq!(fiche(&app, id).await["fixed_sample_rate"], json!(48000));
    let (statut, _) = patch(&app, id, json!({"fixed_sample_rate": null})).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(fiche(&app, id).await["fixed_sample_rate"], Value::Null);
}

#[tokio::test]
async fn frequence_fixe_8428_airplay_refuse_la_cadence_mais_permet_de_l_effacer() {
    let (app, state) = serveur();
    let repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    for sortie in ["airplay", "airplay2"] {
        let id = repo.create("AirPlay", Some(sortie), None).unwrap();
        let (statut, _) = patch(&app, id, json!({"fixed_sample_rate": 48000})).await;
        assert_eq!(statut, StatusCode::BAD_REQUEST);
        assert_eq!(fiche(&app, id).await["fixed_sample_rate"], Value::Null);
        let (statut, _) = patch(&app, id, json!({"fixed_sample_rate": null})).await;
        assert_eq!(statut, StatusCode::OK);
    }
}

#[tokio::test]
async fn protection_du_niveau_dsd_8813_est_active_par_defaut_et_persistante() {
    let (app, state) = serveur();
    let id = zone_dlna(&app).await;
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    let cle = tune_core::audio::dop_ecarte_7265::cle_protection_du_niveau(id);
    assert_eq!(fiche(&app, id).await["dsd_volume_protection"], json!(true));
    let (statut, reponse) = patch(&app, id, json!({"dsd_volume_protection": false})).await;
    assert_eq!(statut, StatusCode::OK, "{reponse}");
    assert_eq!(fiche(&app, id).await["dsd_volume_protection"], json!(false));
    assert_eq!(reglages.get(&cle).unwrap().as_deref(), Some("false"));
    let (statut, reponse) = patch(&app, id, json!({"dsd_volume_protection": true})).await;
    assert_eq!(statut, StatusCode::OK, "{reponse}");
    assert_eq!(fiche(&app, id).await["dsd_volume_protection"], json!(true));
    assert!(
        reglages.get(&cle).unwrap().is_none(),
        "activer retrouve le défaut sûr"
    );
}

#[tokio::test]
async fn activer_la_protection_dsd_8813_remplace_le_dop_attenue_en_cours() {
    let (app, state) = serveur();
    let appareil = "local:protection-api-8813";
    let zones = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let zone = zones
        .create("DoP factice API", Some("local"), Some(appareil))
        .unwrap();
    zones.update_dsd_mode(zone, "dop").unwrap();
    zones.update_volume(zone, 6.0).unwrap();
    state.playback.set_volume(zone, 0.06).await;
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            &tune_core::audio::dop_ecarte_7265::cle_protection_du_niveau(zone),
            "false",
        )
        .unwrap();
    let chemin = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tune-core/tests/fixtures/dsd/ref_dsd64_stereo.dsf");
    let mut piste = tune_core::db::models::Track::new("Témoin API DSD".into());
    piste.file_path = Some(chemin.to_string_lossy().into_owned());
    piste.format = Some("dsf".into());
    piste.sample_rate = Some(2_822_400);
    piste.bit_depth = Some(1);
    piste.channels = 2;
    piste.duration_ms = 1000;
    let id = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone())
        .create(&piste)
        .unwrap();
    state.outputs.lock().await.register(Box::new(
        tune_core::outputs::mock::MockOutput::new(appareil, "Sans son").with_type("local"),
    ));
    state
        .orchestrator
        .play(tune_core::orchestrator::PlayRequest {
            zone_id: zone,
            output_device_id: Some(appareil.into()),
            track_id: Some(id),
            source: Some("local".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let sortie = state.outputs.lock().await.get(appareil).unwrap();
    sortie
        .lock()
        .await
        .as_any()
        .downcast_ref::<tune_core::outputs::mock::MockOutput>()
        .unwrap()
        .poser_dop_actif(true);
    let (statut, reponse) = patch(&app, zone, json!({"dsd_volume_protection": true})).await;
    assert_eq!(statut, StatusCode::OK, "{reponse}");
    let appels = sortie
        .lock()
        .await
        .as_any()
        .downcast_ref::<tune_core::outputs::mock::MockOutput>()
        .unwrap()
        .play_call_count()
        .await;
    assert_eq!(
        appels, 2,
        "#8813 : activer la protection laisse le DoP atténué en cours"
    );
    assert_eq!(
        fiche(&app, zone).await["dsd_volume_protection"],
        json!(true)
    );
}

/// #8696 — période WASAPI exclusive : publiée à `null` (Auto) avec ses
/// choix, persistée par la route, refusée hors bornes (422), et remise à Auto par `null` ou `0`
/// en SUPPRIMANT la clé.
#[tokio::test]
async fn periode_wasapi_8696_est_persistee_par_la_route() {
    let (app, state) = serveur();
    let id = zone_dlna(&app).await;
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    let cle = tune_core::outputs::periode_reglable_8696::cle_de_zone(id);

    let avant = fiche(&app, id).await;
    assert_eq!(avant["wasapi_exclusive_period_ms"], Value::Null, "{avant}");
    assert_eq!(
        avant["wasapi_exclusive_period_choices_ms"],
        json!([10, 20, 50, 100])
    );
    // Zone DLNA : jamais applicable, quelle que soit la plateforme.
    assert_eq!(avant["wasapi_exclusive_period_applicable"], json!(false));

    let (statut, reponse) = patch(&app, id, json!({ "wasapi_exclusive_period_ms": 50 })).await;
    assert_eq!(statut, StatusCode::OK, "PATCH refusé : {reponse}");
    assert_eq!(
        fiche(&app, id).await["wasapi_exclusive_period_ms"],
        json!(50)
    );
    assert_eq!(reglages.get(&cle).unwrap().as_deref(), Some("50"));

    // Hors bornes : 422 en clair, et le réglage en place n'est pas touché.
    for hors in [5000, 2] {
        let (statut, reponse) =
            patch(&app, id, json!({ "wasapi_exclusive_period_ms": hors })).await;
        assert_eq!(statut, StatusCode::UNPROCESSABLE_ENTITY, "{reponse}");
        assert!(reponse.to_string().contains("3 à 500 ms"), "{reponse}");
        assert_eq!(reglages.get(&cle).unwrap().as_deref(), Some("50"));
    }

    // Un PATCH sans le champ ne touche pas au réglage.
    let (statut, _) = patch(&app, id, json!({ "strict_bitperfect": false })).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(reglages.get(&cle).unwrap().as_deref(), Some("50"));

    let (statut, _) = patch(&app, id, json!({ "wasapi_exclusive_period_ms": 0 })).await;
    assert_eq!(statut, StatusCode::OK);
    assert!(
        reglages.get(&cle).unwrap().is_none(),
        "0 = Auto : clé supprimée"
    );

    let (statut, _) = patch(&app, id, json!({ "wasapi_exclusive_period_ms": 20 })).await;
    assert_eq!(statut, StatusCode::OK);
    let (statut, _) = patch(&app, id, json!({ "wasapi_exclusive_period_ms": null })).await;
    assert_eq!(statut, StatusCode::OK);
    assert!(
        reglages.get(&cle).unwrap().is_none(),
        "null = Auto : clé supprimée"
    );
    assert_eq!(
        fiche(&app, id).await["wasapi_exclusive_period_ms"],
        Value::Null
    );
}
