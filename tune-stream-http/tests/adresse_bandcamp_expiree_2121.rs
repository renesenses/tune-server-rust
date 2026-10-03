//! Fil 2121 — une adresse Bandcamp expirée ne meurt plus en silence.
//!
//! FabienM (1.0.0-rc1 Linux, zone Cast « Parents », 03/10) : une piste
//! Bandcamp relancée depuis la file garde l'URL bcbits signée le 30/09. Le
//! 03/10, Bandcamp répond **410 Gone**. Le relais Bandcamp n'a pas de
//! mécanisme de nouvelle résolution : `send_with_reresolve` sortait par
//! `return Err(())` sans rien écrire, et la Beosound recevait un 502 que
//! personne ne pouvait lire dans le journal.
//!
//! Étape 1 — sans mécanisme de nouvelle résolution (le relais Bandcamp
//! d'aujourd'hui) : le renderer reçoit 502, ET le journal le dit
//! (`proxy_upstream_expired_no_reresolver`, statut 410, hôte amont).
//!
//! Étape 2 — le même amont, avec un mécanisme de nouvelle résolution qui rend
//! une signature fraîche : succès (206 sur `bytes=0-`) et des octets `audio/mpeg`. C'est le chemin
//! Qobuz/Tidal (#1136) ; il établit que le relais sait déjà guérir un 410 dès
//! qu'on lui donne de quoi resigner — ce qui manque au relais Bandcamp, c'est
//! la référence stable de la piste (adresse de la page), pas le mécanisme.
//!
//! Un seul `#[tokio::test]` : l'abonné de journal est global au binaire.
//!
//! Contre-épreuve : retirer le `warn!` `proxy_upstream_expired_no_reresolver`
//! de `send_with_reresolve` fait tomber l'étape 1 (le 502 reste, la ligne
//! manque).
//!
//! `tune-stream-http` n'a PAS `autotests = false` : ce fichier est compilé
//! sans déclaration `[[test]]`.

use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tune_core::http::streamer::{ReresolveFn, SharedSessions, StreamInfo, StreamSession};

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

/// La signature du 30/09 (celle du journal de Fabien) et une signature fraîche.
const SIGNATURE_PERIMEE: &str = "p=0&ts=1790782809&t=dcd4&token=1790782809_perimee";
const SIGNATURE_FRAICHE: &str = "p=0&ts=1791020000&t=f00d&token=1791020000_fraiche";

/// Un faux bcbits : 410 Gone sur l'ancienne signature, 200 `audio/mpeg` sur
/// toute autre. Rend la base `http://hôte:port/stream/<empreinte>/mp3-128/<id>`.
async fn faux_bcbits(corps: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!(
        "http://{}/stream/e43be2a9/mp3-128/29192493",
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
                let ligne = String::from_utf8_lossy(&requete)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string();
                if ligne.contains("ts=1790782809") {
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 410 Gone\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                } else {
                    let entete = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        corps.len()
                    );
                    let _ = socket.write_all(entete.as_bytes()).await;
                    let _ = socket.write_all(&corps).await;
                }
                let _ = socket.shutdown().await;
            });
        }
    });
    base
}

async fn session_bandcamp(
    id: &str,
    amont: String,
    reresolve: Option<ReresolveFn>,
) -> SharedSessions {
    let info = StreamInfo {
        format: "mp3".into(),
        mime_type: "audio/mpeg".into(),
        sample_rate: 44_100,
        bit_depth: 16,
        channels: 2,
        ..StreamInfo::default()
    };
    let session = StreamSession::new(id.to_string(), info, false, 16);
    *session.proxy_url.lock().await = Some(amont);
    *session.reresolve.lock().await = reresolve;
    Arc::new(tokio::sync::Mutex::new(
        [(id.to_string(), Arc::new(session))].into_iter().collect(),
    ))
}

/// `GET bytes=0-`, comme la Beosound (CrKey). Rend statut, type et octets.
async fn get(sessions: &SharedSessions, id: &str) -> (u16, String, Vec<u8>) {
    let mut h = axum::http::HeaderMap::new();
    h.insert(
        "User-Agent",
        "Mozilla/5.0 (X11; Linux armv7l) CrKey/1.52.272222"
            .parse()
            .unwrap(),
    );
    h.insert("Range", "bytes=0-".parse().unwrap());
    let rep =
        tune_stream_http::handle_stream(Path(format!("{id}.mp3")), State(sessions.clone()), h)
            .await;
    let statut = rep.status().as_u16();
    let type_ = rep
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let mut flux = rep.into_body().into_data_stream();
    let mut octets = Vec::new();
    while let Some(bloc) = tokio::time::timeout(std::time::Duration::from_secs(10), flux.next())
        .await
        .expect("le corps doit arriver")
    {
        octets.extend_from_slice(&bloc.expect("bloc lisible"));
    }
    (statut, type_, octets)
}

#[tokio::test]
async fn une_adresse_bandcamp_expiree_se_dit_et_guerit_quand_on_sait_resigner() {
    let capture = JournalCapture::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(abonne)
        .expect("ce binaire ne contient qu'un test : l'abonné global est libre");

    // Une trame MPEG factice suffit : le relais ne décode rien.
    let corps: Vec<u8> = (0..64_000).map(|i| (i % 251) as u8).collect();
    let base = faux_bcbits(corps.clone()).await;
    let perimee = format!("{base}?{SIGNATURE_PERIMEE}");
    let fraiche = format!("{base}?{SIGNATURE_FRAICHE}");

    // 1. Le relais Bandcamp d'aujourd'hui : aucun mécanisme de nouvelle
    //    résolution. 502 au renderer — et le journal le DIT.
    let repere = capture.repere();
    let sessions = session_bandcamp("bandcamp-2121-sans", perimee.clone(), None).await;
    let (statut, _, _) = get(&sessions, "bandcamp-2121-sans").await;
    assert_eq!(
        statut, 502,
        "sans nouvelle résolution, l'appareil reçoit 502"
    );
    let journal = capture.depuis(repere);
    let ligne = journal
        .lines()
        .find(|l| l.contains("proxy_upstream_expired_no_reresolver"))
        .unwrap_or_else(|| {
            panic!(
                "le 502 doit laisser une ligne proxy_upstream_expired_no_reresolver :\n{journal}"
            )
        });
    assert!(
        ligne.contains("WARN"),
        "un avertissement, pas un détail : {ligne}"
    );
    assert!(ligne.contains("410"), "le statut amont est nommé : {ligne}");
    assert!(
        ligne.contains("amont=127.0.0.1"),
        "l'hôte amont est nommé : {ligne}"
    );

    // 2. Le même amont, avec un mécanisme qui resigne : le 410 guérit.
    let repere = capture.repere();
    let appels = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let reresolve: ReresolveFn = {
        let fraiche = fraiche.clone();
        let appels = Arc::clone(&appels);
        Arc::new(move || {
            let fraiche = fraiche.clone();
            appels.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move { Ok(fraiche) })
        })
    };
    let sessions = session_bandcamp("bandcamp-2121-avec", perimee, Some(reresolve)).await;
    let (statut, type_, octets) = get(&sessions, "bandcamp-2121-avec").await;
    // `bytes=0-` : le relais répond 206 depuis l'octet 0, comme à tout renderer
    // qui demande une plage ; l'essentiel est un succès servi, plus un 502.
    assert!(
        matches!(statut, 200 | 206),
        "resignée, la piste est servie (statut {statut})"
    );
    assert_eq!(type_, "audio/mpeg");
    assert_eq!(octets, corps, "octets audio relayés tels quels");
    assert_eq!(appels.load(std::sync::atomic::Ordering::SeqCst), 1);
    let journal = capture.depuis(repere);
    assert!(
        journal.contains("proxy_url_reresolved"),
        "la nouvelle résolution est journalisée :\n{journal}"
    );
    assert!(
        !journal.contains("proxy_upstream_expired_no_reresolver"),
        "avec un mécanisme, pas d'avertissement d'abandon :\n{journal}"
    );
}
