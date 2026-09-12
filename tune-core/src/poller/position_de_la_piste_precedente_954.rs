//! La position publiée ne doit jamais porter la piste PRÉCÉDENTE
//! (renesenses/tune-web-client#954).
//!
//! Fil forum 1764, 0.9.145, Linux/ALSA, zone locale : « the next track will
//! start after clicking, but the timeline will stay in the same place, or jump
//! a bit. After the 3rd - 4th track change the line resets correctly. »
//!
//! # Le fait mesuré
//!
//! Le sondeur publie la position AVANT de décider si l'échantillon est
//! recevable. Les deux instructions vivent dans le même tour, dans cet ordre :
//!
//! 1. `tick.rs` — `update_position` puis `emit_position` : la valeur rendue par
//!    la sortie devient l'état servi par `GET /zones` et l'évènement
//!    `position` ;
//! 2. `tick.rs`, quatre-vingts lignes plus bas — `stale_start_position` :
//!    « discard provably-stale early samples BEFORE they poison anything »,
//!    puis `continue`.
//!
//! Entre le moment où `PlaybackManager::play` bascule `now_playing` sur la
//! piste suivante et le moment où la sortie joue réellement ce flux, il s'écoule
//! du temps — le journal du .18 du 12/09/2026 le chiffre à **deux secondes**
//! (`playback_timing … output_ms=2002`, entre
//! `poller_track_generation_changed_resetting_state` à 14:18:55.975 et
//! `oaat: play_media … title=Mon ego` à 14:18:57.080). Pendant cette fenêtre la
//! sortie rend encore la position de la piste précédente, et le sondeur la
//! recopie sur la piste NEUVE.
//!
//! La garde de monotonie de `update_position` (#3229) fait le reste : la valeur
//! périmée, plus GRANDE que le plancher que `play` vient de remettre à zéro, est
//! acceptée comme une avance et devient le nouveau plancher. Les échantillons
//! honnêtes qui suivent — 0, 1 s, 2 s du nouveau morceau — sont alors des reculs,
//! et il en faut `OBSERVATIONS_EN_RECUL_AVANT_DE_CEDER` (cinq, soit cinq
//! secondes à `POLL_INTERVAL_MS`) pour que le plancher cède.
//!
//! # Pourquoi « ça se recale après 3-4 changements »
//!
//! Le mensonge vaut la position d'où l'on vient. Au premier saut on vient du
//! milieu d'un morceau : l'erreur est énorme et se voit. Aux suivants, enchaînés
//! vite, on ne vient que de deux ou trois secondes de lecture : la valeur
//! périmée est petite et l'écran a l'air juste. Rien ne s'est réparé.
//!
//! # Ce que ces témoins ne voient pas
//!
//! Ils montent le VRAI `tick` avec une sortie factice, donc ils ne disent rien
//! de la durée réelle de la fenêtre sur une sortie ALSA, ni du rendu de l'écran.
//! Ils tiennent l'invariant du serveur : *ce que le serveur publie décrit la
//! piste qu'il annonce*.

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

const APPAREIL: &str = "local:temoin-954";

/// Banc minimal : une zone, une sortie factice dont on pose la position à la
/// main, et le vrai `tick` appelé tour par tour avec un état de sondage qui
/// SURVIT d'un tour à l'autre (sans quoi le changement de génération ne serait
/// jamais détecté).
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
            .create("Salon", Some("local"), Some(APPAREIL))
            .unwrap();
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(
            MockOutput::new(APPAREIL, "Témoin 954").with_type("local"),
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

    /// Lancer une piste : c'est le geste de l'auditeur qui clique.
    async fn jouer(&mut self, track_id: i64, titre: &str, duree_ms: i64) {
        self.playback
            .play(
                self.zone_id,
                NowPlaying {
                    track_id: Some(track_id),
                    title: titre.into(),
                    source: "local".into(),
                    duration_ms: duree_ms,
                    ..Default::default()
                },
            )
            .await;
        let generation = self.playback.get_state(self.zone_id).await.track_generation;
        self.poll_states
            .entry(self.zone_id)
            .or_insert_with(|| ZonePollState::new(generation));
    }

    /// Ce que la SORTIE rend au sondeur ce tour-ci.
    async fn renderer(&self, etat: TransportState, position_ms: u64, duree_ms: u64) {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.set_state(etat).await;
        mock.set_duration(duree_ms);
        mock.set_position(position_ms);
    }

    /// Reculer l'horloge de piste pour que le premier échantillon soit jugé
    /// plausible (`stale_start_position` compare à l'horloge murale).
    fn dater_le_debut(&mut self, il_y_a: Duration) {
        let ps = self.poll_states.get_mut(&self.zone_id).unwrap();
        ps.track_started_at = Some(Instant::now() - il_y_a);
    }

    async fn tic(&mut self) {
        self.poller
            .tick(&mut self.poll_states, &mut self.idle, &Instant::now())
            .await;
    }

    /// Ce que `GET /zones` sert : `routes/zones/lecture.rs` compose
    /// `position_ms` et `current_track` depuis CE seul appel.
    async fn servi(&self) -> (i64, Option<i64>) {
        let etat = self.playback.get_state(self.zone_id).await;
        (
            etat.position_ms,
            etat.now_playing.and_then(|np| np.track_id),
        )
    }
}

/// Le couple servi par `GET /zones` reste-t-il cohérent au changement de piste ?
///
/// Rouge attendu sans correctif : la piste est la 2, la position celle de la 1.
#[tokio::test]
async fn au_changement_de_piste_la_position_servie_ne_reste_pas_celle_de_la_precedente() {
    let mut banc = Banc::monter().await;

    // ── Piste 1, trois cents secondes, écoutée jusqu'à 2:30 ──
    banc.jouer(1, "Piste A", 300_000).await;
    banc.dater_le_debut(Duration::from_secs(160));
    banc.renderer(TransportState::Playing, 150_000, 300_000)
        .await;
    banc.tic().await;
    assert_eq!(
        banc.servi().await,
        (150_000, Some(1)),
        "le banc lui-même est faux : la piste 1 devrait être servie à 2:30"
    );

    // ── L'auditeur clique « suivant » ──
    //
    // `play` bascule `now_playing` et remet la position à zéro. La SORTIE, elle,
    // n'a pas encore reçu le nouveau flux : elle rend toujours 2:30 (deux
    // secondes de fenêtre mesurées sur le .18, `output_ms=2002`).
    banc.jouer(2, "Piste B", 200_000).await;
    banc.renderer(TransportState::Playing, 150_000, 300_000)
        .await;
    banc.tic().await;

    let (position, piste) = banc.servi().await;
    assert_eq!(piste, Some(2), "la zone doit bien annoncer la piste 2");
    assert!(
        position < 5_000,
        "le serveur sert la piste 2 avec la position de la piste 1 : \
         position_ms={position} alors que la lecture vient de commencer. \
         C'est ce couple incohérent que l'écran affiche (#954)."
    );
}

/// Une fois la sortie honnête, combien de tours le mensonge tient-il ?
///
/// Rouge attendu sans correctif : le plancher de monotonie garde la valeur
/// périmée jusqu'à ce que cinq reculs consécutifs le fassent céder.
#[tokio::test]
async fn la_position_perimee_ne_survit_pas_au_premier_echantillon_honnete() {
    let mut banc = Banc::monter().await;

    banc.jouer(1, "Piste A", 300_000).await;
    banc.dater_le_debut(Duration::from_secs(160));
    banc.renderer(TransportState::Playing, 150_000, 300_000)
        .await;
    banc.tic().await;

    // Changement de piste, puis un tour où la sortie rend encore l'ancienne
    // position — c'est lui qui pose le faux plancher.
    banc.jouer(2, "Piste B", 200_000).await;
    banc.renderer(TransportState::Playing, 150_000, 300_000)
        .await;
    banc.tic().await;

    // La sortie joue enfin la piste 2 et le dit : une seconde.
    banc.renderer(TransportState::Playing, 1_000, 200_000).await;
    banc.tic().await;

    let (position, piste) = banc.servi().await;
    assert_eq!(piste, Some(2));
    assert!(
        position < 5_000,
        "la sortie annonce 1 s sur la piste 2 et le serveur sert toujours \
         {position} ms : le plancher de monotonie a adopté la position de la \
         piste précédente et ne cédera qu'après cinq reculs (#954)."
    );
}
