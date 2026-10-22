//! #6541 — les routes des boutons + / − et des limites, de bout en bout par
//! le routeur public, sur une sortie factice.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;

const APPAREIL: &str = "dlna-salon-6541";

async fn serveur() -> (axum::Router, crate::state::AppState, i64) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let routeur = crate::routes::router(state.clone());
    let zone_id = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon", Some("dlna"), Some(APPAREIL))
        .unwrap();
    state.outputs.lock().await.register(Box::new(
        MockOutput::new(APPAREIL, "Salon").with_type("dlna"),
    ));
    state.playback.set_volume(zone_id, 0.2).await;
    (routeur, state, zone_id)
}

async fn appeler(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Value,
) -> (StatusCode, Value) {
    let requete = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("Content-Type", "application/json")
        .body(Body::from(corps.to_string()))
        .unwrap();
    let reponse = app.clone().oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// `PUT /zones/{id}/volume-limits`, réponse attendue 200.
async fn regler(app: &axum::Router, id: i64, corps: Value) -> Value {
    let (statut, rendu) = appeler(
        app,
        "PUT",
        &format!("/api/v1/zones/{id}/volume-limits"),
        corps.clone(),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps} -> {rendu}");
    rendu
}

async fn recus(state: &crate::state::AppState) -> Vec<f64> {
    let outputs = state.outputs.lock().await;
    let out = outputs.get(APPAREIL).unwrap();
    let guard = out.lock().await;
    guard
        .as_any()
        .downcast_ref::<MockOutput>()
        .unwrap()
        .volume_calls()
        .await
}

fn proche(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

/// Le scénario du fil, par l'API : sécurité 70 %, confort −10,457 dB (30 %) ;
/// le PUT du curseur à 100 % s'arrête au confort, les boutons montent jusqu'à
/// la sécurité et le disent quand elle les retient.
#[tokio::test]
async fn curseur_borne_au_confort_et_boutons_bornes_a_la_securite_6541() {
    let (app, state, id) = serveur().await;
    let (statut, corps) = appeler(
        &app,
        "PUT",
        &format!("/api/v1/zones/{id}/volume-limits"),
        json!({ "safety": 0.7, "comfort_db": 20.0 * 0.3f64.log10() }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert!(proche(corps["volume_limit_comfort"].as_f64().unwrap(), 0.3));

    let (statut, _) = appeler(
        &app,
        "PUT",
        &format!("/api/v1/zones/{id}/volume"),
        json!({ "volume": 1.0 }),
    )
    .await;
    assert_eq!(statut, StatusCode::NO_CONTENT);
    assert!(proche(*recus(&state).await.last().unwrap(), 0.3));

    let mut dernier = Value::Null;
    for _ in 0..30 {
        let (statut, corps) = appeler(
            &app,
            "POST",
            &format!("/api/v1/zones/{id}/volume/step"),
            json!({ "direction": "up", "step": 0.05 }),
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{corps}");
        dernier = corps;
    }
    assert!(
        proche(dernier["volume"].as_f64().unwrap(), 0.7),
        "{dernier}"
    );
    assert_eq!(dernier["limited"], true);
    assert!(
        recus(&state).await.iter().all(|v| *v <= 0.7 + 1e-9),
        "aucune commande au-dessus de la sécurité : {:?}",
        recus(&state).await
    );
}

/// Poser une sécurité sous le volume courant ramène la zone sur-le-champ.
#[tokio::test]
async fn une_securite_sous_le_volume_courant_ramene_la_zone_6541() {
    let (app, state, id) = serveur().await;
    appeler(
        &app,
        "PUT",
        &format!("/api/v1/zones/{id}/volume"),
        json!({ "volume": 0.9 }),
    )
    .await;
    let (statut, _) = appeler(
        &app,
        "PUT",
        &format!("/api/v1/zones/{id}/volume-limits"),
        json!({ "safety": 0.5 }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(recus(&state).await, vec![0.9, 0.5]);
    // Abaissée encore : ramenée encore, à la baisse.
    regler(&app, id, json!({ "safety": 0.3 })).await;
    assert_eq!(recus(&state).await, vec![0.9, 0.5, 0.3]);
    // Un confort sous le volume courant ne commande rien : il ne retient que
    // les montées du curseur.
    regler(&app, id, json!({ "safety": 0.3, "comfort": 0.1 })).await;
    assert_eq!(recus(&state).await, vec![0.9, 0.5, 0.3]);
    // GET de la zone : les limites sont publiées.
    let (_, zone) = appeler(&app, "GET", &format!("/api/v1/zones/{id}"), Value::Null).await;
    assert!(
        proche(zone["volume_limit_safety"].as_f64().unwrap(), 0.3),
        "{zone}"
    );
    assert!(
        proche(zone["volume_limit_comfort"].as_f64().unwrap(), 0.1),
        "{zone}"
    );
    assert!(zone["volume_limits_refusal"].is_null(), "{zone}");
}

#[tokio::test]
async fn les_demandes_invalides_sont_refusees_6541() {
    let (app, _state, id) = serveur().await;
    for corps in [
        json!({ "safety": 0.7, "safety_db": -3.0 }),
        json!({ "safety": 0.0 }),
        json!({ "comfort_db": 3.0 }),
    ] {
        let (statut, _) = appeler(
            &app,
            "PUT",
            &format!("/api/v1/zones/{id}/volume-limits"),
            corps.clone(),
        )
        .await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
    }
    for corps in [
        json!({ "direction": "left" }),
        json!({ "direction": "up", "step": 0.5 }),
        json!({ "direction": "up", "step": 0.02, "step_db": 1.0 }),
    ] {
        let (statut, _) = appeler(
            &app,
            "POST",
            &format!("/api/v1/zones/{id}/volume/step"),
            corps.clone(),
        )
        .await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
    }
}

/// Sans limite, le bouton fait un pas de 2 points (défaut) ; le PUT efface.
#[tokio::test]
async fn sans_limite_le_pas_par_defaut_est_de_deux_points_6541() {
    let (app, _state, id) = serveur().await;
    let (_, corps) = appeler(
        &app,
        "POST",
        &format!("/api/v1/zones/{id}/volume/step"),
        json!({ "direction": "down" }),
    )
    .await;
    assert!(proche(corps["volume"].as_f64().unwrap(), 0.18), "{corps}");
    assert_eq!(corps["limited"], false);
}

fn poser_pure(state: &crate::state::AppState, id: i64, verrou: bool) {
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set(
            &format!("zone_{id}_audiophile"),
            &json!({ "enabled": true, "lock_volume": verrou }).to_string(),
        )
        .unwrap();
}

/// Décision du 21/10 : pas de limite sur une zone PURE, verrouillée en
/// bit-perfect ou non. Refus 409 au code stable, rien d'écrit, rien envoyé.
#[tokio::test]
async fn une_limite_sur_une_zone_pure_est_refusee_6541() {
    for verrou in [true, false] {
        let (app, state, id) = serveur().await;
        poser_pure(&state, id, verrou);
        for corps in [json!({ "safety": 0.5 }), json!({ "comfort_db": -20.0 })] {
            let (statut, rendu) = appeler(
                &app,
                "PUT",
                &format!("/api/v1/zones/{id}/volume-limits"),
                corps.clone(),
            )
            .await;
            assert_eq!(
                statut,
                StatusCode::CONFLICT,
                "verrou {verrou}, {corps}: {rendu}"
            );
            assert_eq!(rendu["error"], "limite_refusee_zone_pure", "{rendu}");
            assert!(
                rendu["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("PURE")),
                "{rendu}"
            );
        }
        let (_, limites) = appeler(
            &app,
            "GET",
            &format!("/api/v1/zones/{id}/volume-limits"),
            Value::Null,
        )
        .await;
        assert!(limites["volume_limit_safety"].is_null(), "{limites}");
        assert!(limites["volume_limit_comfort"].is_null(), "{limites}");
        assert_eq!(limites["volume_limits_refusal"], "limite_refusee_zone_pure");
        assert!(recus(&state).await.is_empty(), "{:?}", recus(&state).await);
    }
}

/// Pas de limite sur une zone à volume fixe. C'est aussi le défaut de la
/// relecture du 21/10 : sécurité 0,6 posée, zone passée en volume fixe, puis
/// sécurité relevée à 0,9 — l'appareil montait de 0,6 à 0,9 sans geste. Le
/// relèvement est refusé et l'appareil ne reçoit RIEN ; effacer reste permis
/// et n'envoie rien non plus.
#[tokio::test]
async fn une_limite_sur_une_zone_a_volume_fixe_est_refusee_6541() {
    let (app, state, id) = serveur().await;
    regler(&app, id, json!({ "safety": 0.6 })).await;
    ZoneRepo::with_backend(state.backend.clone())
        .update_fixed_volume(id, true)
        .unwrap();
    let (statut, rendu) = appeler(
        &app,
        "PUT",
        &format!("/api/v1/zones/{id}/volume-limits"),
        json!({ "safety": 0.9 }),
    )
    .await;
    assert_eq!(statut, StatusCode::CONFLICT, "{rendu}");
    assert_eq!(rendu["error"], "limite_refusee_volume_fixe", "{rendu}");
    assert!(
        recus(&state).await.is_empty(),
        "zone à volume fixe : aucune commande de volume, reçu {:?}",
        recus(&state).await
    );
    let efface = regler(&app, id, json!({})).await;
    assert!(efface["volume_limit_safety"].is_null(), "{efface}");
    assert_eq!(
        efface["volume_limits_refusal"],
        "limite_refusee_volume_fixe"
    );
    assert!(recus(&state).await.is_empty(), "{:?}", recus(&state).await);
}

/// Une limite posée AVANT le passage en PURE est ignorée : elle ne commande
/// rien, et le geste de l'utilisateur passe sans elle. Elle reste enregistrée
/// et publiée, avec la raison pour laquelle elle n'a pas cours.
#[tokio::test]
async fn une_limite_existante_est_ignoree_en_pure_6541() {
    let (app, state, id) = serveur().await;
    regler(&app, id, json!({ "safety": 0.5, "comfort": 0.3 })).await;
    assert!(
        recus(&state).await.is_empty(),
        "zone à 20 % : rien à ramener"
    );
    poser_pure(&state, id, false);
    let (statut, _) = appeler(
        &app,
        "PUT",
        &format!("/api/v1/zones/{id}/volume"),
        json!({ "volume": 0.9 }),
    )
    .await;
    assert_eq!(statut, StatusCode::NO_CONTENT);
    assert_eq!(
        recus(&state).await,
        vec![0.9],
        "en PURE, la limite ne retient ni ne commande rien : seul le geste part"
    );
    let (_, zone) = appeler(&app, "GET", &format!("/api/v1/zones/{id}"), Value::Null).await;
    assert!(
        proche(zone["volume_limit_safety"].as_f64().unwrap(), 0.5),
        "{zone}"
    );
    assert_eq!(
        zone["volume_limits_refusal"], "limite_refusee_zone_pure",
        "{zone}"
    );
}

/// Relever une limite (ou l'effacer) n'envoie JAMAIS rien à l'appareil : la
/// nouvelle limite ne sert qu'aux gestes suivants.
#[tokio::test]
async fn relever_une_limite_n_envoie_aucune_commande_6541() {
    let (app, state, id) = serveur().await;
    regler(&app, id, json!({ "safety": 0.5 })).await;
    // Curseur à 100 % : retenu à la sécurité.
    appeler(
        &app,
        "PUT",
        &format!("/api/v1/zones/{id}/volume"),
        json!({ "volume": 1.0 }),
    )
    .await;
    assert_eq!(recus(&state).await, vec![0.5]);
    // Relevée au-dessus du volume courant : rien ne part.
    regler(&app, id, json!({ "safety": 0.9 })).await;
    regler(&app, id, json!({ "safety": 0.9, "comfort": 0.8 })).await;
    regler(&app, id, json!({})).await;
    assert_eq!(
        recus(&state).await,
        vec![0.5],
        "relever ou effacer la limite ne doit commander aucun volume"
    );
    assert_eq!(state.playback.get_state(id).await.volume, 0.5);
}
