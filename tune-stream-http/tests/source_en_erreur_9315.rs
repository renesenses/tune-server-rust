//! #9315 — la source d'un flux relayé répond par une erreur HTTP.
//!
//! Forum (relevé du 21/11) : un épisode de podcast en 404 chez l'éditeur. Le
//! mandataire relayait la page d'erreur au renderer comme si c'était de
//! l'audio (statut 200, `Content-Type` audio) ; la zone semblait démarrer,
//! puis s'arrêtait 77 s plus tard sans message.
//!
//! Témoins : une source factice qui répond 404, puis 503 ⇒ le renderer reçoit
//! un 502, AUCUN octet de la page d'erreur, et le statut est noté sur la
//! session pour le sondeur. Contre-témoin : une source saine (200) est
//! relayée comme avant, sans rien noter.
//!
//! Contre-épreuve : retirer le bloc `statut_amont_en_echec` de
//! `proxy_stream` (tune-stream-http/src/lib.rs) fait tomber les deux témoins
//! d'erreur (200 et la page d'erreur relayée).
//!
//! `tune-stream-http` n'a PAS `autotests = false` : ce fichier est compilé
//! sans déclaration `[[test]]`.

use std::sync::Arc;

use axum::extract::{Path, State};
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tune_core::http::streamer::{SharedSessions, StreamInfo, StreamSession};

const PAGE_D_ERREUR: &str = "<html><body><h1>Not Found</h1></body></html>";

/// Une source factice qui répond `statut` à toute requête, avec `corps`.
async fn source(statut: &'static str, type_: &'static str, corps: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/podcast/episode-9315.m4a",
        listener.local_addr().unwrap()
    );
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
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
                let entete = format!(
                    "HTTP/1.1 {statut}\r\nContent-Type: {type_}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    corps.len()
                );
                let _ = socket.write_all(entete.as_bytes()).await;
                let _ = socket.write_all(&corps).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    url
}

async fn session_relayee(id: &str, amont: String) -> (SharedSessions, Arc<StreamSession>) {
    let info = StreamInfo {
        format: "m4a".into(),
        mime_type: "audio/mp4".into(),
        channels: 2,
        ..StreamInfo::default()
    };
    let session = Arc::new(StreamSession::new(id.to_string(), info, false, 16));
    *session.proxy_url.lock().await = Some(amont);
    let sessions: SharedSessions = Arc::new(tokio::sync::Mutex::new(
        [(id.to_string(), session.clone())].into_iter().collect(),
    ));
    (sessions, session)
}

/// `GET` du flux par un renderer DLNA : (statut, octets reçus).
async fn get(sessions: &SharedSessions, id: &str) -> (u16, Vec<u8>) {
    let mut h = axum::http::HeaderMap::new();
    h.insert("User-Agent", "Eversolo DMP-A6".parse().unwrap());
    let rep =
        tune_stream_http::handle_stream(Path(format!("{id}.m4a")), State(sessions.clone()), h)
            .await;
    let statut = rep.status().as_u16();
    let mut flux = rep.into_body().into_data_stream();
    let mut octets = Vec::new();
    while let Some(bloc) = tokio::time::timeout(std::time::Duration::from_secs(10), flux.next())
        .await
        .expect("le corps doit arriver")
    {
        octets.extend_from_slice(&bloc.expect("bloc lisible"));
    }
    (statut, octets)
}

async fn verifier_l_erreur(statut_source: &'static str, attendu: u16) {
    let id = format!("relais-9315-{attendu}");
    let amont = source(statut_source, "text/html", PAGE_D_ERREUR.as_bytes().to_vec()).await;
    let (sessions, session) = session_relayee(&id, amont).await;
    let (statut, octets) = get(&sessions, &id).await;
    assert_eq!(
        statut, 502,
        "#9315 : une source en {statut_source} ne se relaie pas comme de l'audio"
    );
    assert!(
        !String::from_utf8_lossy(&octets).contains("Not Found"),
        "#9315 : la page d'erreur de la source n'atteint pas le renderer"
    );
    assert_eq!(
        session.erreur_amont(),
        Some(attendu),
        "#9315 : le statut de la source est noté pour le sondeur"
    );
}

#[tokio::test]
async fn une_source_en_404_rend_502_et_le_note_9315() {
    verifier_l_erreur("404 Not Found", 404).await;
}

#[tokio::test]
async fn une_source_en_503_rend_502_et_le_note_9315() {
    verifier_l_erreur("503 Service Unavailable", 503).await;
}

#[tokio::test]
async fn une_source_saine_est_relayee_comme_avant_9315() {
    let audio: Vec<u8> = (0..32_000).map(|i| (i % 251) as u8).collect();
    let amont = source("200 OK", "audio/mp4", audio.clone()).await;
    let (sessions, session) = session_relayee("relais-9315-sain", amont).await;
    let (statut, octets) = get(&sessions, "relais-9315-sain").await;
    assert_eq!(statut, 200);
    assert_eq!(octets, audio, "les octets de la source passent verbatim");
    assert_eq!(session.erreur_amont(), None);
}
