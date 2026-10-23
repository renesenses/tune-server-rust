//! #6599 — d'où vient la cible d'une réduction réseau, dit par le journal.
//!
//! Le rapport de l'Eversolo DMP-A6 (fil 2356) portait trois
//! `reduction_de_canaux_pour_le_renderer … canaux_cible=2` sans AUCUNE ligne
//! de sonde pour l'A6 et sans le Sink d'aucun renderer : la réponse venait du
//! cache par appareil, muet, ou de la disposition déclarée pour la zone, et
//! rien ne permettait de dire laquelle. Ces témoins exigent que chaque cas le
//! dise.
//!
//! Binaire à lui seul, UN SEUL test : il pose un abonné `tracing` GLOBAL (le
//! cache d'intérêt des points d'appel est un état de processus, cf. #2665).

use std::sync::{Arc, OnceLock};

#[path = "banc_canaux_reseau_4573.rs"]
mod banc;
use banc::*;

//
// Le rapport de l'Eversolo DMP-A6 (fil 2356) portait trois
// `reduction_de_canaux_pour_le_renderer … canaux_cible=2` sans AUCUNE ligne de
// sonde pour l'A6 et sans le Sink d'aucun renderer : la réponse venait du
// cache par appareil, muet, ou de la disposition déclarée pour la zone, et
// rien ne permettait de dire laquelle. Ces témoins exigent que chaque cas le
// dise.

#[derive(Clone, Default)]
struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);

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
    fn vider(&self) {
        self.0.lock().unwrap().clear();
    }
    fn lignes(&self, evenement: &str) -> Vec<String> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .filter(|l| l.contains(evenement))
            .map(str::to_string)
            .collect()
    }
}

/// Deux résolutions de la même piste sur le même orchestrateur, journal
/// capturé. `disposition` : la disposition déclarée pour la zone, s'il y en a.
async fn deux_lectures_journalisees(sink: &'static str, disposition: Option<&str>) -> Capture {
    static CAPTURE: OnceLock<Capture> = OnceLock::new();
    let capture = CAPTURE
        .get_or_init(|| {
            let capture = Capture::default();
            tracing::subscriber::set_global_default(
                tracing_subscriber::fmt()
                    .with_writer(capture.clone())
                    .with_ansi(false)
                    .with_max_level(tracing::Level::INFO)
                    .finish(),
            )
            .expect("abonné global unique de ce binaire");
            capture
        })
        .clone();
    capture.vider();
    let (base, handle) = renderer_qui_annonce(sink).await;
    let (backend, zone_id, _track_id) = base_avec_piste(6);
    if let Some(d) = disposition {
        tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone())
            .set(&tune_core::audio::canaux_declares::cle_de_zone(zone_id), d)
            .unwrap();
    }
    let mut registre = tune_core::outputs::registry::OutputRegistry::new();
    registre.register(Box::new(tune_core::outputs::dlna::DlnaOutput::new(
        "Home Theater".into(),
        UDN.into(),
        "127.0.0.1".into(),
        format!("{base}/AVTransport"),
        format!("{base}/RenderingControl"),
        Some(format!("{base}/ConnectionManager")),
    )));
    let orch = tune_core::orchestrator::PlaybackOrchestrator::new(
        backend.clone(),
        Arc::new(tune_core::playback::PlaybackManager::new()),
        Arc::new(tune_core::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            tune_core::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(registre)),
        None,
    );
    for _ in 0..2 {
        let r = orch.resolve_queue_item_url(zone_id, 0).await.unwrap();
        assert_eq!(r.channels, Some(2), "le banc doit bien replier");
    }
    handle.abort();
    capture
}

/// 🔴 Les deux sources possibles du 2, l'une après l'autre (un seul test :
/// l'abonné est global).
#[tokio::test]
async fn chaque_reduction_dit_d_ou_vient_sa_cible() {
    le_cache_dit_son_origine_et_le_sink_est_au_journal().await;
    la_disposition_declaree_le_dit().await;
}

/// Le cas de l'A6 : la seconde lecture répond depuis le cache. Avant #6599,
/// elle ne laissait que la ligne de réduction, sans origine.
async fn le_cache_dit_son_origine_et_le_sink_est_au_journal() {
    let c = deux_lectures_journalisees(SINK_DENON, None).await;
    let sinks = c.lignes("dlna_sink_sonde");
    assert_eq!(
        sinks.len(),
        1,
        "le Sink du renderer doit être porté au journal, une fois : {sinks:?}"
    );
    assert!(
        sinks[0].contains("audio/L16;rate=44100;channels=2") && sinks[0].contains("audio/flac"),
        "la ligne doit porter les entrées du Sink : {}",
        sinks[0]
    );
    assert_eq!(c.lignes("dlna_canaux_annonces_sondes").len(), 1);
    let cache = c.lignes("dlna_canaux_annonces_en_cache");
    assert_eq!(
        cache.len(),
        1,
        "la seconde lecture répond depuis le cache et doit le dire : {cache:?}"
    );
    assert!(cache[0].contains("canaux_max=Some(2)"), "{}", cache[0]);
    let reductions = c.lignes("reduction_de_canaux_pour_le_renderer");
    assert_eq!(reductions.len(), 2, "{reductions:?}");
    for l in &reductions {
        assert!(
            l.contains("origine=\"renderer\"") && l.contains("canaux_renderer=Some(2)"),
            "la réduction doit nommer l'annonce du renderer comme origine : {l}"
        );
    }
}

/// L'autre source possible du 2 : la disposition déclarée pour la zone, sur
/// un renderer muet sur ses canaux.
async fn la_disposition_declaree_le_dit() {
    let c = deux_lectures_journalisees(SINK_MUET_SUR_LES_CANAUX, Some("stereo")).await;
    let reductions = c.lignes("reduction_de_canaux_pour_le_renderer");
    assert_eq!(reductions.len(), 2, "{reductions:?}");
    for l in &reductions {
        assert!(
            l.contains("origine=\"disposition_declaree\"")
                && l.contains("canaux_renderer=None")
                && l.contains("canaux_declares=Some(2)"),
            "la réduction doit nommer la disposition de la zone : {l}"
        );
    }
}
