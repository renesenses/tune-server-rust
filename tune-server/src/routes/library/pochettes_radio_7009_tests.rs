use super::*;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
    routing::get,
};
use std::sync::Arc;
use tower::ServiceExt;
use tune_core::library::artwork_proxy::{Relais, Resolution, Resolveur};

struct Local;
impl Resolveur for Local {
    fn resoudre(&self, _nom: &str) -> Resolution {
        Box::pin(async { Ok(vec!["127.0.0.1".parse().unwrap()]) })
    }
}
fn png(couleur: u8) -> Vec<u8> {
    let mut sortie = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        1,
        1,
        image::Rgb([couleur, 0, 0]),
    ))
    .write_to(&mut sortie, image::ImageFormat::Png)
    .unwrap();
    sortie.into_inner()
}
struct Banc {
    etat: AppState,
    origine: String,
    serveur: tokio::task::JoinHandle<()>,
}
impl Banc {
    async fn nouveau() -> Self {
        let app = Router::new()
            .route(
                "/cover.png",
                get(|| async { ([("Content-Type", "image/png")], png(30)) }),
            )
            .route(
                "/logo.png",
                get(|| async { ([("Content-Type", "image/png")], png(200)) }),
            );
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = ecoute.local_addr().unwrap().port();
        let serveur = tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
        let mut etat = AppState::new(":memory:", 0, Default::default()).unwrap();
        etat.relais_pochettes = Arc::new(Relais::avec(Arc::new(Local), |_| true));
        Self {
            etat,
            origine: format!("http://diffuseur-radio.test:{port}"),
            serveur,
        }
    }
    async fn publier(&self, zone: i64, source: &str, pochette: &str, logo: &str) {
        let url = format!("https://flux.test/station-{zone}");
        let id = self
            .etat
            .backend
            .execute_returning_id(
                "INSERT INTO radio_stations (name,url,logo_url) VALUES ('Station du banc',?,?)",
                &[&url, &logo],
            )
            .unwrap();
        self.etat
            .playback
            .play(
                zone,
                tune_core::playback::NowPlaying {
                    source: source.into(),
                    source_id: Some(id.to_string()),
                    cover_path: Some(pochette.into()),
                    title: "Titre du flux".into(),
                    ..Default::default()
                },
            )
            .await;
    }
    async fn demander(&self, pochette: &str, sig: Option<&str>) -> axum::response::Response {
        let app = Router::new()
            .route("/proxy", get(proxy_artwork))
            .with_state(self.etat.clone());
        let mut uri = reqwest::Url::parse("http://tune.test/proxy").unwrap();
        uri.query_pairs_mut().append_pair("url", pochette);
        if let Some(sig) = sig {
            uri.query_pairs_mut().append_pair("sig", sig);
        }
        app.oneshot(
            Request::builder()
                .uri(uri.as_str())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
    }
}
impl Drop for Banc {
    fn drop(&mut self) {
        self.serveur.abort();
    }
}

#[tokio::test]
async fn la_pochette_radio_publiee_hors_liste_traverse_le_relais_7009() {
    let b = Banc::nouveau().await;
    let url = format!("{}/cover.png", b.origine);
    b.publier(1, "radio", &url, &format!("{}/logo.png", b.origine))
        .await;
    let rep = b.demander(&url, None).await;
    assert_eq!(
        rep.status(),
        StatusCode::OK,
        "#7009 : la pochette publiée par la radio reste refusée par le relais"
    );
    assert_eq!(rep.headers().get("Cache-Control").unwrap(), "no-cache");
    assert_eq!(
        to_bytes(rep.into_body(), 1_000_000).await.unwrap().as_ref(),
        png(30)
    );
}
#[tokio::test]
async fn une_pochette_radio_absente_revient_au_logo_7009() {
    let b = Banc::nouveau().await;
    let url = format!("{}/absente.png", b.origine);
    b.publier(1, "radio", &url, &format!("{}/logo.png", b.origine))
        .await;
    let rep = b.demander(&url, None).await;
    assert_eq!(
        rep.status(),
        StatusCode::OK,
        "#7009 : une pochette absente doit rendre le logo de sa station"
    );
    assert_eq!(rep.headers().get("Cache-Control").unwrap(), "no-cache");
    assert_eq!(
        to_bytes(rep.into_body(), 1_000_000).await.unwrap().as_ref(),
        png(200)
    );
}
#[tokio::test]
async fn une_url_non_publiee_ne_profite_pas_de_la_radio_7009() {
    let b = Banc::nouveau().await;
    b.publier(
        1,
        "radio",
        &format!("{}/cover.png", b.origine),
        &format!("{}/logo.png", b.origine),
    )
    .await;
    assert_eq!(
        b.demander(&format!("{}/autre.png", b.origine), None)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
}
#[tokio::test]
async fn une_source_locale_ne_devient_pas_une_radio_7009() {
    let b = Banc::nouveau().await;
    let url = format!("{}/cover.png", b.origine);
    b.publier(1, "local", &url, &format!("{}/logo.png", b.origine))
        .await;
    assert_eq!(b.demander(&url, None).await.status(), StatusCode::FORBIDDEN);
}
#[tokio::test]
async fn une_signature_invalide_reste_refusee_pour_une_radio_7009() {
    let b = Banc::nouveau().await;
    let url = format!("{}/cover.png", b.origine);
    b.publier(1, "radio", &url, &format!("{}/logo.png", b.origine))
        .await;
    assert_eq!(
        b.demander(&url, Some("invalide")).await.status(),
        StatusCode::BAD_REQUEST
    );
}
#[tokio::test]
async fn le_repli_ambigu_n_affiche_pas_un_mauvais_logo_7009() {
    let b = Banc::nouveau().await;
    let url = format!("{}/cover.png", b.origine);
    b.publier(1, "radio", &url, "https://station-a.test/logo.png")
        .await;
    b.publier(2, "radio", &url, "https://station-b.test/logo.png")
        .await;
    assert_eq!(
        logo_de_pochette_radio_publiee(&b.etat, &url).await,
        Some(None)
    );
}
#[tokio::test]
async fn une_pochette_radio_garde_la_politique_d_adresse_7009() {
    let mut b = Banc::nouveau().await;
    b.etat.relais_pochettes = Arc::new(Relais::production());
    let url = "http://127.0.0.1/cover.png";
    b.publier(1, "radio", url, "http://127.0.0.1/logo.png")
        .await;
    assert_eq!(b.demander(url, None).await.status(), StatusCode::FORBIDDEN);
}
