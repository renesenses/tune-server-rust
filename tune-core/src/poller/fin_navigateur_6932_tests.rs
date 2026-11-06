//! Témoins de #6932 : la zone navigateur dont la piste est finie, servie
//! jusqu'au bout, et dont le lecteur n'a pas demandé la suite.

use super::{MARGE_FIN_NAVIGATEUR, piste_navigateur_finie};
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::{AudioStreamer, StreamInfo};
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlayState, PlaybackManager, ZoneState};
use crate::poller::PositionPoller;
use crate::streaming::ServiceRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const DUREE_MS: i64 = 141_000;

fn il_y_a(d: Duration) -> Instant {
    Instant::now().checked_sub(d).expect("horloge trop jeune")
}

fn etat_qui_joue() -> ZoneState {
    ZoneState {
        zone_id: 14,
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "01 - Prelude No. 1 in C, BWV846".into(),
            stream_id: Some("s1".into()),
            source: "local".into(),
            duration_ms: DUREE_MS,
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// La scène du fil 2572 : aucune mesure du lecteur, la piste a commencé il y
/// a 4 min 14 s, elle en dure 2 min 21 s, le flux est servi en entier.
#[test]
fn piste_finie_sans_aucune_mesure_du_lecteur() {
    let mut e = etat_qui_joue();
    e.observation_rouverte_a = Some(il_y_a(Duration::from_secs(254)));
    assert!(piste_navigateur_finie(&e, true, MARGE_FIN_NAVIGATEUR));
}

/// Le lecteur a mesuré 140 s il y a 30 s : fini depuis bien plus que la marge.
#[test]
fn piste_finie_apres_les_mesures_du_lecteur() {
    let mut e = etat_qui_joue();
    e.observation_rouverte_a = Some(il_y_a(Duration::from_secs(170)));
    e.position_ms = 140_000;
    e.derniere_avance_de_position = Some(il_y_a(Duration::from_secs(30)));
    assert!(piste_navigateur_finie(&e, true, MARGE_FIN_NAVIGATEUR));
}

/// Le lecteur mesure encore (60 s il y a 5 s) : la piste joue, même si la
/// fenêtre a été ouverte il y a longtemps (une pause reprise, un téléphone
/// qui a mis du temps à démarrer).
#[test]
fn contre_epreuve_le_lecteur_qui_mesure_encore_nest_pas_touche() {
    let mut e = etat_qui_joue();
    e.observation_rouverte_a = Some(il_y_a(Duration::from_secs(600)));
    e.position_ms = 60_000;
    e.derniere_avance_de_position = Some(il_y_a(Duration::from_secs(5)));
    assert!(!piste_navigateur_finie(&e, true, MARGE_FIN_NAVIGATEUR));
}

/// Fin du morceau à l'instant : le lecteur garde la priorité pendant la marge.
#[test]
fn contre_epreuve_la_marge_laisse_la_main_au_lecteur() {
    let mut e = etat_qui_joue();
    e.observation_rouverte_a = Some(il_y_a(Duration::from_millis(DUREE_MS as u64 + 5_000)));
    assert!(!piste_navigateur_finie(&e, true, MARGE_FIN_NAVIGATEUR));
}

/// Flux pas servi en entier : le lecteur peut encore attendre des octets.
#[test]
fn contre_epreuve_flux_pas_servi_en_entier() {
    let mut e = etat_qui_joue();
    e.observation_rouverte_a = Some(il_y_a(Duration::from_secs(600)));
    assert!(!piste_navigateur_finie(&e, false, MARGE_FIN_NAVIGATEUR));
}

/// Pause, radio, durée inconnue, fenêtre non datée : on ne conclut rien.
#[test]
fn contre_epreuves_pause_radio_duree_inconnue_etat_restaure() {
    let mut e = etat_qui_joue();
    e.observation_rouverte_a = Some(il_y_a(Duration::from_secs(600)));
    let mut pause = e.clone();
    pause.state = PlayState::Paused;
    assert!(!piste_navigateur_finie(&pause, true, MARGE_FIN_NAVIGATEUR));
    let mut radio = e.clone();
    radio.now_playing.as_mut().unwrap().source = "radio".into();
    assert!(!piste_navigateur_finie(&radio, true, MARGE_FIN_NAVIGATEUR));
    let mut sans_duree = e.clone();
    sans_duree.now_playing.as_mut().unwrap().duration_ms = 0;
    assert!(!piste_navigateur_finie(
        &sans_duree,
        true,
        MARGE_FIN_NAVIGATEUR
    ));
    let mut restaure = e;
    restaure.observation_rouverte_a = None;
    assert!(!piste_navigateur_finie(
        &restaure,
        true,
        MARGE_FIN_NAVIGATEUR
    ));
}

struct Banc {
    poller: PositionPoller,
    playback: Arc<PlaybackManager>,
    orchestrator: Arc<PlaybackOrchestrator>,
    db: Arc<dyn crate::db::backend::DbBackend>,
    tmp: tempfile::TempDir,
}

impl Banc {
    async fn monter() -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let playback = Arc::new(PlaybackManager::new());
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        let orchestrator = Arc::new(PlaybackOrchestrator::new(
            db.clone(),
            playback.clone(),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            outputs.clone(),
            None,
        ));
        let poller = PositionPoller::new(
            orchestrator.clone(),
            playback.clone(),
            outputs,
            db.clone(),
            Arc::new(Mutex::new(HashMap::new())),
        );
        Self {
            poller,
            playback,
            orchestrator,
            db,
            tmp: tempfile::TempDir::new().unwrap(),
        }
    }

    /// Une zone `output_type` en lecture d'un fichier de 10 octets, dont
    /// `octets` ont été servis.
    async fn zone(&self, output_type: &str, octets: u64) -> i64 {
        let zone_id = ZoneRepo::with_backend(self.db.clone())
            .create("S20 Ultra", Some(output_type), None)
            .unwrap();
        let fichier = self.tmp.path().join(format!("{zone_id}.wav"));
        std::fs::write(&fichier, b"0123456789").unwrap();
        let sid = self
            .orchestrator
            .streamer
            .create_file_session(
                StreamInfo {
                    format: "wav".into(),
                    mime_type: "audio/wav".into(),
                    ..Default::default()
                },
                fichier.to_string_lossy().into_owned(),
                false,
            )
            .await;
        {
            let sessions = self.orchestrator.streamer.sessions_state();
            let sessions = sessions.lock().await;
            sessions
                .get(&sid)
                .unwrap()
                .bytes_sent
                .store(octets, std::sync::atomic::Ordering::Relaxed);
        }
        self.playback
            .play(
                zone_id,
                NowPlaying {
                    title: "01 - Prelude No. 1 in C, BWV846".into(),
                    stream_id: Some(sid),
                    source: "local".into(),
                    duration_ms: DUREE_MS,
                    ..Default::default()
                },
            )
            .await;
        zone_id
    }

    /// L'instantané du sondeur, la fenêtre ouverte il y a `age`.
    async fn instantane(&self, zone_id: i64, age: Duration) -> ZoneState {
        let mut zs = self.playback.get_state(zone_id).await;
        zs.observation_rouverte_a = Some(il_y_a(age));
        zs
    }
}

/// ⛔ LE DÉFAUT DU FIL 2572 : la piste est finie depuis deux minutes, servie
/// en entier, et la zone restait « en lecture » jusqu'à ce que l'utilisateur
/// appuie sur « suivant ». File épuisée ici : la fin de piste arrête la zone,
/// comme pour toute autre sortie.
#[tokio::test]
async fn la_zone_navigateur_finie_et_servie_enchaine() {
    let banc = Banc::monter().await;
    let zone_id = banc.zone("browser", 10).await;
    let zs = banc.instantane(zone_id, Duration::from_secs(254)).await;
    assert!(
        banc.poller.enchainer_la_zone_navigateur_finie(&zs).await,
        "piste finie depuis 113 s, flux servi en entier : personne n'a \
         demandé la suite et la zone navigateur reste en lecture (#6932)"
    );
    assert_ne!(
        banc.playback.get_state(zone_id).await.state,
        PlayState::Playing,
        "la fin de piste n'a pas été traitée"
    );
}

#[tokio::test]
async fn contre_epreuve_flux_servi_en_partie_rien_ne_bouge() {
    let banc = Banc::monter().await;
    let zone_id = banc.zone("browser", 4).await;
    let zs = banc.instantane(zone_id, Duration::from_secs(254)).await;
    assert!(!banc.poller.enchainer_la_zone_navigateur_finie(&zs).await);
    assert_eq!(
        banc.playback.get_state(zone_id).await.state,
        PlayState::Playing
    );
}

#[tokio::test]
async fn contre_epreuve_piste_en_cours_rien_ne_bouge() {
    let banc = Banc::monter().await;
    let zone_id = banc.zone("browser", 10).await;
    let zs = banc.instantane(zone_id, Duration::from_secs(60)).await;
    assert!(!banc.poller.enchainer_la_zone_navigateur_finie(&zs).await);
    assert_eq!(
        banc.playback.get_state(zone_id).await.state,
        PlayState::Playing
    );
}

/// Une zone sans périphérique qui n'est PAS navigateur relève d'autres
/// gardes (#2630) : ce repli ne la touche pas.
#[tokio::test]
async fn contre_epreuve_zone_orpheline_non_navigateur() {
    let banc = Banc::monter().await;
    let zone_id = banc.zone("local", 10).await;
    let zs = banc.instantane(zone_id, Duration::from_secs(254)).await;
    assert!(!banc.poller.enchainer_la_zone_navigateur_finie(&zs).await);
}

/// Garde de câblage : le repli vit dans la branche « sans périphérique » du
/// tick, avant l'abandon d'une lecture sans destination. Retiré de la boucle,
/// tout le reste resterait vert et plus personne ne l'appellerait.
#[test]
fn le_tick_appelle_le_repli_dans_la_branche_sans_peripherique() {
    const TICK: &str = include_str!("tick.rs");
    let appel = ".enchainer_la_zone_navigateur_finie(zone_state)";
    let branche = TICK
        .find("decisions::deviceless_radio_refresh_due(")
        .expect("branche sans périphérique introuvable");
    let abandon = TICK
        .find(".abandonner_lecture_sans_destination(zone_state)")
        .expect("abandon introuvable");
    let position = TICK.find(appel);
    assert!(
        position.is_some_and(|p| branche < p && p < abandon),
        "le tick n'enchaîne plus la zone navigateur finie (#6932)"
    );
}
