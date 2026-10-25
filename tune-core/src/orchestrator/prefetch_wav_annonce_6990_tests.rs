//! #6990 — une piste TIDAL servie depuis le préchargement à un renderer
//! DLNA qui ne lit pas le FLAC (ou dont le Sink n'a pas pu être lu) est
//! encodée en WAV. L'URL et le type annoncés doivent dire WAV : annoncer
//! « audio/flac » sous une URL `.flac` fait décoder du RIFF comme du FLAC à
//! un renderer qui croit l'étiquette.
//!
//! Sabotage (contre-épreuve) : dans `dsp.rs`, `encoder_le_prefetch_en_fichier`,
//! remettre `get_stream_url(.., "flac")` et `mime_type: "audio/flac"` fait
//! rougir `prefetch_6990_wav_annonce_comme_wav`.
use super::*;

fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    )
}

const RENDERER: &str = "dlna:emerson-6990";

fn demande() -> PlayRequest {
    PlayRequest {
        zone_id: 1,
        output_device_id: Some(RENDERER.into()),
        track_id: None,
        source: Some("tidal".into()),
        source_id: Some("12345".into()),
        title: Some("Titre TIDAL".into()),
        artist_name: Some("Artiste".into()),
        album_title: None,
        cover_url: None,
        duration_ms: Some(1_000),
        seek_ms: None,
        temp_file_path: None,
        sample_rate: Some(44_100),
        bit_depth: Some(16),
        media_format: None,
        track_number: None,
        disc_number: None,
        album_ref: None,
    }
}

fn tampon() -> crate::prefetch::PrefetchedTrack {
    crate::prefetch::PrefetchedTrack {
        source: "tidal".into(),
        source_id: "12345".into(),
        stream_id: None,
        // Une seconde de PCM 16 bits stéréo à 44,1 kHz.
        pcm_data: vec![0u8; 44_100 * 4],
        format: "flac".into(),
        sample_rate: 44_100,
        bit_depth: 16,
        channels: 2,
        duration_ms: 1_000,
        title: "Titre TIDAL".into(),
        artist: Some("Artiste".into()),
        album: None,
        cover_url: None,
        mime_type: "audio/flac".into(),
        upstream_url: None,
    }
}

fn chemin_de_l_url(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

#[tokio::test]
async fn prefetch_6990_wav_annonce_comme_wav() {
    let orch = orchestrateur();
    // Le Sink du renderer a été lu et ne porte pas le FLAC.
    orch.dlna_unsupported_mimes
        .lock()
        .await
        .insert(RENDERER.into(), vec!["audio/flac".into()]);
    let r = orch
        .serve_prefetched_pcm(tampon(), &demande())
        .await
        .expect("le préchargement doit être servi");
    assert!(r.stream_id.is_some(), "session absente");
    assert_eq!(
        r.mime_type, "audio/wav",
        "octets WAV annoncés comme {} au renderer (URL {})",
        r.mime_type, r.url
    );
    assert!(
        chemin_de_l_url(&r.url).ends_with(".wav"),
        "octets WAV servis sous une URL qui n'est pas .wav : {}",
        r.url
    );
}

/// Témoin : un renderer qui lit le FLAC reçoit toujours du FLAC, annoncé FLAC.
#[tokio::test]
async fn prefetch_6990_flac_reste_annonce_flac() {
    let orch = orchestrateur();
    let r = orch
        .serve_prefetched_pcm(tampon(), &demande())
        .await
        .expect("le préchargement doit être servi");
    // Sans sortie inscrite, la négociation ne s'applique pas : FLAC.
    assert_eq!(r.mime_type, "audio/flac");
    assert!(chemin_de_l_url(&r.url).ends_with(".flac"), "{}", r.url);
}
