//! #8956 — **Atoll ST200 : un DSD de serveur UPnP part dans l'orthographe
//! que le renderer publie pour son conteneur.**
//!
//! # Le défaut (Atoll, 17/11, rc11 Windows)
//!
//! Un DSD64 de MinimServer (`.../MAGNIFICAT_04.dsf`, MIME source
//! `application/x-dsd`) vers un Atoll ST200 (StreamUnlimited S810), zone en
//! DSD `auto` : relais brut par Tune (`/stream/<id>.dsf`), puis
//!
//! - essai 1 : `application/x-dsd` → 714 « Illegal MIME-type » — le Sink
//!   (82 entrées) publie `audio/x-dsf` et `audio/x-dff`, pas ce générique ;
//! - essai 2 : `audio/wav;DLNA.ORG_PN=LPCM` → 714 — une ÉTIQUETTE PCM posée
//!   sur des octets DSF : le ST200 fait un `HEAD` sur l'adresse (deux
//!   `stream_head_request format=dsf` au journal) et y lit le
//!   `Content-Type` DSD du flux.
//!
//! # Le banc
//!
//! Le faux ST200 publie le Sink RÉEL relevé au journal du testeur
//! (`dlna_set_uri_illegal_mime_714 … sink=[…]`). À `SetAVTransportURI`, il
//! fait ce que fait le vrai : MIME du `protocolInfo` absent du Sink → 714 ;
//! `Content-Type` du flux (ce que le serveur de flux de Tune rendrait au
//! `HEAD`, `content_type_du_flux`) différent du MIME annoncé → 714. Aucun son :
//! rien ne lit le flux.
//!
//! Contre-épreuve : sur la base sans le correctif, le premier témoin voit
//! `application/x-dsd` à la résolution et deux refus à la pose ; le second
//! voit l'étiquette `audio/wav` en second essai.

use crate::db::backend::DbBackend;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::{PlayRequest, PlaybackOrchestrator};
use crate::outputs::dlna::{DlnaOutput, parse_dsd_capability};
use crate::outputs::registry::OutputRegistry;
use crate::outputs::traits::{OutputTarget, PlayMedia};
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;
use axum::Router;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::post;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

const ID: i64 = 89_561;
const RENDERER: &str = "uuid:ab598f16-4c2f-41c0-98f7-banc-8956";

/// Le Sink du ST200, tel que le journal du testeur le porte (17/11 14:28).
const SINK_ST200: [&str; 82] = [
    "http-get:*:text/xml:DLNA.ORG_PN=DIDL_S",
    "http-get:*:audio/vnd.dlna.adts:DLNA.ORG_PN=HEAACv2_L2_320",
    "http-get:*:audio/x-ms-wma:DLNA.ORG_PN=WMAFULL",
    "http-get:*:audio/x-ms-wma:DLNA.ORG_PN=WMABASE",
    "http-get:*:audio/3gpp:DLNA.ORG_PN=HEAACv2_L2_320",
    "http-get:*:audio/mp4:DLNA.ORG_PN=HEAACv2_L2_320",
    "http-get:*:audio/l16;rate=48000;channels=1:DLNA.ORG_PN=LPCM",
    "http-get:*:audio/l16;rate=48000;channels=2:DLNA.ORG_PN=LPCM",
    "http-get:*:audio/l16;rate=44100;channels=1:DLNA.ORG_PN=LPCM",
    "http-get:*:audio/l16;rate=44100;channels=2:DLNA.ORG_PN=LPCM",
    "http-get:*:audio/mpeg:DLNA.ORG_PN=MP3",
    "rtsp-rtp-udp:*:text/xml:DLNA.ORG_PN=DIDL_S",
    "rtsp-rtp-udp:*:audio/vnd.dlna.adts:DLNA.ORG_PN=HEAACv2_L2_320",
    "rtsp-rtp-udp:*:audio/x-ms-wma:DLNA.ORG_PN=WMAFULL",
    "rtsp-rtp-udp:*:audio/x-ms-wma:DLNA.ORG_PN=WMABASE",
    "rtsp-rtp-udp:*:audio/3gpp:DLNA.ORG_PN=HEAACv2_L2_320",
    "rtsp-rtp-udp:*:audio/mp4:DLNA.ORG_PN=HEAACv2_L2_320",
    "rtsp-rtp-udp:*:audio/l16;rate=48000;channels=1:DLNA.ORG_PN=LPCM",
    "rtsp-rtp-udp:*:audio/l16;rate=48000;channels=2:DLNA.ORG_PN=LPCM",
    "rtsp-rtp-udp:*:audio/l16;rate=44100;channels=1:DLNA.ORG_PN=LPCM",
    "rtsp-rtp-udp:*:audio/l16;rate=44100;channels=2:DLNA.ORG_PN=LPCM",
    "rtsp-rtp-udp:*:audio/mpeg:DLNA.ORG_PN=MP3",
    "http-get:*:audio/l16;rate=48000;channels=2:*",
    "http-get:*:audio/aiff:*",
    "http-get:*:audio/x-flac:*",
    "http-get:*:audio/wave:*",
    "http-get:*:audio/3gpp:*",
    "http-get:*:audio/vnd.dlna.adts:*",
    "http-get:*:text/xml:*",
    "http-get:*:audio/flac:*",
    "http-get:*:audio/x-m4a:*",
    "http-get:*:audio/x-wav:*",
    "http-get:*:audio/vorbis:*",
    "http-get:*:audio/x-aac:*",
    "http-get:*:audio/ogg:*",
    "http-get:*:audio/x-aiff:*",
    "http-get:*:audio/x-dsf:*",
    "http-get:*:audio/l16;rate=44100;channels=1:*",
    "http-get:*:audio/m4a:*",
    "http-get:*:audio/wav:*",
    "http-get:*:audio/x-ms-wma:*",
    "http-get:*:application/ogg:*",
    "http-get:*:audio/l16;rate=44100;channels=2:*",
    "http-get:*:audio/x-dff:*",
    "http-get:*:audio/mp4:*",
    "http-get:*:audio/aac:*",
    "http-get:*:audio/l16;rate=48000;channels=1:*",
    "http-get:*:audio/mpeg:*",
    "http-get:*:audio/mpegurl:*",
    "http-get:*:audio/x-mpegurl:*",
    "http-get:*:video/mpegurl:*",
    "http-get:*:video/x-mpegurl:*",
    "rtsp-rtp-udp:*:audio/l16;rate=48000;channels=2:*",
    "rtsp-rtp-udp:*:audio/aiff:*",
    "rtsp-rtp-udp:*:audio/x-flac:*",
    "rtsp-rtp-udp:*:audio/wave:*",
    "rtsp-rtp-udp:*:audio/3gpp:*",
    "rtsp-rtp-udp:*:audio/vnd.dlna.adts:*",
    "rtsp-rtp-udp:*:text/xml:*",
    "rtsp-rtp-udp:*:audio/flac:*",
    "rtsp-rtp-udp:*:audio/x-m4a:*",
    "rtsp-rtp-udp:*:audio/x-wav:*",
    "rtsp-rtp-udp:*:audio/vorbis:*",
    "rtsp-rtp-udp:*:audio/x-aac:*",
    "rtsp-rtp-udp:*:audio/ogg:*",
    "rtsp-rtp-udp:*:audio/x-aiff:*",
    "rtsp-rtp-udp:*:audio/x-dsf:*",
    "rtsp-rtp-udp:*:audio/l16;rate=44100;channels=1:*",
    "rtsp-rtp-udp:*:audio/m4a:*",
    "rtsp-rtp-udp:*:audio/wav:*",
    "rtsp-rtp-udp:*:audio/x-ms-wma:*",
    "rtsp-rtp-udp:*:application/ogg:*",
    "rtsp-rtp-udp:*:audio/l16;rate=44100;channels=2:*",
    "rtsp-rtp-udp:*:audio/x-dff:*",
    "rtsp-rtp-udp:*:audio/mp4:*",
    "rtsp-rtp-udp:*:audio/aac:*",
    "rtsp-rtp-udp:*:audio/l16;rate=48000;channels=1:*",
    "rtsp-rtp-udp:*:audio/mpeg:*",
    "rtsp-rtp-udp:*:audio/mpegurl:*",
    "rtsp-rtp-udp:*:audio/x-mpegurl:*",
    "rtsp-rtp-udp:*:video/mpegurl:*",
    "rtsp-rtp-udp:*:video/x-mpegurl:*",
];

fn sink_st200() -> Vec<String> {
    SINK_ST200.iter().map(|s| s.to_string()).collect()
}

// ─── Le faux ST200 ──────────────────────────────────────────────────────────

#[derive(Clone)]
struct St200 {
    /// MIME de base publiés (minuscules).
    publies: Arc<Vec<String>>,
    /// MIME de la SESSION de flux de Tune (ce que le `HEAD` rendrait sans
    /// orthographe annoncée par la sortie).
    mime_session: Arc<String>,
    /// MIME du `protocolInfo` de chaque `SetAVTransportURI` reçu.
    poses: Arc<Mutex<Vec<String>>>,
    get_protocol_info: Arc<AtomicU32>,
    current_uri: Arc<Mutex<String>>,
}

fn enveloppe(corps: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>{corps}</s:Body></s:Envelope>"#
    )
}

fn refus_714() -> axum::response::Response {
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        enveloppe(
            r#"<s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>714</errorCode><errorDescription>Illegal MIME-type</errorDescription></UPnPError></detail></s:Fault>"#,
        ),
    )
        .into_response()
}

fn action(corps: &str) -> String {
    corps
        .split("<u:")
        .nth(1)
        .and_then(|r| r.split([' ', '>']).next())
        .unwrap_or_default()
        .to_string()
}

async fn av(State(s): State<St200>, corps: String) -> axum::response::Response {
    match action(&corps).as_str() {
        "SetAVTransportURI" => {
            let annonce = crate::outputs::dlna_repli_set_uri::protocol_info_du_didl(&corps)
                .and_then(|p| p.split(':').nth(2).map(|m| m.to_ascii_lowercase()))
                .unwrap_or_default();
            s.poses.lock().await.push(annonce.clone());
            if !s.publies.contains(&annonce) {
                return refus_714();
            }
            // Le `HEAD` du ST200 : le `Content-Type` que servirait Tune.
            let uri = crate::outputs::dlna::extract_tag(&corps, "CurrentURI").unwrap_or_default();
            if let Some(sid) = crate::http::streamer::stream_id_de_l_url(&uri) {
                let servi = crate::http::streamer::content_type_du_flux(sid, &s.mime_session);
                if !servi.eq_ignore_ascii_case(&annonce) {
                    return refus_714();
                }
            }
            *s.current_uri.lock().await = uri;
            enveloppe("<u:SetAVTransportURIResponse/>").into_response()
        }
        "GetMediaInfo" => {
            let uri = s.current_uri.lock().await.clone();
            enveloppe(&format!(
                "<u:GetMediaInfoResponse><CurrentURI>{uri}</CurrentURI></u:GetMediaInfoResponse>"
            ))
            .into_response()
        }
        "GetPositionInfo" => {
            let uri = s.current_uri.lock().await.clone();
            enveloppe(&format!(
                "<u:GetPositionInfoResponse><TrackURI>{uri}</TrackURI><RelTime>0:00:00</RelTime><TrackDuration>0:04:00</TrackDuration></u:GetPositionInfoResponse>"
            ))
            .into_response()
        }
        "GetTransportInfo" => enveloppe(
            "<u:GetTransportInfoResponse><CurrentTransportState>STOPPED</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed></u:GetTransportInfoResponse>",
        )
        .into_response(),
        autre => enveloppe(&format!("<u:{autre}Response/>")).into_response(),
    }
}

async fn cm(State(s): State<St200>, corps: String) -> axum::response::Response {
    if action(&corps) == "GetProtocolInfo" {
        s.get_protocol_info.fetch_add(1, Ordering::Relaxed);
        return enveloppe(&format!(
            "<u:GetProtocolInfoResponse><Source></Source><Sink>{}</Sink></u:GetProtocolInfoResponse>",
            SINK_ST200.join(",")
        ))
        .into_response();
    }
    enveloppe("<u:Response/>").into_response()
}

async fn st200(udn: &str, mime_session: &str) -> (DlnaOutput, St200, tokio::task::JoinHandle<()>) {
    let etat = St200 {
        publies: Arc::new(
            SINK_ST200
                .iter()
                .filter_map(|e| e.split(':').nth(2))
                .map(|m| m.split(';').next().unwrap_or(m).trim().to_ascii_lowercase())
                .collect(),
        ),
        mime_session: Arc::new(mime_session.to_string()),
        poses: Arc::default(),
        get_protocol_info: Arc::default(),
        current_uri: Arc::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new()
        .route("/av", post(av))
        .route("/rc", post(|| async { enveloppe("<u:R/>") }))
        .route("/cm", post(cm))
        .with_state(etat.clone());
    let vie = tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let sortie = DlnaOutput::new(
        "ST200 RD".into(),
        udn.into(),
        "127.0.0.1".into(),
        format!("http://127.0.0.1:{port}/av"),
        format!("http://127.0.0.1:{port}/rc"),
        Some(format!("http://127.0.0.1:{port}/cm")),
    );
    (sortie, etat, vie)
}

// ─── L'orchestrateur ────────────────────────────────────────────────────────

fn orchestrateur(res: &str) -> (PlaybackOrchestrator, i64) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    db.execute_batch(&format!(
        "INSERT INTO tracks (id,title,source,source_id,format,sample_rate,bit_depth,channels,duration_ms) \
         VALUES ({ID},'MAGNIFICAT 4. Et misericordia','upnp','minimserver|8956','dsd',2822400,1,2,240000); \
         INSERT INTO track_metadata (track_id,key,value) \
         VALUES ({ID},'upnp_res_url','{res}');"
    ))
    .unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let zones = ZoneRepo::with_backend(db.clone());
    let zone_id = zones
        .create("ST200 RD", Some("dlna"), Some(RENDERER))
        .unwrap();
    zones.update_dsd_mode(zone_id, "auto").unwrap();
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

fn demande(zone_id: i64) -> PlayRequest {
    PlayRequest {
        zone_id,
        output_device_id: Some(RENDERER.into()),
        track_id: Some(ID),
        source: None,
        source_id: None,
        title: Some("MAGNIFICAT 4. Et misericordia".into()),
        artist_name: Some("Arnesen".into()),
        album_title: Some("MAGNIFICAT".into()),
        cover_url: None,
        duration_ms: Some(240_000),
        seek_ms: None,
        temp_file_path: None,
        sample_rate: Some(2_822_400),
        bit_depth: Some(1),
        media_format: None,
        track_number: None,
        disc_number: None,
        album_ref: None,
    }
}

/// Résout la piste pour la zone du ST200 (son Sink réel en cache, comme après
/// `dsd_passthrough_decide … annonce_du_renderer=Some(true)`), puis la pose
/// sur le faux ST200. Rend (MIME résolu, MIME de chaque pose, sondes du Sink,
/// issue de la pose).
async fn resoudre_et_poser(res: &str, udn: &str) -> (String, Vec<String>, u32, Result<(), String>) {
    crate::outputs::dlna_repli_set_uri::oublier_profil(udn, "application/x-dsd");
    let (orch, zone_id) = orchestrateur(res);
    orch.retenir_annonce_dsd_pour_banc(RENDERER, parse_dsd_capability(&sink_st200()))
        .await;
    let resolu = orch.resolve_stream(&demande(zone_id)).await.unwrap();
    let sid = resolu
        .stream_id
        .clone()
        .expect("relais brut par une session de Tune");
    let mime_session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(&sid)
        .map(|s| s.info.mime_type.clone())
        .expect("la session existe");
    let (sortie, etat, vie) = st200(udn, &mime_session).await;
    let media = PlayMedia {
        url: &resolu.url,
        mime_type: &resolu.mime_type,
        title: Some(resolu.title.as_str()),
        duration_ms: Some(240_000),
        sample_rate: resolu.sample_rate,
        bit_depth: resolu.bit_depth,
        ..Default::default()
    };
    let issue = sortie.play_media(&media).await;
    vie.abort();
    let poses = etat.poses.lock().await.clone();
    (
        resolu.mime_type,
        poses,
        etat.get_protocol_info.load(Ordering::Relaxed),
        issue,
    )
}

/// **Le défaut, en un test.** Le `.dsf` de MinimServer part en `audio/x-dsf`
/// dès le PREMIER `SetAVTransportURI`, accepté : pas de 714, pas même une
/// sonde du Sink à la pose.
#[tokio::test]
async fn st200_un_dsf_de_minimserver_part_en_x_dsf_au_premier_essai() {
    let res = "http://192.168.1.151:9790/minimserver/*/music/DSD/DSD64/2L-106_stereo_DSD-2822k_MAGNIFICAT_04.dsf";
    let (mime, poses, sondes, issue) = resoudre_et_poser(res, "uuid:banc-8956-dsf").await;
    assert_eq!(mime, "audio/x-dsf", "orthographe du Sink pour un DSF");
    assert_eq!(
        poses,
        vec!["audio/x-dsf".to_string()],
        "une seule pose, en audio/x-dsf"
    );
    assert_eq!(sondes, 0, "aucun refus, donc aucune sonde du Sink");
    issue.expect("le ST200 accepte la pose");
}

/// Même chose pour un `.dff` : `audio/x-dff`, jamais l'orthographe du DSF.
#[tokio::test]
async fn st200_un_dff_de_minimserver_part_en_x_dff_au_premier_essai() {
    let res = "http://192.168.1.151:9790/minimserver/*/music/DSD/DSD128/piste.dff";
    let (mime, poses, sondes, issue) = resoudre_et_poser(res, "uuid:banc-8956-dff").await;
    assert_eq!(mime, "audio/x-dff");
    assert_eq!(poses, vec!["audio/x-dff".to_string()]);
    assert_eq!(sondes, 0);
    issue.expect("le ST200 accepte la pose");
}

/// Un serveur dont l'adresse ne nomme pas le conteneur (serveur natif
/// Synology, `.../item?id=…`) : Tune lit les quatre premiers octets
/// (`FRM8` → DFF) et annonce `audio/x-dff`.
#[tokio::test]
async fn st200_sans_extension_le_conteneur_se_lit_a_l_en_tete() {
    for (tete, attendu) in [(&b"FRM8"[..], "audio/x-dff"), (&b"DSD "[..], "audio/x-dsf")] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/media/item?id=8956",
            listener.local_addr().unwrap()
        );
        let corps: Vec<u8> = tete
            .iter()
            .copied()
            .chain(std::iter::repeat_n(0u8, 60))
            .collect();
        let serveur = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut requete = Vec::new();
                let mut octet = [0u8; 1];
                while !requete.ends_with(b"\r\n\r\n") {
                    if socket.read_exact(&mut octet).await.is_err() {
                        break;
                    }
                    requete.push(octet[0]);
                }
                let entete = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/x-dsd\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    corps.len()
                );
                let _ = socket.write_all(entete.as_bytes()).await;
                let _ = socket.write_all(&corps).await;
                let _ = socket.shutdown().await;
            }
        });
        let udn = format!("uuid:banc-8956-entete-{attendu}");
        let (mime, poses, _, issue) = resoudre_et_poser(&url, &udn).await;
        serveur.abort();
        assert_eq!(mime, attendu, "en-tête {:?}", std::str::from_utf8(tete));
        assert_eq!(poses, vec![attendu.to_string()]);
        issue.expect("le ST200 accepte la pose");
    }
}

/// **La reprise de la sortie, seule.** Un flux DSD posé sous le générique
/// `application/x-dsd` (renderer pas encore sondé à la résolution) : la
/// reprise du 714 prend l'orthographe du CONTENEUR que nomme l'adresse, et
/// ne pose JAMAIS une étiquette PCM sur des octets DSD (le `audio/wav;
/// DLNA.ORG_PN=LPCM` du journal du testeur).
#[tokio::test]
async fn st200_la_reprise_714_d_un_dsd_prend_le_conteneur_jamais_le_wav() {
    for (ext, attendu) in [("dsf", "audio/x-dsf"), ("dff", "audio/x-dff")] {
        let udn = format!("uuid:banc-8956-reprise-{ext}");
        crate::outputs::dlna_repli_set_uri::oublier_profil(&udn, "application/x-dsd");
        let sid = format!("banc8956reprise{ext}");
        let url = format!("http://127.0.0.1:9/stream/{sid}.{ext}");
        let (sortie, etat, vie) = st200(&udn, "application/x-dsd").await;
        let media = PlayMedia {
            url: &url,
            mime_type: "application/x-dsd",
            title: Some("MAGNIFICAT"),
            duration_ms: Some(240_000),
            ..Default::default()
        };
        let issue = sortie.play_media(&media).await;
        vie.abort();
        crate::http::streamer::oublier_mime_du_flux(&sid);
        let poses = etat.poses.lock().await.clone();
        assert!(
            !poses.iter().any(|m| m.contains("wav") || m.contains("l16")),
            "[{ext}] aucune étiquette PCM sur du DSD : {poses:?}"
        );
        assert_eq!(
            poses,
            vec!["application/x-dsd".to_string(), attendu.to_string()],
            "[{ext}] générique refusé, puis l'orthographe du conteneur"
        );
        issue.unwrap_or_else(|e| panic!("[{ext}] la reprise doit passer : {e}"));
    }
}
