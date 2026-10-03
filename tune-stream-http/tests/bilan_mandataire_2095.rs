//! Fil 2095 — le mandataire écrit le bilan de chaque connexion.
//!
//! FabienM (Devialet Phantom en DLNA, Qobuz 24/192, 02/10) : après un Seek,
//! le Phantom demande `bytes=51706410-`, Tune relaie le 206 du CDN — et plus
//! de son. Le journal ne permettait pas de dire si le Phantom avait réellement
//! tiré les octets : le chemin mandataire (`resumable_proxy_body`) n'écrivait
//! aucun bilan, contrairement à la conversion (`stream_connexion_terminee`,
//! #5649). Il l'écrit désormais : plage demandée, agent, octets envoyés,
//! durée, et la cause de fin.
//!
//! Un seul `#[tokio::test]` : l'abonné de journal est global au binaire.
//!
//! Contre-épreuve : retirer le bilan de `resumable_proxy_body` fait tomber
//! l'étape 1 (« aucune ligne stream_connexion_terminee »).
//!
//! `tune-stream-http` n'a PAS `autotests = false` : ce fichier est compilé
//! sans déclaration `[[test]]`.

use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tune_core::http::streamer::{SharedSessions, StreamInfo, StreamSession};

#[derive(Clone, Default)]
struct JournalCapture(Arc<Mutex<Vec<u8>>>);

impl JournalCapture {
    fn depuis(&self, repere: usize) -> String {
        let brut = self.0.lock().unwrap();
        String::from_utf8_lossy(&brut[repere.min(brut.len())..]).into_owned()
    }
    fn repere(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

impl std::io::Write for JournalCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for JournalCapture {
    type Writer = JournalCapture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

const SECRET: &str = "etsp=1790000000&sig=cafe2095";

const AGENT: &str = "Devialet/2.16.1-51b2a353 libsoup/2.74.2";

/// Un CDN factice : `Range: bytes=N-` ⇒ 206 exact avec `Content-Range`,
/// sinon 200 et `Content-Length`. Il annonce `Accept-Ranges: bytes`.
async fn cdn(corps: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/file/track.flac?{SECRET}",
        listener.local_addr().unwrap()
    );
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
                let (statut, entetes, tranche) = match debut {
                    Some(n) if n < total => (
                        "206 Partial Content",
                        format!(
                            "Content-Range: bytes {n}-{}/{total}\r\nContent-Length: {}\r\n",
                            total - 1,
                            total - n
                        ),
                        &corps[n..],
                    ),
                    _ => ("200 OK", format!("Content-Length: {total}\r\n"), &corps[..]),
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

async fn session_mandataire(id: &str, amont: String) -> SharedSessions {
    let info = StreamInfo {
        format: "flac".into(),
        mime_type: "audio/flac".into(),
        sample_rate: 192_000,
        bit_depth: 24,
        channels: 2,
        ..StreamInfo::default()
    };
    let session = StreamSession::new(id.to_string(), info, false, 16);
    *session.proxy_url.lock().await = Some(amont);
    Arc::new(tokio::sync::Mutex::new(
        [(id.to_string(), Arc::new(session))].into_iter().collect(),
    ))
}

/// `GET` avec `Range`, comme le Phantom après un Seek. Lit au plus `borne`
/// octets puis LÂCHE le corps (le renderer qui ferme) ; `None` lit tout.
/// Rend le statut et les octets reçus.
async fn get(
    sessions: &SharedSessions,
    id: &str,
    range: &str,
    borne: Option<usize>,
) -> (u16, usize) {
    let mut h = axum::http::HeaderMap::new();
    h.insert("User-Agent", AGENT.parse().unwrap());
    h.insert("Range", range.parse().unwrap());
    let rep =
        tune_stream_http::handle_stream(Path(format!("{id}.flac")), State(sessions.clone()), h)
            .await;
    let statut = rep.status().as_u16();
    let mut flux = rep.into_body().into_data_stream();
    let mut n = 0;
    while let Some(bloc) = tokio::time::timeout(std::time::Duration::from_secs(10), flux.next())
        .await
        .expect("le corps doit arriver")
    {
        n += bloc.expect("bloc lisible").len();
        if borne.is_some_and(|b| n >= b) {
            break;
        }
    }
    drop(flux);
    (statut, n)
}

fn ligne_de_bilan(journal: &str) -> String {
    let evenement = ["stream", "connexion", "terminee"].join("_");
    let lignes: Vec<&str> = journal.lines().filter(|l| l.contains(&evenement)).collect();
    assert_eq!(
        lignes.len(),
        1,
        "une connexion mandataire doit laisser UNE ligne {evenement} :\n{journal}"
    );
    lignes[0].to_string()
}

#[tokio::test]
async fn le_mandataire_ecrit_le_bilan_de_chaque_connexion() {
    let capture = JournalCapture::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(abonne)
        .expect("ce binaire ne contient qu'un test : l'abonné global est libre");

    const TOTAL: usize = 4_000_000;
    const DEPUIS: usize = 1_500_000;
    let corps: Vec<u8> = (0..TOTAL).map(|i| (i % 251) as u8).collect();
    let amont = cdn(corps).await;
    let id = "mandataire-2095";
    let sessions = session_mandataire(id, amont).await;

    // 1. Le renderer lit une partie du 206 puis ferme : la ligne dit ce qu'il
    //    a réellement emporté, et que c'est LUI qui est parti.
    let repere = capture.repere();
    let (statut, lus) = get(&sessions, id, &format!("bytes={DEPUIS}-"), Some(100_000)).await;
    assert_eq!(statut, 206);
    assert!(
        lus < TOTAL - DEPUIS,
        "prémisse : le client a coupé avant la fin ({lus} octets)"
    );
    let ligne = ligne_de_bilan(&capture.depuis(repere));
    for attendu in [
        "voie=\"mandataire\"".to_string(),
        format!("range=bytes={DEPUIS}-"),
        format!("agent={AGENT}"),
        format!("octets_envoyes={lus} "),
        "tuyau=-".to_string(),
        "duree_ms=".to_string(),
        "fin=\"client_parti\"".to_string(),
    ] {
        assert!(ligne.contains(&attendu), "« {attendu} » absent : {ligne}");
    }
    assert!(
        !ligne.contains("cafe2095") && !ligne.contains("127.0.0.1"),
        "le bilan ne doit porter aucune URL signée : {ligne}"
    );

    // 2. Lecture complète : total − N octets, corps allé au bout.
    let repere = capture.repere();
    let (statut, lus) = get(&sessions, id, &format!("bytes={DEPUIS}-"), None).await;
    assert_eq!((statut, lus), (206, TOTAL - DEPUIS));
    let ligne = ligne_de_bilan(&capture.depuis(repere));
    for attendu in [
        format!("octets_envoyes={} ", TOTAL - DEPUIS),
        "fin=\"corps_complet\"".to_string(),
    ] {
        assert!(ligne.contains(&attendu), "« {attendu} » absent : {ligne}");
    }
}
