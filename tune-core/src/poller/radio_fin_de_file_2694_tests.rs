//! Forum, fil 2694 (Ilan, 12/11/2026) ; décision de Bertrand du 13/11 — la
//! radio de fin de file, « comme Roon » : activée par défaut, désactivable,
//! jamais sur une fin voulue (« Vider la suite », réveil), jamais sous
//! contrôle parental, sans doublon, et les titres ajoutés sont marqués.
//!
//! Tout passe par la vraie fin de piste (`handle_track_end`). Sortie
//! simulée (`MockOutput`), services bouchonnés, aucun appel réseau : sans
//! adresse posée, l'API d'enrichissement ne répond rien sous test.

use super::*;
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::{AutoplayMode, ZoneRepo};
use crate::error::TuneError;
use crate::outputs::mock::MockOutput;
use crate::playback::radio_fin_de_file as rff;
use crate::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

const SORTIE: &str = "mock-radio-2694";

struct Banc {
    db: Arc<dyn DbBackend>,
    poller: PositionPoller,
    playback: Arc<crate::playback::PlaybackManager>,
    zone_id: i64,
    events: tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
    _tmp: tempfile::TempDir,
}

/// Bibliothèque de quatre pistes de jazz (1 à 4), file [1, 3], la piste 3 en
/// cours et dernière de la file. La zone naît par `ZoneRepo::create` : son
/// réglage est donc celui de toute nouvelle zone.
async fn banc(services: crate::streaming::ServiceRegistry) -> Banc {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let cree = ZoneRepo::with_backend(db.clone())
        .create("Radio 2694", Some("mock"), Some(SORTIE))
        .unwrap();
    // Les consignes de la radio (fin voulue, fin sur erreur, dernier lot)
    // vivent en mémoire, par zone : un identifiant propre à chaque épreuve
    // les empêche de se croiser entre épreuves parallèles.
    static PROCHAINE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(26_940_000);
    let zone_id = PROCHAINE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    db.execute(
        "UPDATE zones SET id = ? WHERE id = ?",
        &[&zone_id as &dyn ToSqlValue, &cree],
    )
    .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    db.execute_batch(
        "INSERT INTO artists (id, name) VALUES (1, 'Artiste');
         INSERT INTO albums (id, title, artist_id, year) VALUES (1, 'Album', 1, 2001);",
    )
    .unwrap();
    for id in 1..=4i64 {
        let path = tmp.path().join(format!("{id}.wav"));
        let mut wav =
            crate::audio::wav::build_wav_header_with_duration(2, 44100, 16, Some(1000)).to_vec();
        wav.resize(wav.len() + 44100 * 4, 0);
        std::fs::write(&path, wav).unwrap();
        db.execute(
            "INSERT INTO tracks (id, title, artist_id, album_id, file_path, format, sample_rate, \
             bit_depth, duration_ms, genre, year) \
             VALUES (?, ?, 1, 1, ?, 'wav', 44100, 16, 1000, 'Jazz', 2001)",
            &[
                &id as &dyn ToSqlValue,
                &format!("Piste {id}"),
                &path.to_str().unwrap(),
            ],
        )
        .unwrap();
    }
    PlayQueueRepo::with_backend(db.clone())
        .append_tracks(zone_id, &[1, 3])
        .unwrap();
    let playback = Arc::new(crate::playback::PlaybackManager::new());
    let outputs = Arc::new(Mutex::new(crate::outputs::registry::OutputRegistry::new()));
    outputs
        .lock()
        .await
        .register(Box::new(MockOutput::new(SORTIE, "Radio 2694")));
    let orchestrator = Arc::new(crate::orchestrator::PlaybackOrchestrator::new(
        db.clone(),
        playback.clone(),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(Mutex::new(services)),
        outputs.clone(),
        None,
    ));
    let bus = Arc::new(crate::event_bus::EventBus::new());
    let events = bus.subscribe();
    let poller = PositionPoller::new(
        orchestrator,
        playback.clone(),
        outputs,
        db.clone(),
        Arc::new(Mutex::new(HashMap::new())),
    )
    .with_event_bus(bus);
    Banc {
        db,
        poller,
        playback,
        zone_id,
        events,
        _tmp: tmp,
    }
}

/// La piste 3 s'achève, dernière de la file [1, 3].
fn fin_de_la_piste_3(zone_id: i64) -> crate::playback::ZoneState {
    crate::playback::ZoneState {
        zone_id,
        state: crate::playback::PlayState::Playing,
        queue_length: 2,
        queue_position: 1,
        now_playing: Some(crate::playback::NowPlaying {
            track_id: Some(3),
            title: "Piste 3".into(),
            artist_name: Some("Artiste".into()),
            source: "local".into(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn pistes_de_la_file(b: &Banc) -> Vec<Option<i64>> {
    PlayQueueRepo::with_backend(b.db.clone())
        .get_ordered(b.zone_id)
        .unwrap()
        .into_iter()
        .map(|r| r.track_id)
        .collect()
}

async fn rien_n_est_ajoute(b: &Banc) {
    assert_eq!(pistes_de_la_file(b), vec![Some(1), Some(3)]);
    assert_eq!(
        b.playback.get_state(b.zone_id).await.state,
        crate::playback::PlayState::Stopped
    );
    assert!(rff::lignes_ajoutees_par_la_radio(&b.db, b.zone_id).is_empty());
}

#[tokio::test]
async fn une_nouvelle_zone_nait_avec_la_radio_de_fin_de_file() {
    let b = banc(crate::streaming::ServiceRegistry::new()).await;
    assert_eq!(
        ZoneRepo::with_backend(b.db.clone()).get_autoplay_mode(b.zone_id),
        AutoplayMode::Similar
    );
}

#[tokio::test]
async fn fin_de_file_ajoute_des_titres_similaires_marques_sans_doublon_et_continue() {
    let mut b = banc(crate::streaming::ServiceRegistry::new()).await;
    let avant = rff::lignes_de_la_file(&b.db, b.zone_id);
    b.poller
        .handle_track_end(b.zone_id, &fin_de_la_piste_3(b.zone_id))
        .await;

    let file = pistes_de_la_file(&b);
    assert!(file.len() > 2, "la radio a ajouté des titres : {file:?}");
    let ajoutees: Vec<i64> = file[2..].iter().map(|t| t.unwrap()).collect();
    for id in &ajoutees {
        assert!(
            [2, 4].contains(id),
            "ni la piste qui s'achève ni une piste déjà dans la file : {file:?}"
        );
    }
    let mut uniques = ajoutees.clone();
    uniques.sort();
    uniques.dedup();
    assert_eq!(uniques.len(), ajoutees.len(), "aucun doublon : {file:?}");

    let etat = b.playback.get_state(b.zone_id).await;
    assert_eq!(etat.state, crate::playback::PlayState::Playing);
    assert_eq!(
        etat.queue_position, 2,
        "la lecture continue sur le lot ajouté"
    );

    // Les lignes ajoutées, et elles seules, portent la marque de la radio.
    let marques = rff::lignes_ajoutees_par_la_radio(&b.db, b.zone_id);
    let lignes = rff::lignes_de_la_file(&b.db, b.zone_id);
    for (i, ligne) in lignes.iter().enumerate() {
        assert_eq!(
            marques.contains(ligne),
            !avant.contains(ligne),
            "ligne {i} : marque attendue seulement sur les ajouts"
        );
    }
    let mut annonce = false;
    while let Ok(e) = b.events.try_recv() {
        if e.event_type == "playback.queue_changed" && e.data["added_by_radio"].as_u64().is_some() {
            annonce = true;
        }
    }
    assert!(annonce, "la file est redite pour que le badge paraisse");
}

#[tokio::test]
async fn reglage_desactive_arrete_comme_avant() {
    let b = banc(crate::streaming::ServiceRegistry::new()).await;
    ZoneRepo::with_backend(b.db.clone())
        .update_autoplay_mode(b.zone_id, AutoplayMode::Off)
        .unwrap();
    b.poller
        .handle_track_end(b.zone_id, &fin_de_la_piste_3(b.zone_id))
        .await;
    rien_n_est_ajoute(&b).await;
}

#[tokio::test]
async fn repetition_de_la_file_ne_declenche_pas_la_radio() {
    let b = banc(crate::streaming::ServiceRegistry::new()).await;
    let mut etat = fin_de_la_piste_3(b.zone_id);
    etat.repeat = RepeatMode::All;
    b.poller.handle_track_end(b.zone_id, &etat).await;
    assert_eq!(pistes_de_la_file(&b), vec![Some(1), Some(3)]);
    assert_eq!(b.playback.get_state(b.zone_id).await.queue_position, 0);
    assert!(rff::lignes_ajoutees_par_la_radio(&b.db, b.zone_id).is_empty());
}

#[tokio::test]
async fn fin_voulue_vider_la_suite_ou_reveil_ne_declenche_pas_la_radio() {
    let b = banc(crate::streaming::ServiceRegistry::new()).await;
    rff::noter_fin_voulue(&b.db, b.zone_id);
    b.poller
        .handle_track_end(b.zone_id, &fin_de_la_piste_3(b.zone_id))
        .await;
    rien_n_est_ajoute(&b).await;
}

#[tokio::test]
async fn fin_voulue_perimee_si_la_file_a_change() {
    let b = banc(crate::streaming::ServiceRegistry::new()).await;
    rff::noter_fin_voulue(&b.db, b.zone_id);
    // L'auditeur rajoute un titre après coup : la consigne ne vaut plus.
    PlayQueueRepo::with_backend(b.db.clone())
        .append_tracks(b.zone_id, &[2])
        .unwrap();
    assert!(!rff::fin_voulue(&b.db, b.zone_id));
}

#[tokio::test]
async fn zone_suiveuse_d_un_groupe_ne_lance_pas_la_radio() {
    let b = banc(crate::streaming::ServiceRegistry::new()).await;
    crate::db::settings_repo::SettingsRepo::with_backend(b.db.clone())
        .set(
            "zone_groups",
            &serde_json::json!([{ "leader_id": 9999, "zone_ids": [9999, b.zone_id] }]).to_string(),
        )
        .unwrap();
    b.poller
        .handle_track_end(b.zone_id, &fin_de_la_piste_3(b.zone_id))
        .await;
    rien_n_est_ajoute(&b).await;
}

#[tokio::test]
async fn la_meneuse_d_un_groupe_lance_la_radio() {
    let b = banc(crate::streaming::ServiceRegistry::new()).await;
    crate::db::settings_repo::SettingsRepo::with_backend(b.db.clone())
        .set(
            "zone_groups",
            &serde_json::json!([{ "leader_id": b.zone_id, "zone_ids": [b.zone_id, 9999] }])
                .to_string(),
        )
        .unwrap();
    b.poller
        .handle_track_end(b.zone_id, &fin_de_la_piste_3(b.zone_id))
        .await;
    assert!(pistes_de_la_file(&b).len() > 2);
}

#[tokio::test]
async fn fin_sur_erreur_ne_lance_pas_la_radio() {
    let b = banc(crate::streaming::ServiceRegistry::new()).await;
    rff::noter_fin_sur_erreur(b.zone_id);
    b.poller
        .handle_track_end(b.zone_id, &fin_de_la_piste_3(b.zone_id))
        .await;
    rien_n_est_ajoute(&b).await;
    // La consigne est consommée : la fin naturelle suivante relance.
    assert!(!rff::prendre_fin_sur_erreur(b.zone_id));
}

#[tokio::test]
async fn un_lot_qui_n_a_joue_aucun_titre_en_entier_n_en_appelle_pas_un_autre() {
    let b = banc(crate::streaming::ServiceRegistry::new()).await;
    // La ligne en cours (piste 3) est celle d'un lot dont rien n'a joué.
    let lignes = rff::lignes_de_la_file(&b.db, b.zone_id);
    rff::noter_lot(b.zone_id, vec![lignes[1]]);
    b.poller
        .terminer_la_file(b.zone_id, &fin_de_la_piste_3(b.zone_id), Some(SORTIE))
        .await;
    rien_n_est_ajoute(&b).await;
}

#[tokio::test]
async fn un_titre_ajoute_par_l_auditeur_pendant_l_appel_n_est_pas_marque() {
    let b = banc(crate::streaming::ServiceRegistry::new()).await;
    let avant = rff::lignes_de_la_file(&b.db, b.zone_id);
    let file = PlayQueueRepo::with_backend(b.db.clone());
    file.append_tracks(b.zone_id, &[2]).unwrap(); // l'auditeur
    file.append_tracks(b.zone_id, &[4]).unwrap(); // la radio, 1 titre
    let lot = rff::marquer_les_ajouts(&b.db, b.zone_id, &avant, 1);
    let lignes = rff::lignes_de_la_file(&b.db, b.zone_id);
    assert_eq!(lot, vec![lignes[3]]);
    assert!(!rff::lignes_ajoutees_par_la_radio(&b.db, b.zone_id).contains(&lignes[2]));
}

#[tokio::test]
async fn les_lignes_de_radio_deja_jouees_sont_purgees_au_dela_du_seuil() {
    let b = banc(crate::streaming::ServiceRegistry::new()).await;
    let file = PlayQueueRepo::with_backend(b.db.clone());
    file.clear(b.zone_id).unwrap();
    let anciennes: Vec<QueueInput> = (0..rff::GARDER_JOUEES + 5)
        .map(|i| QueueInput::Streaming {
            source: "tidal".into(),
            source_id: format!("x{i}"),
            title: format!("Ancienne {i}"),
            artist: "Voisin".into(),
            album: None,
            cover_url: None,
            duration_ms: 180_000,
            track_number: None,
            disc_number: None,
            album_ref: None,
            artist_ref: None,
        })
        .collect();
    file.append(b.zone_id, &anciennes).unwrap();
    let marques = rff::lignes_de_la_file(&b.db, b.zone_id);
    crate::db::settings_repo::SettingsRepo::with_backend(b.db.clone())
        .set(
            &rff::cle(b.zone_id),
            &serde_json::to_string(&marques).unwrap(),
        )
        .unwrap();
    file.append_tracks(b.zone_id, &[3]).unwrap();
    let n = anciennes.len() as i64;
    let mut etat = fin_de_la_piste_3(b.zone_id);
    etat.queue_position = n;
    etat.queue_length = n + 1;
    b.poller.handle_track_end(b.zone_id, &etat).await;

    let restantes = file.get_ordered(b.zone_id).unwrap();
    assert_eq!(
        restantes[0].source_id.as_deref(),
        Some("x5"),
        "les 5 plus anciennes sont parties"
    );
    assert_eq!(
        restantes
            .iter()
            .filter(|r| r.source.as_deref() == Some("tidal"))
            .count(),
        rff::GARDER_JOUEES
    );
    let etat = b.playback.get_state(b.zone_id).await;
    assert_eq!(etat.state, crate::playback::PlayState::Playing);
    assert_eq!(
        etat.queue_position,
        rff::GARDER_JOUEES as i64 + 1,
        "la lecture part sur le premier titre du nouveau lot"
    );
}

#[tokio::test]
async fn controle_parental_aucun_titre_ajoute() {
    use crate::db::politique_parentale::{ArtisteBloque, PolitiqueParentale, poser};
    let b = banc(crate::streaming::ServiceRegistry::new()).await;
    // Profil des sélections automatiques d'une base vierge : le profil 1.
    poser(
        &b.db,
        1,
        1,
        0,
        &PolitiqueParentale {
            artistes_bloques: vec![ArtisteBloque {
                id: None,
                nom: Some("Artiste".into()),
            }],
            ..Default::default()
        },
    )
    .unwrap();
    b.poller
        .handle_track_end(b.zone_id, &fin_de_la_piste_3(b.zone_id))
        .await;
    rien_n_est_ajoute(&b).await;
}

#[tokio::test]
async fn migration_152_active_la_radio_des_zones_au_reglage_par_defaut() {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    db.execute_batch(
        "INSERT INTO zones (id, name, autoplay_enabled) VALUES (1, 'Défaut', 0);
         INSERT INTO zones (id, name, autoplay_enabled) VALUES (2, 'Album', 'random_album');
         INSERT INTO zones (id, name, autoplay_enabled) VALUES (3, 'Nulle', NULL);
         DELETE FROM _migrations WHERE version >= 152;",
    )
    .unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(db);
    assert_eq!(repo.get_autoplay_mode(1), AutoplayMode::Similar);
    assert_eq!(repo.get_autoplay_mode(2), AutoplayMode::RandomAlbum);
    assert_eq!(repo.get_autoplay_mode(3), AutoplayMode::Similar);
}

// --- Radio du titre chez le service (TIDAL) --------------------------------

/// Un « tidal » bouchonné dont la radio de titre rend : la graine, un titre
/// déjà dans la file, deux fois `t1`, puis `t2`.
struct TidalBouchonne;

fn piste(id: &str) -> StreamTrack {
    StreamTrack {
        id: id.into(),
        title: format!("Titre {id}"),
        artist: "Voisin".into(),
        album: None,
        album_id: None,
        duration_ms: 180_000,
        cover_path: None,
        track_number: None,
        disc_number: None,
        explicit: false,
        quality: None,
        isrc: None,
        disponible: None,
        composer: None,
        artist_id: None,
    }
}

fn hors_sujet() -> TuneError {
    "hors sujet".into()
}

#[async_trait::async_trait]
impl StreamingService for TidalBouchonne {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        "tidal"
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _enabled: bool) {}
    async fn authenticate(&mut self, _c: &serde_json::Value) -> Result<AuthStatus, TuneError> {
        Err(hors_sujet())
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: true,
            ..AuthStatus::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        Err(hors_sujet())
    }
    async fn get_track(&self, _id: &str) -> Result<StreamTrack, TuneError> {
        Err(hors_sujet())
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err(hors_sujet())
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err(hors_sujet())
    }
    async fn get_album_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(hors_sujet())
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err(hors_sujet())
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err(hors_sujet())
    }
    async fn get_playlist_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(hors_sujet())
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(vec![])
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(vec![])
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(vec![])
    }
    async fn get_track_radio(
        &self,
        track_id: &str,
        _limit: usize,
    ) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(vec![
            piste(track_id),
            piste("deja"),
            piste("t1"),
            piste("t1"),
            piste("t2"),
        ])
    }
}

#[tokio::test]
async fn la_radio_du_titre_tidal_prolonge_la_file_sans_doublon() {
    let mut services = crate::streaming::ServiceRegistry::new();
    services.register(Box::new(TidalBouchonne));
    let b = banc(services).await;
    PlayQueueRepo::with_backend(b.db.clone())
        .append(
            b.zone_id,
            &[QueueInput::Streaming {
                source: "tidal".into(),
                source_id: "deja".into(),
                title: "Déjà là".into(),
                artist: "Voisin".into(),
                album: None,
                cover_url: None,
                duration_ms: 180_000,
                track_number: None,
                disc_number: None,
                album_ref: None,
                artist_ref: None,
            }],
        )
        .unwrap();
    let ajoutes = b
        .poller
        .autoplay_streaming_radio(
            b.zone_id,
            "Artiste",
            "tidal",
            Some("graine"),
            &["deja".to_string()].into_iter().collect(),
        )
        .await;
    assert_eq!(ajoutes, 2);
    let ids: Vec<Option<String>> = PlayQueueRepo::with_backend(b.db.clone())
        .get_ordered(b.zone_id)
        .unwrap()
        .into_iter()
        .map(|r| r.source_id)
        .collect();
    assert_eq!(
        ids[3..].to_vec(),
        vec![Some("t1".to_string()), Some("t2".to_string())]
    );
}
