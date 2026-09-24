//! #4894 — `dlna_accepte_lpcm` pour une sortie connue qui n'est PAS un
//! `DlnaOutput`. Avant : `false` pour toutes, repli du `downcast` raté. Après :
//! la réponse établie par type de sortie (`capacite_lpcm_par_sortie`), et une
//! vraie `DlnaOutput` suit toujours sa sonde `GetProtocolInfo`.
//!
//! Témoins COMPORTEMENTAUX : la vraie méthode publique, un vrai registre, un
//! vrai serveur SOAP local pour la `DlnaOutput`.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_core::orchestrator::PlaybackOrchestrator;
use tune_core::outputs::mock::MockOutput;
use tune_core::outputs::registry::OutputRegistry;

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

/// Ce que `dlna_accepte_lpcm` rend pour une sortie de ce type, enregistrée.
async fn reponse_pour(output_type: &str, hi_res: bool) -> bool {
    let mut registre = OutputRegistry::new();
    registre.register(Box::new(
        MockOutput::new("sortie-4894", "Salon").with_type(output_type),
    ));
    orchestrateur(registre)
        .dlna_accepte_lpcm("sortie-4894", hi_res)
        .await
}

/// 🔴 Rouge avant : un Chromecast lit le WAV (LPCM) d'après la documentation
/// Google Cast, et la méthode répondait `false` par repli.
#[tokio::test]
async fn un_chromecast_accepte_le_lpcm_16_bits() {
    assert!(
        reponse_pour("chromecast", false).await,
        "Chromecast : « WAV (LPCM) » est un format Cast documenté"
    );
}

/// 🔴 Rouge avant : même défaut pour BluOS.
#[tokio::test]
async fn un_lecteur_bluos_accepte_le_lpcm_16_bits() {
    assert!(
        reponse_pour("bluos", false).await,
        "BluOS : le WAV est un format Bluesound documenté"
    );
}

/// Aucune profondeur n'est documentée au-delà : inconnu, donc NON.
#[tokio::test]
async fn chromecast_et_bluos_restent_prudents_au_dela_de_16_bits() {
    assert!(!reponse_pour("chromecast", true).await);
    assert!(!reponse_pour("bluos", true).await);
}

/// Témoin négatif : établi « refuse » (slimproto annonce FLAC en dur) ou
/// « inconnu » (LMS, OpenHome, une sortie « dlna » sans Sink) — `false`.
#[tokio::test]
async fn refuse_et_inconnu_valent_non() {
    for t in ["slimproto", "squeezebox", "openhome", "dlna", "mock"] {
        assert!(!reponse_pour(t, false).await, "{t} doit rester à non");
    }
}

/// La sortie ABSENTE du registre garde sa réponse d'avant : `true`.
#[tokio::test]
async fn une_sortie_absente_reste_presumee_capable() {
    assert!(
        orchestrateur(OutputRegistry::new())
            .dlna_accepte_lpcm("personne", false)
            .await
    );
}

/// Un ConnectionManager local qui répond toujours le même Sink.
async fn renderer_qui_annonce(sink: &'static str) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut brut = Vec::new();
                let mut tampon = [0u8; 4096];
                loop {
                    let Ok(n) = sock.read(&mut tampon).await else {
                        break;
                    };
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
                let corps = format!(
                    concat!(
                        r#"<?xml version="1.0"?>"#,
                        r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">"#,
                        "<s:Body><u:GetProtocolInfoResponse><Sink>{}</Sink>",
                        "</u:GetProtocolInfoResponse></s:Body></s:Envelope>"
                    ),
                    sink
                );
                let reponse = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nConnection: close\r\n\
                     Content-Length: {}\r\n\r\n{corps}",
                    corps.len()
                );
                let _ = sock.write_all(reponse.as_bytes()).await;
                let _ = sock.flush().await;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            });
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (format!("http://127.0.0.1:{port}"), handle)
}

async fn reponse_dlna(sink: &'static str, hi_res: bool) -> bool {
    let (base, handle) = renderer_qui_annonce(sink).await;
    let mut registre = OutputRegistry::new();
    registre.register(Box::new(tune_core::outputs::dlna::DlnaOutput::new(
        "Salon".into(),
        "uuid:dlna-4894".into(),
        "127.0.0.1".into(),
        format!("{base}/AVTransport"),
        format!("{base}/RenderingControl"),
        Some(format!("{base}/ConnectionManager")),
    )));
    let r = orchestrateur(registre)
        .dlna_accepte_lpcm("uuid:dlna-4894", hi_res)
        .await;
    handle.abort();
    r
}

/// Une vraie `DlnaOutput` suit sa sonde, inchangée : L16 annoncé ⇒ oui en
/// 16 bits, non en 24 ; FLAC seul ⇒ non.
#[tokio::test]
async fn une_vraie_dlna_output_suit_toujours_sa_sonde() {
    const L16: &str = "http-get:*:audio/L16;rate=44100;channels=2:*,http-get:*:audio/flac:*";
    const FLAC_SEUL: &str = "http-get:*:audio/flac:*,http-get:*:audio/mpeg:*";
    assert!(reponse_dlna(L16, false).await, "L16 annoncé, 16 bits");
    assert!(
        !reponse_dlna(L16, true).await,
        "L16 seul ne couvre pas 24 bits"
    );
    assert!(!reponse_dlna(FLAC_SEUL, false).await, "aucun LPCM annoncé");
}
