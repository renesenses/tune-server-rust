//! #7058 — le planificateur lance réellement la première piste sur un mock.
use super::*;
use crate::db::{
    migrations::run_migrations, models::Track, play_queue_repo::PlayQueueRepo,
    playlist_repo::PlaylistRepo, profile_repo::ProfileRepo, sqlite::SqliteDb,
    track_repo::TrackRepo, zone_repo::ZoneRepo,
};
use crate::http::streamer::AudioStreamer;
use crate::outputs::{OutputRegistry, mock::MockOutput};
use crate::playback::{PlayState, PlaybackManager};
use crate::streaming::ServiceRegistry;
use serde_json::json;

struct Banc {
    _dossier: tempfile::TempDir,
    db: Arc<dyn DbBackend>,
    orch: Arc<PlaybackOrchestrator>,
    zone: i64,
    profil: i64,
    playlist: i64,
    pistes: [i64; 2],
}
impl Banc {
    async fn nouveau() -> Self {
        let sql = SqliteDb::open_in_memory().unwrap();
        sql.init_schema().unwrap();
        run_migrations(&sql).unwrap();
        let db: Arc<dyn DbBackend> = Arc::new(sql);
        let profil = ProfileRepo::with_backend(db.clone())
            .create("reveil-7058", None, None)
            .unwrap();
        let zone = ZoneRepo::with_backend(db.clone())
            .create("Pièce", Some("dlna"), Some("mock-7058"))
            .unwrap();
        db.execute(
            "INSERT INTO albums (id, title) VALUES (1, 'Album du réveil')",
            &[],
        )
        .unwrap();
        let dossier = tempfile::tempdir().unwrap();
        let repo = TrackRepo::with_backend(db.clone());
        let mut pistes = [0; 2];
        for (i, numero) in [2i32, 1].into_iter().enumerate() {
            let chemin = dossier.path().join(format!("{numero}.wav"));
            std::fs::write(
                &chemin,
                include_bytes!("../../tests/fixtures/wav/ref_16_44100_stereo.wav"),
            )
            .unwrap();
            let mut piste = Track::new(format!("Piste {numero}"));
            piste.album_id = Some(1);
            piste.track_number = numero;
            piste.disc_number = 1;
            piste.file_path = Some(chemin.to_string_lossy().into_owned());
            piste.format = Some("wav".into());
            piste.duration_ms = 60_000;
            piste.sample_rate = Some(44_100);
            piste.bit_depth = Some(16);
            pistes[i] = repo.create(&piste).unwrap();
        }
        let listes = PlaylistRepo::with_backend(db.clone());
        let playlist = listes.create("Playlist du réveil", None, profil).unwrap();
        listes.add_tracks(playlist, &pistes, None).unwrap();
        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs
            .lock()
            .await
            .register(Box::new(MockOutput::new("mock-7058", "Sortie factice")));
        let orch = Arc::new(PlaybackOrchestrator::new(
            db.clone(),
            Arc::new(PlaybackManager::new()),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            outputs,
            None,
        ));
        Self {
            _dossier: dossier,
            db,
            orch,
            zone,
            profil,
            playlist,
            pistes,
        }
    }
    fn alarme(&self, source: &str, id: i64) -> serde_json::Value {
        json!({"id":7058,"name":"Réveil", "zone_id":self.zone,"profile_id":self.profil,
            "source_type":source,"source_id":id.to_string(),"volume":0.5,"fade_duration_s":0})
    }
}

#[tokio::test]
async fn un_reveil_album_joue_la_premiere_piste_et_garde_la_suite_7058() {
    let b = Banc::nouveau().await;
    let scheduler = AlarmScheduler::with_backend(b.db.clone(), b.orch.clone());
    scheduler.fire_alarm(&b.alarme("album", 1)).await;
    let etat = b.orch.playback.get_state(b.zone).await;
    assert_eq!(
        etat.state,
        PlayState::Playing,
        "#7058 : album doit être développé, pas résolu comme un service"
    );
    assert_eq!(etat.now_playing.unwrap().track_id, Some(b.pistes[1]));
    let file = PlayQueueRepo::with_backend(b.db).get_queue(b.zone).unwrap();
    assert_eq!(
        file.iter().map(|t| t.track_id).collect::<Vec<_>>(),
        [b.pistes[1], b.pistes[0]]
    );
}

#[tokio::test]
async fn un_reveil_playlist_respecte_son_ordre_et_la_zone_en_lecture_7058() {
    let b = Banc::nouveau().await;
    let scheduler = AlarmScheduler::with_backend(b.db.clone(), b.orch.clone());
    scheduler
        .fire_alarm(&b.alarme("playlist", b.playlist))
        .await;
    let etat = b.orch.playback.get_state(b.zone).await;
    assert_eq!(
        etat.state,
        PlayState::Playing,
        "#7058 : playlist doit être développée avant lecture"
    );
    assert_eq!(
        etat.now_playing.as_ref().unwrap().track_id,
        Some(b.pistes[0])
    );
    let generation = etat.track_generation;
    let autre = scheduler.fire_alarm(&b.alarme("album", 1)).await;
    assert_eq!(autre.sautees, [b.zone]);
    assert_eq!(
        b.orch.playback.get_state(b.zone).await.track_generation,
        generation
    );
    let file = PlayQueueRepo::with_backend(b.db).get_queue(b.zone).unwrap();
    assert_eq!(
        file.iter().map(|t| t.track_id).collect::<Vec<_>>(),
        b.pistes
    );
}

#[tokio::test]
async fn un_contenu_de_reveil_vide_ou_invalide_ne_remplace_pas_la_file_7058() {
    let b = Banc::nouveau().await;
    let file = PlayQueueRepo::with_backend(b.db.clone());
    file.set_queue(b.zone, &b.pistes).unwrap();
    let scheduler = AlarmScheduler::with_backend(b.db.clone(), b.orch.clone());
    for alarme in [b.alarme("album", 999), b.alarme("playlist", 999)] {
        assert!(scheduler.fire_alarm(&alarme).await.sonnees.is_empty());
        assert_eq!(file.get_queue(b.zone).unwrap().len(), 2);
    }
    assert!(
        scheduler
            .contenu_du_reveil("radio", "https://example.invalid/radio", None)
            .unwrap()
            .is_none()
    );
}
