//! #6468 — le journal du mandataire et de la sortie navigateur ne porte que
//! l'hôte et le type de source d'un flux, jamais son URL complète.
//!
//! L'URL d'un flux de service (Bandcamp, podcast privé, CDN Qobuz/Tidal)
//! porte souvent une signature ou un jeton dans sa requête. Le journal se
//! joint au forum : il doit rester lisible sans rien en révéler.
//!
//! Aucun réseau : les URL visent des hôtes `.test` ou ne sont jamais
//! ouvertes, et la sortie est un onglet de navigateur qui n'existe pas.
use std::sync::{Arc, Mutex};

use super::test_orchestrator;
use crate::db::zone_repo::ZoneRepo;

const JETON: &str = "secret-jeton-6468";
const SIGNATURE: &str = "sig-6468-c0ffee";

#[derive(Clone, Default)]
struct Journal(Arc<Mutex<Vec<u8>>>);
impl Journal {
    fn texte(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
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

/// Pose l'abonné de capture sur le fil du test (runtime `current_thread`).
fn capturer() -> (Journal, tracing::subscriber::DefaultGuard) {
    crate::journal_de_test::fiabiliser_la_capture();
    let journal = Journal::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(journal.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let garde = tracing::subscriber::set_default(abonne);
    (journal, garde)
}

fn sans_secret(texte: &str) {
    for interdit in [JETON, SIGNATURE, "token=", "sig="] {
        assert!(
            !texte.contains(interdit),
            "le journal porte « {interdit} » : l'URL complète du flux y est passée\n{texte}"
        );
    }
}

fn demande(zone_id: i64, source: &str, url: &str) -> super::super::PlayRequest {
    super::super::PlayRequest {
        zone_id,
        output_device_id: None,
        track_id: None,
        source: Some(source.into()),
        source_id: Some(url.into()),
        title: Some("Un titre".into()),
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms: Some(212_000),
        seek_ms: None,
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
        album_ref: None,
    }
}

/// La ligne de #6468 : un épisode relayé vers un onglet.
#[tokio::test]
async fn le_relais_vers_le_navigateur_ne_journalise_que_l_hote() {
    let (journal, _garde) = capturer();
    let orch = test_orchestrator();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Ce PC", Some("browser"), None)
        .unwrap();
    let url = format!("https://cdn.podcast-prive.test/ep/42.mp3?token={JETON}&sig={SIGNATURE}");
    let resolved = orch
        .resolve_direct_url(&demande(zone_id, "podcast", &url))
        .await
        .unwrap();
    assert!(
        resolved.stream_id.is_some(),
        "un onglet passe par le mandataire"
    );

    let texte = journal.texte();
    let ligne = texte
        .lines()
        .find(|l| l.contains("direct_proxy_for_browser_output"))
        .unwrap_or_else(|| panic!("ligne direct_proxy_for_browser_output absente :\n{texte}"));
    assert!(
        ligne.contains("cdn.podcast-prive.test"),
        "l'hôte doit rester : {ligne}"
    );
    assert!(
        ligne.contains("podcast"),
        "le type de source doit rester : {ligne}"
    );
    assert!(
        texte.contains("direct_proxy_session"),
        "la ligne voisine du relais doit être là :\n{texte}"
    );
    sans_secret(&texte);
}

/// Bandcamp : l'URL de flux est SIGNÉE. Ni le relais ni l'avertissement de
/// signature périmée ne la recopient.
#[tokio::test]
async fn le_relais_bandcamp_ne_journalise_que_l_hote() {
    let (journal, _garde) = capturer();
    let orch = test_orchestrator();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Ce PC", Some("browser"), None)
        .unwrap();
    let url = format!(
        "https://t4.bcbits.com/stream/0123456789abcdef/mp3-128/1234567?p=0&ts=1&token={JETON}&sig={SIGNATURE}"
    );
    let resolved = orch
        .resolve_direct_url(&demande(zone_id, "bandcamp", &url))
        .await
        .unwrap();
    assert!(
        resolved.stream_id.is_some(),
        "un onglet passe par le mandataire"
    );

    let texte = journal.texte();
    for evenement in [
        "bandcamp_proxy_for_network_or_browser_output",
        "bandcamp_signature_perimee_sans_reference",
    ] {
        let ligne = texte
            .lines()
            .find(|l| l.contains(evenement))
            .unwrap_or_else(|| panic!("ligne {evenement} absente :\n{texte}"));
        assert!(
            ligne.contains("t4.bcbits.com"),
            "l'hôte doit rester : {ligne}"
        );
        assert!(
            ligne.contains("bandcamp"),
            "le type de source doit rester : {ligne}"
        );
    }
    sans_secret(&texte);
}

/// Le chien de garde d'une session jamais consommée lisait l'URL amont du
/// mandataire en entier.
#[tokio::test]
async fn la_session_de_mandataire_jamais_consommee_ne_journalise_que_l_hote() {
    use crate::http::streamer::{AudioStreamer, StreamInfo};

    let (journal, _garde) = capturer();
    let streamer = Arc::new(AudioStreamer::new(0));
    let info = StreamInfo {
        format: "flac".to_string(),
        mime_type: "audio/flac".to_string(),
        ..StreamInfo::default()
    };
    let amont = format!("https://cdn.service.test/piste.flac?token={JETON}&sig={SIGNATURE}");
    let session = streamer.create_proxy_session(info, amont, false).await;
    let tache = super::arm_local_stream_consumer_watch(
        streamer.clone(),
        session,
        7,
        "local:test".to_string(),
        std::time::Duration::ZERO,
    )
    .await
    .expect("la garde s'arme");
    assert!(
        tache.await.expect("tâche de garde"),
        "la session n'a rien servi"
    );

    let texte = journal.texte();
    let ligne = texte
        .lines()
        .find(|l| l.contains("local_stream_never_consumed"))
        .unwrap_or_else(|| panic!("ligne local_stream_never_consumed absente :\n{texte}"));
    assert!(
        ligne.contains("cdn.service.test"),
        "l'hôte doit rester : {ligne}"
    );
    sans_secret(&texte);
}
