//! `Seek` REFUSÉ par un renderer DLNA (lot L3b, rc3).
//!
//! `DlnaOutput::seek` ne lisait pas la réponse : une faute SOAP 701
//! (« Transition not available »), 710 (« Seek mode not supported ») ou 711
//! (« Illegal seek target ») passait pour un acquittement. L'orchestrateur
//! publiait alors la nouvelle position, que l'appareil n'avait jamais prise.
//!
//! Le banc : un VRAI `DlnaOutput` branché sur un renderer factice (axum) qui
//! répond au `Seek` ce qu'on lui dit, et acquitte tout le reste.
//!
//! Contre-épreuve : rendre à `DlnaOutput::seek` son `.await?;` nu (sans lire
//! la réponse) fait tomber
//! `un_seek_refuse_701_710_711_ne_deplace_pas_la_position_et_le_dit` et
//! `un_saut_de_reprise_refuse_se_journalise_sans_boucle_ni_arret`.
use super::PlaybackOrchestrator;
use super::session::REPLAY_OUTPUT_SEEK_SETTLE_MS;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::event_bus::EventBus;
use crate::http::streamer::{AudioStreamer, StreamInfo};
use crate::outputs::dlna::{DlnaOutput, SEEK_REFUSE_PREFIX};
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlayState, PlaybackManager};
use crate::streaming::ServiceRegistry;
use axum::http::StatusCode;
use axum::{Router, routing::post};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;
use tune_output_api::{OutputCommand, OutputCommandError};

const APPAREIL: &str = "dlna:uuid-seek-refuse";
const DUREE_MS: i64 = 300_000;
const POSITION_AVANT_MS: i64 = 30_000;
const CIBLE_MS: u64 = 120_000;

fn faute(code: u32, description: &str) -> String {
    format!(
        "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
         <s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring>\
         <detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">\
         <errorCode>{code}</errorCode><errorDescription>{description}</errorDescription>\
         </UPnPError></detail></s:Fault></s:Body></s:Envelope>"
    )
}

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
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

struct Banc {
    orch: PlaybackOrchestrator,
    zone_id: i64,
    seeks_recus: Arc<AtomicU64>,
    serveur: tokio::task::JoinHandle<()>,
    _scratch: crate::test_scratch::ScratchDir,
}

impl Drop for Banc {
    fn drop(&mut self) {
        self.serveur.abort();
    }
}

/// Une zone DLNA en lecture à 0:30 d'une piste de 5:00 servie par une
/// session fichier (donc cherchable), dont le renderer répond au `Seek`
/// `(statut, corps)`.
async fn zone_dlna(statut: StatusCode, corps: String) -> Banc {
    let seeks_recus = Arc::new(AtomicU64::new(0));
    let compteur = seeks_recus.clone();
    let corps = Arc::new(corps);
    let app = Router::new().route(
        "/control",
        post(move |headers: axum::http::HeaderMap, _body: String| {
            let compteur = compteur.clone();
            let corps = corps.clone();
            async move {
                let action = headers
                    .get("SOAPAction")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                if action.contains("#Seek") {
                    compteur.fetch_add(1, Ordering::SeqCst);
                    (statut, (*corps).clone())
                } else {
                    (StatusCode::OK, "<u:Response/>".to_owned())
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hote = format!("http://{}", listener.local_addr().unwrap());
    let serveur = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let mut orch = PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );
    orch.event_bus = Some(Arc::new(EventBus::new()));
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon", Some("dlna"), Some(APPAREIL))
        .unwrap();
    orch.outputs.lock().await.register(Box::new(DlnaOutput::new(
        "Salon".into(),
        APPAREIL.into(),
        hote.clone(),
        format!("{hote}/control"),
        format!("{hote}/control"),
        None,
    )));
    let scratch = crate::test_scratch::scratch_dir("tune-seek-refuse");
    let fichier = scratch.join("piste.flac");
    std::fs::write(&fichier, b"fLaC").unwrap();
    let sid = orch
        .streamer
        .create_file_session(
            StreamInfo {
                format: "flac".into(),
                mime_type: "audio/flac".into(),
                file_size: Some(27_200_000),
                ..Default::default()
            },
            fichier.to_string_lossy().into_owned(),
            false,
        )
        .await;
    orch.playback
        .play(
            zone_id,
            NowPlaying {
                title: "Piste".into(),
                source: "local".into(),
                stream_id: Some(sid),
                duration_ms: DUREE_MS,
                ..Default::default()
            },
        )
        .await;
    orch.playback.seek(zone_id, POSITION_AVANT_MS).await;
    Banc {
        orch,
        zone_id,
        seeks_recus,
        serveur,
        _scratch: scratch,
    }
}

/// LE défaut : un refus UPnP passait pour un succès et la position publique
/// bougeait. Désormais l'erreur est typée (tête `SEEK_REFUSE_PREFIX`), la
/// position reste où l'appareil est, et l'interface reçoit un
/// `zone.playback_error` NON fatal.
#[tokio::test]
async fn un_seek_refuse_701_710_711_ne_deplace_pas_la_position_et_le_dit() {
    for (code, description) in [
        (701, "Transition not available"),
        (710, "Seek mode not supported"),
        (711, "Illegal seek target"),
    ] {
        let b = zone_dlna(StatusCode::INTERNAL_SERVER_ERROR, faute(code, description)).await;
        let mut rx = b.orch.event_bus.as_ref().unwrap().subscribe();

        let erreur = b
            .orch
            .seek(b.zone_id, CIBLE_MS, Some(APPAREIL))
            .await
            .expect_err(&format!("un refus {code} n'est pas un succès"));
        let OutputCommandError::Failed { command, message } = &erreur else {
            panic!("{code} : erreur inattendue {erreur:?}");
        };
        assert_eq!(*command, OutputCommand::Seek);
        assert!(
            message.starts_with(SEEK_REFUSE_PREFIX) && message.contains(&code.to_string()),
            "{code} : erreur non typée : {message}"
        );
        assert_eq!(b.seeks_recus.load(Ordering::SeqCst), 1, "{code}");
        let etat = b.orch.playback.get_state(b.zone_id).await;
        assert_eq!(
            etat.position_ms, POSITION_AVANT_MS,
            "{code} : la position publique ne doit pas suivre un Seek refusé"
        );
        assert_eq!(etat.state, PlayState::Playing, "{code} : la zone joue");

        b.orch.dire_deplacement_refuse(b.zone_id, &erreur);
        let ev = rx.try_recv().expect("zone.playback_error attendu");
        assert_eq!(ev.event_type, "zone.playback_error");
        assert_eq!(ev.data["zone_id"], b.zone_id);
        assert_eq!(ev.data["fatal"], false, "{code} : la zone joue toujours");
        let texte = ev.data["error"].as_str().unwrap();
        assert!(
            texte.contains("refusé le déplacement") && texte.contains(&code.to_string()),
            "{code} : message peu clair : {texte}"
        );
    }
}

/// Un `Seek` acquitté garde sa conduite : la position suit.
#[tokio::test]
async fn un_seek_accepte_deplace_toujours_la_position() {
    let b = zone_dlna(
        StatusCode::OK,
        "<u:SeekResponse xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\"/>".into(),
    )
    .await;
    b.orch
        .seek(b.zone_id, CIBLE_MS, Some(APPAREIL))
        .await
        .expect("un Seek acquitté réussit");
    assert_eq!(b.seeks_recus.load(Ordering::SeqCst), 1);
    assert_eq!(
        b.orch.playback.get_state(b.zone_id).await.position_ms,
        CIBLE_MS as i64
    );
}

/// Les autres échecs ne déclenchent pas le message de refus : ils gardent la
/// seule réponse HTTP, comme avant.
#[test]
fn seul_un_refus_du_renderer_produit_le_message() {
    for message in [
        "soap timeout: Seek",
        "output dlna:x disappeared during seek",
    ] {
        let erreur = OutputCommandError::failed(OutputCommand::Seek, message);
        assert_eq!(
            PlaybackOrchestrator::message_deplacement_refuse(&erreur),
            None,
            "{message}"
        );
    }
    assert_eq!(
        PlaybackOrchestrator::message_deplacement_refuse(&OutputCommandError::unsupported(
            OutputCommand::Seek
        )),
        None
    );
}

/// #5719 / #5666 — le saut automatique qui suit une reprise est refusé :
/// un journal, un seul `Seek` (aucune boucle), et la zone continue.
///
/// Fil courant (`current_thread`) : la tâche détachée tourne sur le même fil
/// que l'abonné de capture.
#[tokio::test]
async fn un_saut_de_reprise_refuse_se_journalise_sans_boucle_ni_arret() {
    crate::journal_de_test::fiabiliser_la_capture();
    let b = zone_dlna(
        StatusCode::INTERNAL_SERVER_ERROR,
        faute(701, "Transition not available"),
    )
    .await;
    let capture = Capture::default();
    let _abonne = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish(),
    );
    b.orch
        .sauter_apres_reprise_de_renderer_cale(b.zone_id, Some(APPAREIL), 194_000)
        .await
        .expect("le saut part en tâche détachée");
    tokio::time::sleep(Duration::from_millis(REPLAY_OUTPUT_SEEK_SETTLE_MS + 700)).await;
    let journal = capture.text();
    assert!(journal.contains("dlna_seek_refuse"), "{journal}");
    assert!(journal.contains("seek_apres_reprise_echoue"), "{journal}");
    assert!(
        !journal.contains("seek_apres_reprise_envoye"),
        "un refus n'est pas un Seek envoyé : {journal}"
    );
    tokio::time::sleep(Duration::from_millis(REPLAY_OUTPUT_SEEK_SETTLE_MS + 300)).await;
    assert_eq!(
        b.seeks_recus.load(Ordering::SeqCst),
        1,
        "aucune boucle de Seek"
    );
    assert_eq!(
        b.orch.playback.get_state(b.zone_id).await.state,
        PlayState::Playing,
        "la zone continue"
    );
}
