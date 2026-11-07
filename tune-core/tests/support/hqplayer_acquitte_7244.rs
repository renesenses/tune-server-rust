//! Fake control endpoint for the XML declaration contracts, without audio.
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

pub async fn octets_recus(socket: TcpStream) -> String {
    let mut socket = BufReader::new(socket);
    let mut vus = String::new();
    let mut state = 0;
    let garde = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let mut ligne = String::new();
        let Ok(Ok(n)) = tokio::time::timeout_at(garde, socket.read_line(&mut ligne)).await else {
            break;
        };
        if n == 0 {
            break;
        }
        vus.push_str(&ligne);
        let texte = ligne.trim();
        if texte.is_empty() || texte.starts_with("<?xml") {
            continue;
        }
        let nom: String = texte
            .trim_start_matches('<')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        let reponse = if nom == "Status" {
            format!(r#"<Status state="{state}" position="0" length="300"/>"#)
        } else {
            if nom == "Play" {
                state = 2;
            }
            if nom == "Stop" {
                state = 0;
            }
            format!(r#"<{nom} result="OK"/>"#)
        };
        if socket
            .get_mut()
            .write_all(reponse.as_bytes())
            .await
            .is_err()
        {
            break;
        }
    }
    vus
}
