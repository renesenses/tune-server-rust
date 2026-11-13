//! Discussion #8273, point 3 — la sonde d'une adresse de station collée.
//!
//! Aucun réseau réel : chaque station est un petit serveur HTTP local, joint
//! par le client HTTP PARTAGÉ (`tune_core::http::client::shared`), celui-là
//! même que la route reçoit de l'état.

use super::*;
use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode};
use axum::routing::get as route_get;
use serde_json::{Value, json};
use tower::ServiceExt;

fn client() -> &'static reqwest::Client {
    tune_core::http::client::shared()
}

/// Un faux diffuseur sur 127.0.0.1 ; rend sa base `http://127.0.0.1:port`.
async fn diffuseur() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let b = base.clone();
    let app = axum::Router::new()
        .route(
            "/flux.mp3",
            route_get(|| async {
                (
                    [
                        ("content-type", "audio/mpeg"),
                        ("icy-name", "Radio Témoin"),
                        ("icy-genre", "Jazz"),
                        ("icy-br", "128"),
                        ("icy-url", "https://radio-temoin.example"),
                        ("icy-logo", "https://radio-temoin.example/logo.png"),
                        ("icy-description", "La radio des essais"),
                    ],
                    vec![0u8; 4096],
                )
            }),
        )
        .route(
            "/latin1",
            route_get(|| async {
                let mut h = axum::http::HeaderMap::new();
                h.insert("content-type", HeaderValue::from_static("audio/aacp"));
                // « Musique Française » en Latin-1, comme beaucoup d'Icecast.
                h.insert(
                    "icy-name",
                    HeaderValue::from_bytes(b"Musique Fran\xe7aise").unwrap(),
                );
                h.insert(
                    "ice-audio-info",
                    HeaderValue::from_static("ice-samplerate=48000;ice-bitrate=96;ice-channels=2"),
                );
                (h, vec![0u8; 16])
            }),
        )
        .route(
            "/liste.pls",
            route_get(move || {
                let b = b.clone();
                async move {
                    format!(
                        "[playlist]\nNumberOfEntries=1\nFile1={b}/flux.mp3\nTitle1=Ma Radio PLS\nLength1=-1\n"
                    )
                }
            }),
        )
        .route(
            "/sans-extension",
            route_get({
                let b = base.clone();
                move || {
                    let b = b.clone();
                    async move {
                        (
                            [("content-type", "audio/x-mpegurl")],
                            format!("#EXTM3U\n#EXTINF:-1,Liste M3U\n{b}/latin1\n"),
                        )
                    }
                }
            }),
        )
        .route(
            "/direct.m3u8",
            route_get(|| async {
                (
                    [("content-type", "application/vnd.apple.mpegurl")],
                    "#EXTM3U\n\
                     #EXT-X-STREAM-INF:BANDWIDTH=64000,CODECS=\"mp4a.40.5\"\nbas.m3u8\n\
                     #EXT-X-STREAM-INF:BANDWIDTH=330000,AVERAGE-BANDWIDTH=320000,CODECS=\"mp4a.40.2\"\nhaut.m3u8\n",
                )
            }),
        )
        .route(
            "/page",
            route_get(|| async {
                (
                    [("content-type", "text/html; charset=utf-8")],
                    "<html></html>",
                )
            }),
        )
        .route(
            "/vide.pls",
            route_get(|| async { "[playlist]\nNumberOfEntries=0\n" }),
        );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    base
}

#[tokio::test]
async fn un_flux_icecast_pre_remplit_le_formulaire() {
    let base = diffuseur().await;
    let s = sonder(client(), &format!("{base}/flux.mp3")).await.unwrap();
    assert!(s.reachable);
    assert_eq!(s.stream_url, format!("{base}/flux.mp3"));
    assert_eq!(s.playlist_url, None);
    assert_eq!(s.codec.as_deref(), Some("MP3"));
    assert_eq!(s.bitrate, Some(128));
    assert_eq!(s.name.as_deref(), Some("Radio Témoin"));
    assert_eq!(s.genre.as_deref(), Some("Jazz"));
    assert_eq!(s.description.as_deref(), Some("La radio des essais"));
    assert_eq!(
        s.homepage_url.as_deref(),
        Some("https://radio-temoin.example")
    );
    assert_eq!(
        s.logo_url.as_deref(),
        Some("https://radio-temoin.example/logo.png")
    );
    assert!(!s.hls);
}

#[tokio::test]
async fn un_nom_en_latin1_et_ice_audio_info_sont_lus() {
    let base = diffuseur().await;
    let s = sonder(client(), &format!("{base}/latin1")).await.unwrap();
    assert_eq!(s.name.as_deref(), Some("Musique Française"));
    assert_eq!(s.codec.as_deref(), Some("AAC"));
    assert_eq!(s.bitrate, Some(96));
    assert_eq!(s.sample_rate, Some(48000));
    assert_eq!(s.channels, Some(2));
}

/// Une liste `.pls` est déréférencée : l'adresse à enregistrer est le flux,
/// et c'est lui qui est sondé ; l'en-tête du flux passe avant le titre.
#[tokio::test]
async fn une_liste_pls_est_dereferencee() {
    let base = diffuseur().await;
    let s = sonder(client(), &format!("{base}/liste.pls"))
        .await
        .unwrap();
    assert_eq!(s.playlist_url, Some(format!("{base}/liste.pls")));
    assert_eq!(s.stream_url, format!("{base}/flux.mp3"));
    assert_eq!(s.codec.as_deref(), Some("MP3"));
    assert_eq!(s.name.as_deref(), Some("Radio Témoin"));
}

/// Une liste reconnue à son `Content-Type`, sans extension ; le titre de la
/// liste sert quand le flux n'annonce pas de nom.
#[tokio::test]
async fn une_liste_m3u_sans_extension_est_reconnue_a_son_type() {
    let base = diffuseur().await;
    let s = sonder(client(), &format!("{base}/sans-extension"))
        .await
        .unwrap();
    assert_eq!(s.stream_url, format!("{base}/latin1"));
    assert_eq!(s.playlist_url, Some(format!("{base}/sans-extension")));
    assert_eq!(s.name.as_deref(), Some("Musique Française"));
}

#[tokio::test]
async fn un_direct_hls_donne_son_meilleur_variant() {
    let base = diffuseur().await;
    let s = sonder(client(), &format!("{base}/direct.m3u8"))
        .await
        .unwrap();
    assert!(s.hls);
    assert_eq!(s.stream_url, format!("{base}/direct.m3u8"));
    assert_eq!(s.bitrate, Some(320));
    assert_eq!(s.codec.as_deref(), Some("AAC"));
}

/// Une page web est refusée comme `POST /radios` la refuserait.
#[tokio::test]
async fn une_page_web_est_refusee() {
    let base = diffuseur().await;
    let e = sonder(client(), &format!("{base}/page")).await.unwrap_err();
    assert_eq!(e.code(), "radio_url_pas_un_flux");
}

/// Injoignable, 404 ou liste vide : pas d'erreur, un formulaire non rempli.
#[tokio::test]
async fn un_serveur_muet_n_est_pas_une_erreur() {
    let base = diffuseur().await;
    for chemin in ["/absent", "/vide.pls"] {
        let url = format!("{base}{chemin}");
        let s = sonder(client(), &url).await.unwrap();
        assert!(!s.reachable, "{chemin}");
        assert_eq!(s.stream_url, url);
        assert_eq!(s.codec, None);
    }
    // Port fermé.
    let s = sonder(client(), "http://127.0.0.1:9/flux").await.unwrap();
    assert!(!s.reachable);
}

#[tokio::test]
async fn une_adresse_mal_formee_est_refusee_sans_reseau() {
    let e = sonder(client(), "http;//radio.example/flux")
        .await
        .unwrap_err();
    assert_eq!(e.code(), "radio_url_separateur_faux");
}

#[test]
fn les_codecs_des_types_et_de_hls() {
    assert_eq!(codec_du_type("audio/mpeg"), Some("MP3"));
    assert_eq!(codec_du_type("audio/aacp; charset=x"), Some("AAC"));
    assert_eq!(codec_du_type("application/ogg"), Some("OGG"));
    assert_eq!(codec_du_type("audio/flac"), Some("FLAC"));
    assert_eq!(codec_du_type("application/octet-stream"), None);
    assert_eq!(
        variant_hls("#EXT-X-STREAM-INF:BANDWIDTH=900000,CODECS=\"fLaC\"\nx.m3u8"),
        (Some(900), Some("FLAC"))
    );
    assert_eq!(
        variant_hls("#EXTM3U\n#EXTINF:10,\nseg1.aac\n"),
        (None, None)
    );
}

/// Par la route : la station déjà dans « Mes radios » au même flux est
/// signalée, et rien n'est enregistré.
#[tokio::test]
async fn la_route_signale_une_station_deja_presente_et_n_enregistre_rien() {
    let base = diffuseur().await;
    let dossier = tempfile::tempdir().unwrap();
    let state = AppState::new(
        dossier.path().join("tune.db").to_str().unwrap(),
        0,
        Default::default(),
    )
    .unwrap();
    let repo = RadioRepo::with_backend(state.backend.clone());
    let id = repo
        .create(&tune_core::db::radio_repo::RadioStation {
            id: None,
            name: "Déjà là".into(),
            url: format!("{base}/flux.mp3"),
            homepage: None,
            logo_url: None,
            country: None,
            language: None,
            genre: None,
            codec: None,
            bitrate: None,
            is_favorite: false,
            last_played: None,
            play_count: 0,
        })
        .unwrap();
    let avant = repo.list().unwrap().len();
    let app = crate::routes::router(state.clone());
    let reponse = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/radios/probe")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"url": format!("{base}/liste.pls")}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reponse.status(), StatusCode::OK);
    let corps: Value = serde_json::from_slice(
        &axum::body::to_bytes(reponse.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(corps["existing_id"], json!(id));
    assert_eq!(corps["existing_name"], "Déjà là");
    assert_eq!(corps["stream_url"], format!("{base}/flux.mp3"));
    assert_eq!(corps["codec"], "MP3");
    assert_eq!(
        repo.list().unwrap().len(),
        avant,
        "la sonde n'enregistre rien"
    );
}
