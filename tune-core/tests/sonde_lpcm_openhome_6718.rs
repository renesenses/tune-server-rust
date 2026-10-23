//! #6718 — `dlna_accepte_lpcm` pour un renderer OPENHOME : on lit ce qu'il
//! annonce (`Playlist:1 ProtocolInfo`, sinon le `Sink` du `ConnectionManager`)
//! au lieu de répondre « inconnu » par type.
//!
//! Le LUXMAN NT-07 du fil 2363 : un traitement actif sur une piste Qobuz
//! imposait un pré-transcodage COMPLET (6 s pour 155 Mo) avant le premier
//! son, parce que `capacite_lpcm("openhome")` valait `Inconnu` et fermait le
//! WAV progressif (#5080). Décision de Bertrand du 22/10 : sonder, puis WAV
//! progressif si le renderer accepte le LPCM, sinon le comportement actuel.
//!
//! Témoins COMPORTEMENTAUX : la vraie méthode publique, un vrai registre, une
//! vraie `OpenHomeOutput` contre un faux renderer SOAP sur `127.0.0.1`.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_core::orchestrator::PlaybackOrchestrator;
use tune_core::outputs::registry::OutputRegistry;

const ID: &str = "uuid:luxman-nt07-6718";

fn orchestrateur(registre: OutputRegistry) -> PlaybackOrchestrator {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        backend,
        Arc::new(tune_core::playback::PlaybackManager::new()),
        Arc::new(tune_core::http::streamer::AudioStreamer::new(0)),
        Arc::new(Mutex::new(
            tune_core::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(Mutex::new(registre)),
        None,
    )
}

/// Un faux renderer : `/Playlist` répond `ProtocolInfo` par `playlist`
/// (`None` : faute SOAP 500), `/ConnectionManager` répond `GetProtocolInfo`
/// par `sink`.
async fn renderer(playlist: Option<&'static str>, sink: &'static str) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut brut = Vec::new();
                let mut tampon = [0u8; 4096];
                while let Ok(n) = sock.read(&mut tampon).await {
                    if n == 0 {
                        break;
                    }
                    brut.extend_from_slice(&tampon[..n]);
                    let texte = String::from_utf8_lossy(&brut);
                    let Some(fin) = texte.find("\r\n\r\n") else {
                        continue;
                    };
                    let attendu: usize = texte
                        .lines()
                        .find_map(|l| {
                            let (nom, valeur) = l.split_once(':')?;
                            nom.eq_ignore_ascii_case("content-length")
                                .then(|| valeur.trim().parse().ok())?
                        })
                        .unwrap_or(0);
                    if brut.len() >= fin + 4 + attendu {
                        break;
                    }
                }
                let texte = String::from_utf8_lossy(&brut).to_string();
                let (statut, corps) = if texte.starts_with("POST /Playlist") {
                    match playlist {
                        Some(v) => (
                            "200 OK",
                            format!(
                                "<s:Envelope><s:Body><u:ProtocolInfoResponse><Value>{v}</Value>\
                                 </u:ProtocolInfoResponse></s:Body></s:Envelope>"
                            ),
                        ),
                        None => (
                            "500 Internal Server Error",
                            "<s:Envelope><s:Body><s:Fault><faultstring>UPnPError</faultstring>\
                             </s:Fault></s:Body></s:Envelope>"
                                .to_string(),
                        ),
                    }
                } else {
                    (
                        "200 OK",
                        format!(
                            "<s:Envelope><s:Body><u:GetProtocolInfoResponse><Sink>{sink}</Sink>\
                             </u:GetProtocolInfoResponse></s:Body></s:Envelope>"
                        ),
                    )
                };
                let reponse = format!(
                    "HTTP/1.1 {statut}\r\nContent-Type: text/xml\r\nConnection: close\r\n\
                     Content-Length: {}\r\n\r\n{corps}",
                    corps.len()
                );
                let _ = sock.write_all(reponse.as_bytes()).await;
                let _ = sock.flush().await;
            });
        }
    });
    port
}

async fn reponse(
    playlist: Option<&'static str>,
    sink: &'static str,
    avec_connection_manager: bool,
    hi_res: bool,
) -> bool {
    let port = renderer(playlist, sink).await;
    let mut services = HashMap::new();
    services.insert("playlist".to_string(), "/Playlist".to_string());
    if avec_connection_manager {
        services.insert(
            "connectionmanager".to_string(),
            "/ConnectionManager".to_string(),
        );
    }
    let mut registre = OutputRegistry::new();
    registre.register(Box::new(tune_core::outputs::openhome::OpenHomeOutput::new(
        "LUXMAN NT-07".into(),
        ID.into(),
        "127.0.0.1".into(),
        port,
        services,
        None,
        HashMap::new(),
    )));
    orchestrateur(registre).dlna_accepte_lpcm(ID, hi_res).await
}

const L16_L24: &str = "http-get:*:audio/L16;rate=44100;channels=2:*,\
                       http-get:*:audio/L24;rate=96000;channels=2:*,\
                       http-get:*:audio/flac:*";
const L16_SEUL: &str = "http-get:*:audio/L16;rate=44100;channels=2:*,http-get:*:audio/flac:*";
const SANS_LPCM: &str = "http-get:*:audio/flac:*,http-get:*:audio/mpeg:*";

/// 🔴 Le témoin de #6718. Rouge avant : `false` par type, quoi qu'annonce le
/// renderer — le pré-transcodage complet était donc obligatoire.
#[tokio::test]
async fn un_renderer_openhome_qui_annonce_le_lpcm_l_accepte() {
    assert!(
        reponse(Some(L16_L24), "", false, false).await,
        "ProtocolInfo annonce du L16 : le WAV progressif 16 bits doit s'ouvrir"
    );
    assert!(
        reponse(Some(L16_L24), "", false, true).await,
        "ProtocolInfo annonce du L24 : le WAV progressif 24 bits doit s'ouvrir"
    );
}

/// La même lecture que la sonde DLNA : L16 seul ne couvre pas 24 bits, et un
/// renderer sans LPCM garde le comportement actuel.
#[tokio::test]
async fn la_lecture_suit_ce_qui_est_annonce_sinon_le_repli_reste() {
    assert!(reponse(Some(L16_SEUL), "", false, false).await);
    assert!(
        !reponse(Some(L16_SEUL), "", false, true).await,
        "L16 seul ne couvre pas 24 bits (#1137)"
    );
    assert!(
        !reponse(Some(SANS_LPCM), "", false, false).await,
        "aucun LPCM annoncé : repli sur le pré-transcodage"
    );
}

/// Sans `ProtocolInfo` OpenHome (faute SOAP), le `Sink` du
/// `ConnectionManager` du même appareil fait foi ; sans l'un ni l'autre,
/// c'est non, comme une sonde DLNA inconcluante.
#[tokio::test]
async fn le_connection_manager_prend_le_relais_et_rien_vaut_non() {
    assert!(reponse(None, L16_SEUL, true, false).await);
    assert!(!reponse(None, L16_SEUL, false, false).await);
}
