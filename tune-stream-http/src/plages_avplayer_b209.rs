//! Écoute à distance sur iPhone : AVPlayer sonde avec `Range: bytes=0-1`.
//!
//! Mesuré le 10/10/2026 (1.0.0-rc3, zone « Ce téléphone », iPhone en 5G via
//! le relais) : à `bytes=0-1`, le mandataire répondait
//! `206 Content-Range: bytes 0-41008206/41008207` et envoyait le fichier
//! entier. AVPlayer exige les deux octets demandés et un `Content-Range`
//! exact ; il rejette la réponse et la zone reste muette.
//!
//! Cause : `proxy_stream` ne lisait que le DÉBUT de la plage
//! (`starts_with("bytes=0-")`), jamais sa fin, ni le suffixe `bytes=-n`, ni
//! une plage hors fichier. Ces gardes passent par `handle_stream`, sur une
//! session MANDATAIRE (Qobuz, Tidal, piste d'un autre Tune) branchée sur un
//! faux CDN qui honore `bytes=N-` comme Akamai.
use super::*;
use axum::extract::{Path, State};
use futures_util::StreamExt;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const AVPLAYER: &str = "AppleCoreMedia/1.0.0.22A3354 (iPhone; U; CPU OS 18_0 like Mac OS X; fr_fr)";
const TOTAL: usize = 10_000;

fn corps() -> Vec<u8> {
    (0..TOTAL).map(|i| (i % 251) as u8).collect()
}

/// Faux CDN : `Range: bytes=N-` (N dans le fichier) → 206 exact depuis N ;
/// N hors fichier → 416 `bytes */total` ; sans Range → 200 entier.
async fn faux_cdn(corps: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/flac", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let corps = corps.clone();
            tokio::spawn(async move {
                let mut requete = Vec::new();
                let mut octet = [0u8; 1];
                while !requete.ends_with(b"\r\n\r\n") && requete.len() < 16_384 {
                    if socket.read_exact(&mut octet).await.is_err() {
                        return;
                    }
                    requete.push(octet[0]);
                }
                let texte = String::from_utf8_lossy(&requete).to_string();
                let debut = texte
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("Range: bytes=")
                            .or(l.strip_prefix("range: bytes="))
                    })
                    .and_then(|r| r.split('-').next()?.parse::<usize>().ok());
                let total = corps.len();
                let (statut, entetes, tranche): (&str, String, &[u8]) = match debut {
                    Some(n) if n < total => (
                        "206 Partial Content",
                        format!(
                            "Content-Range: bytes {n}-{}/{total}\r\nContent-Length: {}\r\n",
                            total - 1,
                            total - n
                        ),
                        &corps[n..],
                    ),
                    Some(_) => (
                        "416 Range Not Satisfiable",
                        format!("Content-Range: bytes */{total}\r\nContent-Length: 0\r\n"),
                        &[],
                    ),
                    None => ("200 OK", format!("Content-Length: {total}\r\n"), &corps[..]),
                };
                let entete = format!(
                    "HTTP/1.1 {statut}\r\nContent-Type: audio/flac\r\nAccept-Ranges: bytes\r\n{entetes}Connection: close\r\n\r\n"
                );
                let _ = socket.write_all(entete.as_bytes()).await;
                let _ = socket.write_all(tranche).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    url
}

async fn session_mandataire(id: &str) -> SharedSessions {
    let url = faux_cdn(corps()).await;
    let info = StreamInfo {
        format: "flac".into(),
        mime_type: "audio/flac".into(),
        ..StreamInfo::default()
    };
    let session = StreamSession::new(id.to_string(), info, false, 16);
    *session.proxy_url.lock().await = Some(url);
    Arc::new(tokio::sync::Mutex::new(
        [(id.to_string(), Arc::new(session))].into_iter().collect(),
    ))
}

async fn demander(
    sessions: SharedSessions,
    id: &str,
    range: &str,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut h = HeaderMap::new();
    h.insert("User-Agent", AVPLAYER.parse().unwrap());
    h.insert("Range", range.parse().unwrap());
    let rep = handle_stream(Path(format!("{id}.flac")), State(sessions), h).await;
    let statut = rep.status();
    let entetes = rep.headers().clone();
    let mut flux = rep.into_body().into_data_stream();
    let mut recu = Vec::new();
    while let Some(bloc) = tokio::time::timeout(std::time::Duration::from_secs(10), flux.next())
        .await
        .expect("le corps doit arriver")
    {
        recu.extend_from_slice(&bloc.expect("bloc lisible"));
    }
    (statut, entetes, recu)
}

fn entete(h: &HeaderMap, nom: &str) -> String {
    h.get(nom)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<absent>")
        .to_string()
}

/// LA sonde d'AVPlayer : deux octets, pas le fichier.
#[tokio::test]
async fn la_sonde_bytes_0_1_d_avplayer_recoit_deux_octets() {
    let s = session_mandataire("avp-0-1").await;
    let (statut, h, recu) = demander(s, "avp-0-1", "bytes=0-1").await;
    assert_eq!(statut, StatusCode::PARTIAL_CONTENT);
    assert_eq!(entete(&h, "Content-Range"), format!("bytes 0-1/{TOTAL}"));
    assert_eq!(entete(&h, "Content-Length"), "2");
    assert_eq!(recu, corps()[0..2].to_vec());
}

/// Plage bornée au milieu du fichier (start > 0 : transmise au CDN en
/// `bytes=N-`, bornée ici à M).
#[tokio::test]
async fn une_plage_bornee_au_milieu_rend_exactement_ses_octets() {
    let s = session_mandataire("avp-borne").await;
    let (statut, h, recu) = demander(s, "avp-borne", "bytes=100-199").await;
    assert_eq!(statut, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        entete(&h, "Content-Range"),
        format!("bytes 100-199/{TOTAL}")
    );
    assert_eq!(entete(&h, "Content-Length"), "100");
    assert_eq!(recu, corps()[100..200].to_vec());
}

/// Une fin au-delà du dernier octet se ramène au dernier octet.
#[tokio::test]
async fn une_fin_au_dela_du_fichier_est_ramenee_au_dernier_octet() {
    let s = session_mandataire("avp-fin-loin").await;
    let (statut, h, recu) = demander(s, "avp-fin-loin", "bytes=9990-99999").await;
    assert_eq!(statut, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        entete(&h, "Content-Range"),
        format!("bytes 9990-9999/{TOTAL}")
    );
    assert_eq!(entete(&h, "Content-Length"), "10");
    assert_eq!(recu, corps()[9990..].to_vec());
}

/// Plage ouverte : du point demandé à la fin.
#[tokio::test]
async fn une_plage_ouverte_rend_la_fin_du_fichier() {
    let s = session_mandataire("avp-ouverte").await;
    let (statut, h, recu) = demander(s, "avp-ouverte", "bytes=5000-").await;
    assert_eq!(statut, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        entete(&h, "Content-Range"),
        format!("bytes 5000-9999/{TOTAL}")
    );
    assert_eq!(entete(&h, "Content-Length"), "5000");
    assert_eq!(recu, corps()[5000..].to_vec());
}

/// `bytes=0-` (DLNA, Eversolo) : inchangé, 206 du fichier entier.
#[tokio::test]
async fn bytes_0_ouvert_reste_un_206_du_fichier_entier() {
    let s = session_mandataire("avp-zero").await;
    let (statut, h, recu) = demander(s, "avp-zero", "bytes=0-").await;
    assert_eq!(statut, StatusCode::PARTIAL_CONTENT);
    assert_eq!(entete(&h, "Content-Range"), format!("bytes 0-9999/{TOTAL}"));
    assert_eq!(entete(&h, "Content-Length"), TOTAL.to_string());
    assert_eq!(recu, corps());
}

/// Suffixe : les n DERNIERS octets, pas les n premiers.
#[tokio::test]
async fn un_suffixe_rend_les_derniers_octets() {
    let s = session_mandataire("avp-suffixe").await;
    let (statut, h, recu) = demander(s, "avp-suffixe", "bytes=-100").await;
    assert_eq!(statut, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        entete(&h, "Content-Range"),
        format!("bytes 9900-9999/{TOTAL}")
    );
    assert_eq!(entete(&h, "Content-Length"), "100");
    assert_eq!(recu, corps()[9900..].to_vec());
}

/// Hors fichier : 416 qui dit la taille, aucun octet audio.
#[tokio::test]
async fn une_plage_hors_fichier_repond_416() {
    let s = session_mandataire("avp-416").await;
    let (statut, h, recu) = demander(s, "avp-416", "bytes=20000-20001").await;
    assert_eq!(statut, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(entete(&h, "Content-Range"), format!("bytes */{TOTAL}"));
    assert!(
        recu.is_empty(),
        "un 416 ne porte pas d'audio : {} octets",
        recu.len()
    );
}

/// Même sonde sur une session FICHIER (piste locale, ou Qobuz en cache) :
/// `serve_file` la bornait déjà — ce témoin le garde.
#[tokio::test]
async fn la_sonde_bytes_0_1_sur_un_fichier_rend_deux_octets() {
    let fichier = tune_core::test_scratch::scratch_file("avp-fichier-0-1", ".flac");
    std::fs::write(fichier.path(), corps()).expect("fichier de test");
    let info = StreamInfo {
        format: "flac".into(),
        mime_type: "audio/flac".into(),
        ..StreamInfo::default()
    };
    let session = Arc::new(StreamSession::new("avp-fichier".into(), info, false, 8));
    *session.file_path.lock().await = Some(fichier.path().to_string_lossy().into_owned());
    let s: SharedSessions = Arc::new(tokio::sync::Mutex::new(
        [("avp-fichier".to_string(), session)].into_iter().collect(),
    ));
    let (statut, h, recu) = demander(s, "avp-fichier", "bytes=0-1").await;
    assert_eq!(statut, StatusCode::PARTIAL_CONTENT);
    assert_eq!(entete(&h, "Content-Range"), format!("bytes 0-1/{TOTAL}"));
    assert_eq!(entete(&h, "Content-Length"), "2");
    assert_eq!(recu, corps()[0..2].to_vec());
}
