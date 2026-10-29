//! #6018 (Dimitri, fil 2192) — un titre Spotify se joue dans une zone : le
//! PCM d'un faux librespot traverse la VRAIE résolution de l'orchestrateur et
//! sort par une session `/stream/<id>.wav`, celle que lisent toutes les
//! sorties. Aucun compte Spotify : faux librespot et faux pilote.
use super::*;
use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::{PlayQueueRepo, StreamingQueueItem};
use crate::db::sqlite::SqliteDb;
use crate::streaming::spotify_connect::OCTETS_PAR_SECONDE;
use crate::streaming::spotify_lecture::essais::banc;

fn orchestrateur() -> PlaybackOrchestrator {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    PlaybackOrchestrator::new(
        Arc::new(db),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        Some("127.0.0.1".into()),
    )
}

fn demande(zone_id: i64) -> PlayRequest {
    PlayRequest {
        zone_id,
        source: Some("spotify".into()),
        source_id: Some("4uLU6hMCjMI75M1A2tKUQC".into()),
        title: Some("Never Gonna Give You Up".into()),
        artist_name: Some("Rick Astley".into()),
        duration_ms: Some(1000),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spotify_6018_un_titre_spotify_se_joue_dans_une_zone() {
    let orch = orchestrateur();
    let b = banc(1000, OCTETS_PAR_SECONDE as u64);
    orch.sources_pcm()
        .inscrire("spotify", Arc::new(b.fournisseur()));
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon", Some("dlna"), Some("uuid:renderer"))
        .unwrap();

    let r = orch
        .resolve_stream(&demande(zone_id))
        .await
        .expect("la lecture Spotify ne doit plus être refusée (#6018)");
    assert_eq!(r.source, "spotify");
    assert!(
        r.url.contains("/stream/") && r.url.ends_with(".wav"),
        "{}",
        r.url
    );
    assert_eq!(r.mime_type, "audio/wav");
    assert_eq!(
        (r.sample_rate, r.bit_depth, r.channels),
        (Some(44_100), Some(16), Some(2))
    );

    let sid = r.stream_id.clone().unwrap();
    let session = orch.streamer.sessions_state().lock().await[&sid].clone();
    let mut recu = Vec::new();
    while let Some(t) =
        tokio::time::timeout(std::time::Duration::from_secs(10), session.recv_chunk())
            .await
            .expect("le PCM de librespot doit arriver dans la session")
    {
        recu.extend_from_slice(&t);
    }
    assert_eq!(&recu[..4], b"RIFF", "en-tête WAV de longueur exacte");
    let corps = &recu[44..];
    assert_eq!(corps.len(), OCTETS_PAR_SECONDE);
    assert!(
        corps.iter().all(|&o| o == 0x55),
        "le PCM de librespot, tel quel"
    );
}

/// Le pré-armement gapless ouvrirait le titre suivant PENDANT que l'autre
/// joue — pour Spotify, le lancer sur l'appareil et couper le titre en cours.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spotify_6018_le_titre_suivant_n_est_pas_lance_par_le_pre_armement() {
    let orch = orchestrateur();
    let b = banc(1000, OCTETS_PAR_SECONDE as u64);
    orch.sources_pcm()
        .inscrire("spotify", Arc::new(b.fournisseur()));
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon", Some("dlna"), Some("uuid:renderer"))
        .unwrap();
    let file: Vec<StreamingQueueItem> = ["piste-a", "piste-b"]
        .iter()
        .enumerate()
        .map(|(i, id)| {
            (
                id.to_string(),
                format!("Titre {i}"),
                "Artiste".to_string(),
                None,
                None,
                1000i64,
                Some("spotify".to_string()),
                Some(i as i64 + 1),
                None,
            )
        })
        .collect();
    PlayQueueRepo::with_backend(orch.db.clone())
        .set_streaming_queue(zone_id, &file)
        .unwrap();

    for position in [0, 1] {
        let refus = orch.resolve_queue_item_url(zone_id, position).await;
        assert!(
            refus.is_err(),
            "pré-armement Spotify accepté en position {position}"
        );
    }
    assert!(
        b.pilote.lancements.lock().unwrap().is_empty(),
        "le pré-armement a lancé un titre sur l'appareil Spotify"
    );
}

/// #6946 (Belkadi Yacine, fil 2423) — librespot ne reçoit de Spotify que de
/// l'Ogg Vorbis, 320 kbit/s au plus, quel que soit le compte. Son PCM passait
/// pourtant pour la source : le titre s'annonçait « wav » 44,1 kHz / 16 bits,
/// sans débit, c'est-à-dire comme un CD sans perte. La lecture en cours dit
/// désormais ce qui a été reçu ; le fil, lui, reste le WAV 16 bits d'avant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spotify_6946_le_titre_s_annonce_ogg_vorbis_320_et_non_wav_cd() {
    let orch = orchestrateur();
    let b = banc(1000, OCTETS_PAR_SECONDE as u64);
    orch.sources_pcm()
        .inscrire("spotify", Arc::new(b.fournisseur()));
    // Une zone à soi : l'arrêt des pompes d'une zone est global, et la zone 1
    // du test voisin, lancé en parallèle, verrait la sienne coupée.
    let req = demande(6946);

    let r = orch.resolve_stream(&req).await.expect("lecture Spotify");
    assert_eq!(
        (r.format_source.as_deref(), r.bitrate_kbps),
        (Some("ogg"), Some(320)),
        "le flux résolu doit porter ce que librespot a reçu (Ogg Vorbis 320), \
         pas seulement le WAV qu'il en a fait"
    );
    assert_eq!(
        (r.mime_type.as_str(), r.bit_depth),
        ("audio/wav", Some(16)),
        "le fil ne change pas : même WAV 16 bits qu'avant #6946"
    );

    let habillage = super::transport::Habillage {
        album: None,
        cover_path: None,
    };
    let np = orch.composer_le_now_playing(&req, &r, &habillage);
    assert_eq!(
        (np.format.as_deref(), np.bitrate_kbps),
        (Some("ogg"), Some(320)),
        "la lecture en cours annonçait « wav » sans débit, comme un CD"
    );
}
