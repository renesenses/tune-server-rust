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

/// #8395 — HQPlayer 3 Desktop (journal 3.18.2) : pas de `position`, mais
/// `min` et `sec`. La position doit avancer, la durée rester inconnue.
#[tokio::test]
async fn hqplayer_3_min_sec_donnent_la_position_8395() {
    for (reply, position_ms) in [
        (
            r#"<Status state="2" track="1" min="0" sec="2" clips="0"/>"#,
            2_000,
        ),
        (
            r#"<Status state="2" track="1" min="3" sec="7" clips="0"/>"#,
            187_000,
        ),
    ] {
        let (status, unknown) = status_from_peer(reply).await;
        let status = status.expect("état valide");
        assert_eq!(status.state, TransportState::Playing, "{reply}");
        assert!(!unknown);
        assert_eq!(status.position_ms, position_ms, "{reply}");
        assert_eq!(status.duration_ms, 0, "durée inconnue : {reply}");
    }
}

/// #8395 — HQPlayer 4+ donne `position` : elle prime sur `min`/`sec`.
#[tokio::test]
async fn position_explicite_prime_sur_min_sec_8395() {
    let (status, _) =
        status_from_peer(r#"<Status state="2" min="9" sec="9" position="21.5" length="110"/>"#)
            .await;
    let status = status.expect("état valide");
    assert_eq!(status.position_ms, 21_500);
    assert_eq!(status.duration_ms, 110_000);
}

#[test]
fn min_sec_lus_sur_les_attributs_exacts_8395() {
    assert_eq!(
        position_min_sec(r#"<Status total_min="5" total_sec="300" min="0" sec="2"/>"#),
        Some(2.0)
    );
    assert_eq!(
        position_min_sec(r#"<Status total_min="5" total_sec="300"/>"#),
        None
    );
    assert_eq!(position_min_sec(r#"<Status min="1"/>"#), None);
    assert_eq!(position_min_sec(r#"<Status min="x" sec="2"/>"#), None);
    assert_eq!(
        position_min_sec(r#"<Status min="18446744073709551615" sec="2"/>"#),
        None,
        "pas de dépassement sur une valeur venue du réseau"
    );
}
