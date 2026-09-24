//! #3365 — la piste de SERVICE atteinte par enchaînement garde sa qualité.
//!
//! Serge Asselin, fil 1670 : album Qobuz en DLNA sur un Hifi Rose RS250A.
//! Piste 1 : Tune affiche 192 kHz / 24 bits. Piste 2 : 44,1 / 16. Le Rose
//! reste à 192 / 24 — c'est l'annonce de Tune qui est fausse, pas le flux.
//!
//! La chronologie rejouée ici est celle de production :
//! 1. la piste 1 joue, avec la qualité que `composer_le_now_playing` lui a
//!    donnée depuis `resolve_stream` ;
//! 2. `resolve_queue_item_url` arme la piste 2 : une session de flux rangée
//!    sous `gapless_sessions[zone]`, et la qualité que `resolve_stream` a
//!    rendue pour elle — ici posée par le même point d'entrée que le site de
//!    production (`ranger_la_qualite_pre_armee` +
//!    `QualitePreArmee::d_un_flux_de_service`), le service réel n'étant pas
//!    joignable en test ;
//! 3. le sondeur prononce l'enchaînement : `advance_queue_metadata`.
//!
//! Les témoins négatifs : une qualité INCONNUE reste inconnue, et une qualité
//! rangée pour un AUTRE flux n'est jamais reprise.

use std::sync::Arc;
use tokio::sync::Mutex;

use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::{PlayQueueRepo, StreamingQueueItem};
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::{AudioStreamer, StreamInfo};
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::registry::ServiceRegistry;

use super::{PlaybackOrchestrator, QualitePreArmee, ResolvedStream};

fn orchestrateur() -> PlaybackOrchestrator {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
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

async fn ouvrir_un_flux(
    orch: &PlaybackOrchestrator,
) -> (String, tokio::sync::mpsc::Sender<Vec<u8>>) {
    let (id, tx, _pret) = orch
        .streamer
        .create_session(
            StreamInfo {
                format: "flac".to_string(),
                mime_type: "audio/flac".to_string(),
                ..StreamInfo::default()
            },
            false,
            1,
        )
        .await;
    (id, tx)
}

/// Ce que `resolve_stream` rend pour une piste Qobuz relayée.
fn resolu_qobuz(
    stream_id: &str,
    sample_rate: Option<u32>,
    bit_depth: Option<u32>,
) -> ResolvedStream {
    ResolvedStream {
        url: format!("http://127.0.0.1:8888/stream/{stream_id}.flac"),
        mime_type: "audio/flac".into(),
        title: String::new(),
        artist: None,
        album: None,
        duration_ms: Some(331_000),
        source: "qobuz".into(),
        cover_url: None,
        stream_id: Some(stream_id.to_string()),
        file_size: None,
        sample_rate,
        bit_depth,
        channels: Some(2),
        origin_url: None,
        bitrate_kbps: None,
    }
}

struct Chronologie {
    orch: PlaybackOrchestrator,
    zone_id: i64,
    flux_pre_arme: String,
    _emetteurs: Vec<tokio::sync::mpsc::Sender<Vec<u8>>>,
}

/// L'album Qobuz de Serge, deux pistes, la piste 1 joue en 192/24 et la
/// piste 2 est armée. `qualite_piste_2` est ce que `resolve_stream` en sait.
async fn album_qobuz_piste_2_armee(qualite_piste_2: (Option<u32>, Option<u32>)) -> Chronologie {
    let orch = orchestrateur();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Rose RS250A", Some("dlna"), Some("uuid:hifirose-rs250a"))
        .unwrap();
    let file: Vec<StreamingQueueItem> = ["School", "Bloody Well Right"]
        .iter()
        .enumerate()
        .map(|(i, titre)| {
            (
                format!("qobuz-{i}"),
                titre.to_string(),
                "Supertramp".to_string(),
                Some("Crime of the Century (2014 HD Remaster)".to_string()),
                None,
                331_000i64,
                Some("qobuz".to_string()),
                Some(i as i64 + 1),
                None,
            )
        })
        .collect();
    PlayQueueRepo::with_backend(orch.db.clone())
        .set_streaming_queue(zone_id, &file)
        .unwrap();

    let mut emetteurs = Vec::new();
    let (flux_1, tx) = ouvrir_un_flux(&orch).await;
    emetteurs.push(tx);
    // Piste 1 : ce que `composer_le_now_playing` publie (192/24, FLAC).
    orch.playback
        .play(
            zone_id,
            NowPlaying {
                track_id: None,
                title: "School".into(),
                duration_ms: 331_000,
                source: "qobuz".into(),
                source_id: Some("qobuz-0".into()),
                stream_id: Some(flux_1),
                format: Some("flac".into()),
                sample_rate: Some(192_000),
                bit_depth: Some(24),
                ..Default::default()
            },
        )
        .await;

    // Armement de la piste 2, comme `resolve_queue_item_url`.
    let (flux_2, tx) = ouvrir_un_flux(&orch).await;
    emetteurs.push(tx);
    orch.gapless_sessions
        .lock()
        .await
        .insert(zone_id, flux_2.clone());
    let resolu = resolu_qobuz(&flux_2, qualite_piste_2.0, qualite_piste_2.1);
    orch.ranger_la_qualite_pre_armee(
        zone_id,
        QualitePreArmee::d_un_flux_de_service("qobuz", Some("qobuz-1"), &resolu),
    )
    .await;

    Chronologie {
        orch,
        zone_id,
        flux_pre_arme: flux_2,
        _emetteurs: emetteurs,
    }
}

async fn piste_en_cours(c: &Chronologie) -> NowPlaying {
    c.orch
        .playback
        .get_state(c.zone_id)
        .await
        .now_playing
        .expect("la zone joue")
}

/// Le témoin : après l'enchaînement, la piste 2 annonce 192/24 FLAC.
#[tokio::test]
async fn la_piste_de_service_suivante_garde_sa_qualite_3365() {
    let c = album_qobuz_piste_2_armee((Some(192_000), Some(24))).await;

    c.orch
        .advance_queue_metadata(c.zone_id, 1)
        .await
        .expect("l'avance gapless doit aboutir");

    let np = piste_en_cours(&c).await;
    assert_eq!(np.title, "Bloody Well Right", "la file a bien avancé");
    assert_eq!(
        np.stream_id.as_deref(),
        Some(c.flux_pre_arme.as_str()),
        "la zone adopte le flux pré-armé (#3442)"
    );
    assert_eq!(
        (np.format.as_deref(), np.sample_rate, np.bit_depth),
        (Some("flac"), Some(192_000), Some(24)),
        "la piste 2 doit annoncer la qualité que `resolve_stream` a rendue à \
         l'armement, comme la piste 1 — et non retomber à rien (affiché \
         « 44,1 / 16 » par le repli du chemin du signal, fil 1670)"
    );
}

/// Négatif : `resolve_stream` ne connaissait ni la fréquence ni la
/// profondeur. Elles restent `None` — aucun chiffre inventé.
#[tokio::test]
async fn une_qualite_inconnue_reste_inconnue_3365() {
    let c = album_qobuz_piste_2_armee((None, None)).await;

    c.orch.advance_queue_metadata(c.zone_id, 1).await.unwrap();

    let np = piste_en_cours(&c).await;
    assert_eq!(np.title, "Bloody Well Right");
    assert_eq!(
        (np.sample_rate, np.bit_depth),
        (None, None),
        "une qualité inconnue à l'armement ne doit pas recevoir de valeur"
    );
}

/// Négatif : la qualité rangée appartient à un AUTRE flux que celui que la
/// zone adopte (armement remplacé, ou pas d'armement par flux). Elle n'est
/// pas reprise, et elle ne traîne pas pour la piste d'après.
#[tokio::test]
async fn la_qualite_d_un_autre_flux_n_est_pas_reprise_3365() {
    let c = album_qobuz_piste_2_armee((Some(192_000), Some(24))).await;
    // Le flux rangé sous la zone n'est plus celui dont on a noté la qualité.
    let (autre, _tx) = ouvrir_un_flux(&c.orch).await;
    c.orch
        .gapless_sessions
        .lock()
        .await
        .insert(c.zone_id, autre.clone());

    c.orch.advance_queue_metadata(c.zone_id, 1).await.unwrap();

    let np = piste_en_cours(&c).await;
    assert_eq!(np.stream_id.as_deref(), Some(autre.as_str()));
    assert_eq!(
        (np.sample_rate, np.bit_depth),
        (None, None),
        "la qualité d'un autre flux ne doit pas être annoncée"
    );
    assert!(
        c.orch.qualites_pre_armees.lock().await.is_empty(),
        "la qualité périmée ne doit pas survivre à la transition"
    );
}

/// Le site de production range bien la qualité : les épreuves ci-dessus
/// passeraient encore si l'appel disparaissait de `resolve_queue_item_url`.
/// Le corps est découpé entre sa signature et la fonction suivante.
#[test]
fn l_armement_de_production_range_la_qualite_3365() {
    let src = include_str!("queue.rs");
    let debut = src
        .find("pub async fn resolve_queue_item_url(")
        .expect("resolve_queue_item_url existe");
    let fin = src[debut..]
        .find("pub async fn resolve_gapless_next_local_file(")
        .expect("la fonction suivante existe")
        + debut;
    let corps = &src[debut..fin];
    assert!(
        corps.contains("self.ranger_la_qualite_pre_armee(")
            && corps.contains("QualitePreArmee::d_un_flux_de_service("),
        "resolve_queue_item_url doit ranger la qualité du flux de service armé"
    );
}
