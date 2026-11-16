//! #8738 — vrai client SOAP face à un renderer synthétique, aucune sortie audio.
use crate::db::backend::DbBackend;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::{AudioStreamer, StreamInfo};
use crate::orchestrator::PlaybackOrchestrator;
use crate::playback::{NowPlaying, PlayState, PlaybackManager};
use std::sync::{Arc, Mutex};

#[derive(Default, Debug)]
struct Etat {
    appels: Vec<String>,
    uri: String,
    joue: bool,
    refus_restant: bool,
    position: String,
}
struct Serveur(tokio::task::JoinHandle<()>);
impl Drop for Serveur {
    fn drop(&mut self) {
        self.0.abort();
    }
}
fn champ(xml: &str, nom: &str) -> String {
    xml.split_once(&format!("<{nom}>"))
        .and_then(|(_, v)| v.split_once(&format!("</{nom}>")))
        .map(|(v, _)| v.replace("&amp;", "&"))
        .unwrap_or_default()
}
async fn renderer(code: u16) -> (crate::outputs::dlna::DlnaOutput, Arc<Mutex<Etat>>, Serveur) {
    let state = Arc::new(Mutex::new(Etat {
        refus_restant: code != 0,
        position: "00:00:00".into(),
        ..Default::default()
    }));
    let shared = state.clone();
    let app = axum::Router::new().route("/control", axum::routing::post(
        move |headers: axum::http::HeaderMap, body: String| {
            let shared = shared.clone();
            async move {
                let action = headers.get("SOAPAction").unwrap().to_str().unwrap()
                    .trim_matches('"').rsplit('#').next().unwrap().to_string();
                let mut state = shared.lock().unwrap();
                state.appels.push(action.clone());
                if action == "Play" && state.refus_restant {
                    state.refus_restant = false;
                    return (axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>701</errorCode><errorDescription>Transition not available</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#.replace("<errorCode>701</errorCode>", &format!("<errorCode>{code}</errorCode>")).replace("Transition not available", if code == 701 { "Transition not available" } else { "Illegal MIME-type" }));
                }
                let fields = match action.as_str() {
                    "SetAVTransportURI" => { state.uri = champ(&body, "CurrentURI"); String::new() },
                    "Play" => { state.joue = true; String::new() },
                    "Stop" => { state.joue = false; String::new() },
                    "Seek" => { state.position = champ(&body, "Target"); String::new() },
                    "GetTransportInfo" => format!("<CurrentTransportState>{}</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed>", if state.joue { "PLAYING" } else { "STOPPED" }),
                    "GetPositionInfo" => format!("<Track>1</Track><TrackDuration>00:04:05</TrackDuration><TrackURI>{}</TrackURI><RelTime>{}</RelTime><AbsTime>{}</AbsTime><RelCount>0</RelCount><AbsCount>0</AbsCount>", state.uri.replace('&', "&amp;"), state.position, state.position),
                    "GetMediaInfo" => format!("<NrTracks>1</NrTracks><MediaDuration>00:04:05</MediaDuration><CurrentURI>{}</CurrentURI>", state.uri.replace('&', "&amp;")),
                    "GetProtocolInfo" => "<Source></Source><Sink>http-get:*:audio/flac:*</Sink>".into(),
                    "GetVolume" => "<CurrentVolume>50</CurrentVolume>".into(),
                    "GetMute" => "<CurrentMute>0</CurrentMute>".into(),
                    _ => String::new(),
                };
                (axum::http::StatusCode::OK, format!(r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:{action}Response xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">{fields}</u:{action}Response></s:Body></s:Envelope>"#))
            }
        }
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    let control = format!("{host}/control");
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (
        crate::outputs::dlna::DlnaOutput::new(
            "Renderer 8738".into(),
            "uuid:8738".into(),
            host,
            control.clone(),
            control.clone(),
            Some(control),
        ),
        state,
        Serveur(task),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reprise_8738_un_701_recharge_l_uri_et_retrouve_la_position() {
    let sqlite = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    sqlite.init_schema().unwrap();
    crate::db::migrations::run_migrations(&sqlite).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(sqlite);
    let orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    );
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("piste.flac");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test.flac"),
        &file,
    )
    .unwrap();
    let chemin = file.to_string_lossy().into_owned();
    db.execute("INSERT INTO tracks (id,title,file_path,format,duration_ms,sample_rate,bit_depth,channels) VALUES (8738,'Reprise',?,'flac',245000,44100,16,2)", &[&chemin]).unwrap();
    let zone = ZoneRepo::with_backend(db)
        .create("Banc 8738", Some("dlna"), Some("uuid:8738"))
        .unwrap();
    let (output, state, _server) = renderer(701).await;
    orch.outputs.lock().await.register(Box::new(output));
    let sid = orch
        .streamer
        .create_proxy_session(
            StreamInfo {
                format: "flac".into(),
                mime_type: "audio/flac".into(),
                sample_rate: 44100,
                bit_depth: 16,
                channels: 2,
                ..Default::default()
            },
            "http://127.0.0.1:9/ancienne-session.flac".into(),
            false,
        )
        .await;
    orch.playback
        .play(
            zone,
            NowPlaying {
                title: "Reprise".into(),
                source: "local".into(),
                track_id: Some(8738),
                stream_id: Some(sid.clone()),
                duration_ms: 245000,
                ..Default::default()
            },
        )
        .await;
    orch.playback.update_position(zone, 40_000).await;
    orch.playback.pause(zone).await;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        orch.resume(zone, Some("uuid:8738")),
    )
    .await
    .unwrap();
    assert!(
        result.is_ok(),
        "#8738 : le 701 doit rétablir la piste : {result:?}"
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if state.lock().unwrap().position == "0:00:40" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|e| {
        panic!(
            "Seek à la position mémorisée : {e}; {:?}",
            state.lock().unwrap()
        )
    });
    let current = orch.playback.get_state(zone).await;
    assert_eq!(current.state, PlayState::Playing);
    assert_eq!(current.position_ms, 40_000);
    let state = state.lock().unwrap();
    let first_play = state.appels.iter().position(|a| a == "Play").unwrap();
    let set_uri = state
        .appels
        .iter()
        .position(|a| a == "SetAVTransportURI")
        .unwrap();
    let last_play = state.appels.iter().rposition(|a| a == "Play").unwrap();
    let seek = state.appels.iter().position(|a| a == "Seek").unwrap();
    assert!(
        first_play < set_uri && set_uri < last_play && last_play < seek,
        "{:?}",
        state.appels
    );
    assert_eq!(
        state
            .appels
            .iter()
            .filter(|a| *a == "SetAVTransportURI")
            .count(),
        1
    );
    assert!(!state.uri.is_empty());
}

#[tokio::test]
async fn reprise_8738_un_succes_ou_un_autre_refus_ne_signale_pas_de_session_perdue() {
    use crate::outputs::traits::OutputTarget;
    for code in [0, 714] {
        let (output, state, _server) = renderer(code).await;
        let result = output.resume().await;
        assert_eq!(result.is_ok(), code == 0);
        assert!(
            !output.device_released_on_pause(),
            "seul 701 autorise le rétablissement : {code}"
        );
        assert_eq!(state.lock().unwrap().appels, vec!["Play"]);
    }
}
