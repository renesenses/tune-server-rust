//! #4971 — le journal INFO doit dire QUAND un renderer cesse de répondre, et
//! ce que Tune lui avait envoyé juste avant.
//!
//! Banc : un faux renderer répond à deux actions, puis son port se ferme
//! (le Diretta Renderer du NUC qui cesse d'écouter). Les actions suivantes
//! doivent produire UNE ligne `dlna_contact_perdu`, pas une par sondage, qui
//! nomme le dernier échange réussi et l'historique.

use super::*;
use axum::{Router, routing::post};
use std::sync::Mutex;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
    fn subscribe(&self) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(self.clone())
                .with_ansi(false)
                .with_max_level(tracing::Level::INFO)
                .finish(),
        )
    }
}

const REPONSE: &str = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:SetVolumeResponse xmlns:u="urn:schemas-upnp-org:service:RenderingControl:1"></u:SetVolumeResponse></s:Body></s:Envelope>"#;

#[tokio::test]
async fn un_renderer_qui_cesse_d_ecouter_se_dit_une_fois_au_journal_info() {
    let journal = Capture::default();
    let _garde = journal.subscribe();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new().route("/control", post(|| async { REPONSE }));
    let vie = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    // Aucun répondeur SSDP : la redécouverte ciblée (#3829) échoue vite.
    let port_ssdp = {
        let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        s.local_addr().unwrap().port()
    };
    let base = format!("http://127.0.0.1:{port}");
    let sortie = DlnaOutput::new(
        "Tune".into(),
        "uuid:diretta-renderer-fc802a162cc1574c".into(),
        "127.0.0.1".into(),
        format!("{base}/control"),
        format!("{base}/control"),
        None,
    )
    .with_redecouverte(port_ssdp, std::time::Duration::from_millis(200));

    sortie.set_volume(0.5).await.expect("renderer vivant");
    sortie.set_volume(0.6).await.expect("renderer vivant");

    // Le renderer cesse d'écouter.
    vie.abort();
    let _ = vie.await;
    for _ in 0..50 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    for v in [0.7, 0.8, 0.9] {
        sortie
            .set_volume(v)
            .await
            .expect_err("plus personne n'écoute");
    }

    let texte = journal.text();
    let pertes: Vec<&str> = texte
        .lines()
        .filter(|l| l.contains("dlna_contact_perdu"))
        .collect();
    assert_eq!(
        pertes.len(),
        1,
        "UNE ligne pour UNE perte, pas une par envoi : {texte}"
    );
    let ligne = pertes[0];
    assert!(
        ligne.contains(" WARN "),
        "une perte se lit en WARN : {ligne}"
    );
    assert!(
        ligne.contains("dernier_ok_action=\"SetVolume\""),
        "la ligne doit nommer le dernier échange réussi : {ligne}"
    );
    assert!(
        ligne.contains("issue=\"refus\""),
        "un port fermé est un refus : {ligne}"
    );
    assert!(
        ligne.contains("SetVolume/ok/") && ligne.contains("SetVolume/refus/"),
        "l'historique doit porter les échanges d'avant la perte : {ligne}"
    );
}
