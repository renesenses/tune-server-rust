//! #6130 — quitter une radio sur une sortie DLNA : `Stop`, puis attendre que
//! le renderer ait lâché la connexion de la radio, et ensuite seulement
//! `SetAVTransportURI` + `Play` (`play_media`).
//!
//! Le terrain (.18, rc3, 12/10 05:49, Eversolo DMP-A8) : FIP jouait depuis
//! 5 944 s ; l'utilisateur lance un titre Qobuz. `stream_session_removed`
//! (radio) à 52.067, `dlna_set_uri_ok` à 52.527, `dlna_play` à 52.531 — et la
//! connexion radio n'est fermée par le renderer qu'à 53.830, 1,3 s APRÈS le
//! `Play`. Aucun `dlna_stop` avant le `SetAVTransportURI`. Puis trois
//! connexions `bytes=0-` et 118 s sans lecture.
//!
//! Le banc : une sortie `dlna` factice qui, comme le DMP-A8, tient la
//! connexion de la radio et ne la lâche qu'après avoir reçu `Stop` (ou
//! jamais). Le consommateur HTTP est le compteur `active_consumers` de la
//! session radio, celui que tient `RadioConsumerGuard` dans tune-stream-http.
//!
//! Contre-épreuve : titre → titre, aucun `Stop` de plus, aucune attente.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use super::{PlayRequest, PlaybackOrchestrator};
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::{AudioStreamer, StreamInfo, StreamSession};
use crate::outputs::registry::OutputRegistry;
use crate::outputs::{OutputCapabilities, OutputStatus, OutputTarget, PlayMedia};
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::ServiceRegistry;

const APPAREIL: &str = "dlna:uuid-dmp-a8-6130";
const PISTE_A: i64 = 1;
const PISTE_B: i64 = 2;
/// Le renderer lâche la radio 300 ms après le `Stop` (banc « poli »).
const DEPART_APRES_STOP_MS: u64 = 300;

#[derive(Debug, Clone)]
struct Evenement {
    quoi: &'static str,
    quand: Instant,
    /// Consommateurs HTTP encore accrochés à la radio à cet instant.
    consommateurs_radio: u32,
}

type Journal = Arc<std::sync::Mutex<Vec<Evenement>>>;

/// Le DMP-A8 de #6130 : il tient la connexion de la radio tant qu'on ne lui
/// a pas dit `Stop`, puis la lâche après `depart_apres_stop` (ou jamais).
struct Dmpa8Simule {
    journal: Journal,
    radio: Arc<std::sync::Mutex<Option<Arc<StreamSession>>>>,
    depart_apres_stop: Option<Duration>,
}

impl Dmpa8Simule {
    fn consommateurs(&self) -> u32 {
        self.radio
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.active_consumers.load(Ordering::SeqCst))
            .unwrap_or(0)
    }

    fn noter(&self, quoi: &'static str) {
        let consommateurs_radio = self.consommateurs();
        self.journal.lock().unwrap().push(Evenement {
            quoi,
            quand: Instant::now(),
            consommateurs_radio,
        });
    }
}

#[async_trait::async_trait]
impl OutputTarget for Dmpa8Simule {
    fn name(&self) -> &str {
        "DMP-A8"
    }
    fn device_id(&self) -> &str {
        APPAREIL
    }
    fn output_type(&self) -> &str {
        "dlna"
    }
    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities::v1(true, true, true, true, true, true)
    }
    async fn play_media(&self, _media: &PlayMedia<'_>) -> Result<(), String> {
        self.noter("play_media");
        Ok(())
    }
    async fn pause(&self) -> Result<(), String> {
        Ok(())
    }
    async fn resume(&self) -> Result<(), String> {
        Ok(())
    }
    async fn stop(&self) -> Result<(), String> {
        self.noter("stop");
        let radio = self.radio.lock().unwrap().clone();
        if let (Some(session), Some(delai)) = (radio, self.depart_apres_stop) {
            tokio::spawn(async move {
                tokio::time::sleep(delai).await;
                // Le renderer ferme sa connexion : `RadioConsumerGuard` tombe.
                let _ = session.active_consumers.fetch_update(
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                    |n| n.checked_sub(1),
                );
            });
        }
        Ok(())
    }
    async fn seek(&self, _: u64) -> Result<(), String> {
        Ok(())
    }
    async fn set_volume(&self, _: f64) -> Result<(), String> {
        Ok(())
    }
    async fn set_mute(&self, _: bool) -> Result<(), String> {
        Ok(())
    }
    async fn get_status(&self) -> Result<OutputStatus, String> {
        Ok(OutputStatus::default())
    }
    async fn is_available(&self) -> bool {
        true
    }
}

struct Banc {
    orch: PlaybackOrchestrator,
    zone_id: i64,
    journal: Journal,
    radio: Arc<std::sync::Mutex<Option<Arc<StreamSession>>>>,
    _emetteur_radio: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    _dossier: tempfile::TempDir,
}

async fn banc(depart_apres_stop: Option<Duration>) -> Banc {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let orch = PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );
    let dossier = tempfile::tempdir().unwrap();
    orch.db
        .execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'Hall & Oates');
             INSERT INTO albums (id, title, artist_id) VALUES (1, 'Private Eyes', 1);",
        )
        .unwrap();
    for (id, titre, nom) in [
        (PISTE_A, "I Can't Go For That", "a.flac"),
        (PISTE_B, "Private Eyes", "b.flac"),
    ] {
        let chemin = dossier.path().join(nom);
        std::fs::copy(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test.flac"),
            &chemin,
        )
        .unwrap();
        let chemin = chemin.to_string_lossy().into_owned();
        let p: [&dyn ToSqlValue; 3] = [&id, &titre, &chemin];
        orch.db
            .execute(
                "INSERT INTO tracks (id, title, album_id, artist_id, duration_ms, sample_rate, \
                 bit_depth, file_path, format, channels, source) \
                 VALUES (?, ?, 1, 1, 300000, 44100, 16, ?, 'flac', 2, 'local')",
                &p,
            )
            .unwrap();
    }
    let zone_repo = ZoneRepo::with_backend(orch.db.clone());
    let zone_id = zone_repo
        .create("Zone 10", Some("dlna"), Some(APPAREIL))
        .unwrap();
    zone_repo.update_dlna_native_flac(zone_id, true).unwrap();
    let journal: Journal = Arc::new(std::sync::Mutex::new(Vec::new()));
    let radio = Arc::new(std::sync::Mutex::new(None));
    orch.outputs.lock().await.register(Box::new(Dmpa8Simule {
        journal: journal.clone(),
        radio: radio.clone(),
        depart_apres_stop,
    }));
    Banc {
        orch,
        zone_id,
        journal,
        radio,
        _emetteur_radio: None,
        _dossier: dossier,
    }
}

/// FIP joue depuis longtemps sur la zone : session radio (WAV 48 kHz) dont le
/// renderer tient la connexion.
async fn fip_en_cours(b: &mut Banc) {
    let (sid, tx, _pret, session) = b
        .orch
        .streamer
        .create_radio_session(
            StreamInfo {
                format: "wav".into(),
                mime_type: "audio/wav".into(),
                sample_rate: 48_000,
                bit_depth: 16,
                channels: 2,
                ..Default::default()
            },
            64,
        )
        .await;
    // Le GET du renderer, servi par `RadioConsumerGuard`.
    session.active_consumers.fetch_add(1, Ordering::SeqCst);
    *b.radio.lock().unwrap() = Some(session);
    b._emetteur_radio = Some(tx);
    b.orch
        .playback
        .play(
            b.zone_id,
            NowPlaying {
                title: "FIP".into(),
                source: "radio".into(),
                stream_id: Some(sid),
                ..Default::default()
            },
        )
        .await;
}

fn piste(zone_id: i64, track_id: i64) -> PlayRequest {
    PlayRequest {
        zone_id,
        output_device_id: Some(APPAREIL.into()),
        track_id: Some(track_id),
        source: Some("local".into()),
        ..Default::default()
    }
}

fn journal(b: &Banc) -> Vec<Evenement> {
    b.journal.lock().unwrap().clone()
}

fn noms(evenements: &[Evenement]) -> Vec<&'static str> {
    evenements.iter().map(|e| e.quoi).collect()
}

/// LE défaut de #6130 : radio en cours, on lance un titre. `Stop` doit partir
/// AVANT le `SetAVTransportURI`, et celui-ci attendre que le renderer ait
/// lâché la connexion de la radio.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quitter_la_radio_envoie_stop_puis_attend_la_fermeture_avant_set_uri() {
    let mut b = banc(Some(Duration::from_millis(DEPART_APRES_STOP_MS))).await;
    fip_en_cours(&mut b).await;

    b.orch
        .play(piste(b.zone_id, PISTE_A))
        .await
        .expect("le titre doit partir");

    let j = journal(&b);
    assert_eq!(
        noms(&j),
        vec!["stop", "play_media"],
        "sortie de radio DLNA : Stop d'abord, puis SetAVTransportURI/Play"
    );
    assert_eq!(
        j[1].consommateurs_radio, 0,
        "le SetAVTransportURI doit attendre que le renderer ait lâché la radio"
    );
    let ecart = j[1].quand.duration_since(j[0].quand);
    assert!(
        ecart >= Duration::from_millis(DEPART_APRES_STOP_MS),
        "SetURI {ecart:?} après Stop : la fermeture de la radio n'a pas été attendue"
    );
    assert!(
        ecart < Duration::from_millis(1_500),
        "l'attente doit s'arrêter dès la fermeture, pas au bout du délai ({ecart:?})"
    );
}

/// Renderer qui ne lâche jamais la radio : l'attente est bornée à 2 s, puis
/// le titre part quand même.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_renderer_qui_ne_lache_pas_la_radio_n_attend_que_deux_secondes() {
    let mut b = banc(None).await;
    fip_en_cours(&mut b).await;

    b.orch
        .play(piste(b.zone_id, PISTE_A))
        .await
        .expect("le titre doit partir malgré tout");

    let j = journal(&b);
    assert_eq!(noms(&j), vec!["stop", "play_media"]);
    let ecart = j[1].quand.duration_since(j[0].quand);
    assert!(
        ecart >= Duration::from_millis(1_900),
        "le délai de 2 s doit être laissé au renderer ({ecart:?})"
    );
    assert!(
        ecart < Duration::from_millis(4_000),
        "l'attente doit être bornée ({ecart:?})"
    );
}

/// Contre-épreuve : titre → titre, rien ne change — aucun `Stop` envoyé par
/// l'orchestrateur, aucune attente.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn titre_puis_titre_reste_inchange() {
    let b = banc(Some(Duration::from_millis(DEPART_APRES_STOP_MS))).await;

    b.orch
        .play(piste(b.zone_id, PISTE_A))
        .await
        .expect("premier titre");
    let debut = Instant::now();
    b.orch
        .play(piste(b.zone_id, PISTE_B))
        .await
        .expect("second titre");
    let duree = debut.elapsed();

    assert_eq!(
        noms(&journal(&b)),
        vec!["play_media", "play_media"],
        "titre → titre : ni Stop ajouté ni attente"
    );
    assert!(
        duree < Duration::from_millis(1_500),
        "titre → titre ne doit pas attendre ({duree:?})"
    );
}

/// Un consommateur HTTP de la radio qui suit le protocole du vrai corps
/// (`tune-stream-http`, boucle radio) : il réclame le canal, sert le direct,
/// et ne s'arrête QUE si l'époque du canal change — le renderer, lui, ne
/// ferme jamais. `active_consumers` retombe quand la boucle sort, comme
/// `RadioConsumerGuard` à la chute du corps.
fn connexion_radio_tenue(session: Arc<StreamSession>) -> tokio::task::JoinHandle<()> {
    session.active_consumers.fetch_add(1, Ordering::SeqCst);
    tokio::spawn(async move {
        let mon_epoque = session.claim_channel_consumer();
        loop {
            let supplante = session.consumer_supersede.notified();
            tokio::pin!(supplante);
            supplante.as_mut().enable();
            if !session.is_current_channel_consumer(mon_epoque) {
                break;
            }
            tokio::select! {
                biased;
                _ = &mut supplante => continue,
                bloc = session.recv_chunk() => if bloc.is_none() { break },
            }
        }
        session.active_consumers.fetch_sub(1, Ordering::SeqCst);
    })
}

/// #6130, journal du .18 (22–25/10, rc7) : 12 sorties de radio sur 12 vers
/// le DMP-A8 finissent en `delai_depasse=true consommateurs_restants=1` — le
/// renderer ne lâche pas la radio sur `Stop`, il ne la ferme qu'environ 1 s
/// APRÈS le `Play` suivant (25/10 09:35:36.026 Play, 09:35:37.108
/// `radio_stream_client_disconnect`). C'est l'état des deux bruits blancs
/// (11/10, 12/10). Le serveur doit fermer lui-même le direct : au
/// `SetAVTransportURI`, plus aucune connexion radio, et sans attendre 2 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_renderer_qui_garde_la_radio_apres_stop_est_deconnecte_par_le_serveur() {
    // `None` : le renderer simulé ne ferme JAMAIS de lui-même.
    let mut b = banc(None).await;
    let (sid, tx, _pret, session) = b
        .orch
        .streamer
        .create_radio_session(
            StreamInfo {
                format: "wav".into(),
                mime_type: "audio/wav".into(),
                sample_rate: 48_000,
                bit_depth: 16,
                channels: 2,
                ..Default::default()
            },
            64,
        )
        .await;
    let connexion = connexion_radio_tenue(session.clone());
    // La connexion a réclamé le canal avant le geste de l'utilisateur.
    for _ in 0..100 {
        if session.consumer_epoch.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    *b.radio.lock().unwrap() = Some(session);
    b._emetteur_radio = Some(tx);
    b.orch
        .playback
        .play(
            b.zone_id,
            NowPlaying {
                title: "FIP".into(),
                source: "radio".into(),
                stream_id: Some(sid),
                ..Default::default()
            },
        )
        .await;

    b.orch
        .play(piste(b.zone_id, PISTE_A))
        .await
        .expect("le titre doit partir");

    let j = journal(&b);
    assert_eq!(noms(&j), vec!["stop", "play_media"]);
    assert_eq!(
        j[1].consommateurs_radio, 0,
        "connexion radio encore ouverte au SetAVTransportURI : le serveur n'a pas \
         fermé le direct d'un renderer qui ne lâche pas la radio sur Stop (#6130)"
    );
    let ecart = j[1].quand.duration_since(j[0].quand);
    assert!(
        ecart < Duration::from_millis(1_000),
        "la fermeture par le serveur est immédiate, pas le délai de 2 s ({ecart:?})"
    );
    tokio::time::timeout(Duration::from_secs(2), connexion)
        .await
        .expect("le corps radio doit être terminé")
        .unwrap();
}
