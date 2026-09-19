use super::*;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;

const DEADLINE: Duration = Duration::from_secs(3);
type Ports = (u16, u16);

fn query(sequence: u16) -> [u8; 32] {
    let mut packet = [0; 32];
    packet[..2].copy_from_slice(&[0x80, 0xd2]);
    packet[2..4].copy_from_slice(&sequence.to_be_bytes());
    packet[24..32].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    packet
}

async fn request(reader: &mut BufReader<tokio::net::TcpStream>) -> Option<(String, String)> {
    let mut first = String::new();
    if reader.read_line(&mut first).await.unwrap() == 0 {
        return None;
    }
    let mut headers = String::new();
    let mut body_len = 0;
    loop {
        let mut line = String::new();
        assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length: ") {
            body_len = value.trim().parse().unwrap();
        }
        headers.push_str(&line);
    }
    let mut body = vec![0; body_len];
    reader.read_exact(&mut body).await.unwrap();
    Some((
        first.split_whitespace().next().unwrap().to_string(),
        headers,
    ))
}

fn advertised_ports(headers: &str) -> Ports {
    let transport = headers
        .lines()
        .find_map(|line| line.strip_prefix("Transport: "))
        .unwrap();
    let port = |name: &str| {
        transport
            .split(';')
            .find_map(|part| part.strip_prefix(name))
            .unwrap()
            .parse::<u16>()
            .unwrap()
    };
    (port("control_port="), port("timing_port="))
}

fn assert_reserved(ports: Ports) {
    assert_ne!(ports.0, ports.1, "control and timing must be distinct");
    for port in [ports.0, ports.1] {
        assert_ne!(port, 0, "SETUP must announce a real bound port");
        assert!(
            UdpSocket::bind(("127.0.0.1", port)).is_err(),
            "advertised UDP port {port} must belong to this session"
        );
    }
}

fn assert_released(ports: Ports) {
    let control = UdpSocket::bind(("127.0.0.1", ports.0)).expect("control port must be released");
    let timing =
        UdpSocket::bind(("127.0.0.1", ports.1)).expect("timing responder/socket must be released");
    drop((control, timing));
}

async fn timing_exchange(ports: Ports, invalid: bool) {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let target = ("127.0.0.1", ports.1);
    if invalid {
        let bad = query(1);
        socket.send_to(&bad[..31], target).await.unwrap();
        let mut oversized = [0_u8; 33];
        oversized[..32].copy_from_slice(&bad);
        socket.send_to(&oversized, target).await.unwrap();
        let mut bad_version = bad;
        bad_version[0] = 0x40;
        socket.send_to(&bad_version, target).await.unwrap();
        let mut bad_type = bad;
        bad_type[1] = 0xd3;
        socket.send_to(&bad_type, target).await.unwrap();
    }
    let request = query(0x1234);
    socket.send_to(&request, target).await.unwrap();
    let mut reply = [0; 64];
    let (length, source) = tokio::time::timeout(DEADLINE, socket.recv_from(&mut reply))
        .await
        .expect("announced timing port must answer a valid RAOP query")
        .unwrap();
    assert_eq!(
        source.port(),
        ports.1,
        "reply must come from the advertised timing port"
    );
    assert_eq!(length, 32);
    assert_eq!(
        &reply[..8],
        &[0x80, 0xd3, 0x12, 0x34, 0, 0, 0, 0],
        "invalid datagrams must not produce a reply before the valid query"
    );
    assert_eq!(
        &reply[8..16],
        &request[24..32],
        "origin must echo the request transmit timestamp"
    );
    let receive = u64::from_be_bytes(reply[16..24].try_into().unwrap());
    let transmit = u64::from_be_bytes(reply[24..32].try_into().unwrap());
    assert!(
        receive <= transmit,
        "receive timestamp must precede transmit timestamp"
    );
    let ntp_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 2_208_988_800;
    assert!(
        (receive >> 32).abs_diff(ntp_seconds) <= 3,
        "timing must use NTP epoch/seconds"
    );
}

async fn receiver(
    refuse: Option<&'static str>,
    invalid: bool,
) -> (
    u16,
    tokio::sync::oneshot::Receiver<Ports>,
    tokio::task::JoinHandle<Vec<String>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (ports_tx, ports_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(socket);
        let mut ports_tx = Some(ports_tx);
        let audio = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut methods = Vec::new();
        while let Some((method, headers)) = request(&mut reader).await {
            if method == "SETUP" {
                let ports = advertised_ports(&headers);
                assert_reserved(ports);
                // The receiver can ask for timing before it acknowledges SETUP.
                timing_exchange(ports, invalid).await;
                ports_tx.take().unwrap().send(ports).unwrap();
            }
            let status = if refuse == Some(method.as_str()) {
                "403 Forbidden"
            } else {
                "200 OK"
            };
            let transport = if method == "SETUP" {
                format!(
                    "Session: timing-test\r\nTransport: RTP/AVP/UDP;server_port={}\r\n",
                    audio.local_addr().unwrap().port()
                )
            } else {
                String::new()
            };
            reader
                .get_mut()
                .write_all(
                    format!("RTSP/1.0 {status}\r\nCSeq: 1\r\n{transport}Content-Length: 0\r\n\r\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            let done = method == "TEARDOWN";
            methods.push(method);
            if done {
                break;
            }
        }
        methods
    });
    (port, ports_rx, task)
}

async fn start(
    refuse: Option<&'static str>,
    invalid: bool,
) -> (
    AirplayOutput,
    Ports,
    Result<(), String>,
    tokio::task::JoinHandle<Vec<String>>,
) {
    let (port, ports, server) = receiver(refuse, invalid).await;
    let output = AirplayOutput::new(
        "Timing fixture".into(),
        "airplay-loopback".into(),
        "127.0.0.1".into(),
        port,
    );
    let result = tokio::time::timeout(
        Duration::from_secs(8),
        output.play_media(&PlayMedia {
            url: "file:///dev/null",
            mime_type: "audio/wav",
            ..Default::default()
        }),
    )
    .await
    .expect("RTSP setup must terminate");
    let ports = ports
        .await
        .expect("receiver must observe a successful timing exchange");
    (output, ports, result, server)
}

#[tokio::test]
async fn timing_answers_before_setup_ack_and_ignores_invalid_packets_then_stop_releases_ports() {
    let (output, ports, result, server) = start(Some("TEARDOWN"), true).await;
    result.unwrap();
    timing_exchange(ports, false).await;
    output.stop().await.unwrap();
    assert_released(ports);
    assert_eq!(
        server.await.unwrap(),
        ["ANNOUNCE", "SETUP", "RECORD", "TEARDOWN"]
    );
}

#[tokio::test]
async fn setup_refusal_releases_timing_and_control_without_losing_403_diagnostic() {
    let (_output, ports, result, server) = start(Some("SETUP"), false).await;
    assert!(
        result
            .unwrap_err()
            .contains("AirPlay connection refused by the device (403)")
    );
    assert_released(ports);
    assert_eq!(server.await.unwrap(), ["ANNOUNCE", "SETUP", "TEARDOWN"]);
}

#[tokio::test]
async fn record_refusal_releases_timing_and_control() {
    let (_output, ports, result, server) = start(Some("RECORD"), false).await;
    assert!(result.unwrap_err().contains("RECORD failed: 403"));
    assert_released(ports);
    assert_eq!(
        server.await.unwrap(),
        ["ANNOUNCE", "SETUP", "RECORD", "TEARDOWN"]
    );
}

#[tokio::test]
async fn dropping_output_aborts_timing_responder_and_releases_ports() {
    let (output, ports, result, server) = start(None, false).await;
    result.unwrap();
    drop(output);
    // Abort is cancellation-safe but asynchronous; observe its completion.
    tokio::time::timeout(DEADLINE, async {
        loop {
            if let Ok(socket) = UdpSocket::bind(("127.0.0.1", ports.1)) {
                drop(socket);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Drop must abort the timing task");
    assert_released(ports);
    assert_eq!(server.await.unwrap(), ["ANNOUNCE", "SETUP", "RECORD"]);
}

#[tokio::test]
async fn timing_does_not_reply_to_an_ip_other_than_the_rtsp_peer() {
    let auxiliary = timing::AuxiliaryUdp::bind("192.0.2.1".parse().unwrap())
        .await
        .unwrap();
    let ports = auxiliary.ports();
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    socket
        .send_to(&query(7), ("127.0.0.1", ports.1))
        .await
        .unwrap();
    let mut reply = [0; 64];
    assert!(
        tokio::time::timeout(Duration::from_millis(100), socket.recv_from(&mut reply))
            .await
            .is_err(),
        "a source other than the RTSP peer must not receive a timing reply"
    );
    auxiliary.close().await;
    assert_released(ports);
}
