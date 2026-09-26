//! #5143 — une piste suivante BANNIE n'est ni préchargée, ni armée, ni jouée
//! après un échec : la file [A, B banni, C], A en cours, désigne C partout.
//!
//! Relevé par la PR #5132 (#4806, bannir un titre de service) : l'avance de la
//! file enjambait B, mais le préchargement le téléchargeait quand même (bande
//! passante, quota ou comptage chez le service), et l'armement sans blanc
//! renonçait au lieu d'armer C. Tous passent désormais par UNE fonction,
//! [`PositionPoller::prochaine_position_jouable`], dont le cœur est
//! [`crate::orchestrator::lignes_bannies_a_enjamber`] — le même que
//! l'avance (`enjamber_les_pistes_bannies`).
//!
//! Trois témoins, chacun par la vraie porte :
//!  1. le préchargement (`PrefetchEngine::prefetch_next`) demande l'URL de C
//!     au service, jamais celle de B ;
//!  2. l'armement sans blanc (vrai `tick`, renderer simulé) envoie C au
//!     renderer, une seule fois, et l'écran suit C à l'enchaînement ;
//!  3. la reprise après une piste injouable (vraie fin de piste) enjambe le
//!     banni qui suit la piste en échec.
use super::lire_ensuite_dans_la_fenetre_gapless::{ARMEE, Banc, COURANTE, INSEREE, SUITE};
use crate::db::backend::DbBackend;
use crate::db::hidden_repo::{HiddenRepo, TitreDeService};
use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::error::TuneError;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::prefetch::PrefetchEngine;
use crate::streaming::ServiceRegistry;
use crate::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Profil des sélections automatiques d'une base vierge (`active_profile_id`
/// absent) : le profil global.
const PROFIL: i64 = 1;

// --- 1. Préchargement ------------------------------------------------------

/// Un service qui NOTE chaque `get_track_url` et n'en sert aucune : le
/// préchargement s'arrête là, sans rien décoder. Ce qu'il a noté est
/// exactement ce qui aurait été téléchargé.
struct ServiceQuiNote {
    demandes: Arc<std::sync::Mutex<Vec<String>>>,
}

fn non_servi() -> TuneError {
    TuneError::Streaming("service de banc : rien n'est servi".into())
}

#[async_trait::async_trait]
impl StreamingService for ServiceQuiNote {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        "qobuz"
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _enabled: bool) {}
    async fn authenticate(
        &mut self,
        _credentials: &serde_json::Value,
    ) -> Result<AuthStatus, TuneError> {
        Err(non_servi())
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
    async fn search(&self, _query: &str, _limit: usize) -> Result<SearchResults, TuneError> {
        Err(non_servi())
    }
    async fn get_track(&self, _track_id: &str) -> Result<StreamTrack, TuneError> {
        Err(non_servi())
    }
    async fn get_track_url(
        &self,
        track_id: &str,
        _quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError> {
        self.demandes.lock().unwrap().push(track_id.to_string());
        Err(non_servi())
    }
    async fn get_album(&self, _album_id: &str) -> Result<StreamAlbum, TuneError> {
        Err(non_servi())
    }
    async fn get_album_tracks(&self, _album_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(non_servi())
    }
    async fn get_artist(&self, _artist_id: &str) -> Result<StreamArtist, TuneError> {
        Err(non_servi())
    }
    async fn get_playlist(&self, _playlist_id: &str) -> Result<StreamPlaylist, TuneError> {
        Err(non_servi())
    }
    async fn get_playlist_tracks(&self, _playlist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(non_servi())
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
}

fn titre_qobuz(source_id: &str) -> QueueInput {
    QueueInput::Streaming {
        source: "qobuz".into(),
        source_id: source_id.into(),
        title: format!("Titre {source_id}"),
        artist: "Quelqu'un".into(),
        album: Some("Un album".into()),
        cover_url: None,
        duration_ms: 200_000,
        track_number: None,
        disc_number: None,
    }
}

/// **LA MESURE DU TICKET.** File Qobuz [A, B banni, C], A en cours : le
/// préchargement doit demander C au service. Avant le correctif il demandait
/// B — `streaming_queue.get(next_pos)`, sans regarder les bannis.
#[tokio::test]
async fn le_prechargement_vise_c_et_jamais_la_suivante_bannie() {
    let sqlite = SqliteDb::open_in_memory().unwrap();
    sqlite.init_schema().unwrap();
    run_migrations(&sqlite).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(sqlite);
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("Salon", Some("dlna"), Some("uuid:banc-5143"))
        .unwrap();
    PlayQueueRepo::with_backend(db.clone())
        .append(
            zone_id,
            &[titre_qobuz("A"), titre_qobuz("B"), titre_qobuz("C")],
        )
        .unwrap();
    assert!(
        HiddenRepo::with_backend(db.clone())
            .ban_streaming_track(
                PROFIL,
                &TitreDeService {
                    source: "qobuz".into(),
                    source_id: "B".into(),
                    ..Default::default()
                },
            )
            .unwrap()
    );

    let playback = Arc::new(PlaybackManager::new());
    playback
        .play(
            zone_id,
            NowPlaying {
                title: "Titre A".into(),
                source: "qobuz".into(),
                source_id: Some("A".into()),
                duration_ms: 200_000,
                ..Default::default()
            },
        )
        .await;
    playback.update_queue_info(zone_id, 0, 3).await;

    let demandes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(ServiceQuiNote {
        demandes: demandes.clone(),
    }));

    PrefetchEngine::new()
        .prefetch_next(db, Arc::new(Mutex::new(registre)), playback, zone_id)
        .await;

    assert_eq!(
        *demandes.lock().unwrap(),
        vec!["C".to_string()],
        "le préchargement doit demander au service la piste que l'avance \
         jouera (C), jamais la suivante bannie (B)"
    );
}

// --- 2. Armement sans blanc ------------------------------------------------

/// File [COURANTE, ARMEE bannie, SUITE] sur un renderer DLNA : dans la
/// fenêtre des trente secondes, c'est SUITE qui part au renderer, une seule
/// fois malgré dix sondages, et l'écran la suit à l'enchaînement. Avant le
/// correctif, rien n'était armé (l'enchaînement sans blanc était perdu) ; et
/// un armement de SUITE sans que la garde « file changée » ne partage la même
/// suivante serait désarmé puis ré-armé à chaque sondage.
#[tokio::test]
async fn le_gapless_arme_c_et_pas_la_suivante_bannie() {
    let mut banc = Banc::monter().await;
    assert!(
        HiddenRepo::with_backend(banc.db.clone())
            .ban_track(PROFIL, banc.pistes[1])
            .unwrap()
    );

    for ms in 0..10u64 {
        banc.a(275_000 + ms * 1_000).await;
        banc.tic().await;
    }
    assert_eq!(
        banc.armees().await,
        vec![SUITE.to_string()],
        "UN SEUL armement, et de C : jamais de B ({ARMEE}), et pas de \
         ré-armement à chaque sondage"
    );

    banc.le_renderer_enchaine().await;
    banc.tic().await;
    assert_eq!(banc.joue_par_le_renderer().await.as_deref(), Some(SUITE));
    assert_eq!(
        banc.ecran().await,
        (2, SUITE.to_string()),
        "l'écran suit la piste armée, par-dessus la bannie"
    );
    assert!(
        banc.play_complets().await.iter().all(|t| t != ARMEE),
        "la bannie n'est jamais lancée"
    );
}

// --- 3. Reprise après échec ------------------------------------------------

/// File [COURANTE, ARMEE injouable, SUITE bannie, INSEREE] : à la fin de
/// COURANTE, ARMEE échoue (son fichier a disparu) ; la reprise doit enjamber
/// SUITE, bannie, et lancer INSEREE. C'est le branchement de #5132 dans la
/// boucle de reprises d'`avancer_avec_reprises`, jusqu'ici sans témoin.
#[tokio::test]
async fn la_reprise_apres_un_echec_enjambe_la_suivante_bannie() {
    let banc = Banc::monter().await;
    banc.ajouter_en_fin(3).await;
    std::fs::remove_file(banc._fichiers[1].path()).unwrap();
    assert!(
        HiddenRepo::with_backend(banc.db.clone())
            .ban_track(PROFIL, banc.pistes[2])
            .unwrap()
    );

    let etat = banc.playback.get_state(banc.zone_id).await;
    assert_eq!(
        (etat.queue_position, etat.queue_length),
        (0, 4),
        "{COURANTE} en cours, quatre lignes en file"
    );
    banc.poller.handle_track_end(banc.zone_id, &etat).await;

    let (position, titre) = banc.ecran().await;
    assert_eq!(
        (position, titre.as_str()),
        (3, INSEREE),
        "après l'échec de {ARMEE}, la reprise enjambe {SUITE} (bannie)"
    );
    assert!(
        banc.play_complets().await.iter().all(|t| t != SUITE),
        "la bannie n'est jamais lancée"
    );
}
