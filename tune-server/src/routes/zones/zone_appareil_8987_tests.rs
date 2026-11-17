//! #8987 (fil 2708) — témoins de la règle et des trois routes.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;

use super::*;

fn serveur() -> (axum::Router, AppState) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
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

// ── La règle ───────────────────────────────────────────────────────────────

#[test]
fn table_de_l_occupation() {
    assert_eq!(occupation(7, None), OccupationDeLaSortie::Libre);
    assert_eq!(
        occupation(7, Some((7, false, "Moi".into()))),
        OccupationDeLaSortie::CetteZone
    );
    assert_eq!(
        occupation(7, Some((6, true, "Supprimée".into()))),
        OccupationDeLaSortie::ZoneSupprimee(6)
    );
    assert_eq!(
        occupation(7, Some((1, false, "DENAFRIPS DAC USB".into()))),
        OccupationDeLaSortie::ZoneActive {
            id: 1,
            nom: "DENAFRIPS DAC USB".into()
        }
    );
}

#[test]
fn l_hote_de_la_sortie_se_lit_au_registre_puis_a_l_endpoint() {
    // Le registre fait foi.
    assert_eq!(sortie_en_asio(Some("asio"), Some("wasapi:{x}")), Some(true));
    assert_eq!(
        sortie_en_asio(Some("wasapi"), Some("asio:DAC")),
        Some(false)
    );
    assert_eq!(sortie_en_asio(Some("auto"), None), Some(false));
    // Sortie absente du registre : le préfixe de l'endpoint enregistré.
    assert_eq!(
        sortie_en_asio(None, Some("asio:DENAFRIPS USB DAC")),
        Some(true)
    );
    assert_eq!(
        sortie_en_asio(None, Some("wasapi:{0.0.0.00000000}.{g}")),
        Some(false)
    );
    assert_eq!(
        sortie_en_asio(None, Some("alsa:hw:CARD=2,DEV=0")),
        Some(false)
    );
    // Rien de connu : rien à dire.
    assert_eq!(sortie_en_asio(None, None), None);
    assert_eq!(sortie_en_asio(None, Some("bizarre")), None);
}

#[test]
fn seule_la_frontiere_asio_compte() {
    assert!(
        restauree_hors_hote_actif("wasapi", Some(true)),
        "zone ASIO sous WASAPI"
    );
    assert!(
        restauree_hors_hote_actif("auto", Some(true)),
        "zone ASIO sous auto"
    );
    assert!(
        restauree_hors_hote_actif("asio", Some(false)),
        "zone WASAPI sous ASIO"
    );
    assert!(!restauree_hors_hote_actif("asio", Some(true)));
    assert!(!restauree_hors_hote_actif("wasapi", Some(false)));
    assert!(
        !restauree_hors_hote_actif("ASIO", None),
        "hôte inconnu : silence"
    );
}

// ── 1. « Nouvelle zone » sur un appareil qui a déjà une zone ───────────────

#[tokio::test]
async fn nouvelle_zone_sur_un_appareil_deja_pris_rend_409_qui_nomme_la_zone() {
    let (app, state) = serveur();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let id = repo
        .create(
            "DENAFRIPS DAC USB",
            Some("local"),
            Some("local:Haut-parleurs"),
        )
        .unwrap();

    let (statut, corps) = envoyer(
        &app,
        "POST",
        "/api/v1/zones",
        json!({"name": "Haut-parleurs PC", "output_type": "local", "output_device_id": "local:Haut-parleurs"}),
    )
    .await;
    assert_eq!(statut, StatusCode::CONFLICT, "{corps}");
    assert_eq!(corps["code"], "zone_existe_deja", "{corps}");
    assert_eq!(corps["zone_id"], id, "{corps}");
    assert_eq!(corps["zone_name"], "DENAFRIPS DAC USB", "{corps}");
    assert_eq!(corps["message_key"], "api.zones.deviceAlreadyHasZone");
    assert!(
        corps["message"]
            .as_str()
            .unwrap()
            .contains("DENAFRIPS DAC USB"),
        "la phrase nomme la zone existante : {corps}"
    );
    // Rien n'a bougé.
    assert_eq!(repo.get(id).unwrap().unwrap().name, "DENAFRIPS DAC USB");
    assert_eq!(repo.list().unwrap().len(), 1);
}

#[tokio::test]
async fn nouvelle_zone_sur_un_appareil_d_une_zone_supprimee_la_ressuscite() {
    let (app, state) = serveur();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let id = repo
        .create("Ancienne", Some("local"), Some("local:Haut-parleurs"))
        .unwrap();
    repo.delete(id).unwrap();
    let (statut, corps) = envoyer(
        &app,
        "POST",
        "/api/v1/zones",
        json!({"name": "Nouvelle", "output_type": "local", "output_device_id": "local:Haut-parleurs"}),
    )
    .await;
    assert_eq!(
        statut,
        StatusCode::OK,
        "une zone supprimée se ressuscite : {corps}"
    );
    assert_eq!(corps["id"], id);
    assert_eq!(repo.get(id).unwrap().unwrap().name, "Nouvelle");
    assert!(!repo.is_device_hidden("local:Haut-parleurs"));
}

// ── 2. Changer l'appareil d'une zone ───────────────────────────────────────

#[tokio::test]
async fn changer_vers_l_appareil_d_une_zone_supprimee_la_libere_au_lieu_du_500() {
    let (app, state) = serveur();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let supprimee = repo
        .create(
            "DENAFRIPS USB DAC",
            Some("local"),
            Some("local:DENAFRIPS USB DAC"),
        )
        .unwrap();
    repo.delete(supprimee).unwrap();
    let zone = repo
        .create("Bureau", Some("local"), Some("local:Haut-parleurs"))
        .unwrap();

    let (statut, corps) = envoyer(
        &app,
        "PATCH",
        &format!("/api/v1/zones/{zone}"),
        json!({"output_device_id": "local:DENAFRIPS USB DAC"}),
    )
    .await;
    assert!(
        statut.is_success(),
        "la zone supprimée lâche la sortie, pas de 500 UNIQUE : {statut} {corps}"
    );
    assert_eq!(
        repo.get(zone).unwrap().unwrap().output_device_id.as_deref(),
        Some("local:DENAFRIPS USB DAC")
    );
    let etat = repo.etat_de_masquage(supprimee).unwrap().unwrap();
    assert!(etat.masquee, "la zone supprimée le reste");
    assert_eq!(repo.get(supprimee).unwrap().unwrap().output_device_id, None);
}

#[tokio::test]
async fn changer_vers_l_appareil_d_une_zone_active_rend_409_qui_la_nomme() {
    let (app, state) = serveur();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let tenante = repo
        .create("Salon DAC", Some("local"), Some("local:DENAFRIPS USB DAC"))
        .unwrap();
    let zone = repo
        .create("Bureau", Some("local"), Some("local:Haut-parleurs"))
        .unwrap();

    let (statut, corps) = envoyer(
        &app,
        "PATCH",
        &format!("/api/v1/zones/{zone}"),
        json!({"output_device_id": "local:DENAFRIPS USB DAC", "name": "Bureau 2"}),
    )
    .await;
    assert_eq!(statut, StatusCode::CONFLICT, "{corps}");
    assert_eq!(corps["code"], "sortie_tenue_par_une_zone", "{corps}");
    assert_eq!(corps["holder_zone_id"], tenante, "{corps}");
    assert_eq!(corps["holder_zone_name"], "Salon DAC", "{corps}");
    assert!(
        corps["message"].as_str().unwrap().contains("Salon DAC"),
        "{corps}"
    );
    // Refusé AVANT toute écriture : le nom n'a pas bougé non plus.
    let z = repo.get(zone).unwrap().unwrap();
    assert_eq!(z.output_device_id.as_deref(), Some("local:Haut-parleurs"));
    assert_eq!(z.name, "Bureau");
    assert_eq!(
        repo.get(tenante)
            .unwrap()
            .unwrap()
            .output_device_id
            .as_deref(),
        Some("local:DENAFRIPS USB DAC")
    );
}

/// La seconde porte du changement d'appareil (`zone-manager` hot-swap) suit
/// la même règle : zone supprimée libérée, zone active nommée en 409.
#[tokio::test]
async fn le_hot_swap_suit_la_meme_regle() {
    let (app, state) = serveur();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let supprimee = repo
        .create("Ancienne", Some("local"), Some("local:DAC A"))
        .unwrap();
    repo.delete(supprimee).unwrap();
    let active = repo
        .create("Salon", Some("local"), Some("local:DAC B"))
        .unwrap();
    let zone = repo
        .create("Bureau", Some("local"), Some("local:HP"))
        .unwrap();

    let (statut, corps) = envoyer(
        &app,
        "POST",
        &format!("/api/v1/zone-manager/zones/{zone}/hot-swap"),
        json!({"output_device_id": "local:DAC A"}),
    )
    .await;
    assert!(statut.is_success(), "{statut} {corps}");
    assert_eq!(
        repo.get(zone).unwrap().unwrap().output_device_id.as_deref(),
        Some("local:DAC A")
    );

    let (statut, corps) = envoyer(
        &app,
        "POST",
        &format!("/api/v1/zone-manager/zones/{zone}/hot-swap"),
        json!({"output_device_id": "local:DAC B"}),
    )
    .await;
    assert_eq!(statut, StatusCode::CONFLICT, "{corps}");
    assert_eq!(corps["holder_zone_id"], active, "{corps}");
    assert_eq!(
        repo.get(zone).unwrap().unwrap().output_device_id.as_deref(),
        Some("local:DAC A")
    );
}

// ── 3. Restauration sous un autre hôte audio ───────────────────────────────

async fn restaurer_zone_asio(backend_configure: &str) -> (StatusCode, Value) {
    let (app, state) = serveur();
    SettingsRepo::with_backend(state.backend.clone())
        .set("local_audio_backend", backend_configure)
        .unwrap();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let id = repo
        .create(
            "DENAFRIPS USB DAC",
            Some("local"),
            Some("local:DENAFRIPS USB DAC"),
        )
        .unwrap();
    repo.update_output_endpoint_id(id, "asio:DENAFRIPS USB DAC")
        .unwrap();
    repo.delete(id).unwrap();
    envoyer(
        &app,
        "POST",
        &format!("/api/v1/zones/{id}/restaurer"),
        json!({}),
    )
    .await
}

#[tokio::test]
async fn une_zone_asio_restauree_sous_wasapi_est_signalee() {
    let (statut, corps) = restaurer_zone_asio("wasapi").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let a = &corps["avertissement_restauration"];
    assert_eq!(a["code"], "zone_restauree_autre_hote_audio", "{corps}");
    assert_eq!(a["hote_de_la_zone"], "ASIO");
    assert_eq!(a["hote_actif"], "WASAPI");
    assert_eq!(a["message_key"], "api.zones.restoredOnOtherAudioHost");
    let m = a["message"].as_str().unwrap();
    assert!(m.contains("ASIO") && m.contains("WASAPI"), "{m}");
}

/// Contre-épreuve : même zone, Tune réglé en ASIO — rien à signaler.
#[tokio::test]
async fn une_zone_asio_restauree_sous_asio_n_est_pas_signalee() {
    let (statut, corps) = restaurer_zone_asio("asio").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert!(corps.get("avertissement_restauration").is_none(), "{corps}");
}
