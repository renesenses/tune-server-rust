//! #7807 — une ligne INFO par requête d'un renderer vers le streamer.
//!
//! WiiM Ultra (firmware 5.2.834652) : `SetAVTransportURI` et `Play` sont
//! acquittés, puis le transport passe de lui-même à `STOPPED` 200 à 400 ms
//! plus tard. Le journal ne permettait pas de dire si le WiiM avait seulement
//! TIRÉ le flux : `stream_request` n'est écrite qu'une fois la session
//! trouvée, sans méthode, sans adresse du renderer, sans statut ni bilan, et
//! un `HEAD` ou un `404` (session inconnue) ne laissait aucune trace.
//!
//! Cette couche, posée sur la seule route `/stream/{stream_id}`, écrit à la
//! FERMETURE de la réponse (corps rendu en entier, ou lâché parce que la
//! connexion est partie) une ligne `stream_requete_renderer` :
//!
//! - `methode` (`GET`/`HEAD`), `chemin` (le chemin seul : la chaîne de
//!   requête, qui pourrait porter un jeton, n'est jamais écrite) ;
//! - `range` demandée, `agent` (User-Agent, tronqué), `ip` du renderer ;
//! - `statut` renvoyé, `octets` réellement remis au transport, `duree_ms`
//!   depuis l'arrivée de la requête, `corps_complet` (le corps est allé à son
//!   terme ou non ; un `HEAD` n'a pas de corps à lire).
//!
//! ⛔ Aucun comportement ne change : le corps est relayé tel quel, sa
//! `size_hint` (donc le `Content-Length` calculé par hyper) et sa fin sont
//! ceux du corps d'origine. Rien n'est écrit par paquet.

use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes, HttpBody};
use axum::extract::{ConnectInfo, Request};
use axum::middleware::Next;
use axum::response::Response;
use tracing::info;

/// Longueur maximale du User-Agent porté au journal.
const AGENT_MAX: usize = 160;

/// Ce qu'on sait de la requête à son arrivée.
struct Requete {
    methode: String,
    chemin: String,
    range: String,
    agent: String,
    ip: String,
    debut: std::time::Instant,
}

fn entete(req: &Request, nom: &str) -> String {
    req.headers()
        .get(nom)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string()
}

fn tronquer(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut fin = max;
    while !s.is_char_boundary(fin) {
        fin -= 1;
    }
    format!("{}…", &s[..fin])
}

/// La couche elle-même (`axum::middleware::from_fn`).
pub(crate) async fn journaliser(req: Request, next: Next) -> Response {
    let requete = Requete {
        methode: req.method().as_str().to_string(),
        // `path()` n'inclut jamais la chaîne de requête.
        chemin: req.uri().path().to_string(),
        range: entete(&req, "Range"),
        agent: tronquer(entete(&req, "User-Agent"), AGENT_MAX),
        ip: req
            .extensions()
            .get::<ConnectInfo<std::net::SocketAddr>>()
            .map_or_else(|| "-".to_string(), |c| c.0.ip().to_string()),
        debut: std::time::Instant::now(),
    };
    let reponse = next.run(req).await;
    let statut = reponse.status().as_u16();
    // hyper cesse de lire le corps dès que le `Content-Length` annoncé est
    // atteint, sans attendre sa fin : c'est cette longueur qui dit « complet ».
    let annonce = reponse
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    reponse.map(|corps| {
        let annonce = annonce.or_else(|| corps.size_hint().exact());
        Body::new(CorpsCompte {
            corps,
            requete,
            statut,
            annonce,
            octets: 0,
            fini: false,
        })
    })
}

/// Le corps de la réponse, relayé tel quel, qui compte ses octets et écrit la
/// ligne quand il est lâché.
struct CorpsCompte {
    corps: Body,
    requete: Requete,
    statut: u16,
    /// Longueur annoncée au renderer (`Content-Length`), si elle l'est.
    annonce: Option<u64>,
    octets: u64,
    fini: bool,
}

impl HttpBody for CorpsCompte {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, axum::Error>>> {
        let this = &mut *self;
        let r = Pin::new(&mut this.corps).poll_frame(cx);
        match &r {
            Poll::Ready(Some(Ok(trame))) => {
                if let Some(d) = trame.data_ref() {
                    this.octets += d.len() as u64;
                }
            }
            Poll::Ready(None) => this.fini = true,
            _ => {}
        }
        r
    }

    fn is_end_stream(&self) -> bool {
        self.corps.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.corps.size_hint()
    }
}

impl Drop for CorpsCompte {
    fn drop(&mut self) {
        let r = &self.requete;
        info!(
            methode = %r.methode,
            chemin = %r.chemin,
            range = %r.range,
            agent = %r.agent,
            ip = %r.ip,
            statut = self.statut,
            octets = self.octets,
            duree_ms = r.debut.elapsed().as_millis() as u64,
            corps_complet = self.fini
                || self.corps.is_end_stream()
                || self.annonce.is_some_and(|a| self.octets >= a),
            "stream_requete_renderer"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::tronquer;

    #[test]
    fn le_user_agent_est_tronque_sans_couper_un_caractere() {
        assert_eq!(tronquer("Lavf/58.29.100".into(), 160), "Lavf/58.29.100");
        let long = "é".repeat(100); // 200 octets
        let t = tronquer(long, 161);
        assert!(t.ends_with('…'));
        assert!(t.len() <= 161 + '…'.len_utf8());
    }
}
