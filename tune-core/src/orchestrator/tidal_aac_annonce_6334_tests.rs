//! #6334 — un AAC Tidal décodé pour la sortie s'annonce AAC, pas « WAV CD ».
//!
//! pissa7, fil 2265 : « Keith Don't Go » (Tidal 29113109) arrive en AAC
//! 320 kbit/s. L'écran montrait **WAV · 44.1 kHz · 16-bit**, un badge **CD**
//! et **Bit-Perfect**. `resolve_stream` décodait l'AAC en WAV pour la sortie,
//! et `composer_le_now_playing` retombait sur le MIME du conteneur servi
//! (`audio/wav`) pour nommer le format de la SOURCE. Il y ajoutait les
//! 16 bits que `tidal.rs` prêtait à tout AAC.
//!
//! Le témoin passe par la porte publique (`resolve_stream`) avec le VRAI
//! `TidalService`. L'API est un faux Tidal en boucle locale (réponse
//! `playbackinfopostpaywall` rendue `HIGH`, manifeste BTS `mp4a.40.2`), et le
//! « CDN », le même serveur, sert un vrai AAC (`tests/fixtures/test.m4a`).
//! Aucun appel réseau réel, aucun son : la sortie locale n'est jamais ouverte.

use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine as _;

use super::transport::Habillage;
use super::{PlayRequest, PlaybackOrchestrator, QualitePreArmee, profondeur_du_fil};
use crate::db::backend::DbBackend;
use crate::db::zone_repo::ZoneRepo;
use crate::streaming::tidal::TidalService;

const PISTE: &str = "29113109";

/// Le faux Tidal : lecture `HIGH` en BTS AAC, fiche catalogue (qui, elle,
/// annonce LOSSLESS, comme le catalogue réel), et l'AAC lui-même.
async fn faux_tidal() -> String {
    use axum::extract::{Query, State};
    use axum::response::IntoResponse;
    use axum::routing::get;

    async fn lecture(
        State(base): State<String>,
        Query(_q): Query<HashMap<String, String>>,
    ) -> axum::response::Response {
        let manifeste = serde_json::json!({
            "mimeType": "audio/mp4",
            "codecs": "mp4a.40.2",
            "encryptionType": "NONE",
            "urls": [format!("{base}/cdn/piste.m4a")],
        })
        .to_string();
        axum::Json(serde_json::json!({
            "trackId": 29113109,
            "assetPresentation": "FULL",
            "audioMode": "STEREO",
            "audioQuality": "HIGH",
            "manifestMimeType": "application/vnd.tidal.bts",
            "manifest": base64::engine::general_purpose::STANDARD.encode(manifeste),
        }))
        .into_response()
    }

    async fn fiche() -> axum::response::Response {
        axum::Json(serde_json::json!({
            "id": 29113109,
            "title": "Keith Don't Go",
            "duration": 1,
            "audioQuality": "LOSSLESS",
            "artist": {"id": 1, "name": "Nils Lofgren"},
            "album": {"id": 2, "title": "Acoustic Live"},
        }))
        .into_response()
    }

    async fn cdn() -> axum::response::Response {
        let aac = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/test.m4a"
        ))
        .unwrap();
        ([(axum::http::header::CONTENT_TYPE, "audio/mp4")], aac).into_response()
    }

    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let base = format!("http://{}", ecoute.local_addr().expect("adresse locale"));
    let app = axum::Router::new()
        .route("/tracks/{id}/playbackinfopostpaywall", get(lecture))
        .route("/tracks/{id}", get(fiche))
        .route("/cdn/piste.m4a", get(cdn))
        .with_state(base.clone());
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    base
}

fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    )
}

/// Une zone locale : elle exige du WAV, donc l'AAC y est décodé.
async fn zone_locale_et_demande() -> (PlaybackOrchestrator, PlayRequest) {
    let orch = orchestrateur();
    let base = faux_tidal().await;
    orch.services
        .lock()
        .await
        .register(Box::new(TidalService::pour_un_faux_tidal(base)));
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Casque", Some("local"), Some("local:Casque factice"))
        .unwrap();
    let req = PlayRequest {
        zone_id,
        output_device_id: Some("local:Casque factice".into()),
        source: Some("tidal".into()),
        source_id: Some(PISTE.into()),
        title: Some("Keith Don't Go".into()),
        duration_ms: Some(1_000),
        ..Default::default()
    };
    (orch, req)
}

#[tokio::test]
async fn un_aac_tidal_decode_en_wav_s_annonce_aac_320_sans_profondeur() {
    let (orch, req) = zone_locale_et_demande().await;
    let resolu = orch.resolve_stream(&req).await.expect("flux résolu");

    // Le conteneur servi est bien du WAV : c'est la sortie qui l'exige.
    assert_eq!(resolu.mime_type, "audio/wav");

    let np = orch.composer_le_now_playing(
        &req,
        &resolu,
        &Habillage {
            album: None,
            cover_path: None,
        },
    );
    assert_eq!(
        np.format.as_deref(),
        Some("aac"),
        "le format annoncé est celui de la SOURCE (AAC), pas le conteneur servi (WAV)"
    );
    assert_eq!(
        np.bit_depth, None,
        "un AAC n'a pas de profondeur : 16 bits est celle d'un CD"
    );
    assert_eq!(np.bitrate_kbps, Some(320), "le débit nommé par Tidal HIGH");
    assert_eq!(np.sample_rate, Some(44_100));

    // La piste atteinte par enchaînement dit la même chose (#3365).
    let pre_armee = QualitePreArmee::d_un_flux_de_service("tidal", Some(PISTE), &resolu)
        .expect("un flux de session");
    assert_eq!(pre_armee.format.as_deref(), Some("aac"));
    assert_eq!(pre_armee.bit_depth, None);
    assert_eq!(pre_armee.bitrate_kbps, Some(320));
}

/// Relecture rc7 du 20/10 : la profondeur ôtée à la SOURCE ne doit pas l'être
/// au FIL. Un AAC décodé en WAV pour une sortie réseau part en 16 bits ; le
/// `<res bitsPerSample>` du DIDL et l'annonce SlimProto le disent.
///
/// La sortie est un endpoint OAAT : le faux CDN est en `http://`, et seul le
/// bras OAAT/local décode sans exiger `https://`. Il produit le même WAV
/// 16 bits que le canal AAC d'un renderer DLNA (`cap_output_bit_depth(0)`).
#[tokio::test]
async fn un_aac_decode_pour_une_sortie_reseau_annonce_les_16_bits_du_fil() {
    use crate::outputs::mock::MockOutput;
    const SORTIE: &str = "oaat:salon-6334";

    let orch = orchestrateur();
    let base = faux_tidal().await;
    orch.services
        .lock()
        .await
        .register(Box::new(TidalService::pour_un_faux_tidal(base)));
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon", Some("oaat"), Some(SORTIE))
        .unwrap();
    orch.outputs
        .lock()
        .await
        .register(Box::new(MockOutput::new(SORTIE, "Salon").with_type("oaat")));
    let req = PlayRequest {
        zone_id,
        output_device_id: Some(SORTIE.into()),
        source: Some("tidal".into()),
        source_id: Some(PISTE.into()),
        title: Some("Keith Don't Go".into()),
        duration_ms: Some(1_000),
        ..Default::default()
    };

    // La résolution seule : la source n'a pas de profondeur, le fil en a une.
    let resolu = orch.resolve_stream(&req).await.expect("flux résolu");
    assert_eq!(
        resolu.mime_type, "audio/wav",
        "l'AAC est décodé pour la sortie"
    );
    assert_eq!(
        resolu.bit_depth, None,
        "la SOURCE AAC n'a pas de profondeur"
    );
    assert_eq!(profondeur_du_fil(&resolu), Some(16));

    // La lecture : ce que la sortie reçoit dans `PlayMedia`.
    orch.play(req).await.expect("lecture lancée");
    let outputs = orch.outputs.lock().await;
    let sortie = outputs.get(SORTIE).expect("sortie enregistrée");
    let guard = sortie.lock().await;
    let mock = guard
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("mock output");
    assert_eq!(
        mock.last_play_bit_depth().await,
        Some(16),
        "le WAV servi est en 16 bits : PlayMedia doit le dire, sans quoi le \
         DIDL perd bitsPerSample"
    );
}

/// Le même fil, à l'enchaînement : le `SetNext` d'une piste pré-armée passe
/// par `resolve_queue_item_url`, qui doit porter la profondeur du fil.
#[tokio::test]
async fn la_piste_pre_armee_annonce_aussi_les_16_bits_du_fil() {
    use crate::db::play_queue_repo::{PlayQueueRepo, StreamingQueueItem};
    const SORTIE: &str = "oaat:salon-6334";

    let orch = orchestrateur();
    let base = faux_tidal().await;
    orch.services
        .lock()
        .await
        .register(Box::new(TidalService::pour_un_faux_tidal(base)));
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon", Some("oaat"), Some(SORTIE))
        .unwrap();
    let file: Vec<StreamingQueueItem> = vec![(
        PISTE.to_string(),
        "Keith Don't Go".to_string(),
        "Nils Lofgren".to_string(),
        None,
        None,
        1_000i64,
        Some("tidal".to_string()),
        Some(1),
        None,
    )];
    PlayQueueRepo::with_backend(orch.db.clone())
        .set_streaming_queue(zone_id, &file)
        .unwrap();

    let pre_arme = orch
        .resolve_queue_item_url(zone_id, 0)
        .await
        .expect("piste pré-armée");
    assert_eq!(pre_arme.mime_type, "audio/wav");
    assert_eq!(
        pre_arme.bit_depth,
        Some(16),
        "le SetNext part vers la sortie : il annonce les 16 bits du WAV servi"
    );
}

/// Ce qui ne bouge pas : un AAC relayé tel quel n'a toujours aucune
/// profondeur, et une profondeur connue passe inchangée.
#[test]
fn la_profondeur_du_fil_ne_s_invente_pas() {
    let flux = |mime: &str, format: Option<&str>, bd: Option<u32>| super::ResolvedStream {
        url: String::new(),
        mime_type: mime.into(),
        title: String::new(),
        artist: None,
        album: None,
        duration_ms: None,
        source: "tidal".into(),
        cover_url: None,
        stream_id: None,
        file_size: None,
        sample_rate: Some(44_100),
        bit_depth: bd,
        channels: Some(2),
        origin_url: None,
        bitrate_kbps: None,
        format_source: format.map(String::from),
    };
    assert_eq!(
        profondeur_du_fil(&flux("audio/mp4", Some("aac"), None)),
        None
    );
    assert_eq!(
        profondeur_du_fil(&flux("audio/wav", Some("flac"), Some(24))),
        Some(24)
    );
    assert_eq!(
        profondeur_du_fil(&flux("audio/wav", None, None)),
        None,
        "sans codec source nommé, rien n'est prêté"
    );
    assert_eq!(
        profondeur_du_fil(&flux("audio/L16;rate=44100", Some("opus"), None)),
        Some(16)
    );
}
