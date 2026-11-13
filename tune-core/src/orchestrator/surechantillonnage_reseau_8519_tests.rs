//! Discussion #8519 — le suréchantillonneur sur une zone DLNA, de la décision
//! au fichier servi, devant un FAUX renderer (serveur SOAP local qui publie un
//! `Sink` choisi par le banc). Aucun son : rien n'est joué, seul le fichier
//! servi est lu.

use super::*;
use std::sync::Arc;

use axum::Router;
use axum::routing::post;

use crate::db::models::Track;
use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use crate::db::settings_repo::SettingsRepo;
use crate::db::track_repo::TrackRepo;
use crate::db::zone_repo::ZoneRepo;

const FLAC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test.flac");
const RENDERER: &str = "uuid:faux-renderer-8519";

fn enveloppe(corps: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>{corps}</s:Body></s:Envelope>"#
    )
}

/// Un faux renderer DLNA qui publie `sink` dans `GetProtocolInfo`.
async fn faux_renderer(sink: &[&str]) -> crate::outputs::dlna::DlnaOutput {
    let sink = Arc::new(sink.join(","));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new()
        .route(
            "/cm",
            post(move |corps: String| {
                let sink = sink.clone();
                async move {
                    if corps.contains("GetProtocolInfo") {
                        enveloppe(&format!(
                            "<u:GetProtocolInfoResponse><Source></Source><Sink>{sink}</Sink></u:GetProtocolInfoResponse>"
                        ))
                    } else {
                        enveloppe("<u:Response/>")
                    }
                }
            }),
        )
        .route("/av", post(|| async { enveloppe("<u:R/>") }))
        .route("/rc", post(|| async { enveloppe("<u:R/>") }));
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    crate::outputs::dlna::DlnaOutput::new(
        "Faux renderer 8519".into(),
        RENDERER.into(),
        "127.0.0.1".into(),
        format!("http://127.0.0.1:{port}/av"),
        format!("http://127.0.0.1:{port}/rc"),
        Some(format!("http://127.0.0.1:{port}/cm")),
    )
}

/// Un orchestrateur Premium, une zone DLNA servie par le faux renderer, le
/// greffon installé et activé sur la zone, la machine mesurée (tous les
/// filtres admis).
async fn banc(sink: &[&str]) -> (PlaybackOrchestrator, i64) {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let mut orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    );
    orch.license = Some(crate::license::licence_premium_pour_test(db.clone()));
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("Salon DLNA (banc 8519)", Some("dlna"), Some(RENDERER))
        .unwrap();
    orch.outputs
        .lock()
        .await
        .register(Box::new(faux_renderer(sink).await));
    let s = SettingsRepo::with_backend(db);
    s.set(crate::audio::premium_plugins::MIGRATION, "complete")
        .unwrap();
    s.set("plugin_upsampler_installed", "true").unwrap();
    s.set("plugin_upsampler_enabled", "true").unwrap();
    crate::audio::surechantillonnage::enregistrer_les_reglages(
        &s,
        zone_id,
        &serde_json::json!({"enabled": true}),
    )
    .unwrap();
    let mesure = |de, vers| crate::audio::surechantillonnage::MesureMachine {
        filter: "polyvalent".into(),
        from_hz: de,
        to_hz: vers,
        realtime_factor: 10.0,
        admitted: true,
    };
    crate::audio::surechantillonnage::enregistrer_le_verdict(
        &s,
        &crate::audio::surechantillonnage::VerdictMachine {
            simd_path: "test".into(),
            margin: 3.0,
            results: vec![
                mesure(44_100, 352_800),
                mesure(44_100, 705_600),
                mesure(48_000, 384_000),
                mesure(48_000, 768_000),
            ],
            measured_at: 0,
        },
    )
    .unwrap();
    (orch, zone_id)
}

/// Résout la piste FLAC 16/44,1 d'une seconde (position `pos` de la file) et
/// rend l'élément résolu, ce que la session publie et le chemin qu'elle sert.
async fn resoudre(
    orch: &PlaybackOrchestrator,
    zone_id: i64,
    pos: i64,
) -> (
    crate::orchestrator::ResolvedQueueItem,
    crate::http::streamer::StreamInfo,
    Option<String>,
) {
    // Une seule ligne `tracks` par fichier (`file_path` est UNIQUE) : la
    // suivante de la file rejoue la même piste, comme un album en boucle.
    let existante = TrackRepo::with_backend(orch.db.clone())
        .get_by_path(FLAC)
        .unwrap()
        .and_then(|t| t.id);
    let track_id = match existante {
        Some(id) => id,
        None => {
            let mut t = Track::new(format!("Piste {pos} du banc 8519"));
            t.duration_ms = 1_000;
            t.file_path = Some(FLAC.into());
            t.format = Some("flac".into());
            t.sample_rate = Some(44_100);
            t.bit_depth = Some(16);
            t.channels = 2;
            t.file_size = std::fs::metadata(FLAC).ok().map(|m| m.len() as i64);
            t.source = "local".into();
            TrackRepo::with_backend(orch.db.clone()).create(&t).unwrap()
        }
    };
    PlayQueueRepo::with_backend(orch.db.clone())
        .append(zone_id, &[QueueInput::Local { track_id }])
        .unwrap();
    let r = orch
        .resolve_queue_item_url(zone_id, pos)
        .await
        .expect("résolution");
    let sid = r.stream_id.clone().expect("une session est ouverte");
    let fil = orch
        .streamer
        .stream_output_wire(&sid)
        .await
        .expect("session vivante");
    let session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(&sid)
        .cloned()
        .expect("session vivante");
    let servi = session.file_path.lock().await.clone();
    (r, fil, servi)
}

/// STREAMINFO d'un FLAC : (cadence, profondeur, trames).
fn streaminfo(chemin: &str) -> (u32, u32, u64) {
    let d = std::fs::read(chemin).unwrap();
    assert_eq!(&d[..4], b"fLaC", "{chemin} n'est pas un FLAC");
    let si = &d[8..42];
    let cadence = (u32::from(si[10]) << 12) | (u32::from(si[11]) << 4) | (u32::from(si[12]) >> 4);
    let bps = (((u32::from(si[12]) & 1) << 4) | (u32::from(si[13]) >> 4)) + 1;
    let total = ((u64::from(si[13]) & 0x0f) << 32)
        | u64::from(u32::from_be_bytes([si[14], si[15], si[16], si[17]]));
    (cadence, bps, total)
}

/// Le renderer DÉCLARE le FLAC jusqu'à 192 kHz : la piste 44,1 kHz est
/// décodée à 44,1, suréchantillonnée ×4 et servie en FLAC 176,4 kHz / 24
/// bits par le fichier ; le flux le publie pour le chemin du signal. La
/// suivante (gapless : même résolution, rendu en cache) aussi.
#[tokio::test]
async fn un_renderer_qui_declare_192_khz_recoit_le_flac_a_176_4_khz_8519() {
    let (orch, zone_id) = banc(&[
        "http-get:*:audio/flac;rate=88200:*",
        "http-get:*:audio/flac;rate=176400:*",
        "http-get:*:audio/flac;rate=192000:*",
        "http-get:*:audio/L16;rate=44100;channels=2:DLNA.ORG_PN=LPCM",
    ])
    .await;
    let (r, fil, servi) = resoudre(&orch, zone_id, 0).await;
    assert_eq!(r.mime_type, "audio/flac");
    assert_eq!(r.sample_rate, Some(176_400), "cadence annoncée au renderer");
    assert_eq!(r.bit_depth, Some(24));
    let su = fil
        .surechantillonnage
        .clone()
        .expect("le flux porte l'étage du suréchantillonneur");
    assert_eq!((su.de, su.vers), (44_100, 176_400));
    assert_eq!(su.filtre, "polyvalent");
    assert!(su.reserve_db <= 0.0);
    assert!(fil.refus_surechantillonnage.is_none());
    let servi = servi.expect("un fichier converti");
    assert_ne!(servi, FLAC, "jamais le fichier d'origine");
    let (cadence, bps, trames) = streaminfo(&servi);
    assert_eq!(
        (cadence, bps),
        (176_400, 24),
        "le FLAC servi est à 176,4/24"
    );
    assert!(
        (176_400..176_400 + 17_640).contains(&trames),
        "une seconde ×4, plus la queue du noyau : {trames} trames"
    );

    // La suivante de l'album : même décision, rendu servi depuis le cache.
    let (r2, fil2, _) = resoudre(&orch, zone_id, 1).await;
    assert_eq!(r2.sample_rate, Some(176_400));
    assert!(
        fil2.surechantillonnage.is_some(),
        "rendu en cache : le flux le dit aussi"
    );
}

/// Renderer limité à 48 kHz (LPCM 44,1/48 seulement, FLAC sans cadence) :
/// aucune cadence au-dessus de la source, la piste part TELLE QUELLE et le
/// flux dit pourquoi.
#[tokio::test]
async fn un_renderer_limite_a_48_khz_garde_la_piste_et_dit_pourquoi_8519() {
    let (orch, zone_id) = banc(&[
        "http-get:*:audio/flac:*",
        "http-get:*:audio/L16;rate=44100;channels=2:*",
        "http-get:*:audio/L16;rate=48000;channels=2:*",
    ])
    .await;
    let (r, fil, servi) = resoudre(&orch, zone_id, 0).await;
    assert_eq!(r.sample_rate.unwrap_or(44_100), 44_100, "{}", r.url);
    assert_eq!(servi.as_deref(), Some(FLAC), "passthrough d'origine");
    assert!(fil.surechantillonnage.is_none());
    assert_eq!(fil.refus_surechantillonnage, Some("renderer_rate_ceiling"));
}

/// Sink sans aucune cadence : pas de suréchantillonnage, message clair. Le
/// plafond réglé sur la zone suffit ensuite à l'autoriser (×2 sous 96 kHz).
#[tokio::test]
async fn un_renderer_sans_cadence_declaree_n_est_pas_surechantillonne_8519() {
    let (orch, zone_id) = banc(&["http-get:*:audio/flac:*", "http-get:*:audio/wav:*"]).await;
    let (_, fil, servi) = resoudre(&orch, zone_id, 0).await;
    assert_eq!(servi.as_deref(), Some(FLAC));
    assert_eq!(fil.refus_surechantillonnage, Some("renderer_rates_unknown"));

    ZoneRepo::with_backend(orch.db.clone())
        .update_max_sample_rate(zone_id, Some(96_000))
        .unwrap();
    let (r, fil, _) = resoudre(&orch, zone_id, 1).await;
    assert_eq!(r.sample_rate, Some(88_200), "plafond de zone 96 kHz → 88,2");
    assert_eq!(fil.surechantillonnage.map(|s| s.vers), Some(88_200));
}

/// PURE : aucun suréchantillonnage, aucun refus noté (le chemin du signal
/// dit déjà « inactif (mode PURE) »). Bit-perfect strict : rien non plus,
/// avec la raison.
#[tokio::test]
async fn pure_et_strict_ne_surechantillonnent_jamais_8519() {
    let sink = ["http-get:*:audio/flac;rate=176400:*"];
    let (orch, zone_id) = banc(&sink).await;
    SettingsRepo::with_backend(orch.db.clone())
        .set(&format!("zone_{zone_id}_audiophile"), r#"{"enabled":true}"#)
        .unwrap();
    let (r, fil, _) = resoudre(&orch, zone_id, 0).await;
    assert_eq!(r.sample_rate.unwrap_or(44_100), 44_100);
    assert!(fil.surechantillonnage.is_none());
    assert!(fil.refus_surechantillonnage.is_none());

    let (orch, zone_id) = banc(&sink).await;
    SettingsRepo::with_backend(orch.db.clone())
        .set(
            &crate::audio::bitperfect_strict::cle_de_zone(zone_id),
            "true",
        )
        .unwrap();
    let (r, fil, servi) = resoudre(&orch, zone_id, 0).await;
    assert_eq!(r.sample_rate.unwrap_or(44_100), 44_100);
    assert_eq!(servi.as_deref(), Some(FLAC));
    assert_eq!(fil.refus_surechantillonnage, Some("strict_bitperfect"));
}

/// Greffon désactivé sur la zone : strictement le comportement d'avant — la
/// piste part telle quelle, rien n'est noté.
#[tokio::test]
async fn sans_reglage_rien_ne_change_8519() {
    let (orch, zone_id) = banc(&["http-get:*:audio/flac;rate=176400:*"]).await;
    crate::audio::surechantillonnage::enregistrer_les_reglages(
        &SettingsRepo::with_backend(orch.db.clone()),
        zone_id,
        &serde_json::json!({"enabled": false}),
    )
    .unwrap();
    let (r, fil, servi) = resoudre(&orch, zone_id, 0).await;
    assert_eq!(r.sample_rate.unwrap_or(44_100), 44_100);
    assert_eq!(servi.as_deref(), Some(FLAC));
    assert!(fil.surechantillonnage.is_none() && fil.refus_surechantillonnage.is_none());
}
