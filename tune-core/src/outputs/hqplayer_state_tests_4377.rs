use super::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

async fn status_from_peer(reply: &str) -> (Result<OutputStatus, String>, bool) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let reply = reply.to_owned();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = BufReader::new(socket);
        loop {
            let mut line = String::new();
            assert_ne!(socket.read_line(&mut line).await.unwrap(), 0);
            if line.starts_with("<Status ") {
                socket.get_mut().write_all(reply.as_bytes()).await.unwrap();
                break;
            }
        }
    });
    let output = HqplayerOutput::new(
        "Protocol fixture".into(),
        "hqp-state-4377".into(),
        "127.0.0.1".into(),
        port,
    );
    let status = tokio::time::timeout(std::time::Duration::from_secs(3), output.get_status())
        .await
        .expect("Status query must terminate");
    server.await.unwrap();
    (status, output.etat_inconnu_dit.load(Ordering::Relaxed))
}

#[tokio::test]
async fn official_numeric_states_reach_the_output_contract() {
    // Signalyst ControlInterface::State, not a capture from the user's device.
    for (wire, expected) in [
        ("2", TransportState::Playing),
        ("0", TransportState::Stopped),
        ("1", TransportState::Paused),
        ("3", TransportState::Transitioning),
    ] {
        let reply = format!(r#"<Status state="{wire}" position="21.5" length="110" />"#);
        let (status, unknown) = status_from_peer(&reply).await;
        let status = status.expect("état valide");
        assert_eq!(status.state, expected, "HQPlayer numeric state {wire}");
        assert!(
            !unknown,
            "official enum value must not trigger unknown-state warning"
        );
        assert_eq!(status.position_ms, 21_500);
        assert_eq!(status.duration_ms, 110_000);
    }
}

#[tokio::test]
async fn track_metadata_cannot_override_status_state() {
    for reply in [
        r#"<Status title="playing" state="0"/>"#,
        r#"<Status state="0"><Title>playing</Title></Status>"#,
        r#"<Status data-state="playing" state="0"/>"#,
        r#"<Status state="0"><State>playing</State></Status>"#,
    ] {
        let (status, unknown) = status_from_peer(reply).await;
        let status = status.expect("état valide");
        assert_eq!(status.state, TransportState::Stopped, "{reply}");
        assert!(!unknown);
    }
}

#[tokio::test]
async fn absent_or_unknown_state_keeps_the_existing_diagnostic() {
    for reply in [
        r#"<Status state="99" title="playing"/>"#,
        r#"<Status title="playing"/>"#,
        r#"<Status><Title>playing</Title></Status>"#,
        r#"<Status data-state="playing"/>"#,
    ] {
        let (status, unknown) = status_from_peer(reply).await;
        assert!(
            status
                .expect_err("un état inconnu ne doit jamais devenir Stopped")
                .contains("hqplayer_status_etat_inconnu"),
            "{reply}"
        );
        assert!(
            unknown,
            "missing/unknown Status.state must remain observable: {reply}"
        );
    }
}

#[tokio::test]
async fn textual_status_forms_remain_supported() {
    for (reply, expected) in [
        (r#"<Status state="PLAYING"/>"#, TransportState::Playing),
        (
            r#"<?xml version="1.0"?>
<Status state = '2' />"#,
            TransportState::Playing,
        ),
        ("<Status state = 'paused'/>", TransportState::Paused),
        (
            "<Status><State>stopped</State></Status>",
            TransportState::Stopped,
        ),
        (
            r#"<Status state="buffering"/>"#,
            TransportState::Transitioning,
        ),
    ] {
        let (status, unknown) = status_from_peer(reply).await;
        let status = status.expect("état valide");
        assert_eq!(status.state, expected, "{reply}");
        assert!(!unknown);
    }
}

#[test]
fn a_state_after_a_closed_status_is_not_its_child() {
    for xml in [
        "<Status/><State>playing</State>",
        "<Status></Status><State>playing</State>",
    ] {
        assert_eq!(etat_reconnu(xml), None, "{xml}");
    }
}

#[tokio::test]
async fn unknown_status_does_not_poison_the_next_valid_poll_6515() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let (read, mut write) = socket.into_split();
        let mut lines = BufReader::new(read).lines();
        for reply in [
            r#"<Status state="99"/>"#,
            r#"<Status state="2" position="12.5"/>"#,
        ] {
            while let Some(line) = lines.next_line().await.unwrap() {
                if line.contains("<Status") {
                    break;
                }
            }
            write
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        }
    });
    let output = HqplayerOutput::new(
        "test-6515".into(),
        "Fake HQPlayer".into(),
        "127.0.0.1".into(),
        port,
    );
    assert!(output.get_status().await.is_err());
    let recovered = tokio::time::timeout(std::time::Duration::from_secs(3), output.get_status())
        .await
        .expect("la sonde doit reprendre")
        .expect("état reconnu après l'inconnu");
    assert_eq!(recovered.state, TransportState::Playing);
    assert_eq!(recovered.position_ms, 12_500);
    server.await.unwrap();
}
