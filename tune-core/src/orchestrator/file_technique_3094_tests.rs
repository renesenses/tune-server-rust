//! Fil 3094 — Lecture sur une file qui contient encore un balayage de
//! calibration : il est écarté une fois, jamais rejoué, et ne compte nulle
//! part comme une écoute.

use std::sync::Arc;

use tokio::sync::Mutex;

use super::BALAYAGE_PERIME;
use crate::db::backend::DbBackend;
use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::{ContexteEcoute, PlayRequest, PlaybackOrchestrator};
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::source_pcm::{FluxPcm, FournisseurPcm, SOURCE_CALIBRATION};
use crate::streaming::registry::ServiceRegistry;

fn banc() -> (PlaybackOrchestrator, PlayQueueRepo, i64) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let zone = ZoneRepo::with_backend(db.clone())
        .create("Zone 7", Some("local"), None)
        .unwrap();
    let orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        Some("127.0.0.1".into()),
    );
    (orch, PlayQueueRepo::with_backend(db), zone)
}

fn ligne(source: &str, id: &str) -> QueueInput {
    QueueInput::Streaming {
        media_format: None,
        sample_rate: None,
        bit_depth: None,
        source: source.into(),
        source_id: id.into(),
        title: format!("titre {id}"),
        artist: String::new(),
        album: None,
        cover_url: None,
        duration_ms: 20_000,
        track_number: None,
        disc_number: None,
        album_ref: None,
        artist_ref: None,
    }
}

fn ids(repo: &PlayQueueRepo, zone: i64) -> Vec<String> {
    repo.get_ordered(zone)
        .unwrap()
        .into_iter()
        .map(|e| e.source_id.unwrap_or_default())
        .collect()
}

/// Le fournisseur du greffon, réduit à ce qui compte ici : seule la mesure
/// `m-en-cours` est armée.
struct UneMesureEnCours;
impl FournisseurPcm for UneMesureEnCours {
    fn ouvrir(&self, source_id: &str, _: u64) -> Result<FluxPcm, String> {
        Err(format!(
            "aucune mesure de calibration armée sous « {source_id} »"
        ))
    }
    fn encore_jouable(&self, source_id: &str) -> bool {
        source_id == "m-en-cours"
    }
}

#[tokio::test]
async fn lecture_sur_une_file_qui_ne_contient_que_le_balayage_ne_le_rejoue_pas_3094() {
    let (orch, repo, zone) = banc();
    repo.append(zone, &[ligne(SOURCE_CALIBRATION, "m3-1764000000")])
        .unwrap();
    repo.set_current_pos(zone, 0).unwrap();

    for appui in 0..2 {
        let e = orch
            .play_from_queue(zone, 0)
            .await
            .expect_err("rien à lire");
        assert!(
            e.starts_with(PlaybackOrchestrator::QUEUE_POSITION_OUT_OF_RANGE),
            "appui {appui} : file vide, pas l'erreur du fournisseur : {e}"
        );
        assert!(!e.contains("calibration"), "{e}");
        assert!(ids(&repo, zone).is_empty());
    }
}

#[tokio::test]
async fn lecture_saute_le_balayage_et_passe_a_la_ligne_suivante_3094() {
    let (orch, repo, zone) = banc();
    repo.append(
        zone,
        &[
            ligne(SOURCE_CALIBRATION, "m3-1764000000"),
            ligne("qobuz", "b"),
        ],
    )
    .unwrap();
    repo.set_current_pos(zone, 0).unwrap();

    // « b » ne se résout pas sur ce banc (aucun service) : ce qui compte est
    // que la lecture vise « b », et que le balayage ait quitté la file.
    let r = orch.play_from_queue(zone, 0).await;
    if let Err(e) = &r {
        assert!(!e.contains("calibration"), "{e}");
    }
    assert_eq!(ids(&repo, zone), ["b"]);
    let courant = repo
        .get_ordered(zone)
        .unwrap()
        .into_iter()
        .find(|e| e.is_current)
        .and_then(|e| e.source_id);
    assert_eq!(courant.as_deref(), Some("b"));
}

#[tokio::test]
async fn le_balayage_d_une_mesure_en_cours_reste_jouable_3094() {
    let (orch, repo, zone) = banc();
    orch.sources_pcm()
        .inscrire(SOURCE_CALIBRATION, Arc::new(UneMesureEnCours));
    repo.append(
        zone,
        &[
            ligne(SOURCE_CALIBRATION, "m-finie"),
            ligne(SOURCE_CALIBRATION, "m-en-cours"),
            ligne("qobuz", "b"),
        ],
    )
    .unwrap();
    let r = orch.ecarter_les_balayages_perimes(zone).await;
    assert_eq!(r.retirees, [0]);
    assert_eq!(ids(&repo, zone), ["m-en-cours", "b"]);
}

#[tokio::test]
async fn jouer_directement_un_balayage_perime_est_refuse_en_une_phrase_3094() {
    let (orch, repo, zone) = banc();
    // Le client renvoie la piste affichée : la route l'a déjà mise en file.
    repo.append(zone, &[ligne(SOURCE_CALIBRATION, "m3-1764000000")])
        .unwrap();
    let e = orch
        .play(PlayRequest {
            zone_id: zone,
            source: Some(SOURCE_CALIBRATION.into()),
            source_id: Some("m3-1764000000".into()),
            title: Some("Calibration — balayage de mesure m3-1764000000".into()),
            ..Default::default()
        })
        .await
        .expect_err("refus");
    assert_eq!(e, BALAYAGE_PERIME);
    assert!(ids(&repo, zone).is_empty(), "la file en est purgée");
}

#[tokio::test]
async fn une_zone_arretee_oublie_le_balayage_affiche_3094() {
    let (orch, _repo, zone) = banc();
    orch.playback
        .play(
            zone,
            NowPlaying {
                title: "Calibration — balayage de mesure m3-1764000000".into(),
                source: SOURCE_CALIBRATION.into(),
                source_id: Some("m3-1764000000".into()),
                duration_ms: 20_000,
                ..Default::default()
            },
        )
        .await;
    orch.playback.stop(zone).await;
    orch.ecarter_les_balayages_perimes(zone).await;
    assert!(
        orch.playback.get_state(zone).await.now_playing.is_none(),
        "le client ne doit plus avoir de balayage à renvoyer"
    );
}

#[tokio::test]
async fn un_balayage_n_entre_ni_dans_l_historique_ni_dans_les_scrobbles_3094() {
    let (orch, _repo, zone) = banc();
    orch.record_listen(
        "Calibration — balayage de mesure m3-1764000000",
        None,
        None,
        SOURCE_CALIBRATION,
        Some("m3-1764000000"),
        None,
        20_000,
        zone,
        None,
        None,
        ContexteEcoute::default(),
        None,
        None,
    );
    assert!(
        !orch.ecrire_l_ecoute_en_attente(zone),
        "aucune écoute armée pour un balayage"
    );
    let lignes = crate::db::history_repo::HistoryRepo::with_backend(orch.db.clone())
        .recent(10)
        .unwrap();
    assert!(lignes.is_empty());
    assert!(!crate::poller::decisions::should_dispatch_scrobble(
        None,
        "cle",
        SOURCE_CALIBRATION,
        20_000,
        19_000
    ));
}
