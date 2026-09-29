//! #5476 — une commande de l'utilisateur arrivée après une reprise rend caduc
//! le `Seek` de reprise détaché : il ne part plus.
//!
//! FabienM (Devialet Phantom, zone Salon, 0.9.168, fil 2037) : le `Play` de
//! reprise a pris 8,3 s ; l'utilisateur a avancé à 74 053 ms pendant ce
//! temps ; 70 ms après la fin de son Seek, la tâche détachée a mesuré
//! 46 200 ms, a pris l'avance pour un décalage et a renvoyé l'appareil à la
//! position de la pause, 33 718 ms. La garde ne comparait que `play_seq`, qu'un
//! Seek ne fait pas bouger.
//!
//! Le renderer factice est LENT : chaque action SOAP lui prend
//! [`DELAI_RENDERER`] (2 s), comme un Devialet un bon jour. Il est servi par
//! un vrai `DlnaOutput` enregistré dans l'orchestrateur : c'est le chemin de
//! production de `resume`, `seek` et `pause`, tâche détachée comprise, qui
//! décide.
//!
//! Contre-épreuve : rendre à `detacher_le_seek_apres_reprise` la garde sur
//! `current_play_seq` fait tomber les essais d'avance et de pause ; retirer la
//! seule seconde lecture (sortie en main) fait tomber
//! `une_avance_pendant_la_mesure_rend_le_seek_caduc`.
use super::PlaybackOrchestrator;
use super::session::RESUME_OUTPUT_SEEK_SETTLE_MS;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::{AudioStreamer, StreamInfo};
use crate::outputs::OutputTarget;
use crate::outputs::dlna::{DlnaOutput, parse_upnp_time};
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::registry::ServiceRegistry;
use axum::{Router, http::StatusCode, routing::post};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::Mutex;

/// La position de la pause du journal de FabienM.
const POSITION_PAUSE_MS: u64 = 33_718;
/// L'avance de l'utilisateur, même journal.
const AVANCE_MS: u64 = 74_053;
/// Temps de réponse du renderer factice, pour chaque action.
const DELAI_RENDERER: Duration = Duration::from_secs(2);

/// Ce que fait l'appareil au `Play` d'après Pause.
#[derive(Clone, Copy, PartialEq)]
enum AuPlay {
    /// Il reprend là où il était (Devialet, Beosound).
    EnPlace,
    /// Il repart du début (Cyrus Stream X) : le Seek de reprise est pour lui.
    DeZero,
}

struct Appareil {
    au_play: AuPlay,
    position_ms: u64,
    en_pause: bool,
    /// Actions SOAP, dans l'ordre de réception.
    actions: Vec<String>,
    /// Cibles des `Seek` reçus, dans l'ordre de réception.
    seeks: Vec<u64>,
}

fn rel_time(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// `RelTime` et `Target` se disent à la seconde : ce que l'appareil reçoit
/// d'une position en millisecondes.
fn a_la_seconde(ms: u64) -> u64 {
    ms / 1000 * 1000
}

fn cible(corps: &str) -> Option<u64> {
    let debut = corps.find("<Target>")? + "<Target>".len();
    let fin = corps[debut..].find("</Target>")? + debut;
    Some(parse_upnp_time(&corps[debut..fin]))
}

struct Banc {
    orch: Arc<PlaybackOrchestrator>,
    zone_id: i64,
    did: String,
    appareil: Arc<StdMutex<Appareil>>,
    _serveur: tokio::task::JoinHandle<()>,
}

impl Drop for Banc {
    fn drop(&mut self) {
        self._serveur.abort();
    }
}

impl Banc {
    fn seeks(&self) -> Vec<u64> {
        self.appareil.lock().unwrap().seeks.clone()
    }
    fn compte(&self, action: &str) -> usize {
        self.appareil
            .lock()
            .unwrap()
            .actions
            .iter()
            .filter(|a| a.as_str() == action)
            .count()
    }
    fn position_appareil(&self) -> u64 {
        self.appareil.lock().unwrap().position_ms
    }
    async fn position_tune(&self) -> i64 {
        self.orch.playback.get_state(self.zone_id).await.position_ms
    }
    fn journal(&self) -> String {
        let a = self.appareil.lock().unwrap();
        format!("actions {:?}, seeks {:?}", a.actions, a.seeks)
    }
    /// Attendre que le renderer ne reçoive plus rien pendant plus que le
    /// plus long silence possible de la tâche détachée (pose + une réponse).
    async fn attendre_le_silence(&self) {
        let calme = Duration::from_millis(RESUME_OUTPUT_SEEK_SETTLE_MS) + DELAI_RENDERER * 2;
        let mut vu = usize::MAX;
        loop {
            let n = self.appareil.lock().unwrap().actions.len();
            if n == vu {
                return;
            }
            vu = n;
            tokio::time::sleep(calme).await;
        }
    }
    fn reprendre(&self) -> tokio::task::JoinHandle<()> {
        let (orch, zone_id, did) = (self.orch.clone(), self.zone_id, self.did.clone());
        tokio::spawn(async move {
            orch.resume(zone_id, Some(&did))
                .await
                .expect("la reprise doit aboutir");
        })
    }
    fn avancer(&self, position_ms: u64) -> tokio::task::JoinHandle<()> {
        let (orch, zone_id, did) = (self.orch.clone(), self.zone_id, self.did.clone());
        tokio::spawn(async move {
            orch.seek(zone_id, position_ms, Some(&did))
                .await
                .expect("le déplacement doit aboutir");
        })
    }
}

/// Une zone DLNA en pause à [`POSITION_PAUSE_MS`] sur un renderer lent, sur
/// une session de flux mandataire (seekable : la reprise se fait sur place et
/// le Seek de l'utilisateur part en `Seek` SOAP direct, comme dans le
/// journal).
async fn banc(au_play: AuPlay) -> Banc {
    let appareil = Arc::new(StdMutex::new(Appareil {
        au_play,
        position_ms: POSITION_PAUSE_MS,
        en_pause: true,
        actions: Vec::new(),
        seeks: Vec::new(),
    }));
    let a = appareil.clone();
    let app = Router::new().route(
        "/control",
        post(move |headers: axum::http::HeaderMap, corps: String| {
            let a = a.clone();
            async move {
                let soap = headers
                    .get("SOAPAction")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                let action = soap
                    .rsplit('#')
                    .next()
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_owned();
                let reponse = {
                    let mut a = a.lock().unwrap();
                    a.actions.push(action.clone());
                    match action.as_str() {
                        "Play" => {
                            a.en_pause = false;
                            if a.au_play == AuPlay::DeZero {
                                a.position_ms = 0;
                            }
                            "<u:PlayResponse/>".to_string()
                        }
                        "Pause" => {
                            a.en_pause = true;
                            "<u:PauseResponse/>".to_string()
                        }
                        "Seek" => {
                            if let Some(ms) = cible(&corps) {
                                a.seeks.push(ms);
                                a.position_ms = ms;
                            }
                            "<u:SeekResponse/>".to_string()
                        }
                        "GetPositionInfo" => format!(
                            "<s:Envelope><s:Body><u:GetPositionInfoResponse><Track>1</Track>\
                             <TrackDuration>0:05:00</TrackDuration><RelTime>{}</RelTime>\
                             </u:GetPositionInfoResponse></s:Body></s:Envelope>",
                            rel_time(a.position_ms)
                        ),
                        "GetTransportInfo" => format!(
                            "<s:Envelope><s:Body><u:GetTransportInfoResponse>\
                             <CurrentTransportState>{}</CurrentTransportState>\
                             <CurrentTransportStatus>OK</CurrentTransportStatus>\
                             </u:GetTransportInfoResponse></s:Body></s:Envelope>",
                            if a.en_pause {
                                "PAUSED_PLAYBACK"
                            } else {
                                "PLAYING"
                            }
                        ),
                        _ => "<Response/>".to_string(),
                    }
                };
                tokio::time::sleep(DELAI_RENDERER).await;
                (StatusCode::OK, reponse)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    let serveur = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let output = DlnaOutput::new(
        "Salon test".into(),
        "uuid:5476".into(),
        host.clone(),
        format!("{host}/control"),
        format!("{host}/control"),
        None,
    );
    let did = output.device_id().to_string();

    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let orch = Arc::new(PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    ));
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon", Some("dlna"), Some(&did))
        .unwrap();
    orch.outputs.lock().await.register(Box::new(output));
    let sid = orch
        .streamer
        .create_proxy_session(
            StreamInfo {
                format: "flac".into(),
                mime_type: "audio/flac".into(),
                sample_rate: 44_100,
                bit_depth: 16,
                channels: 2,
                ..Default::default()
            },
            "http://127.0.0.1:9/people-rise-up.flac".into(),
            false,
        )
        .await;
    orch.playback
        .play(
            zone_id,
            NowPlaying {
                track_id: Some(5476),
                title: "People Rise Up".into(),
                source: "qobuz".into(),
                stream_id: Some(sid),
                duration_ms: 300_000,
                ..Default::default()
            },
        )
        .await;
    orch.playback
        .update_position(zone_id, POSITION_PAUSE_MS as i64)
        .await;
    orch.playback.pause(zone_id).await;
    Banc {
        orch,
        zone_id,
        did,
        appareil,
        _serveur: serveur,
    }
}

/// Le cas du journal : l'avance part PENDANT le `Play` de reprise, attend le
/// verrou de la sortie, et la tâche détachée démarre derrière elle.
#[tokio::test]
async fn une_avance_pendant_le_play_de_reprise_n_est_pas_ecrasee() {
    let b = banc(AuPlay::EnPlace).await;
    let reprise = b.reprendre();
    tokio::time::sleep(Duration::from_millis(300)).await;
    b.avancer(AVANCE_MS).await.unwrap();
    reprise.await.unwrap();
    b.attendre_le_silence().await;
    assert_eq!(
        b.seeks(),
        vec![a_la_seconde(AVANCE_MS)],
        "seul le Seek de l'utilisateur doit partir — {}",
        b.journal()
    );
    assert_eq!(
        b.position_appareil(),
        a_la_seconde(AVANCE_MS),
        "{}",
        b.journal()
    );
    assert_eq!(b.position_tune().await, AVANCE_MS as i64);
}

/// Pause, reprise, puis avance immédiate : la position finale est celle de
/// l'avance, chez l'appareil comme dans Tune.
#[tokio::test]
async fn une_avance_juste_apres_la_reprise_gagne() {
    let b = banc(AuPlay::EnPlace).await;
    b.reprendre().await.unwrap();
    b.avancer(AVANCE_MS).await.unwrap();
    b.attendre_le_silence().await;
    assert_eq!(b.seeks(), vec![a_la_seconde(AVANCE_MS)], "{}", b.journal());
    assert_eq!(
        b.position_appareil(),
        a_la_seconde(AVANCE_MS),
        "{}",
        b.journal()
    );
    assert_eq!(b.position_tune().await, AVANCE_MS as i64);
}

/// Deux avances de suite après la reprise : la dernière gagne.
#[tokio::test]
async fn deux_avances_de_suite_la_derniere_gagne() {
    let b = banc(AuPlay::EnPlace).await;
    b.reprendre().await.unwrap();
    let premiere = b.avancer(60_000);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let seconde = b.avancer(90_000);
    premiere.await.unwrap();
    seconde.await.unwrap();
    b.attendre_le_silence().await;
    assert_eq!(b.seeks(), vec![60_000, 90_000], "{}", b.journal());
    assert_eq!(b.position_appareil(), 90_000, "{}", b.journal());
    assert_eq!(b.position_tune().await, 90_000);
}

/// L'avance arrive APRÈS la pose, pendant que la tâche tient la sortie et
/// mesure la position d'un renderer reparti de zéro : la seconde lecture de
/// la génération, juste avant le Seek, le rend caduc.
#[tokio::test]
async fn une_avance_pendant_la_mesure_rend_le_seek_caduc() {
    let b = banc(AuPlay::DeZero).await;
    b.reprendre().await.unwrap();
    tokio::time::sleep(Duration::from_millis(RESUME_OUTPUT_SEEK_SETTLE_MS + 500)).await;
    assert_eq!(
        b.compte("GetPositionInfo"),
        1,
        "la tâche doit être en train de mesurer — {}",
        b.journal()
    );
    b.avancer(AVANCE_MS).await.unwrap();
    b.attendre_le_silence().await;
    assert_eq!(
        b.seeks(),
        vec![a_la_seconde(AVANCE_MS)],
        "le Seek de reprise, caduc, ne doit pas partir — {}",
        b.journal()
    );
    assert_eq!(b.position_appareil(), a_la_seconde(AVANCE_MS));
    assert_eq!(b.position_tune().await, AVANCE_MS as i64);
}

/// Une pause juste après la reprise n'est pas défaite : ni Seek de reprise,
/// ni `Play` de relance (la tâche relançait la lecture d'un renderer qu'elle
/// trouvait en pause après son Seek).
#[tokio::test]
async fn une_pause_juste_apres_la_reprise_n_est_pas_defaite() {
    let b = banc(AuPlay::DeZero).await;
    b.reprendre().await.unwrap();
    b.orch
        .pause(b.zone_id, Some(&b.did))
        .await
        .expect("la pause doit aboutir");
    b.attendre_le_silence().await;
    assert!(b.seeks().is_empty(), "aucun Seek — {}", b.journal());
    assert_eq!(b.compte("Play"), 1, "un seul Play — {}", b.journal());
    assert!(b.appareil.lock().unwrap().en_pause, "{}", b.journal());
}

/// La reprise simple, sans commande derrière, rattrape toujours un renderer
/// reparti de zéro (#2893, d01986a8) — même lent.
#[tokio::test]
async fn la_reprise_seule_rattrape_toujours_un_renderer_reparti_de_zero() {
    let b = banc(AuPlay::DeZero).await;
    b.reprendre().await.unwrap();
    b.attendre_le_silence().await;
    assert_eq!(
        b.seeks(),
        vec![a_la_seconde(POSITION_PAUSE_MS)],
        "{}",
        b.journal()
    );
    assert_eq!(b.position_appareil(), a_la_seconde(POSITION_PAUSE_MS));
    assert_eq!(b.position_tune().await, POSITION_PAUSE_MS as i64);
}

/// La reprise simple d'un renderer qui reprend en place : aucun Seek (#5050).
#[tokio::test]
async fn la_reprise_seule_en_place_n_envoie_aucun_seek() {
    let b = banc(AuPlay::EnPlace).await;
    b.reprendre().await.unwrap();
    b.attendre_le_silence().await;
    assert!(b.compte("GetPositionInfo") >= 1, "{}", b.journal());
    assert!(b.seeks().is_empty(), "{}", b.journal());
    assert_eq!(b.position_tune().await, POSITION_PAUSE_MS as i64);
}

/// Le compteur lui-même : une commande de transport le fait bouger sans
/// toucher `play_seq` ; une nouvelle lecture fait bouger les deux.
#[tokio::test]
async fn la_generation_de_transport_compte_les_commandes_sans_toucher_play_seq() {
    let p = PlaybackManager::new();
    assert_eq!(p.current_transport_seq(7).await, 0);
    let seq = p.marquer_commande_de_transport(7).await;
    assert_eq!(seq, 1);
    assert_eq!(p.current_transport_seq(7).await, 1);
    assert_eq!(p.current_play_seq(7).await, 0, "play_seq garde son sens");
    p.bump_generation(7).await;
    assert_eq!(p.current_play_seq(7).await, 1);
    assert_eq!(
        p.current_transport_seq(7).await,
        2,
        "une lecture compte aussi"
    );
    // Un déplacement confirmé (`PlaybackManager::seek`) ne compte pas : c'est
    // l'orchestrateur qui marque, à l'ENTRÉE de la commande.
    p.seek(7, 1_000).await;
    assert_eq!(p.current_transport_seq(7).await, 2);
}
