//! Le tunnel : une connexion HTTP/1.1 par flux QUIC, rejouée en local
//! exactement comme le relais rejoue ses requêtes (`relay.rs`) — mêmes
//! routes, même liste blanche d'en-têtes, même marque [`ENTETE_RELAIS`].
//!
//! Rien d'autre n'est atteignable : un chemin qui ne commence pas, APRÈS
//! normalisation, par `/api/v1/` ou `/stream/` répond 404 sans appel local.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{BodyExt, Full, Limited, StreamBody, combinators::BoxBody};
use hyper::body::{Frame, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tracing::debug;

use super::{EcouteDirecte, Session};
use crate::cloud::ENTETE_RELAIS;

pub(super) type Corps = BoxBody<Bytes, std::io::Error>;

/// Corps de requête accepté au plus (une API, pas un dépôt de fichiers).
const CORPS_MAX: usize = 16 * 1024 * 1024;

/// Les en-têtes de l'app transmis au serveur : la liste blanche du relais
/// (`tune-bridge`, `ENTETES_RELAYES`), plus l'agent du lecteur, que le relais
/// transmet aussi pour un flux (#6453).
pub const ENTETES_TRANSMIS: [&str; 10] = [
    "content-type",
    "accept",
    "authorization",
    "range",
    "if-range",
    "if-none-match",
    "if-modified-since",
    "cache-control",
    "x-profile-id",
    "user-agent",
];

/// Les en-têtes de la réponse locale rendus à l'app.
const ENTETES_RENDUS: [&str; 8] = [
    "content-type",
    "content-length",
    "content-range",
    "accept-ranges",
    "content-disposition",
    "cache-control",
    "etag",
    "last-modified",
];

/// En-têtes posés par le serveur pour dire QUI est connecté à distance (les
/// mêmes noms que ceux du relais) : information, pas contrôle.
pub const ENTETE_APPAREIL_ID: &str = "x-tune-appareil-distant";
pub const ENTETE_APPAREIL_NOM: &str = "x-tune-appareil-distant-nom";

/// L'URL locale d'un chemin demandé par l'app, si elle est permise.
///
/// Le chemin est d'abord normalisé (points, `%2e`), PUIS jugé : ce qui sort
/// de `/api/v1/` ou de `/stream/` après normalisation est refusé.
pub fn url_locale(port: u16, chemin: &str) -> Option<reqwest::Url> {
    if !chemin.starts_with('/') || chemin.starts_with("//") || chemin.contains('\\') {
        return None;
    }
    let url = reqwest::Url::parse(&format!("http://127.0.0.1:{port}{chemin}")).ok()?;
    if url.host_str() != Some("127.0.0.1") || url.port() != Some(port) {
        return None;
    }
    let p = url.path();
    (p.starts_with("/api/v1/") || p.starts_with("/stream/")).then_some(url)
}

/// Le chemin transporte-t-il de l'audio ? C'est ce qui fait d'une session
/// une ÉCOUTE (limite d'une écoute à distance par compte). Plus large que le
/// relais, qui ne compte que `/stream/` : l'audio d'une piste par l'API compte
/// aussi.
pub fn est_audio(chemin: &str) -> bool {
    chemin.starts_with("/stream/")
        || (chemin.starts_with("/api/v1/library/tracks/") && chemin.ends_with("/audio"))
}

fn json(statut: StatusCode, corps: serde_json::Value) -> Response<Corps> {
    let mut r = Response::new(
        Full::new(Bytes::from(corps.to_string()))
            .map_err(|n: Infallible| match n {})
            .boxed(),
    );
    *r.status_mut() = statut;
    r.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    r
}

/// Sert UNE connexion HTTP/1.1 portée par un flux QUIC de la session.
pub(super) async fn servir_flux(
    moteur: Arc<EcouteDirecte>,
    session: Arc<Session>,
    send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
) {
    let io = TokioIo::new(tokio::io::join(recv, send));
    let service = hyper::service::service_fn(move |req: Request<Incoming>| {
        let moteur = moteur.clone();
        let session = session.clone();
        async move { Ok::<_, Infallible>(moteur.relayer(&session, req).await) }
    });
    if let Err(e) = hyper::server::conn::http1::Builder::new()
        .serve_connection(io, service)
        .await
    {
        debug!(erreur = %e, "ecoute_directe_flux_termine");
    }
}

impl EcouteDirecte {
    /// Une requête de l'app, rejouée en local.
    pub(super) async fn relayer(
        &self,
        session: &Arc<Session>,
        req: Request<Incoming>,
    ) -> Response<Corps> {
        if let Some(c) = session.coupure() {
            return json(
                StatusCode::UNAUTHORIZED,
                serde_json::json!({"code": "session_coupee", "motif": c.motif}),
            );
        }
        let (parts, corps) = req.into_parts();
        let chemin = parts
            .uri
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/");
        let Some(url) = url_locale(self.local_port, chemin) else {
            return json(
                StatusCode::NOT_FOUND,
                serde_json::json!({"code": "route_hors_ecoute_distante"}),
            );
        };
        let flux = url.path().starts_with("/stream/");
        if flux && !matches!(parts.method, Method::GET | Method::HEAD) {
            return json(
                StatusCode::METHOD_NOT_ALLOWED,
                serde_json::json!({"code": "methode"}),
            );
        }
        let Ok(methode) = reqwest::Method::from_bytes(parts.method.as_str().as_bytes()) else {
            return json(
                StatusCode::METHOD_NOT_ALLOWED,
                serde_json::json!({"code": "methode"}),
            );
        };
        let corps = match Limited::new(corps, CORPS_MAX).collect().await {
            Ok(c) => c.to_bytes(),
            Err(_) => {
                return json(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    serde_json::json!({"code": "corps_trop_long"}),
                );
            }
        };
        if est_audio(url.path()) {
            self.audio(session);
        }

        let mut requete = self.local.request(methode, url);
        for nom in ENTETES_TRANSMIS {
            for valeur in parts.headers.get_all(nom) {
                if let Ok(v) = valeur.to_str() {
                    requete = requete.header(nom, v);
                }
            }
        }
        // APRÈS les en-têtes de l'app : elle peut en ajouter, jamais retirer
        // ceux-ci. Vue du serveur, la requête vient de 127.0.0.1 comme le
        // navigateur local ; `ENTETE_RELAIS` dit qu'elle vient de loin.
        requete = requete
            .header(ENTETE_RELAIS, "1")
            .header(ENTETE_APPAREIL_ID, session.did.as_str());
        if let Some(nom) = session.appareil.as_deref() {
            requete = requete.header(ENTETE_APPAREIL_NOM, nom);
        }
        if !corps.is_empty() {
            requete = requete.body(corps);
        }

        let reponse = match requete.send().await {
            Ok(r) => r,
            Err(e) => {
                debug!(erreur = %e.without_url(), "ecoute_directe_appel_local_echoue");
                return json(
                    StatusCode::BAD_GATEWAY,
                    serde_json::json!({"code": "appel_local_echoue"}),
                );
            }
        };
        let statut = reponse.status().as_u16();
        let mut sortie = Response::builder()
            .status(StatusCode::from_u16(statut).unwrap_or(StatusCode::BAD_GATEWAY));
        for nom in ENTETES_RENDUS {
            if let Some(v) = reponse.headers().get(nom).and_then(|v| v.to_str().ok()) {
                sortie = sortie.header(nom, v);
            }
        }
        let compteur = session.clone();
        let octets = reponse.bytes_stream().map(move |morceau| {
            morceau
                .map(|b| {
                    compteur.octets.fetch_add(b.len() as u64, Ordering::Relaxed);
                    Frame::data(b)
                })
                .map_err(std::io::Error::other)
        });
        sortie
            .body(BodyExt::boxed(StreamBody::new(octets)))
            .unwrap_or_else(|_| {
                json(
                    StatusCode::BAD_GATEWAY,
                    serde_json::json!({"code": "reponse_locale"}),
                )
            })
    }
}
