//! #7778, point 9 — fil 2637 (rc10) : `top_podcasts_failed error=apple top
//! podcasts HTTP 502 Bad Gateway`, remonté en `internal_error`. Le palmarès
//! d'Apple indisponible faisait répondre 500 à `GET /podcasts/top`.
//!
//! Le banc : un faux Apple local qui répond 502 à toute demande de palmarès.
//!
//! Contre-épreuve : remettre `Err(AppError::internal(e))` dans la branche
//! d'erreur de `reponse_du_top` fait tomber
//! `un_palmares_apple_en_502_s_affiche_vide_avec_un_message_7778`.

use super::reponse_du_top;
use tune_core::streaming::podcasts::PodcastService;

/// Un faux Apple qui répond `502 Bad Gateway` à chaque requête.
async fn faux_apple_en_502() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut flux, _)) = ecoute.accept().await {
            let mut tampon = [0u8; 4096];
            let _ = flux.read(&mut tampon).await;
            let _ = flux
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await;
        }
    });
    base
}

#[tokio::test]
async fn un_palmares_apple_en_502_s_affiche_vide_avec_un_message_7778() {
    let base = faux_apple_en_502().await;
    let client = tune_core::http::client::builder()
        .no_proxy()
        .build()
        .unwrap();
    let svc = PodcastService::with_apple_bases(client, &base, &base);
    // Pays propre au banc : le cache du palmarès est global au processus.
    let corps = match reponse_du_top(&svc, None, "zz", "fr").await {
        Ok(json) => json.0,
        Err(e) => panic!(
            "#7778 : un palmarès Apple en 502 ne doit pas devenir une erreur du serveur ({})",
            e.message
        ),
    };
    assert_eq!(
        corps["items"],
        serde_json::json!([]),
        "#7778 : la liste doit s'afficher vide quand Apple ne rend pas le palmarès : {corps}"
    );
    assert_eq!(corps["unavailable"], true, "{corps}");
    assert_eq!(corps["code"], "top_podcasts_unavailable", "{corps}");
    let message = corps["message"].as_str().unwrap();
    assert!(
        message.contains("indisponible"),
        "#7778 : le message doit dire que le classement est indisponible : {message}"
    );
}
