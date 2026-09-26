//! #5080 — un titre Qobuz sur une zone réseau à égaliseur ou crossfeed
//! attendait la piste ENTIÈRE avant la première note.
//!
//! Fil 1949 (0.9.165, DDC-0 C19 en DLNA, égaliseur et crossfeed armés,
//! `dsp_progressif_reseau: true`) : `streaming_pretranscode_for_renderer_or_dsp
//! dsp_active=true`, puis `streaming_pretranscode_complete file_size=322500931`
//! et `playback_timing resolve_ms=92559` — 92,6 s de silence pour un 24/96 de
//! Liszt, pendant lesquelles le testeur a mis en pause puis relancé cinq fois.
//! Le bras HTTPS des services (`relayer_le_flux`, #2863) téléchargeait,
//! décodait, traitait et ré-encodait le fichier entier avant de rendre une
//! adresse. La bibliothèque locale, sur la même zone, démarrait en 26 ms :
//! LAT-F1 (#3357) l'avait mise sur le WAV progressif, jamais les services.
//!
//! ## Le banc
//!
//! La vraie `relayer_le_flux`, un vrai serveur HTTP local à la place du CDN.
//! 1. Un CDN qui ne répond PAS : la résolution doit rendre son adresse quand
//!    même. Avant le correctif, elle attendait le dernier octet.
//! 2. Un CDN qui sert le FLAC de test par `Range` : la session reçoit l'en-tête
//!    WAV intact puis le PCM TRAITÉ par l'égaliseur de la zone — exactement la
//!    sortie de la chaîne DSP appliquée au décodage brut.

use super::{PlayRequest, PlaybackOrchestrator, service_en_wav_progressif};
use crate::db::migrations::run_migrations;
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::{AudioStreamer, StreamInfo};
use crate::outputs::registry::OutputRegistry;
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;
use axum::{
    Router,
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test.flac");

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

/// La zone du fil 1949 : un renderer DLNA (absent du registre, donc présumé
/// capable — comme dans les témoins de LAT-F1), un égaliseur installé et
/// armé, l'opt-in `dsp_progressif_reseau` coché.
fn zone_du_fil_1949(orch: &PlaybackOrchestrator) -> i64 {
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("DDC-0 C19", Some("dlna"), Some("uuid:c19-5080"))
        .unwrap();
    let settings = SettingsRepo::with_backend(orch.db.clone());
    let profil = crate::audio::eq::EqProfile {
        enabled: true,
        bands: vec![crate::audio::eq::EqBandSpec {
            freq: 1000.0,
            gain: 9.0,
            q: 0.71,
            band_type: "peak".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    settings
        .set(
            &format!("zone_{zone_id}_eq_profile"),
            &serde_json::to_string(&profil).unwrap(),
        )
        .unwrap();
    settings.set("plugin_equalizer_installed", "true").unwrap();
    settings.set("dsp_progressif_reseau", "true").unwrap();
    zone_id
}

fn requete_qobuz(zone_id: i64) -> PlayRequest {
    PlayRequest {
        zone_id,
        output_device_id: None,
        track_id: None,
        source: Some("qobuz".into()),
        source_id: Some("441697196".into()),
        title: Some("Les préludes".into()),
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms: None,
        seek_ms: None,
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
    }
}

fn flux_qobuz(url: String, sample_rate: u32, bit_depth: u16) -> crate::streaming::StreamUrl {
    crate::streaming::StreamUrl {
        url,
        mime_type: "audio/flac".into(),
        quality: crate::streaming::StreamQuality {
            codec: "flac".into(),
            sample_rate,
            bit_depth,
            bitrate: None,
            channels: 2,
        },
        expires_at: None,
        headers: Vec::new(),
    }
}

fn info_amont(flux: &crate::streaming::StreamUrl) -> StreamInfo {
    StreamInfo {
        format: "flac".into(),
        mime_type: flux.mime_type.clone(),
        sample_rate: flux.quality.sample_rate,
        bit_depth: flux.quality.bit_depth,
        channels: 2,
        ..Default::default()
    }
}

/// Un CDN qui accepte la connexion et ne répond RIEN tant que le témoin ne
/// l'a pas libéré — un téléchargement qui n'en finit pas. Libéré, il répond
/// 503 : les fils bloquants encore suspendus sur lui se terminent, et le
/// runtime du test peut s'arrêter.
async fn cdn_muet() -> (String, tokio::sync::watch::Sender<bool>) {
    let (liberer, attente) = tokio::sync::watch::channel(false);
    let app = Router::new().route(
        "/piste.flac",
        get(move || {
            let mut attente = attente.clone();
            async move {
                let _ = attente.wait_for(|libre| *libre).await;
                StatusCode::SERVICE_UNAVAILABLE
            }
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/piste.flac", ecoute.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
    (url, liberer)
}

/// Un CDN qui sert le FLAC de test en `Range`, comme Qobuz.
async fn cdn_par_range() -> String {
    let octets: Arc<Vec<u8>> = Arc::new(std::fs::read(FIXTURE).unwrap());
    let app = Router::new().route(
        "/piste.flac",
        get(move |entetes: HeaderMap| {
            let octets = octets.clone();
            async move {
                let total = octets.len();
                let (debut, fin) = entetes
                    .get(header::RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("bytes="))
                    .and_then(|v| {
                        let (a, b) = v.split_once('-')?;
                        let a: usize = a.parse().ok()?;
                        let b: usize = if b.is_empty() {
                            total - 1
                        } else {
                            b.parse().ok()?
                        };
                        Some((a, b.min(total - 1)))
                    })
                    .unwrap_or((0, total - 1));
                (
                    StatusCode::PARTIAL_CONTENT,
                    [
                        (
                            header::CONTENT_RANGE,
                            format!("bytes {debut}-{fin}/{total}"),
                        ),
                        (header::CONTENT_TYPE, "audio/flac".to_string()),
                    ],
                    octets[debut..=fin].to_vec(),
                )
                    .into_response()
            }
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/piste.flac", ecoute.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
    url
}

/// **Le témoin du fil 1949.** Le CDN ne rend rien : la résolution doit
/// quand même rendre son adresse — un WAV progressif, que le renderer
/// commencera à tirer pendant que le décodage avance.
///
/// Sabotage qui rend ce témoin ROUGE : retirer le bras #5080 de
/// `relayer_le_flux` — la résolution retombe sur le pré-transcodage, qui
/// attend le dernier octet du CDN, et le délai de 5 s expire.
#[tokio::test(flavor = "multi_thread")]
async fn un_titre_qobuz_a_egaliseur_n_attend_pas_la_piste_entiere_5080() {
    let orch = orchestrateur();
    let zone_id = zone_du_fil_1949(&orch);
    let (url, liberer) = cdn_muet().await;
    let flux = flux_qobuz(url, 96_000, 24);
    let req = requete_qobuz(zone_id);

    let resolu = tokio::time::timeout(
        Duration::from_secs(5),
        orch.relayer_le_flux(
            &req,
            "441697196",
            "qobuz",
            &flux,
            info_amont(&flux),
            false,
            "flac".into(),
            None,
        ),
    )
    .await;
    // Libérer le CDN AVANT d'affirmer : un fil bloquant encore suspendu
    // dessus retiendrait l'arrêt du runtime.
    let _ = liberer.send(true);

    let (adresse, session, mime, taille) = resolu
        .expect(
            "la résolution a attendu le CDN : le flux de service part encore \
             par le pré-transcodage du fichier entier (#5080)",
        )
        .expect("la résolution doit réussir");
    assert_eq!(mime, "audio/wav", "WAV progressif attendu");
    assert!(adresse.ends_with(".wav"), "{adresse}");
    assert!(session.is_some());
    assert_eq!(taille, None, "un flux progressif n'annonce pas de taille");
    let wire = orch
        .streamer
        .stream_output_wire(session.as_deref().unwrap())
        .await
        .expect("la session doit exister");
    assert_eq!(
        (wire.sample_rate, wire.bit_depth),
        (96_000, 24),
        "le Hi-Res garde sa profondeur : le renderer lit le FLAC amont"
    );
}

/// Le PCM qui sort de la session est celui de la chaîne DSP de la zone
/// appliquée au décodage — pas le décodage brut. Sans le relais, l'égaliseur
/// serait armé, affiché… et absent du signal (#2863 à l'envers).
///
/// Sabotage qui rend ce témoin ROUGE : passer `tx` au lieu du canal relayé
/// dans `servir_le_service_en_wav_progressif`.
#[tokio::test(flavor = "multi_thread")]
async fn le_wav_progressif_d_un_service_porte_l_egaliseur_de_la_zone_5080() {
    let orch = orchestrateur();
    let zone_id = zone_du_fil_1949(&orch);
    let brut = crate::audio::decode::decode_to_pcm(FIXTURE, None, Some(2), 0.0, 0.0).unwrap();
    let (sr, bd) = (brut.sample_rate, brut.bit_depth);
    let url = cdn_par_range().await;
    let flux = flux_qobuz(url, sr, bd);
    let req = requete_qobuz(zone_id);

    let (_, session, mime, _) = orch
        .relayer_le_flux(
            &req,
            "441697196",
            "qobuz",
            &flux,
            info_amont(&flux),
            false,
            "flac".into(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(mime, "audio/wav");
    let session = {
        let etat = orch.streamer.sessions_state();
        let etat = etat.lock().await;
        etat.get(session.as_deref().unwrap()).unwrap().clone()
    };

    let attendu_brut = brut.pcm_bytes();
    let mut attendu = attendu_brut.clone();
    orch.load_streaming_dsp(zone_id, None, sr, 2)
        .process(&mut attendu, bd);
    assert_ne!(
        attendu, attendu_brut,
        "le montage doit armer un égaliseur qui change le signal, sinon le test ne prouve rien"
    );

    let premier = tokio::time::timeout(Duration::from_secs(10), session.recv_chunk())
        .await
        .expect("aucun octet en 10 s")
        .expect("canal fermé avant l'en-tête");
    assert_eq!(&premier[..4], b"RIFF", "l'en-tête WAV doit passer intact");
    // L'en-tête part SEUL, premier chunk du décodeur : il ne porte pas de PCM.
    assert!(
        premier.len() <= 80,
        "en-tête attendu seul : {} octets",
        premier.len()
    );
    let mut pcm = Vec::new();
    while pcm.len() < attendu.len() {
        match tokio::time::timeout(Duration::from_secs(10), session.recv_chunk()).await {
            Ok(Some(chunk)) => pcm.extend_from_slice(&chunk),
            _ => break,
        }
    }
    let n = pcm.len().min(attendu.len());
    assert!(n > 4096, "trop peu de PCM reçu : {n} octets");
    assert_ne!(
        &pcm[..n],
        &attendu_brut[..n],
        "la session sert le décodage BRUT : l'égaliseur de la zone est perdu"
    );
    assert_eq!(
        &pcm[..n],
        &attendu[..n],
        "la session doit servir le décodage traité par la chaîne DSP de la zone"
    );
}

/// La règle pure : chaque condition manquante rend au fichier.
#[test]
fn le_wav_progressif_d_un_service_exige_toutes_ses_conditions_5080() {
    assert!(service_en_wav_progressif(true, true, true, true, true));
    // Aucun traitement : le proxy verbatim, bit-perfect, ne bouge pas.
    assert!(!service_en_wav_progressif(false, true, true, true, true));
    // Sortie non réseau (navigateur, PULL) : leurs propres bras.
    assert!(!service_en_wav_progressif(true, false, true, true, true));
    // Ni opt-in ni crossfeed seul : le format servi ne change pas sans accord.
    assert!(!service_en_wav_progressif(true, true, false, true, true));
    // Flux qui ne se décode pas au fil de l'eau (AAC Tidal, Deezer…).
    assert!(!service_en_wav_progressif(true, true, true, false, true));
    // Renderer qui n'annonce pas le LPCM à cette profondeur.
    assert!(!service_en_wav_progressif(true, true, true, true, false));
}
