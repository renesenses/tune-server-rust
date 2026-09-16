//! #3727 — un Tune Endpoint déjà tenu par un autre serveur ACCEPTE la
//! connexion TCP puis se tait. Mesuré le 09/09/2026 sur le .42 : 36 délais
//! expirés, 4 relances du superviseur en trois minutes, et `GET /zones` qui
//! annonçait `online = true | state = playing` sans qu'un octet ne sorte.
//!
//! Le banc reconstitue le cas grandeur nature : un écouteur TCP local qui
//! `accept` puis dort, une vraie [`OaatOutput`] enregistrée sur une vraie
//! zone, et le sondeur qui tourne tick après tick. Ce qu'il éprouve :
//!
//! 1. la boucle de connexion RENONCE sur trois verdicts « muet » et remet un
//!    motif qui nomme la cause (`oaat_endpoint_muet_probablement_deja_tenu`) ;
//! 2. ce motif atteint l'écran (`zone.playback_error`, `fatal: true`) et la
//!    zone NE RESTE PAS `Playing` — le fantôme de #3581, en version réseau.
//!
//! Le contre-témoin garde l'autre sens : un port que PERSONNE n'écoute reste
//! « injoignable », renonce lui aussi en le disant, et n'accuse aucun voisin.

use super::{IdlePollBackoff, PositionPoller, ZonePollState};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::event_bus::EventBus;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::oaat::OaatOutput;
use crate::outputs::registry::OutputRegistry;
use crate::outputs::traits::PlayMedia;
use crate::playback::{NowPlaying, PlayState, PlaybackManager};
use crate::streaming::ServiceRegistry;

const DEVICE_ID: &str = "oaat:endpoint-tenu";

/// L'endpoint tenu par le .18, reconstitué : il accepte, garde la connexion
/// ouverte, et n'écrit jamais un octet.
async fn endpoint_qui_accepte_et_se_tait() -> u16 {
    let ecouteur = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecouteur.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut gardees = Vec::new();
        while let Ok((flux, _)) = ecouteur.accept().await {
            gardees.push(flux);
        }
    });
    port
}

/// Un port que personne n'écoute : lié puis relâché, il refuse (RST).
async fn port_sans_personne() -> u16 {
    let ecouteur = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecouteur.local_addr().unwrap().port();
    drop(ecouteur);
    port
}

struct Banc {
    poller: PositionPoller,
    playback: Arc<PlaybackManager>,
    outputs: Arc<Mutex<OutputRegistry>>,
    zone_id: i64,
    recu: tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
}

impl Banc {
    /// Une zone OAAT en lecture, dont la sortie vise `port` sur l'hôte local.
    async fn monter(port: u16) -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("Salon", Some("oaat"), Some(DEVICE_ID))
            .unwrap();

        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(OaatOutput::new(
            "Tune Endpoint".into(),
            "127.0.0.1".into(),
            port,
            DEVICE_ID.into(),
        )));

        let playback = Arc::new(PlaybackManager::new());
        let orchestrator = Arc::new(PlaybackOrchestrator::new(
            db.clone(),
            playback.clone(),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            outputs.clone(),
            None,
        ));
        let bus = Arc::new(EventBus::new());
        let recu = bus.subscribe();
        let poller = PositionPoller::new(
            orchestrator,
            playback.clone(),
            outputs.clone(),
            db.clone(),
            Arc::new(Mutex::new(HashMap::new())),
        )
        .with_event_bus(bus);

        // Ce que l'orchestrateur fait d'un `play` : la zone passe `Playing`
        // AVANT que la sortie ait dit quoi que ce soit — c'est exactement ce
        // que `GET /zones` montrait sur le .42.
        playback
            .play(
                zone_id,
                NowPlaying {
                    title: "Never Make It on Time".into(),
                    source: "local".into(),
                    duration_ms: 240_000,
                    ..Default::default()
                },
            )
            .await;
        let sortie = outputs.lock().await.get(DEVICE_ID).unwrap();
        sortie
            .lock()
            .await
            .play_media(&PlayMedia {
                url: "http://127.0.0.1:1/never.wav",
                mime_type: "audio/wav",
                title: Some("Never Make It on Time"),
                duration_ms: Some(240_000),
                ..Default::default()
            })
            .await
            .expect("play_media rend la main avant la connexion");

        Self {
            poller,
            playback,
            outputs,
            zone_id,
            recu,
        }
    }

    async fn un_tick(&self) {
        let mut poll_states: HashMap<i64, ZonePollState> = HashMap::new();
        let mut idle_backoff: HashMap<i64, IdlePollBackoff> = HashMap::new();
        self.poller
            .tick(&mut poll_states, &mut idle_backoff, &Instant::now())
            .await;
    }

    /// Ce que l'écran reçoit pour cette zone : le message, et son `fatal`.
    fn erreur(&mut self) -> Option<(String, bool)> {
        let mut trouvee = None;
        while let Ok(ev) = self.recu.try_recv() {
            if ev.event_type == "zone.playback_error"
                && ev.data.get("zone_id").and_then(|v| v.as_i64()) == Some(self.zone_id)
            {
                trouvee = Some((
                    ev.data
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    ev.data.get("fatal").and_then(|v| v.as_bool()) == Some(true),
                ));
            }
        }
        trouvee
    }

    /// Sonde la zone une fois par seconde, comme le sondeur de production,
    /// jusqu'à ce que l'écran reçoive un motif — ou que `budget` s'épuise.
    async fn sonder_jusqu_au_motif(&mut self, budget: Duration) -> Option<(String, bool)> {
        let debut = Instant::now();
        while debut.elapsed() < budget {
            self.un_tick().await;
            if let Some(erreur) = self.erreur() {
                return Some(erreur);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        None
    }

    async fn instantane(&self) -> serde_json::Value {
        let sortie = self.outputs.lock().await.get(DEVICE_ID).unwrap();
        let sortie = sortie.lock().await;
        sortie
            .as_any()
            .downcast_ref::<OaatOutput>()
            .unwrap()
            .diagnostics_snapshot()
    }
}

/// LE CAS DU .42 : trois verdicts « muet » (≈ 3 × 3 s de poignée, plus les
/// pauses) et la boucle renonce, le motif nomme l'endpoint tenu, l'écran le
/// reçoit, et la zone n'est plus `Playing`. Le budget de 40 s est très en
/// deçà des quinze tentatives (≈ 85 s) : une boucle qui n'aurait pas renoncé
/// laisse ce témoin rouge.
#[tokio::test]
async fn un_endpoint_qui_accepte_et_se_tait_est_nomme_et_la_zone_ne_reste_pas_playing() {
    let port = endpoint_qui_accepte_et_se_tait().await;
    let mut banc = Banc::monter(port).await;
    assert_eq!(
        banc.playback.get_state(banc.zone_id).await.state,
        PlayState::Playing,
        "avant tout verdict, la zone est `Playing` — c'est le point de départ du fantôme"
    );

    let resultat = banc.sonder_jusqu_au_motif(Duration::from_secs(40)).await;
    let instantane = banc.instantane().await;
    let (message, fatal) = resultat.unwrap_or_else(|| {
        panic!(
            "après 40 s la zone est toujours sans motif : la boucle de connexion n'a pas \
             renoncé sur un endpoint muet, et l'écran ne sait rien (instantané : {instantane})"
        )
    });

    assert!(
        message.contains("oaat_endpoint_muet_probablement_deja_tenu")
            && message.contains("autre serveur")
            && message.contains("Tune Endpoint"),
        "le motif doit nommer l'endpoint tenu, pas « connect timed out » : {message}"
    );
    assert!(
        fatal,
        "sans `fatal`, la fenêtre de grâce du client avale le message"
    );
    assert_ne!(
        banc.playback.get_state(banc.zone_id).await.state,
        PlayState::Playing,
        "une zone dont pas un octet ne sort ne doit pas rester « en lecture »"
    );

    assert_eq!(
        instantane["derniere_cause_de_connexion"], "oaat_endpoint_muet_probablement_deja_tenu",
        "le superviseur de stall lit cette cause pour ne pas relancer en boucle"
    );
    assert_eq!(
        instantane["playing"], false,
        "la sortie elle-même ne doit plus se dire en lecture"
    );
}

/// CONTRE-TÉMOIN : un port que personne n'écoute. La boucle fait ses quinze
/// tentatives (≈ 40 s de pauses cumulées), renonce en nommant « injoignable »,
/// et n'accuse aucun serveur voisin — sans quoi tout appareil éteint serait
/// déclaré « tenu par un autre serveur Tune ».
#[tokio::test]
async fn un_port_sans_personne_renonce_en_disant_injoignable_sans_accuser_un_voisin() {
    let port = port_sans_personne().await;
    let mut banc = Banc::monter(port).await;

    let (message, fatal) = banc
        .sonder_jusqu_au_motif(Duration::from_secs(75))
        .await
        .expect("après quinze refus, la boucle doit renoncer et le dire");

    assert!(
        message.contains("oaat_endpoint_injoignable"),
        "un port fermé est « injoignable » : {message}"
    );
    assert!(
        !message.contains("autre serveur"),
        "un appareil éteint ne doit pas être accusé d'être tenu par un voisin : {message}"
    );
    assert!(fatal);
    assert_ne!(
        banc.playback.get_state(banc.zone_id).await.state,
        PlayState::Playing
    );
    assert_ne!(
        banc.instantane().await["derniere_cause_de_connexion"],
        "oaat_endpoint_muet_probablement_deja_tenu"
    );
}
