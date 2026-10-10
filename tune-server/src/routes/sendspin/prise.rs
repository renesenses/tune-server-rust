//! La « prise » WebSocket d'une session Sendspin, quel que soit le côté qui a
//! composé (#3326).
//!
//! La spécification exige d'un serveur les DEUX modes de connexion
//! (`connection.md`) : l'enceinte compose vers Tune (`_sendspin-server._tcp`,
//! prise ENTRANTE servie par axum), ou Tune compose vers l'enceinte qu'il a
//! découverte (`_sendspin._tcp`, prise SORTANTE ouverte par tungstenite).
//! « After this point, Sendspin works independently of how the connection was
//! established » : toute la suite du protocole (côté serveur, Tune initiateur
//! Noise) passe par cette seule interface, `recv` et `send`, en messages axum.
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as MessageT;

pub(crate) type FluxSortant =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub(crate) enum Prise {
    /// L'enceinte a composé vers `ws://…/sendspin` de Tune.
    Entrante(WebSocket),
    /// Tune a composé vers l'enceinte (connexion initiée par le serveur).
    Sortante(Box<FluxSortant>),
}

impl Prise {
    pub(crate) async fn recv(&mut self) -> Option<Result<Message, axum::Error>> {
        match self {
            Self::Entrante(ws) => ws.recv().await,
            Self::Sortante(ws) => loop {
                let m = match ws.next().await? {
                    Ok(m) => m,
                    Err(e) => return Some(Err(axum::Error::new(e))),
                };
                return Some(Ok(match m {
                    MessageT::Text(t) => Message::Text(t.as_str().into()),
                    MessageT::Binary(b) => Message::Binary(b),
                    MessageT::Ping(b) => Message::Ping(b),
                    MessageT::Pong(b) => Message::Pong(b),
                    MessageT::Close(c) => Message::Close(c.map(|c| CloseFrame {
                        code: c.code.into(),
                        reason: c.reason.as_str().into(),
                    })),
                    // Trame brute : jamais rendue par la lecture d'un flux.
                    MessageT::Frame(_) => continue,
                }));
            },
        }
    }

    pub(crate) async fn send(&mut self, m: Message) -> Result<(), axum::Error> {
        match self {
            Self::Entrante(ws) => ws.send(m).await,
            Self::Sortante(ws) => {
                let m = match m {
                    Message::Text(t) => MessageT::Text(t.as_str().into()),
                    Message::Binary(b) => MessageT::Binary(b),
                    Message::Ping(b) => MessageT::Ping(b),
                    Message::Pong(b) => MessageT::Pong(b),
                    Message::Close(c) => MessageT::Close(c.map(|c| {
                        tokio_tungstenite::tungstenite::protocol::CloseFrame {
                            code: c.code.into(),
                            reason: c.reason.as_str().into(),
                        }
                    })),
                };
                ws.send(m).await.map_err(axum::Error::new)
            }
        }
    }
}
