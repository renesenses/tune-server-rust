use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tokio::sync::oneshot;
use tracing::warn;

use crate::state::{Attente, CorpsRelaye, PendingResponse, RelayState};
use crate::trafic::{Categorie, CompteursServeur, Erreur, Trafic};

pub async fn proxy_stream(
    State(state): State<Arc<RelayState>>,
    Path((server_id, stream_path)): Path<(String, String)>,
    uri: axum::http::Uri,
    headers: HeaderMap,
) -> Response {
    let debut = Instant::now();
    let conn = match state.servers.get(&server_id) {
        Some(c) => c,
        None => {
            state.trafic.erreur(Erreur::ServeurAbsent);
            return StatusCode::NOT_FOUND.into_response();
        }
    };

    // Auth : meme regle que le proxy d'API — `X-Bridge-Token` d'abord,
    // l'ancienne forme `Authorization: BridgeToken …` ensuite. Le commentaire
    // d'origine annoncait « from query or header » alors que seul l'en-tete
    // etait lu ; le lecteur audio du navigateur, lui, ne peut PAS poser
    // d'en-tete sur la source d'une balise <audio>, d'ou le parametre de
    // requete accepte ici.
    let jeton = crate::api_proxy::extraire_jeton(&headers)
        .or_else(|| crate::api_proxy::jeton_de_requete(uri.query()));
    // Jeton de session (#6603) ou ancien jeton du pont : voir
    // `RelayState::autoriser`. Un flux audio est ce qui fait d'une session
    // une ECOUTE : c'est ici que s'applique la limite d'une ecoute a
    // distance par compte.
    let acces = match state.autoriser(jeton.as_deref(), &server_id) {
        Ok(a) => a,
        Err(rejet) => return rejet.en_reponse(),
    };
    if let crate::sessions::Acces::Session(s) = &acces {
        state.sessions.audio(s);
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let range = headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let stream_req = message_de_flux(&request_id, &stream_path, &headers);

    let (tx, rx) = oneshot::channel::<PendingResponse>();
    conn.pending
        .lock()
        .await
        .insert(request_id.clone(), Attente::nouvelle(tx, Categorie::Audio));

    let message = stream_req.to_string();
    let compteurs = conn.trafic.clone();
    if conn.ws_tx.send(message.clone()).await.is_err() {
        conn.pending.lock().await.remove(&request_id);
        state.trafic.erreur(Erreur::ServeurAbsent);
        return StatusCode::BAD_GATEWAY.into_response();
    }
    state
        .trafic
        .sortie(&compteurs, Categorie::Audio, message.len());

    drop(conn);

    // Wait for the relay.stream_start response with headers/status
    match tokio::time::timeout(Duration::from_secs(30), rx).await {
        Ok(Ok(resp)) => crate::sessions::habiller(
            reponse_relayee_comptee(
                resp,
                Some(Comptage {
                    trafic: state.trafic.clone(),
                    serveur: compteurs,
                    categorie: Categorie::Audio,
                    cle: stream_path,
                    range,
                    debut,
                }),
            ),
            &acces,
        ),
        Ok(Err(_)) => {
            state.trafic.erreur(Erreur::CanalRompu);
            warn!(request_id = %request_id, "stream response channel dropped");
            StatusCode::BAD_GATEWAY.into_response()
        }
        Err(_) => {
            if let Some(conn) = state.servers.get(&server_id) {
                conn.pending.lock().await.remove(&request_id);
            }
            state.trafic.erreur(Erreur::Delai);
            warn!(request_id = %request_id, "stream request timeout");
            StatusCode::GATEWAY_TIMEOUT.into_response()
        }
    }
}

/// De quoi compter une reponse rendue au navigateur (#6512).
pub struct Comptage {
    pub trafic: Arc<Trafic>,
    pub serveur: Arc<CompteursServeur>,
    /// `Api` est requalifiee en `Images` d'apres le `content-type` rendu.
    pub categorie: Categorie,
    /// Le flux, pour reconnaitre une reprise : chemin ou piste de cercle.
    pub cle: String,
    pub range: Option<String>,
    /// Arrivee de la requete, pour le delai jusqu'au premier octet.
    pub debut: Instant,
}

/// Le message `relay.stream_request` d'une lecture : le `Range` ET le
/// `User-Agent` du lecteur.
///
/// #6453 — le serveur choisit le contrat d'une radio selon le lecteur
/// (`accepts_chunked_live_stream`, tune-stream-http) : corps chunké pour un
/// navigateur ou un renderer Lavf, « fichier borné » pour les autres, dont
/// AVPlayer. Sans l'agent, la requête locale du serveur n'en portait aucun, et
/// l'iPhone recevait le contrat chunké, qu'AVPlayer rejette
/// (`CoreMediaErrorDomain -12939`). Un serveur plus ancien ignore le champ.
pub fn message_de_flux(
    request_id: &str,
    stream_path: &str,
    headers: &HeaderMap,
) -> serde_json::Value {
    let entete = |nom: &str| {
        headers
            .get(nom)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    };
    serde_json::json!({
        "type": "relay.stream_request",
        "id": request_id,
        "stream_id": stream_path,
        "range": entete("range"),
        "user_agent": entete("user-agent"),
    })
}

/// Batit la reponse HTTP a partir de ce que le serveur a renvoye.
///
/// Un flux audio ne peut pas etre mis en memoire avant d'etre rendu : le
/// corps est cable directement sur le canal de morceaux, pour que le lecteur
/// entende le debut du morceau pendant que la fin voyage encore.
#[cfg(test)]
pub fn reponse_relayee(resp: PendingResponse) -> Response {
    reponse_relayee_comptee(resp, None)
}

/// [`reponse_relayee`], en comptant ce qui part vers le navigateur.
///
/// Un corps entier est compte d'un coup. Un flux ouvre une
/// [`crate::trafic::Session`] qui compte chaque morceau AU MOMENT ou il est
/// remis a la connexion HTTP — un navigateur parti avant la fin n'est pas
/// compte pour ce qu'il n'a pas recu — et note la fin quand le corps tombe.
pub fn reponse_relayee_comptee(resp: PendingResponse, comptage: Option<Comptage>) -> Response {
    let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = Response::builder().status(status);
    if let Some(entetes) = response.headers_mut() {
        *entetes = entetes_relayes(&resp.headers);
    }

    // Le type d'ORIGINE, pour les compteurs seulement (pochette, format du
    // flux) : le navigateur, lui, recoit celui de `entetes_relayes`.
    let content_type = resp
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .and_then(|(_, v)| v.as_str())
        .map(str::to_string);

    let corps = match (resp.body, comptage) {
        (CorpsRelaye::Entier(body), comptage) => {
            let body = body.unwrap_or_default();
            if let Some(c) = comptage {
                let categorie = match c.categorie {
                    Categorie::Api => Categorie::d_une_reponse(content_type.as_deref()),
                    autre => autre,
                };
                c.trafic.sortie(&c.serveur, categorie, body.len());
            }
            Body::from(body)
        }
        (CorpsRelaye::Morceaux(reception, _), None) => {
            let morceaux = futures_util::stream::unfold(reception, |mut rx| async move {
                rx.recv()
                    .await
                    .map(|morceau| (Ok::<Vec<u8>, std::io::Error>(morceau), rx))
            });
            Body::from_stream(morceaux)
        }
        (CorpsRelaye::Morceaux(reception, fin_propre), Some(c)) => {
            let session = c.trafic.ouvrir_session(crate::trafic::Ouverture {
                serveur: c.serveur.clone(),
                categorie: c.categorie,
                content_type: content_type.as_deref(),
                cle: c.cle,
                range: c.range.as_deref(),
                debut_requete: c.debut,
                fin_propre,
            });
            let morceaux = futures_util::stream::unfold(
                (reception, session),
                |(mut rx, mut session)| async move {
                    match rx.recv().await {
                        Some(morceau) => {
                            session.morceau_rendu(morceau.len());
                            Some((Ok::<Vec<u8>, std::io::Error>(morceau), (rx, session)))
                        }
                        None => {
                            // Le canal est ferme : la session se clot ici, et
                            // `Drop` lit si la fin etait propre.
                            session.fin_du_flux();
                            None
                        }
                    }
                },
            );
            Body::from_stream(morceaux)
        }
    };

    response
        .body(corps)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// En-tetes de la reponse du serveur qui passent jusqu'au navigateur. Tout le
/// reste est retire. `content-type` passe par [`type_relaye`].
const ENTETES_RELAYES: [&str; 7] = [
    "content-length",
    "content-range",
    "accept-ranges",
    "etag",
    "last-modified",
    "cache-control",
    "vary",
];

/// Types de contenu rendus tels quels, en plus de `audio/*`.
const TYPES_RELAYES: [&str; 10] = [
    "application/json",
    "image/jpeg",
    "image/bmp",
    "image/png",
    "image/webp",
    "image/gif",
    "image/avif",
    "application/vnd.apple.mpegurl",
    // Radios en Ogg (Ogg FLAC, Vorbis, Opus) annoncees sous ce type.
    "application/ogg",
    "text/plain",
];

const TYPE_PAR_DEFAUT: &str = "application/octet-stream";

/// Le type de contenu rendu au navigateur : celui du serveur s'il fait partie
/// de la liste, `application/octet-stream` sinon (ou s'il manque).
pub(crate) fn type_relaye(brut: Option<&str>) -> HeaderValue {
    let par_defaut = HeaderValue::from_static(TYPE_PAR_DEFAUT);
    let Some(brut) = brut.map(str::trim) else {
        return par_defaut;
    };
    let essence = brut
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let audio = essence.strip_prefix("audio/").is_some_and(|sous_type| {
        !sous_type.is_empty()
            && sous_type
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'+' | b'-' | b'_'))
    });
    if !audio && !TYPES_RELAYES.contains(&essence.as_str()) {
        return par_defaut;
    }
    HeaderValue::from_str(brut).unwrap_or(par_defaut)
}

/// Les en-tetes d'une reponse relayee : la liste blanche du serveur, le type
/// de contenu filtre, et les en-tetes de securite du relais.
pub(crate) fn entetes_relayes(serveur: &serde_json::Map<String, serde_json::Value>) -> HeaderMap {
    let mut entetes = HeaderMap::new();
    let mut type_brut = None;
    for (nom, valeur) in serveur {
        let Some(valeur) = valeur.as_str() else {
            continue;
        };
        let nom = nom.to_ascii_lowercase();
        if nom == "content-type" {
            type_brut = Some(valeur);
            continue;
        }
        if !ENTETES_RELAYES.contains(&nom.as_str()) {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(nom.as_bytes()),
            HeaderValue::from_str(valeur),
        ) {
            entetes.insert(n, v);
        }
    }

    let type_ = type_relaye(type_brut);
    let affichable = type_.to_str().is_ok_and(|t| {
        let t = t.trim_start().to_ascii_lowercase();
        // La liste de lecture HLS et l'Ogg sont lus par le lecteur comme
        // l'audio.
        t.starts_with("image/")
            || t.starts_with("audio/")
            || t.starts_with("application/vnd.apple.mpegurl")
            || t.starts_with("application/ogg")
    });
    entetes.insert(header::CONTENT_TYPE, type_);
    entetes.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    entetes.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox; default-src 'none'"),
    );
    if !affichable {
        entetes.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment"),
        );
    }
    entetes.insert(
        HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );
    entetes
}

#[cfg(test)]
mod tests_entetes {
    use super::*;

    fn reponse(status: u16, entetes: &[(&str, &str)]) -> Response {
        let mut map = serde_json::Map::new();
        for (n, v) in entetes {
            map.insert(
                (*n).to_string(),
                serde_json::Value::String((*v).to_string()),
            );
        }
        reponse_relayee(PendingResponse {
            status,
            headers: map,
            body: CorpsRelaye::Entier(Some(b"x".to_vec())),
        })
    }

    fn entete<'a>(r: &'a Response, nom: &str) -> Option<&'a str> {
        r.headers().get(nom).and_then(|v| v.to_str().ok())
    }

    #[test]
    fn une_page_html_du_serveur_part_en_octets_bruts_et_bac_a_sable() {
        for type_ in [
            "text/html; charset=utf-8",
            "TEXT/HTML",
            "image/svg+xml",
            "text/xml",
            "application/xml",
            "text/javascript",
            "application/xhtml+xml",
            "audio/",
        ] {
            let r = reponse(200, &[("Content-Type", type_)]);
            assert_eq!(entete(&r, "content-type"), Some(TYPE_PAR_DEFAUT), "{type_}");
            assert_eq!(
                entete(&r, "x-content-type-options"),
                Some("nosniff"),
                "{type_}"
            );
            assert_eq!(
                entete(&r, "content-security-policy"),
                Some("sandbox; default-src 'none'"),
                "{type_}"
            );
            assert_eq!(
                entete(&r, "content-disposition"),
                Some("attachment"),
                "{type_}"
            );
            assert_eq!(
                entete(&r, "cross-origin-resource-policy"),
                Some("same-origin"),
                "{type_}"
            );
        }
        // Sans type du tout : octets bruts aussi.
        let r = reponse(200, &[]);
        assert_eq!(entete(&r, "content-type"), Some(TYPE_PAR_DEFAUT));
    }

    #[test]
    fn les_entetes_hors_liste_sont_retires() {
        let r = reponse(
            302,
            &[
                ("Content-Type", "application/json"),
                ("Set-Cookie", "a=b; Path=/"),
                ("Location", "https://ailleurs.invalid/"),
                ("Refresh", "0; url=https://ailleurs.invalid/"),
                ("Content-Disposition", "inline"),
                ("Content-Security-Policy", "default-src *"),
                ("Access-Control-Allow-Origin", "*"),
                ("Access-Control-Allow-Credentials", "true"),
                ("X-Content-Type-Options", "sniff"),
                ("ETag", "\"v1\""),
                ("Cache-Control", "max-age=60"),
                ("Vary", "Accept"),
                ("Last-Modified", "Tue, 27 Oct 2026 08:00:00 GMT"),
            ],
        );
        for retire in [
            "set-cookie",
            "location",
            "refresh",
            "access-control-allow-origin",
            "access-control-allow-credentials",
        ] {
            assert!(
                r.headers().get(retire).is_none(),
                "{retire} doit etre retire"
            );
        }
        assert_eq!(entete(&r, "content-disposition"), Some("attachment"));
        assert_eq!(
            entete(&r, "content-security-policy"),
            Some("sandbox; default-src 'none'")
        );
        assert_eq!(entete(&r, "x-content-type-options"), Some("nosniff"));
        assert_eq!(entete(&r, "content-type"), Some("application/json"));
        assert_eq!(entete(&r, "etag"), Some("\"v1\""));
        assert_eq!(entete(&r, "cache-control"), Some("max-age=60"));
        assert_eq!(entete(&r, "vary"), Some("Accept"));
        assert_eq!(
            entete(&r, "last-modified"),
            Some("Tue, 27 Oct 2026 08:00:00 GMT")
        );
    }

    #[test]
    fn pochettes_et_audio_gardent_type_et_plages() {
        let r = reponse(
            200,
            &[("Content-Type", "image/jpeg"), ("Content-Length", "1")],
        );
        assert_eq!(entete(&r, "content-type"), Some("image/jpeg"));
        assert_eq!(entete(&r, "content-length"), Some("1"));
        assert!(r.headers().get("content-disposition").is_none());
        assert_eq!(entete(&r, "x-content-type-options"), Some("nosniff"));

        let r = reponse(
            206,
            &[
                ("content-type", "audio/flac"),
                ("content-range", "bytes 0-0/1000"),
                ("accept-ranges", "bytes"),
                ("content-length", "1"),
            ],
        );
        assert_eq!(r.status(), 206);
        assert_eq!(entete(&r, "content-type"), Some("audio/flac"));
        assert_eq!(entete(&r, "content-range"), Some("bytes 0-0/1000"));
        assert_eq!(entete(&r, "accept-ranges"), Some("bytes"));
        assert_eq!(entete(&r, "content-length"), Some("1"));
        assert!(r.headers().get("content-disposition").is_none());

        // Lus par le lecteur ou affiches en <img> : type garde, sans
        // telechargement force.
        for type_ in [
            "image/png",
            "image/webp",
            "image/gif",
            "image/avif",
            "image/bmp",
            "audio/mpeg",
            "audio/x-flac",
            "audio/ogg",
            "audio/ogg; codecs=flac",
            "application/ogg",
            "application/vnd.apple.mpegurl",
        ] {
            let r = reponse(200, &[("Content-Type", type_)]);
            assert_eq!(entete(&r, "content-type"), Some(type_), "{type_}");
            assert!(
                r.headers().get("content-disposition").is_none(),
                "{type_} ne doit pas etre telecharge"
            );
            assert_eq!(
                entete(&r, "x-content-type-options"),
                Some("nosniff"),
                "{type_}"
            );
        }
        // Une radio Ogg en plage garde ses en-tetes de plage.
        let r = reponse(
            206,
            &[
                ("Content-Type", "application/ogg"),
                ("Content-Range", "bytes 0-0/1000"),
                ("Accept-Ranges", "bytes"),
            ],
        );
        assert_eq!(entete(&r, "content-type"), Some("application/ogg"));
        assert_eq!(entete(&r, "content-range"), Some("bytes 0-0/1000"));
        assert_eq!(entete(&r, "accept-ranges"), Some("bytes"));
        // Donnees : type garde, telechargement force hors navigation.
        for type_ in [
            "text/plain; charset=utf-8",
            "application/json; charset=utf-8",
        ] {
            let r = reponse(200, &[("Content-Type", type_)]);
            assert_eq!(entete(&r, "content-type"), Some(type_), "{type_}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::message_de_flux;
    use axum::http::HeaderMap;

    /// #6453 — l'agent d'AVPlayer voyage jusqu'au serveur avec le `Range`.
    #[test]
    fn le_message_de_flux_porte_le_range_et_l_agent_du_lecteur() {
        let mut h = HeaderMap::new();
        h.insert("range", "bytes=0-1".parse().unwrap());
        h.insert(
            "user-agent",
            "AppleCoreMedia/1.0.0.24A446 (iPhone; U; CPU OS 27_0_1 like Mac OS X; fr_fr)"
                .parse()
                .unwrap(),
        );
        let m = message_de_flux("req-1", "abc.flac", &h);
        assert_eq!(m["type"], "relay.stream_request");
        assert_eq!(m["id"], "req-1");
        assert_eq!(m["stream_id"], "abc.flac");
        assert_eq!(m["range"], "bytes=0-1");
        assert!(
            m["user_agent"]
                .as_str()
                .is_some_and(|ua| ua.starts_with("AppleCoreMedia/")),
            "l'agent du lecteur doit partir vers le serveur : {m}"
        );
    }

    /// Sans en-têtes, les deux champs valent `null` : le serveur garde son
    /// comportement d'avant (agent absent → contrat chunké).
    #[test]
    fn sans_entetes_les_champs_sont_nuls() {
        let m = message_de_flux("req-2", "abc", &HeaderMap::new());
        assert!(m["range"].is_null());
        assert!(m["user_agent"].is_null());
    }
}
