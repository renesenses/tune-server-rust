//! #8452 (Cover+, spécification § 1.1 et § 1.3) — le lecteur ICY tolérant :
//! un serveur SHOUTcast qui répond `ICY 200 OK`, et un bloc en Latin-1.
//! Aucun réseau : un faux serveur local.

use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Un faux SHOUTcast v1 : ligne d'état `ICY 200 OK`, `icy-metaint: 16`, puis
/// 16 octets d'audio et un bloc portant `meta` (octets bruts).
async fn faux_shoutcast(meta: Vec<u8>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/;stream.mp3", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let meta = meta.clone();
            tokio::spawn(async move {
                let mut requete = Vec::new();
                let mut tampon = [0u8; 1024];
                while !requete.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut tampon).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => requete.extend_from_slice(&tampon[..n]),
                    }
                }
                let blocs = meta.len().div_ceil(16);
                let mut corps = b"ICY 200 OK\r\nicy-name: Faux SHOUTcast\r\nicy-metaint: 16\r\ncontent-type: audio/mpeg\r\n\r\n".to_vec();
                corps.extend_from_slice(&[0u8; 16]);
                corps.push(blocs as u8);
                let mut charge = meta;
                charge.resize(blocs * 16, 0);
                corps.extend_from_slice(&charge);
                corps.extend_from_slice(&[0u8; 16]);
                corps.push(0);
                let _ = sock.write_all(&corps).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    url
}

#[tokio::test]
async fn icy_200_ok_est_lu_et_le_latin_1_decode() {
    // « Beyoncé » en Latin-1 : l'octet 0xE9 n'est pas de l'UTF-8 valide.
    let mut meta = b"StreamTitle='Beyonc".to_vec();
    meta.push(0xE9);
    meta.extend_from_slice(b" - Halo';");
    let url = faux_shoutcast(meta).await;
    let lu = fetch_icy_metadata_dans(&url, std::time::Duration::from_secs(5))
        .await
        .expect("le lecteur tolérant lit un serveur « ICY 200 OK »");
    assert_eq!(lu.artist.as_deref(), Some("Beyoncé"));
    assert_eq!(lu.title, "Halo");
    assert_eq!(lu.station.as_deref(), Some("Faux SHOUTcast"));
}

#[tokio::test]
async fn icy_200_ok_en_utf_8() {
    let url = faux_shoutcast("StreamTitle='Oüi FM - Smokie - Alice';".as_bytes().to_vec()).await;
    let lu = fetch_icy_metadata_dans(&url, std::time::Duration::from_secs(5))
        .await
        .expect("titre lu");
    assert_eq!(lu.artist.as_deref(), Some("Oüi FM"));
    assert_eq!(lu.title, "Smokie - Alice");
}

#[test]
fn ligne_d_etat_icy_ou_http_seulement_en_succes() {
    let ok = entetes_icy_tolerants("ICY 200 OK\r\nicy-metaint: 8192\r\nicy-name: X\r\n\r\n")
        .expect("ICY 200");
    assert_eq!(ok.get("icy-metaint").map(String::as_str), Some("8192"));
    assert!(entetes_icy_tolerants("HTTP/1.0 200 OK\r\n\r\n").is_some());
    assert!(entetes_icy_tolerants("ICY 404 Resource Not Found\r\n\r\n").is_none());
    assert!(entetes_icy_tolerants("ICY 401 Service Unavailable\r\n\r\n").is_none());
    assert!(entetes_icy_tolerants("SSH-2.0-OpenSSH\r\n\r\n").is_none());
}
