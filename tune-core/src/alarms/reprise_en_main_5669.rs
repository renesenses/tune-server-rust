//! #5669 — le réveil programmé rend la main à l'utilisateur.
//!
//! Cyrille Moutia (fil 2111, 1.0.0-rc1, Yamaha R-N2000A) : un réveil d'essai
//! oublié coupe son album Qobuz à 10:42 pour lancer France Inter, puis son
//! fondu de volume continue jusqu'à 10:43:03 — après la pause du réveil
//! (10:42:25) et la relance de l'album (10:42:47) — en écrasant ses gestes de
//! volume.
//!
//! Deux propriétés gardées ici :
//! 1. le fondu s'arrête dès que l'utilisateur agit sur la zone (volume,
//!    pause ou arrêt, autre lecture) ;
//! 2. un réveil programmé qui sonne sur une zone qui JOUE déjà est ignoré
//!    (décision de Bertrand du 05/10) : la musique continue, le réveil n'est
//!    pas noté comme ayant sonné.

use super::*;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::OutputRegistry;
use crate::outputs::mock::MockOutput;
use crate::playback::{NowPlaying, PlayState, PlaybackManager};
use crate::streaming::ServiceRegistry;
use std::time::Duration;

const APPAREIL: &str = "mock-yamaha";
const CIBLE: f64 = 0.5;
/// 3 s de fondu = 6 pas de 0,5 s : 0 ; 0,083 ; 0,167 ; 0,25 ; …
const FONDU_S: u64 = 3;

struct Banc {
    db: Arc<dyn DbBackend>,
    orchestrator: Arc<PlaybackOrchestrator>,
    playback: Arc<PlaybackManager>,
    zone_id: i64,
}

impl Banc {
    async fn monter() -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn DbBackend> = Arc::new(db);
        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("Pièce par défaut", Some("dlna"), Some(APPAREIL))
            .unwrap();
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs
            .lock()
            .await
            .register(Box::new(MockOutput::new(APPAREIL, "R-N2000A")));
        let playback = Arc::new(PlaybackManager::new());
        let orchestrator = Arc::new(PlaybackOrchestrator::new(
            db.clone(),
            playback.clone(),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            outputs,
            None,
        ));
        Self {
            db,
            orchestrator,
            playback,
            zone_id,
        }
    }

    fn album_qobuz() -> NowPlaying {
        NowPlaying {
            title: "L’histoire du loup dans la bergerie".into(),
            source: "qobuz".into(),
            source_id: Some("qobuz-123".into()),
            ..Default::default()
        }
    }

    fn radio_du_reveil() -> NowPlaying {
        NowPlaying {
            title: "franceinter-hifi".into(),
            source: "radio".into(),
            source_id: Some("http://127.0.0.1:9/franceinter-hifi.aac".into()),
            ..Default::default()
        }
    }

    /// La source du réveil vient d'être lancée : le fondu part de là.
    async fn lancer_le_fondu(&self) -> tokio::task::JoinHandle<()> {
        self.playback
            .play(self.zone_id, Self::radio_du_reveil())
            .await;
        let generation = self.playback.get_state(self.zone_id).await.track_generation;
        let orch = self.orchestrator.clone();
        let zone = self.zone_id;
        tokio::spawn(async move {
            fade_in_volume(
                &orch,
                zone,
                Some(APPAREIL.into()),
                CIBLE,
                FONDU_S,
                generation,
            )
            .await;
        })
    }

    async fn volume(&self) -> f64 {
        self.playback.get_state(self.zone_id).await.volume
    }

    fn reveil(&self, id: i64) -> serde_json::Value {
        self.db
            .execute(
                "INSERT INTO alarms (id, name, time, zone_id, source_type, source_id, volume, fade_duration_s, enabled, one_shot, days_of_week) \
                 VALUES (?, 'Réveil', '10:42', ?, 'radio', 'http://127.0.0.1:9/franceinter-hifi.aac', 0.5, 0, '1', 0, '1111111')",
                &[&id as &dyn ToSqlValue, &self.zone_id],
            )
            .unwrap();
        AlarmScheduler::with_backend(self.db.clone(), self.orchestrator.clone())
            .get_alarm(id)
            .unwrap()
            .expect("le réveil inséré se relit")
    }

    fn a_sonne(&self, id: i64) -> bool {
        self.db
            .query_one(
                "SELECT last_fired_at FROM alarms WHERE id = ?",
                &[&id as &dyn ToSqlValue],
            )
            .unwrap()
            .and_then(|r| r.first().and_then(|v| v.as_str().map(str::to_string)))
            .is_some()
    }
}

// ─── 1. Le fondu rend la main ──────────────────────────────────

#[tokio::test(start_paused = true)]
async fn le_fondu_va_au_bout_quand_personne_n_y_touche() {
    let banc = Banc::monter().await;
    banc.lancer_le_fondu().await.await.unwrap();
    assert!(
        (banc.volume().await - CIBLE).abs() < 1e-9,
        "sans geste de l'utilisateur, le fondu doit atteindre {CIBLE} (lu : {})",
        banc.volume().await
    );
}

#[tokio::test(start_paused = true)]
async fn le_fondu_s_arrete_quand_l_utilisateur_regle_le_volume() {
    let banc = Banc::monter().await;
    let fondu = banc.lancer_le_fondu().await;
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    banc.orchestrator
        .set_volume(banc.zone_id, 0.61, Some(APPAREIL))
        .await
        .unwrap();
    fondu.await.unwrap();
    assert!(
        (banc.volume().await - 0.61).abs() < 1e-9,
        "#5669 : le fondu du réveil a écrasé le volume réglé par l'utilisateur \
         (0,61 attendu, {} lu)",
        banc.volume().await
    );
}

#[tokio::test(start_paused = true)]
async fn le_fondu_s_arrete_a_la_pause() {
    let banc = Banc::monter().await;
    let fondu = banc.lancer_le_fondu().await;
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    banc.playback.pause(banc.zone_id).await;
    let a_la_pause = banc.volume().await;
    fondu.await.unwrap();
    assert!(
        (banc.volume().await - a_la_pause).abs() < 1e-9,
        "#5669 : le fondu du réveil a continué après la pause ({a_la_pause} → {})",
        banc.volume().await
    );
}

#[tokio::test(start_paused = true)]
async fn le_fondu_s_arrete_quand_une_autre_lecture_commence() {
    let banc = Banc::monter().await;
    let fondu = banc.lancer_le_fondu().await;
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    banc.playback.play(banc.zone_id, Banc::album_qobuz()).await;
    let a_la_relance = banc.volume().await;
    fondu.await.unwrap();
    assert!(
        (banc.volume().await - a_la_relance).abs() < 1e-9,
        "#5669 : le fondu du réveil a continué sur l'album relancé \
         ({a_la_relance} → {})",
        banc.volume().await
    );
}

#[test]
fn reprise_en_main_lit_les_trois_gestes() {
    let mut etat = crate::playback::ZoneState {
        state: PlayState::Playing,
        track_generation: 4,
        volume: 0.25,
        ..Default::default()
    };
    assert_eq!(reprise_en_main(&etat, 4, Some(0.25)), None);
    assert_eq!(reprise_en_main(&etat, 4, None), None);
    assert_eq!(reprise_en_main(&etat, 4, Some(0.2)), Some("volume"));
    assert_eq!(reprise_en_main(&etat, 3, Some(0.25)), Some("autre_lecture"));
    etat.state = PlayState::Paused;
    assert_eq!(
        reprise_en_main(&etat, 4, Some(0.25)),
        Some("pause_ou_arret")
    );
    etat.state = PlayState::Stopped;
    assert_eq!(
        reprise_en_main(&etat, 4, Some(0.25)),
        Some("pause_ou_arret")
    );
}

// ─── 2. Un réveil programmé ne coupe pas une lecture en cours ──

#[tokio::test]
async fn un_reveil_programme_ne_coupe_pas_la_musique_en_cours() {
    let banc = Banc::monter().await;
    banc.playback.play(banc.zone_id, Banc::album_qobuz()).await;
    let avant = banc.playback.get_state(banc.zone_id).await;
    let reveil = banc.reveil(3);

    let planificateur = AlarmScheduler::with_backend(banc.db.clone(), banc.orchestrator.clone());
    tokio::time::timeout(
        Duration::from_secs(20),
        planificateur.sonner(&reveil, false),
    )
    .await
    .expect("le déclenchement doit rendre la main");

    let apres = banc.playback.get_state(banc.zone_id).await;
    assert_eq!(
        apres.track_generation, avant.track_generation,
        "#5669 : le réveil programmé a relancé une lecture sur une zone qui jouait"
    );
    assert_eq!(
        apres.state,
        PlayState::Playing,
        "#5669 : le réveil programmé a interrompu la musique en cours"
    );
    assert_eq!(
        apres.now_playing.as_ref().map(|np| np.source.as_str()),
        Some("qobuz"),
        "#5669 : la zone ne joue plus l'album de l'utilisateur"
    );
    assert!(
        !banc.a_sonne(3),
        "un réveil sauté ne doit pas être noté comme ayant sonné (last_fired_at)"
    );
}

#[tokio::test]
async fn un_reveil_programme_sonne_sur_une_zone_en_pause() {
    let banc = Banc::monter().await;
    banc.playback.play(banc.zone_id, Banc::album_qobuz()).await;
    banc.playback.pause(banc.zone_id).await;
    let reveil = banc.reveil(4);

    let planificateur = AlarmScheduler::with_backend(banc.db.clone(), banc.orchestrator.clone());
    tokio::time::timeout(
        Duration::from_secs(20),
        planificateur.sonner(&reveil, false),
    )
    .await
    .expect("le déclenchement doit rendre la main");

    assert!(
        banc.a_sonne(4),
        "une zone en pause ne joue pas : le réveil doit y sonner"
    );
}
