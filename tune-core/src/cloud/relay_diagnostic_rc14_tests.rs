//! Diagnostic rc14 (#8963, #9174, #7398, #9273) : les lignes que le relais
//! écrit pour départager les hypothèses doivent être émises.
//!
//! - `relay_demande_de_flux_recue` : le pont a demandé un flux (lequel) ;
//! - `relay_flux_bilan` : octets, premier octet, attente vers le pont, débit ;
//! - `relay_commande_de_zone` : une commande de zone relayée et sa réponse.

use super::*;
use axum::Router;
use axum::body::Body;
use axum::http::Uri;
use axum::response::{IntoResponse, Response};
use std::sync::Mutex as StdMutex;

const OCTETS_DU_FLUX: usize = 3000;

#[derive(Clone, Default)]
struct Journal(Arc<StdMutex<Vec<u8>>>);
impl Journal {
    fn lignes(&self, motif: &str) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .filter(|l| l.contains(motif))
            .map(str::to_owned)
            .collect()
    }
}
impl std::io::Write for Journal {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Journal {
    type Writer = Journal;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn capturer() -> (Journal, tracing::subscriber::DefaultGuard) {
    crate::journal_de_test::fiabiliser_la_capture();
    let journal = Journal::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(journal.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    (journal.clone(), tracing::subscriber::set_default(abonne))
}

/// Le faux serveur local : un flux de `OCTETS_DU_FLUX` octets sous `/stream/`,
/// un 409 sous `/api/v1/zones/…`.
async fn repondre(uri: Uri) -> Response {
    if uri.path().starts_with("/stream/") {
        (
            [("content-type", "audio/wav")],
            Body::from(vec![0u8; OCTETS_DU_FLUX]),
        )
            .into_response()
    } else {
        (
            axum::http::StatusCode::CONFLICT,
            [("content-type", "application/json")],
            r#"{"error":"browser_current_changed"}"#,
        )
            .into_response()
    }
}

async fn serveur_local() -> u16 {
    let app = Router::new().fallback(repondre);
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.unwrap();
    });
    port
}

async fn client(port: u16) -> (RelayClient, mpsc::Receiver<Trame>) {
    let mut c = RelayClient::new("srv".into(), "jeton".into(), "ws://x".into(), port);
    c.http_flux = client_de_flux(Duration::from_secs(5));
    let (tx, rx) = mpsc::channel::<Trame>(256);
    *c.ws_tx.lock().await = Some(tx);
    (c, rx)
}

/// Lit les trames jusqu'à `relay.stream_end` (flux) ou la réponse d'API.
async fn attendre(rx: &mut mpsc::Receiver<Trame>, fin: &str) {
    loop {
        let t = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("aucune trame vers le pont")
            .expect("canal fermé")
            .en_texte();
        if t.starts_with("BINARY:") {
            continue;
        }
        if t.contains(fin) {
            return;
        }
    }
}

/// #8963 / #9174 — une demande de flux relayée laisse deux lignes : la
/// demande reçue (flux court, plage) et le bilan (octets, débit, attente).
#[tokio::test]
async fn une_demande_de_flux_relayee_laisse_sa_demande_et_son_bilan() {
    let (journal, _garde) = capturer();
    let port = serveur_local().await;
    let (c, mut rx) = client(port).await;
    c.handle_message(
        &serde_json::json!({
            "type": "relay.stream_request",
            "id": "s-rc14",
            "stream_id": "038e9337cafe.wav",
            "range": "bytes=44-",
            "user_agent": "AppleCoreMedia/1.0",
        })
        .to_string(),
    )
    .await;
    attendre(&mut rx, "relay.stream_end").await;
    // Le bilan est écrit avant l'émission de `relay.stream_end`.
    let demandes = journal.lignes("relay_demande_de_flux_recue");
    assert_eq!(demandes.len(), 1, "{demandes:?}");
    assert!(
        demandes[0].contains("flux=038e9337") && demandes[0].contains("range=\"bytes=44-\""),
        "la demande ne dit ni le flux ni la plage : {}",
        demandes[0]
    );
    assert!(
        !demandes[0].contains("038e9337cafe"),
        "l'identifiant entier est recopié : {}",
        demandes[0]
    );
    let bilans = journal.lignes("relay_flux_bilan");
    assert_eq!(bilans.len(), 1, "{bilans:?}");
    let b = &bilans[0];
    for attendu in [
        "flux=038e9337",
        "statut=200",
        &format!("octets={OCTETS_DU_FLUX}"),
        "premier_octet_ms=Some(",
        "attente_relais_ms=",
        "debit_kbit_s=",
        "fin=\"complet\"",
    ] {
        assert!(b.contains(attendu), "« {attendu} » absent du bilan : {b}");
    }
    // L'instant est global au processus : un test voisin qui relaie un autre
    // flux peut l'avoir remplacé. Seule la présence est garantie ici.
    assert!(
        super::super::activite_du_relais::derniere_demande_de_flux_relayee().is_some(),
        "la demande de flux n'est pas notée"
    );
}

/// #9273 / #7398 — une commande de zone relayée dit son statut et la taille
/// du corps rendu au pont ; une position (`browser-playing`) n'est pas écrite.
#[tokio::test]
async fn une_commande_de_zone_relayee_dit_son_statut() {
    let (journal, _garde) = capturer();
    let port = serveur_local().await;
    let (c, mut rx) = client(port).await;
    for chemin in [
        "/api/v1/zones/27/browser-next",
        "/api/v1/zones/27/browser-playing",
    ] {
        c.handle_message(
            &serde_json::json!({
                "type": "relay.request",
                "id": format!("r-{chemin}"),
                "method": "POST",
                "path": chemin,
                "headers": {"content-type": "application/json"},
                "body": "{\"current_stream_id\":\"x\"}",
            })
            .to_string(),
        )
        .await;
        attendre(&mut rx, "relay.response").await;
    }
    let lignes = journal.lignes("relay_commande_de_zone");
    assert_eq!(lignes.len(), 1, "{lignes:?}");
    let l = &lignes[0];
    assert!(
        l.contains("chemin=\"/api/v1/zones/27/browser-next\"")
            && l.contains("statut=409")
            && l.contains("octets=35"),
        "commande relayée mal décrite : {l}"
    );
    assert!(
        super::super::activite_du_relais::depuis_la_derniere_requete_relayee_ms().is_some(),
        "la requête relayée n'est pas notée"
    );
}

#[test]
fn seules_les_ecritures_de_zone_sont_journalisees() {
    assert_eq!(
        commande_de_zone_a_journaliser("POST", "/api/v1/zones/3/next?x=1").as_deref(),
        Some("/api/v1/zones/3/next")
    );
    assert_eq!(
        commande_de_zone_a_journaliser("GET", "/api/v1/zones/3"),
        None
    );
    assert_eq!(
        commande_de_zone_a_journaliser("POST", "/api/v1/zones/3/browser-playing"),
        None
    );
    assert_eq!(
        commande_de_zone_a_journaliser("POST", "/api/v1/zones/3/volume"),
        None
    );
    assert_eq!(
        commande_de_zone_a_journaliser("POST", "/api/v1/playlists/3"),
        None
    );
}
