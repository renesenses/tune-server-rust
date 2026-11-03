//! #6718 — le LUXMAN NT-07 du fil 2363 : un titre Qobuz 96/24 sur une zone
//! OpenHome à traitement actif attendait son pré-transcodage COMPLET
//! (`streaming_pretranscode_complete file_size=155290816`, 6 s) avant
//! `oh_play`. Le WAV progressif de #5080 lui était fermé : la capacité LPCM
//! d'une sortie OpenHome valait « inconnu » par type, faute de sonde.
//!
//! Le banc de #5080 (la vraie `relayer_le_flux`, un CDN local qui ne répond
//! rien), avec une vraie `OpenHomeOutput` enregistrée contre un faux renderer
//! dont `Playlist:1 ProtocolInfo` rend la liste donnée.

use super::service_wav_progressif_5080::{cdn_muet, flux_qobuz, info_amont, requete_qobuz};
use super::{PlayRequest, PlaybackOrchestrator};
use crate::db::migrations::run_migrations;
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::registry::OutputRegistry;
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;
use axum::{Router, routing::post};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const ID: &str = "uuid:luxman-nt07-6718";

/// Un faux renderer OpenHome : `POST /Playlist` rend `ProtocolInfo`.
async fn renderer_openhome(protocol_info: &'static str) -> u16 {
    let app = Router::new().route(
        "/Playlist",
        post(move || async move {
            format!(
                "<s:Envelope><s:Body><u:ProtocolInfoResponse><Value>{protocol_info}</Value>\
                 </u:ProtocolInfoResponse></s:Body></s:Envelope>"
            )
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
    port
}

/// La zone du fil 2363 : OpenHome, un égaliseur armé, l'opt-in
/// `dsp_progressif_reseau` coché (le consentement de #5080 reste exigé).
async fn banc(protocol_info: &'static str) -> (PlaybackOrchestrator, PlayRequest) {
    let port = renderer_openhome(protocol_info).await;
    let mut services = HashMap::new();
    services.insert("playlist".to_string(), "/Playlist".to_string());
    let mut registre = OutputRegistry::new();
    registre.register(Box::new(crate::outputs::openhome::OpenHomeOutput::new(
        "LUXMAN NT-07".into(),
        ID.into(),
        "127.0.0.1".into(),
        port,
        services,
        None,
        HashMap::new(),
    )));
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let orch = PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(registre)),
        None,
    );
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("LUXMAN", Some("openhome"), Some(ID))
        .unwrap();
    let settings = SettingsRepo::with_backend(orch.db.clone());
    let profil = crate::audio::eq::EqProfile {
        enabled: true,
        bands: vec![crate::audio::eq::EqBandSpec {
            freq: 1000.0,
            gain: 3.0,
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
    (orch, requete_qobuz(zone_id))
}

/// Résout le titre Qobuz 96/24 contre un CDN muet, en `budget` au plus.
async fn resoudre(
    orch: &PlaybackOrchestrator,
    req: &PlayRequest,
    budget: Duration,
) -> Option<Result<(String, Option<String>, String, Option<u64>), String>> {
    let (url, liberer) = cdn_muet().await;
    let flux = flux_qobuz(url, 96_000, 24);
    let r = tokio::time::timeout(
        budget,
        orch.relayer_le_flux(
            req,
            "441697196",
            "qobuz",
            &flux,
            info_amont(&flux),
            false,
            "flac".into(),
            None,
            None,
        ),
    )
    .await
    .ok();
    let _ = liberer.send(true);
    r
}

/// 🔴 Le témoin de #6718. Rouge avant : la sonde LPCM répondait « inconnu »
/// pour OpenHome, la résolution partait au pré-transcodage et attendait le
/// dernier octet du CDN — le délai expire.
#[tokio::test(flavor = "multi_thread")]
async fn un_renderer_openhome_qui_annonce_le_lpcm_recoit_le_wav_progressif() {
    let (orch, req) = banc(
        "http-get:*:audio/L16;rate=44100;channels=2:*,\
         http-get:*:audio/L24;rate=96000;channels=2:*,http-get:*:audio/flac:*",
    )
    .await;
    let (adresse, session, mime, _) = resoudre(&orch, &req, Duration::from_secs(5))
        .await
        .expect(
            "la résolution a attendu le CDN : vers OpenHome, le flux traité part \
             encore par le pré-transcodage du fichier entier (#6718)",
        )
        .expect("la résolution doit réussir");
    assert_eq!(mime, "audio/wav", "WAV progressif attendu");
    assert!(adresse.ends_with(".wav"), "{adresse}");
    let wire = orch
        .streamer
        .stream_output_wire(session.as_deref().unwrap())
        .await
        .expect("la session doit exister");
    assert_eq!((wire.sample_rate, wire.bit_depth), (96_000, 24));
}

/// Le repli : un renderer OpenHome qui n'annonce aucun LPCM garde le
/// comportement actuel (le pré-transcodage, qui attend ici le CDN muet).
#[tokio::test(flavor = "multi_thread")]
async fn un_renderer_openhome_sans_lpcm_garde_le_pre_transcodage() {
    let (orch, req) = banc("http-get:*:audio/flac:*,http-get:*:audio/mpeg:*").await;
    assert!(
        resoudre(&orch, &req, Duration::from_secs(2))
            .await
            .is_none(),
        "sans LPCM annoncé, aucun WAV progressif : la résolution attend le \
         pré-transcodage"
    );
}
