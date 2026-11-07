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

pub(super) struct Banc {
    poller: PositionPoller,
    db: Arc<dyn crate::db::backend::DbBackend>,
    outputs: Arc<Mutex<OutputRegistry>>,
    pub(super) zone_id: i64,
    poll_states: HashMap<i64, ZonePollState>,
    idle: HashMap<i64, IdlePollBackoff>,
    position_ms: u64,
    recu: tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
    /// L'identifiant de la sortie factice (#7379 : DLNA ou locale).
    appareil: &'static str,
}

impl Banc {
    /// Une zone DLNA qui joue, volume 100 % en base et en mémoire, PURE
    /// verrouillé ou non.
    pub(super) async fn monter(pure_verrouille: bool) -> Self {
        Self::monter_sur(pure_verrouille, APPAREIL, "dlna").await
    }

    /// #7379 — le même banc sur la sortie factice `appareil` de type
    /// `output_type` (`dlna`, ou `local` pour une sortie à volume logiciel).
    pub(super) async fn monter_sur(
        pure_verrouille: bool,
        appareil: &'static str,
        output_type: &str,
    ) -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("My Devialet", Some(output_type), Some(appareil))
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
            MockOutput::new(appareil, "My Devialet").with_type(output_type),
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
            appareil,
        };
        // Sous PURE verrouillé, l'appareil part à 100 %, comme après un
        // armement confirmé : la baisse que ces tests provoquent ensuite est
        // un vrai front, vu par le site d'adoption.
        banc.appareil_a(if pure_verrouille { 1.0 } else { 0.5 })
            .await;
        banc
    }

    async fn avec_mock<R>(&self, f: impl AsyncFnOnce(&MockOutput) -> R) -> R {
        let reg = self.outputs.lock().await;
        let arc = reg.get(self.appareil).unwrap();
        let sortie = arc.lock().await;
        f(sortie.as_any().downcast_ref::<MockOutput>().unwrap()).await
    }

    /// Le volume de l'appareil, changé hors de Tune (télécommande,
    /// application du fabricant). Rend le nombre de commandes déjà comptées,
    /// pour que ce geste-ci ne soit pas pris pour une commande de Tune.
    pub(super) async fn appareil_a(&self, volume: f64) -> usize {
        self.avec_mock(async |m| {
            m.set_volume(volume).await.unwrap();
            m.volume_call_count().await
        })
        .await
    }

    /// #6739 — une consigne de volume par le VRAI orchestrateur, puis la
    /// grâce de volume écoulée (le sondeur juge le relevé au tour suivant).
    pub(super) async fn consigne(&self, volume: f64) {
        // #6721 — la consigne du banc est un geste CONFIRMÉ : la garde contre
        // les montées brusques n'est pas l'objet de ces tests.
        self.poller
            .orchestrator
            .set_volume_avec_garde(
                self.zone_id,
                volume,
                Some(self.appareil),
                crate::audio::garde_de_hausse::Hausse::Confirmee,
            )
            .await
            .unwrap();
        self.poller
            .playback
            .ecouler_la_grace_de_volume(self.zone_id)
            .await;
    }

    /// #6721 — un geste NON confirmé (curseur, pas) par le VRAI orchestrateur,
    /// puis la grâce de volume écoulée.
    pub(super) async fn consigne_gardee(&self, volume: f64) {
        self.poller
            .orchestrator
            .set_volume(self.zone_id, volume, Some(self.appareil))
            .await
            .unwrap();
        self.poller
            .playback
            .ecouler_la_grace_de_volume(self.zone_id)
            .await;
    }

    /// #8031 — la sourdine par le VRAI orchestrateur (`SetMute`, #6700).
    pub(super) async fn sourdine(&self, muted: bool) {
        self.poller
            .orchestrator
            .set_mute(self.zone_id, muted, Some(self.appareil))
            .await
            .unwrap();
    }

    /// #8031 — la grâce qui suit un changement de sourdine, écoulée.
    pub(super) async fn ecouler_la_grace_de_sourdine(&self) {
        self.poller
            .playback
            .ecouler_la_grace_de_sourdine(self.zone_id)
            .await;
    }

    /// #8031 — les évènements du gestionnaire de lecture (ce que les clients
    /// reçoivent : `playback.volume`, `playback.muted`).
    pub(super) fn evenements(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::playback::PlaybackEvent> {
        self.poller.playback.subscribe()
    }

    pub(super) async fn commandes_depuis(&self, deja: usize) -> Vec<f64> {
        self.avec_mock(async |m| m.volume_calls().await[deja..].to_vec())
            .await
    }

    /// Pause puis reprise par le VRAI orchestrateur, sur la sortie factice.
    pub(super) async fn pause_puis_reprise(&self) {
        let orch = self.poller.orchestrator.clone();
        orch.pause(self.zone_id, Some(self.appareil)).await.unwrap();
        orch.resume(self.zone_id, Some(self.appareil))
            .await
            .unwrap();
    }

    /// Le volume de la zone en mémoire (celui que le client lit).
    pub(super) async fn volume_en_memoire(&self) -> f64 {
        self.poller.playback.get_state(self.zone_id).await.volume
    }

    pub(super) fn base(&self) -> Arc<dyn crate::db::backend::DbBackend> {
        self.db.clone()
    }

    /// Un tour de la vraie boucle, l'appareil en lecture qui avance.
    pub(super) async fn tic(&mut self) {
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

    /// #8031 — la zone joue une webradio (site d'adoption « radio »).
    pub(super) async fn en_radio(&self) {
        self.poller
            .playback
            .play(
                self.zone_id,
                NowPlaying {
                    title: "FIP".into(),
                    source: "radio".into(),
                    ..Default::default()
                },
            )
            .await;
    }

    /// #8031 — un tour sur une zone radio : l'étranglement du sondage radio
    /// (`RADIO_POLL_INTERVAL_SECS`) est échu, l'appareil est relu.
    pub(super) async fn tic_radio(&mut self) {
        if let Some(ps) = self.poll_states.get_mut(&self.zone_id)
            && let Some(avant) = Instant::now()
                .checked_sub(std::time::Duration::from_secs(RADIO_POLL_INTERVAL_SECS + 1))
        {
            ps.last_radio_poll = avant;
        }
        self.avec_mock(async |m| m.set_state(TransportState::Playing).await)
            .await;
        self.poller
            .tick(&mut self.poll_states, &mut self.idle, &Instant::now())
            .await;
    }

    /// La zone à `pour_cent` en base ET en mémoire, sans grâce de volume.
    pub(super) async fn zone_a(&self, pour_cent: f64) {
        ZoneRepo::with_backend(self.db.clone())
            .update_volume(self.zone_id, pour_cent)
            .unwrap();
        self.poller
            .playback
            .set_volume(self.zone_id, pour_cent / 100.0)
            .await;
    }

    /// #7379 — les évènements de lecture, tels que le serveur les relaie aux
    /// clients (`volume` : le curseur).
    pub(super) fn evenements_de_lecture(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::playback::PlaybackEvent> {
        self.poller.playback.subscribe()
    }

    /// Pose un réglage clé/valeur (#6541 : les limites de volume).
    pub(super) fn poser_reglage(&self, cle: &str, valeur: &str) {
        SettingsRepo::with_backend(self.db.clone())
            .set(cle, valeur)
            .unwrap();
    }

    pub(super) fn volume_en_base(&self) -> f64 {
        ZoneRepo::with_backend(self.db.clone())
            .get(self.zone_id)
            .unwrap()
            .unwrap()
            .volume
    }

    /// #7379 — la réponse de volume de l'appareil devient illisible (repli
    /// 0,5 de #5662) ou redevient lisible.
    pub(super) async fn volume_illisible(&self, illisible: bool) {
        self.avec_mock(async |m| m.rendre_le_volume_illisible(illisible))
            .await;
    }

    /// #7379 — la zone ne joue plus dans Tune (site « zone au repos »).
    pub(super) async fn arreter_dans_tune(&self) {
        self.poller.playback.stop(self.zone_id).await;
    }

    /// #7379 — un tour sur une zone au repos, l'appareil dans `etat`, la
    /// grâce de démarrage du sondeur (30 s) échue.
    pub(super) async fn tic_au_repos(&mut self, etat: TransportState) {
        self.avec_mock(async |m| m.set_state(etat).await).await;
        // Le recul de repos (`IDLE_REPOS_POLL_TICKS`) n'est pas l'objet des
        // témoins : chaque tour relit l'appareil.
        self.idle.clear();
        let demarrage = Instant::now()
            .checked_sub(std::time::Duration::from_secs(120))
            .unwrap();
        self.poller
            .tick(&mut self.poll_states, &mut self.idle, &demarrage)
            .await;
    }

    /// #7379 (B2) — ce que fait `PATCH /zones/{id}` quand le trim change : le
    /// trim posé (effacé à 0), puis la consigne courante repoussée par
    /// `set_volume` (« Effet immédiat »), grâce de volume écoulée ensuite.
    pub(super) async fn changer_le_trim(&self, trim_db: f64) {
        let cle = format!("zone_{}_gain_trim_db", self.zone_id);
        let reglages = SettingsRepo::with_backend(self.db.clone());
        if trim_db == 0.0 {
            reglages.delete(&cle).unwrap();
        } else {
            reglages.set(&cle, &format!("{trim_db}")).unwrap();
        }
        let consigne = self.volume_en_base() / 100.0;
        self.poller
            .orchestrator
            .set_volume(self.zone_id, consigne, Some(self.appareil))
            .await
            .unwrap();
        self.poller
            .playback
            .ecouler_la_grace_de_volume(self.zone_id)
            .await;
    }

    /// #7379 — une consigne de l'auditeur, la grâce de volume LAISSÉE ouverte
    /// (elle vient d'arriver).
    pub(super) async fn consigne_a_l_instant(&self, volume: f64) {
        self.poller
            .orchestrator
            .set_volume(self.zone_id, volume, Some(self.appareil))
            .await
            .unwrap();
    }

    /// #7379 — applique un relevé jugé AVANT une consigne concurrente
    /// (`ancien` : la consigne vue au relevé).
    pub(super) async fn appliquer_un_releve(
        &self,
        ancien: f64,
        suite: super::releve_ne_monte_pas_6739::SuiteDuReleve,
    ) {
        self.poller
            .appliquer_la_suite_du_releve(self.zone_id, self.appareil, ancien, suite)
            .await;
    }

    /// #7379 — une seconde zone en lecture, sur sa propre sortie factice
    /// `dlna`, dans la même base ; elle rejoint le groupe `groupe` avec la
    /// première. Rend son identifiant.
    pub(super) async fn grouper_avec(&self, appareil: &'static str, groupe: &str) -> i64 {
        let repo = ZoneRepo::with_backend(self.db.clone());
        let zone = repo
            .create("Salon B", Some("dlna"), Some(appareil))
            .unwrap();
        repo.update_volume(zone, 40.0).unwrap();
        repo.update_group(zone, Some(groupe)).unwrap();
        repo.update_group(self.zone_id, Some(groupe)).unwrap();
        self.outputs.lock().await.register(Box::new(
            MockOutput::new(appareil, "Salon B").with_type("dlna"),
        ));
        self.poller
            .playback
            .play(
                zone,
                NowPlaying {
                    title: "O Fortuna".into(),
                    source: "upnp".into(),
                    duration_ms: DUREE_MS as i64,
                    ..Default::default()
                },
            )
            .await;
        self.poller.playback.set_volume(zone, 0.4).await;
        self.appareil_de(appareil, 0.4).await;
        zone
    }

    /// #7379 — le volume de la sortie factice `appareil`, changé hors de Tune.
    pub(super) async fn appareil_de(&self, appareil: &str, volume: f64) -> usize {
        let reg = self.outputs.lock().await;
        let arc = reg.get(appareil).unwrap();
        let sortie = arc.lock().await;
        let m = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        m.set_volume(volume).await.unwrap();
        m.set_state(TransportState::Playing).await;
        m.set_duration(DUREE_MS);
        m.set_position(self.position_ms);
        m.volume_call_count().await
    }

    /// #7379 — les commandes de volume reçues par `appareil` depuis `deja`.
    pub(super) async fn commandes_de(&self, appareil: &str, deja: usize) -> Vec<f64> {
        let reg = self.outputs.lock().await;
        let arc = reg.get(appareil).unwrap();
        let sortie = arc.lock().await;
        let m = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        m.volume_calls().await[deja..].to_vec()
    }

    /// #7379 — le volume en base de la zone `zone`.
    pub(super) fn volume_en_base_de(&self, zone: i64) -> f64 {
        ZoneRepo::with_backend(self.db.clone())
            .get(zone)
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

/// #6721 (fil 2366, Marantz NR1200, tweeter détruit) — décision de Bertrand
/// du 22/10 : sous PURE verrouillé, une baisse faite SUR L'APPAREIL
/// (télécommande, application du fabricant) est respectée. Tune ne remonte
/// jamais le volume tout seul. Avant ce correctif (#5695), le sondeur voyait
/// le 27 % et commandait `[1.0]` à l'appareil.
#[tokio::test]
async fn sous_pure_force_une_baisse_sur_l_appareil_est_respectee_6721() {
    let mut banc = Banc::monter(true).await;
    banc.tic().await; // première observation, appareil à 100 %.
    let deja = banc.appareil_a(0.27).await;
    for _ in 0..3 {
        banc.tic().await;
    }
    assert_eq!(
        banc.commandes_depuis(deja).await,
        Vec::<f64>::new(),
        "#6721 : le sondeur ne doit JAMAIS remonter de lui-même le volume de l'appareil"
    );
    assert!(
        (banc.volume_en_base() - 27.0).abs() < 1e-6,
        "la baisse de l'appareil est adoptée comme pour toute zone, pas {}",
        banc.volume_en_base()
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

/// #6721 — une zone déjà à 83 % quand le verrou se met à valoir 100 % (base
/// héritée, verrou global armé après coup) n'est plus « rattrapée » à 100 % :
/// aucun ordre ne part sans geste. Avant ce correctif, le sondeur commandait
/// `[1.0]` dès sa première observation.
#[tokio::test]
async fn sous_pure_force_un_volume_herite_n_est_pas_remonte_6721() {
    let mut banc = Banc::monter(true).await;
    banc.zone_a(83.0).await;
    let deja = banc.appareil_a(0.83).await;
    banc.tic().await;
    banc.tic().await;
    assert_eq!(
        banc.commandes_depuis(deja).await,
        Vec::<f64>::new(),
        "#6721 : aucun 100 % commandé par le sondeur"
    );
    assert!((banc.volume_en_base() - 83.0).abs() < 1e-6);
}

/// #6721 — le verrou GLOBAL armé sur une zone déjà en PURE ne fait pas non
/// plus monter la zone : seul l'armement confirmé (route PURE) commande 100 %.
#[tokio::test]
async fn le_verrou_global_arme_apres_coup_ne_remonte_pas_la_zone_6721() {
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
    SettingsRepo::with_backend(banc.db.clone())
        .set(crate::audio::audiophile::SETTING_LOCK_VOLUME, "true")
        .unwrap();
    banc.tic().await;
    banc.tic().await;
    assert_eq!(banc.commandes_depuis(deja).await, Vec::<f64>::new());
    assert!((banc.volume_en_base() - 83.0).abs() < 1e-6);
}

/// #5662 — sur le VRAI tick, hors PURE : l'appareil passe de lui-même à
/// 100 %. Tune ne l'adopte pas (la règle reste), n'envoie aucune commande
/// (il n'impose rien), et le SIGNALE : un `zone.volume_externe` à
/// l'ouverture, aucun pendant l'épisode, un à la fin.
#[tokio::test]
async fn un_renderer_a_100_est_signale_sans_etre_adopte_ni_corrige_5662() {
    let mut banc = Banc::monter(false).await;
    banc.zone_a(42.0).await;
    banc.appareil_a(0.42).await;
    banc.tic().await;
    let deja = banc.appareil_a(1.0).await;
    for _ in 0..5 {
        banc.tic().await;
    }
    let mut externes = Vec::new();
    while let Ok(e) = banc.recu.try_recv() {
        if e.event_type == "zone.volume_externe" {
            externes.push(e.data);
        }
    }
    assert_eq!(
        externes.len(),
        1,
        "un seul signal pour cinq tours à 100 % : {externes:?}"
    );
    assert_eq!(externes[0]["zone_id"], banc.zone_id);
    assert_eq!(externes[0]["actif"], true);
    assert_eq!(externes[0]["renderer_volume"], 1.0);
    assert_eq!(externes[0]["tune_volume"], 0.42);
    assert_eq!(
        banc.volume_en_base(),
        42.0,
        "le 100 % de l'appareil ne doit pas être adopté"
    );
    assert!(
        banc.commandes_depuis(deja).await.is_empty(),
        "Tune signale et n'impose rien : aucune commande de volume"
    );

    banc.appareil_a(0.42).await;
    banc.tic().await;
    banc.tic().await;
    let mut fins = Vec::new();
    while let Ok(e) = banc.recu.try_recv() {
        if e.event_type == "zone.volume_externe" {
            fins.push(e.data);
        }
    }
    assert_eq!(fins.len(), 1, "une seule fin d'épisode : {fins:?}");
    assert_eq!(fins[0]["actif"], false);
}
