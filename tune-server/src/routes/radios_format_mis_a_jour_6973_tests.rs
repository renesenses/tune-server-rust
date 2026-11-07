//! #6973 : vraie route/base, annuaire synthétique, aucun appel radio ni son.
use super::*;
use axum::{body::Body, http::Request};
use tower::ServiceExt;

async fn actualiser(url: &str, format: Option<&str>) -> (Value, RadioStation) {
    let etat = AppState::new(":memory:", 0, Default::default()).unwrap();
    let fiche = json!({"external_id":"S6973", "name":"Station témoin", "status":"active",
        "stream_url":"https://radio.example/ogg.flac", "streams":[
            {"url":"https://radio.example/ogg.flac", "format":format,
             "bitrate":format.map(|_|1536), "is_primary":true,"is_published":true},
            {"url":"https://radio.example/secondaire.aac","format":"AAC",
             "bitrate":128,"is_primary":false,"is_published":true}]});
    *etat.annuaire_radios.write().unwrap() = annuaire_depuis_json(&[fiche]);
    let repo = RadioRepo::with_backend(etat.backend.clone());
    let id = repo
        .create(&RadioStation {
            id: None,
            name: "Station témoin".into(),
            url: url.into(),
            homepage: None,
            logo_url: None,
            country: None,
            language: None,
            genre: None,
            codec: Some("AAC".into()),
            bitrate: Some(128),
            is_favorite: true,
            last_played: None,
            play_count: 0,
        })
        .unwrap();
    repo.lier_a_l_annuaire(id, Some("S6973")).unwrap();
    let reponse = crate::routes::router(etat)
        .oneshot(
            Request::post(format!("/api/v1/radios/directory/update-stream/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reponse.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        serde_json::from_slice(&bytes).unwrap(),
        repo.get(id).unwrap().unwrap(),
    )
}

#[tokio::test]
async fn changer_d_adresse_remplace_aussi_codec_et_debit_6973() {
    let (corps, station) = actualiser("https://radio.example/ancien", Some("FLAC")).await;
    assert_eq!(station.url, "https://radio.example/ogg.flac");
    assert_eq!(
        station.codec.as_deref(),
        Some("flac"),
        "#6973 : l'ancien AAC ne doit pas survivre au nouveau flux FLAC : {corps}"
    );
    assert_eq!(station.bitrate, Some(1536));
    assert_eq!(corps["codec"], "flac");
    assert_eq!(corps["bitrate"], 1536);
    assert!(station.is_favorite, "le choix favori doit rester conservé");
}

#[tokio::test]
async fn un_flux_secondaire_encore_publie_garde_son_format_6973() {
    let (_, station) = actualiser("https://radio.example/secondaire.aac", Some("FLAC")).await;
    assert_eq!(station.url, "https://radio.example/secondaire.aac");
    assert_eq!(station.codec.as_deref(), Some("AAC"));
    assert_eq!(station.bitrate, Some(128));
}

#[tokio::test]
async fn le_nouveau_flux_sans_format_ne_reprend_pas_l_ancien_6973() {
    let (_, station) = actualiser("https://radio.example/ancien", None).await;
    assert_eq!(station.url, "https://radio.example/ogg.flac");
    assert_eq!(
        station.codec, None,
        "#6973 : le nouveau codec inconnu ne doit pas hériter de l'AAC précédent"
    );
    assert_eq!(station.bitrate, None);
}
