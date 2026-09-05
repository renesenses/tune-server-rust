//! Témoins de `resolve_local_track` (REF-5, #2219).
//!
//! `resolve_local_track` (1 693 lignes) n'avait aucun test qui l'appelle
//! directement : `dsd_passthrough_tests` et `resolution_annoncee_tests`
//! éprouvent des règles pures, `annonce_apres_sortie_guard` relit du texte.
//! Avant de la découper, on fixe ici son comportement par sa porte publique,
//! `resolve_queue_item_url`, sur une base mémoire et un fichier de la caisse.
//!
//! Chaque témoin nomme un chemin de décision et ce qu'il doit rendre : le type
//! MIME servi, l'extension de l'URL, la présence d'une session, les propriétés
//! annoncées. Aucun renderer réel : une zone DLNA dont l'appareil est inconnu du
//! registre des sorties est réputée accepter tout MIME (`dlna_supports_mime`
//! rend `Some(true)`), ce qui est le comportement de production sans sonde.

use std::sync::Arc;

use tokio::sync::Mutex;
use tune_core::db::backend::DbBackend;
use tune_core::db::migrations::run_migrations;
use tune_core::db::models::Track;
use tune_core::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::http::streamer::AudioStreamer;
use tune_core::orchestrator::PlaybackOrchestrator;
use tune_core::outputs::registry::OutputRegistry;
use tune_core::playback::PlaybackManager;
use tune_core::streaming::registry::ServiceRegistry;

/// Un FLAC réel de la caisse : le décodeur et le transcodeur le lisent vraiment.
const FLAC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test.flac");

struct Banc {
    orch: PlaybackOrchestrator,
    db: Arc<dyn DbBackend>,
    zone_id: i64,
    file: PlayQueueRepo,
}

/// Une base mémoire, un orchestrateur sans sortie ni service, une zone du type
/// demandé. `device_id` est ce que la zone croit adresser ; il n'existe pas.
async fn banc(output_type: &str, device_id: &str) -> Banc {
    let sqlite = SqliteDb::open_in_memory().expect("base mémoire");
    sqlite.init_schema().expect("schéma");
    run_migrations(&sqlite).expect("migrations");
    let db: Arc<dyn DbBackend> = Arc::new(sqlite);
    let orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("Salon (banc local)", Some(output_type), Some(device_id))
        .expect("zone");
    Banc {
        orch,
        zone_id,
        file: PlayQueueRepo::with_backend(db.clone()),
        db,
    }
}

impl Banc {
    fn zones(&self) -> ZoneRepo {
        ZoneRepo::with_backend(self.db.clone())
    }

    /// Une piste locale en base, pointant sur `chemin`, avec les propriétés
    /// annoncées `sample_rate`/`bit_depth` (la base peut différer du fichier :
    /// c'est ce que la résolution annonce qu'on veut fixer).
    fn piste(&self, chemin: &str, sample_rate: i32, bit_depth: i32) -> i64 {
        let mut t = Track::new("Piste du banc".into());
        t.artist_name = Some("Artiste du banc".into());
        t.album_title = Some("Album du banc".into());
        t.duration_ms = 1_000;
        t.file_path = Some(chemin.into());
        t.format = Some("flac".into());
        t.sample_rate = Some(sample_rate);
        t.bit_depth = Some(bit_depth);
        t.channels = 2;
        t.file_size = std::fs::metadata(chemin).ok().map(|m| m.len() as i64);
        t.source = "local".into();
        TrackRepo::with_backend(self.db.clone())
            .create(&t)
            .expect("piste")
    }

    async fn resoudre(
        &self,
        track_id: i64,
    ) -> Result<tune_core::orchestrator::ResolvedQueueItem, String> {
        self.file
            .append(self.zone_id, &[QueueInput::Local { track_id }])
            .expect("file");
        self.orch.resolve_queue_item_url(self.zone_id, 0).await
    }
}

fn extension(url: &str) -> &str {
    url.rsplit('.').next().unwrap_or("")
}

#[tokio::test]
async fn un_flac_part_tel_quel_vers_un_renderer_dlna_qui_ne_refuse_rien() {
    let b = banc("dlna", "uuid:renderer-inconnu").await;
    let id = b.piste(FLAC, 44_100, 16);

    let r = b.resoudre(id).await.expect("résolution");

    assert_eq!(
        r.mime_type, "audio/flac",
        "aucun forçage : le FLAC est servi brut"
    );
    assert_eq!(
        extension(&r.url),
        "flac",
        "l'URL porte l'extension du conteneur servi : {}",
        r.url
    );
    assert!(
        r.url.contains("/stream/"),
        "servi par le relais de Tune : {}",
        r.url
    );
    assert!(
        r.stream_id.is_some(),
        "une session de fichier est ouverte pour le passthrough"
    );
    assert_eq!(
        r.sample_rate,
        Some(44_100),
        "la fréquence annoncée est celle de la base"
    );
    assert_eq!(r.bit_depth, Some(16));
    assert_eq!(r.channels, Some(2));
    assert_eq!(
        r.file_size,
        std::fs::metadata(FLAC).ok().map(|m| m.len()),
        "en passthrough réseau, la taille annoncée est celle du fichier sur disque (#1132)"
    );
    assert_eq!(r.source.as_deref(), Some("local"));
    assert_eq!(r.title, "Piste du banc");
}

#[tokio::test]
async fn le_forcage_lpcm_de_la_zone_transcode_le_flac_en_wav() {
    let b = banc("dlna", "uuid:renderer-inconnu").await;
    b.zones()
        .update_dlna_lpcm(b.zone_id, true)
        .expect("dlna_lpcm");
    let id = b.piste(FLAC, 44_100, 16);

    let r = b.resoudre(id).await.expect("résolution");

    assert_eq!(
        r.mime_type, "audio/wav",
        "`dlna_lpcm` force le WAV, même quand le renderer accepterait le FLAC"
    );
    assert_eq!(extension(&r.url), "wav", "{}", r.url);
    assert!(r.stream_id.is_some());
}

#[tokio::test]
async fn le_plafond_16_bits_de_la_zone_reencode_un_flac_hi_res_en_16_bits() {
    let b = banc("dlna", "uuid:renderer-inconnu").await;
    b.zones()
        .update_dlna_cap_16bit(b.zone_id, true)
        .expect("cap");
    let id = b.piste(FLAC, 96_000, 24);

    let r = b.resoudre(id).await.expect("résolution");

    assert_eq!(
        r.mime_type, "audio/flac",
        "le plafond garde le FLAC, il ne change que la profondeur (#1137)"
    );
    assert_eq!(
        r.bit_depth,
        Some(16),
        "24 bits annoncés en base, 16 bits servis : le passthrough a bien été refusé"
    );
    assert!(r.stream_id.is_some());
}

#[tokio::test]
async fn une_sortie_oaat_recoit_toujours_du_wav() {
    let b = banc("oaat", "oaat:banc").await;
    let id = b.piste(FLAC, 44_100, 16);

    let r = b.resoudre(id).await.expect("résolution");

    assert_eq!(
        r.mime_type, "audio/wav",
        "OAAT attend du PCM brut : le FLAC est décodé et servi en WAV"
    );
    assert_eq!(extension(&r.url), "wav", "{}", r.url);
    assert!(r.stream_id.is_some());
}

#[tokio::test]
async fn une_sortie_locale_recoit_toujours_du_wav() {
    let b = banc("local", "local:default").await;
    let id = b.piste(FLAC, 44_100, 16);

    let r = b.resoudre(id).await.expect("résolution");

    assert_eq!(
        r.mime_type, "audio/wav",
        "la sortie locale ne lit que du PCM entier normalisé : tout passe par le décodeur, même un WAV"
    );
    assert_eq!(extension(&r.url), "wav", "{}", r.url);
    assert!(r.stream_id.is_some());
}

#[tokio::test]
async fn un_fichier_disparu_est_nomme_dans_l_erreur_et_n_ouvre_aucune_session() {
    let b = banc("dlna", "uuid:renderer-inconnu").await;
    let disparu = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/n_existe_pas.flac"
    );
    let id = b.piste(disparu, 44_100, 16);

    let err = b
        .resoudre(id)
        .await
        .err()
        .expect("le fichier manque : la résolution doit échouer");

    assert!(
        err.starts_with("file_not_found:"),
        "l'erreur nomme le motif, que l'interface traduit : {err}"
    );
    assert!(
        err.contains("n_existe_pas.flac"),
        "et le chemin fautif : {err}"
    );
}
