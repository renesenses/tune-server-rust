//! **Une piste DSD de serveur média suit la politique DSD de la ZONE**, comme
//! un `.dsf` local — témoins par la porte publique (`resolve_stream`).
//!
//! # Le défaut
//!
//! Sur le `.18` le 23/09/2026 à 14:48 UTC, « Abacab » (Genesis, DSD64) indexé
//! depuis le `.15` partait vers la zone « Eversolo DMP-A8 » (`dlna`) par
//! `resolve_direct`, qui remettait l'URL distante TELLE QUELLE au renderer :
//! `dlna_set_uri_ok url="http://192.168.1.15:8888/api/v1/library/tracks/581978/audio"
//! advertised_mime=application/x-dsd`. Résultat : du BRUIT, la position
//! avançant ; les FLAC joués ensuite sur la même zone étaient bons.
//!
//! Le chemin LOCAL, lui, lit `dsd_mode` et la réponse du renderer avant
//! d'envoyer quoi que ce soit (`transport_dsd`, `should_dsd_passthrough`) :
//! DoP, `.dsf` brut servi par Tune (`/stream/<id>.dsf`), ou PCM décimé côté
//! serveur. Le chemin des serveurs média ne posait AUCUNE de ces questions.
//!
//! Sabotage qui fait rougir le premier témoin : retirer la branche
//! `est_dsd_brut(mime_type) && is_network_output_type(..)` de
//! `resolve_direct_url_de_source` — l'URL distante repart brute.

use crate::db::backend::DbBackend;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::{PlayRequest, PlaybackOrchestrator};
use crate::outputs::registry::OutputRegistry;
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

const ID: i64 = 62_301;
const RENDERER: &str = "dlna:renderer-1";

/// Nombre de super-blocs (un bloc par canal) du DSF de test.
const SUPER_BLOCS: usize = 16;
/// Ce que le serveur média envoie AVANT de marquer une pause : l'en-tête et
/// la première moitié des blocs — de quoi décoder plusieurs blocs PCM.
const MOITIE: usize = 92 + SUPER_BLOCS / 2 * 2 * 4096;
/// La pause du serveur entre les deux moitiés : un serveur qui cadence sa
/// route au débit nominal du flux, en accéléré.
const PAUSE: std::time::Duration = std::time::Duration::from_millis(1500);

/// Un DSF petit mais VALIDE (en-tête `DSD `, `fmt `, `data`), stéréo DSD64,
/// seize blocs par canal. Le motif 0x55 (±1 alterné) décode en quasi
/// silence — ce n'est pas le son qu'on teste, c'est le CHEMIN.
fn dsf_minuscule() -> Vec<u8> {
    let channels: u32 = 2;
    let block_size: u32 = 4096;
    let sample_rate: u32 = 2_822_400;
    let total_samples: u64 = block_size as u64 * 8 * SUPER_BLOCS as u64;
    let mut data = Vec::new();
    for _ in 0..SUPER_BLOCS {
        for _ in 0..channels {
            data.extend(std::iter::repeat_n(0x55u8, block_size as usize));
        }
    }
    let mut buf = Vec::new();
    buf.extend_from_slice(b"DSD ");
    buf.extend_from_slice(&28u64.to_le_bytes());
    buf.extend_from_slice(&(28 + 52 + 12 + data.len() as u64).to_le_bytes());
    buf.extend_from_slice(&0u64.to_le_bytes());
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&52u64.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&2u32.to_le_bytes());
    buf.extend_from_slice(&channels.to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&total_samples.to_le_bytes());
    buf.extend_from_slice(&block_size.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(12 + data.len() as u64).to_le_bytes());
    buf.extend_from_slice(&data);
    buf
}

/// Le « serveur média » : un serveur HTTP qui publie le DSF sous une URL SANS
/// extension, en `application/x-dsd` — exactement ce que le serveur média de
/// Tune publie dans son `<res>`. Compte les GET reçus, et dit quand il a
/// FINI d'envoyer un corps : il marque une pause au milieu, comme le .15 qui
/// cadence sa route audio au débit nominal du flux (5 minutes pour un DSD64).
struct ServeurMedia {
    url: String,
    requetes: Arc<AtomicUsize>,
    corps_termines: Arc<AtomicUsize>,
    _tache: tokio::task::JoinHandle<()>,
}

impl Drop for ServeurMedia {
    fn drop(&mut self) {
        self._tache.abort();
    }
}

async fn serveur_media() -> ServeurMedia {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/api/v1/library/tracks/581978/audio",
        listener.local_addr().unwrap()
    );
    let requetes = Arc::new(AtomicUsize::new(0));
    let corps_termines = Arc::new(AtomicUsize::new(0));
    let compteur = requetes.clone();
    let termines = corps_termines.clone();
    let corps = dsf_minuscule();
    let tache = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let mut requete = Vec::new();
            let mut octet = [0u8; 1];
            while !requete.ends_with(b"\r\n\r\n") {
                match socket.read_exact(&mut octet).await {
                    Ok(_) => requete.push(octet[0]),
                    Err(_) => break,
                }
                if requete.len() > 16_384 {
                    break;
                }
            }
            compteur.fetch_add(1, Ordering::SeqCst);
            let entete = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-dsd\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                corps.len()
            );
            let _ = socket.write_all(entete.as_bytes()).await;
            let _ = socket.write_all(&corps[..MOITIE]).await;
            let _ = socket.flush().await;
            tokio::time::sleep(PAUSE).await;
            let _ = socket.write_all(&corps[MOITIE..]).await;
            let _ = socket.shutdown().await;
            termines.fetch_add(1, Ordering::SeqCst);
        }
    });
    ServeurMedia {
        url,
        requetes,
        corps_termines,
        _tache: tache,
    }
}

/// Un orchestrateur dont la base porte UNE zone DLNA au `dsd_mode` demandé et
/// UNE piste de serveur média — sa source, son format indexé, sa résolution,
/// et son URL de lecture dans l'instantané (#2219, phase 2).
fn orchestrateur(dsd_mode: &str, format: &str, res: &str) -> (PlaybackOrchestrator, i64) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let (sample_rate, bit_depth) = if format == "dsd" {
        (2_822_400, 1)
    } else {
        (44_100, 16)
    };
    db.execute_batch(&format!(
        "INSERT INTO tracks (id,title,source,source_id,format,sample_rate,bit_depth,channels,duration_ms) \
         VALUES ({ID},'Abacab','upnp','uuid:258FC2D5-E2C3-B734-0-1|85944171f73967e8','{format}',{sample_rate},{bit_depth},2,417146); \
         INSERT INTO track_metadata (track_id,key,value) \
         VALUES ({ID},'upnp_res_url','{res}');"
    ))
    .unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let zones = ZoneRepo::with_backend(db.clone());
    let zone_id = zones
        .create("Eversolo DMP-A8", Some("dlna"), Some(RENDERER))
        .unwrap();
    zones.update_dsd_mode(zone_id, dsd_mode).unwrap();
    let orch = PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );
    (orch, zone_id)
}

/// La demande du bouton Lecture et de l'avance de file : un `track_id`, la
/// résolution lue sur la ligne, et RIEN sur la source — c'est la ligne qui
/// sait.
fn demande(zone_id: i64, sample_rate: u32, bit_depth: u16) -> PlayRequest {
    PlayRequest {
        zone_id,
        output_device_id: Some(RENDERER.into()),
        track_id: Some(ID),
        source: None,
        source_id: None,
        title: Some("Abacab".into()),
        artist_name: Some("Genesis".into()),
        album_title: Some("Abacab".into()),
        cover_url: None,
        duration_ms: Some(417_146),
        seek_ms: None,
        temp_file_path: None,
        sample_rate: Some(sample_rate),
        bit_depth: Some(bit_depth),
        media_format: None,
        track_number: None,
        disc_number: None,
    }
}

/// **Le défaut, en un test.** Zone en `pcm` (et `auto`, dont le renderer ne
/// répond rien : chemin sûr) : la sortie ne reçoit PAS l'URL distante brute
/// mais un flux WAV de Tune, décimé côté serveur — ce que le chemin local
/// fait d'un `.dsf` sur cette même zone.
///
/// Et AU FIL DE L'EAU : la résolution rend la main avant la fin du
/// téléchargement, et le premier octet WAV — en-tête puis PCM décodé — part
/// pendant que le serveur média marque encore sa pause. Mesuré sur le .18
/// (23/09, zone en `pcm`) : 5 minutes de téléchargement entier avant le
/// premier décodage, `/play` muet 30 s, rien au bout de 10 minutes.
#[tokio::test]
async fn en_pcm_le_dsd_distant_part_decime_par_tune_pas_en_url_brute() {
    for dsd_mode in ["pcm", "auto"] {
        let serveur = serveur_media().await;
        let (orch, zone_id) = orchestrateur(dsd_mode, "dsd", &serveur.url);
        let depart = std::time::Instant::now();
        let resolu = orch
            .resolve_stream(&demande(zone_id, 2_822_400, 1))
            .await
            .unwrap();
        assert!(
            depart.elapsed() < PAUSE,
            "[{dsd_mode}] la résolution ne doit pas attendre la fin du téléchargement \
             ({:?})",
            depart.elapsed()
        );

        assert_ne!(
            resolu.url, serveur.url,
            "[{dsd_mode}] l'URL distante ne doit PAS partir brute au renderer : \
             la zone demande du PCM"
        );
        assert!(
            resolu.url.contains("/stream/") && resolu.url.ends_with(".wav"),
            "[{dsd_mode}] la sortie doit tirer un flux de Tune : {}",
            resolu.url
        );
        assert_eq!(resolu.mime_type, "audio/wav", "[{dsd_mode}]");
        assert_eq!(
            resolu.sample_rate,
            Some(176_400),
            "[{dsd_mode}] un DSD64 se décime à 176,4 kHz, comme en local"
        );
        assert_eq!(resolu.bit_depth, Some(24), "[{dsd_mode}]");
        assert!(
            resolu.stream_id.is_some(),
            "[{dsd_mode}] une session doit porter le flux"
        );
        assert_eq!(
            resolu.origin_url.as_deref(),
            Some(serveur.url.as_str()),
            "[{dsd_mode}] l'origine reste connue"
        );
        assert_eq!(resolu.source, "upnp", "[{dsd_mode}]");

        // Le premier son part AVANT que le serveur ait fini d'envoyer.
        let sid = resolu.stream_id.as_deref().unwrap();
        let session = orch
            .streamer
            .sessions_state()
            .lock()
            .await
            .get(sid)
            .cloned()
            .expect("la session existe");
        let mut recu = Vec::new();
        while recu.len() <= 44 {
            let bloc = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                session.recv_chunk(),
            )
            .await
            .unwrap_or_else(|_| {
                panic!("[{dsd_mode}] du WAV doit arriver sans attendre la fin du téléchargement")
            })
            .expect("le canal est ouvert");
            recu.extend_from_slice(&bloc);
        }
        assert_eq!(
            &recu[..4],
            b"RIFF",
            "[{dsd_mode}] l'en-tête WAV part en premier"
        );
        assert!(recu.len() > 44, "[{dsd_mode}] du PCM décodé suit l'en-tête");
        assert_eq!(
            serveur.corps_termines.load(Ordering::SeqCst),
            0,
            "[{dsd_mode}] le PCM est parti pendant que le serveur média envoyait encore : \
             décodage au fil de l'eau, pas de téléchargement préalable"
        );
        assert_eq!(
            serveur.requetes.load(Ordering::SeqCst),
            1,
            "[{dsd_mode}] Tune tire le fichier UNE fois, pour le décoder"
        );
    }
}

/// **Témoin natif.** Zone en `native` : le `.dsf` part BRUT — mais par le fil
/// EXACT du `.dsf` local en passthrough : une adresse `/stream/<id>.dsf` de
/// Tune, même MIME, octets verbatim (session mandataire, Range servi), rien
/// n'est téléchargé ni décodé à la résolution.
#[tokio::test]
async fn en_natif_le_dsd_distant_part_brut_par_le_fil_du_dsf_local() {
    let serveur = serveur_media().await;
    let (orch, zone_id) = orchestrateur("native", "dsd", &serveur.url);
    let resolu = orch
        .resolve_stream(&demande(zone_id, 2_822_400, 1))
        .await
        .unwrap();

    assert_eq!(
        resolu.mime_type, "application/x-dsd",
        "en natif, le renderer reçoit du DSD brut"
    );
    assert!(
        resolu.url.contains("/stream/") && resolu.url.ends_with(".dsf"),
        "le fil doit être celui du .dsf local, `/stream/<id>.dsf` : {}",
        resolu.url
    );
    assert_eq!(resolu.sample_rate, Some(2_822_400));
    assert_eq!(resolu.bit_depth, Some(1));
    let sid = resolu.stream_id.as_deref().expect("une session mandataire");
    assert!(
        orch.streamer.is_seekable_session(sid).await,
        "les octets passent verbatim par une session mandataire, Range compris"
    );
    assert_eq!(
        resolu.origin_url.as_deref(),
        Some(serveur.url.as_str()),
        "l'adresse d'origine reste connue de la zone"
    );
    assert_eq!(
        serveur.requetes.load(Ordering::SeqCst),
        0,
        "rien n'est téléchargé ni décodé côté serveur en natif"
    );
}

/// **Témoin DoP.** Zone en `dop` : le DSD part emballé en trames PCM 24 bits
/// à 176,4 kHz (DSD64), par le même `anticiper_le_dop` qu'une piste locale ;
/// le fichier a été téléchargé pour cela.
#[tokio::test]
async fn en_dop_le_dsd_distant_part_emballe_comme_une_piste_locale() {
    let serveur = serveur_media().await;
    let (orch, zone_id) = orchestrateur("dop", "dsd", &serveur.url);
    let resolu = orch
        .resolve_stream(&demande(zone_id, 2_822_400, 1))
        .await
        .unwrap();

    assert_ne!(resolu.url, serveur.url);
    assert!(resolu.url.ends_with(".wav"), "{}", resolu.url);
    assert_eq!(resolu.mime_type, "audio/wav");
    assert_eq!(
        resolu.sample_rate,
        Some(176_400),
        "la cadence DoP d'un DSD64 est 176,4 kHz"
    );
    assert_eq!(resolu.bit_depth, Some(24));
    assert_eq!(resolu.source, "upnp");
    assert_eq!(resolu.origin_url.as_deref(), Some(serveur.url.as_str()));
    assert_eq!(serveur.requetes.load(Ordering::SeqCst), 1);
}

/// **Non-régression.** Une piste de serveur média qui n'est PAS du DSD
/// (FLAC) continue de partir telle quelle, quel que soit le réglage DSD de
/// la zone : la politique DSD ne concerne que le DSD.
#[tokio::test]
async fn une_piste_upnp_flac_part_toujours_telle_quelle() {
    for dsd_mode in ["pcm", "native", "dop"] {
        let serveur = serveur_media().await;
        let (orch, zone_id) = orchestrateur(dsd_mode, "flac", &serveur.url);
        let resolu = orch
            .resolve_stream(&demande(zone_id, 44_100, 16))
            .await
            .unwrap();
        assert_eq!(
            resolu.url, serveur.url,
            "[{dsd_mode}] un FLAC de serveur média part tel quel"
        );
        assert_eq!(resolu.mime_type, "audio/flac", "[{dsd_mode}]");
        assert!(resolu.stream_id.is_none(), "[{dsd_mode}]");
        assert_eq!(serveur.requetes.load(Ordering::SeqCst), 0, "[{dsd_mode}]");
    }
}

/// Le conteneur nommé pour l'adresse rendue et le décodeur : `dff` seulement
/// quand l'URL ou le MIME le disent, `dsf` sinon.
#[test]
fn le_conteneur_dsd_suit_l_url_puis_le_mime() {
    use crate::orchestrator::resolve_direct::conteneur_dsd;
    assert_eq!(
        conteneur_dsd(
            "http://192.168.1.15:8888/api/v1/library/tracks/581978/audio",
            "application/x-dsd"
        ),
        "dsf"
    );
    assert_eq!(
        conteneur_dsd("http://nas/x/y.dff", "application/x-dsd"),
        "dff"
    );
    assert_eq!(
        conteneur_dsd("http://nas/x/y.DFF?token=1", "audio/x-dsd"),
        "dff"
    );
    assert_eq!(conteneur_dsd("http://nas/x/y", "audio/x-dff"), "dff");
    assert_eq!(conteneur_dsd("http://nas/x/y.dsf", "audio/dsf"), "dsf");
}
