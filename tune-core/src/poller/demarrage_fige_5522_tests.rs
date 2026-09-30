//! #5522 — le banc : le VRAI sondeur (`tick`), une vraie file, un vrai
//! orchestrateur, et une sortie factice qui dit « en lecture » en restant à 0.
//!
//! Les 8 s ne sont pas attendues : l'horloge du démarrage figé est un champ
//! de l'état de sondage, on la date dans le passé. C'est l'injection, pas un
//! `sleep` déguisé.
//!
//! Témoins :
//!  1. sortie réseau figée : UNE relance de la même piste, puis, la relance
//!     restant figée, l'arrêt avec le bandeau — et plus rien ensuite ;
//!  2. la même chose sur la sortie locale ;
//!  3. un démarrage lent mais réel (0 pendant 7 s, puis la position part) ;
//!  4. une pause : l'horloge repart de zéro à la reprise ;
//!  5. un renderer qui n'a jamais rapporté de position n'est jamais jugé.
use super::*;
use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::PlayQueueRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::event_bus::EventBus;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::OutputRegistry;
use crate::outputs::mock::MockOutput;
use crate::outputs::traits::TransportState;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::ServiceRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const TITRE: &str = "Poem of Chinese Drum";
const DUREE_MS: i64 = 605_000;

/// Un WAV minuscule mais réel : la relance passe par `resolve_*`, qui ouvre
/// le fichier.
fn ecrire_wav(chemin: &std::path::Path) {
    use std::io::Write;
    let octets_data: u32 = 44_100 * 4 / 5;
    let mut v: Vec<u8> = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + octets_data).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&44_100u32.to_le_bytes());
    v.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&octets_data.to_le_bytes());
    v.extend(std::iter::repeat_n(0u8, octets_data as usize));
    let mut f = std::fs::File::create(chemin).unwrap();
    f.write_all(&v).unwrap();
    f.flush().unwrap();
}

struct Banc {
    poller: PositionPoller,
    playback: Arc<PlaybackManager>,
    outputs: Arc<Mutex<OutputRegistry>>,
    appareil: String,
    zone_id: i64,
    piste: i64,
    _fichier: tempfile::NamedTempFile,
    poll_states: HashMap<i64, ZonePollState>,
    idle: HashMap<i64, IdlePollBackoff>,
    recu: tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
}

impl Banc {
    /// Une zone en lecture de `TITRE`, file `[TITRE]`, sortie factice
    /// `type_sortie` sur `appareil`.
    async fn monter(type_sortie: &str, appareil: &str) -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("Salon", Some(type_sortie), Some(appareil))
            .unwrap();

        let fichier = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
        ecrire_wav(fichier.path());
        let mut t = crate::db::models::Track::new(TITRE.to_string());
        t.file_path = Some(fichier.path().to_str().unwrap().to_string());
        t.format = Some("wav".into());
        t.sample_rate = Some(44_100);
        t.bit_depth = Some(16);
        t.channels = 2;
        t.track_number = 1;
        t.duration_ms = DUREE_MS;
        let piste = TrackRepo::with_backend(db.clone()).create(&t).unwrap();
        PlayQueueRepo::with_backend(db.clone())
            .set_queue(zone_id, &[piste])
            .unwrap();

        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(
            MockOutput::new(appareil, "NEO Stream").with_type(type_sortie),
        ));
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

        let mut banc = Self {
            poller,
            playback,
            outputs,
            appareil: appareil.to_string(),
            zone_id,
            piste,
            _fichier: fichier,
            poll_states: HashMap::new(),
            idle: HashMap::new(),
            recu,
        };
        banc.lancer().await;
        banc
    }

    /// `play()` de la piste, comme un « Lire » : nouvelle génération.
    async fn lancer(&mut self) {
        self.playback
            .play(
                self.zone_id,
                NowPlaying {
                    track_id: Some(self.piste),
                    title: TITRE.into(),
                    source: "local".into(),
                    duration_ms: DUREE_MS,
                    ..Default::default()
                },
            )
            .await;
        self.playback.update_queue_info(self.zone_id, 0, 1).await;
    }

    async fn titres_joues(&self) -> Vec<String> {
        let reg = self.outputs.lock().await;
        let arc = reg.get(&self.appareil).unwrap();
        let sortie = arc.lock().await;
        sortie
            .as_any()
            .downcast_ref::<MockOutput>()
            .unwrap()
            .play_titles()
            .await
    }

    /// Ce que la sortie rapporte au prochain sondage.
    async fn sortie(&self, etat: TransportState, position_ms: u64) {
        let reg = self.outputs.lock().await;
        let arc = reg.get(&self.appareil).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.set_state(etat).await;
        mock.set_duration(DUREE_MS as u64);
        mock.set_position(position_ms);
    }

    async fn lectures(&self) -> usize {
        let reg = self.outputs.lock().await;
        let arc = reg.get(&self.appareil).unwrap();
        let sortie = arc.lock().await;
        sortie
            .as_any()
            .downcast_ref::<MockOutput>()
            .unwrap()
            .play_call_count()
            .await
    }

    async fn tic(&mut self) {
        self.poller
            .tick(&mut self.poll_states, &mut self.idle, &Instant::now())
            .await;
    }

    /// Dater l'horloge du démarrage figé de `secs` dans le passé.
    fn figee_depuis(&mut self, secs: u64) {
        let ps = self
            .poll_states
            .get_mut(&self.zone_id)
            .expect("la zone doit être sondée");
        assert!(
            ps.fige_a_zero_depuis.is_some(),
            "prémisse : le sondeur doit avoir vu la piste à 0 à ce tour"
        );
        ps.fige_a_zero_depuis = Some(Instant::now() - Duration::from_secs(secs));
    }

    async fn etat(&self) -> PlayState {
        self.playback.get_state(self.zone_id).await.state
    }

    /// Le dernier `zone.playback_error` de cette zone : message et `fatal`.
    fn bandeau(&mut self) -> Option<(String, bool)> {
        let mut trouve = None;
        while let Ok(ev) = self.recu.try_recv() {
            if ev.event_type == "zone.playback_error"
                && ev.data.get("zone_id").and_then(|v| v.as_i64()) == Some(self.zone_id)
            {
                trouve = Some((
                    ev.data
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    ev.data.get("fatal").and_then(|v| v.as_bool()) == Some(true),
                ));
            }
        }
        trouve
    }

    /// Un renderer réseau prouve qu'il rapporte sa position : un tour à 5 s,
    /// puis la piste est relancée par l'utilisateur (nouvelle génération).
    async fn prouver_la_position(&mut self) {
        self.sortie(TransportState::Playing, 5_000).await;
        self.tic().await;
        self.lancer().await;
    }
}

/// Le scénario complet : figée 8 s ⇒ relance ; la relance figée 8 s ⇒ arrêt
/// avec bandeau ; et plus aucune relance ensuite.
async fn relance_puis_bandeau(banc: &mut Banc) {
    banc.sortie(TransportState::Playing, 0).await;
    banc.tic().await;
    assert_eq!(
        banc.lectures().await,
        0,
        "prémisse : rien n'a encore été rejoué"
    );
    banc.figee_depuis(9);
    banc.tic().await;

    assert_eq!(
        banc.lectures().await,
        1,
        "une piste « en lecture » restée à 0 pendant 8 s doit être relancée une fois"
    );
    assert_eq!(
        banc.titres_joues().await,
        vec![TITRE.to_string()],
        "la relance doit renvoyer la MÊME piste à la sortie"
    );
    assert_eq!(
        banc.playback.get_state(banc.zone_id).await.queue_position,
        0,
        "la relance rejoue la MÊME ligne de file"
    );
    assert!(banc.bandeau().is_none(), "la relance réussie ne crie pas");
    assert_eq!(banc.etat().await, PlayState::Playing);

    // La relance reste figée à 0 (la sortie factice repart à 0 sur Play).
    banc.tic().await;
    banc.figee_depuis(9);
    banc.tic().await;

    let (message, fatal) = banc
        .bandeau()
        .expect("la relance restée figée doit faire parler l'écran");
    assert!(
        message.contains("n'a pas démarré") && message.contains(TITRE),
        "le bandeau doit dire que le morceau n'a pas démarré, et lequel : {message}"
    );
    assert!(
        fatal,
        "sans `fatal`, la fenêtre de grâce du client avale le message"
    );
    assert_ne!(
        banc.etat().await,
        PlayState::Playing,
        "la zone doit être arrêtée après l'échec de la relance"
    );
    assert_eq!(
        banc.lectures().await,
        1,
        "jamais de seconde relance : pas de boucle"
    );

    // Et plus rien ensuite, même si la sortie dit encore « en lecture ».
    banc.sortie(TransportState::Playing, 0).await;
    for _ in 0..3 {
        banc.tic().await;
    }
    assert_eq!(
        banc.lectures().await,
        1,
        "une zone arrêtée n'est plus relancée"
    );
}

#[tokio::test]
async fn sortie_reseau_figee_a_zero_une_relance_puis_le_bandeau() {
    let mut banc = Banc::monter("dlna", "uuid:neo-stream-5522").await;
    banc.prouver_la_position().await;
    relance_puis_bandeau(&mut banc).await;
}

#[tokio::test]
async fn sortie_locale_figee_a_zero_une_relance_puis_le_bandeau() {
    let mut banc = Banc::monter("local", "local:haut-parleurs-5522").await;
    relance_puis_bandeau(&mut banc).await;
}

/// Démarrage lent MAIS réel : 7 s à 0, puis la position part. Rien.
#[tokio::test]
async fn un_demarrage_lent_mais_reel_ne_declenche_rien() {
    let mut banc = Banc::monter("local", "local:haut-parleurs-5522").await;
    banc.sortie(TransportState::Playing, 0).await;
    banc.tic().await;
    banc.figee_depuis(7);
    banc.tic().await;
    banc.sortie(TransportState::Playing, 1_200).await;
    banc.tic().await;
    assert!(
        banc.poll_states[&banc.zone_id].fige_a_zero_depuis.is_none(),
        "une position qui démarre efface l'horloge"
    );
    banc.sortie(TransportState::Playing, 2_200).await;
    banc.tic().await;

    assert_eq!(
        banc.lectures().await,
        0,
        "aucune relance d'une piste qui a démarré"
    );
    assert!(banc.bandeau().is_none());
    assert_eq!(banc.etat().await, PlayState::Playing);
}

/// Une pause, même longue, ne compte pas : à la reprise l'horloge repart de 0.
#[tokio::test]
async fn une_pause_ne_declenche_rien_et_remet_l_horloge_a_zero() {
    let mut banc = Banc::monter("local", "local:haut-parleurs-5522").await;
    banc.sortie(TransportState::Playing, 0).await;
    banc.tic().await;
    banc.figee_depuis(5);

    // Pause faite depuis le renderer (télécommande) : Tune joue encore,
    // la sortie dit `Paused`.
    banc.sortie(TransportState::Paused, 0).await;
    banc.tic().await;
    assert!(
        banc.poll_states[&banc.zone_id].fige_a_zero_depuis.is_none(),
        "une sortie en pause efface l'horloge du démarrage figé"
    );
    banc.sortie(TransportState::Playing, 0).await;
    banc.tic().await;
    banc.figee_depuis(5);

    // Pause faite depuis Tune.
    banc.playback.pause(banc.zone_id).await;
    banc.sortie(TransportState::Paused, 0).await;
    banc.tic().await;
    assert!(
        banc.poll_states
            .get(&banc.zone_id)
            .is_none_or(|ps| ps.fige_a_zero_depuis.is_none()),
        "la pause efface l'horloge du démarrage figé"
    );

    banc.playback.resume(banc.zone_id).await;
    banc.sortie(TransportState::Playing, 0).await;
    banc.tic().await;
    banc.tic().await;

    assert_eq!(
        banc.lectures().await,
        0,
        "5 s avant la pause + la pause ne font pas 8 s de démarrage figé"
    );
    assert!(banc.bandeau().is_none());
    assert_eq!(banc.etat().await, PlayState::Playing);
}

/// Un renderer qui n'a JAMAIS rapporté de position (certains rendent 0 en
/// permanence en jouant) n'est pas jugé : rien, même après 30 s.
#[tokio::test]
async fn un_renderer_sans_position_prouvee_n_est_jamais_juge() {
    let mut banc = Banc::monter("dlna", "uuid:renderer-muet-5522").await;
    banc.sortie(TransportState::Playing, 0).await;
    banc.tic().await;
    let ps = banc.poll_states.get_mut(&banc.zone_id).unwrap();
    assert!(
        ps.fige_a_zero_depuis.is_none(),
        "aucune horloge sans position prouvée"
    );
    ps.fige_a_zero_depuis = Some(Instant::now() - Duration::from_secs(30));
    banc.tic().await;

    assert_eq!(banc.lectures().await, 0);
    assert!(banc.bandeau().is_none());
    assert_eq!(banc.etat().await, PlayState::Playing);
}
