//! #7133 — témoins PAR ROUTE de la double sortie (Lulu, fil 2467).
//!
//! Le banc : deux sorties locales WASAPI SIMULÉES (`MockOutput` typées
//! `local`, aucun périphérique ouvert, aucun son), les haut-parleurs du PC et
//! un DAC USB. Une zone joue sur les haut-parleurs ; l'utilisateur lui choisit
//! le DAC par l'écran Zones, puis la lecture repart sur le DAC. Le défaut se
//! mesure en comptant les sorties qui jouent : il doit en rester UNE.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::outputs::mock::MockOutput;
use tune_core::outputs::traits::TransportState;
use tune_core::playback::{NowPlaying, PlayState};

const HAUT_PARLEURS: &str = "local:Haut-parleurs (Realtek(R) Audio)";
const DAC: &str = "local:Topping D90";

async fn banc() -> (axum::Router, crate::state::AppState) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    {
        let mut sorties = state.outputs.lock().await;
        sorties.register(Box::new(
            MockOutput::new(HAUT_PARLEURS, "Haut-parleurs (Realtek(R) Audio)").with_type("local"),
        ));
        sorties.register(Box::new(
            MockOutput::new(DAC, "Topping D90").with_type("local"),
        ));
    }
    let routeur = crate::routes::router(state.clone());
    (routeur, state)
}

async fn envoyer(
    app: &axum::Router,
    methode: &str,
    uri: &str,
    corps: Value,
) -> (StatusCode, Value) {
    let reponse = app
        .clone()
        .oneshot(
            Request::builder()
                .method(methode)
                .uri(uri)
                .header("Content-Type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(json!(null)),
    )
}

/// Pose l'état de transport d'une sortie simulée du registre.
async fn poser(state: &crate::state::AppState, device_id: &str, etat: TransportState) {
    let sortie = state.outputs.lock().await.get(device_id).unwrap();
    let sortie = sortie.lock().await;
    sortie
        .as_any()
        .downcast_ref::<MockOutput>()
        .unwrap()
        .set_state(etat)
        .await;
}

async fn etat(state: &crate::state::AppState, device_id: &str) -> TransportState {
    let sortie = state.outputs.lock().await.get(device_id).unwrap();
    sortie.lock().await.get_status().await.unwrap().state
}

/// Les sorties `local:` du registre qui jouent : ce que l'auditeur entend.
async fn sorties_qui_jouent(state: &crate::state::AppState) -> Vec<String> {
    let mut jouent = Vec::new();
    for id in [HAUT_PARLEURS, DAC] {
        if etat(state, id).await == TransportState::Playing {
            jouent.push(id.to_string());
        }
    }
    jouent
}

/// Une zone locale sur les haut-parleurs du PC, en lecture (ou en pause).
async fn zone_qui_joue(
    app: &axum::Router,
    state: &crate::state::AppState,
    etat_de_zone: PlayState,
) -> i64 {
    let (statut, fiche) = envoyer(
        app,
        "POST",
        "/api/v1/zones",
        json!({ "name": "Bureau 7133", "output_type": "local", "output_device_id": HAUT_PARLEURS }),
    )
    .await;
    assert_eq!(statut, StatusCode::CREATED, "création de zone : {fiche}");
    let id = fiche["id"].as_i64().expect("un id de zone");
    state
        .playback
        .play(
            id,
            NowPlaying {
                title: "Morceau A".into(),
                source: "local".into(),
                duration_ms: 240_000,
                ..Default::default()
            },
        )
        .await;
    poser(state, HAUT_PARLEURS, TransportState::Playing).await;
    if etat_de_zone == PlayState::Paused {
        state.playback.pause(id).await;
        poser(state, HAUT_PARLEURS, TransportState::Paused).await;
    }
    id
}

/// TÉMOIN — la zone qui joue sur les haut-parleurs passe sur le DAC par
/// `PATCH /zones/{id}` (le geste de l'écran Zones du client web), puis la
/// lecture repart sur le DAC : une seule sortie doit jouer.
#[tokio::test]
async fn changer_la_sortie_d_une_zone_qui_joue_coupe_l_ancienne_7133() {
    let (app, state) = banc().await;
    let id = zone_qui_joue(&app, &state, PlayState::Playing).await;

    let (statut, fiche) = envoyer(
        &app,
        "PATCH",
        &format!("/api/v1/zones/{id}"),
        json!({ "output_type": "local", "output_device_id": DAC }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{fiche}");
    assert_eq!(fiche["output_device_id"], json!(DAC), "{fiche}");

    // La lecture repart sur la sortie choisie (Lire, ou la suivante).
    poser(&state, DAC, TransportState::Playing).await;

    assert_eq!(
        sorties_qui_jouent(&state).await,
        vec![DAC.to_string()],
        "double sortie (#7133) : la zone est passée sur le DAC, mais « {HAUT_PARLEURS} » \
         joue encore — l'appareil quitté n'a reçu aucun arrêt"
    );
    assert_eq!(
        state.playback.get_state(id).await.state,
        PlayState::Stopped,
        "la zone est arrêtée avec son ancienne sortie : l'écran ne doit pas la dire en lecture"
    );
}

/// En pause, la sortie quittée garde son flux ouvert : elle est arrêtée aussi.
#[tokio::test]
async fn changer_la_sortie_d_une_zone_en_pause_coupe_l_ancienne_7133() {
    let (app, state) = banc().await;
    let id = zone_qui_joue(&app, &state, PlayState::Paused).await;

    let (statut, fiche) = envoyer(
        &app,
        "PATCH",
        &format!("/api/v1/zones/{id}"),
        json!({ "output_type": "local", "output_device_id": DAC }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{fiche}");
    assert_eq!(
        etat(&state, HAUT_PARLEURS).await,
        TransportState::Stopped,
        "la sortie quittée en pause garde son flux (#7133)"
    );
}

/// Le profil de zones est l'autre chemin qui change l'appareil d'une zone.
#[tokio::test]
async fn un_profil_qui_change_la_sortie_coupe_l_ancienne_7133() {
    let (app, state) = banc().await;
    let id = zone_qui_joue(&app, &state, PlayState::Playing).await;
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            "zone_profiles",
            &json!([{ "id": 1, "name": "Soir", "zones": [
                { "zone_id": id, "output_device_id": DAC, "output_type": "local" }
            ]}])
            .to_string(),
        )
        .unwrap();
    let (statut, reponse) = envoyer(
        &app,
        "POST",
        "/api/v1/zone-manager/profiles/1/activate",
        json!({}),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{reponse}");
    poser(&state, DAC, TransportState::Playing).await;
    assert_eq!(
        sorties_qui_jouent(&state).await,
        vec![DAC.to_string()],
        "double sortie (#7133) par un profil de zones : « {HAUT_PARLEURS} » joue encore"
    );
}

/// CONTRE-ÉPREUVE — un PATCH qui ne change pas d'appareil (le même, ou un
/// autre champ) n'arrête rien : la musique ne se coupe pas quand on renomme
/// une zone.
#[tokio::test]
async fn un_patch_sans_changement_d_appareil_ne_coupe_rien_7133() {
    let (app, state) = banc().await;
    let id = zone_qui_joue(&app, &state, PlayState::Playing).await;

    for corps in [
        json!({ "name": "Bureau renommé" }),
        json!({ "output_type": "local", "output_device_id": HAUT_PARLEURS }),
    ] {
        let (statut, fiche) =
            envoyer(&app, "PATCH", &format!("/api/v1/zones/{id}"), corps.clone()).await;
        assert_eq!(statut, StatusCode::OK, "{corps} : {fiche}");
        assert_eq!(
            etat(&state, HAUT_PARLEURS).await,
            TransportState::Playing,
            "{corps} : la zone n'a pas changé d'appareil, sa lecture ne doit pas être coupée"
        );
        assert_eq!(state.playback.get_state(id).await.state, PlayState::Playing);
    }
}
