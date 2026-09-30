//! #5498 — après un Seek en streaming sur une sortie réseau, la position
//! retenue par Tune restait figée pendant les 10 s de grâce du sondeur.
//!
//! FabienM (Devialet, zone Salon, 0.9.168, fil 2037) : Seek de reprise vers
//! 33 718 ms à 15:41:21 ; 11,7 s de lecture ; la Pause de 15:41:32 enregistre
//! 33 718 ; la reprise suivante relit 33 718 et le Seek de rattrapage #5050
//! renvoie le lecteur de 41,7 s à 33,7 s.
//!
//! Le témoin monte le VRAI `tick` avec une sortie factice de type `dlna` et
//! une piste Qobuz servie par une session : c'est la grâce longue
//! (`SEEK_STREAMING_GRACE_SECS`) qui s'applique.
//!
//! Contre-épreuve : rendre à la publication sa seule condition
//! `!in_seek_grace` fait tomber
//! `apres_un_seek_la_position_suit_l_appareil_pendant_la_grace` et
//! `apres_une_avance_la_position_suit_l_appareil_pendant_la_grace` ; les deux
//! gardes de l'ancienne position restent vertes, ce qui montre que la grâce
//! tient toujours son rôle.

use super::*;

use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::OutputRegistry;
use crate::outputs::mock::MockOutput;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::ServiceRegistry;
use std::sync::Arc;
use tokio::sync::Mutex;

const APPAREIL: &str = "uuid:temoin-5498";
const DUREE_MS: u64 = 277_081;
/// La cible du Seek de reprise du journal.
const CIBLE_MS: u64 = 33_718;
/// Ce que le Devialet rapportait encore 170 ms après ce Seek.
const AVANT_LE_SEEK_MS: u64 = 46_200;

struct Banc {
    poller: PositionPoller,
    playback: Arc<PlaybackManager>,
    outputs: Arc<Mutex<OutputRegistry>>,
    zone_id: i64,
    poll_states: HashMap<i64, ZonePollState>,
    idle: HashMap<i64, IdlePollBackoff>,
}

impl Banc {
    async fn monter() -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("Salon", Some("dlna"), Some(APPAREIL))
            .unwrap();
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(
            MockOutput::new(APPAREIL, "Salon").with_type("dlna"),
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
        let poller = PositionPoller::new(
            orchestrator,
            playback.clone(),
            outputs.clone(),
            db.clone(),
            Arc::new(Mutex::new(HashMap::new())),
        );
        Self {
            poller,
            playback,
            outputs,
            zone_id,
            poll_states: HashMap::new(),
            idle: HashMap::new(),
        }
    }

    /// « People Rise Up », Qobuz, servie par une session : le déplacement est
    /// un « streaming seek » au sens du sondeur, grâce de 10 s.
    fn piste() -> NowPlaying {
        NowPlaying {
            track_id: Some(5498),
            title: "People Rise Up".into(),
            source: "qobuz".into(),
            source_id: Some("186376994".into()),
            stream_id: Some("7139518c-temoin-5498".into()),
            duration_ms: DUREE_MS as i64,
            ..Default::default()
        }
    }

    /// La piste joue depuis `position_ms` : un tour l'a constaté et publié.
    async fn jouer_jusqu_a(&mut self, position_ms: u64) {
        self.playback.play(self.zone_id, Self::piste()).await;
        let generation = self.playback.get_state(self.zone_id).await.track_generation;
        let ps = self
            .poll_states
            .entry(self.zone_id)
            .or_insert_with(|| ZonePollState::new(generation));
        ps.track_started_at = Some(Instant::now() - Duration::from_millis(position_ms + 2_000));
        self.appareil_a(position_ms).await;
        self.tic().await;
        assert_eq!(
            self.servi().await,
            position_ms as i64,
            "le banc lui-même est faux : la position devrait être publiée"
        );
    }

    async fn appareil_a(&self, position_ms: u64) {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.set_state(TransportState::Playing).await;
        mock.set_duration(DUREE_MS);
        mock.set_position(position_ms);
    }

    /// Un tour de sondage où l'appareil rapporte `position_ms`.
    async fn tour(&mut self, position_ms: u64) -> i64 {
        self.appareil_a(position_ms).await;
        self.tic().await;
        self.servi().await
    }

    async fn tic(&mut self) {
        self.poller
            .tick(&mut self.poll_states, &mut self.idle, &Instant::now())
            .await;
    }

    async fn servi(&self) -> i64 {
        self.playback.get_state(self.zone_id).await.position_ms
    }

    async fn en_grace(&self) -> bool {
        self.playback
            .get_state(self.zone_id)
            .await
            .last_seek_at
            .is_some_and(|t| t.elapsed() < Duration::from_secs(SEEK_STREAMING_GRACE_SECS))
    }
}

/// Le cas du journal : Seek (de reprise) vers 33 718 alors que l'appareil
/// est à 46 200. L'ancienne position reste écartée ; celles d'après se
/// publient au fil des tours, grâce en cours.
#[tokio::test]
async fn apres_un_seek_la_position_suit_l_appareil_pendant_la_grace() {
    let mut banc = Banc::monter().await;
    banc.jouer_jusqu_a(AVANT_LE_SEEK_MS - 1_000).await;
    banc.playback.seek(banc.zone_id, CIBLE_MS as i64).await;

    assert_eq!(
        banc.tour(AVANT_LE_SEEK_MS).await,
        CIBLE_MS as i64,
        "la position d'avant le Seek ne doit pas revenir"
    );
    for position in [34_000u64, 35_000, 35_600] {
        assert!(
            banc.en_grace().await,
            "le témoin doit tourner DANS la grâce"
        );
        assert_eq!(
            banc.tour(position).await,
            position as i64,
            "l'appareil joue à {position} ms après le Seek : la position retenue \
             reste figée à la cible pendant la grâce (#5498)"
        );
    }
}

/// Même chose pour une avance (74 053 ms dans le journal), depuis 33 s : la
/// position d'avant reste écartée, celle d'après se publie.
#[tokio::test]
async fn apres_une_avance_la_position_suit_l_appareil_pendant_la_grace() {
    let mut banc = Banc::monter().await;
    banc.jouer_jusqu_a(33_000).await;
    banc.playback.seek(banc.zone_id, 74_053).await;

    assert_eq!(
        banc.tour(34_000).await,
        74_053,
        "l'ancienne position revient"
    );
    assert!(banc.en_grace().await);
    // 74 000 (RelTime tronqué) est en deçà de la cible : c'est la garde de
    // monotonie d'`update_position` qui le retient, pas la grâce. On juge
    // donc sur les secondes suivantes.
    assert_eq!(
        banc.tour(75_000).await,
        75_000,
        "l'appareil joue à 75 s après l'avance : la position retenue reste \
         figée à la cible pendant la grâce (#5498)"
    );
    assert_eq!(banc.tour(76_000).await, 76_000);
}

/// La grâce tient toujours son rôle : pendant qu'elle court, un appareil qui
/// rapporte encore et encore l'ancienne position ne fait pas ressauter le
/// curseur (c'est la raison d'être de `SEEK_STREAMING_GRACE_SECS`).
#[tokio::test]
async fn pendant_la_grace_l_ancienne_position_reste_ecartee() {
    let mut banc = Banc::monter().await;
    banc.jouer_jusqu_a(20_000).await;
    banc.playback.seek(banc.zone_id, 120_000).await;
    for position in [20_000u64, 21_000, 22_000, 23_000, 24_000, 25_000] {
        assert_eq!(
            banc.tour(position).await,
            120_000,
            "{position} ms est la position d'AVANT le déplacement"
        );
    }
}

/// La règle seule, bornes comprises.
#[test]
fn la_fenetre_encadre_la_cible_et_le_temps_ecoule() {
    use decisions::{
        MARGE_AUTOUR_DE_LA_CIBLE_MS as M, echantillon_posterieur_au_deplacement as ok,
    };
    let c = CIBLE_MS;
    assert!(ok(c, 0, c));
    assert!(ok(c, 0, c - M), "borne basse");
    assert!(!ok(c, 0, c - M - 1));
    assert!(ok(c, 0, c + M), "borne haute sans temps écoulé");
    assert!(!ok(c, 0, c + M + 1));
    assert!(
        ok(c, 11_700, c + 11_700 + M),
        "la lecture avance la borne haute"
    );
    assert!(
        !ok(c, 170, AVANT_LE_SEEK_MS),
        "le 46 200 du journal reste écarté"
    );
    assert!(ok(500, 0, 0), "cible proche de 0 : pas de débordement");
}
