//! #5522 — une réponse du CDN Qobuz (Akamai) à plus de 100 en-têtes.
//!
//! Mesuré le 11/10/2026 sur `streaming-qobuz-std.akamaized.net` : la sonde
//! `Range: bytes=0-0` de la piste 921595 rend **108 en-têtes**, dont 46
//! `X-AK-FWD-ERROR` et 46 `X-AK-GRN` (une paire par nouvel essai interne du
//! CDN). La piste voisine 921596 en rend 44 et passe. hyper refuse par défaut
//! toute réponse HTTP/1 au-delà de 100 en-têtes, avec l'erreur
//! `message head is too large` relevée dans le journal du testeur : la sonde
//! Range puis le téléchargement de repli échouent, la piste ne joue jamais.
//!
//! Le serveur de ce banc rejoue cette forme de réponse.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

/// Nombre de paires `X-AK-FWD-ERROR` / `X-AK-GRN` relevé sur la piste 921595.
const PAIRES_AKAMAI: usize = 46;

/// Sert `connexions` réponses `206` d'un octet portant les en-têtes relevés
/// sur le CDN, puis s'arrête. Rend l'URL à interroger.
fn serveur_akamai(connexions: usize) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/file?eid=921595", listener.local_addr().unwrap());
    let fil = std::thread::spawn(move || {
        for _ in 0..connexions {
            let Ok((mut flux, _)) = listener.accept() else {
                return;
            };
            let mut lecteur = BufReader::new(flux.try_clone().unwrap());
            loop {
                let mut ligne = String::new();
                if lecteur.read_line(&mut ligne).unwrap_or(0) == 0 || ligne == "\r\n" {
                    break;
                }
            }
            let mut tete = String::from(
                "HTTP/1.1 206 Partial Content\r\n\
                 Server: AkamaiNetStorage\r\n\
                 Content-Type: audio/flac\r\n\
                 Content-Range: bytes 0-0/42383382\r\n\
                 Content-Length: 1\r\n\
                 ETag: \"5522\"\r\n\
                 Cache-Control: max-age=3600\r\n\
                 CDN-Origin-Protocol: https\r\n\
                 X-Forward-Proto: https\r\n\
                 Akamai-Request-BC: [a=1,b=2]\r\n\
                 Akamai-Mon-Iucid-Del: 1\r\n\
                 Date: Sun, 11 Oct 2026 08:00:00 GMT\r\n\
                 Connection: close\r\n",
            );
            for i in 0..PAIRES_AKAMAI {
                tete.push_str(&format!(
                    "X-AK-FWD-ERROR: ERR_{i}\r\nX-AK-GRN: 0.{i}.abcdef\r\n"
                ));
            }
            tete.push_str("\r\n");
            let _ = flux.write_all(tete.as_bytes());
            let _ = flux.write_all(b"f");
        }
    });
    (url, fil)
}

#[test]
fn la_sonde_range_accepte_les_108_entetes_du_cdn_qobuz_5522() {
    let (url, fil) = serveur_akamai(1);
    let source = crate::audio::http_range::HttpRangeSource::open(&url);
    fil.join().unwrap();
    let source = source.unwrap_or_else(|e| panic!("sonde refusée : {e}"));
    assert_eq!(source.taille_annoncee(), 42_383_382);
}

#[tokio::test]
async fn le_client_asynchrone_accepte_les_108_entetes_du_cdn_qobuz_5522() {
    let (url, fil) = serveur_akamai(1);
    let client = super::builder().build().unwrap();
    let reponse = client.get(&url).send().await;
    let reponse = reponse
        .unwrap_or_else(|e| panic!("téléchargement refusé : {}", super::decrire_erreur_http(&e)));
    assert_eq!(reponse.status().as_u16(), 206);
    assert_eq!(reponse.bytes().await.unwrap().as_ref(), b"f");
    tokio::task::spawn_blocking(move || fil.join().unwrap())
        .await
        .unwrap();
}
