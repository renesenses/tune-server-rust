//! #4666 — après une pause, la zone reprend mais le sondeur ne la voit plus
//! jouer : `stale_start_position_ignored … wall_s=0` à chaque tour, position
//! jamais publiée, fin de piste jamais vue, aucun enchaînement.
//!
//! Fil forum 1882, Jean Valjean, 0.9.161 Windows, sortie locale WASAPI
//! exclusive. Deux épisodes le même soir, sur deux albums :
//!
//! - pause à 2:19, reprise deux heures plus tard par le rétablissement de
//!   session (#4177 : le périphérique exclusif a été rendu à la pause) ; la
//!   piste va au bout et la zone reste figée 9 min 25 s ;
//! - au second épisode, la boucle `stale_start_position_ignored wall_s=0`
//!   tourne PENDANT que la position avance d'une seconde par tour, et le
//!   testeur écrit « le morceau redémarre à 0:00 pour Tune mais en fait le
//!   morceau continue ».
//!
//! # Le mécanisme, lu dans le code
//!
//! 1. `tick` ouvre sur `poll_states.retain(… Playing)` : une zone en PAUSE perd
//!    son état de sondage ;
//! 2. à la reprise, l'état est recréé par `ZonePollState::new(génération
//!    courante)` : la génération est égale, donc la remise à zéro « changement
//!    de piste » — seul site qui date `track_started_at` hors du chemin de
//!    position — ne s'exécute pas, et `track_started_at` reste `None` ;
//! 3. `wall_elapsed = 0`, la sortie rend la position de reprise, et
//!    `stale_start_position(0, 139_666)` est vrai : `continue` ;
//! 4. les trois sites qui reposent `track_started_at` sont APRÈS ce
//!    `continue`. Rien ne peut plus sortir de là avant un nouveau `play`.
//!
//! La publication de position (`echantillon_perime`) appelle le même prédicat :
//! l'écran garde la position d'avant — ou 0:00 après un rétablissement, puisque
//! `play()` l'a remise à zéro. C'est la phrase du testeur.
//!
//! # Ce que ces témoins ne voient pas
//!
//! Ils montent le VRAI `tick` avec une sortie factice. Ils ne disent rien de
//! l'origine de `error decoding response body` (non élucidée), ni du rendu
//! WASAPI : aucune machine Windows n'a été utilisée.

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

const APPAREIL: &str = "local:temoin-4666";
const DUREE_MS: u64 = 328_602;

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
            .create("Local", Some("local"), Some(APPAREIL))
            .unwrap();
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(
            MockOutput::new(APPAREIL, "Haut-parleurs").with_type("local"),
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

    fn piste() -> NowPlaying {
        NowPlaying {
            track_id: Some(11),
            title: "High Above The Storm".into(),
            source: "local".into(),
            duration_ms: DUREE_MS as i64,
            ..Default::default()
        }
    }

    /// La piste joue depuis 2:30 : un tour de sondage l'a constaté et publié.
    async fn jouer_jusqu_a(&mut self, position_ms: u64) {
        self.playback.play(self.zone_id, Self::piste()).await;
        let generation = self.playback.get_state(self.zone_id).await.track_generation;
        let ps = self
            .poll_states
            .entry(self.zone_id)
            .or_insert_with(|| ZonePollState::new(generation));
        ps.track_started_at = Some(Instant::now() - Duration::from_millis(position_ms + 2_000));
        self.renderer(TransportState::Playing, position_ms).await;
        self.tic().await;
        assert_eq!(
            self.servi().await,
            position_ms as i64,
            "le banc lui-même est faux : la position devrait être publiée"
        );
    }

    /// L'auditeur met en pause : l'état de sondage est jeté par `retain`.
    async fn mettre_en_pause(&mut self, position_ms: u64) {
        self.playback.pause(self.zone_id).await;
        self.renderer(TransportState::Paused, position_ms).await;
        self.tic().await;
        assert!(
            !self.poll_states.contains_key(&self.zone_id),
            "le banc lui-même est faux : une zone en pause perd son état de sondage"
        );
    }

    async fn renderer(&self, etat: TransportState, position_ms: u64) {
        let reg = self.outputs.lock().await;
        let arc = reg.get(APPAREIL).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.set_state(etat).await;
        mock.set_duration(DUREE_MS);
        mock.set_position(position_ms);
    }

    async fn tic(&mut self) {
        self.poller
            .tick(&mut self.poll_states, &mut self.idle, &Instant::now())
            .await;
    }

    async fn servi(&self) -> i64 {
        self.playback.get_state(self.zone_id).await.position_ms
    }

    fn etat(&self) -> EtatDeLecture {
        self.poll_states
            .get(&self.zone_id)
            .expect("la zone joue : elle doit avoir un état de sondage")
            .etat
            .clone()
    }
}

/// Reprise ORDINAIRE (session vivante, `resume` sur place) : le premier
/// échantillon après la pause doit être reçu, pas jeté comme fantôme.
///
/// Rouge attendu sans correctif : l'état neuf part d'une horloge à zéro,
/// `stale_start_position(0, 151_000)` saute le tour, la zone reste `Neuve` et
/// la position servie reste celle d'avant la pause — à chaque tour.
#[tokio::test]
async fn apres_une_pause_la_reprise_sur_place_est_vue_par_le_sondeur_4666() {
    let mut banc = Banc::monter().await;
    banc.jouer_jusqu_a(150_000).await;
    banc.mettre_en_pause(150_000).await;

    banc.playback.resume(banc.zone_id).await;
    for (tour, position) in [151_000u64, 152_000, 153_000].into_iter().enumerate() {
        banc.renderer(TransportState::Playing, position).await;
        banc.tic().await;
        assert_eq!(
            banc.etat(),
            EtatDeLecture::Lecture,
            "tour {tour} après la reprise : la sortie joue à {position} ms et le \
             sondeur saute le tour (stale_start_position_ignored wall_s=0) — \
             la fin de piste ne sera jamais vue (#4666)"
        );
        assert_eq!(
            banc.servi().await,
            position as i64,
            "tour {tour} après la reprise : la position servie est figée alors \
             que la sortie avance (#4666)"
        );
    }
}

/// Reprise par RÉTABLISSEMENT de session (#4177 : le périphérique exclusif a
/// été rendu à la pause). `resume` relance la même piste à la position par
/// `play_without_history` : c'est la séquence que `resume` pose désormais
/// sur l'état de lecture — `seek`, `play`, `seek`.
///
/// Le témoin tient le CONTRAT entre les deux moitiés : ainsi encadrée, la
/// relecture est vue par le sondeur ; la garde de branchement plus bas tient
/// que `resume` pose bien cet encadrement.
#[tokio::test]
async fn apres_un_retablissement_de_session_la_piste_est_vue_par_le_sondeur_4666() {
    let mut banc = Banc::monter().await;
    banc.jouer_jusqu_a(139_000).await;
    banc.mettre_en_pause(139_666).await;

    banc.playback.seek(banc.zone_id, 139_666).await;
    banc.playback.play(banc.zone_id, Banc::piste()).await;
    banc.playback.seek(banc.zone_id, 139_666).await;

    banc.renderer(TransportState::Playing, 140_500).await;
    banc.tic().await;
    assert_eq!(
        banc.etat(),
        EtatDeLecture::Lecture,
        "la piste rétablie joue à 2:20 et le sondeur saute le tour : c'est la \
         zone figée du fil 1882 (#4666)"
    );
}

/// CONTRE-ÉPREUVE du témoin précédent, sur le banc lui-même : sans
/// l'encadrement, une relecture à la position reste invisible — la zone ne
/// sort pas de `Neuve`. C'est la preuve que l'encadrement porte la propriété,
/// et pas le banc.
#[tokio::test]
async fn un_retablissement_sans_encadrement_reste_invisible_au_sondeur_4666() {
    let mut banc = Banc::monter().await;
    banc.jouer_jusqu_a(139_000).await;
    banc.mettre_en_pause(139_666).await;

    banc.playback.play(banc.zone_id, Banc::piste()).await;
    banc.renderer(TransportState::Playing, 140_500).await;
    banc.tic().await;
    assert_eq!(banc.etat(), EtatDeLecture::Neuve);
    assert_eq!(
        banc.servi().await,
        0,
        "l'écran affiche 0:00 : la phrase du testeur"
    );
}

/// Un démarrage FRAIS garde sa protection : la position de la session
/// précédente rendue par un renderer (DMP-A6 : 374 s six secondes après un
/// Play) reste jetée. `play()` remet la position de zone à zéro, l'ancrage
/// neuf rend donc `None` comme avant.
#[tokio::test]
async fn un_demarrage_frais_jette_toujours_la_position_de_la_session_precedente() {
    let mut banc = Banc::monter().await;
    banc.playback.play(banc.zone_id, Banc::piste()).await;
    banc.renderer(TransportState::Playing, 374_000).await;
    banc.tic().await;
    assert_eq!(banc.etat(), EtatDeLecture::Neuve);
    assert_eq!(banc.servi().await, 0);
}

#[test]
fn l_ancrage_d_un_etat_neuf_date_le_debut_de_piste_a_la_position_4666() {
    let maintenant = Instant::now();
    assert_eq!(decisions::ancrage_d_un_etat_neuf(maintenant, 0), None);
    assert_eq!(decisions::ancrage_d_un_etat_neuf(maintenant, -5), None);
    let ancre = decisions::ancrage_d_un_etat_neuf(maintenant, 139_666).unwrap();
    assert_eq!(maintenant - ancre, Duration::from_millis(139_666));
    assert!(!decisions::stale_start_position(
        (maintenant - ancre).as_secs(),
        140_500
    ));
}

/// Garde de BRANCHEMENT : dans `resume`, le bras « rétablir à la position »
/// encadre `play_without_history` par deux `playback.seek`.
#[test]
fn resume_encadre_le_retablissement_par_deux_seek_4666() {
    let src = include_str!("../orchestrator/transport.rs");
    let prod = src.split("#[cfg(test)]").next().unwrap();
    let debut = prod
        .find("\"resume_stream_session_restore\"")
        .expect("le bras de rétablissement de resume");
    let fin = debut
        + prod[debut..]
            .find("dire_session_perdue")
            .expect("la fin du bras de rétablissement");
    let bras = &prod[debut..fin];
    let relecture = bras
        .find("self.play_without_history(req)")
        .expect("la relecture");
    assert!(
        bras[..relecture].contains("self.playback.seek(zone_id, position_ms as i64)"),
        "un seek doit précéder la relecture"
    );
    assert!(
        bras[relecture..].contains("self.playback.seek(zone_id, position_ms as i64)"),
        "un seek doit suivre la relecture réussie"
    );
}
