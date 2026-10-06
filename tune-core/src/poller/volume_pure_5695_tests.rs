//! #5695 — le banc : le VRAI sondeur (`tick`), une zone en lecture, une
//! sortie factice dont on change le volume « depuis la télécommande ».
//!
//! Fil 2119 : PURE forcé à 100 %, et `zones.volume` à 83. Seule l'adoption du
//! volume par le sondeur recopie en base ce que l'appareil dit de lui-même ;
//! elle ignorait le verrou PURE.
use super::*;
use crate::db::migrations::run_migrations;
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;
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
use tokio::sync::Mutex;

const APPAREIL: &str = "dlna-my-devialet";
const DUREE_MS: u64 = 600_000;

struct Banc {
    poller: PositionPoller,
    db: Arc<dyn crate::db::backend::DbBackend>,
    outputs: Arc<Mutex<OutputRegistry>>,
    zone_id: i64,
    poll_states: HashMap<i64, ZonePollState>,
    idle: HashMap<i64, IdlePollBackoff>,
    position_ms: u64,
    recu: tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
}

impl Banc {
    /// Une zone DLNA qui joue, volume 100 % en base et en mémoire, PURE
    /// verrouillé ou non.
    async fn monter(pure_verrouille: bool) -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("My Devialet", Some("dlna"), Some(APPAREIL))
            .unwrap();
        ZoneRepo::with_backend(db.clone())
            .update_volume(zone_id, 100.0)
            .unwrap();
        if pure_verrouille {
            SettingsRepo::with_backend(db.clone())
                .set(
                    &format!("zone_{zone_id}_audiophile"),
                    r#"{"enabled":true,"lock_volume":true}"#,
                )
                .unwrap();
        }
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(
            MockOutput::new(APPAREIL, "My Devialet").with_type("dlna"),
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
        playback
            .play(
                zone_id,
                NowPlaying {
                    title: "O Fortuna".into(),
                    source: "upnp".into(),
                    duration_ms: DUREE_MS as i64,
                    ..Default::default()
                },
            )
            .await;
        // Volume en mémoire posé SANS `mark_volume_changed` : aucune grâce de
        // volume ne doit masquer l'adoption.
        playback.set_volume(zone_id, 1.0).await;
        let banc = Self {
            poller,
            db,
            outputs,
            zone_id,
            poll_states: HashMap::new(),
            idle: HashMap::new(),
            position_ms: 10_000,
            recu,
        };
        // Sous PURE verrouillé, l'appareil part conforme (100 %) : un écart
        // dès la première observation serait rattrapé par
        // `volume_pure_concilie`, et sa grâce de volume masquerait le front
        // que ces tests provoquent ensuite.
        banc.appareil_a(if pure_verrouille { 1.0 } else { 0.5 })
            .await;
        banc
    }

    async fn avec_mock<R>(&self, f: impl AsyncFnOnce(&MockOutput) -> R) -> R {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        f(sortie.as_any().downcast_ref::<MockOutput>().unwrap()).await
    }

    /// Le volume de l'appareil, changé hors de Tune (télécommande,
    /// application du fabricant). Rend le nombre de commandes déjà comptées,
    /// pour que ce geste-ci ne soit pas pris pour une commande de Tune.
    async fn appareil_a(&self, volume: f64) -> usize {
        self.avec_mock(async |m| {
            m.set_volume(volume).await.unwrap();
            m.volume_call_count().await
        })
        .await
    }

    async fn commandes_depuis(&self, deja: usize) -> Vec<f64> {
        self.avec_mock(async |m| m.volume_calls().await[deja..].to_vec())
            .await
    }

    /// Un tour de la vraie boucle, l'appareil en lecture qui avance.
    async fn tic(&mut self) {
        self.position_ms += 1_000;
        let position = self.position_ms;
        self.avec_mock(async |m| {
            m.set_state(TransportState::Playing).await;
            m.set_duration(DUREE_MS);
            m.set_position(position);
        })
        .await;
        self.poller
            .tick(&mut self.poll_states, &mut self.idle, &Instant::now())
            .await;
    }

    /// La zone à `pour_cent` en base ET en mémoire, sans grâce de volume.
    async fn zone_a(&self, pour_cent: f64) {
        ZoneRepo::with_backend(self.db.clone())
            .update_volume(self.zone_id, pour_cent)
            .unwrap();
        self.poller
            .playback
            .set_volume(self.zone_id, pour_cent / 100.0)
            .await;
    }

    fn volume_en_base(&self) -> f64 {
        ZoneRepo::with_backend(self.db.clone())
            .get(self.zone_id)
            .unwrap()
            .unwrap()
            .volume
    }

    fn avertissements(&mut self) -> Vec<serde_json::Value> {
        let mut v = Vec::new();
        while let Ok(e) = self.recu.try_recv() {
            if e.event_type == "zone.playback_error" {
                v.push(e.data);
            }
        }
        v
    }
}

/// LE défaut du fil 2119 : l'appareil passe à 83 % hors de Tune ; le sondeur
/// recopiait 83 en base, et le chemin du signal affichait `Volume 83%` sous
/// PURE forcé. Désormais : rien n'est adopté, le 100 % est recommandé, et
/// c'est dit UNE fois, sans arrêter la lecture.
#[tokio::test]
async fn sous_pure_force_le_sondeur_reimpose_100_au_lieu_d_adopter_5695() {
    let mut banc = Banc::monter(true).await;
    banc.tic().await; // première observation, appareil conforme.
    let deja = banc.appareil_a(0.83).await;
    banc.tic().await;

    assert_eq!(
        banc.volume_en_base(),
        100.0,
        "PURE forcé : le 83 % de l'appareil ne doit pas devenir le volume de la zone"
    );
    assert_eq!(
        banc.commandes_depuis(deja).await,
        vec![1.0],
        "le sondeur doit réimposer 100 % à l'appareil, sans trim"
    );
    let avertis = banc.avertissements();
    assert_eq!(avertis.len(), 1, "un avertissement, pas plus : {avertis:?}");
    assert_eq!(avertis[0]["fatal"], false, "la lecture continue");
    assert_eq!(avertis[0]["zone_id"], banc.zone_id);
    assert!(
        avertis[0]["error"].as_str().unwrap().contains("83 %"),
        "le bandeau nomme le volume rapporté : {}",
        avertis[0]["error"]
    );
}

/// TÉMOIN : le même banc, hors PURE, ADOPTE bien le 83 % de l'appareil —
/// c'est le comportement voulu d'une zone ordinaire, et la preuve que le banc
/// passe réellement par le site d'adoption.
#[tokio::test]
async fn temoin_hors_pure_le_sondeur_adopte_le_volume_de_l_appareil() {
    let mut banc = Banc::monter(false).await;
    banc.tic().await;
    let deja = banc.appareil_a(0.83).await;
    banc.tic().await;

    assert!(
        (banc.volume_en_base() - 83.0).abs() < 1e-6,
        "hors PURE, le volume changé sur l'appareil doit être adopté, pas {}",
        banc.volume_en_base()
    );
    assert!(banc.commandes_depuis(deja).await.is_empty());
    assert!(banc.avertissements().is_empty());
}

/// Une fois par zone : un second écart est réimposé, mais pas redit.
#[tokio::test]
async fn l_avertissement_ne_part_qu_une_fois_par_zone_5695() {
    let mut banc = Banc::monter(true).await;
    assert!(banc.poller.volume_pure_reimpose(banc.zone_id, 0.83).await);
    assert!(banc.poller.volume_pure_reimpose(banc.zone_id, 0.6).await);
    assert_eq!(banc.avertissements().len(), 1);
    assert_eq!(banc.volume_en_base(), 100.0);
}

/// Hors verrou, l'aiguillage ne fait rien : ni commande, ni bandeau.
#[tokio::test]
async fn hors_verrou_l_aiguillage_laisse_adopter() {
    let mut banc = Banc::monter(false).await;
    let deja = banc.appareil_a(0.83).await;
    assert!(!banc.poller.volume_pure_reimpose(banc.zone_id, 0.83).await);
    assert!(banc.commandes_depuis(deja).await.is_empty());
    assert!(banc.avertissements().is_empty());
}

#[test]
fn le_bandeau_dit_l_echec_de_la_reimposition() {
    let m = volume_pure_5695::message_de_volume_reimpose(0.83, Some("timeout"));
    assert!(m.contains("83 %") && m.contains("n'a pas pu") && m.contains("timeout"));
}

/// L'état hérité : la zone était DÉJÀ à 83 % en base et sur l'appareil quand
/// le verrou s'est mis à valoir 100 % (montée de version depuis une base où le
/// trim avait été composé, ou verrou global armé sur une zone déjà en PURE).
/// Aucun front de volume ne survient alors : l'adoption n'est jamais tentée,
/// donc la réimposition non plus, et « Volume 83 % » restait affiché à vie.
/// Le sondeur doit rattraper l'écart à sa première observation.
#[tokio::test]
async fn sous_pure_force_un_volume_herite_est_rattrape_sans_front_5695() {
    let mut banc = Banc::monter(true).await;
    banc.zone_a(83.0).await;
    let deja = banc.appareil_a(0.83).await;
    banc.tic().await;
    banc.tic().await;

    assert_eq!(
        banc.volume_en_base(),
        100.0,
        "PURE forcé : un 83 % hérité doit être ramené à 100 %"
    );
    assert_eq!(
        banc.commandes_depuis(deja).await,
        vec![1.0],
        "le 100 % est commandé UNE fois, sans trim, pas à chaque tour"
    );
}

/// Le verrou GLOBAL armé sur une zone déjà en PURE (sans surcharge par zone) :
/// la route de configuration ne commande rien, c'est le sondeur qui rattrape.
/// Avant l'armement, la zone n'est pas touchée (témoin).
#[tokio::test]
async fn le_verrou_global_arme_apres_coup_ramene_la_zone_a_100_5695() {
    let mut banc = Banc::monter(false).await;
    SettingsRepo::with_backend(banc.db.clone())
        .set(
            &format!("zone_{}_audiophile", banc.zone_id),
            r#"{"enabled":true}"#,
        )
        .unwrap();
    banc.zone_a(83.0).await;
    let deja = banc.appareil_a(0.83).await;
    banc.tic().await;
    assert!(
        (banc.volume_en_base() - 83.0).abs() < 1e-6,
        "verrou global éteint : la zone garde son volume"
    );
    assert!(banc.commandes_depuis(deja).await.is_empty());

    SettingsRepo::with_backend(banc.db.clone())
        .set(crate::audio::audiophile::SETTING_LOCK_VOLUME, "true")
        .unwrap();
    banc.tic().await;
    banc.tic().await;
    assert_eq!(banc.volume_en_base(), 100.0);
    assert_eq!(banc.commandes_depuis(deja).await, vec![1.0]);
}
