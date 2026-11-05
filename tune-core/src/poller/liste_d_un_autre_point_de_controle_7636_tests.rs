//! #7636 — la liste qu'un autre point de contrôle (Kazoo) a chargée dans un
//! renderer OpenHome reste intacte.
//!
//! Banc : le VRAI sondeur (`tick`), un vrai orchestrateur, une file de
//! pistes réelles (copies de `tests/fixtures/test.flac`) et une sortie
//! factice de type `openhome`. Chaque `play_media` d'une sortie OpenHome
//! commence par `DeleteAll`, chaque `set_next_media` fait un `Insert` : compter
//! ces ordres reçus par la sortie, c'est compter les fois où la liste de
//! l'appareil aurait été vidée ou réécrite.
//!
//! Témoins :
//!  1. Kazoo joue un morceau de sa liste jusqu'au bout, puis le Linn s'arrête :
//!     sans la garde, le sondeur prend ce morceau pour la fin du sien et
//!     enchaîne sur la piste suivante de la file Tune (`play_media`, donc
//!     `DeleteAll`, plus un `Insert` de la suivante armée). Avec la garde,
//!     aucun ordre ne part ;
//!  2. Kazoo prend la main : après trois relevés, la zone Tune lâche
//!     l'appareil, sans lui envoyer ni `Stop` ni lecture, et ne le reprend
//!     pas ensuite, même quand Kazoo passe d'une piste à l'autre ;
//!  3. juste après un « Lire » fait dans Tune, le Linn peut rapporter encore
//!     quelques secondes l'URI de Kazoo : la zone ne lâche pas sa propre
//!     lecture ;
//!  4. notre propre flux, ou la suivante armée d'un enchaînement (un flux
//!     Tune d'un autre identifiant), ne fait rien lâcher ;
//!  5. la même URI étrangère sur une zone DLNA ne change pas de comportement.
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
use tokio::sync::Mutex;

const TITRE: &str = "Spiegel im Spiegel";
const DUREE_MS: i64 = 605_000;
const NOTRE_FLUX: &str = "sid-7636";
const URI_DE_TUNE: &str = "http://192.168.1.20:8888/stream/sid-7636.flac";
/// La suivante armée d'un enchaînement : un flux Tune, d'un autre identifiant.
const URI_DE_LA_SUIVANTE: &str = "http://192.168.1.20:8888/stream/sid-suivante.flac";
/// Ce que le Linn rapporte quand Kazoo joue sa liste, tirée du serveur média
/// de Tune.
const URI_DE_KAZOO: &str = "http://192.168.1.20:8888/api/v1/library/tracks/42/audio";

#[derive(Debug, PartialEq, Eq)]
struct Ordres {
    lectures_donc_delete_all: usize,
    suivantes_inserees: usize,
    stops: u64,
}

const AUCUN_ORDRE: Ordres = Ordres {
    lectures_donc_delete_all: 0,
    suivantes_inserees: 0,
    stops: 0,
};

struct Banc {
    poller: PositionPoller,
    playback: Arc<PlaybackManager>,
    outputs: Arc<Mutex<OutputRegistry>>,
    db: Arc<dyn crate::db::backend::DbBackend>,
    appareil: String,
    zone_id: i64,
    duree: i64,
    _dossier: tempfile::TempDir,
    poll_states: HashMap<i64, ZonePollState>,
    idle: HashMap<i64, IdlePollBackoff>,
}

impl Banc {
    async fn monter(type_sortie: &str, appareil: &str) -> Self {
        Self::monter_avec(type_sortie, appareil, 1, DUREE_MS).await
    }

    /// Une file de `nb` pistes de `duree` ms, la première en lecture sur
    /// notre flux.
    async fn monter_avec(type_sortie: &str, appareil: &str, nb: usize, duree: i64) -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("Klimax", Some(type_sortie), Some(appareil))
            .unwrap();
        let mut t = crate::db::models::Track::new(TITRE.to_string());
        t.file_path = Some("/Musique/spiegel.flac".into());
        t.format = Some("flac".into());
        t.duration_ms = duree;
        let dossier = tempfile::tempdir().unwrap();
        let gabarit =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test.flac");
        let mut pistes = Vec::new();
        for i in 0..nb {
            let chemin = dossier.path().join(format!("piste-{i}.flac"));
            std::fs::copy(&gabarit, &chemin).unwrap();
            let mut ti = t.clone();
            ti.file_path = Some(chemin.to_string_lossy().into_owned());
            pistes.push(TrackRepo::with_backend(db.clone()).create(&ti).unwrap());
        }
        let piste = pistes[0];
        PlayQueueRepo::with_backend(db.clone())
            .set_queue(zone_id, &pistes)
            .unwrap();
        ZoneRepo::with_backend(db.clone())
            .save_play_state(zone_id, "playing")
            .unwrap();

        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(
            MockOutput::new(appareil, "Klimax DSM").with_type(type_sortie),
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
        )
        .with_event_bus(Arc::new(EventBus::new()));

        playback
            .play(
                zone_id,
                NowPlaying {
                    track_id: Some(piste),
                    title: TITRE.into(),
                    source: "local".into(),
                    duration_ms: duree,
                    stream_id: Some(NOTRE_FLUX.into()),
                    ..Default::default()
                },
            )
            .await;
        playback.update_queue_info(zone_id, 0, nb as i64).await;

        Self {
            poller,
            playback,
            outputs,
            db,
            appareil: appareil.to_string(),
            zone_id,
            duree,
            _dossier: dossier,
            poll_states: HashMap::new(),
            idle: HashMap::new(),
        }
    }

    /// Ce que la sortie rapporte au prochain sondage.
    async fn sortie(&self, etat: TransportState, position_ms: u64, uri: &str) {
        let reg = self.outputs.lock().await;
        let arc = reg.get(&self.appareil).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.set_state(etat).await;
        mock.set_duration(self.duree as u64);
        mock.set_position(position_ms);
        mock.set_current_uri(Some(uri.to_string())).await;
    }

    /// Ordres reçus par la sortie.
    async fn ordres_recus(&self) -> Ordres {
        let reg = self.outputs.lock().await;
        let arc = reg.get(&self.appareil).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        Ordres {
            lectures_donc_delete_all: mock.play_call_count().await,
            suivantes_inserees: mock.set_next_call_count().await,
            stops: mock.stop_call_count(),
        }
    }

    /// Un relevé du sondeur, puis une seconde qui passe pour ses horloges.
    async fn tic(&mut self) {
        // Le futur de `tick` est gros en debug : sur le tas, il ne fait pas
        // déborder la pile du test quand une lecture part vraiment.
        Box::pin(
            self.poller
                .tick(&mut self.poll_states, &mut self.idle, &Instant::now()),
        )
        .await;
        self.vieillir(1);
    }

    /// Fait passer `secs` secondes pour les horloges du sondeur : les tours
    /// du banc s'enchaînent sans attendre, ses seuils en secondes non.
    fn vieillir(&mut self, secs: u64) {
        let d = std::time::Duration::from_secs(secs);
        let recule = |t: Instant| t.checked_sub(d).unwrap_or(t);
        if let Some(ps) = self.poll_states.get_mut(&self.zone_id) {
            ps.track_loaded_at = recule(ps.track_loaded_at);
            ps.track_started_at = ps.track_started_at.map(recule);
            ps.premier_arret_a = ps.premier_arret_a.map(recule);
            ps.playing_stall_depuis = ps.playing_stall_depuis.map(recule);
            ps.fige_a_zero_depuis = ps.fige_a_zero_depuis.map(recule);
            ps.gapless_sent_at = ps.gapless_sent_at.map(recule);
            ps.famine_releve_at = ps.famine_releve_at.map(recule);
            ps.last_seek_seen = ps.last_seek_seen.map(recule);
        }
    }

    async fn etat(&self) -> PlayState {
        self.playback.get_state(self.zone_id).await.state
    }
}

/// Durée de la piste Tune du témoin 1 : 3 min 20.
const DUREE_COURTE_MS: i64 = 200_000;

#[tokio::test]
async fn kazoo_joue_sa_liste_tune_n_y_envoie_ni_lecture_ni_insertion_7636() {
    // Tune joue la première piste d'un album de deux, depuis une minute.
    let mut banc =
        Banc::monter_avec("openhome", "uuid:klimax-7636-liste", 2, DUREE_COURTE_MS).await;
    banc.sortie(TransportState::Playing, 5_000, URI_DE_TUNE)
        .await;
    banc.tic().await;
    banc.vieillir(60);

    // Kazoo charge sa liste et en joue un morceau jusqu'au bout (près de
    // trois minutes), puis le Linn s'arrête : fin de la liste de Kazoo, ou
    // « Stop » dans Kazoo.
    for s in 1..=175u64 {
        banc.sortie(TransportState::Playing, s * 1_000, URI_DE_KAZOO)
            .await;
        banc.tic().await;
    }
    for _ in 0..10 {
        banc.sortie(TransportState::Stopped, 0, URI_DE_KAZOO).await;
        banc.tic().await;
    }

    // Les ordres d'abord : c'est la liste de Kazoo qu'ils effacent.
    assert_eq!(
        banc.ordres_recus().await,
        AUCUN_ORDRE,
        "Tune a pris le morceau de Kazoo pour la fin du sien et a envoyé au \
         Linn sa piste suivante : chaque lecture OpenHome commence par \
         DeleteAll et vide la liste que Kazoo a chargée, chaque suivante armée \
         y insère une piste de Tune"
    );
    assert_ne!(
        banc.etat().await,
        PlayState::Playing,
        "la zone a lâché l'appareil au lieu de suivre la lecture de Kazoo"
    );
}

#[tokio::test]
async fn juste_apres_un_lire_dans_tune_l_ancienne_uri_ne_fait_rien_lacher_7636() {
    // On vient de faire « Lire » dans Tune : le Linn rapporte encore, pendant
    // quelques secondes, l'URI du morceau que Kazoo jouait (événement Info en
    // retard).
    let mut banc = Banc::monter("openhome", "uuid:klimax-7636-lire").await;
    for s in 0..5u64 {
        banc.sortie(TransportState::Playing, s * 1_000, URI_DE_KAZOO)
            .await;
        banc.tic().await;
    }
    assert_eq!(
        banc.etat().await,
        PlayState::Playing,
        "une lecture lancée depuis Tune il y a moins de {} s n'est pas lâchée \
         sur l'URI que le Linn n'a pas encore remplacée",
        decisions::DELAI_AVANT_DE_JUGER_LA_TENUE_SECS
    );
    assert_ne!(
        ZoneRepo::with_backend(banc.db.clone())
            .get_last_play_state(banc.zone_id)
            .as_deref(),
        Some("stopped"),
        "la zone n'a pas été lâchée"
    );

    // Le Linn rapporte enfin notre flux : la lecture continue.
    for s in 5..=8u64 {
        banc.sortie(TransportState::Playing, s * 1_000, URI_DE_TUNE)
            .await;
        banc.tic().await;
    }
    assert_eq!(banc.etat().await, PlayState::Playing);
    assert_eq!(banc.ordres_recus().await.stops, 0, "aucun Stop envoyé");
}

#[tokio::test]
async fn la_liste_chargee_par_kazoo_reste_intacte_7636() {
    let mut banc = Banc::monter("openhome", "uuid:klimax-7636").await;

    // Prémisse : Tune joue son flux depuis une minute, rien ne bouge.
    banc.sortie(TransportState::Playing, 5_000, URI_DE_TUNE)
        .await;
    banc.tic().await;
    banc.vieillir(60);
    assert_eq!(banc.etat().await, PlayState::Playing, "prémisse");

    // Kazoo charge sa liste et la joue.
    for s in 1..=3u64 {
        banc.sortie(TransportState::Playing, s * 1_000, URI_DE_KAZOO)
            .await;
        banc.tic().await;
    }
    assert_ne!(
        banc.etat().await,
        PlayState::Playing,
        "un autre point de contrôle tient le renderer OpenHome depuis trois \
         relevés : la zone Tune doit le lâcher. Restée « en lecture », elle \
         prend le morceau de Kazoo pour le sien et, à sa fin, envoie sa piste \
         suivante au Linn (voir le témoin \
         kazoo_joue_sa_liste_tune_n_y_envoie_ni_lecture_ni_insertion_7636)"
    );
    assert_eq!(
        ZoneRepo::with_backend(banc.db.clone())
            .get_last_play_state(banc.zone_id)
            .as_deref(),
        Some("stopped"),
        "l'état enregistré doit être « stopped » : sinon le sondeur « reprend » \
         la lecture de Kazoo comme celle de Tune"
    );

    // Kazoo passe d'une piste à l'autre : un arrêt bref, puis il rejoue.
    for _ in 0..6 {
        banc.sortie(TransportState::Stopped, 0, URI_DE_KAZOO).await;
        banc.tic().await;
    }
    for s in 1..=6u64 {
        banc.sortie(TransportState::Playing, s * 1_000, URI_DE_KAZOO)
            .await;
        banc.tic().await;
    }

    assert_eq!(
        banc.ordres_recus().await,
        AUCUN_ORDRE,
        "aucune lecture (donc aucun DeleteAll), aucune insertion ni aucun Stop \
         ne doit partir vers un renderer qu'un autre point de contrôle tient"
    );
    assert_ne!(
        banc.etat().await,
        PlayState::Playing,
        "la zone ne reprend pas la lecture de Kazoo à son compte"
    );
}

#[tokio::test]
async fn notre_flux_et_la_suivante_armee_ne_font_rien_lacher_7636() {
    let mut banc = Banc::monter("openhome", "uuid:klimax-7636-b").await;
    banc.sortie(TransportState::Playing, 0, URI_DE_TUNE).await;
    banc.tic().await;
    banc.vieillir(60);
    for s in 1..=4u64 {
        banc.sortie(TransportState::Playing, s * 1_000, URI_DE_TUNE)
            .await;
        banc.tic().await;
    }
    for s in 5..=9u64 {
        banc.sortie(TransportState::Playing, s * 1_000, URI_DE_LA_SUIVANTE)
            .await;
        banc.tic().await;
    }
    assert_eq!(
        banc.ordres_recus().await.stops,
        0,
        "aucun Stop envoyé à la sortie"
    );
    assert!(
        banc.playback
            .get_state(banc.zone_id)
            .await
            .now_playing
            .is_some(),
        "un flux Tune, même d'un autre identifiant, n'est pas un autre point de contrôle"
    );
    assert_ne!(
        ZoneRepo::with_backend(banc.db.clone())
            .get_last_play_state(banc.zone_id)
            .as_deref(),
        Some("stopped"),
        "la zone n'a pas été lâchée"
    );
}

#[tokio::test]
async fn une_zone_dlna_garde_son_comportement_7636() {
    let mut banc = Banc::monter("dlna", "uuid:renderer-dlna-7636").await;
    banc.sortie(TransportState::Playing, 0, URI_DE_TUNE).await;
    banc.tic().await;
    banc.vieillir(60);
    for s in 1..=4u64 {
        banc.sortie(TransportState::Playing, s * 1_000, URI_DE_KAZOO)
            .await;
        banc.tic().await;
    }
    assert_eq!(
        banc.etat().await,
        PlayState::Playing,
        "la garde est propre à OpenHome, seule sortie dont Tune réécrit la liste"
    );
}
