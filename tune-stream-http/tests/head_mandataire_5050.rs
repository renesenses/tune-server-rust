//! #5050 — le `HEAD` d'une session MANDATAIRE (Qobuz/Tidal relayés) dit ce
//! que le `GET` dit.
//!
//! Journaux de FabienM (Beosound Stage, zone Parents, fils 1943, 1981, 2013) :
//! chaque `stream_head_request` d'une piste Qobuz porte `file_size=None`, et
//! le `HEAD` annonçait pourtant `Accept-Ranges: bytes` et `DLNA.ORG_OP=01` —
//! une recherche par octets promise, sans la longueur qui la rend possible.
//! Le `GET`, lui, recopiait le `Content-Length` du CDN.
//!
//! Décision du 29/09 : longueur connue ⇒ `Content-Length` au `HEAD`, comme au
//! `GET` ; longueur inconnue ⇒ ni `Accept-Ranges` ni `OP=01`. La longueur est
//! celle que le CDN a rapportée au premier `GET` relayé : aucune requête amont
//! n'est ajoutée pour le `HEAD`.
//!
//! Le même scénario éprouve la ligne de diagnostic `proxy_get_amont` : statut
//! et en-têtes du CDN, plage demandée, et AUCUNE URL (l'URL amont est signée).
//!
//! Un seul `#[tokio::test]` : l'abonné de journal est global au binaire.
//!
//! Contre-épreuves :
//! - rendre au `HEAD` son ancienne branche (toujours `Accept-Ranges`/`OP=01`)
//!   fait tomber l'étape « longueur inconnue » ;
//! - retirer `session.noter_longueur_amont(total)` de `proxy_stream` fait
//!   tomber l'étape « longueur connue ».
//!
//! `tune-stream-http` n'a PAS `autotests = false` : ce fichier est compilé
//! sans déclaration `[[test]]`.
//!
//! Refs renesenses/tune-server-rust#5050

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

const SECRET: &str = "etsp=1790000000&sig=cafe5050";

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
        sample_rate: 44_100,
        bit_depth: 16,
        channels: 2,
        ..StreamInfo::default()
    };
    let session = StreamSession::new(id.to_string(), info, false, 16);
    *session.proxy_url.lock().await = Some(amont);
    Arc::new(tokio::sync::Mutex::new(
        [(id.to_string(), Arc::new(session))].into_iter().collect(),
    ))
}

async fn head(sessions: &SharedSessions, id: &str) -> axum::http::HeaderMap {
    let rep = tune_stream_http::handle_head(
        Path(format!("{id}.flac")),
        State(sessions.clone()),
        axum::http::HeaderMap::new(),
    )
    .await;
    assert_eq!(rep.status(), axum::http::StatusCode::OK);
    rep.headers().clone()
}

async fn get(sessions: &SharedSessions, id: &str, range: Option<&str>) -> (u16, usize) {
    let mut h = axum::http::HeaderMap::new();
    h.insert(
        "User-Agent",
        "GStreamer souphttpsrc 1.16.2 libsoup/2.62.3"
            .parse()
            .unwrap(),
    );
    if let Some(r) = range {
        h.insert("Range", r.parse().unwrap());
    }
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
    }
    (statut, n)
}

fn entete(h: &axum::http::HeaderMap, nom: &str) -> Option<String> {
    h.get(nom).and_then(|v| v.to_str().ok()).map(str::to_string)
}

#[tokio::test]
async fn le_head_mandataire_dit_la_longueur_quand_il_la_sait_et_ne_promet_rien_sinon() {
    let capture = JournalCapture::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(abonne)
        .expect("ce binaire ne contient qu'un test : l'abonné global est libre");
    let evenement = ["proxy", "get", "amont"].join("_");

    const TOTAL: usize = 64_000;
    let corps: Vec<u8> = (0..TOTAL).map(|i| (i % 251) as u8).collect();
    let amont = cdn(corps).await;
    let id = "mandataire-5050";
    let sessions = session_mandataire(id, amont).await;

    // 1. Longueur inconnue : aucun GET n'a encore rapporté la réponse du CDN.
    let h = head(&sessions, id).await;
    assert_eq!(
        entete(&h, "Accept-Ranges"),
        None,
        "longueur inconnue : le HEAD ne doit pas promettre de Range"
    );
    assert_eq!(
        entete(&h, "contentFeatures.dlna.org").as_deref(),
        Some("DLNA.ORG_OP=00;DLNA.ORG_FLAGS=01700000000000000000000000000000"),
        "longueur inconnue : pas de DLNA.ORG_OP=01"
    );
    assert_eq!(entete(&h, "Content-Length"), None);

    // 2. Premier GET relayé (la sonde des VU-mètres, sans Range) : 200 et
    //    Content-Length du CDN ; la ligne de diagnostic le dit, sans URL.
    let repere = capture.repere();
    let (statut, lus) = get(&sessions, id, None).await;
    assert_eq!((statut, lus), (200, TOTAL));
    let journal = capture.depuis(repere);
    let ligne = journal
        .lines()
        .find(|l| l.contains(&evenement))
        .unwrap_or_else(|| panic!("aucune ligne {evenement} :\n{journal}"));
    for attendu in [
        "range=\"-\"",
        "statut_amont=200",
        &format!("content_length_amont=Some({TOTAL})"),
        "accept_ranges_amont=\"bytes\"",
        "statut_servi=200",
        &format!("content_length_servi={TOTAL}"),
        &format!("longueur_retenue=Some({TOTAL})"),
    ] {
        assert!(ligne.contains(attendu), "« {attendu} » absent : {ligne}");
    }
    assert!(
        !ligne.contains("sig=") && !ligne.contains("etsp") && !ligne.contains("127.0.0.1"),
        "la ligne ne doit porter aucune URL signée : {ligne}"
    );
    assert!(
        !journal.contains("cafe5050"),
        "le secret de l'URL amont a fui dans le journal :\n{journal}"
    );

    // 3. Longueur connue : le HEAD annonce Content-Length, comme le GET.
    let h = head(&sessions, id).await;
    assert_eq!(
        entete(&h, "Content-Length").as_deref(),
        Some(TOTAL.to_string().as_str()),
        "longueur connue : le HEAD doit dire le Content-Length du GET"
    );
    assert_eq!(entete(&h, "Accept-Ranges").as_deref(), Some("bytes"));
    assert_eq!(
        entete(&h, "contentFeatures.dlna.org").as_deref(),
        Some("DLNA.ORG_OP=01;DLNA.ORG_FLAGS=01700000000000000000000000000000"),
    );

    // 4. Une reprise relayée (206) : la ligne dit la plage et le Content-Range
    //    du CDN ; la longueur retenue reste le TOTAL, pas la tranche.
    let repere = capture.repere();
    let (statut, lus) = get(&sessions, id, Some("bytes=40000-")).await;
    assert_eq!((statut, lus), (206, TOTAL - 40_000));
    let journal = capture.depuis(repere);
    let ligne = journal
        .lines()
        .find(|l| l.contains(&evenement))
        .unwrap_or_else(|| panic!("aucune ligne {evenement} :\n{journal}"));
    for attendu in [
        "range=\"bytes=40000-\"",
        "statut_amont=206",
        &format!("content_range_amont=\"bytes 40000-{}/{TOTAL}\"", TOTAL - 1),
        "statut_servi=206",
        &format!("longueur_retenue=Some({TOTAL})"),
    ] {
        assert!(ligne.contains(attendu), "« {attendu} » absent : {ligne}");
    }

    // 5. Une nouvelle session dont la reprise est le PREMIER GET : la longueur
    //    se déduit du total du Content-Range, pas du Content-Length partiel.
    let id2 = "mandataire-5050-reprise";
    let amont2 = cdn((0..TOTAL).map(|i| (i % 7) as u8).collect()).await;
    let sessions2 = session_mandataire(id2, amont2).await;
    let (statut, _) = get(&sessions2, id2, Some("bytes=1000-")).await;
    assert_eq!(statut, 206);
    let h = head(&sessions2, id2).await;
    assert_eq!(
        entete(&h, "Content-Length").as_deref(),
        Some(TOTAL.to_string().as_str()),
        "la longueur d'une reprise est le total du Content-Range"
    );
}
