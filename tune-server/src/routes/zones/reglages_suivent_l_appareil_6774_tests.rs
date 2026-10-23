//! #6774 — témoins PAR ROUTE : une zone qui change d'appareil pour un AUTRE
//! appareil repart d'un trim de 0 dB ; ses limites restent. Le même appareil
//! garde tout. Les trois chemins par lesquels l'utilisateur change l'appareil
//! d'une zone : `PATCH /zones/{id}`, `POST /zone-manager/zones/{id}/hot-swap`
//! et l'activation d'un profil de zones.

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

/// Une zone DLNA réglée : trim +8,5 dB, limite de sécurité 50 %.
async fn zone_reglee(app: &axum::Router, state: &crate::state::AppState, appareil: &str) -> i64 {
    let (statut, fiche) = envoyer(
        app,
        "POST",
        "/api/v1/zones",
        json!({ "name": "Salon 6774", "output_type": "dlna", "output_device_id": appareil }),
    )
    .await;
    assert_eq!(statut, StatusCode::CREATED, "création de zone : {fiche}");
    let id = fiche["id"].as_i64().expect("un id de zone");
    let s = SettingsRepo::with_backend(state.backend.clone());
    s.set(&format!("zone_{id}_gain_trim_db"), "8.5").unwrap();
    s.set(&format!("zone_{id}_volume_limit_safety"), "0.5")
        .unwrap();
    id
}

fn trim(state: &crate::state::AppState, id: i64) -> Option<String> {
    SettingsRepo::with_backend(state.backend.clone())
        .get(&format!("zone_{id}_gain_trim_db"))
        .unwrap()
}

fn limite(state: &crate::state::AppState, id: i64) -> Option<String> {
    SettingsRepo::with_backend(state.backend.clone())
        .get(&format!("zone_{id}_volume_limit_safety"))
        .unwrap()
}

#[tokio::test]
async fn patch_vers_un_autre_appareil_remet_le_trim_a_zero() {
    let (app, state) = serveur();
    let id = zone_reglee(&app, &state, "uuid:p6774-marantz").await;

    let (statut, reponse) = envoyer(
        &app,
        "PATCH",
        &format!("/api/v1/zones/{id}"),
        json!({ "output_device_id": "uuid:p6774-meitner" }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{reponse}");
    assert_eq!(
        trim(&state, id),
        None,
        "PATCH output_device_id : le trim de +8,5 dB a suivi la zone sur un AUTRE appareil (#6774)"
    );
    assert_eq!(
        limite(&state, id).as_deref(),
        Some("0.5"),
        "les limites restent"
    );
}

#[tokio::test]
async fn patch_vers_le_meme_appareil_garde_le_trim() {
    let (app, state) = serveur();
    let id = zone_reglee(&app, &state, "uuid:p6774-meme").await;
    let (statut, reponse) = envoyer(
        &app,
        "PATCH",
        &format!("/api/v1/zones/{id}"),
        json!({ "output_device_id": "uuid:p6774-meme", "name": "Salon renommé" }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{reponse}");
    assert_eq!(
        trim(&state, id).as_deref(),
        Some("8.5"),
        "même appareil : tout reste"
    );
}

#[tokio::test]
async fn hot_swap_vers_un_autre_appareil_remet_le_trim_a_zero() {
    let (app, state) = serveur();
    let id = zone_reglee(&app, &state, "uuid:h6774-a").await;
    let (statut, reponse) = envoyer(
        &app,
        "POST",
        &format!("/api/v1/zone-manager/zones/{id}/hot-swap"),
        json!({ "output_device_id": "uuid:h6774-b", "output_type": "dlna" }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{reponse}");
    assert_eq!(
        trim(&state, id),
        None,
        "hot-swap : le trim a suivi la zone sur un AUTRE appareil (#6774)"
    );
    assert_eq!(limite(&state, id).as_deref(), Some("0.5"));
}

#[tokio::test]
async fn un_profil_qui_change_l_appareil_remet_le_trim_a_zero() {
    let (app, state) = serveur();
    let id = zone_reglee(&app, &state, "uuid:z6774-a").await;
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            "zone_profiles",
            &json!([{ "id": 1, "name": "Soir", "zones": [
                { "zone_id": id, "output_device_id": "uuid:z6774-b", "output_type": "dlna" }
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
    assert_eq!(
        trim(&state, id),
        None,
        "profil de zones : le trim a suivi la zone sur un AUTRE appareil (#6774)"
    );
    assert_eq!(limite(&state, id).as_deref(), Some("0.5"));
}

/// Découverte : chaque rattachement d'une zone à un appareil DÉCOUVERT passe
/// par `changer_d_appareil` avec la MAC de cet appareil — c'est elle qui
/// distingue « même appareil » (tout reste) d'« autre appareil » (trim à 0).
/// La règle elle-même est éprouvée côté dépôt
/// (`tune-core/src/db/reglages_suivent_l_appareil_6774_tests.rs`).
#[test]
fn la_decouverte_rattache_avec_la_mac_de_l_appareil() {
    let source: String = include_str!("../../discovery_setup.rs")
        .split_whitespace()
        .collect();
    // Coupé avant les modules de test du fichier.
    let source = &source[..source
        .find("#[cfg(test)]modfilet_par_mac_ssdp_4580")
        .expect("discovery_setup.rs a ses tests après le code")];
    let sans_mac = [
        concat!("zone_repo.update_output_device(zid,&", "dev.id)"),
        concat!("zone_repo.update_device_id(", "zid,&dev.id)"),
        concat!("zone_repo.update_device_id(", "existing_zone_id,&dev.id)"),
    ];
    for motif in sans_mac {
        assert_eq!(
            source.matches(motif).count(),
            0,
            "la découverte rattache une zone à un appareil découvert SANS sa MAC (`{motif}`) : \
             le même appareil perdrait son trim, et rien ne prouve qu'un autre est le même (#6774)"
        );
    }
    let avec_mac = source
        .matches(concat!("mac:dev.mac_address", ".as_deref(),"))
        .count();
    assert!(
        avec_mac >= 7,
        "sept rattachements de la découverte doivent porter la MAC de l'appareil, vu {avec_mac}"
    );
}

/// #6774 suite — un PATCH qui change d'appareil ET réaffirme
/// `fixed_volume: true` sans `confirm_full_volume` ne réarme pas en douce le
/// volume fixe que le changement d'appareil vient de désarmer ; l'état de
/// lecture ne reste pas à 100 %.
#[tokio::test]
async fn patch_vers_un_autre_appareil_ne_rearme_pas_le_volume_fixe() {
    let (app, state) = serveur();
    let id = zone_reglee(&app, &state, "uuid:p6774-fixe-a").await;
    let repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    repo.update_volume(id, 100.0).unwrap();
    repo.update_fixed_volume(id, true).unwrap();
    state.playback.set_volume(id, 1.0).await;

    let (statut, reponse) = envoyer(
        &app,
        "PATCH",
        &format!("/api/v1/zones/{id}"),
        json!({ "output_device_id": "uuid:p6774-fixe-b", "fixed_volume": true }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{reponse}");
    let zone = repo.get(id).unwrap().unwrap();
    assert!(
        !zone.fixed_volume,
        "#6774 : le volume fixe de l'ancien appareil a été réarmé sur le nouveau sans confirmation"
    );
    assert_eq!(
        zone.volume, 50.0,
        "le volume repart de la limite de sécurité (50 %)"
    );
    assert!(
        state.playback.get_state(id).await.volume <= 0.5,
        "#6774 : l'état de lecture est resté à 100 % après le changement d'appareil"
    );
}
