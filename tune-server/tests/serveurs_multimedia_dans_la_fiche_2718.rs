//! #2718 — « plus de serveurs multimédia » : la fiche support et le rapport de
//! bogue doivent NOMMER le registre dont le testeur signale la disparition.
//!
//! Quatre tickets support (61 du 28/08, 87 du 07/09, 97 et 98 du 08/09/2026,
//! Belkadi Yacine) disent tous « plus de serveurs multimédia » ou « serveur
//! multimédia inactif ». Les quatre portent une fiche système et trois portent
//! un rapport de bogue complet. **Aucune des deux pièces ne comptait les
//! serveurs multimédia.** Elles décrivaient les zones jusqu'à la marque et au
//! modèle du DAC, et la section « Network » se limitait à
//! `Discovered devices`, qui ne compte QUE les renderers — les serveurs
//! multimédia vivent dans un autre registre (`AppState::media_servers`,
//! alimenté par `SsdpEvent::MediaServerDiscovered`).
//!
//! #2718 s'est refermée « mécanisme non établi » : la réponse tenait dans un
//! compteur que personne n'avait écrit.
//!
//! Le témoin passe par les ROUTES MONTÉES — `tune_server::routes::router` —
//! et pas par les deux fonctions en direct : « écrit mais pas branché » est le
//! défaut que ce dépôt connaît par cœur.
//!
//! ⚠️ `autotests = false` dans `tune-server/Cargo.toml` : sans l'entrée
//! `[[test]]` correspondante, ce fichier ne serait JAMAIS compilé et cette
//! garde serait verte contre rien.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::time::{Duration, Instant};
use tower::ServiceExt;
use tune_core::discovery::ssdp::MediaServerInfo;
use tune_server::state::AppState;

async fn obtenir(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// La Freebox de Belkadi Yacine, telle que son journal du ticket 61 la montre
/// enregistrée : `ssdp_media_server_discovered … name=Freebox Server
/// cd_url=http://192.168.0.254:52424/service/ContentDirectory/control`.
fn freebox() -> MediaServerInfo {
    MediaServerInfo {
        id: "uuid:75823e36-eeac-3aee-a862-d2fad309bff6".into(),
        name: "Freebox Server".into(),
        manufacturer: "Freebox SA".into(),
        model: "Freebox Server".into(),
        location: "http://192.168.0.254:52424/device.xml".into(),
        content_directory_url: "http://192.168.0.254:52424/service/ContentDirectory/control".into(),
        host: "192.168.0.254".into(),
        port: 52424,
        last_seen: Instant::now(),
        max_age: Duration::from_secs(1800),
    }
}

/// Les deux pièces dans la MÊME épreuve : ce sont les deux que le testeur
/// joint, et elles doivent s'accorder. Une seule des deux corrigée laisserait
/// l'instruction repartir de la pièce muette.
#[tokio::test]
async fn la_fiche_et_le_rapport_comptent_les_serveurs_multimedia() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state.clone());

    // ---- 1. Témoin d'origine : registre vide. Le compte doit être ÉCRIT, pas
    // absent — une clé absente et une liste vide se lisent pareil à l'œil, et
    // c'est exactement l'ambiguïté qui a coûté #2718.
    let (code, fiche) = obtenir(&app, "/api/v1/system/profile").await;
    assert_eq!(code, StatusCode::OK, "fiche support : {fiche}");
    assert_eq!(
        fiche["network"]["media_servers_count"], 0,
        "registre vide : la fiche doit l'écrire, et non se taire — c'est cette \
         absence-là qui a fait refermer #2718 « mécanisme non établi ».\n{fiche}"
    );

    // ---- 2. Un serveur multimédia connu, celui du testeur.
    state
        .media_servers
        .lock()
        .await
        .insert(freebox().id.clone(), freebox());

    let (code, fiche) = obtenir(&app, "/api/v1/system/profile").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        fiche["network"]["media_servers_count"], 1,
        "la fiche support ne compte pas le serveur multimédia connu : {fiche}"
    );
    assert_eq!(
        fiche["network"]["media_servers"][0]["name"], "Freebox Server",
        "la fiche doit NOMMER le serveur : un compte seul ne dit pas LEQUEL a \
         disparu entre deux rapports.\n{fiche}"
    );
    assert_eq!(
        fiche["network"]["media_servers"][0]["host"], "192.168.0.254",
        "sans l'hôte, on ne sait pas si c'est la Freebox, le PC ou Tune \
         lui-même.\n{fiche}"
    );

    // ---- 3. Le rapport de bogue — l'autre pièce jointe, celle que le testeur
    // colle aussi sur le forum.
    let (code, rapport) = obtenir(&app, "/api/v1/system/bug-report").await;
    assert_eq!(code, StatusCode::OK);
    let md = rapport["markdown"]
        .as_str()
        .unwrap_or_else(|| panic!("le rapport n'a pas de markdown : {rapport}"));
    assert!(
        md.contains("- Serveurs multimedia: 1"),
        "la section « Network » du rapport de bogue ne compte toujours pas les \
         serveurs multimédia. « Discovered devices » ne compte QUE les \
         renderers : les quatre rapports du testeur ont été lus sans que la \
         liste dont il signalait la disparition y figure une seule fois.\n\
         section Network obtenue :\n{}",
        md.split("## Network").nth(1).unwrap_or("<absente>")
    );
    assert!(
        md.contains("Freebox Server") && md.contains("192.168.0.254"),
        "le rapport compte sans nommer : il faut savoir LEQUEL manque.\n\
         section Network obtenue :\n{}",
        md.split("## Network").nth(1).unwrap_or("<absente>")
    );
}
