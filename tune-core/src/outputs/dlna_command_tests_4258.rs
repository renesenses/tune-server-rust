use super::*;
use axum::{Router, http::StatusCode, routing::post};
use std::sync::Mutex;
use std::time::Duration;

const FAULT: &str = "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><s:Fault><detail><UPnPError><errorCode>701</errorCode><errorDescription>Transition not available</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>";

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
    fn subscribe(&self) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(self.clone())
                .with_ansi(false)
                .with_max_level(tracing::Level::INFO)
                .finish(),
        )
    }
}
struct Renderer {
    output: DlnaOutput,
    received: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Renderer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn renderer(status: StatusCode, response: &'static str, delay: Duration) -> Renderer {
    controlled_renderer(status, response, delay, None).await
}
async fn controlled_renderer(
    status: StatusCode,
    response: &'static str,
    delay: Duration,
    barrier: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
) -> Renderer {
    let received = Arc::new(Mutex::new(Vec::new()));
    let requests = received.clone();
    let app = Router::new().route(
        "/control",
        post(move |headers: axum::http::HeaderMap, _body: String| {
            let requests = requests.clone();
            let barrier = barrier.clone();
            async move {
                requests.lock().unwrap().push(
                    headers
                        .get("SOAPAction")
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned(),
                );
                if let Some((request_seen, reply_gate)) = barrier {
                    request_seen.notify_one();
                    reply_gate.notified().await;
                }
                tokio::time::sleep(delay).await;
                (status, response)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    let output = DlnaOutput::new(
        "Salon test".into(),
        "uuid:4258".into(),
        host.clone(),
        format!("{host}/control"),
        format!("{host}/control"),
        None,
    );
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Renderer {
        output,
        received,
        task,
    }
}
fn field<'a>(line: &'a str, name: &str) -> &'a str {
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("missing {name}: {line}"))
}
fn assert_pair(log: &str, action: &str, outcome: &str) {
    let start: Vec<_> = log
        .lines()
        .filter(|l| l.contains("dlna_command_sending"))
        .collect();
    let end: Vec<_> = log
        .lines()
        .filter(|l| l.contains("dlna_command_finished"))
        .collect();
    assert_eq!(start.len(), 1, "command must log exactly one start: {log}");
    assert_eq!(end.len(), 1, "command must log exactly one result: {log}");
    assert_eq!(
        field(start[0], "command_id"),
        field(end[0], "command_id"),
        "{log}"
    );
    assert!(start[0].contains(&format!("action=\"{action}\"")), "{log}");
    assert!(end[0].contains(&format!("action=\"{action}\"")), "{log}");
    assert!(end[0].contains(&format!("outcome=\"{outcome}\"")), "{log}");
    assert!(start[0].contains("device_id=uuid:4258"), "{log}");
    assert!(end[0].contains("device_id=uuid:4258"), "{log}");
    assert!(
        log.find("dlna_command_sending") < log.find("dlna_command_finished"),
        "{log}"
    );
    assert!(
        !log.contains("<s:Envelope"),
        "SOAP body must not be logged: {log}"
    );
}

#[tokio::test]
async fn pause_logs_start_before_reply_and_measures_soap_delay() {
    let request_seen = Arc::new(tokio::sync::Notify::new());
    let reply_gate = Arc::new(tokio::sync::Notify::new());
    let renderer = controlled_renderer(
        StatusCode::OK,
        "<u:PauseResponse/>",
        Duration::ZERO,
        Some((request_seen.clone(), reply_gate.clone())),
    )
    .await;
    let capture = Capture::default();
    let _guard = capture.subscribe();
    let pause = renderer.output.pause();
    tokio::pin!(pause);
    tokio::select! {
        result = &mut pause => panic!("pause returned before renderer released reply: {result:?}"),
        seen = tokio::time::timeout(Duration::from_secs(10), request_seen.notified()) => {
            seen.expect("renderer must receive Pause");
        }
    }
    assert!(
        capture.text().contains("dlna_command_sending"),
        "start missing before response"
    );
    assert!(
        !capture.text().contains("dlna_command_finished"),
        "ack before response"
    );
    tokio::time::sleep(Duration::from_millis(80)).await;
    reply_gate.notify_one();
    pause.await.unwrap();
    let log = capture.text();
    assert_pair(&log, "Pause", "response_received");
    let end = log
        .lines()
        .find(|l| l.contains("dlna_command_finished"))
        .unwrap();
    assert!(
        field(end, "elapsed_ms").parse::<u64>().unwrap() >= 75,
        "SOAP delay not measured: {log}"
    );
    assert_eq!(
        renderer.received.lock().unwrap().len(),
        1,
        "no added SOAP request"
    );
}

#[tokio::test]
async fn resume_logs_its_acknowledgement_without_extra_request() {
    let renderer = renderer(StatusCode::OK, "<u:PlayResponse/>", Duration::ZERO).await;
    let capture = Capture::default();
    let _guard = capture.subscribe();
    renderer.output.resume().await.unwrap();
    assert_pair(&capture.text(), "Play", "response_received");
    assert_eq!(
        renderer.received.lock().unwrap().as_slice(),
        [format!("\"{AV_TRANSPORT_URN}#Play\"")]
    );
}

#[tokio::test]
async fn pause_and_resume_propagate_soap_faults_instead_of_false_success() {
    for action in ["Pause", "Play"] {
        let renderer = renderer(StatusCode::INTERNAL_SERVER_ERROR, FAULT, Duration::ZERO).await;
        let capture = Capture::default();
        let _guard = capture.subscribe();
        let result = if action == "Pause" {
            renderer.output.pause().await
        } else {
            renderer.output.resume().await
        };
        let error =
            result.expect_err("renderer SOAP 701 must reject pause/resume, not report success");
        assert!(error.contains("701"), "{error}");
        assert_pair(&capture.text(), action, "soap_fault");
        // #5050 — sur un 701, `pause()` lit `GetTransportInfo` ; ce renderer
        // n'en rend rien d'exploitable : UNE seule commande est envoyée.
        let sent = format!("\"{AV_TRANSPORT_URN}#{action}\"");
        assert_eq!(
            renderer
                .received
                .lock()
                .unwrap()
                .iter()
                .filter(|a| **a == sent)
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn namespaced_fault_in_http_200_is_not_an_acknowledgement() {
    let renderer = renderer(StatusCode::OK, "<soap:Fault xmlns:soap=\"http://schemas.xmlsoap.org/soap/envelope/\"><faultstring>refused</faultstring></soap:Fault>", Duration::ZERO).await;
    let capture = Capture::default();
    let _guard = capture.subscribe();
    assert!(
        renderer.output.pause().await.is_err(),
        "HTTP 200 does not turn a SOAP Fault into success"
    );
    assert_pair(&capture.text(), "Pause", "soap_fault");
}

#[tokio::test]
async fn http_failure_is_logged_and_returned_without_false_acknowledgement() {
    let renderer = renderer(StatusCode::INTERNAL_SERVER_ERROR, "", Duration::ZERO).await;
    let capture = Capture::default();
    let _guard = capture.subscribe();
    let error = renderer.output.pause().await.unwrap_err();
    assert!(error.starts_with(SOAP_HTTP_SANS_CORPS_PREFIX), "{error}");
    assert_pair(&capture.text(), "Pause", "transport_error");
}

#[tokio::test]
async fn raw_play_fault_is_preserved_for_existing_recovery_and_polling_stays_quiet() {
    let renderer = renderer(StatusCode::INTERNAL_SERVER_ERROR, FAULT, Duration::ZERO).await;
    let capture = Capture::default();
    let _guard = capture.subscribe();
    let raw = renderer
        .output
        .av_action("Play", "<InstanceID>0</InstanceID>")
        .await
        .unwrap();
    assert_eq!(
        raw, FAULT,
        "play_media must retain the raw fault for its 701 recovery"
    );
    assert_pair(&capture.text(), "Play", "soap_fault");
    let before = capture.text();
    let _ = renderer
        .output
        .av_action("GetTransportInfo", "<InstanceID>0</InstanceID>")
        .await;
    assert_eq!(
        capture.text(),
        before,
        "polling must not add command info logs"
    );
}

#[tokio::test]
async fn concurrent_commands_keep_distinct_ids_and_matching_results() {
    let renderer = renderer(StatusCode::OK, "<Response/>", Duration::from_millis(10)).await;
    let capture = Capture::default();
    let _guard = capture.subscribe();
    let (pause, resume) = tokio::join!(renderer.output.pause(), renderer.output.resume());
    pause.unwrap();
    resume.unwrap();
    let log = capture.text();
    let starts: Vec<_> = log
        .lines()
        .filter(|l| l.contains("dlna_command_sending"))
        .collect();
    let ends: Vec<_> = log
        .lines()
        .filter(|l| l.contains("dlna_command_finished"))
        .collect();
    assert_eq!(starts.len(), 2, "{log}");
    assert_eq!(ends.len(), 2, "{log}");
    assert_ne!(
        field(starts[0], "command_id"),
        field(starts[1], "command_id"),
        "concurrent commands cannot share an id: {log}"
    );
    for start in starts {
        let end = ends
            .iter()
            .find(|end| field(end, "command_id") == field(start, "command_id"))
            .expect("each command needs its own result");
        assert_eq!(field(start, "action"), field(end, "action"), "{log}");
    }
}
