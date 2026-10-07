//! #2264 — la règle de version appliquée à la LECTURE, de bout en bout.
//!
//! Mesuré sur ce que joue la zone (`now_playing` après `play()` ou
//! `play_from_queue()`, sur une vraie sortie factice) et non sur la règle
//! seule : sans le branchement dans `play_inner`, la piste demandée joue
//! telle quelle et ces épreuves rougissent.
//!
//! Le banc : « So What » en deux exemplaires LOCAUX du même enregistrement
//! (même titre, même artiste, 0,5 s d'écart), l'un en 44,1/16, l'autre en
//! 96/24, et un « So What (Live) » qui n'est PAS le même enregistrement.

use std::sync::Arc;

use tokio::sync::Mutex;

use super::super::{PlayRequest, PlaybackOrchestrator};
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::error::TuneError;
use crate::http::streamer::AudioStreamer;
use crate::library::groupes_versions::RegleDeChoix;
use crate::library::regle_de_version::poser_regle_du_profil;
use crate::outputs::mock::MockOutput;
use crate::outputs::registry::OutputRegistry;
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;
use crate::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamQuality,
    StreamTrack, StreamUrl, StreamingService,
};

const SORTIE: &str = "dlna:banc-2264";
/// 44,1/16, la piste qu'on lance.
const STANDARD: i64 = 1;
/// 96/24, le même enregistrement.
const HAUTE: i64 = 2;
/// Le live : un autre enregistrement.
const LIVE: i64 = 3;

struct Banc {
    orch: PlaybackOrchestrator,
    zone_id: i64,
    _dossier: tempfile::TempDir,
    chemin_haute: String,
}

async fn banc() -> Banc {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let orch = PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );
    let dossier = tempfile::tempdir().unwrap();
    let mut chemins = Vec::new();
    for nom in ["standard.flac", "haute.flac", "live.flac"] {
        let chemin = dossier.path().join(nom);
        std::fs::copy(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test.flac"),
            &chemin,
        )
        .unwrap();
        chemins.push(chemin.to_string_lossy().into_owned());
    }
    orch.db
        .execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'Miles Davis');
             INSERT INTO albums (id, title, artist_id) VALUES (1, 'Kind of Blue', 1);
             INSERT INTO albums (id, title, artist_id) VALUES (2, 'Kind of Blue (Legacy)', 1);
             INSERT INTO albums (id, title, artist_id) VALUES (3, 'Newport 1958', 1);",
        )
        .unwrap();
    for (id, titre, album, duree, sr, bd, chemin) in [
        (
            STANDARD,
            "So What",
            1i64,
            300_000i64,
            44_100i64,
            16i64,
            &chemins[0],
        ),
        (HAUTE, "So What", 2, 300_500, 96_000, 24, &chemins[1]),
        (LIVE, "So What (Live)", 3, 300_200, 44_100, 16, &chemins[2]),
    ] {
        let p: [&dyn ToSqlValue; 7] = [&id, &titre, &album, &duree, &sr, &bd, chemin];
        orch.db
            .execute(
                "INSERT INTO tracks (id, title, album_id, artist_id, duration_ms, sample_rate, \
                 bit_depth, file_path, format, channels, source) \
                 VALUES (?, ?, ?, 1, ?, ?, ?, ?, 'flac', 2, 'local')",
                &p,
            )
            .unwrap();
    }
    let zone_repo = ZoneRepo::with_backend(orch.db.clone());
    let zone_id = zone_repo
        .create("Salon 2264", Some("dlna"), Some(SORTIE))
        .unwrap();
    zone_repo.update_dlna_native_flac(zone_id, true).unwrap();
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(SORTIE, "Salon 2264").with_type("dlna"),
    ));
    Banc {
        orch,
        zone_id,
        chemin_haute: chemins[1].clone(),
        _dossier: dossier,
    }
}

fn piste(zone_id: i64, track_id: i64) -> PlayRequest {
    PlayRequest {
        zone_id,
        track_id: Some(track_id),
        source: Some("local".into()),
        ..Default::default()
    }
}

async fn joue(b: &Banc) -> crate::playback::NowPlaying {
    b.orch
        .playback
        .get_state(b.zone_id)
        .await
        .now_playing
        .expect("la zone joue")
}

// ─── La règle, à chaque type de lancement ──────────────────────────────

#[tokio::test]
async fn lancer_une_piste_joue_la_version_de_la_regle() {
    let b = banc().await;
    b.orch.play(piste(b.zone_id, STANDARD)).await.unwrap();
    let np = joue(&b).await;
    assert_eq!(
        np.track_id,
        Some(HAUTE),
        "règle `local` : la bibliothèque, puis la meilleure qualité — le 96/24"
    );
    assert_eq!(
        np.sample_rate,
        Some(96_000),
        "la piste en cours dit la qualité JOUÉE"
    );
    let v = np.version.expect("la décision est publiée");
    assert_eq!(v.origin, "rule");
    assert_eq!(v.rule, "local");
    assert_eq!(v.rule_origin, "default");
    assert_eq!(v.requested.and_then(|r| r.track_id), Some(STANDARD));
    assert!(!v.fallback);
}

#[tokio::test]
async fn un_autre_enregistrement_n_est_jamais_substitue() {
    // Contre-épreuve du rapprochement : le live a même artiste et même durée
    // à 0,2 s près, mais un marqueur d'édition.
    let b = banc().await;
    b.orch.play(piste(b.zone_id, LIVE)).await.unwrap();
    let np = joue(&b).await;
    assert_eq!(np.track_id, Some(LIVE));
    let v = np.version.expect("la règle s'est prononcée");
    assert!(v.requested.is_none(), "rien n'a été substitué");
}

#[tokio::test]
async fn lancer_depuis_la_file_joue_la_version_de_la_regle() {
    // La file porte ce qu'y mettent une playlist, un album, un favori ou
    // « lire ensuite » : tous avancent par `play_from_queue`.
    let b = banc().await;
    b.orch.persist_local_queue(b.zone_id, &[LIVE, STANDARD], 0);
    b.orch.playback.update_queue_info(b.zone_id, 1, 2).await;
    b.orch.play_from_queue(b.zone_id, 1).await.unwrap();
    let np = joue(&b).await;
    assert_eq!(np.track_id, Some(HAUTE));
    assert_eq!(
        np.version
            .and_then(|v| v.requested)
            .and_then(|r| r.track_id),
        Some(STANDARD)
    );
}

#[tokio::test]
async fn une_reprise_garde_la_version_qui_joue() {
    // Reprise après Stop, avance rapide, reconnexion : la demande nomme la
    // piste EN COURS, et sa version ne change pas — même si la règle, elle,
    // a changé entre-temps.
    let b = banc().await;
    b.orch.play(piste(b.zone_id, STANDARD)).await.unwrap();
    assert_eq!(joue(&b).await.track_id, Some(HAUTE));
    let avant = joue(&b).await.version;
    b.orch
        .play_without_history(PlayRequest {
            seek_ms: Some(30_000),
            ..piste(b.zone_id, HAUTE)
        })
        .await
        .unwrap();
    let np = joue(&b).await;
    assert_eq!(np.track_id, Some(HAUTE));
    assert_eq!(np.version, avant, "la décision du lancement reste publiée");
}

#[tokio::test]
async fn l_enchainement_sans_blanc_renonce_quand_la_version_change() {
    let b = banc().await;
    b.orch
        .persist_local_queue(b.zone_id, &[LIVE, STANDARD, HAUTE], 0);
    let ligne = |pos| {
        crate::db::play_queue_repo::PlayQueueRepo::with_backend(b.orch.db.clone())
            .get_at(b.zone_id, pos)
            .unwrap()
            .unwrap()
    };
    assert!(
        b.orch.la_version_change(b.zone_id, &ligne(1)).await,
        "STANDARD jouerait HAUTE : on n'arme pas"
    );
    assert!(
        !b.orch.la_version_change(b.zone_id, &ligne(2)).await,
        "HAUTE se joue elle-même : on arme"
    );
    let refus = b.orch.resolve_queue_item_url(b.zone_id, 1).await.err();
    assert_eq!(refus.as_deref(), Some("version_rule_declines_gapless"));
    assert!(b.orch.resolve_queue_item_url(b.zone_id, 2).await.is_ok());
}

// ─── Le choix explicite prime ───────────────────────────────────────────

#[tokio::test]
async fn le_choix_explicite_prime_sur_la_regle() {
    let b = banc().await;
    b.orch
        .epingler_version_explicite(b.zone_id, Some(STANDARD), Some("local"), None);
    b.orch.play(piste(b.zone_id, STANDARD)).await.unwrap();
    let np = joue(&b).await;
    assert_eq!(
        np.track_id,
        Some(STANDARD),
        "la version choisie à la main joue"
    );
    let v = np.version.unwrap();
    assert_eq!(v.origin, "explicit");
    assert!(v.requested.is_none());

    // Contre-épreuve : l'épingle est consommée ; le lancement « normal »
    // suivant applique la règle.
    b.orch.play(piste(b.zone_id, LIVE)).await.unwrap();
    b.orch.play(piste(b.zone_id, STANDARD)).await.unwrap();
    assert_eq!(joue(&b).await.track_id, Some(HAUTE));
}

#[tokio::test]
async fn l_epingle_d_une_autre_piste_ne_protege_pas_celle_ci() {
    let b = banc().await;
    b.orch
        .epingler_version_explicite(b.zone_id, Some(LIVE), Some("local"), None);
    b.orch.play(piste(b.zone_id, STANDARD)).await.unwrap();
    assert_eq!(joue(&b).await.track_id, Some(HAUTE));
}

// ─── Le repli est signalé ───────────────────────────────────────────────

#[tokio::test]
async fn la_version_preferee_absente_du_disque_est_un_repli_signale() {
    let b = banc().await;
    std::fs::remove_file(&b.chemin_haute).unwrap();
    b.orch.play(piste(b.zone_id, STANDARD)).await.unwrap();
    let np = joue(&b).await;
    assert_eq!(
        np.track_id,
        Some(STANDARD),
        "on passe à la suivante disponible"
    );
    let v = np.version.expect("et on le dit");
    assert!(v.fallback, "version de repli");
    assert_eq!(v.unavailable_source.as_deref(), Some("local"));
    assert!(v.requested.is_none(), "la piste lancée est celle qui joue");
}

#[tokio::test]
async fn le_service_prefere_non_connecte_est_un_repli_signale() {
    let b = banc().await;
    poser_regle_du_profil(
        &b.orch.db,
        1,
        Some(&RegleDeChoix::PrefererService("qobuz".into())),
    )
    .unwrap();
    b.orch
        .playback
        .set_session_profile(b.zone_id, Some(1))
        .await;
    b.orch.play(piste(b.zone_id, STANDARD)).await.unwrap();
    let np = joue(&b).await;
    assert_eq!(
        np.track_id,
        Some(HAUTE),
        "Qobuz absent : bibliothèque, meilleure qualité"
    );
    let v = np.version.unwrap();
    assert_eq!(v.rule, "service:qobuz");
    assert_eq!(v.rule_origin, "profile");
    assert!(v.fallback);
    assert_eq!(v.unavailable_source.as_deref(), Some("qobuz"));
}

// ─── La règle par profil ────────────────────────────────────────────────

/// Un Qobuz qui répond à toute recherche « So What » 192/24, même durée.
struct QobuzDeBanc;

fn non_servi() -> TuneError {
    TuneError::Streaming("service de banc".into())
}

fn so_what_qobuz() -> StreamTrack {
    StreamTrack {
        id: "q-so-what".into(),
        title: "So What".into(),
        artist: "Miles Davis".into(),
        album: Some("Kind of Blue".into()),
        album_id: Some("q-kob".into()),
        duration_ms: 300_100,
        cover_path: None,
        track_number: Some(1),
        disc_number: Some(1),
        explicit: false,
        quality: Some(StreamQuality {
            codec: "FLAC".into(),
            sample_rate: 192_000,
            bit_depth: 24,
            bitrate: None,
            channels: 2,
        }),
        isrc: None,
        disponible: None,
        composer: None,
        artist_id: None,
    }
}

#[async_trait::async_trait]
impl StreamingService for QobuzDeBanc {
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
    async fn authenticate(&mut self, _c: &serde_json::Value) -> Result<AuthStatus, TuneError> {
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
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        Ok(SearchResults {
            tracks: vec![so_what_qobuz()],
            albums: vec![],
            artists: vec![],
            playlists: vec![],
        })
    }
    async fn get_track(&self, _t: &str) -> Result<StreamTrack, TuneError> {
        Ok(so_what_qobuz())
    }
    async fn get_track_url(&self, _t: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err(non_servi())
    }
    async fn get_album(&self, _a: &str) -> Result<StreamAlbum, TuneError> {
        Err(non_servi())
    }
    async fn get_album_tracks(&self, _a: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(non_servi())
    }
    async fn get_artist(&self, _a: &str) -> Result<StreamArtist, TuneError> {
        Err(non_servi())
    }
    async fn get_playlist(&self, _p: &str) -> Result<StreamPlaylist, TuneError> {
        Err(non_servi())
    }
    async fn get_playlist_tracks(&self, _p: &str) -> Result<Vec<StreamTrack>, TuneError> {
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

#[tokio::test]
async fn la_regle_est_celle_du_profil_de_la_zone() {
    let b = banc().await;
    b.orch.services.lock().await.register(Box::new(QobuzDeBanc));
    poser_regle_du_profil(
        &b.orch.db,
        2,
        Some(&RegleDeChoix::PrefererService("qobuz".into())),
    )
    .unwrap();

    // Profil 2 : Qobuz d'abord. La demande est RÉÉCRITE vers Qobuz (la
    // résolution d'un flux Qobuz n'existe pas sur ce banc : on lit la
    // décision au point unique, sans aller jusqu'au flux).
    b.orch
        .playback
        .set_session_profile(b.zone_id, Some(2))
        .await;
    let mut req = piste(b.zone_id, STANDARD);
    let v = b
        .orch
        .appliquer_la_regle_de_version(&mut req)
        .await
        .unwrap();
    assert_eq!(req.source.as_deref(), Some("qobuz"));
    assert_eq!(req.source_id.as_deref(), Some("q-so-what"));
    assert_eq!(req.track_id, None);
    assert_eq!(req.album_ref.as_deref(), Some("q-kob"));
    assert_eq!(v.rule, "service:qobuz");
    assert_eq!(v.rule_origin, "profile");
    assert!(!v.fallback);

    // Contre-épreuve : profil 3, rien de réglé — le défaut `local`.
    b.orch
        .playback
        .set_session_profile(b.zone_id, Some(3))
        .await;
    let mut req = piste(b.zone_id, STANDARD);
    let v = b
        .orch
        .appliquer_la_regle_de_version(&mut req)
        .await
        .unwrap();
    assert_eq!(req.track_id, Some(HAUTE));
    assert_eq!(req.source, None);
    assert_eq!(v.rule, "local");
    assert_eq!(v.rule_origin, "default");
}

#[tokio::test]
async fn une_piste_de_service_lancee_rejoint_la_bibliotheque_sous_la_regle_local() {
    let b = banc().await;
    b.orch.services.lock().await.register(Box::new(QobuzDeBanc));
    let mut req = PlayRequest {
        zone_id: b.zone_id,
        source: Some("qobuz".into()),
        source_id: Some("q-so-what".into()),
        title: Some("So What".into()),
        artist_name: Some("Miles Davis".into()),
        duration_ms: Some(300_100),
        ..Default::default()
    };
    let v = b
        .orch
        .appliquer_la_regle_de_version(&mut req)
        .await
        .unwrap();
    assert_eq!(
        req.track_id,
        Some(HAUTE),
        "la copie de la bibliothèque, en 96/24"
    );
    assert_eq!(req.source, None);
    assert_eq!(
        req.title, None,
        "la demande ne décrit plus la piste de service"
    );
    let r = v.requested.unwrap();
    assert_eq!(
        (r.source.as_str(), r.source_id.as_deref()),
        ("qobuz", Some("q-so-what"))
    );
}

#[tokio::test]
async fn une_radio_ou_un_fichier_glisse_n_ont_pas_de_version() {
    let b = banc().await;
    let mut radio = PlayRequest {
        zone_id: b.zone_id,
        source: Some("radio".into()),
        source_id: Some("https://exemple.invalid/flux".into()),
        ..Default::default()
    };
    assert!(
        b.orch
            .appliquer_la_regle_de_version(&mut radio)
            .await
            .is_none()
    );
    let mut glisse = PlayRequest {
        zone_id: b.zone_id,
        temp_file_path: Some("/tmp/x.flac".into()),
        ..piste(b.zone_id, STANDARD)
    };
    assert!(
        b.orch
            .appliquer_la_regle_de_version(&mut glisse)
            .await
            .is_none()
    );
    assert_eq!(glisse.track_id, Some(STANDARD));
}
