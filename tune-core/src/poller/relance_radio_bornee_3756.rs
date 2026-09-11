//! #3756 — « Radio dont l'adresse rend une page web : Tune relance sans fin »
//! (Belkadi Yacine, fil 1734, v0.9.143 Linux, zone DENAFRIPS).
//!
//! ## Le constat, chiffré
//!
//! Sur les 3 min 12 s de journal joint au rapport (12:02:54 → 12:06:06), la
//! station `https://jb-radio.net` — la page d'accueil du site, pas un flux :
//!
//! | marqueur | occurrences |
//! |---|---|
//! | `radio_local_decode_failed` (`radio_not_audio`) | 8 |
//! | `orchestrator_play … output_sent=true` | 10 |
//! | `radio_auto_retry_success` | 4 |
//! | `radio_renderer_stopped_giving_up` | **0** |
//!
//! Quatre « succès » pour huit décodages échoués, et **aucun abandon**.
//!
//! ## Pourquoi la boucle n'avait pas de fond
//!
//! Le décodage d'une radio tourne dans une tâche DÉTACHÉE
//! (`orchestrator::resolve_direct`) : `play()` rend `Ok` dès que l'ordre est
//! accepté, et l'échec arrive une seconde plus tard. Le sondeur ne voyait que
//! ce `Ok` — il le journalisait `radio_auto_retry_success` et remettait
//! `radio_stopped_ticks` à zéro, si bien que la borne des six ticks
//! (`radio_renderer_stopped_giving_up`) n'était jamais atteinte.
//!
//! Le remède a deux temps, et le second est celui qui porte :
//!
//! 1. l'`Ok` de `play()` ne remet plus le compteur à zéro — seule la preuve
//!    qu'un renderer JOUE le fait, au tick suivant ;
//! 2. le décodeur, qui sait que `radio_not_audio` et `radio_hls_unsupported`
//!    ne guériront pas, le NOTE (`PlaybackOrchestrator::radios_refusees`) ; le
//!    sondeur consulte cette mémoire AVANT de relancer et renonce sans
//!    réémettre une lecture.
//!
//! ## Le banc
//!
//! Rien n'est simulé du côté de Tune : la base est celle de production
//! (SQLite en mémoire, migrations réelles), l'orchestrateur et le sondeur sont
//! ceux de production, et `tick()` est la vraie boucle. Seule la station est
//! locale : un serveur lié sur `127.0.0.1:0` qui répond **200 `text/html`**,
//! c'est-à-dire exactement ce que `curl -sI https://jb-radio.net` rend.
//!
//! L'étranglement radio (`RADIO_POLL_INTERVAL_SECS` = 15 s) est reculé à la
//! main entre deux ticks. C'est le seul artifice, et c'est celui que les
//! autres bancs du sondeur emploient déjà : le temps réel est versé de
//! l'extérieur pour ne pas dormir.

use super::{IdlePollBackoff, OutputStatus, PositionPoller, TransportState, ZonePollState};
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::event_bus::EventBus;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::registry::OutputRegistry;
use crate::outputs::traits::OutputTarget;
use crate::playback::{NowPlaying, PlayState, PlaybackManager};
use crate::streaming::ServiceRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const DEVICE_ID: &str = "local:denafrips";

/// La sortie du constat : elle dit « arrêté » et sa position ne bouge pas.
///
/// C'est la conjonction EXACTE que `tick` lit comme « la radio s'est tue »
/// (`radio_stopped`) : un renderer qui dit `Stopped` tout en avançant est,
/// lui, considéré comme jouant (Yamaha R-N2000A sur MP3 Icecast).
struct SortieMuette {
    position_ms: u64,
    /// Combien de fois la sortie a reçu un ordre de lecture. C'est le
    /// compteur qui dit si la station a été RELANCÉE.
    lectures: Arc<AtomicU64>,
}

#[async_trait::async_trait]
impl OutputTarget for SortieMuette {
    fn name(&self) -> &str {
        "DENAFRIPS USB Audio V3.14"
    }
    fn device_id(&self) -> &str {
        DEVICE_ID
    }
    fn output_type(&self) -> &str {
        "local"
    }
    async fn play_media(
        &self,
        _media: &crate::outputs::traits::PlayMedia<'_>,
    ) -> Result<(), String> {
        self.lectures.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    async fn pause(&self) -> Result<(), String> {
        Ok(())
    }
    async fn resume(&self) -> Result<(), String> {
        Ok(())
    }
    async fn stop(&self) -> Result<(), String> {
        Ok(())
    }
    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Ok(())
    }
    async fn set_volume(&self, _volume: f64) -> Result<(), String> {
        Ok(())
    }
    async fn set_mute(&self, _muted: bool) -> Result<(), String> {
        Ok(())
    }
    async fn get_status(&self) -> Result<OutputStatus, String> {
        Ok(OutputStatus {
            state: TransportState::Stopped,
            position_ms: self.position_ms,
            duration_ms: 0,
            ..Default::default()
        })
    }
    async fn is_available(&self) -> bool {
        true
    }
}

/// Une station qui répond **200 `text/html`** : la page d'accueil d'un site,
/// pas un flux. C'est le cas du ticket, mesuré le 09/09/2026.
async fn station_qui_sert_une_page_web() -> String {
    use axum::Router;
    use axum::http::header::CONTENT_TYPE;

    let app = Router::new().fallback(|| async {
        (
            [(CONTENT_TYPE, "text/html; charset=utf-8")],
            "<!doctype html><html><body>JB Radio</body></html>",
        )
    });
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let adresse = ecoute.local_addr().expect("adresse locale");
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    format!("http://{adresse}/")
}

struct Banc {
    poller: PositionPoller,
    playback: Arc<PlaybackManager>,
    orchestrateur: Arc<PlaybackOrchestrator>,
    zone_id: i64,
    url: String,
    lectures: Arc<AtomicU64>,
    etats: HashMap<i64, ZonePollState>,
    reculs: HashMap<i64, IdlePollBackoff>,
}

impl Banc {
    async fn monter(url: String) -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("Salon", Some("local"), Some(DEVICE_ID))
            .unwrap();

        let lectures = Arc::new(AtomicU64::new(0));
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(SortieMuette {
            position_ms: 0,
            lectures: lectures.clone(),
        }));

        let playback = Arc::new(PlaybackManager::new());
        let orchestrateur = Arc::new(PlaybackOrchestrator::new(
            db.clone(),
            playback.clone(),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            outputs.clone(),
            None,
        ));
        let poller = PositionPoller::new(
            orchestrateur.clone(),
            playback.clone(),
            outputs,
            db.clone(),
            Arc::new(Mutex::new(HashMap::new())),
        )
        .with_event_bus(Arc::new(EventBus::new()));

        playback
            .play(
                zone_id,
                NowPlaying {
                    title: "jb-radio".into(),
                    source: "radio".into(),
                    source_id: Some(url.clone()),
                    duration_ms: 0,
                    ..Default::default()
                },
            )
            .await;

        Self {
            poller,
            playback,
            orchestrateur,
            zone_id,
            url,
            lectures,
            etats: HashMap::new(),
            reculs: HashMap::new(),
        }
    }

    /// Un tick du sondeur, l'étranglement radio étant échu.
    async fn un_tick(&mut self) {
        if let Some(ps) = self.etats.get_mut(&self.zone_id) {
            ps.last_radio_poll = Instant::now() - Duration::from_secs(60);
        }
        self.poller
            .tick(&mut self.etats, &mut self.reculs, &Instant::now())
            .await;
        // La tâche de décodage est détachée : on lui laisse le temps de
        // rendre son verdict, comme elle le fait en production (158 ms dans
        // le journal du ticket, 12:03:11.496 → 12:03:11.654).
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    async fn joue_encore(&self) -> bool {
        self.playback.get_state(self.zone_id).await.state == PlayState::Playing
    }
}

/// LE FAIT DE BASE — une station qui rend une page web finit par être
/// ABANDONNÉE. Avant #3756 la boucle n'avait aucun fond : quatre « succès »
/// pour huit échecs et zéro `radio_renderer_stopped_giving_up` en 3 min 12 s.
#[tokio::test(flavor = "multi_thread")]
async fn une_station_qui_rend_une_page_web_finit_par_etre_abandonnee() {
    let url = station_qui_sert_une_page_web().await;
    let mut banc = Banc::monter(url).await;

    // Douze ticks : le double de la borne des six. Si la boucle a encore un
    // trou, elle le montre ici.
    let mut abandonnee_au_tick = None;
    for n in 1..=12 {
        banc.un_tick().await;
        if !banc.joue_encore().await {
            abandonnee_au_tick = Some(n);
            break;
        }
    }

    let n = abandonnee_au_tick.unwrap_or_else(|| {
        panic!(
            "douze ticks et la zone joue toujours : la relance est SANS FIN. \
             C'est le défaut du fil 1734 — `play()` rend Ok dès que l'ordre \
             est accepté, le décodage échoue une seconde plus tard dans une \
             tâche détachée, et personne ne compte les échecs."
        )
    });
    assert!(
        n <= 8,
        "la zone doit être abandonnée dans la fenêtre des six ticks, pas au \
         bout de {n}"
    );
}

/// LE VERDICT EST RETENU — le décodeur a dit `radio_not_audio`, et la mémoire
/// de l'orchestrateur le porte. C'est cette mémoire que le sondeur consulte
/// avant de relancer ; sans elle il n'a que le `Ok` de `play()`, qui ne dit
/// rien du flux.
#[tokio::test(flavor = "multi_thread")]
async fn l_echec_definitif_du_decodeur_est_retenu_pour_la_zone() {
    let url = station_qui_sert_une_page_web().await;
    let mut banc = Banc::monter(url).await;

    for _ in 1..=4 {
        banc.un_tick().await;
        let station = banc.url.clone();
        if banc
            .orchestrateur
            .radio_deja_refusee(banc.zone_id, &station)
        {
            return;
        }
    }
    panic!(
        "le décodeur a rendu `radio_not_audio` sur cette station et personne \
         ne l'a retenu : le sondeur n'a plus que le `Ok` de `play()`, qui ne \
         prouve rien du flux, et la relance repart pour un tour."
    );
}

/// LA RELANCE EST BORNÉE EN NOMBRE — une station refusée n'est pas relancée
/// indéfiniment. Le compteur d'ordres de lecture reçus par la sortie est le
/// témoin : en production le journal en comptait dix en 3 min 12 s, sans
/// jamais s'arrêter.
#[tokio::test(flavor = "multi_thread")]
async fn une_station_refusee_n_est_pas_relancee_a_l_infini() {
    let url = station_qui_sert_une_page_web().await;
    let mut banc = Banc::monter(url).await;

    for _ in 1..=12 {
        banc.un_tick().await;
        if !banc.joue_encore().await {
            break;
        }
    }

    let relances = banc.lectures.load(Ordering::Relaxed);
    assert!(
        relances <= 2,
        "une station dont le décodeur a dit qu'elle ne guérirait pas ne doit \
         pas être relancée {relances} fois : le verdict est définitif, la \
         relance doit s'arrêter au premier."
    );
}

/// TÉMOIN VERT — la borne ne doit pas manger la reprise LÉGITIME. Une radio
/// qui joue (position qui avance, même si le renderer dit `Stopped` — le cas
/// Yamaha R-N2000A) ne doit jamais être abandonnée, et rien ne doit entrer
/// dans la mémoire des refus.
///
/// Sans ce témoin, « n'abandonne jamais » et « abandonne toujours » passent
/// tous les deux les trois gardes ci-dessus.
#[tokio::test(flavor = "multi_thread")]
async fn une_radio_qui_avance_n_est_ni_abandonnee_ni_refusee() {
    /// La même sortie, mais dont la position AVANCE à chaque relevé.
    struct SortieQuiAvance {
        position_ms: AtomicU64,
    }
    #[async_trait::async_trait]
    impl OutputTarget for SortieQuiAvance {
        fn name(&self) -> &str {
            "DENAFRIPS USB Audio V3.14"
        }
        fn device_id(&self) -> &str {
            DEVICE_ID
        }
        fn output_type(&self) -> &str {
            "local"
        }
        async fn pause(&self) -> Result<(), String> {
            Ok(())
        }
        async fn resume(&self) -> Result<(), String> {
            Ok(())
        }
        async fn stop(&self) -> Result<(), String> {
            Ok(())
        }
        async fn seek(&self, _position_ms: u64) -> Result<(), String> {
            Ok(())
        }
        async fn set_volume(&self, _volume: f64) -> Result<(), String> {
            Ok(())
        }
        async fn set_mute(&self, _muted: bool) -> Result<(), String> {
            Ok(())
        }
        async fn get_status(&self) -> Result<OutputStatus, String> {
            Ok(OutputStatus {
                state: TransportState::Stopped,
                position_ms: self.position_ms.fetch_add(1_000, Ordering::Relaxed) + 1_000,
                duration_ms: 0,
                ..Default::default()
            })
        }
        async fn is_available(&self) -> bool {
            true
        }
    }

    let url = station_qui_sert_une_page_web().await;
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("Salon", Some("local"), Some(DEVICE_ID))
        .unwrap();
    let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
    outputs.lock().await.register(Box::new(SortieQuiAvance {
        position_ms: AtomicU64::new(0),
    }));
    let playback = Arc::new(PlaybackManager::new());
    let orchestrateur = Arc::new(PlaybackOrchestrator::new(
        db.clone(),
        playback.clone(),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        outputs.clone(),
        None,
    ));
    let poller = PositionPoller::new(
        orchestrateur.clone(),
        playback.clone(),
        outputs,
        db.clone(),
        Arc::new(Mutex::new(HashMap::new())),
    )
    .with_event_bus(Arc::new(EventBus::new()));
    playback
        .play(
            zone_id,
            NowPlaying {
                title: "FIP".into(),
                source: "radio".into(),
                source_id: Some(url.clone()),
                duration_ms: 0,
                ..Default::default()
            },
        )
        .await;

    let mut etats: HashMap<i64, ZonePollState> = HashMap::new();
    let mut reculs: HashMap<i64, IdlePollBackoff> = HashMap::new();
    for _ in 1..=12 {
        if let Some(ps) = etats.get_mut(&zone_id) {
            ps.last_radio_poll = Instant::now() - Duration::from_secs(60);
        }
        poller.tick(&mut etats, &mut reculs, &Instant::now()).await;
    }

    assert_eq!(
        playback.get_state(zone_id).await.state,
        PlayState::Playing,
        "une radio dont la position AVANCE joue, même si le renderer annonce \
         « arrêté » : l'abandonner couperait TSF Jazz et Radio Classique \
         toutes les 45 s, exactement le défaut que la garde d'origine \
         corrigeait"
    );
    assert!(
        !orchestrateur.radio_deja_refusee(zone_id, &url),
        "aucun décodage n'a été lancé sur cette zone : rien ne doit entrer \
         dans la mémoire des refus"
    );
}
