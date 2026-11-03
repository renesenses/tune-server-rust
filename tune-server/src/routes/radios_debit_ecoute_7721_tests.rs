//! #7721 — « demandé en 320, reçu en 97 ». Fiche synthétique sur le modèle
//! d'une station qui réserve son flux 320 kbit/s au Royaume-Uni et publie un
//! flux mondial à 96 kbit/s. Aucun accès réseau ni sortie audio.
use super::*;
use crate::routes::{radios::annuaire_depuis_json, radios_annuaire::station_depuis_fiche};
use axum::{body::Body, http::Request};
use tower::ServiceExt;

const FLUX_GB: &str = "https://radio.example/uk_320.m3u8";
const FLUX_MONDE: &str = "https://radio.example/ww_96.m3u8";

fn document() -> Value {
    json!({
        "external_id": "S7721", "name": "Station témoin 7721", "country": "GB",
        "stream_url": FLUX_GB,
        "streams": [
            {"url": FLUX_GB, "format": "AAC", "bitrate": 320, "is_hls": true,
             "is_primary": true, "is_published": true, "restricted_to_countries": ["GB"]},
            {"url": FLUX_MONDE, "format": "AAC", "bitrate": 96, "is_hls": true,
             "is_published": true, "restricted_to_countries": null}
        ]
    })
}

/// Un serveur dont l'annuaire porte la fiche, et une station de « Mes radios »
/// liée à elle qui a gardé l'adresse du flux 320 réservé au Royaume-Uni.
fn station_sur_le_flux_gb(pays: &str) -> (AppState, RadioStation) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    *state.annuaire_radios.write().unwrap() = annuaire_depuis_json(&[document()]);
    SettingsRepo::with_backend(state.backend.clone())
        .set(COUNTRY_SETTING, pays)
        .unwrap();
    let repo = RadioRepo::with_backend(state.backend.clone());
    let mut radio = station_depuis_fiche(&state.annuaire_radios.read().unwrap()[0]);
    assert_eq!((radio.url.as_str(), radio.bitrate), (FLUX_GB, Some(320)));
    radio.id = Some(repo.create(&radio).unwrap());
    repo.lier_a_l_annuaire(radio.id.unwrap(), Some("S7721"))
        .unwrap();
    (state, radio)
}

async fn get(app: &axum::Router, path: &str) -> Value {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "{path}: {}",
        response.status()
    );
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn debit_affiche_est_celui_du_flux_joue_hors_du_pays_reserve_7721() {
    let (state, radio) = station_sur_le_flux_gb("FR");
    let id = radio.id.unwrap();
    let joue = stream_for_saved(&state, &radio).unwrap();
    assert_eq!(
        joue, FLUX_MONDE,
        "la lecture écarte le flux réservé au Royaume-Uni"
    );

    let app = crate::routes::router(state.clone());
    let station = get(&app, &format!("/api/v1/radios/{id}")).await;
    let liste = get(&app, "/api/v1/radios").await;
    let dans_la_liste = liste
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == id)
        .unwrap()
        .clone();
    for (route, corps) in [
        ("GET /radios/{id}", station),
        ("GET /radios", dans_la_liste),
    ] {
        assert_eq!(
            corps["bitrate"], 96,
            "{route} : le débit affiché doit être celui du flux joué ({joue}), \
             pas les 320 kbit/s du flux enregistré réservé au Royaume-Uni : {corps}"
        );
        assert_eq!(corps["bitrate_enregistre"], 320, "{route}: {corps}");
        assert_eq!(corps["flux_ecoute"]["url"], joue, "{route}: {corps}");
        assert_eq!(corps["flux_ecoute"]["bitrate"], 96, "{route}: {corps}");
        assert_eq!(
            corps["flux_ecoute"]["raison"], "restreint_au_pays",
            "{route}"
        );
        assert_eq!(corps["flux_ecoute"]["pays"], "FR", "{route}");
        assert_eq!(
            corps["stream_url"], FLUX_GB,
            "l'adresse enregistrée reste visible"
        );
    }
    assert_eq!(
        RadioRepo::with_backend(state.backend.clone())
            .get(id)
            .unwrap()
            .unwrap()
            .bitrate,
        Some(320),
        "l'affichage n'écrit rien en base"
    );
}

#[tokio::test]
async fn flux_demande_autorise_garde_son_debit_7721() {
    let (state, radio) = station_sur_le_flux_gb("GB");
    assert_eq!(stream_for_saved(&state, &radio).unwrap(), FLUX_GB);
    let app = crate::routes::router(state);
    let corps = get(&app, &format!("/api/v1/radios/{}", radio.id.unwrap())).await;
    assert_eq!(corps["bitrate"], 320, "{corps}");
    assert!(corps.get("flux_ecoute").is_none(), "{corps}");
    assert!(corps.get("bitrate_enregistre").is_none(), "{corps}");
}

#[tokio::test]
async fn choisir_le_flux_reserve_annonce_le_debit_reellement_joue_7721() {
    let (state, radio) = station_sur_le_flux_gb("FR");
    let id = radio.id.unwrap();
    let app = crate::routes::router(state);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/radios/directory/preferred-stream/{id}"))
                .header("content-type", "application/json")
                .body(Body::from(json!({"stream_url": FLUX_GB}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let corps: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        corps["bitrate"], 96,
        "le flux 320 choisi n'est pas jouable en France : le débit annoncé est \
         celui du flux joué : {corps}"
    );
    assert_eq!(corps["flux_ecoute"]["url"], FLUX_MONDE, "{corps}");
}
