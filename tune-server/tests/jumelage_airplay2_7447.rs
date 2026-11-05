//! #7447 — le bouton « Pair » d'Audio / Network Devices, éprouvé par le VRAI
//! routeur contre un faux démon AirPlay 2 (aucun récepteur réel, aucun son).
//!
//! Le client web envoie `POST /devices/{id}/pair` SANS corps ni
//! `Content-Type`, sur un identifiant `airplay-…` donné par la découverte.
//! Avant le correctif : refus 415 avant la route, chemin AirPlay 2 réservé à
//! un préfixe `airplay2:` jamais donné, et sinon un faux 201 « paired ».
//! Ces témoins exigent que le jumelage parvienne au composant AirPlay 2 et
//! que chaque échec soit nommé.
//!
//! Binaire à lui seul : le registre des programmes substitués
//! (`tune_core::services_factices`) est un état de PROCESSUS ; les tests qui
//! s'en servent se suivent sous [`SUBSTITUTIONS`].
#![cfg(target_os = "linux")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::path::Path;
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::test_scratch::ScratchDir;
use tune_server::state::AppState;

/// Identifiant d'une sortie AirPlay tel que la découverte mDNS le forme.
const APPAREIL: &str = "airplay-a1b2c3d4e5f6";

static SUBSTITUTIONS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn etat(dossier: &ScratchDir) -> AppState {
    let db = dossier.join("tune.db");
    let etat = AppState::new(db.to_str().unwrap(), 0, Default::default()).expect("état isolé");
    SettingsRepo::with_backend(etat.backend.clone())
        .set(tune_core::cloud::telemetry::TELEMETRY_SETTING_KEY, "false")
        .expect("télémétrie coupée");
    etat
}

async fn appeler(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(methode).uri(chemin);
    let body = match corps {
        Some(c) => {
            req = req.header("content-type", "application/json");
            Body::from(c.to_string())
        }
        // Comme `api.beginPairing` du client web : ni corps ni Content-Type.
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let statut = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let valeur = serde_json::from_slice(&octets)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&octets).into_owned()));
    (statut, valeur)
}

/// Faux `airplay-daemon` : annonce `ready`, puis répond ligne à ligne selon
/// `reponses` (un `case` shell sur la commande reçue).
fn faux_demon(dossier: &Path, reponses: &str) {
    use std::os::unix::fs::PermissionsExt;
    let chemin = dossier.join("faux-airplay-daemon");
    let script = format!(
        "#!/bin/sh\necho '{{\"event\":\"ready\"}}'\nwhile IFS= read -r ligne; do\n  case \"$ligne\" in\n{reponses}\n  esac\ndone\n"
    );
    std::fs::write(&chemin, script).unwrap();
    std::fs::set_permissions(&chemin, std::fs::Permissions::from_mode(0o755)).unwrap();
    tune_core::services_factices::substituer_programme("airplay-daemon", chemin.to_str().unwrap());
}

/// Un récepteur qui affiche un code : 1234 est le bon.
const AVEC_CODE: &str = r#"    *'"pair_pin_start"'*) echo '{"event":"pin_requested"}' ;;
    *'"pin":"1234"'*) echo '{"event":"connected"}' ;;
    *'"connect"'*) echo '{"event":"error","message":"pair-verify refused: wrong PIN"}' ;;"#;

/// Une enceinte sans écran : refuse le jumelage par code, accepte le
/// jumelage transitoire.
const SANS_CODE: &str = r#"    *'"pair_pin_start"'*) echo '{"event":"error","message":"pair-pin-start refused by receiver"}' ;;
    *'"pin":"3939"'*) echo '{"event":"connected"}' ;;
    *'"connect"'*) echo '{"event":"error","message":"unexpected PIN"}' ;;"#;

/// Un récepteur qui refuse tout.
const REFUSE_TOUT: &str = r#"    *'"pair_pin_start"'*) echo '{"event":"error","message":"pair-pin-start refused by receiver"}' ;;
    *'"connect"'*) echo '{"event":"error","message":"pair-setup refused: authentication required"}' ;;"#;

/// Un récepteur muet : le démon démarre, le récepteur ne répond jamais.
const MUET: &str = r#"    *) : ;;"#;

async fn etat_avec_airplay2(dossier: &ScratchDir) -> AppState {
    let etat = etat(dossier);
    let sortie = tune_core::outputs::airplay2::Airplay2Output::new(
        "Enceinte témoin".into(),
        "127.0.0.1".into(),
        7000,
        APPAREIL.into(),
        "A1:B2:C3:D4:E5:F6".into(),
    );
    etat.outputs.lock().await.register(Box::new(sortie));
    etat
}

fn chemin_pair() -> String {
    format!("/api/v1/devices/{APPAREIL}/pair")
}

fn chemin_pin() -> String {
    format!("/api/v1/devices/{APPAREIL}/pair/pin")
}

fn aucune_trace_de_faux_jumelage(etat: &AppState) {
    let settings = SettingsRepo::with_backend(etat.backend.clone());
    assert_eq!(
        settings
            .get(&format!("device_pair_{APPAREIL}"))
            .ok()
            .flatten(),
        None,
        "plus de réglage `device_pair_*` que rien ne relit"
    );
}

#[tokio::test]
async fn pair_sans_corps_demande_le_code_au_recepteur_puis_le_jumelle_7447() {
    let _une_a_la_fois = SUBSTITUTIONS.lock().await;
    let dossier = tune_core::test_scratch::scratch_dir("tune-7447-code");
    faux_demon(&dossier, AVEC_CODE);
    let etat = etat_avec_airplay2(&dossier).await;
    let app = tune_server::routes::router(etat.clone());

    let (statut, corps) = appeler(&app, "POST", &chemin_pair(), None).await;
    assert_eq!(
        statut,
        StatusCode::OK,
        "Pair sans corps doit atteindre le composant AirPlay 2 : {corps}"
    );
    assert_eq!(
        corps["status"], "awaiting_pin",
        "le récepteur affiche un code : {corps}"
    );

    let (statut, corps) =
        appeler(&app, "POST", &chemin_pin(), Some(json!({"pin": " 1234 "}))).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["status"], "paired", "{corps}");
    aucune_trace_de_faux_jumelage(&etat);
}

#[tokio::test]
async fn un_mauvais_code_est_refuse_par_le_recepteur_et_le_dit_7447() {
    let _une_a_la_fois = SUBSTITUTIONS.lock().await;
    let dossier = tune_core::test_scratch::scratch_dir("tune-7447-mauvais-code");
    faux_demon(&dossier, AVEC_CODE);
    let etat = etat_avec_airplay2(&dossier).await;
    let app = tune_server::routes::router(etat.clone());

    let (statut, corps) = appeler(&app, "POST", &chemin_pair(), None).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let (statut, corps) = appeler(&app, "POST", &chemin_pin(), Some(json!({"pin": "0000"}))).await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "un code faux ne doit jamais donner « paired » : {corps}"
    );
    assert_eq!(corps["code"], "airplay2_pairing_failed", "{corps}");
    assert!(
        corps["error"]
            .as_str()
            .unwrap_or_default()
            .contains("wrong PIN"),
        "la cause donnée par le récepteur doit être rendue : {corps}"
    );
    aucune_trace_de_faux_jumelage(&etat);
}

#[tokio::test]
async fn une_enceinte_sans_ecran_est_jumelee_sans_code_7447() {
    let _une_a_la_fois = SUBSTITUTIONS.lock().await;
    let dossier = tune_core::test_scratch::scratch_dir("tune-7447-sans-code");
    faux_demon(&dossier, SANS_CODE);
    let app = tune_server::routes::router(etat_avec_airplay2(&dossier).await);

    let (statut, corps) = appeler(&app, "POST", &chemin_pair(), None).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["status"], "paired", "{corps}");
    assert_eq!(corps["pin_required"], false, "{corps}");
}

#[tokio::test]
async fn un_recepteur_qui_refuse_tout_nomme_sa_cause_au_lieu_d_un_faux_succes_7447() {
    let _une_a_la_fois = SUBSTITUTIONS.lock().await;
    let dossier = tune_core::test_scratch::scratch_dir("tune-7447-refus");
    faux_demon(&dossier, REFUSE_TOUT);
    let etat = etat_avec_airplay2(&dossier).await;
    let app = tune_server::routes::router(etat.clone());

    let (statut, corps) = appeler(&app, "POST", &chemin_pair(), None).await;
    assert_eq!(
        statut,
        StatusCode::BAD_GATEWAY,
        "un refus du récepteur n'est jamais un succès : {corps}"
    );
    assert_eq!(corps["code"], "airplay2_pairing_failed", "{corps}");
    let message = corps["error"].as_str().unwrap_or_default();
    assert!(
        message.contains("authentication required"),
        "le message doit dire ce que le récepteur a répondu : {corps}"
    );
    aucune_trace_de_faux_jumelage(&etat);
}

#[tokio::test]
async fn un_recepteur_muet_rend_un_delai_depasse_nomme_7447() {
    let _une_a_la_fois = SUBSTITUTIONS.lock().await;
    let dossier = tune_core::test_scratch::scratch_dir("tune-7447-muet");
    faux_demon(&dossier, MUET);
    let app = tune_server::routes::router(etat_avec_airplay2(&dossier).await);

    let (statut, corps) = appeler(&app, "POST", &chemin_pair(), None).await;
    assert_eq!(statut, StatusCode::GATEWAY_TIMEOUT, "{corps}");
    assert_eq!(corps["code"], "airplay2_pairing_timeout", "{corps}");
}

#[tokio::test]
async fn le_composant_airplay2_absent_est_nomme_7447() {
    let _une_a_la_fois = SUBSTITUTIONS.lock().await;
    let dossier = tune_core::test_scratch::scratch_dir("tune-7447-absent");
    tune_core::services_factices::substituer_programme(
        "airplay-daemon",
        dossier.join("airplay-daemon-absent").to_str().unwrap(),
    );
    let app = tune_server::routes::router(etat_avec_airplay2(&dossier).await);

    let (statut, corps) = appeler(&app, "POST", &chemin_pair(), None).await;
    assert_eq!(statut, StatusCode::SERVICE_UNAVAILABLE, "{corps}");
    assert_eq!(corps["code"], "airplay2_component_missing", "{corps}");
}

#[tokio::test]
async fn un_appareil_servi_en_airplay1_dit_pourquoi_il_ne_peut_etre_jumele_7447() {
    let dossier = tune_core::test_scratch::scratch_dir("tune-7447-airplay1");
    let etat = etat(&dossier);
    let exige = tune_core::outputs::airplay::AirplayOutput::new(
        "Téléviseur sans identifiant".into(),
        "airplay-tv-7000".into(),
        "127.0.0.1".into(),
        7000,
    )
    .avec_appairage_airplay2_requis(true);
    let ordinaire = tune_core::outputs::airplay::AirplayOutput::new(
        "Borne AirPlay 1".into(),
        "airplay-borne-5000".into(),
        "127.0.0.1".into(),
        5000,
    );
    {
        let mut registre = etat.outputs.lock().await;
        registre.register(Box::new(exige));
        registre.register(Box::new(ordinaire));
    }
    let app = tune_server::routes::router(etat.clone());

    let (statut, corps) = appeler(&app, "POST", "/api/v1/devices/airplay-tv-7000/pair", None).await;
    assert_eq!(statut, StatusCode::CONFLICT, "{corps}");
    assert_eq!(corps["code"], "airplay2_pairing_unavailable", "{corps}");

    let (statut, corps) = appeler(
        &app,
        "POST",
        "/api/v1/devices/airplay-borne-5000/pair",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::CONFLICT, "{corps}");
    assert_eq!(corps["code"], "pairing_not_supported", "{corps}");

    let (statut, corps) = appeler(&app, "POST", "/api/v1/devices/airplay-inconnu/pair", None).await;
    assert_eq!(statut, StatusCode::NOT_FOUND, "{corps}");
    assert_eq!(corps["code"], "device_not_found", "{corps}");

    let settings = SettingsRepo::with_backend(etat.backend.clone());
    for id in ["airplay-tv-7000", "airplay-borne-5000", "airplay-inconnu"] {
        assert_eq!(
            settings.get(&format!("device_pair_{id}")).ok().flatten(),
            None,
            "aucun faux jumelage enregistré pour {id}"
        );
    }
}

#[tokio::test]
async fn une_zone_se_cree_sur_un_recepteur_airplay2_decouvert_7447() {
    let dossier = tune_core::test_scratch::scratch_dir("tune-7447-zone");
    let app = tune_server::routes::router(etat_avec_airplay2(&dossier).await);

    for (nom, genre) in [("Salon témoin", "airplay2"), ("Bureau témoin", "airplay")] {
        let (statut, corps) = appeler(
            &app,
            "POST",
            "/api/v1/zones",
            Some(json!({"name": nom, "output_type": genre, "output_device_id": APPAREIL})),
        )
        .await;
        assert!(
            statut.is_success(),
            "la zone {genre} sur {APPAREIL} doit se créer : {statut} {corps}"
        );
        assert!(corps["id"].as_i64().is_some(), "{corps}");
    }
}
