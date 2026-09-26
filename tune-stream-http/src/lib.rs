//! Axum HTTP handlers for audio streaming.
//!
//! The business logic (session management, buffer handling) lives in
//! `tune_core::http::streamer`. This module provides the HTTP layer only.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use tracing::{debug, info, warn};

use tune_core::http::streamer::{
    ICY_METAINT, ReresolveFn, SharedSessions, StreamInfo, StreamSession, build_icy_metadata,
    build_wav_header, build_wav_header_bounded_live, build_wav_header_streaming, extract_stream_id,
};

/// Tracks one HTTP consumer of a radio→WAV session for the lifetime of its
/// stream (drops on normal end AND on client disconnect, since the streaming
/// body future is cancelled at a yield point). Diagnostics for the
/// "FIP silent after upstream reconnect" case: the PCM channel is
/// single-consumer, so a second concurrent request would split the stream.
struct RadioConsumerGuard {
    session: std::sync::Arc<StreamSession>,
    started: std::time::Instant,
    /// Set true when the channel closed cleanly (recv returned None), so the
    /// Drop path can tell a graceful end from a client disconnect.
    completed: bool,
    /// Set true when a NEWER connection claimed the single-consumer channel and
    /// this one handed it off (a DLNA renderer re-requesting without closing the
    /// first). That is an expected, internal end — not a client disconnect — so
    /// the Drop path stays quiet about it.
    superseded: bool,
}

/// Can this renderer consume a live radio stream as a chunked, length-less
/// body?
///
/// A libavformat (`Lavf`) renderer can, and in fact *needs* to: it wants the
/// `0xFFFF_FFFF` indeterminate-length WAV header, without which it treats the
/// transcoded radio as a bounded PCM file, fills its ~64 MiB read-ahead cache
/// and stops after ~6 minutes (FIP, .15, commit 3d5a3a8f).
///
/// Others cannot. The darTZeel LHC-208 refuses chunked transfer outright and
/// requires `Content-Length` + `Range` (session support JP + Yves, 01/08).
/// Everything that plays on it carries a length — local files via `serve_file`,
/// Qobuz tracks via `proxy_stream`. Radio was the only length-less stream, and
/// the only one that never started: it connected, waited ~7 s, dropped, and
/// never came back (#1689).
///
/// The two contracts are mutually exclusive, so the response follows the
/// renderer. An absent or unreadable User-Agent keeps the current behaviour:
/// only a renderer that positively identifies itself as something other than
/// Lavf gets the file contract.
///
/// ## #3513 — un navigateur n'est pas un renderer
///
/// Le filet posé par #1689 (« tout ce qui n'est pas Lavf ») a attrapé le
/// **navigateur**, qui n'a jamais refusé le chunké : c'est le mode de transfert
/// par défaut de HTTP/1.1, et l'élément `<audio>` de Firefox le lit sans rien
/// demander. Servi comme un fichier de 2 Gio, Firefox faisait au contraire ce
/// qu'un fichier autorise — il rouvrait la connexion avec un `Range` — et
/// comme le canal PCM d'une station n'admet **qu'un seul consommateur**, cette
/// reconnexion supplantait la précédente. Chez Fabien, six fois en un quart
/// d'heure : neuf secondes de son, `radio_stream_superseded`, puis
/// `radio_stream_client_disconnect … remaining_consumers=0` — plus personne.
///
/// Le contrat fichier reste ce qu'il est pour les appareils qui l'exigent
/// (darTZeel LHC-208, Marantz, Sonos…) : ils ne s'annoncent pas `Mozilla`.
/// Un navigateur, lui, retrouve le chunké — donc **aucun `Content-Length`,
/// aucun `Accept-Ranges`, plus rien qui l'invite à se reconnecter**. C'est la
/// cause qui disparaît, pas le symptôme qu'on rattrape.
///
/// `Mozilla/5.0` est le préfixe que déclarent Firefox, Chrome, Safari et Edge
/// sans exception. Aucun des renderers de #1689 ne le porte, et si un appareil
/// inconnu s'annonçait ainsi, il retomberait simplement sur le comportement
/// d'avant #1689 — chunké — et non sur une panne nouvelle.
fn accepts_chunked_live_stream(user_agent: Option<&str>) -> bool {
    match user_agent {
        Some(ua) if !ua.is_empty() => {
            let ua = ua.to_ascii_lowercase();
            ua.contains("lavf") || ua.contains("mozilla")
        }
        _ => true,
    }
}

/// Entrelacer les blocs de métadonnées ICY dans un morceau du corps.
///
/// Rend la charge découpée sur la fenêtre `icy-metaint` annoncée, un bloc de
/// métadonnées inséré à chaque frontière. `depuis_meta` est le nombre d'octets
/// déjà servis dans la fenêtre courante ; il est mis à jour.
///
/// **Il se compte depuis le PREMIER octet du corps de la réponse, en-tête WAV
/// compris.** Le renderer, lui, compte comme ça : c'est la définition d'ICY. Ne
/// compter que le PCM décalerait chaque bloc de 44 octets, et l'appareil lirait
/// un octet de son comme longueur de métadonnées — du bruit à la place du
/// morceau. Le piège ne se voyait pas tant que le canal restait fermé.
///
/// `bloc` n'est appelé que lorsqu'un bloc part réellement : c'est lui qui relit
/// le titre courant, et le relire à chaque morceau de PCM ne servirait à rien.
fn decoupe_icy(
    charge: &[u8],
    depuis_meta: &mut usize,
    bloc: &dyn Fn() -> Vec<u8>,
) -> Vec<bytes::Bytes> {
    let mut sorties = Vec::new();
    let mut offset = 0usize;
    while offset < charge.len() {
        let restant = ICY_METAINT.saturating_sub(*depuis_meta);
        let fin = (offset + restant).min(charge.len());
        if fin > offset {
            sorties.push(bytes::Bytes::copy_from_slice(&charge[offset..fin]));
            *depuis_meta += fin - offset;
            offset = fin;
        }
        if *depuis_meta >= ICY_METAINT {
            sorties.push(bytes::Bytes::from(bloc()));
            *depuis_meta = 0;
        }
    }
    sorties
}

/// Réponse HEAD d'un flux radio live, sans créer de session ni contacter la
/// station. Le MediaServer publie une URL stable avant que le renderer fasse
/// son GET ; ce point d'entrée partage donc exactement le contrat du HEAD
/// d'une session radio déjà créée.
pub fn live_radio_head_response(mime_type: &str, req_headers: &HeaderMap) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_str(mime_type).expect("valid radio MIME type"),
    );
    headers.insert("Connection", HeaderValue::from_static("keep-alive"));
    headers.insert(
        "transferMode.dlna.org",
        HeaderValue::from_static("Streaming"),
    );

    let ua = req_headers.get("User-Agent").and_then(|v| v.to_str().ok());
    if accepts_chunked_live_stream(ua) {
        headers.insert("Transfer-Encoding", HeaderValue::from_static("chunked"));
    } else {
        headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
        headers.insert(
            "Content-Length",
            HeaderValue::from(tune_core::http::streamer::LIVE_BOUNDED_TOTAL_LEN),
        );
    }

    (StatusCode::OK, headers).into_response()
}

impl RadioConsumerGuard {
    fn new(session: std::sync::Arc<StreamSession>) -> Self {
        use std::sync::atomic::Ordering::Relaxed;
        let n = session.active_consumers.fetch_add(1, Relaxed) + 1;
        if n > 1 {
            // Transient: a 2nd request briefly overlaps the first while the
            // older connection is being handed off (see the supersede logic in
            // handle_stream). It no longer splits the stream — the older
            // consumer stops without pulling further chunks — so this is
            // informational, not an error.
            info!(
                stream_id = %session.id,
                consumers = n,
                "radio_stream_reconnect — a newer request is taking over the \
                 single-consumer PCM channel; the older connection is handed off"
            );
        }
        Self {
            session,
            started: std::time::Instant::now(),
            completed: false,
            superseded: false,
        }
    }
}

impl Drop for RadioConsumerGuard {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        let remaining = self
            .session
            .active_consumers
            .fetch_sub(1, Relaxed)
            .saturating_sub(1);
        if !self.completed && !self.superseded {
            info!(
                stream_id = %self.session.id,
                connected_secs = self.started.elapsed().as_secs(),
                remaining_consumers = remaining,
                "radio_stream_client_disconnect — HTTP consumer dropped mid-stream"
            );
        }
    }
}

/// `Content-Type` d'une session : l'orthographe du MIME que la sortie DLNA a
/// ANNONCÉE au renderer si elle en a posé une (reprise 714, #4958), sinon le
/// MIME de la session. Le `HEAD` et le `GET` doivent dire ce que dit la DIDL :
/// le Beosound Stage confronte le `Content-Type` du `HEAD` à son Sink.
fn content_type_servi(stream_id: &str, mime_session: &str) -> HeaderValue {
    let mime = tune_core::http::streamer::content_type_du_flux(stream_id, mime_session);
    HeaderValue::from_str(&mime).unwrap_or_else(|_| HeaderValue::from_str(mime_session).unwrap())
}

pub async fn handle_head(
    Path(raw_id): Path<String>,
    State(sessions): State<SharedSessions>,
    req_headers: HeaderMap,
) -> Response {
    let stream_id = extract_stream_id(&raw_id);
    // Clone the Arc so we release the sessions lock before any async I/O.
    // Holding the global sessions lock across tokio::fs::metadata() (an async
    // syscall) would serialize ALL concurrent stream requests — HEAD and GET
    // included — on a single lock, causing unnecessary latency on renderers
    // that issue HEAD+GET in quick succession (DMP-A8, darTZeel, etc.).
    let session = {
        let sessions = sessions.lock().await;
        sessions.get(stream_id).cloned()
    };

    let Some(session) = session else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // For file sessions, read actual size from filesystem (consistent with GET)
    let file_size = if session.info.file_size.is_some() {
        session.info.file_size
    } else {
        let fp = session.file_path.lock().await;
        if let Some(ref path) = *fp {
            tokio::fs::metadata(path.as_str())
                .await
                .ok()
                .map(|m| m.len())
        } else {
            session.info.wav_content_length()
        }
    };

    let is_radio = session.is_radio;

    info!(
        stream_id,
        format = %session.info.format,
        file_size = ?file_size,
        is_radio,
        "stream_head_request"
    );

    if is_radio {
        // Le HEAD doit annoncer le même contrat que le GET qui suit, sans quoi
        // un lecteur qui sonde d'abord conclut « pas de longueur » et n'essaie
        // même pas (#1689).
        return live_radio_head_response(&session.info.mime_type, &req_headers);
    }

    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        content_type_servi(&session.id, &session.info.mime_type),
    );
    headers.insert("Connection", HeaderValue::from_static("keep-alive"));

    if session.is_channel().await {
        // Conversion à la volée : le canal ne rejoue aucun octet passé. Le HEAD
        // doit dire la même vérité que la DIDL (DLNA.ORG_OP=00) — annoncer
        // Accept-Ranges ici invite le renderer à seeker un tuyau (DMP-A8,
        // gel à 0:00 en boucle sur tout DSD converti, 24/08).
        headers.insert(
            "transferMode.dlna.org",
            HeaderValue::from_static("Streaming"),
        );
        headers.insert(
            "contentFeatures.dlna.org",
            HeaderValue::from_static(
                "DLNA.ORG_OP=00;DLNA.ORG_FLAGS=01700000000000000000000000000000",
            ),
        );
        if let Some(size) = file_size {
            headers.insert("Content-Length", HeaderValue::from(size));
        }
    } else {
        headers.insert(
            "transferMode.dlna.org",
            HeaderValue::from_static("Interactive"),
        );
        headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
        headers.insert(
            "contentFeatures.dlna.org",
            HeaderValue::from_static(
                "DLNA.ORG_OP=01;DLNA.ORG_FLAGS=01700000000000000000000000000000",
            ),
        );
        if let Some(size) = file_size {
            headers.insert("Content-Length", HeaderValue::from(size));
        }
    }

    (StatusCode::OK, headers).into_response()
}

/// Sentinelle du corps d'une conversion : elle NOMME un blocage qui ne finit
/// jamais.
///
/// `StreamSession::note_delivery_stall` ne se prononce qu'au RETOUR du
/// `yield` : elle mesure une attente FINIE. Un corps lâché EN VOL — la
/// connexion meurt, la zone est arrêtée, le processus n'en sort plus — ne
/// passe jamais par ce point de mesure et ne laisse donc pas une ligne. C'est
/// la forme même de #3575 (« sortie locale imprenable pour toute la vie du
/// processus ») : le pire cas est précisément celui qui se tait.
///
/// La sentinelle vit dans le corps du flux. Quand le corps est lâché, son
/// `Drop` demande à la session si une attente était en vol, et l'écrit.
struct SentinelleDuCorps(std::sync::Arc<StreamSession>);

impl Drop for SentinelleDuCorps {
    fn drop(&mut self) {
        if let Some(attente) = self.0.note_delivery_abandoned() {
            warn!(
                stream_id = %self.0.id,
                attente_transport_ms = attente.as_millis() as u64,
                bytes_sent = self
                    .0
                    .bytes_sent
                    .load(std::sync::atomic::Ordering::Relaxed),
                "stream_delivery_abandoned — le corps du flux interne a été lâché ALORS \
                 qu'un morceau attendait encore de partir : cette attente ne finira \
                 jamais, et personne ne serait revenu la mesurer"
            );
        }
    }
}

pub async fn handle_stream(
    Path(raw_id): Path<String>,
    State(sessions): State<SharedSessions>,
    req_headers: HeaderMap,
) -> Response {
    let stream_id = extract_stream_id(&raw_id);
    let session = {
        let sessions = sessions.lock().await;
        sessions.get(stream_id).cloned()
    };

    let Some(session) = session else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let range_hdr = req_headers
        .get("Range")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
    // Possédé, pas emprunté : le corps du flux en a besoin après la réponse,
    // et c'est lui qui décide de la taille annoncée dans l'en-tête WAV d'une
    // radio (voir wants_indeterminate_wav_length).
    let user_agent = req_headers
        .get("User-Agent")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    // `wants_icy` est lu ICI, avant les branches fichier et mandataire, pour
    // qu'il figure sur CHAQUE `stream_request` : le journal de Jean Valjean ne
    // portait pas une ligne « icy », et rien ne permettait de savoir si son
    // renderer avait demandé les métadonnées ou non (#2161).
    let wants_icy = req_headers
        .get("Icy-MetaData")
        .and_then(|v| v.to_str().ok())
        == Some("1");
    info!(
        stream_id,
        range = range_hdr,
        agent = user_agent.as_deref().unwrap_or("-"),
        format = %session.info.format,
        wants_icy,
        "stream_request"
    );
    session.first_request.notify_waiters();

    // File serving with Range support
    let file_path = session.file_path.lock().await.clone();
    if let Some(ref path) = file_path {
        // Cette branche ne découpe pas le corps : `Icy-MetaData: 1` a beau
        // avoir été demandé, aucun bloc ne partira jamais. On le NOTE au lieu
        // de laisser le poller conclure « aucun renderer connecté » (#2991).
        tune_core::http::streamer::note_icy_channel(
            stream_id,
            wants_icy,
            false,
            tune_core::http::streamer::VOIE_FICHIER,
            false,
        );
        return serve_file(path, &session.info, &req_headers, session.clone()).await;
    }

    // Proxy mode
    let proxy_url = session.proxy_url.lock().await.clone();
    if let Some(ref url) = proxy_url {
        // Idem : le mandataire recopie l'amont octet pour octet. C'est la voie
        // que prend une radio non transcodée, et elle est SANS ICY (#2991).
        tune_core::http::streamer::note_icy_channel(
            stream_id,
            wants_icy,
            false,
            tune_core::http::streamer::VOIE_MANDATAIRE,
            false,
        );
        return proxy_stream(
            url,
            &session.info,
            session.is_radio,
            &req_headers,
            session.clone(),
        )
        .await;
    }

    // Chunked streaming mode
    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        content_type_servi(&session.id, &session.info.mime_type),
    );
    headers.insert(
        "transferMode.dlna.org",
        HeaderValue::from_static("Streaming"),
    );
    headers.insert("Cache-Control", HeaderValue::from_static("no-cache"));

    // When we know the WAV content length, send it so DLNA renderers
    // (DMP-A6/A8) don't need to probe the stream end with seek requests.
    let is_wav = session.info.format == "wav";
    let is_radio = session.is_radio;
    // A live radio→WAV stream has no length. A Lavf renderer wants it that way
    // (chunked body + indeterminate WAV header). A renderer that refuses
    // chunked transfer gets the file contract instead: a large finite
    // Content-Length, Accept-Ranges, and Range honoured (#1689).
    let bounded_live = is_wav && is_radio && !accepts_chunked_live_stream(user_agent.as_deref());
    let wav_length = if is_wav && !is_radio {
        session.info.wav_content_length()
    } else {
        None
    };
    if is_radio && !bounded_live {
        headers.insert("Transfer-Encoding", HeaderValue::from_static("chunked"));
    }

    // DLNA renderers (Marantz SR7009, Eversolo DMP-A8) send Range: bytes=0-
    // even for the initial request and expect a 206 Partial Content response
    // with Content-Range.  Without this, they reject the stream and stop
    // playback.  When we know the content length, honour the Range request
    // by responding with 206 + Content-Range.
    //
    // L'Eversolo va plus loin : il télécharge par tranches (~1,3 Mo), ferme la
    // connexion, puis revient avec `bytes=N-` pour la tranche suivante. Répondre
    // 200 + longueur totale à cette reprise casse son contrat HTTP : il jette la
    // réponse et redemande le même offset en boucle — la « boucle de 4-7 s »
    // entendue sur tout DSD converti (.42, Locatelli/Abacab, 24/08). Le canal
    // est séquentiel : la reprise à N est exactement la suite du direct, on
    // l'honore donc avec un vrai 206 dont le Content-Range part de N.
    let finite_range_start = req_headers
        .get("Range")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_range_start)
        .filter(|s| wav_length.is_none_or(|len| *s < len));
    let use_partial = finite_range_start.is_some() && wav_length.is_some();

    // Pas d'`Accept-Ranges` sur une conversion : ce serait inviter le renderer
    // à seeker un tuyau. Le contrat annoncé est celui de la DIDL et du HEAD :
    // DLNA.ORG_OP=00, streaming séquentiel. Les 206 ci-dessous restent pour les
    // renderers qui sondent (`bytes=0-`, Marantz) ou reprennent une tranche
    // exacte malgré tout — mieux qu'un 200 menteur, jamais une invitation.
    if let Some(len) = wav_length {
        headers.insert(
            "contentFeatures.dlna.org",
            HeaderValue::from_static(
                "DLNA.ORG_OP=00;DLNA.ORG_FLAGS=01700000000000000000000000000000",
            ),
        );
        match finite_range_start {
            Some(start) => {
                headers.insert("Content-Length", HeaderValue::from(len - start));
                headers.insert(
                    "Content-Range",
                    HeaderValue::from_str(&format!("bytes {start}-{}/{}", len - 1, len)).unwrap(),
                );
            }
            None => {
                headers.insert("Content-Length", HeaderValue::from(len));
            }
        }
    }

    // Radio servie comme un fichier borné. Le lecteur qui refuse le chunké
    // repart après l'en-tête WAV (`bytes=44-` sur ses fichiers locaux) : on
    // honore ce Range en n'émettant que la fin de l'en-tête, puis le direct.
    // Une radio n'a pas de position — au-delà de l'en-tête, « reprendre à N »
    // ne peut vouloir dire que « donne-moi le direct maintenant ».
    let mut bounded_status = StatusCode::OK;
    let mut header_skip: usize = 0;
    if bounded_live {
        let total = tune_core::http::streamer::LIVE_BOUNDED_TOTAL_LEN;
        headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
        let start = req_headers
            .get("Range")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_range_start)
            .filter(|s| *s < total);
        match start {
            Some(start) if start > 0 => {
                header_skip = start.min(44) as usize;
                headers.insert("Content-Length", HeaderValue::from(total - start));
                headers.insert(
                    "Content-Range",
                    HeaderValue::from_str(&format!("bytes {start}-{}/{total}", total - 1)).unwrap(),
                );
                bounded_status = StatusCode::PARTIAL_CONTENT;
            }
            _ => {
                headers.insert("Content-Length", HeaderValue::from(total));
            }
        }
        info!(
            stream_id,
            total,
            range_start = ?start,
            header_skip,
            "radio_bounded_live_response — renderer refuses chunked, serving the file contract"
        );
    }

    // ── Pourquoi une RADIO ouvre le canal ICY sans titre de session ──
    //
    // `StreamSession::track_title` / `track_artist` sont posés à `None` par
    // `StreamSession::new` et ne sont écrits NULLE PART ailleurs du dépôt :
    // ce sont des champs sans mutabilité intérieure sur une structure qui part
    // aussitôt dans un `Arc`. La condition d'origine
    // (`wants_icy && track_title.is_some()`) était donc TOUJOURS fausse :
    // `icy-metaint` n'était jamais annoncé, et le rafraîchissement du titre
    // ajouté par #1473 (`publish_radio_now` → `radio_now`) restait injoignable.
    // C'est ce que montre le journal de Jean Valjean : pas une ligne « icy »
    // (Marantz ND8006 + Radio Paradise, #2161).
    //
    // Une radio n'a de toute façon pas de titre au moment où sa session est
    // créée — il n'existe pas encore. Il arrive plus tard, par le poller, et se
    // relit à chaque bloc dans le registre `radio_now`. C'est exactement le cas
    // où le canal ICY sert : le seul flux dont le titre change en cours de
    // route. On l'ouvre donc sur `is_radio`, et le renderer qui n'a pas
    // demandé `Icy-MetaData: 1` ne voit, lui, strictement aucun changement.
    let has_icy = wants_icy
        && (session.is_radio || session.track_title.is_some() || session.track_artist.is_some());

    if has_icy {
        headers.insert("icy-metaint", HeaderValue::from(ICY_METAINT as u64));
    }

    // Ce que le poller n'avait aucun moyen de savoir : il publie un titre dans
    // `radio_now` sans jamais apprendre si quelqu'un est en mesure de le
    // relire. Le voici noté sous la clé qu'ils partagent (#2991).
    // `bounded_live` complète la note : les blocs s'entrelacent de la même
    // façon, mais la réponse se présente au renderer comme un FICHIER
    // (`Content-Length` fini + `Accept-Ranges`) et non comme un direct. C'est
    // le cas ORDINAIRE — `accepts_chunked_live_stream` ne rend `true` que pour
    // un agent `Lavf` ou absent — et c'est la moitié de la négociation qu'aucun
    // journal ne portait quand un appareil ne suivait pas (#2991).
    tune_core::http::streamer::note_icy_channel(
        stream_id,
        wants_icy,
        has_icy,
        tune_core::http::streamer::VOIE_FLUX,
        bounded_live,
    );

    // Sans cette ligne, ce défaut n'est pas diagnosticable à distance : le
    // journal du testeur ne disait ni si son renderer avait demandé l'ICY, ni
    // si on le lui avait accordé — deux allers-retours pour la même personne.
    let contrat_journal = if bounded_live {
        "fichier borné"
    } else {
        "chunké"
    };
    info!(
        stream_id,
        agent = user_agent.as_deref().unwrap_or("-"),
        wants_icy,
        has_icy,
        is_radio,
        contrat = contrat_journal,
        "icy_metadata_negotiated"
    );
    // Le nom de l'appareil doit suivre le corps du flux : c'est lui qu'on veut
    // lire sur CHAQUE poussée de métadonnées, et non seulement à la connexion.
    let agent_journal = user_agent.clone().unwrap_or_else(|| "-".to_string());

    let sr = session.info.sample_rate;
    let bd = session.info.bit_depth;
    let ch = session.info.channels;
    let dur_ms = session.info.duration_ms;

    // Bloc ICY de repli : celui de la piste au moment de la connexion. Pour un
    // fichier il ne changera jamais, et c'est correct. Pour une RADIO il est
    // reconstruit a chaque emission depuis le titre courant (voir plus bas) —
    // sans quoi le renderer affiche eternellement le morceau qui passait quand
    // il s'est branche (Marantz + Radio Paradise, forum du 10 aout).
    let icy_block = if has_icy {
        build_icy_metadata(
            session.track_artist.as_deref(),
            session.track_title.as_deref(),
            session.cover_url.as_deref(),
        )
    } else {
        vec![0u8]
    };
    let icy_cover = session.cover_url.clone();
    let icy_stream_id = stream_id.to_string();

    let wav_header_included = session
        .wav_header_included
        .load(std::sync::atomic::Ordering::Relaxed);
    let long_wav = is_wav
        && !is_radio
        && tune_core::audio::wav::wav_stream_needs_indeterminate_length(ch, sr, bd, dur_ms);
    let data_ready = session.data_ready.clone();
    // Les six `yield` de cette branche — en-tete WAV, blocs ICY, morceaux de
    // radio, deux vidages de tampon — sont comptes par `corps_compte`.
    let compteur = session.clone();
    let flux = async_stream::stream! {
        // Fenêtre ICY : elle se compte depuis le PREMIER octet du corps, donc
        // en-tête WAV compris (voir `decoupe_icy`). Le compteur vit ici, et non
        // dans la boucle, pour cette seule raison.
        let mut bytes_since_meta: usize = 0;
        // Le bloc à insérer : le titre courant s'il a été publié par le poller,
        // sinon celui de la connexion. Relu à CHAQUE insertion — c'est la seule
        // chose que le renderer verra changer sans qu'on relance quoi que ce soit.
        //
        // La POCHETTE se relit ici au même titre que l'artiste. Elle était prise
        // sur `session.cover_url`, capturé une fois hors de la boucle — un champ
        // qui vaut TOUJOURS `None` (posé par `StreamSession::new`, écrit nulle
        // part), donc `StreamUrl='…'` ne partait jamais et l'écran du renderer
        // gardait la première image (Serge Asselin, RS250A, fil 1529). Le repli
        // sur `icy_cover` reste pour les sessions non-radio, le jour où ce champ
        // sera renseigné.
        //
        // ── Le journal de la POUSSÉE (#2991) ──
        //
        // Jusqu'ici, aucune ligne n'était écrite quand un bloc partait
        // RÉELLEMENT vers l'appareil : `radio_refresh_channel`, côté poller,
        // annonce par où le changement DEVRAIT passer, et `canal_radio` le
        // déduit de deux registres. Un testeur qui répond « la pochette ne
        // change pas » laissait donc le choix entre « aucun bloc n'est parti »
        // et « le bloc est parti sans pochette » — deux corrections opposées,
        // et pas une trace pour les départager. Le bloc étant reconstruit plus
        // de dix fois par seconde, `SuiviBlocIcy` ne laisse passer que le
        // premier puis les CHANGEMENTS : une ligne par morceau.
        // `Mutex` et non `RefCell` : le corps du flux doit être `Send`, et une
        // référence partagée sur une cellule ne l'est pas. Aucune contention —
        // un seul consommateur tient le canal PCM (`claim_channel_consumer`).
        let suivi_icy =
            std::sync::Mutex::new(tune_core::http::streamer::SuiviBlocIcy::nouveau());
        let bloc_icy_courant = || match tune_core::http::streamer::radio_now(&icy_stream_id) {
            Some(np) => {
                let pochette = np.cover.as_deref().or(icy_cover.as_deref());
                let bloc = build_icy_metadata(np.artist.as_deref(), Some(&np.title), pochette);
                if suivi_icy
                    .lock()
                    .is_ok_and(|mut s| s.a_journaliser(&np.title, pochette))
                {
                    info!(
                        stream_id = %icy_stream_id,
                        appareil = %agent_journal,
                        methode = "icy in-band",
                        contrat = contrat_journal,
                        artiste = np.artist.as_deref().unwrap_or("-"),
                        titre = %np.title,
                        pochette = pochette.unwrap_or("-"),
                        octets = bloc.len(),
                        "radio_icy_block_sent"
                    );
                }
                bloc
            }
            None => {
                // Le renderer lit bien des blocs, mais le poller n'a jamais
                // rien publié sous ce `stream_id` : l'appareil affiche
                // éternellement ce qu'il a reçu à sa connexion. C'est l'autre
                // moitié du diagnostic, et elle se taisait aussi.
                if suivi_icy
                    .lock()
                    .is_ok_and(|mut s| s.a_journaliser("(aucun titre publié)", None))
                {
                    warn!(
                        stream_id = %icy_stream_id,
                        appareil = %agent_journal,
                        methode = "icy in-band",
                        contrat = contrat_journal,
                        "radio_icy_block_sent — bloc de repli : le poller n'a publié aucun \
                         titre sous ce stream_id, l'écran restera sur celui de la connexion"
                    );
                }
                icy_block.clone()
            }
        };

        if is_wav && !wav_header_included {
            // Live radio: a Lavf renderer needs the 0xFFFF_FFFF
            // indeterminate-length header to keep reading until the connection
            // closes; a renderer served the file contract gets sizes that match
            // its Content-Length and stay positive as i32 (#1689). Finite
            // tracks keep the sized header.
            let hdr = if is_radio {
                // Wait until the decoder has probed the upstream so the header
                // advertises the TRUE sample rate/channels (FIP is 48000, not
                // the placeholder 44100). Fall back to the StreamInfo values if
                // the decoder hasn't populated them within a short window.
                if session.detected_output_format().is_none() {
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        data_ready.notified(),
                    )
                    .await;
                }
                let (real_sr, real_ch) = session.detected_output_format().unwrap_or((sr, ch));
                if bounded_live {
                    build_wav_header_bounded_live(real_ch, real_sr, bd)
                } else {
                    build_wav_header_streaming(real_ch, real_sr, bd)
                }
            } else {
                build_wav_header(ch, sr, bd, dur_ms)
            };
            // header_skip n'est non nul que sur une reprise `bytes=N-` d'une
            // radio bornée : le lecteur a déjà lu l'en-tête et veut le PCM.
            if header_skip < hdr.len() {
                let entete = &hdr[header_skip..];
                yield Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(entete));
                bytes_since_meta += entete.len();
            }
        }

        if has_icy && !is_radio {
            while let Some(chunk) = session.recv_chunk().await {
                for part in decoupe_icy(&chunk, &mut bytes_since_meta, &bloc_icy_courant) {
                    yield Ok(part);
                }
            }
        } else if is_radio {
            // Radio streams are infinite — yield chunks immediately for
            // real-time playback.  The coalescing buffer used for finite
            // tracks adds latency that is acceptable for Squeezebox/LMS
            // but can cause the browser's <audio> element (or the local
            // output's HTTP reader) to stall waiting for the first data
            // after the WAV header, resulting in silence.
            // The guard counts concurrent consumers and logs how the stream
            // ends (diagnostics for the FIP silent-after-reconnect case).
            let mut guard = RadioConsumerGuard::new(session.clone());

            // Claim sole ownership of the single-consumer PCM channel. A DLNA
            // renderer that re-requests the radio stream (buffer refill /
            // reconnect) WITHOUT closing its first connection used to leave both
            // connections calling recv_chunk(), so each PCM chunk went to
            // whichever connection asked first — the audio was split between the
            // two sockets and the renderer's live playback only got a fraction
            // of the bytes → periodic dropouts (radio_stream_concurrent_consumer
            // on .15). Bumping the epoch supersedes any older consumer; the loop
            // below (subscribe-then-check + biased select) guarantees the older
            // one stops without pulling a further chunk, so no chunk is split,
            // lost, or duplicated at the hand-off.
            let my_epoch = session.claim_channel_consumer();
            loop {
                // Subscribe to the supersede signal and register the waiter
                // BEFORE checking the epoch. The epoch bump in
                // claim_channel_consumer happens-before its notify, so a newer
                // consumer is observed either as a wake here or as a stale epoch
                // in the check below — never lost. See claim_channel_consumer.
                let superseded = session.consumer_supersede.notified();
                tokio::pin!(superseded);
                superseded.as_mut().enable();

                if !session.is_current_channel_consumer(my_epoch) {
                    // A newer connection took over. Hand off WITHOUT consuming
                    // another chunk (biased select never let recv win a race
                    // against this check either).
                    guard.superseded = true;
                    info!(
                        stream_id = %session.id,
                        connected_secs = guard.started.elapsed().as_secs(),
                        "radio_stream_superseded — handed the PCM channel to a \
                         newer connection (renderer reconnected)"
                    );
                    break;
                }

                tokio::select! {
                    biased;
                    // Supersede wins ties: if a chunk is also ready we still
                    // drop the recv future unread, leaving the chunk in the
                    // channel for the new owner.
                    _ = &mut superseded => continue,
                    maybe_chunk = session.recv_chunk() => {
                        match maybe_chunk {
                            // Le direct passe TOUJOURS par cette boucle-ci, ICY
                            // ou pas : c'est elle qui tient le canal PCM à un
                            // seul consommateur. Router la radio vers la boucle
                            // simple ci-dessus pour lui ajouter des
                            // métadonnées aurait rendu les micro-coupures de
                            // `radio_stream_concurrent_consumer` (.15). Le
                            // canal ICY ne fait qu'entrelacer des blocs dans
                            // les octets déjà servis.
                            Some(chunk) => {
                                if has_icy {
                                    for part in decoupe_icy(
                                        &chunk,
                                        &mut bytes_since_meta,
                                        &bloc_icy_courant,
                                    ) {
                                        yield Ok(part);
                                    }
                                } else {
                                    yield Ok(bytes::Bytes::from(chunk));
                                }
                            }
                            None => {
                                // recv returned None → the PCM channel was closed
                                // (all senders, incl. the keep-alive, dropped). A
                                // radio session should stay open across upstream
                                // reconnects, so this is worth surfacing.
                                guard.completed = true;
                                info!(
                                    stream_id = %session.id,
                                    connected_secs = guard.started.elapsed().as_secs(),
                                    "radio_stream_channel_closed — PCM channel ended \
                                     (senders dropped); renderer will see EOF"
                                );
                                break;
                            }
                        }
                    }
                }
            }
        } else {
            // Coalesce small chunks into larger HTTP writes (target >=64 KB).
            // Network outputs like Squeezebox/LMS fetch audio from this HTTP
            // stream.  Yielding many small chunks (~32 KB each from the decoder)
            // causes per-write overhead and can trigger micro-pauses that manifest
            // as audible stuttering/crackling on the player.  Buffering to >=64 KB
            // gives the network renderer more data per TCP segment, reducing the
            // chance of buffer underrun.
            const MIN_HTTP_CHUNK: usize = 65536;
            let mut coalesce_buf = Vec::with_capacity(MIN_HTTP_CHUNK * 2);

            // ── Une seule connexion possède le canal — comme pour les radios ──
            //
            // Le canal PCM ne se lit qu'UNE fois. Un renderer qui sonde avant de
            // jouer (DMP-A8 : `bytes=0-`, `bytes=44-`, `bytes=0-` en 40 ms)
            // laissait plusieurs connexions tirer dessus EN MÊME TEMPS : chaque
            // chunk partait vers l'une OU l'autre, et la connexion de lecture ne
            // recevait qu'une fraction du signal. Affamé, le renderer rejouait
            // son tampon interne — la « boucle de 4-7 secondes » entendue sur
            // tout DSD converti (le DSF servi brut passait par `serve_file`,
            // avec Range, et n'a jamais eu ce défaut ; c'est le repli PCM de
            // #2152 qui a mis les DSD sur ce chemin-ci).
            //
            // Même mécanisme d'époque que les radios : la DERNIÈRE connexion
            // prend le canal, les précédentes s'arrêtent sans consommer un
            // chunk de plus.
            let my_epoch = session.claim_channel_consumer();

            // ── Qui faisait attendre la sortie locale ? ──
            //
            // `stream_producer_ran_dry` ne couvre que le producteur à sec. Si
            // les octets sont DÉJÀ dans le canal et que c'est le corps HTTP
            // qui n'avance plus, le canal reste PLEIN et cette alerte se tait
            // — pendant que la sortie locale, elle, attend sans limite de
            // temps. Les deux attentes se mesurent ici, au même endroit :
            // celle passée DANS `recv_chunk()` (le canal était vide) et celle
            // passée DANS le `yield` (les octets étaient en main, c'est en
            // aval qu'ils n'avançaient pas). Voir
            // `StreamSession::note_delivery_stall`.
            let mut attente_transport = std::time::Duration::ZERO;

            // Nomme le blocage qui ne finit jamais : voir `SentinelleDuCorps`.
            // Elle vit ICI, dans le corps, pour que son `Drop` parte avec lui.
            let _sentinelle = SentinelleDuCorps(session.clone());

            // ── L'en-tête WAV doit survivre aux connexions de sonde ──
            //
            // Sur une conversion, l'en-tête est le premier chunk DU CANAL : la
            // connexion de sonde le consomme et la connexion de lecture ne voit
            // que du PCM nu — injouable. Celui qui le voit passer le met de
            // côté ; toute connexion suivante partant de l'octet 0 le reçoit
            // d'abord. `bytes=44-` dit explicitement « je l'ai déjà » : on ne
            // le renvoie pas.
            let debut_demande = req_headers
                .get("Range")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_range_start);
            let saute_entete = debut_demande.is_some_and(|s| s >= 44);
            if is_wav
                && wav_header_included
                && !saute_entete
                && let Some(entete) = session.wav_header_stash.get()
            {
                yield Ok(bytes::Bytes::from(entete.clone()));
            }

            // ── La reprise annonce N ; le tuyau en est ailleurs ──
            //
            // Le 206 ci-dessus dit `Content-Range: bytes N-…`, et le renderer
            // range les octets reçus À PARTIR DE N. Un canal ne rejoue rien :
            // il rend l'octet où il en est. L'écart s'entend comme un saut sur
            // du PCM, mais il DÉTRUIT un porteur DoP dès qu'il n'est pas un
            // multiple de la trame — mesuré : `bytes=8236-` sur une session
            // DoP stéréo 24 bits rendait les octets de l'offset 44, soit
            // 8192 octets d'écart, 2 modulo la trame de 6 ; le marqueur
            // `0x05`/`0xFA` ne tombait plus sur l'octet de poids fort d'aucun
            // mot, et le DAC jouait le train DSD comme du PCM (#1894).
            //
            // On ne rattrape pas la position, on rattrape la PHASE : au plus
            // `trame - 1` octets jetés une seule fois. Voir `rognage_de_phase`.
            let trame_de_sortie = if is_wav && wav_header_included {
                u64::from(session.info.channels) * u64::from(session.info.bit_depth / 8)
            } else {
                0
            };
            let mut a_remettre_en_phase =
                debut_demande.filter(|_| saute_entete && trame_de_sortie > 1);
            // Le doublon d'en-tête ne se juge que sur le PREMIER bloc du canal :
            // au-delà, `RIFF` au début d'un bloc est de l'audio.
            let mut doublon_d_entete_juge = false;

            loop {
                let superseded = session.consumer_supersede.notified();
                tokio::pin!(superseded);
                superseded.as_mut().enable();

                if !session.is_current_channel_consumer(my_epoch) {
                    info!(
                        stream_id = %session.id,
                        "finite_stream_superseded — le canal passe à une connexion plus récente"
                    );
                    break;
                }

                // ── Ne jamais s'endormir avec des octets en main ──
                //
                // Le tampon de coalescence n'a qu'un rôle : REGROUPER des
                // morceaux DÉJÀ disponibles pour écrire >= 64 Ko d'un coup.
                // Quand le canal est VIDE, il n'y a plus rien à regrouper :
                // attendre les 64 Ko retient ce qu'on a EN PLUS de ce qui
                // manque. En face, la sortie locale est bloquée dans un
                // `reader.read()` sans limite de temps (`outputs/local.rs`,
                // client construit avec `.timeout(None)`) et ne voit RIEN.
                //
                // C'est le motif pour lequel la branche RADIO ci-dessus émet
                // ses morceaux sans les regrouper : « the coalescing buffer
                // used for finite tracks adds latency […] can cause […] the
                // local output's HTTP reader to stall waiting for the first
                // data ». La branche FINIE — celle de TOUTE conversion WAV
                // servie à une sortie locale ou OAAT — n'a jamais reçu la
                // même exemption.
                //
                // Le regroupement est INTACT tant que le producteur est en
                // avance : `buffered > 0` laisse le tampon se remplir et les
                // trames de 64 Ko partent comme avant.
                let remplissage = session.channel_fill().await;
                if let Some((buffered, max)) = remplissage {
                    if session.note_channel_fill(buffered, max) {
                        warn!(
                            stream_id = %session.id,
                            bytes_sent = session
                                .bytes_sent
                                .load(std::sync::atomic::Ordering::Relaxed),
                            channel_max = max,
                            "stream_producer_ran_dry — le canal du flux interne a été plein puis \
                             s'est vidé : le producteur a cessé d'alimenter la session"
                        );
                    }
                    if buffered == 0 && !coalesce_buf.is_empty() {
                        let restant = std::mem::take(&mut coalesce_buf);
                        session.debut_attente_transport();
                        yield Ok(bytes::Bytes::from(restant));
                        attente_transport += session.fin_attente_transport();
                    }
                }

                let avant_recv = tokio::time::Instant::now();
                tokio::select! {
                    biased;
                    _ = &mut superseded => continue,
                    maybe_chunk = session.recv_chunk() => {
                        let attente_producteur = avant_recv.elapsed();
                        if session.note_delivery_stall(attente_producteur, attente_transport) {
                            // `channel_max = 0` ne peut pas décrire un canal
                            // vivant (sa capacité vaut au moins 1) : c'est le
                            // marqueur d'un canal déjà fermé.
                            let (buffered, channel_max) = remplissage.unwrap_or((0, 0));
                            warn!(
                                stream_id = %session.id,
                                attente_producteur_ms = attente_producteur.as_millis() as u64,
                                attente_transport_ms = attente_transport.as_millis() as u64,
                                buffered,
                                channel_max,
                                // Rang du blocage DANS la session : #2952 en
                                // porte deux par piste, et lire « blocage=2 »
                                // évite de croire qu'on tient le premier.
                                blocage = session
                                    .stall_alerts
                                    .load(std::sync::atomic::Ordering::Relaxed),
                                bytes_sent = session
                                    .bytes_sent
                                    .load(std::sync::atomic::Ordering::Relaxed),
                                "stream_delivery_stall — le flux interne s'est arrêté de \
                                 délivrer : `attente_producteur_ms` dit que le canal était vide \
                                 et qu'on attendait le décodeur, `attente_transport_ms` que les \
                                 octets étaient là et ne partaient pas"
                            );
                        }
                        attente_transport = std::time::Duration::ZERO;
                        let Some(mut chunk) = maybe_chunk else {
                            // Canal fermé : fin de piste. Vider ce qui reste.
                            if !coalesce_buf.is_empty() {
                                let restant = std::mem::take(&mut coalesce_buf);
                                yield Ok(bytes::Bytes::from(restant));
                            }
                            break; // fin de flux : plus rien à mesurer.
                        };
                        // ── Deux en-têtes WAV : le nommer, et n'en servir qu'un ──
                        //
                        // Quand la session ne DÉCLARE pas que son producteur
                        // émet l'en-tête, ce corps en a préfixé un plus haut. Si
                        // le canal en apporte un second, le renderer prend 44
                        // octets d'en-tête pour de l'audio et TOUT ce qui suit
                        // est décalé de 44 octets — 2 modulo une trame de 6, la
                        // mort d'un porteur DoP (#1894).
                        //
                        // Un défaut de producteur, mais qui ne doit plus être
                        // SILENCIEUX : il a vécu trois semaines sans laisser une
                        // ligne de journal. On jette le doublon et on le nomme.
                        if is_wav
                            && !wav_header_included
                            && !doublon_d_entete_juge
                            && chunk.len() >= 44
                            && chunk.starts_with(b"RIFF")
                            && &chunk[8..12] == b"WAVE"
                        {
                            doublon_d_entete_juge = true;
                            warn!(
                                stream_id = %session.id,
                                "double_entete_wav — le producteur a émis son propre en-tête WAV \
                                 sans que la session le déclare (`wav_header_included`) : un \
                                 second en-tête a déjà été préfixé. Le doublon est écarté ; sans \
                                 cela tout le flux partait décalé de 44 octets (#1894)"
                            );
                            if chunk.len() == 44 {
                                continue;
                            }
                            chunk.drain(..44);
                        }
                        doublon_d_entete_juge = true;
                        // Où ce bloc se trouve-t-il DANS LE FLUX ? Le compteur
                        // avance de tout ce qui est tiré du canal, en-tête
                        // compris : c'est la position du tuyau.
                        let debut_du_bloc = session
                            .octets_du_canal
                            .fetch_add(chunk.len() as u64, std::sync::atomic::Ordering::Relaxed);
                        // Mettre l'en-tête de côté au passage, pour les
                        // connexions suivantes. `set` n'écrit qu'une fois.
                        if is_wav
                            && wav_header_included
                            && chunk.len() >= 44
                            && chunk.starts_with(b"RIFF")
                            && session.wav_header_stash.get().is_none()
                        {
                            // Progressive decoders emit an unknown-duration header
                            // (~2 GiB), even when StreamInfo knows the full track.
                            // Fix both size fields BEFORE stashing it, so probes
                            // and reconnects receive the same unbounded container.
                            // Preserve the producer's format and every PCM byte.
                            if long_wav
                                && debut_du_bloc == 0
                                && &chunk[8..12] == b"WAVE"
                                && &chunk[12..16] == b"fmt "
                                && chunk[16..20] == 16u32.to_le_bytes()
                                && &chunk[36..40] == b"data"
                            {
                                chunk[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
                                chunk[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
                            }
                            let _ = session.wav_header_stash.set(chunk[..44].to_vec());
                            // La connexion qui a demandé `bytes=44-` ne veut
                            // PAS l'en-tête : on ne transmet que la suite.
                            if saute_entete {
                                if chunk.len() > 44 {
                                    let suite = &chunk[44..];
                                    let garde = rogner_pour_la_phase(
                                        &session,
                                        &mut a_remettre_en_phase,
                                        trame_de_sortie,
                                        debut_du_bloc + 44,
                                        suite,
                                    );
                                    coalesce_buf.extend_from_slice(&suite[garde..]);
                                }
                                while coalesce_buf.len() >= MIN_HTTP_CHUNK {
                                    let flushed: Vec<u8> = coalesce_buf.drain(..MIN_HTTP_CHUNK).collect();
                                    session.debut_attente_transport();
                                    yield Ok(bytes::Bytes::from(flushed));
                                    attente_transport += session.fin_attente_transport();
                                }
                                continue;
                            }
                        }
                        let garde = rogner_pour_la_phase(
                            &session,
                            &mut a_remettre_en_phase,
                            trame_de_sortie,
                            debut_du_bloc,
                            &chunk,
                        );
                        coalesce_buf.extend_from_slice(&chunk[garde..]);
                        while coalesce_buf.len() >= MIN_HTTP_CHUNK {
                            let flushed: Vec<u8> = coalesce_buf.drain(..MIN_HTTP_CHUNK).collect();
                            session.debut_attente_transport();
                            yield Ok(bytes::Bytes::from(flushed));
                            attente_transport += session.fin_attente_transport();
                        }
                    }
                }
            }
        }
    };
    let body = corps_compte(flux, compteur);

    let status = if use_partial {
        StatusCode::PARTIAL_CONTENT
    } else {
        // 206 déjà décidé plus haut pour une reprise de radio bornée, sinon 200.
        bounded_status
    };

    (status, headers, body).into_response()
}

// ─── File serving with Range ────────────────────────────────────

/// Ce qu'un en-tête `Range` demande VRAIMENT d'un fichier de taille connue.
///
/// Séparé de [`serve_file`] parce que c'est la seule façon d'en éprouver les
/// bords : les cas fautifs ci-dessous sont des en-têtes, pas des fichiers.
#[derive(Debug, PartialEq, Eq)]
enum DemandeDeRange {
    /// Bornes INCLUSES, toutes deux garanties dans le fichier (`debut <= fin`
    /// et `fin < taille`) : le `Content-Length` en découle sans soustraction
    /// risquée.
    Tranche { debut: u64, fin: u64 },
    /// L'en-tête est bien formé mais ne désigne aucun octet existant :
    /// la réponse due est un **416** avec `Content-Range: bytes */taille`.
    Insatisfiable,
    /// En-tête absent, d'une autre unité, ou syntaxiquement invalide. La RFC
    /// 9110 §14.2 impose alors de l'IGNORER : on sert le fichier entier en
    /// 200, comme s'il n'y avait pas eu de `Range`.
    Totalite,
}

/// Lit un en-tête `Range` pour un fichier de `taille` octets.
///
/// # Les deux défauts que cette fonction ferme
///
/// L'ancien code tenait en trois lignes, et chacune portait un défaut :
///
/// ```ignore
/// let start = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
/// let end   = parts.get(1)….unwrap_or(file_size - 1);
/// let length = end - start + 1;
/// ```
///
/// 1. **`bytes=N-` avec `N >= taille`.** `end` vaut `taille - 1`, donc
///    `end < start`, et `end - start + 1` DÉBORDE par le bas. `overflow-checks`
///    étant éteint en `release` (voir `[profile.release]` à la racine), rien ne
///    panique : le serveur annonce un `Content-Length` de l'ordre de 2^64 et un
///    `Content-Range: bytes 9999-42/43` qui ne veut rien dire. Le renderer
///    attend alors des octets qui ne viendront jamais. La réponse due est un
///    416, et c'est ce que rend désormais [`DemandeDeRange::Insatisfiable`].
/// 2. **`bytes=-N` (suffixe).** `split('-')` rend `["", "N"]` : `""` ne se
///    parse pas, `start` retombait sur `0` et le serveur servait les N
///    PREMIERS octets là où le client demandait les N DERNIERS. Un client
///    poli n'en envoie pas — mais celui qui en envoie recevait le début d'un
///    fichier en croyant en lire la fin.
///
/// `taille == 0` referme au passage le `file_size - 1` de la ligne 2, qui
/// débordait lui aussi sur un fichier vide : plus aucun octet n'est
/// satisfaisable, donc 416.
///
/// ⚠️ Hors sujet ici, et délibérément : rien n'aligne `debut` sur la grille
/// des trames PCM. C'est l'hypothèse 1 de l'enquête #4455 (« souffle
/// soudain »), que la sonde `stream_range_hors_trame` de la v0.9.157 doit
/// trancher sur PIÈCES. Cette fonction sert l'octet demandé, comme avant.
fn interpreter_range(entete: &str, taille: u64) -> DemandeDeRange {
    // L'unité est insensible à la casse (RFC 9110 §14.1) ; une autre unité
    // que `bytes` doit être ignorée, pas rejetée.
    let Some(specs) = entete
        .split_once('=')
        .filter(|(unite, _)| unite.trim().eq_ignore_ascii_case("bytes"))
        .map(|(_, reste)| reste)
    else {
        return DemandeDeRange::Totalite;
    };
    // Une demande multi-tranches est légale ; y répondre par une seule
    // tranche l'est aussi (§14.2 : « MAY … send only the first »), et le
    // corps `multipart/byteranges` n'aurait aucun usage ici. On lit donc la
    // première, au lieu de l'ancien `split('-')` qui digérait « 99,200 » en
    // silence.
    let spec = specs.split(',').next().unwrap_or("").trim();
    let Some((avant, apres)) = spec.split_once('-') else {
        return DemandeDeRange::Totalite;
    };
    let (avant, apres) = (avant.trim(), apres.trim());

    // Fichier vide : aucun octet n'existe, donc aucune tranche n'est
    // satisfaisable — y compris le `bytes=0-` d'une sonde.
    if taille == 0 {
        return DemandeDeRange::Insatisfiable;
    }
    let dernier = taille - 1;

    if avant.is_empty() {
        // Suffixe `bytes=-N` : les N DERNIERS octets. `bytes=-0` ne désigne
        // rien (§14.1.2) → 416.
        let Ok(n) = apres.parse::<u64>() else {
            return DemandeDeRange::Totalite;
        };
        if n == 0 {
            return DemandeDeRange::Insatisfiable;
        }
        return DemandeDeRange::Tranche {
            debut: taille.saturating_sub(n),
            fin: dernier,
        };
    }

    let Ok(debut) = avant.parse::<u64>() else {
        return DemandeDeRange::Totalite;
    };
    if apres.is_empty() {
        // `bytes=N-` : jusqu'à la fin. Au-delà du dernier octet, 416.
        return if debut > dernier {
            DemandeDeRange::Insatisfiable
        } else {
            DemandeDeRange::Tranche {
                debut,
                fin: dernier,
            }
        };
    }
    let Ok(fin_demandee) = apres.parse::<u64>() else {
        return DemandeDeRange::Totalite;
    };
    // `first-pos > last-pos` rend la spec INVALIDE, pas insatisfaisable :
    // l'en-tête entier doit être ignoré (§14.1.2).
    if fin_demandee < debut {
        return DemandeDeRange::Totalite;
    }
    if debut > dernier {
        return DemandeDeRange::Insatisfiable;
    }
    DemandeDeRange::Tranche {
        debut,
        fin: fin_demandee.min(dernier),
    }
}

/// La réponse due à une demande insatisfaisable : 416 + `Content-Range:
/// bytes */taille`, qui DIT au renderer la taille réelle pour qu'il refasse
/// sa demande au bon endroit (RFC 9110 §15.5.17).
fn reponse_416(taille: u64) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Range",
        HeaderValue::from_str(&format!("bytes */{taille}")).unwrap(),
    );
    headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
    (StatusCode::RANGE_NOT_SATISFIABLE, headers).into_response()
}

async fn serve_file(
    path: &str,
    info: &StreamInfo,
    req_headers: &HeaderMap,
    session: std::sync::Arc<StreamSession>,
) -> Response {
    // On-the-fly M4A faststart: when present, we serve a virtual file
    // (ftyp + patched moov, from memory) followed by the original mdat, so the
    // renderer reads metadata up front. The virtual size equals the real size.
    let faststart = session
        .faststart
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let file_path = std::path::Path::new(path);
    let disk_size = match tokio::fs::metadata(file_path).await {
        Ok(m) => m.len(),
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let file_size = faststart.as_ref().map(|m| m.total).unwrap_or(disk_size);

    let range_header = req_headers
        .get("Range")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let demande = range_header
        .as_deref()
        .map(|r| interpreter_range(r, file_size))
        .unwrap_or(DemandeDeRange::Totalite);

    if let DemandeDeRange::Insatisfiable = demande {
        warn!(
            stream_id = %session.id,
            range = range_header.as_deref().unwrap_or("-"),
            file_size,
            "stream_range_insatisfiable — 416 ; l'ancien code annonçait ici un \
             Content-Length aberrant né d'une soustraction u64 qui débordait"
        );
        return reponse_416(file_size);
    }

    if let DemandeDeRange::Tranche {
        debut: start,
        fin: end,
    } = demande
    {
        let length = end - start + 1;
        noter_range_hors_trame(&session, info, start);

        let mut headers = HeaderMap::new();
        headers.insert(
            "Content-Type",
            content_type_servi(&session.id, &info.mime_type),
        );
        headers.insert("Content-Length", HeaderValue::from(length));
        headers.insert(
            "Content-Range",
            HeaderValue::from_str(&format!("bytes {start}-{end}/{file_size}")).unwrap(),
        );
        headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
        headers.insert(
            "transferMode.dlna.org",
            HeaderValue::from_static("Interactive"),
        );
        headers.insert("Connection", HeaderValue::from_static("keep-alive"));
        headers.insert(
            "contentFeatures.dlna.org",
            HeaderValue::from_static(
                "DLNA.ORG_OP=01;DLNA.ORG_FLAGS=01700000000000000000000000000000",
            ),
        );

        // Track served bytes so the poller can tell an actively-fetching
        // renderer from a genuinely-stalled one (fixes false force-stop of
        // DLNA renderers that report Stopped while streaming — Linn, RS130).
        let body = build_file_body(
            faststart.clone(),
            path.to_string(),
            start,
            length,
            session.clone(),
            debit_nominal_octets_par_seconde(
                &info.format,
                info.sample_rate,
                info.bit_depth,
                info.channels,
            ),
        );
        return (StatusCode::PARTIAL_CONTENT, headers, body).into_response();
    }

    // Full file
    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        content_type_servi(&session.id, &info.mime_type),
    );
    headers.insert("Content-Length", HeaderValue::from(file_size));
    headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
    headers.insert(
        "transferMode.dlna.org",
        HeaderValue::from_static("Interactive"),
    );
    headers.insert("Connection", HeaderValue::from_static("keep-alive"));
    headers.insert(
        "contentFeatures.dlna.org",
        HeaderValue::from_static("DLNA.ORG_OP=01;DLNA.ORG_FLAGS=01700000000000000000000000000000"),
    );

    let body = build_file_body(
        faststart,
        path.to_string(),
        0,
        file_size,
        session.clone(),
        debit_nominal_octets_par_seconde(
            &info.format,
            info.sample_rate,
            info.bit_depth,
            info.channels,
        ),
    );
    (StatusCode::OK, headers, body).into_response()
}

/// Taille de l'en-tête que TOUS les WAV de Tune portent
/// (`tune_core::audio::wav::build_wav_header*` rendent un `[u8; 44]`).
const EN_TETE_WAV: u64 = 44;

/// Reprises signalées au niveau WARN par session ; au-delà, DEBUG. Un
/// renderer qui boucle en produirait des centaines par minute.
const RANGES_HORS_TRAME_AU_JOURNAL: u32 = 3;

/// #4645 — de combien l'avance de livraison doit reculer avant qu'on le dise,
/// et combien de fois on le dit au niveau WARN.
///
/// Sevy Tabroc (fil 1871, darTZeel LHC-208 en DLNA, 0.9.159) : « parfois, il y
/// a de micro coupure durant la lecture d'un morceau ». Son journal n'en porte
/// AUCUNE trace — ni WARN, ni ERROR, pendant les quinze minutes jouées. La
/// famine de l'anneau ne couvre que la sortie LOCALE (0 évènement sur 0 servi :
/// elle n'a pas joué), et sur le chemin fichier entier la source n'est pas lue
/// pendant la lecture. Rien, nulle part, ne datait une micro-coupure.
const PAS_DE_PERTE_MS: i64 = 1_000;
/// Pertes signalées au niveau WARN par session ; au-delà, DEBUG. Même motif
/// que `RANGES_HORS_TRAME_AU_JOURNAL` : un renderer qui hoquette en continu
/// en produirait des centaines.
const PERTES_AU_JOURNAL: u32 = 3;

/// Débit nominal d'un flux PCM servi en WAV, en octets par seconde.
///
/// `None` hors WAV, et c'est délibéré : sur un format compressé la
/// correspondance octets ↔ temps n'est pas linéaire, et une avance calculée
/// dessus mentirait. Mieux vaut ne rien dire que dire un chiffre faux — même
/// contrat que le `debit_kio_s` sous la milliseconde.
fn debit_nominal_octets_par_seconde(
    format: &str,
    sample_rate: u32,
    bit_depth: u16,
    channels: u16,
) -> Option<u32> {
    if format != "wav" {
        return None;
    }
    let octets_par_trame = u32::from(channels) * u32::from(bit_depth / 8);
    let debit = sample_rate.checked_mul(octets_par_trame)?;
    (debit > 0).then_some(debit)
}

/// De combien de millisecondes d'audio la livraison est-elle EN AVANCE sur
/// l'horloge ? Négatif : elle a pris du retard.
///
/// C'est la quantité que le renderer a devant lui. Tant qu'elle reste stable,
/// il joue sans manquer de rien ; quand elle recule, il a cessé de jouer
/// pendant tout ce qu'elle a perdu.
fn avance_de_livraison_ms(octets_servis: u64, octets_par_seconde: u32, elapsed_ms: u64) -> i64 {
    let audio_ms = (u128::from(octets_servis) * 1000 / u128::from(octets_par_seconde)) as i64;
    audio_ms - elapsed_ms as i64
}

/// Terrain perdu depuis le sommet de l'avance.
///
/// C'est LA mesure qui manquait. Une avance absolue ne dit rien d'un renderer
/// qui tire au rythme exact de la lecture sans jamais prendre d'avance — le
/// profil du darTZeel, justement : l'instrument serait resté muet sur le cas
/// qui l'a motivé. Le recul depuis le sommet, lui, vaut le temps pendant
/// lequel le renderer n'a rien eu à jouer, qu'il ait tampon ou non.
fn terrain_perdu_ms(avance_max_ms: i64, avance_ms: i64) -> i64 {
    (avance_max_ms - avance_ms).max(0)
}

/// #4455 — de combien d'octets une reprise `Range: bytes=N-` d'un WAV tombe
/// À CÔTÉ de la grille des trames PCM. `0` : alignée, ou dans l'en-tête, ou
/// trame inconnue.
///
/// Sevy Tabroc (fil 1843, darTZeel LHC-208 en DLNA, ALAC → WAV 48 kHz 24 bits) :
/// « soudainement est apparu un souffle alors que le morceau continue à être
/// joué ». Rien dans le journal joint : les 200 dernières lignes ne portent
/// aucune ligne du chemin de service du fichier. Or c'est le SEUL étage qui
/// puisse changer quelque chose au milieu d'un fichier déjà transcodé en
/// entier — le DSP, le gain et l'encodage sont faits avant la première note.
/// Un renderer qui reprend le flux à un offset qu'il a calculé lui-même et
/// qui n'est pas un multiple de la trame (6 octets en 24 bits stéréo) lit
/// dès lors l'octet de poids faible comme un octet de poids fort : tous les
/// mots sont déphasés, le signal devient un bruit sous lequel la musique
/// reste reconnaissable — exactement la famille que `rognage_de_phase`
/// documente pour les tuyaux (#1894). Le serveur sert ici l'octet demandé,
/// comme il le doit ; il le DIT désormais, pour que le prochain journal
/// tranche.
fn decalage_de_trame(debut: u64, en_tete: u64, trame: u64) -> u64 {
    if trame <= 1 || debut <= en_tete {
        return 0;
    }
    (debut - en_tete) % trame
}

/// Trace une reprise hors trame sur une session de fichier WAV — WARN les
/// premières fois, DEBUG ensuite (voir [`decalage_de_trame`]).
fn noter_range_hors_trame(session: &StreamSession, info: &StreamInfo, debut: u64) {
    if info.format != "wav" {
        return;
    }
    let trame = u64::from(info.channels) * u64::from(info.bit_depth / 8);
    let decalage = decalage_de_trame(debut, EN_TETE_WAV, trame);
    if decalage == 0 {
        return;
    }
    let occurrence = session
        .ranges_hors_trame
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        + 1;
    if occurrence <= RANGES_HORS_TRAME_AU_JOURNAL {
        warn!(
            stream_id = %session.id,
            debut,
            trame,
            decalage,
            occurrence,
            bit_depth = info.bit_depth,
            channels = info.channels,
            "stream_range_hors_trame — le renderer reprend le WAV entre deux trames ; \
             servi tel quel, mais ses mots PCM seront déphasés (#4455)"
        );
    } else {
        debug!(
            stream_id = %session.id,
            debut,
            trame,
            decalage,
            occurrence,
            "stream_range_hors_trame"
        );
    }
}

/// Stream `length` bytes starting at virtual offset `start` of a file session.
/// With a faststart map the virtual file is `header (ftyp+moov, in memory)` then
/// the original file's mdat body; without one it's the plain file. Byte counting
/// feeds the poller's actively-fetching heuristic.
/// Envelopper un flux de sortie pour compter ce qu'il sert reellement.
///
/// `bytes_sent` n'etait incremente que dans `build_file_body` — le chemin
/// FICHIER. Radio et mandataire n'y passent pas : leur compteur restait a zero
/// quels que soient les octets livres. Or `output_reach` (routes/zones.rs) en
/// deduit « personne n'ecoute », et le diagnostic de zone affiche le meme
/// chiffre : une zone navigateur jouant une radio etait declaree sans onglet
/// pendant que l'onglet jouait (Bilou, #1841).
///
/// On compte a la SORTIE du flux plutot qu'a chaque `yield` : tous les
/// morceaux passent par la, y compris ceux qu'on ajoutera. Un compteur qu'il
/// faut penser a mettre a jour finit toujours par mentir quelque part.
fn corps_compte<S>(flux: S, compteur: std::sync::Arc<StreamSession>) -> Body
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, std::io::Error>> + Send + 'static,
{
    Body::from_stream(futures_util::StreamExt::map(flux, move |morceau| {
        if let Ok(ref o) = morceau {
            compteur
                .bytes_sent
                .fetch_add(o.len() as u64, std::sync::atomic::Ordering::Relaxed);
        }
        morceau
    }))
}

/// Chronometre du segment que le journal ne couvrait pas.
///
/// #2352 — la mesure du 03/09/2026 (journal de Dominique COMET, fil 1653,
/// Tune 0.9.132, `DirettaRenderer/1.0`) a etabli deux bornes : Tune envoie son
/// `Play` entre 129 ms et 1244 ms (`playback_timing`), et le renderer ouvre le
/// flux HTTP dans les 2 ms (`stream_request`). Les « plus de 30 secondes »
/// vecues se jouent donc **apres le premier octet servi** — et AUCUNE ligne du
/// journal ne couvrait ce segment : `build_file_body` incrementait
/// `bytes_sent` sans jamais dire en combien de temps.
///
/// Ce que cette ligne separe, et que rien ne separait :
///
/// * `premier_octet_ms` eleve, debit ensuite normal ⇒ Tune a mis du temps a
///   OUVRIR et lire la source (montage NAS, fichier temporaire de
///   transcodage). La lenteur est en amont du renderer.
/// * `premier_octet_ms` immediat, `debit_kio_s` bas ⇒ c'est le renderer qui
///   tire lentement : le corps est servi par contre-pression HTTP, donc le
///   debit mesure ici est **celui que le consommateur impose**, pas une
///   capacite de Tune.
/// * `complet=false` ⇒ le renderer a laché la connexion avant la fin.
///
/// Le `Drop` est deliberé : il couvre la connexion abandonnee en cours de
/// route aussi bien que le service mene a son terme. Une piste que le renderer
/// abandonne au bout de 30 s ne laissait, elle non plus, aucune trace.
/// 🔴 #4645 — COMMENT le service du corps s'est terminé.
///
/// `service_fichier_termine … complet=false` ne disait pas qui avait lâché.
/// Le générateur du corps a quatre sorties et trois d'entre elles sont les
/// siennes ; la quatrième — de loin la plus probable — est qu'il n'a jamais
/// rendu la main : hyper a détruit le corps parce que la connexion est
/// partie. Les deux cas s'impriment aujourd'hui exactement pareil, et c'est
/// ce qui bloque l'instruction de ce ticket : « la connexion HTTP du WAV se
/// ferme à 87,7 % », sans que rien ne dise si c'est le renderer, le réseau,
/// un ordre de Tune, ou le serveur à court de fichier.
///
/// ⛔ Ce champ ne NOMME pas le coupable d'une fin `ConsommateurParti` : il
/// sépare « le serveur a arrêté d'émettre » de « on a cessé de l'écouter ».
/// C'est la moitié qui manquait ; l'autre se lit dans le journal DLNA autour
/// du même horodatage.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FinDuService {
    /// Tout le corps annoncé est parti. C'est la seule fin normale.
    Complet,
    /// Le fichier s'est terminé AVANT la longueur annoncée (`read` → `Ok(0)`).
    /// Le serveur a promis plus qu'il n'avait : `Content-Length` ment.
    FichierPlusCourt,
    /// Ouverture, positionnement ou lecture en échec — la ligne `file_*_error`
    /// qui précède en donne la cause.
    Erreur,
    /// 🔴 Le générateur n'a JAMAIS rendu la main : il a été détruit au milieu
    /// d'un `yield`. Le corps n'est pas parti de lui-même, on a cessé de le
    /// lire — renderer qui referme, lien qui tombe, ou Tune qui pousse une
    /// nouvelle URI sur le même renderer (`dlna_set_uri_ok`, `dlna_stop`).
    ConsommateurParti,
}

impl FinDuService {
    fn etiquette(self) -> &'static str {
        match self {
            Self::Complet => "complet",
            Self::FichierPlusCourt => "fichier_plus_court",
            Self::Erreur => "erreur",
            Self::ConsommateurParti => "consommateur_parti",
        }
    }
}

struct ChronoServiceFichier {
    stream_id: String,
    demande: u64,
    servis: u64,
    debut: std::time::Instant,
    premier_octet_ms: Option<u64>,
    /// #4645 — débit nominal du flux, quand il est connu (WAV seulement).
    /// `None` : aucune avance n'est calculée et aucun champ n'est publié.
    octets_par_seconde: Option<u32>,
    /// Sommet de l'avance de livraison, et son creux.
    avance_max_ms: i64,
    avance_min_ms: i64,
    /// Dernier recul déjà porté au journal, pour n'annoncer que les pas
    /// suivants et non chaque octet.
    perte_signalee_ms: i64,
    /// Nombre de reculs franchis.
    pertes: u32,
    /// #4645 — comment le service s'est terminé. Posé à `ConsommateurParti`
    /// d'emblée : c'est la seule fin que le générateur ne peut pas écrire
    /// lui-même, puisqu'il ne reprend jamais la main pour le faire.
    fin: FinDuService,
}

impl ChronoServiceFichier {
    fn new(stream_id: String, demande: u64, octets_par_seconde: Option<u32>) -> Self {
        Self {
            stream_id,
            demande,
            servis: 0,
            debut: std::time::Instant::now(),
            premier_octet_ms: None,
            octets_par_seconde,
            avance_max_ms: 0,
            avance_min_ms: 0,
            perte_signalee_ms: 0,
            pertes: 0,
            fin: FinDuService::ConsommateurParti,
        }
    }

    /// #4645 — noter COMMENT le service s'est terminé.
    ///
    /// Une méthode et non une affectation nue : la dernière, posée à la fin du
    /// générateur, est signalée `unused_assignments` par rustc — il ne voit pas
    /// que le champ est lu par `Drop`, la transformation du générateur lui
    /// cachant la portée réelle de `chrono`.
    fn noter_fin(&mut self, fin: FinDuService) {
        self.fin = fin;
    }

    fn compter(&mut self, n: u64) {
        if self.premier_octet_ms.is_none() {
            self.premier_octet_ms = Some(self.debut.elapsed().as_millis() as u64);
        }
        self.servis += n;
        self.mesurer_le_terrain();
    }

    /// Suit l'avance de livraison et porte au journal chaque pas de terrain
    /// perdu (#4645).
    ///
    /// Une PAUSE du renderer produit la même forme : il cesse de tirer,
    /// l'avance recule. La ligne ne dit donc pas « micro-coupure », elle dit
    /// « la livraison a perdu du terrain » — c'est l'état de la zone, côté
    /// sondeur, qui tranche entre les deux. Nommer plus fort que ce qu'on
    /// mesure ferait de cette ligne un faux témoin.
    fn mesurer_le_terrain(&mut self) {
        let Some(octets_par_seconde) = self.octets_par_seconde else {
            return;
        };
        let elapsed_ms = self.debut.elapsed().as_millis() as u64;
        let avance = avance_de_livraison_ms(self.servis, octets_par_seconde, elapsed_ms);
        self.avance_max_ms = self.avance_max_ms.max(avance);
        self.avance_min_ms = self.avance_min_ms.min(avance);
        let perte = terrain_perdu_ms(self.avance_max_ms, avance);
        if perte < self.perte_signalee_ms + PAS_DE_PERTE_MS {
            return;
        }
        self.perte_signalee_ms = perte;
        self.pertes += 1;
        if self.pertes <= PERTES_AU_JOURNAL {
            warn!(
                stream_id = %self.stream_id,
                perte_ms = perte,
                avance_ms = avance,
                avance_max_ms = self.avance_max_ms,
                octets = self.servis,
                elapsed_ms,
                occurrence = self.pertes,
                "service_fichier_perd_du_terrain — le renderer a cessé de tirer \
                 pendant tout ce que l'avance a perdu (pause comprise) ; \
                 l'état de la zone tranche (#4645)"
            );
        } else {
            debug!(
                stream_id = %self.stream_id,
                perte_ms = perte,
                avance_ms = avance,
                elapsed_ms,
                occurrence = self.pertes,
                "service_fichier_perd_du_terrain"
            );
        }
    }
}

impl Drop for ChronoServiceFichier {
    fn drop(&mut self) {
        let elapsed_ms = self.debut.elapsed().as_millis() as u64;
        // Sous la milliseconde, le quotient s'envole : une rafale d'amorcage
        // rapportee a 0 ms donnerait un debit a cinq chiffres. On ne publie
        // pas un chiffre qu'on n'a pas mesure — meme contrat que le
        // `bitrate_kbps` de `network-health` (#2275, f1b8b396), qui rend
        // `null` plutot que de remplir le silence. Un `None` n'imprime PAS le
        // champ : la ligne dit alors « je ne sais pas », pas « zero ».
        let debit_kio_s = if elapsed_ms > 0 {
            Some(
                ((self.servis as f64 / 1024.0) / (elapsed_ms as f64 / 1000.0) * 10.0).round()
                    / 10.0,
            )
        } else {
            None
        };
        info!(
            stream_id = %self.stream_id,
            octets = self.servis,
            demande = self.demande,
            premier_octet_ms = self.premier_octet_ms,
            elapsed_ms,
            debit_kio_s,
            complet = self.servis >= self.demande,
            // #4645 — le bilan du terrain. `None` hors WAV : un `None`
            // n'imprime PAS le champ, la ligne dit « je ne sais pas »
            // plutôt que « zéro », comme `debit_kio_s` sous la ms.
            avance_max_ms = self.octets_par_seconde.map(|_| self.avance_max_ms),
            avance_min_ms = self.octets_par_seconde.map(|_| self.avance_min_ms),
            perte_ms = self
                .octets_par_seconde
                .map(|_| terrain_perdu_ms(self.avance_max_ms, self.avance_min_ms)),
            pertes = self.octets_par_seconde.map(|_| self.pertes),
            // #4645 — LE discriminant : le corps s'est-il arrêté de
            // lui-même, ou a-t-on cessé de le lire ?
            fin = self.fin.etiquette(),
            "service_fichier_termine"
        );
    }
}

fn build_file_body(
    faststart: Option<tune_core::audio::faststart::FaststartMap>,
    path: String,
    start: u64,
    length: u64,
    byte_counter: std::sync::Arc<StreamSession>,
    octets_par_seconde: Option<u32>,
) -> Body {
    use std::sync::atomic::Ordering::Relaxed;
    let mut chrono = ChronoServiceFichier::new(byte_counter.id.clone(), length, octets_par_seconde);
    Body::from_stream(async_stream::stream! {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let mut remaining = length;
        let mut vpos = start;

        if let Some(map) = faststart {
            let header_len = map.header.len() as u64;
            // 1) Header region (ftyp + patched moov) served from memory.
            while remaining > 0 && vpos < header_len {
                let n = ((header_len - vpos).min(remaining)) as usize;
                let s = vpos as usize;
                byte_counter.bytes_sent.fetch_add(n as u64, Relaxed);
                chrono.compter(n as u64);
                yield Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(&map.header[s..s + n]));
                vpos += n as u64;
                remaining -= n as u64;
            }
            // 2) Body region (original file's mdat) mapped by offset.
            if remaining > 0 {
                match tokio::fs::File::open(&path).await {
                    Ok(mut file) => {
                        let file_off = map.body_src_start + (vpos - header_len);
                        if let Err(e) = file.seek(std::io::SeekFrom::Start(file_off)).await {
                            warn!(error = %e, "file_seek_error");
                            chrono.noter_fin(FinDuService::Erreur);
                            return;
                        }
                        let mut buf = vec![0u8; 65536];
                        while remaining > 0 {
                            let to_read = (remaining as usize).min(buf.len());
                            match file.read(&mut buf[..to_read]).await {
                                Ok(0) => { chrono.noter_fin(FinDuService::FichierPlusCourt); break; }
                                Ok(n) => {
                                    remaining -= n as u64;
                                    byte_counter.bytes_sent.fetch_add(n as u64, Relaxed);
                                    chrono.compter(n as u64);
                                    yield Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(&buf[..n]));
                                }
                                Err(e) => { warn!(error = %e, "file_read_error"); chrono.noter_fin(FinDuService::Erreur); break; }
                            }
                        }
                    }
                    Err(e) => { warn!(error = %e, "file_open_error"); chrono.noter_fin(FinDuService::Erreur); }
                }
            }
        } else {
            match tokio::fs::File::open(&path).await {
                Ok(mut file) => {
                    if let Err(e) = file.seek(std::io::SeekFrom::Start(start)).await {
                        warn!(error = %e, "file_seek_error");
                        chrono.noter_fin(FinDuService::Erreur);
                        return;
                    }
                    let mut buf = vec![0u8; 65536];
                    while remaining > 0 {
                        let to_read = (remaining as usize).min(buf.len());
                        match file.read(&mut buf[..to_read]).await {
                            Ok(0) => { chrono.noter_fin(FinDuService::FichierPlusCourt); break; }
                            Ok(n) => {
                                remaining -= n as u64;
                                byte_counter.bytes_sent.fetch_add(n as u64, Relaxed);
                                chrono.compter(n as u64);
                                yield Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(&buf[..n]));
                            }
                            Err(e) => { warn!(error = %e, "file_read_error"); chrono.noter_fin(FinDuService::Erreur); break; }
                        }
                    }
                }
                Err(e) => { warn!(error = %e, "file_open_error"); chrono.noter_fin(FinDuService::Erreur); }
            }
        }
        // #4645 — le générateur rend la main de lui-même : tout ce qui était
        // annoncé est parti. Cette ligne ne s'exécute que si aucune sortie
        // anticipée ne l'a précédée — et surtout, elle NE S'EXÉCUTE PAS quand
        // hyper détruit le corps au milieu d'un `yield`, ce qui est
        // exactement la distinction cherchée.
        if remaining == 0 {
            chrono.noter_fin(FinDuService::Complet);
        }
    })
}

/// Parse the start byte of an HTTP `Range` header value like `bytes=N-` or
/// `bytes=N-M`. Returns `None` for an open `bytes=-N` (suffix) range or a
/// malformed value.
fn parse_range_start(range: &str) -> Option<u64> {
    let spec = range.strip_prefix("bytes=")?;
    let start = spec.split('-').next()?.trim();
    if start.is_empty() {
        return None;
    }
    start.parse::<u64>().ok()
}

/// Combien d'octets rogner en tête du prochain bloc pour que l'octet livré
/// tombe LÀ OÙ LE RENDERER L'ATTEND dans sa grille de trames.
///
/// Une session de conversion est un tuyau : elle ne rejoue pas un octet passé.
/// Le corps HTTP honore pourtant les reprises `Range: bytes=N-` par un vrai 206
/// — sans quoi l'Eversolo DMP-A8 jette la réponse et redemande le même offset
/// en boucle. Le 206 annonce N ; le tuyau, lui, en est à `offset_reel`.
///
/// Le renderer place les octets reçus à partir de N. Tant que l'écart
/// `N - offset_reel` est un multiple de la trame, il n'entend qu'un saut. S'il
/// ne l'est pas, TOUS les mots sont déphasés — et sur un porteur DoP c'est
/// fatal : le marqueur `0x05`/`0xFA` vit dans l'octet de poids fort du mot de
/// 24 bits, il ne tombe plus au bon endroit, le DAC ne verrouille pas en DSD et
/// joue le train DSD comme du PCM, c'est-à-dire du bruit blanc (#1894).
///
/// On ne rattrape donc PAS la position — un tuyau ne le peut pas — mais la
/// PHASE, en jetant au plus `trame - 1` octets. Le saut résiduel est celui
/// qu'on avait déjà ; le porteur, lui, redevient lisible.
///
/// `trame <= 1` (sortie 8 bits mono, format inconnu) : rien à remettre en
/// phase, tout octet est une trame.
fn rognage_de_phase(offset_annonce: u64, offset_reel: u64, trame: u64) -> usize {
    if trame <= 1 {
        return 0;
    }
    (i128::from(offset_annonce) - i128::from(offset_reel)).rem_euclid(i128::from(trame)) as usize
}

/// Applique [`rognage_de_phase`] au premier bloc livré d'une reprise, et le
/// trace. Rend le nombre d'octets à sauter en tête de `bloc`.
///
/// La dette est consommée dès qu'elle est payable ; un bloc plus court que le
/// rognage la reporte au suivant, où l'offset réel aura avancé d'autant — le
/// calcul reste juste sans mémoire supplémentaire.
fn rogner_pour_la_phase(
    session: &StreamSession,
    a_remettre_en_phase: &mut Option<u64>,
    trame: u64,
    offset_reel: u64,
    bloc: &[u8],
) -> usize {
    let Some(offset_annonce) = *a_remettre_en_phase else {
        return 0;
    };
    let rognage = rognage_de_phase(offset_annonce, offset_reel, trame);
    if rognage >= bloc.len() {
        // Bloc trop court pour payer la dette : on le jette en entier et on
        // garde la dette. Ne peut arriver que sur un bloc de moins de 6 octets.
        return bloc.len();
    }
    *a_remettre_en_phase = None;
    if rognage > 0 {
        info!(
            stream_id = %session.id,
            offset_annonce,
            offset_reel,
            trame,
            rognage,
            "reprise_remise_en_phase — le 206 annonce un offset que le canal n'a plus ; \
             la phase de trame est rétablie pour que le porteur reste lisible (#1894)"
        );
    }
    rognage
}

// ─── HTTPS→HTTP proxy ───────────────────────────────────────────

/// Max number of transparent upstream re-connections after a mid-stream
/// body error before we give up and end the response.
const PROXY_MAX_RESUMES: u32 = 5;

/// Max number of URL re-resolutions when a signed CDN URL has expired.
/// Bounded so a genuinely-dead track can't loop forever.
const PROXY_MAX_RERESOLVES: u32 = 3;

/// True when an upstream HTTP status indicates an expired/invalid signed URL
/// (Qobuz/Tidal signatures return 403 Forbidden or 410 Gone once `etsp`
/// passes). These are re-resolvable — a fresh signed URL for the same file
/// will succeed; a plain 404/5xx is not, so we don't re-resolve those.
fn is_expired_url_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::GONE
}

/// Send a GET for `url` (optionally with `Range: bytes={start}-`) and, if the
/// request fails to send OR the CDN answers with an expiry status (403/410),
/// re-resolve a fresh signed URL via `reresolve` and retry — bounded by
/// `PROXY_MAX_RERESOLVES`. Returns the successful response together with the
/// URL that produced it (which the caller stores back on the session so later
/// resumes use the fresh URL). Byte-exactness is preserved: the retry re-uses
/// the SAME absolute `start` offset against the same file.
async fn send_with_reresolve(
    client: &'static reqwest::Client,
    url: String,
    start: Option<u64>,
    reresolve: &Option<ReresolveFn>,
) -> Result<(reqwest::Response, String), ()> {
    let mut url = url;
    let mut attempts: u32 = 0;
    loop {
        let mut req = client.get(&url).header("Accept-Encoding", "identity");
        if let Some(s) = start {
            req = req.header("Range", format!("bytes={s}-"));
        }
        let outcome = req.send().await;

        // Decide whether this attempt needs a fresh URL.
        let needs_reresolve = match &outcome {
            Err(e) => {
                warn!(error = %e, url = %url, "proxy_upstream_error");
                true
            }
            Ok(r) if is_expired_url_status(r.status()) => {
                warn!(status = %r.status(), url = %url, "proxy_upstream_expired_status");
                true
            }
            Ok(_) => false,
        };

        if !needs_reresolve {
            // Safe: matched Ok(_) above.
            return Ok((outcome.unwrap(), url));
        }

        let Some(reresolve) = reresolve else {
            // No re-resolver (local/non-expiring source) — nothing more to do.
            return Err(());
        };
        if attempts >= PROXY_MAX_RERESOLVES {
            warn!(url = %url, "proxy_reresolve_giveup");
            return Err(());
        }
        attempts += 1;
        match reresolve().await {
            Ok(fresh) => {
                info!(attempts, start = ?start, "proxy_url_reresolved");
                url = fresh;
            }
            Err(e) => {
                warn!(error = %e, attempts, "proxy_reresolve_failed");
                return Err(());
            }
        }
    }
}

/// Build a body that streams `initial` chunks to the client and, on a
/// mid-stream body error (reqwest "error decoding response body" — a dropped
/// Akamai keep-alive connection, NOT a content-decode issue since the client
/// has no compression features), transparently re-fetches the CDN from the
/// exact byte offset reached and continues. The renderer never sees the drop,
/// so Hi-Res tracks no longer stop mid-file (#1136).
///
/// Bytes are streamed verbatim — no decoding or transformation — so the audio
/// stays byte-exact. `abs_offset` is the absolute file offset of the first
/// byte of `initial` (0 for a full fetch, N for a `bytes=N-` resume).
/// `sauter` : octets de l'amont à NE PAS livrer en tête — le corps amont
/// commence à `abs_offset`, le renderer veut `abs_offset + sauter`. Sert la
/// petite reprise `bytes=N-` d'un Lavf (sous le seuil de transfert au CDN)
/// avec un vrai 206 depuis N, sans rien demander de plus à l'amont.
fn resumable_proxy_body(
    client: &'static reqwest::Client,
    upstream_url: String,
    initial: reqwest::Response,
    abs_offset: u64,
    sauter: u64,
    reresolve: Option<ReresolveFn>,
    compteur: std::sync::Arc<StreamSession>,
) -> Body {
    let flux = async_stream::stream! {
        use futures_util::StreamExt;
        let mut resp = initial;
        // Current (possibly re-resolved) CDN URL we reconnect against.
        let mut url = upstream_url;
        // Absolute file offset of the next byte we expect to yield.
        let mut pos = abs_offset;
        let mut a_sauter = sauter;
        let mut resumes: u32 = 0;
        loop {
            let mut stream = resp.bytes_stream();
            let mut clean_eof = true;
            loop {
                match stream.next().await {
                    Some(Ok(chunk)) => {
                        pos += chunk.len() as u64;
                        if a_sauter > 0 {
                            let n = chunk.len() as u64;
                            if n <= a_sauter {
                                a_sauter -= n;
                                continue;
                            }
                            let garde = chunk.slice(a_sauter as usize..);
                            a_sauter = 0;
                            yield Ok::<_, std::io::Error>(garde);
                            continue;
                        }
                        yield Ok::<_, std::io::Error>(chunk);
                    }
                    Some(Err(e)) => {
                        warn!(error = %e, pos, resumes, "proxy_chunk_error");
                        clean_eof = false;
                        break;
                    }
                    None => break, // clean end of body
                }
            }
            if clean_eof {
                break;
            }
            if resumes >= PROXY_MAX_RESUMES {
                warn!(pos, "proxy_resume_giveup");
                break;
            }
            resumes += 1;
            // Backoff before reconnecting: the CDN just dropped us.
            tokio::time::sleep(std::time::Duration::from_millis(
                200u64 * u64::from(resumes),
            ))
            .await;
            // Reconnect at the exact byte offset reached. If the connection
            // fails to send or the signed URL has expired (403/410), this
            // re-resolves a fresh signed URL and retries the SAME offset —
            // byte-exact — so a mid-track URL expiry no longer stops playback.
            match send_with_reresolve(client, url.clone(), Some(pos), &reresolve).await {
                Ok((r, fresh_url)) if r.status() == reqwest::StatusCode::PARTIAL_CONTENT => {
                    if fresh_url != url {
                        url = fresh_url;
                    }
                    info!(pos, resumes, "proxy_resume_reconnect_206");
                    resp = r;
                }
                Ok((r, _)) => {
                    warn!(status = %r.status(), pos, "proxy_resume_bad_status");
                    break;
                }
                Err(()) => {
                    warn!(pos, "proxy_resume_upstream_failed");
                    break;
                }
            }
        }
    };
    corps_compte(flux, compteur)
}

async fn proxy_stream(
    upstream_url: &str,
    info: &StreamInfo,
    is_radio: bool,
    req_headers: &HeaderMap,
    session: std::sync::Arc<StreamSession>,
) -> Response {
    // Re-resolver for expiring signed CDN URLs (Qobuz/Tidal). Present only for
    // streaming proxy sessions; None for radio and non-expiring sources.
    let reresolve = session.reresolve.lock().await.clone();
    let client = if is_radio {
        // Radio streams are infinite — use a client with no total timeout
        // so the connection stays alive until the user stops playback.
        tune_core::http::client::infinite_stream()
    } else {
        tune_core::http::client::long_timeout()
    };

    // Parse the Range header once so we can decide how to fetch upstream.
    let range_value = req_headers
        .get("Range")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // DMP-A8 (Lavf) sends rapid micro-Range requests for FLAC header parsing.
    // Forwarding each one to the CDN hammers Akamai and causes drops.
    // User-initiated seeks go through the orchestrator (stream recreation),
    // so we never need to forward Range to the CDN for proxy sessions.
    // A resume Range `bytes=N-` with a large N means the renderer is
    // reconnecting after the proxied CDN connection dropped mid-track: the
    // DMP-A8 (Lavf) buffers ~30s, pauses reading, Akamai drops the idle
    // upstream, and reqwest then reports `proxy_chunk_error: error decoding
    // response body`. The renderer reconnects with `bytes=N-` (N ≈ where it
    // stopped). The old code ignored that Range and re-served from byte 0, so
    // the track restarted from the beginning (.18/.15 Qobuz → DMP-A8). Forward
    // the resume to the CDN so playback continues from N. Small/near-zero
    // ranges (FLAC header parsing) are NOT forwarded — forwarding every
    // micro-range hammers Akamai and itself causes the drops.
    const RESUME_RANGE_THRESHOLD: u64 = 1_048_576; // 1 MiB
    // The 1 MiB threshold exists ONLY to tame the DMP-A8 (Lavf), which fires many
    // rapid micro-Range requests while parsing the FLAC header — forwarding each
    // to the CDN hammers Akamai. Other renderers don't do that. The Lumin
    // firmware (Luxman NT-07 OpenHome, Vincent) instead does a two-step seek:
    // `bytes=0-` to read the header, then `bytes=244-` to fetch the first audio
    // frame. When we neither forward that small range nor answer 206 from 244 —
    // returning 200 from byte 0 — the renderer gets header bytes where it expects
    // audio, rejects the stream, and loops re-reading the header (peak_pos=0,
    // stopped after 74s). So keep the 1 MiB threshold ONLY for Lavf; for every
    // other agent honour any non-zero `bytes=N-` by forwarding it (→ 206 from N).
    let is_lavf = req_headers
        .get("User-Agent")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ua| ua.to_ascii_lowercase().contains("lavf"));
    let resume_threshold = if is_lavf { RESUME_RANGE_THRESHOLD } else { 1 };
    let resume_start = range_value
        .as_deref()
        .and_then(parse_range_start)
        .filter(|&n| n >= resume_threshold);

    if let Some(start) = resume_start {
        info!(
            url = upstream_url,
            start, is_lavf, "proxy_forward_resume_range"
        );
    }

    // Ask the CDN for the raw bytes — `identity` disables any upstream
    // content-coding so we proxy the FLAC verbatim (byte-exact audio).
    // send_with_reresolve retries against a FRESH signed URL when the request
    // fails to send or the CDN returns an expiry status (403/410) — this is the
    // path the client Range-resume hits when the Qobuz `etsp` signature has
    // expired mid-track (#1136), which the old single-shot send could not
    // recover from.
    let (upstream_resp, upstream_url) =
        match send_with_reresolve(client, upstream_url.to_string(), resume_start, &reresolve).await
        {
            Ok(pair) => pair,
            Err(()) => return StatusCode::BAD_GATEWAY.into_response(),
        };
    // Persist a re-resolved URL so later resumes start from the fresh signature.
    {
        let mut pu = session.proxy_url.lock().await;
        if pu.as_deref() != Some(upstream_url.as_str()) {
            *pu = Some(upstream_url.clone());
        }
    }
    let upstream_url = upstream_url.as_str();
    // Only treat it as a real resume if the CDN honoured the Range (206).
    let resume_start = if upstream_resp.status() == reqwest::StatusCode::PARTIAL_CONTENT {
        resume_start
    } else {
        None
    };

    let upstream_content_type = upstream_resp
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or(&info.mime_type)
        .to_string();

    let content_length = upstream_resp
        .headers()
        .get("Content-Length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    // Preserve the CDN's Content-Range for a forwarded resume so we can pass
    // the correct `bytes N-last/total` back to the renderer.
    let upstream_content_range = upstream_resp
        .headers()
        .get("Content-Range")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let mut headers = HeaderMap::new();
    // #4958 — l'orthographe annoncée au renderer prime sur celle du CDN :
    // c'est elle que la DIDL porte, et un Sink strict confronte les deux.
    let upstream_content_type =
        tune_core::http::streamer::content_type_du_flux(&session.id, &upstream_content_type);
    headers.insert(
        "Content-Type",
        HeaderValue::from_str(&upstream_content_type).unwrap(),
    );
    if !is_radio {
        // Only advertise Accept-Ranges for finite streams.  Radio streams
        // are infinite — advertising seekability causes some browsers to
        // attempt byte-range requests that will never succeed.
        headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));
    }
    headers.insert(
        "transferMode.dlna.org",
        HeaderValue::from_static("Streaming"),
    );

    // DLNA renderers (e.g. Eversolo DMP-A8 with Lavf) send Range: bytes=0-
    // and expect 206 Partial Content with Content-Range header.
    // Returning 200 OK causes them to abort after ~31 seconds.
    let range_requested = range_value.as_deref().filter(|r| r.starts_with("bytes=0-"));

    // Resume: the CDN honoured a forwarded `bytes=N-` Range. Pass the 206
    // through with the CDN's Content-Range so the renderer continues from N
    // instead of restarting the track from byte 0.
    if let Some(start) = resume_start {
        if let Some(cr) = upstream_content_range.clone().or_else(|| {
            content_length.map(|cl| format!("bytes {start}-{}/{}", start + cl - 1, start + cl))
        }) {
            headers.insert("Content-Range", HeaderValue::from_str(&cr).unwrap());
        }
        if let Some(cl) = content_length {
            headers.insert("Content-Length", HeaderValue::from(cl));
        }
        info!(url = upstream_url, start, "proxy_resume_206_from_cdn");

        let body = resumable_proxy_body(
            client,
            upstream_url.to_string(),
            upstream_resp,
            start,
            0,
            reresolve.clone(),
            session.clone(),
        );
        return (StatusCode::PARTIAL_CONTENT, headers, body).into_response();
    }

    // Une petite reprise `bytes=N-` (0 < N < seuil) d'un renderer Lavf n'est
    // PAS transmise au CDN — le seuil existe pour ne pas marteler Akamai avec
    // les micro-Range de l'analyse d'en-tête FLAC. Mais y répondre par un 200
    // depuis l'octet 0, comme avant, donne au renderer des octets d'en-tête
    // là où il attend l'octet N : sur un `.dsf` relayé depuis un autre Tune,
    // l'Eversolo DMP-A8 enchaîne `bytes=0-`, `bytes=<fin-187>-` (chunk ID3
    // final), `bytes=28-` — et, trahi par la dernière, recommence les trois
    // en boucle sans jamais jouer (.18, 23/09/2026, position 0 pendant 45 s).
    // Une session de FICHIER répond 206 depuis N (`serve_file`) ; le
    // mandataire fait de même en tirant l'amont depuis 0 et en SAUTANT N
    // octets, sans rien demander de plus au CDN.
    let saut_local = range_value
        .as_deref()
        .and_then(parse_range_start)
        .filter(|&n| n > 0 && n < resume_threshold)
        .filter(|_| !is_radio && upstream_resp.status() == reqwest::StatusCode::OK);
    if let (Some(n), Some(cl)) = (saut_local, content_length)
        && n < cl
    {
        headers.insert("Content-Length", HeaderValue::from(cl - n));
        headers.insert(
            "Content-Range",
            HeaderValue::from_str(&format!("bytes {n}-{}/{}", cl - 1, cl)).unwrap(),
        );
        info!(
            url = upstream_url,
            start = n,
            total = cl,
            "proxy_206_par_saut_local"
        );
        let body = resumable_proxy_body(
            client,
            upstream_url.to_string(),
            upstream_resp,
            0,
            n,
            reresolve.clone(),
            session.clone(),
        );
        return (StatusCode::PARTIAL_CONTENT, headers, body).into_response();
    }

    // Radio streams are infinite — no Content-Length is possible.
    // The DMP-A8 sends Range: bytes=0- initially, then reconnects with
    // bytes=N- (resume). Both must return 206 with an open-ended
    // Content-Range so the renderer keeps consuming the stream.
    let any_range = range_value.as_deref().filter(|r| r.starts_with("bytes="));
    if is_radio && any_range.is_some() {
        headers.remove("Accept-Ranges");
        headers.insert("Content-Range", HeaderValue::from_static("bytes 0-*/*"));
        headers.insert("Transfer-Encoding", HeaderValue::from_static("chunked"));

        info!(url = upstream_url, "proxy_radio_206_open_ended");

        let flux = async_stream::stream! {
            let mut stream = upstream_resp.bytes_stream();
            use futures_util::StreamExt;
            while let Some(chunk_result) = stream.next().await {
                match chunk_result {
                    Ok(chunk) => yield Ok::<_, std::io::Error>(chunk),
                    Err(e) => {
                        warn!(error = %e, "proxy_radio_chunk_error");
                        break;
                    }
                }
            }
        };
        let body = corps_compte(flux, session.clone());

        return (StatusCode::PARTIAL_CONTENT, headers, body).into_response();
    }

    if let (Some(_), Some(cl)) = (range_requested, content_length) {
        headers.insert("Content-Length", HeaderValue::from(cl));
        headers.insert(
            "Content-Range",
            HeaderValue::from_str(&format!("bytes 0-{}/{}", cl - 1, cl)).unwrap(),
        );

        let body = resumable_proxy_body(
            client,
            upstream_url.to_string(),
            upstream_resp,
            0,
            0,
            reresolve.clone(),
            session.clone(),
        );
        return (StatusCode::PARTIAL_CONTENT, headers, body).into_response();
    }

    if let Some(cl) = content_length {
        headers.insert("Content-Length", HeaderValue::from(cl));
    }

    let body = resumable_proxy_body(
        client,
        upstream_url.to_string(),
        upstream_resp,
        0,
        0,
        reresolve.clone(),
        session.clone(),
    );
    (StatusCode::OK, headers, body).into_response()
}

pub fn router(sessions: SharedSessions) -> axum::Router {
    axum::Router::new()
        .route(
            "/stream/{stream_id}",
            axum::routing::get(handle_stream).head(handle_head),
        )
        .with_state(sessions)
}

#[cfg(test)]
mod tests {
    use super::{
        ICY_METAINT, accepts_chunked_live_stream, corps_compte, decalage_de_trame, decoupe_icy,
        parse_range_start,
    };

    /// #4455 — la grille des trames d'un WAV 24 bits stéréo (6 octets) après
    /// ses 44 octets d'en-tête : les reprises alignées ne disent rien, les
    /// autres disent de combien elles tombent à côté. L'en-tête et le début
    /// de fichier ne sont jamais « hors trame », ni une trame inconnue.
    #[test]
    fn le_decalage_de_trame_ne_signale_que_les_reprises_entre_deux_trames() {
        assert_eq!(decalage_de_trame(0, 44, 6), 0);
        assert_eq!(decalage_de_trame(44, 44, 6), 0);
        assert_eq!(decalage_de_trame(44 + 6 * 1_000, 44, 6), 0);
        assert_eq!(decalage_de_trame(44 + 6 * 1_000 + 1, 44, 6), 1);
        assert_eq!(decalage_de_trame(44 + 6 * 1_000 + 3, 44, 6), 3);
        // Dans l'en-tête : la sonde `bytes=1-` d'un renderer, pas une reprise.
        assert_eq!(decalage_de_trame(12, 44, 6), 0);
        // 16 bits stéréo : trame de 4.
        assert_eq!(decalage_de_trame(44 + 4 * 10 + 2, 44, 4), 2);
        // Trame inconnue ou d'un octet : rien à mesurer.
        assert_eq!(decalage_de_trame(1_000_001, 44, 0), 0);
        assert_eq!(decalage_de_trame(1_000_001, 44, 1), 0);
    }

    /// La sonde du DMP-A8, en modèle réduit : une connexion ouvre le flux
    /// d'une conversion (DSD→WAV), puis une seconde arrive pendant que la
    /// première lit encore.
    ///
    /// Avant ce correctif, les deux tiraient sur le même canal mono-
    /// consommateur : chaque chunk partait vers l'une OU l'autre, la lecture
    /// ne recevait qu'une fraction du signal, et le renderer affamé rejouait
    /// son tampon — la « boucle de 4-7 s » entendue sur tout DSD converti.
    ///
    /// Le test vérifie les trois clauses du contrat :
    /// 1. la SECONDE connexion reçoit l'en-tête WAV — rejoué, puisque le
    ///    premier exemplaire est parti dans la première connexion ;
    /// 2. ses chunks sont CONTIGUS — un vol lui ferait des trous ;
    /// 3. elle reçoit la FIN du flux — c'est elle qui joue.
    #[tokio::test]
    async fn une_connexion_de_sonde_ne_vole_plus_une_conversion() {
        use axum::extract::{Path, State};
        use futures_util::StreamExt;
        use std::sync::atomic::Ordering::SeqCst;
        use tune_core::http::streamer::SharedSessions;

        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            ..StreamInfo::default()
        };
        let session = std::sync::Arc::new(StreamSession::new("dsd".into(), info, false, 64));
        session.wav_header_included.store(true, SeqCst);

        let tx = session.tx.lock().await.clone().expect("tx");
        // Retirer les émetteurs de la session : la fin du canal sera la chute
        // de NOTRE clone, comme quand le décodeur termine.
        session.close_sender().await;

        let sessions: SharedSessions = std::sync::Arc::new(tokio::sync::Mutex::new(
            [("dsd".to_string(), session.clone())].into_iter().collect(),
        ));

        // Le décodeur : un en-tête RIFF de 44 octets, puis 40 chunks de
        // 1 000 octets numérotés en tête (comme le vrai premier chunk DSD,
        // l'en-tête part DANS le canal).
        let producteur = tokio::spawn(async move {
            let mut entete = b"RIFF".to_vec();
            entete.resize(44, 0);
            tx.send(entete).await.expect("entete");
            for i in 0..40u32 {
                let mut c = i.to_be_bytes().to_vec();
                c.resize(1_000, 0xAB);
                tx.send(c).await.expect("chunk");
                tokio::time::sleep(std::time::Duration::from_millis(4)).await;
            }
            // la chute de tx ferme le canal
        });

        // Connexion 1 — la sonde : elle lit ce qu'on lui donne.
        let sonde_sessions = sessions.clone();
        let sonde = tokio::spawn(async move {
            let rep = super::handle_stream(
                Path("dsd.wav".into()),
                State(sonde_sessions),
                axum::http::HeaderMap::new(),
            )
            .await;
            let mut corps = rep.into_body().into_data_stream();
            let mut octets = Vec::new();
            while let Some(Ok(b)) = corps.next().await {
                octets.extend_from_slice(&b);
            }
            octets
        });

        // La sonde a le temps de consommer l'en-tête et quelques chunks.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;

        // Connexion 2 — la lecture.
        let rep = super::handle_stream(
            Path("dsd.wav".into()),
            State(sessions.clone()),
            axum::http::HeaderMap::new(),
        )
        .await;
        let mut corps = rep.into_body().into_data_stream();
        let mut lecture = Vec::new();
        while let Some(Ok(b)) = corps.next().await {
            lecture.extend_from_slice(&b);
        }

        // 1. L'en-tête est là, rejoué depuis la réserve.
        assert!(
            lecture.starts_with(b"RIFF"),
            "la connexion de lecture n'a pas reçu l'en-tête WAV (longueur {})",
            lecture.len()
        );
        let charge = &lecture[44..];
        assert!(
            charge.len() % 1_000 == 0 && !charge.is_empty(),
            "charge inattendue : {} octets après l'en-tête",
            charge.len()
        );

        // 2. Contiguïté : chaque numéro suit le précédent. Un vol par la
        //    sonde ferait sauter des numéros.
        let numeros: Vec<u32> = charge
            .chunks(1_000)
            .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        for paire in numeros.windows(2) {
            assert_eq!(
                paire[1],
                paire[0] + 1,
                "trou dans le flux de lecture : {} puis {} — une autre connexion a volé les chunks manquants",
                paire[0],
                paire[1]
            );
        }

        // 3. La fin du flux appartient à la lecture.
        assert_eq!(
            numeros.last().copied(),
            Some(39),
            "la lecture n'a pas reçu la fin du flux"
        );

        // La sonde s'est arrêtée d'elle-même (supersédée), sans bloquer.
        let octets_sonde = tokio::time::timeout(std::time::Duration::from_secs(5), sonde)
            .await
            .expect("la sonde aurait dû se terminer une fois supersédée")
            .expect("join");
        // La sonde peut n'avoir RIEN émis : ses octets attendaient encore dans
        // le tampon de coalescence de 64 Ko quand elle a été supersédée, et on
        // ne vide pas ce tampon vers une connexion qu'on abandonne. Ce qui
        // compte : si elle a émis, ça commençait par l'en-tête.
        assert!(
            octets_sonde.is_empty() || octets_sonde.starts_with(b"RIFF"),
            "la sonde a émis {} octets qui ne commencent pas par RIFF",
            octets_sonde.len()
        );
        producteur.await.expect("producteur");
    }

    /// FAIT DE BASE : les octets réellement délivrés par le flux interne
    /// pendant que le producteur est MUET et que le canal reste OUVERT.
    ///
    /// C'est la situation d'un trou en pleine lecture (#2952) : la sortie
    /// locale est bloquée dans `reader.read()` sur un client construit avec
    /// `.timeout(None)` — elle attend indéfiniment, sans rien signaler avant
    /// 5 s. Pendant ce temps le tampon de coalescence tient jusqu'à 64 Ko
    /// qu'il ne rendra qu'une fois 64 Ko ATTEINTS. Le producteur étant à sec,
    /// ce seuil n'arrive jamais : ces octets-là ne sortent JAMAIS.
    ///
    /// Avant le correctif : 0 octet délivré, et le corps ne rend rien du tout
    /// (la lecture au bout de 2 s expire). Après : les 32 768 octets qui
    /// étaient déjà là partent, puis les suivants au fil de l'eau.
    #[tokio::test]
    async fn un_producteur_a_sec_ne_retient_plus_ce_qui_est_deja_la() {
        use axum::extract::{Path, State};
        use futures_util::StreamExt;
        use std::sync::atomic::Ordering::{Relaxed, SeqCst};
        use tune_core::http::streamer::SharedSessions;

        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            ..StreamInfo::default()
        };
        let session = std::sync::Arc::new(StreamSession::new("conv".into(), info, false, 8));
        // L'en-tête voyage DANS le canal sur une conversion : le handler n'en
        // ajoute pas. On compte donc du PCM nu, sans 44 octets parasites.
        session.wav_header_included.store(true, SeqCst);
        let tx = session.tx.lock().await.clone().expect("tx");
        let sessions: SharedSessions = std::sync::Arc::new(tokio::sync::Mutex::new(
            [("conv".to_string(), session.clone())]
                .into_iter()
                .collect(),
        ));

        // Un seul morceau de 32 768 octets — la moitié du seuil de
        // regroupement — puis PLUS RIEN. Le canal reste ouvert : ce n'est pas
        // une fin de piste, c'est un trou.
        tx.send(vec![0xAB; 32_768]).await.expect("morceau");

        let rep = super::handle_stream(
            Path("conv.wav".into()),
            State(sessions),
            axum::http::HeaderMap::new(),
        )
        .await;
        let mut corps = rep.into_body().into_data_stream();

        let premiere = tokio::time::timeout(std::time::Duration::from_secs(2), corps.next())
            .await
            .expect(
                "le flux interne n'a RIEN délivré : les 32 768 octets déjà décodés \
                 attendent les 64 Ko d'un producteur à sec",
            )
            .expect("le corps s'est terminé au lieu de délivrer")
            .expect("erreur de flux");
        assert_eq!(
            premiere.len(),
            32_768,
            "le flux devait rendre exactement ce qu'il avait en main"
        );
        assert_eq!(
            session.bytes_sent.load(Relaxed),
            32_768,
            "octets délivrés par la session sur la fenêtre : le compteur de \
             production, pas celui du test"
        );

        // …et le flux CONTINUE : le morceau suivant part de la même façon.
        tx.send(vec![0xCD; 32_768]).await.expect("second morceau");
        let seconde = tokio::time::timeout(std::time::Duration::from_secs(2), corps.next())
            .await
            .expect("second morceau jamais délivré")
            .expect("corps terminé")
            .expect("erreur de flux");
        assert_eq!(seconde.len(), 32_768);
        assert_eq!(session.bytes_sent.load(Relaxed), 65_536);
    }

    /// TÉMOIN VERT : tant que le producteur est EN AVANCE, le regroupement est
    /// intact. Le flux écrit toujours des trames de 64 Ko — c'est la raison
    /// d'être du tampon (moins d'écritures TCP vers un renderer réseau), et le
    /// correctif ne doit pas la dissoudre.
    ///
    /// Quatre morceaux de 32 768 sont DÉJÀ dans un canal de capacité 4 quand
    /// la connexion arrive : le canal est plein, donc le producteur est en
    /// avance, exactement comme en régime établi sur une piste locale.
    #[tokio::test]
    async fn un_producteur_en_avance_ecrit_toujours_des_trames_de_64_ko() {
        use axum::extract::{Path, State};
        use futures_util::StreamExt;
        use std::sync::atomic::Ordering::SeqCst;
        use tune_core::http::streamer::SharedSessions;

        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            ..StreamInfo::default()
        };
        let session = std::sync::Arc::new(StreamSession::new("plein".into(), info, false, 4));
        session.wav_header_included.store(true, SeqCst);
        let tx = session.tx.lock().await.clone().expect("tx");
        let sessions: SharedSessions = std::sync::Arc::new(tokio::sync::Mutex::new(
            [("plein".to_string(), session.clone())]
                .into_iter()
                .collect(),
        ));
        for _ in 0..4 {
            tx.send(vec![0xCD; 32_768]).await.expect("morceau");
        }

        let rep = super::handle_stream(
            Path("plein.wav".into()),
            State(sessions),
            axum::http::HeaderMap::new(),
        )
        .await;
        let mut corps = rep.into_body().into_data_stream();

        for rang in 0..2 {
            let trame = tokio::time::timeout(std::time::Duration::from_secs(2), corps.next())
                .await
                .expect("trame jamais délivrée")
                .expect("corps terminé")
                .expect("erreur de flux");
            assert_eq!(
                trame.len(),
                65_536,
                "trame {rang} : le regroupement a été dissous alors que le \
                 producteur était en avance"
            );
        }
    }

    /// Prépare une session de conversion pré-remplie de `morceaux` blocs de
    /// 32 768 octets, et rend la session plus la carte de sessions.
    ///
    /// Le canal est laissé OUVERT : ce n'est pas une fin de piste.
    #[cfg(test)]
    async fn session_pleine(
        id: &str,
        morceaux: usize,
    ) -> (
        std::sync::Arc<StreamSession>,
        tune_core::http::streamer::SharedSessions,
    ) {
        use std::sync::atomic::Ordering::SeqCst;
        use tune_core::http::streamer::SharedSessions;

        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            ..StreamInfo::default()
        };
        let session = std::sync::Arc::new(StreamSession::new(id.into(), info, false, 16));
        session.wav_header_included.store(true, SeqCst);
        let tx = session.tx.lock().await.clone().expect("tx");
        for _ in 0..morceaux {
            tx.send(vec![0xEE; 32_768]).await.expect("morceau");
        }
        let sessions: SharedSessions = std::sync::Arc::new(tokio::sync::Mutex::new(
            [(id.to_string(), session.clone())].into_iter().collect(),
        ));
        (session, sessions)
    }

    /// GARDE #2952 — un blocage EN AVAL du canal doit laisser une trace.
    ///
    /// `stream_producer_ran_dry` ne dit quelque chose que si le canal se VIDE.
    /// Quand les octets sont déjà décodés et que c'est le corps HTTP qui
    /// n'avance plus — réacteur affamé, socket qui ne se vide pas — le canal
    /// reste PLEIN, l'alerte du producteur se tait, et RIEN côté serveur ne
    /// dit pourquoi la sortie locale attend dans son `reader.read()` sans
    /// limite de temps. C'est la moitié du ticket qui n'était pas instrumentée.
    ///
    /// Ici le producteur est en avance (canal plein) et c'est le CONSOMMATEUR
    /// qui cesse de lire pendant 30 s. L'horloge est arrêtée puis avancée à la
    /// main : le vert ne dépend pas de la charge de la machine.
    #[tokio::test]
    async fn un_blocage_du_transport_est_journalise_meme_avec_le_canal_plein() {
        use axum::extract::{Path, State};
        use futures_util::StreamExt;
        use std::sync::atomic::Ordering::Relaxed;

        let (session, sessions) = session_pleine("aval", 6).await;

        let rep = super::handle_stream(
            Path("aval.wav".into()),
            State(sessions),
            axum::http::HeaderMap::new(),
        )
        .await;
        let mut corps = rep.into_body().into_data_stream();

        // Première trame : régime sain, rien à signaler.
        let premiere = corps.next().await.expect("corps terminé").expect("flux");
        assert_eq!(premiere.len(), 65_536);
        assert_eq!(
            session.stall_alerts.load(Relaxed),
            0,
            "une trame livrée normalement ne doit RIEN signaler"
        );

        // Le corps est suspendu DANS son `yield` : personne ne vient chercher
        // la suite pendant 30 s, alors que le canal est plein.
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(30)).await;
        tokio::time::resume();

        let seconde = corps.next().await.expect("corps terminé").expect("flux");
        assert_eq!(seconde.len(), 65_536);
        // `stall_alerts` et non `stall_alert_emitted` : depuis que le témoin se
        // RÉARME au premier tour sain (une ligne par blocage, pas une par
        // session), le drapeau est retombé quand la trame suivante est servie.
        // Le COMPTEUR, lui, est le fait durable.
        assert_eq!(
            session.stall_alerts.load(Relaxed),
            1,
            "30 s sans que les octets DÉJÀ décodés ne partent, et le serveur \
             n'en dit rien : c'est le trou d'instrumentation de #2952"
        );
        assert!(
            !session.dry_alert_emitted.load(Relaxed),
            "le producteur était en avance : ne pas lui imputer le blocage"
        );
    }

    /// TÉMOIN VERT du précédent : un flux lu au fil de l'eau ne signale rien.
    /// Sans lui, une alerte posée sur « toute attente » passerait aussi.
    #[tokio::test]
    async fn un_flux_lu_au_fil_de_l_eau_ne_signale_aucun_blocage() {
        use axum::extract::{Path, State};
        use futures_util::StreamExt;
        use std::sync::atomic::Ordering::Relaxed;

        let (session, sessions) = session_pleine("sain", 6).await;

        let rep = super::handle_stream(
            Path("sain.wav".into()),
            State(sessions),
            axum::http::HeaderMap::new(),
        )
        .await;
        let mut corps = rep.into_body().into_data_stream();

        for _ in 0..3 {
            let trame = corps.next().await.expect("corps terminé").expect("flux");
            assert_eq!(trame.len(), 65_536);
        }
        assert!(
            !session.stall_alert_emitted.load(Relaxed),
            "aucune attente n'a dépassé le seuil : rien ne doit être signalé"
        );
        assert_eq!(
            session.stall_alerts.load(Relaxed),
            0,
            "aucun blocage, aucune ligne"
        );
    }

    /// GARDE #2952 — DEUX blocages dans la même piste doivent donner DEUX
    /// lignes.
    ///
    /// Le journal de Belkadi Yacine porte, sur la MÊME piste et donc la même
    /// session de flux, `local_audio_slow_read wait_ms=38594` à 15:47:42 puis
    /// `wait_ms=44853` à 15:48:36 — la piste a commencé à 15:46:47
    /// (`track_end_gap wall_secs=290` à 15:51:37) et s'est terminée à 15:51:37.
    /// La piste suivante porte la même paire (35 191 ms, 35 367 ms). Avec une
    /// seule ligne par session, le second blocage de chaque piste n'aurait
    /// jamais de contrepartie côté serveur, et l'absence de ligne se lirait
    /// comme « le flux allait bien la seconde fois ».
    ///
    /// Le vrai `handle_stream` est appelé ; l'horloge est arrêtée et avancée à
    /// la main, donc le vert ne dépend pas de la charge de la machine.
    #[tokio::test]
    async fn deux_blocages_dans_la_meme_session_donnent_deux_lignes() {
        use axum::extract::{Path, State};
        use futures_util::StreamExt;
        use std::sync::atomic::Ordering::Relaxed;

        let (session, sessions) = session_pleine("deux", 8).await;

        let rep = super::handle_stream(
            Path("deux.wav".into()),
            State(sessions),
            axum::http::HeaderMap::new(),
        )
        .await;
        let mut corps = rep.into_body().into_data_stream();

        // Trame 1 : régime sain.
        assert_eq!(
            corps.next().await.expect("corps").expect("flux").len(),
            65_536
        );

        // Premier blocage : 30 s sans que le corps soit relu.
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(30)).await;
        tokio::time::resume();
        assert_eq!(
            corps.next().await.expect("corps").expect("flux").len(),
            65_536
        );
        assert_eq!(
            session.stall_alerts.load(Relaxed),
            1,
            "le premier blocage doit être signalé"
        );

        // Trame saine entre les deux : c'est elle qui réarme.
        assert_eq!(
            corps.next().await.expect("corps").expect("flux").len(),
            65_536
        );

        // Second blocage, même session.
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(30)).await;
        tokio::time::resume();
        assert_eq!(
            corps.next().await.expect("corps").expect("flux").len(),
            65_536
        );
        assert_eq!(
            session.stall_alerts.load(Relaxed),
            2,
            "le SECOND blocage de la même piste doit avoir sa ligne : avec un \
             seul tir par session, la moitié de la matière de #2952 reste \
             invisible et l'absence se lit comme un flux sain"
        );
    }

    /// GARDE #3575 — un blocage qui ne FINIT jamais doit être nommé.
    ///
    /// `note_delivery_stall` mesure au RETOUR du `yield` : un corps lâché en
    /// vol ne repasse jamais par ce point et ne laissait donc pas une seule
    /// ligne. C'est le pire cas — « sortie locale imprenable pour toute la vie
    /// du processus » — et c'était précisément celui qui se taisait.
    #[tokio::test]
    async fn un_corps_lache_pendant_le_blocage_le_dit() {
        use axum::extract::{Path, State};
        use futures_util::StreamExt;
        use std::sync::atomic::Ordering::Relaxed;

        let (session, sessions) = session_pleine("lache", 6).await;

        let rep = super::handle_stream(
            Path("lache.wav".into()),
            State(sessions),
            axum::http::HeaderMap::new(),
        )
        .await;
        let mut corps = rep.into_body().into_data_stream();

        assert_eq!(
            corps.next().await.expect("corps").expect("flux").len(),
            65_536
        );

        // Le corps reste suspendu DANS son `yield` pendant 30 s, puis la
        // connexion meurt : personne ne reviendra jamais mesurer cette attente.
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(30)).await;
        tokio::time::resume();
        drop(corps);

        assert!(
            session.abandon_alert_emitted.load(Relaxed),
            "un corps lâché après 30 s d'attente en vol ne laissait AUCUNE \
             trace : c'est le trou de #3575"
        );
        assert_eq!(
            session.stall_alerts.load(Relaxed),
            0,
            "aucune attente FINIE n'a été mesurée : ne pas la compter deux fois"
        );
    }

    /// TÉMOIN VERT du précédent : lâcher un corps est le cas ORDINAIRE.
    ///
    /// Sans lui, une sentinelle qui crierait à chaque fermeture passerait
    /// aussi — et noierait le journal à chaque changement de piste.
    #[tokio::test]
    async fn un_corps_lache_sans_attente_ne_signale_aucun_abandon() {
        use axum::extract::{Path, State};
        use futures_util::StreamExt;
        use std::sync::atomic::Ordering::Relaxed;

        let (session, sessions) = session_pleine("ordinaire", 6).await;

        let rep = super::handle_stream(
            Path("ordinaire.wav".into()),
            State(sessions),
            axum::http::HeaderMap::new(),
        )
        .await;
        let mut corps = rep.into_body().into_data_stream();

        for _ in 0..2 {
            assert_eq!(
                corps.next().await.expect("corps").expect("flux").len(),
                65_536
            );
        }
        drop(corps);

        assert!(
            !session.abandon_alert_emitted.load(Relaxed),
            "un corps lâché entre deux morceaux n'est pas un blocage"
        );
    }

    /// Le seuil et la règle « une ligne par BLOCAGE » sont le contrat de
    /// `note_delivery_stall`. Une attente sous le seuil ne dit rien ; la
    /// première au-dessus alerte ; les suivantes du MÊME blocage se taisent ;
    /// un tour sain réarme, et le blocage suivant a droit à sa ligne.
    #[test]
    fn le_seuil_de_blocage_alerte_une_fois_par_blocage() {
        use std::sync::atomic::Ordering::Relaxed;
        use std::time::Duration;
        use tune_core::http::streamer::DELIVERY_STALL_THRESHOLD;

        let info = StreamInfo::default();
        let session = StreamSession::new("seuil".into(), info, false, 4);
        let sous = DELIVERY_STALL_THRESHOLD - Duration::from_millis(1);

        assert!(!session.note_delivery_stall(sous, sous));
        assert!(
            session.note_delivery_stall(Duration::ZERO, DELIVERY_STALL_THRESHOLD),
            "une attente de transport au seuil doit alerter"
        );
        assert!(
            !session.note_delivery_stall(DELIVERY_STALL_THRESHOLD * 10, Duration::ZERO),
            "le MÊME blocage ne se répète pas"
        );

        // Un tour sain : le blocage est fini.
        assert!(!session.note_delivery_stall(sous, sous));
        assert!(
            session.note_delivery_stall(Duration::ZERO, DELIVERY_STALL_THRESHOLD),
            "le SECOND blocage de la session doit avoir sa ligne : #2952 en \
             porte deux par piste (38 594 ms puis 44 853 ms), et n'en garder \
             qu'un fait lire « la seconde attente n'a pas de contrepartie »"
        );
        assert_eq!(
            session.stall_alerts.load(Relaxed),
            2,
            "deux blocages, deux lignes"
        );
    }

    /// TÉMOIN VERT : une fin de piste reste une fin de piste, pas un trou. Le
    /// producteur émet un morceau puis FERME le canal ; le corps rend ces
    /// octets-là, exactement, puis se termine.
    #[tokio::test]
    async fn une_fin_de_piste_reste_une_fin_de_piste() {
        use axum::extract::{Path, State};
        use futures_util::StreamExt;
        use std::sync::atomic::Ordering::SeqCst;
        use tune_core::http::streamer::SharedSessions;

        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            ..StreamInfo::default()
        };
        let session = std::sync::Arc::new(StreamSession::new("fin".into(), info, false, 8));
        session.wav_header_included.store(true, SeqCst);
        let tx = session.tx.lock().await.clone().expect("tx");
        session.close_sender().await;
        let sessions: SharedSessions = std::sync::Arc::new(tokio::sync::Mutex::new(
            [("fin".to_string(), session.clone())].into_iter().collect(),
        ));
        tx.send(vec![0xEF; 32_768]).await.expect("morceau");
        drop(tx);

        let rep = super::handle_stream(
            Path("fin.wav".into()),
            State(sessions.clone()),
            axum::http::HeaderMap::new(),
        )
        .await;
        let mut corps = rep.into_body().into_data_stream();
        let mut octets = Vec::new();
        while let Some(Ok(b)) =
            tokio::time::timeout(std::time::Duration::from_secs(2), corps.next())
                .await
                .expect("le corps ne s'est jamais terminé")
        {
            octets.extend_from_slice(&b);
        }
        assert_eq!(
            octets.len(),
            32_768,
            "une fin de piste doit rendre tous ses octets et RIEN de plus"
        );
    }

    /// L'Eversolo DMP-A8 télécharge par tranches : `bytes=0-`, puis il ferme et
    /// revient avec `bytes=N-` pour la suite. Répondre 200 + longueur totale à
    /// cette reprise lui fait jeter la réponse et redemander le même offset en
    /// boucle (la « boucle de 4-7 s » des DSD convertis, .42 le 24/08). La
    /// reprise doit recevoir un vrai 206 dont le Content-Range part de N.
    #[tokio::test]
    async fn une_reprise_range_sur_un_wav_fini_recoit_un_206_coherent() {
        use axum::extract::{Path, State};
        use futures_util::StreamExt;
        use std::sync::atomic::Ordering::SeqCst;
        use tune_core::http::streamer::SharedSessions;

        // 100 octets de données : longueur WAV annoncée = 44 + 100 = 144.
        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 100,
            channels: 1,
            bit_depth: 8,
            duration_ms: Some(1_000),
            ..StreamInfo::default()
        };
        let session = std::sync::Arc::new(StreamSession::new("dsd".into(), info, false, 64));
        session.wav_header_included.store(true, SeqCst);
        let tx = session.tx.lock().await.clone().expect("tx");
        session.close_sender().await;
        let sessions: SharedSessions = std::sync::Arc::new(tokio::sync::Mutex::new(
            [("dsd".to_string(), session.clone())].into_iter().collect(),
        ));

        let mut entete = b"RIFF".to_vec();
        entete.resize(44, 0);
        tx.send(entete).await.expect("entete");
        tx.send(vec![0xCD; 100]).await.expect("charge");
        drop(tx);

        let mut req = axum::http::HeaderMap::new();
        req.insert("Range", "bytes=100-".parse().unwrap());
        let rep = super::handle_stream(Path("dsd.wav".into()), State(sessions), req).await;

        assert_eq!(rep.status(), axum::http::StatusCode::PARTIAL_CONTENT);
        let entetes = rep.headers();
        assert_eq!(
            entetes.get("Content-Range").and_then(|v| v.to_str().ok()),
            Some("bytes 100-143/144"),
            "le Content-Range doit partir de l'offset demandé"
        );
        assert_eq!(
            entetes.get("Content-Length").and_then(|v| v.to_str().ok()),
            Some("44"),
            "la longueur doit être ce qui reste après l'offset"
        );

        // Le contrat annoncé reste honnête : pas d'invitation à seeker.
        assert!(
            entetes.get("Accept-Ranges").is_none(),
            "une conversion ne doit pas annoncer Accept-Ranges"
        );
        assert_eq!(
            entetes
                .get("contentFeatures.dlna.org")
                .and_then(|v| v.to_str().ok()),
            Some("DLNA.ORG_OP=00;DLNA.ORG_FLAGS=01700000000000000000000000000000"),
            "le GET doit dire OP=00 comme la DIDL"
        );

        // La reprise a dit « j'ai déjà l'en-tête » : on ne le rejoue pas.
        let mut corps = rep.into_body().into_data_stream();
        let mut octets = Vec::new();
        while let Some(Ok(b)) = corps.next().await {
            octets.extend_from_slice(&b);
        }
        assert!(
            !octets.starts_with(b"RIFF"),
            "l'en-tête WAV a été rejoué sur une reprise bytes=100-"
        );
    }

    /// Le HEAD d'une conversion doit annoncer le même contrat que la DIDL et
    /// le GET : OP=00, pas d'Accept-Ranges. Un HEAD qui promet la seekabilité
    /// invite le renderer à seeker un tuyau — le gel à 0:00 du DMP-A8.
    #[tokio::test]
    async fn le_head_d_une_conversion_n_annonce_pas_la_seekabilite() {
        use axum::extract::{Path, State};
        use std::sync::atomic::Ordering::SeqCst;
        use tune_core::http::streamer::SharedSessions;

        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 100,
            channels: 1,
            bit_depth: 8,
            duration_ms: Some(1_000),
            ..StreamInfo::default()
        };
        let session = std::sync::Arc::new(StreamSession::new("dsd".into(), info, false, 64));
        session.wav_header_included.store(true, SeqCst);
        let sessions: SharedSessions = std::sync::Arc::new(tokio::sync::Mutex::new(
            [("dsd".to_string(), session)].into_iter().collect(),
        ));

        let rep = super::handle_head(
            Path("dsd.wav".into()),
            State(sessions),
            axum::http::HeaderMap::new(),
        )
        .await;
        let entetes = rep.headers();
        assert!(
            entetes.get("Accept-Ranges").is_none(),
            "le HEAD d'une conversion ne doit pas annoncer Accept-Ranges"
        );
        assert_eq!(
            entetes
                .get("contentFeatures.dlna.org")
                .and_then(|v| v.to_str().ok()),
            Some("DLNA.ORG_OP=00;DLNA.ORG_FLAGS=01700000000000000000000000000000"),
        );
        assert_eq!(
            entetes
                .get("transferMode.dlna.org")
                .and_then(|v| v.to_str().ok()),
            Some("Streaming"),
        );
        assert_eq!(
            entetes.get("Content-Length").and_then(|v| v.to_str().ok()),
            Some("144"),
            "la longueur WAV calculée reste annoncée"
        );
    }

    use tune_core::http::streamer::{
        LIVE_BOUNDED_TOTAL_LEN, StreamInfo, StreamSession, build_wav_header_bounded_live,
        build_wav_header_streaming,
    };

    /// #1841 — le compteur d'octets ne bougeait que sur le chemin fichier.
    /// Radio et mandataire servaient des octets sans jamais le dire, et
    /// `output_reach` en concluait que personne n'ecoutait.
    #[tokio::test]
    async fn un_flux_servi_incremente_le_compteur_de_la_session() {
        use futures_util::StreamExt;
        use std::sync::atomic::Ordering::Relaxed;

        let session = std::sync::Arc::new(StreamSession::new(
            "test".into(),
            StreamInfo::default(),
            false,
            4,
        ));
        assert_eq!(session.bytes_sent.load(Relaxed), 0);

        let flux = futures_util::stream::iter(vec![
            Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"abc")),
            Ok(bytes::Bytes::from_static(b"defgh")),
        ]);
        let body = corps_compte(flux, session.clone());

        // Consommer le corps : c'est la lecture qui compte, pas sa creation.
        let mut flux_corps = body.into_data_stream();
        while let Some(morceau) = flux_corps.next().await {
            morceau.unwrap();
        }

        assert_eq!(
            session.bytes_sent.load(Relaxed),
            8,
            "trois octets puis cinq — ce que le client a reellement recu"
        );
    }

    /// Un corps qui n'est jamais lu n'a rien servi : le compteur doit rester
    /// a zero, sinon « quelqu'un ecoute » deviendrait vrai des la creation.
    #[tokio::test]
    async fn un_corps_non_consomme_ne_compte_rien() {
        use std::sync::atomic::Ordering::Relaxed;

        let session = std::sync::Arc::new(StreamSession::new(
            "test".into(),
            StreamInfo::default(),
            false,
            4,
        ));
        let flux = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(
            bytes::Bytes::from_static(b"abc"),
        )]);
        let _body = corps_compte(flux, session.clone());

        assert_eq!(session.bytes_sent.load(Relaxed), 0);
    }

    #[test]
    fn lavf_renderers_keep_the_chunked_contract() {
        // The Eversolo DMP-A10/A8 and every other libavformat renderer: without
        // the chunked, length-less body and its 0xFFFF_FFFF header they treat
        // the radio as a bounded file and cut every ~6 min (FIP, commit 3d5a3a8f).
        assert!(accepts_chunked_live_stream(Some("Lavf/58.45.100")));
        assert!(accepts_chunked_live_stream(Some("lavf/60.3.100")));
        assert!(accepts_chunked_live_stream(Some(
            "SomeRenderer (Lavf/59.27.100)"
        )));
    }

    #[test]
    fn other_renderers_get_the_file_contract() {
        // Yves' darTZeel LHC-208 (#1689): refuses chunked transfer, requires
        // Content-Length + Range. Everything that plays on it carries a length.
        assert!(!accepts_chunked_live_stream(Some("player/100")));
        assert!(!accepts_chunked_live_stream(Some("Sonos/84.1-56110")));
    }

    #[test]
    fn unknown_user_agent_keeps_current_behaviour() {
        // Blast radius: only a renderer that positively identifies itself as
        // something other than Lavf sees a different response.
        assert!(accepts_chunked_live_stream(None));
        assert!(accepts_chunked_live_stream(Some("")));
    }

    #[test]
    fn le_head_radio_stable_reutilise_le_contrat_live_du_renderer() {
        let mut req = axum::http::HeaderMap::new();
        req.insert("User-Agent", "Marantz ND8006".parse().unwrap());
        let rep = super::live_radio_head_response("audio/wav", &req);
        let expected_length = LIVE_BOUNDED_TOTAL_LEN.to_string();

        assert_eq!(rep.status(), axum::http::StatusCode::OK);
        assert_eq!(
            rep.headers()
                .get("Content-Type")
                .and_then(|v| v.to_str().ok()),
            Some("audio/wav")
        );
        assert_eq!(
            rep.headers()
                .get("Content-Length")
                .and_then(|v| v.to_str().ok()),
            Some(expected_length.as_str())
        );
        assert!(rep.headers().get("Accept-Ranges").is_some());
        assert!(rep.headers().get("Transfer-Encoding").is_none());
    }

    #[test]
    fn bounded_live_header_matches_the_announced_content_length() {
        // Le lecteur qui reçoit le contrat fichier lit Content-Length ET les
        // tailles de l'en-tête : les trois doivent concorder et rester
        // positives en 32 bits signés.
        let h = build_wav_header_bounded_live(2, 44100, 16);
        let data_size = u32::from_le_bytes([h[40], h[41], h[42], h[43]]);
        let riff_size = u32::from_le_bytes([h[4], h[5], h[6], h[7]]);
        assert_eq!(data_size as u64 + 44, LIVE_BOUNDED_TOTAL_LEN);
        assert!(data_size as i32 > 0);
        assert!(riff_size as i32 > 0);
        assert!(LIVE_BOUNDED_TOTAL_LEN <= i32::MAX as u64);

        // L'en-tête Lavf est justement celui qui passe en négatif — d'où les
        // deux contrats.
        let l = build_wav_header_streaming(2, 44100, 16);
        assert_eq!(u32::from_le_bytes([l[40], l[41], l[42], l[43]]) as i32, -1);
    }

    #[test]
    fn resume_after_the_wav_header_is_the_range_the_renderer_sends() {
        // Sur ses fichiers WAV locaux, le LHC repart systématiquement à
        // l'octet 44 — juste après l'en-tête. C'est ce Range qu'il faut
        // honorer sur la radio bornée : rien de l'en-tête, puis le direct.
        assert_eq!(parse_range_start("bytes=44-"), Some(44));
        let skip = parse_range_start("bytes=44-").unwrap().min(44) as usize;
        assert_eq!(skip, 44, "l'en-tête entier est sauté");

        // Une reprise au-delà de l'en-tête ne peut pas « chercher » dans un
        // direct : on plafonne le saut à la taille de l'en-tête et on sert le
        // direct maintenant.
        let far = parse_range_start("bytes=1048576-").unwrap().min(44) as usize;
        assert_eq!(far, 44);

        // Le sondage initial (bytes=0-) ne saute rien.
        assert_eq!(parse_range_start("bytes=0-").unwrap().min(44), 0);
    }

    /// #3513 — le navigateur était pris dans le filet de #1689.
    ///
    /// « Tout ce qui n'est pas Lavf » visait les renderers DLNA. Firefox,
    /// Chrome et Safari n'ont jamais refusé le chunké : c'est le transfert par
    /// défaut de HTTP/1.1. Servis comme un fichier de 2 Gio, ils faisaient ce
    /// qu'un fichier autorise — un `Range` — et le canal PCM à consommateur
    /// unique ne s'en relevait pas.
    #[test]
    fn un_navigateur_nest_pas_un_renderer() {
        assert!(accepts_chunked_live_stream(Some(
            "Mozilla/5.0 (X11; Linux x86_64; rv:141.0) Gecko/20100101 Firefox/141.0"
        )));
        assert!(accepts_chunked_live_stream(Some(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/139.0.0.0 Safari/537.36"
        )));
        assert!(accepts_chunked_live_stream(Some(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 \
             (KHTML, like Gecko) Version/18.5 Safari/605.1.15"
        )));

        // Contre-épreuve : les appareils de #1689 ne s'annoncent pas
        // « Mozilla », ils gardent le contrat fichier.
        assert!(!accepts_chunked_live_stream(Some("player/100")));
        assert!(!accepts_chunked_live_stream(Some("Sonos/84.1-56110")));
        assert!(!accepts_chunked_live_stream(Some("Marantz ND8006")));
        assert!(!accepts_chunked_live_stream(Some("LHC-208/1.0")));
    }

    #[test]
    fn parse_range_start_cases() {
        // Resume from a byte offset (DMP-A8 reconnect after a CDN drop).
        assert_eq!(parse_range_start("bytes=26590644-"), Some(26_590_644));
        assert_eq!(parse_range_start("bytes=0-"), Some(0));
        assert_eq!(parse_range_start("bytes=100-200"), Some(100));
        // Suffix range and malformed values yield no start.
        assert_eq!(parse_range_start("bytes=-500"), None);
        assert_eq!(parse_range_start("bytes=abc-"), None);
        assert_eq!(parse_range_start("chunks=0-"), None);
    }

    // ───────────────────────── #2161 — le titre d'une radio ─────────────────
    //
    // Le renderer DLNA restait figé sur le nom de la station. Rien ne partait :
    // `has_icy` était conditionné à `session.track_title`, jamais renseigné.
    // Ces trois épreuves sont hermétiques — aucune socket, aucune station.

    /// Une session radio servie à un renderer qui demande l'ICY, avec le PCM
    /// déjà en file et l'émetteur fermé : le corps est complet et fini.
    async fn corps_radio(
        stream_id: &str,
        agent: &str,
        demande_icy: bool,
        octets_pcm: usize,
    ) -> (axum::http::HeaderMap, Vec<u8>) {
        use axum::extract::{Path, State};
        use futures_util::StreamExt;
        use tune_core::http::streamer::SharedSessions;

        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 44100,
            bit_depth: 16,
            channels: 2,
            ..StreamInfo::default()
        };
        let mut session = StreamSession::new(stream_id.to_string(), info, false, 64);
        session.is_radio = true;
        let session = std::sync::Arc::new(session);
        // Sans format détecté, l'en-tête attend le décodeur dix secondes.
        session.publish_detected_output_format(44100, 2);

        let tx = session.tx.lock().await.clone().expect("tx");
        let mut reste = octets_pcm;
        while reste > 0 {
            let n = reste.min(8192);
            tx.send(vec![0xAA; n]).await.expect("pcm");
            reste -= n;
        }
        drop(tx);
        session.close_sender().await;

        let sessions: SharedSessions = std::sync::Arc::new(tokio::sync::Mutex::new(
            [(stream_id.to_string(), session)].into_iter().collect(),
        ));

        let mut req = axum::http::HeaderMap::new();
        req.insert("User-Agent", agent.parse().unwrap());
        if demande_icy {
            req.insert("Icy-MetaData", "1".parse().unwrap());
        }
        let rep =
            super::handle_stream(Path(format!("{stream_id}.wav")), State(sessions), req).await;
        let entetes = rep.headers().clone();

        let mut corps = rep.into_body().into_data_stream();
        let mut octets = Vec::new();
        while let Some(Ok(b)) = corps.next().await {
            octets.extend_from_slice(&b);
        }
        (entetes, octets)
    }

    /// Le canal ICY doit s'ouvrir pour un DIRECT, alors même que la session n'a
    /// aucun titre — elle n'en a jamais : `track_title` est posé à `None` par
    /// `StreamSession::new` et n'est écrit nulle part. La condition d'origine
    /// était donc toujours fausse, `icy-metaint` n'était jamais annoncé, et le
    /// rafraîchissement de #1473 restait injoignable (journal de Jean Valjean :
    /// pas une ligne « icy »).
    #[tokio::test]
    async fn un_direct_ouvre_le_canal_icy_sans_titre_de_session() {
        let (entetes, _) = corps_radio(
            "i2161-ouverture",
            "GStreamer souphttpsrc 1.22.12 libsoup/3.6.5",
            true,
            4096,
        )
        .await;

        assert_eq!(
            entetes.get("icy-metaint").and_then(|v| v.to_str().ok()),
            Some("16384"),
            "un direct demandé avec Icy-MetaData: 1 doit annoncer la fenêtre ICY"
        );
    }

    /// Le bloc porte le titre COURANT — celui que le poller vient de publier,
    /// pas celui de la connexion — et il tombe au 16384ᵉ octet **du corps**,
    /// en-tête WAV compris. Un bloc décalé de 44 octets ferait lire au renderer
    /// un octet de son comme longueur de métadonnées : du bruit.
    #[tokio::test]
    async fn le_bloc_icy_tombe_a_la_bonne_fenetre_et_porte_le_titre_courant() {
        use tune_core::http::streamer::{forget_radio_now, publish_radio_now};

        let sid = "i2161-fenetre";
        forget_radio_now(sid);
        publish_radio_now(sid, Some("Miles Davis".into()), "So What".into(), None);

        let (_, corps) = corps_radio(
            sid,
            "GStreamer souphttpsrc 1.22.12 libsoup/3.6.5",
            true,
            20_000,
        )
        .await;
        forget_radio_now(sid);

        assert!(
            corps.starts_with(b"RIFF"),
            "le corps commence par l'en-tête WAV"
        );
        assert!(
            corps.len() > 16_400,
            "corps trop court pour porter un bloc : {} octets",
            corps.len()
        );
        assert_eq!(
            corps[16_383], 0xAA,
            "l'octet qui précède la frontière doit encore être du son — \
             l'en-tête WAV compte dans la fenêtre ICY"
        );

        let longueur = corps[16_384] as usize;
        assert!(
            longueur > 0,
            "un bloc vide au moment où un titre est publié"
        );
        let charge = &corps[16_385..16_385 + longueur * 16];
        assert!(
            charge.starts_with(b"StreamTitle='"),
            "le bloc doit commencer EXACTEMENT au 16384e octet du corps ; \
             ici l'octet de longueur lu vaut {longueur} et ce qui suit n'est pas \
             un bloc ICY — la fenêtre est décalée"
        );
        let texte = String::from_utf8_lossy(charge);
        assert!(
            texte.contains("StreamTitle='Miles Davis - So What';"),
            "le bloc doit porter le titre courant, pas celui de la connexion : {texte:?}"
        );
    }

    /// La POCHETTE du morceau courant doit partir dans le bloc, au même titre
    /// que son titre.
    ///
    /// C'est le second témoignage de #2161 : « la pochette et le titre ne
    /// change pas sur le RS250A […] demeure avec la pochette et le titre de la
    /// première écoute » (Serge Asselin, fil 1529). La pochette était lue sur
    /// `session.cover_url`, capturé UNE FOIS hors de la boucle — et ce champ
    /// vaut toujours `None` : posé par `StreamSession::new`, écrit nulle part
    /// du dépôt, exactement comme `track_title` l'était. `StreamUrl='…'` ne
    /// quittait donc jamais ce serveur.
    #[tokio::test]
    async fn le_bloc_icy_porte_la_pochette_courante() {
        use tune_core::http::streamer::{forget_radio_now, publish_radio_now};

        let sid = "i2161-pochette";
        forget_radio_now(sid);
        publish_radio_now(
            sid,
            Some("Miles Davis".into()),
            "So What".into(),
            Some("https://img.radioparadise.com/covers/l/sowhat.jpg".into()),
        );

        let (_, corps) = corps_radio(
            sid,
            "GStreamer souphttpsrc 1.22.12 libsoup/3.6.5",
            true,
            20_000,
        )
        .await;
        forget_radio_now(sid);

        let longueur = corps[16_384] as usize;
        let charge = &corps[16_385..16_385 + longueur * 16];
        let texte = String::from_utf8_lossy(charge);
        assert!(
            texte.contains("StreamUrl='https://img.radioparadise.com/covers/l/sowhat.jpg';"),
            "le bloc doit porter la pochette publiée par le poller : {texte:?}"
        );
        assert!(
            texte.contains("StreamTitle='Miles Davis - So What';"),
            "le titre doit rester présent à côté de la pochette : {texte:?}"
        );
    }

    /// Sans pochette publiée, le bloc reste ce qu'il était : un `StreamTitle`
    /// seul. On n'invente pas d'URL, et un `StreamUrl=''` vide serait pire que
    /// pas de champ du tout — le renderer effacerait l'image qu'il affiche.
    #[tokio::test]
    async fn sans_pochette_publiee_le_bloc_ne_porte_aucun_streamurl() {
        use tune_core::http::streamer::{forget_radio_now, publish_radio_now};

        let sid = "i2161-sans-pochette";
        forget_radio_now(sid);
        publish_radio_now(sid, Some("Miles Davis".into()), "So What".into(), None);

        let (_, corps) = corps_radio(
            sid,
            "GStreamer souphttpsrc 1.22.12 libsoup/3.6.5",
            true,
            20_000,
        )
        .await;
        forget_radio_now(sid);

        let longueur = corps[16_384] as usize;
        let charge = &corps[16_385..16_385 + longueur * 16];
        let texte = String::from_utf8_lossy(charge);
        assert!(
            !texte.contains("StreamUrl"),
            "aucun StreamUrl ne doit partir quand la station n'en donne pas : {texte:?}"
        );
    }

    /// Piège du chantier : un renderer qui n'a pas demandé l'ICY ne doit RIEN
    /// voir changer. Pas d'en-tête `icy-metaint`, et pas un octet inséré dans
    /// le flux — sans quoi on aurait dégradé la lecture de tous ceux qui
    /// ignorent les métadonnées en cours de route.
    #[tokio::test]
    async fn un_renderer_qui_ne_demande_pas_l_icy_recoit_le_flux_intact() {
        let (entetes, corps) = corps_radio(
            "i2161-intact",
            "GStreamer souphttpsrc 1.22.12 libsoup/3.6.5",
            false,
            20_000,
        )
        .await;

        assert!(
            entetes.get("icy-metaint").is_none(),
            "aucune fenêtre ICY ne doit être annoncée sans Icy-MetaData: 1"
        );
        assert_eq!(
            corps.len(),
            44 + 20_000,
            "l'en-tête WAV puis le PCM, octet pour octet"
        );
        assert!(
            corps[44..].iter().all(|o| *o == 0xAA),
            "aucun octet de métadonnées ne doit s'être glissé dans le son"
        );
    }

    // ───────── #2991 — le poller doit pouvoir SAVOIR ce qui a été négocié ────
    //
    // Ces épreuves passent par `handle_stream`, la fonction de production, et
    // relisent le verdict par `canal_radio`, celle que le poller appelle. Rien
    // n'est transcrit : si la note cessait d'être posée dans `handle_stream`,
    // le poller conclurait « aucun renderer connecté » sur un renderer bel et
    // bien connecté — exactement le diagnostic qu'on cherche à rendre sûr.

    /// TÉMOIN. Le chemin qui marche aujourd'hui : un renderer qui demande
    /// `Icy-MetaData: 1` obtient la fenêtre, ET le poller l'apprend.
    #[tokio::test]
    async fn un_renderer_qui_demande_l_icy_est_note_comme_servi() {
        use tune_core::http::streamer::{CanalRadio, canal_radio, forget_icy_channel};

        let sid = "i2991-a4f218-icy-accorde";
        forget_icy_channel(sid);
        let (entetes, _) = corps_radio(
            sid,
            "GStreamer souphttpsrc 1.22.12 libsoup/3.6.5",
            true,
            4096,
        )
        .await;

        assert_eq!(
            entetes.get("icy-metaint").and_then(|v| v.to_str().ok()),
            Some("16384"),
            "témoin : la fenêtre ICY doit rester accordée exactement comme avant"
        );
        // CONTRE-ÉPREUVE #2991. Cet agent ne porte pas `Lavf` :
        // `accepts_chunked_live_stream` rend `false` et la radio lui est servie
        // au contrat FICHIER. Avant le correctif, la note ne le disait pas et
        // le verdict était le même que pour un corps chunké.
        assert_eq!(
            canal_radio(Some(sid)),
            CanalRadio::Icy { borne: true },
            "handle_stream doit avoir noté le canal accordé ET le contrat — sans cette note, \
             le poller ne peut pas distinguer « ça marche » de « personne n'écoute », \
             ni un direct chunké d'une réponse servie comme un fichier"
        );
        forget_icy_channel(sid);
    }
    /// TÉMOIN de l'autre contrat : un agent `Lavf` accepte le corps chunké et
    /// la note doit le dire. Sans ce second cas, `borne` pourrait valoir `true`
    /// partout sans qu'aucune épreuve ne s'en aperçoive.
    #[tokio::test]
    async fn un_renderer_lavf_est_note_sur_le_contrat_chunke() {
        use tune_core::http::streamer::{CanalRadio, canal_radio, forget_icy_channel};
        let sid = "i2991-b2092-icy-chunke";
        forget_icy_channel(sid);
        let (entetes, _) = corps_radio(sid, "Lavf/60.16.100", true, 4096).await;
        assert_eq!(
            entetes.get("icy-metaint").and_then(|v| v.to_str().ok()),
            Some("16384"),
            "témoin : la fenêtre ICY reste accordée sur le corps chunké"
        );
        assert_eq!(
            canal_radio(Some(sid)),
            CanalRadio::Icy { borne: false },
            "un agent Lavf accepte le direct chunké : la note doit le distinguer \
             d'une radio servie au contrat fichier"
        );
        forget_icy_channel(sid);
    }

    /// L'HYPOTHÈSE nº 1 du ticket, jamais vérifiée sur aucun appareil depuis le
    /// 22/08 : le renderer ne demande pas `Icy-MetaData: 1`. Elle laissait
    /// exactement la même trace que l'hypothèse nº 2 (pas de `stream_id`) —
    /// c'est-à-dire aucune. Elle rend maintenant un verdict qui lui est propre.
    #[tokio::test]
    async fn un_renderer_muet_sur_l_icy_est_note_comme_tel() {
        use tune_core::http::streamer::{CanalRadio, canal_radio, forget_icy_channel};

        let sid = "i2991-a4f218-icy-non-demande";
        forget_icy_channel(sid);
        let (entetes, _) = corps_radio(
            sid,
            "GStreamer souphttpsrc 1.22.12 libsoup/3.6.5",
            false,
            4096,
        )
        .await;

        assert!(
            entetes.get("icy-metaint").is_none(),
            "témoin : rien ne change pour un renderer qui n'a pas demandé l'ICY"
        );
        assert_eq!(
            canal_radio(Some(sid)),
            CanalRadio::IcyNonDemande,
            "le journal doit pouvoir NOMMER cette cause, au lieu de laisser \
             Bertrand hésiter entre deux branches"
        );
        forget_icy_channel(sid);
    }

    /// La branche fichier ne découpe pas le corps : elle ne peut porter aucun
    /// bloc, `Icy-MetaData: 1` ou non. « Servi par une voie sans ICY » et
    /// « aucun renderer connecté » sont deux diagnostics différents, et c'est
    /// justement celui-là qu'on n'avait pas.
    #[tokio::test]
    async fn la_branche_fichier_est_notee_comme_voie_sans_icy() {
        use axum::extract::{Path, State};
        use tune_core::http::streamer::{
            CanalRadio, SharedSessions, canal_radio, forget_icy_channel,
        };

        let sid = "i2991-a4f218-voie-fichier";
        forget_icy_channel(sid);

        // Fichier réel, dans un dossier unique par appel que `Drop` emporte —
        // panique comprise (#3030). D'autres agents tournent sur la même
        // machine et le répertoire temporaire est partagé.
        let bac = tune_core::test_scratch::scratch_dir("tune-i2991-a4f218");
        let chemin = bac.join("voie-fichier.wav");
        std::fs::write(&chemin, b"RIFF____WAVE").expect("fixture");

        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 44100,
            bit_depth: 16,
            channels: 2,
            ..StreamInfo::default()
        };
        let session = std::sync::Arc::new(StreamSession::new(sid.to_string(), info, false, 8));
        *session.file_path.lock().await = Some(chemin.to_string_lossy().to_string());

        let sessions: SharedSessions = std::sync::Arc::new(tokio::sync::Mutex::new(
            [(sid.to_string(), session)].into_iter().collect(),
        ));

        let mut req = axum::http::HeaderMap::new();
        // Le renderer DEMANDE l'ICY : c'est le cas piégeux, celui qu'on aurait
        // pris pour un succès.
        req.insert("Icy-MetaData", "1".parse().unwrap());
        let _ = super::handle_stream(Path(format!("{sid}.wav")), State(sessions), req).await;

        assert_eq!(
            canal_radio(Some(sid)),
            CanalRadio::VoieSansIcy,
            "servi par la branche fichier : aucun bloc ne partira, quoi que le \
             renderer ait demandé"
        );

        forget_icy_channel(sid);
        drop(bac);
    }

    // ───────────────── #3513 — la radio qui se tait à la 10e seconde ────────
    //
    // Fabien, v0.9.140 : une radio écoutée dans le navigateur se tait vers la
    // dixième seconde. Le serveur annonçait à Firefox un WAV de 2 Gio, Firefox
    // traitait la réponse comme un fichier borné et rouvrait la connexion avec
    // un `Range` ; le canal PCM à consommateur unique faisait que chaque
    // reconnexion supplantait la précédente. Six fois en un quart d'heure,
    // `radio_stream_superseded connected_secs=9` puis
    // `radio_stream_client_disconnect … remaining_consumers=0`.

    const NAVIGATEUR: &str =
        "Mozilla/5.0 (X11; Linux x86_64; rv:141.0) Gecko/20100101 Firefox/141.0";

    /// Rien, dans la réponse servie au navigateur, ne l'invite à se
    /// reconnecter : pas de longueur, pas d'`Accept-Ranges`, un corps chunké.
    /// C'est la CAUSE de #3513 qui disparaît, et non le symptôme rattrapé.
    #[tokio::test]
    async fn le_navigateur_ne_recoit_plus_de_longueur_a_reprendre() {
        let (entetes, octets) = corps_radio("i3513-contrat", NAVIGATEUR, false, 8192).await;

        assert!(
            entetes.get("Content-Length").is_none(),
            "un Content-Length de 2 Gio est ce qui faisait rouvrir Firefox par Range"
        );
        assert!(
            entetes.get("Accept-Ranges").is_none(),
            "annoncer les Range sur un direct, c'est les inviter"
        );
        assert!(
            entetes.get("Content-Range").is_none(),
            "un direct n'a pas de position, il ne peut pas en annoncer une"
        );
        assert_eq!(
            entetes
                .get("Transfer-Encoding")
                .and_then(|v| v.to_str().ok()),
            Some("chunked"),
            "le navigateur retrouve le contrat sans fin"
        );
        assert_eq!(
            entetes.get("Content-Type").and_then(|v| v.to_str().ok()),
            Some("audio/wav")
        );
        // Le son est bien là : en-tête WAV puis le PCM semé.
        assert_eq!(octets.len(), 44 + 8192);
        assert_eq!(&octets[..4], b"RIFF");
        assert!(octets[44..].iter().all(|o| *o == 0xAA));
    }

    /// Le témoin de #3513 : on rejoue la reconnexion par `Range` sur une
    /// session radio VIVANTE, et le son continue.
    ///
    /// Deux connexions successives sur la même station, la seconde portant
    /// `Range: bytes=44-` — exactement ce que faisait Firefox. La seconde
    /// prend la main sur le canal PCM (`claim_channel_consumer`), la première
    /// rend le canal sans consommer un morceau de plus, et **le direct semé
    /// après la reconnexion sort par la nouvelle connexion**. C'est la
    /// question posée par le ticket : « faut-il que la plus récente prenne la
    /// main sans couper le flux » — elle le fait déjà, ce qui manquait était
    /// un témoin qui le prouve.
    ///
    /// L'ancienne connexion est DRAINÉE en parallèle, et ce n'est pas un
    /// détail de mise en scène : un corps `axum` n'avance que lorsqu'on le
    /// tire, et c'est le serveur HTTP qui le tire en vrai. Sans ce drainage,
    /// la première connexion resterait garée dans `recv_chunk()` en tenant le
    /// verrou du canal, et l'essai attendrait pour de mauvaises raisons.
    #[tokio::test]
    async fn une_reconnexion_par_range_ne_coupe_pas_le_son() {
        use axum::extract::{Path, State};
        use futures_util::StreamExt;
        use tune_core::http::streamer::SharedSessions;

        let sid = "i3513-reconnexion";
        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 44100,
            bit_depth: 16,
            channels: 2,
            ..StreamInfo::default()
        };
        let mut session = StreamSession::new(sid.to_string(), info, false, 64);
        session.is_radio = true;
        let session = std::sync::Arc::new(session);
        // Sans format détecté, l'en-tête attend le décodeur dix secondes.
        session.publish_detected_output_format(44100, 2);
        let tx = session.tx.lock().await.clone().expect("tx");

        let sessions: SharedSessions = std::sync::Arc::new(tokio::sync::Mutex::new(
            [(sid.to_string(), session.clone())].into_iter().collect(),
        ));

        let requete = |range: Option<&str>| {
            let mut h = axum::http::HeaderMap::new();
            h.insert("User-Agent", NAVIGATEUR.parse().unwrap());
            if let Some(r) = range {
                h.insert("Range", r.parse().unwrap());
            }
            h
        };

        // ── Première connexion : l'onglet qui écoute ─────────────────────────
        let premiere = super::handle_stream(
            Path(format!("{sid}.wav")),
            State(sessions.clone()),
            requete(None),
        )
        .await;
        assert_eq!(premiere.status(), axum::http::StatusCode::OK);
        assert!(premiere.headers().get("Content-Length").is_none());
        let mut corps_premiere = premiere.into_body().into_data_stream();

        tx.send(vec![0x11; 4096]).await.expect("pcm avant reprise");
        let mut avant = Vec::new();
        while avant.len() < 44 + 4096 {
            let bloc =
                tokio::time::timeout(std::time::Duration::from_secs(10), corps_premiere.next())
                    .await
                    .expect("la premiere connexion doit recevoir du son")
                    .expect("le flux est ouvert")
                    .expect("bloc lisible");
            avant.extend_from_slice(&bloc);
        }
        assert_eq!(&avant[..4], b"RIFF");
        assert!(avant[44..].contains(&0x11));

        // ── La reconnexion décrite par #3513 ─────────────────────────────────
        let seconde = super::handle_stream(
            Path(format!("{sid}.wav")),
            State(sessions.clone()),
            requete(Some("bytes=44-")),
        )
        .await;
        assert_eq!(
            seconde.status(),
            axum::http::StatusCode::OK,
            "un direct n'est pas un fichier : pas de 206, pas de Content-Range"
        );
        assert!(seconde.headers().get("Content-Range").is_none());
        let mut corps_seconde = seconde.into_body().into_data_stream();

        // Premier morceau de la seconde connexion : l'en-tête WAV entier. Il
        // part avant que le canal PCM soit réclamé, il ne consomme donc rien.
        let entete = corps_seconde
            .next()
            .await
            .expect("le flux est ouvert")
            .expect("bloc lisible");
        assert_eq!(&entete[..4], b"RIFF", "la reprise renvoie l'en-tete entier");
        assert_eq!(entete.len(), 44);

        // Second sondage : c'est LUI qui fait réclamer le canal
        // (`claim_channel_consumer`) et gare la seconde connexion en attente de
        // direct. Rien n'a encore été semé, il doit donc expirer — et l'ordre
        // est ainsi fixé sans dépendre de celui dans lequel `join!` sonde.
        let rien =
            tokio::time::timeout(std::time::Duration::from_millis(300), corps_seconde.next()).await;
        assert!(
            rien.is_err(),
            "aucun direct n'a encore ete seme apres la reprise du canal"
        );

        // Le direct reprend. La première connexion est déjà supplantée : elle
        // rendra le canal sans consommer un morceau de plus.
        for _ in 0..16 {
            tx.send(vec![0x22; 4096]).await.expect("pcm apres reprise");
        }

        let drainage_premiere = async { while let Some(Ok(_)) = corps_premiere.next().await {} };
        let lecture_seconde = async {
            let mut recu = Vec::new();
            while recu.len() < 8192 {
                let bloc =
                    tokio::time::timeout(std::time::Duration::from_secs(10), corps_seconde.next())
                        .await
                        .expect("le son doit CONTINUER apres la reconnexion — c'est #3513")
                        .expect("le flux est ouvert")
                        .expect("bloc lisible");
                recu.extend_from_slice(&bloc);
            }
            recu
        };
        let (_, pcm) = tokio::join!(drainage_premiere, lecture_seconde);

        assert!(
            pcm.contains(&0x22),
            "la nouvelle connexion doit porter le direct semé APRES la reprise"
        );
        assert!(
            pcm.len() >= 8192,
            "au moins deux morceaux de direct, pas un souffle : {} octets",
            pcm.len()
        );
        assert_eq!(
            session
                .active_consumers
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "la premiere connexion a rendu le canal, la seconde le tient"
        );
    }

    /// La frontière tombe exactement sur la fin d'un morceau : le bloc part
    /// quand même, et la fenêtre repart à zéro.
    #[test]
    fn la_decoupe_icy_insere_un_bloc_a_chaque_frontiere() {
        let bloc = || vec![1u8, 0x42];
        let mut depuis = 0usize;

        let sorties = decoupe_icy(&vec![0u8; ICY_METAINT], &mut depuis, &bloc);
        let plat: Vec<u8> = sorties.iter().flat_map(|b| b.to_vec()).collect();
        assert_eq!(plat.len(), ICY_METAINT + 2);
        assert_eq!(&plat[ICY_METAINT..], &[1u8, 0x42]);
        assert_eq!(depuis, 0, "la fenêtre repart à zéro après un bloc");

        // Un morceau plus petit que la fenêtre passe sans rien insérer.
        let sorties = decoupe_icy(&[0u8; 10], &mut depuis, &bloc);
        assert_eq!(sorties.iter().map(|b| b.len()).sum::<usize>(), 10);
        assert_eq!(depuis, 10);
    }
    // ───────────────────── #1894 — le porteur DoP et les reprises ─────────────────────

    /// Un porteur DoP fabriqué par l'encodeur DE PRODUCTION, à partir d'un
    /// train DSD dont chaque octet est identifiable.
    ///
    /// L'objet éprouvé ici est le TRANSPORT HTTP, pas l'encodeur : la charge
    /// utile vient donc du chemin de production, et la règle qui la juge
    /// (`porteur_dop_lisible`) est écrite depuis la spécification DoP, jamais
    /// depuis le module transporté.
    fn porteur_dop_de_production(trames: usize) -> Vec<u8> {
        let mut dsd = Vec::with_capacity(trames * 4);
        for i in 0..(trames * 4) {
            dsd.push((i % 251) as u8);
        }
        let mut encodeur = tune_core::audio::dsd_to_dop::DsdToDoP::new(2, false);
        encodeur.feed(&dsd)
    }

    /// La condition de VERROUILLAGE d'un DAC en DoP, écrite depuis la spec.
    ///
    /// Un mot de 24 bits little-endian par canal : `[dsd_bas, dsd_haut,
    /// marqueur]`. Le marqueur vaut `0x05` ou `0xFA`, il est COMMUN aux canaux
    /// d'une même trame et il ALTERNE d'une trame à la suivante. Si l'une des
    /// trois conditions tombe, le DAC ne verrouille pas, joue le train DSD
    /// comme du PCM, et c'est du bruit blanc.
    ///
    /// Lu sur la grille DU RENDERER : il place le premier octet reçu à
    /// l'offset ANNONCÉ, puis se recale sur la prochaine frontière de trame.
    fn porteur_dop_lisible(recu: &[u8], offset_annonce: u64, canaux: usize) -> Result<(), String> {
        let trame = 3 * canaux;
        let dans_les_donnees = offset_annonce.saturating_sub(44);
        let recalage = ((trame as u64 - dans_les_donnees % trame as u64) % trame as u64) as usize;
        let corps = recu
            .get(recalage..)
            .ok_or_else(|| format!("moins de {recalage} octets reçus"))?;
        let n = corps.len() / trame;
        if n < 4 {
            return Err(format!("{n} trames seulement : rien à juger"));
        }
        let mut attendu: Option<u8> = None;
        for t in 0..n {
            let marqueur = corps[t * trame + 2];
            if marqueur != 0x05 && marqueur != 0xFA {
                return Err(format!(
                    "trame {t} : l'octet de poids fort vaut 0x{marqueur:02X}, ni 0x05 ni 0xFA — \
                     le DAC ne verrouille pas"
                ));
            }
            for ch in 1..canaux {
                let m = corps[t * trame + ch * 3 + 2];
                if m != marqueur {
                    return Err(format!(
                        "trame {t} : canal 0 porte 0x{marqueur:02X} et canal {ch} 0x{m:02X} — \
                         le marqueur doit être commun à la trame"
                    ));
                }
            }
            if let Some(a) = attendu
                && marqueur != a
            {
                return Err(format!(
                    "trame {t} : marqueur 0x{marqueur:02X} au lieu de 0x{a:02X} — \
                     l'alternance 0x05/0xFA est rompue"
                ));
            }
            attendu = Some(if marqueur == 0x05 { 0xFA } else { 0x05 });
        }
        Ok(())
    }

    /// Sert un porteur DoP sur une session de conversion, puis demande une
    /// reprise `Range: bytes={offset}-`. Rend les octets du corps.
    async fn reprise_sur_une_session_dop(offset: u64, charge: &[u8]) -> (String, Vec<u8>) {
        use axum::extract::{Path, State};
        use futures_util::StreamExt;
        use std::sync::atomic::Ordering::SeqCst;
        use tune_core::http::streamer::SharedSessions;

        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 176_400,
            channels: 2,
            bit_depth: 24,
            duration_ms: Some(600_000),
            ..StreamInfo::default()
        };
        let id = format!("dop{offset}");
        let session = std::sync::Arc::new(StreamSession::new(id.clone(), info, true, 64));
        session.wav_header_included.store(true, SeqCst);
        let tx = session.tx.lock().await.clone().expect("tx");
        session.close_sender().await;
        let sessions: SharedSessions = std::sync::Arc::new(tokio::sync::Mutex::new(
            [(id.clone(), session)].into_iter().collect(),
        ));

        tx.send(super::build_wav_header(2, 176_400, 24, None).to_vec())
            .await
            .expect("entête");
        for bloc in charge.chunks(4096) {
            tx.send(bloc.to_vec()).await.expect("charge");
        }
        drop(tx);

        let mut req = axum::http::HeaderMap::new();
        req.insert("Range", format!("bytes={offset}-").parse().unwrap());
        let rep = super::handle_stream(Path(format!("{id}.wav")), State(sessions), req).await;
        let plage = rep
            .headers()
            .get("Content-Range")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let mut corps = rep.into_body().into_data_stream();
        let mut recu = Vec::new();
        while let Ok(Some(Ok(b))) =
            tokio::time::timeout(std::time::Duration::from_secs(5), corps.next()).await
        {
            recu.extend_from_slice(&b);
        }
        (plage, recu)
    }

    /// #1894 — GARDE DE COMPORTEMENT.
    ///
    /// Une session de conversion est un tuyau : elle ne rejoue pas un octet
    /// passé. Le corps HTTP honore pourtant `Range: bytes=N-` par un vrai 206
    /// (sans quoi l'Eversolo DMP-A8 boucle), et le renderer range alors les
    /// octets reçus À PARTIR DE N.
    ///
    /// MESURE du défaut : sur une session DoP stéréo 24 bits, `bytes=8236-`
    /// rendait les octets de l'offset 44 — 8 192 octets d'écart, soit **2
    /// modulo la trame de 6**. Sur la grille du renderer, plus un seul octet de
    /// poids fort ne portait `0x05`/`0xFA` : le DAC ne verrouillait pas en DSD
    /// et jouait le train DSD comme du PCM, c'est-à-dire du bruit blanc.
    ///
    /// La position ne se rattrape pas ; la PHASE, si. Les quatre offsets
    /// couvrent les résidus 0, 2, 3 et 5 de la trame — un rognage nul et trois
    /// rognages différents.
    #[tokio::test]
    async fn une_reprise_dop_rend_un_porteur_lisible_sur_la_grille_annoncee() {
        let charge = porteur_dop_de_production(8192);
        porteur_dop_lisible(&charge, 44, 2)
            .expect("l'encodeur de production doit rendre un porteur DoP valide");

        // Les six résidus de la trame de 6 : 0, 1, 2, 4, 5, 3.
        for offset in [8_234u64, 8_235, 8_236, 8_238, 12_289, 20_483] {
            let (plage, recu) = reprise_sur_une_session_dop(offset, &charge).await;
            assert!(
                plage.starts_with(&format!("bytes {offset}-")),
                "le 206 doit annoncer l'offset demandé, il annonce « {plage} »"
            );
            assert!(
                recu.len() > 1024,
                "reprise à {offset} : {} octets reçus, rien à juger",
                recu.len()
            );
            if let Err(pourquoi) = porteur_dop_lisible(&recu, offset, 2) {
                panic!(
                    "reprise `Range: bytes={offset}-` : le porteur DoP est illisible sur la \
                     grille que le 206 annonce — {pourquoi}. C'est exactement le bruit blanc \
                     de #1894."
                );
            }
        }
    }

    /// #1894 — GARDE DE COMPORTEMENT : jamais deux en-têtes WAV.
    ///
    /// LA CAUSE MESURÉE du bruit blanc. `anticiper_le_dop` émet son propre
    /// en-tête WAV comme premier bloc du canal, mais sa session ne l'a jamais
    /// DÉCLARÉ (`wav_header_included`) — contrairement aux deux autres sessions
    /// de conversion. Ce corps en préfixait donc un second :
    ///
    /// ```text
    /// EN-TETES RIFF A    = [0, 44]
    /// LA CHARGE DoP COMMENCE A L'OCTET 88 (attendu : 44)
    /// DECALAGE = 44 octets, soit 2 modulo la trame de 6
    /// ```
    ///
    /// 44 octets d'en-tête pris pour de l'audio, puis TOUT le porteur décalé de
    /// 2 modulo la trame : le marqueur `0x05`/`0xFA` ne tombe sur l'octet de
    /// poids fort d'aucun mot de 24 bits, le DAC ne verrouille pas en DSD et
    /// joue le train DSD comme du PCM — du bruit blanc, dès le premier
    /// échantillon et sur toute la piste.
    ///
    /// La session est ici bâtie EXACTEMENT comme `anticiper_le_dop` la bâtit,
    /// drapeau non posé : c'est le filet du transport qui est éprouvé, celui
    /// qui nomme le défaut au journal au lieu de le laisser muet.
    #[tokio::test]
    async fn un_producteur_qui_emet_son_entete_n_en_fait_jamais_servir_deux() {
        use axum::extract::{Path, State};
        use futures_util::StreamExt;
        use tune_core::http::streamer::SharedSessions;

        let charge = porteur_dop_de_production(4096);
        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 176_400,
            bit_depth: 24,
            channels: 2,
            file_size: None,
            duration_ms: Some(300_000),
            ..StreamInfo::default()
        };
        // `create_session(wav_info, true, 128)` : `wav_header_included` reste
        // FAUX. On ne le pose PAS ici — c'est tout l'objet de la garde.
        let session = std::sync::Arc::new(StreamSession::new("dopreel".into(), info, true, 128));
        let tx = session.tx.lock().await.clone().expect("tx");
        session.close_sender().await;
        let sessions: SharedSessions = std::sync::Arc::new(tokio::sync::Mutex::new(
            [("dopreel".to_string(), session)].into_iter().collect(),
        ));
        tx.send(super::build_wav_header(2, 176_400, 24, None).to_vec())
            .await
            .expect("l'en-tête du producteur");
        for bloc in charge.chunks(4096) {
            tx.send(bloc.to_vec()).await.expect("charge");
        }
        drop(tx);

        let rep = super::handle_stream(
            Path("dopreel.wav".into()),
            State(sessions),
            axum::http::HeaderMap::new(),
        )
        .await;
        let mut corps = rep.into_body().into_data_stream();
        let mut recu = Vec::new();
        while let Ok(Some(Ok(b))) =
            tokio::time::timeout(std::time::Duration::from_secs(5), corps.next()).await
        {
            recu.extend_from_slice(&b);
        }

        let entetes: Vec<usize> = recu
            .windows(12)
            .enumerate()
            .filter(|(_, w)| &w[0..4] == b"RIFF" && &w[8..12] == b"WAVE")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            entetes,
            vec![0],
            "un seul en-tête WAV, à l'octet 0 — il y en a à {entetes:?}"
        );

        let debut = recu
            .windows(24)
            .position(|w| w == &charge[..24])
            .expect("la charge DoP doit être servie");
        assert_eq!(
            debut,
            44,
            "la charge DoP doit commencer juste après l'en-tête : elle commence à {debut},              soit {} octets trop loin — {} modulo la trame de 6",
            debut - 44,
            (debut - 44) % 6
        );

        if let Err(pourquoi) = porteur_dop_lisible(&recu[44..], 44, 2) {
            panic!(
                "le porteur DoP servi est illisible pour un renderer qui lit l'en-tête —                  {pourquoi}. C'est le bruit blanc de #1894."
            );
        }
    }

    /// #1894 — GARDE DE SOURCE : le rognage est bien celui qui remet la grille
    /// du renderer en phase, quel que soit le sens de l'écart.
    ///
    /// Elle tient l'arithmétique seule, sans HTTP : un `%` sur des `u64` là où
    /// l'écart peut être négatif rendrait un résidu faux sans rien casser
    /// d'autre, et la garde de comportement ci-dessus ne le verrait que sur un
    /// des quatre offsets.
    #[test]
    fn le_rognage_remet_la_grille_du_renderer_en_phase() {
        // Trame de 6 octets : DoP stéréo 24 bits.
        for annonce in 0u64..24 {
            for reel in 0u64..24 {
                let k = super::rognage_de_phase(annonce, reel, 6);
                assert!(k < 6, "le rognage ne doit jamais dépasser la trame : {k}");
                assert_eq!(
                    (reel + k as u64) % 6,
                    annonce % 6,
                    "annoncé {annonce}, réel {reel} : après {k} octets rognés la grille doit \
                     coïncider"
                );
            }
        }
        // Le canal EN AVANCE sur l'offset annoncé (le cas d'une connexion
        // avortée dont le tampon de coalescence a emporté des octets) : l'écart
        // est négatif, le rognage reste positif et juste.
        assert_eq!(super::rognage_de_phase(100, 104, 6), 2);
        // Le cas réel : une connexion avortée a emporté 65 536 octets dans son
        // tampon. 65 536 % 6 == 4, donc deux octets à rogner.
        assert_eq!(super::rognage_de_phase(44, 44 + 65_536, 6), 2);
        // Une avance qui tombe juste sur la trame ne coûte rien.
        assert_eq!(super::rognage_de_phase(44, 44 + 65_538, 6), 0);
        // Trame de 1 : rien à remettre en phase.
        assert_eq!(super::rognage_de_phase(7, 3, 1), 0);
        assert_eq!(super::rognage_de_phase(7, 3, 0), 0);
    }
}

/// Origine publique du relais Tune Bridge. Doit rester accordee au defaut code
/// en dur du client (`wss://bridge.mozaiklabs.fr/ws/server`,
/// tune-core/src/cloud/relay.rs) et a la base utilisee par les applications.
pub const RELAIS_ORIGINE: &str = "https://bridge.mozaiklabs.fr";

/// URL de flux joignable depuis l'exterieur, ou `None`.
///
/// `stream_url` est fabrique en adresse LAN absolue —
/// `http://192.168.1.18:8888/stream/<id>.flac`. Correct chez soi, inutilisable
/// ailleurs : depuis un telephone en 4G, cette adresse ne mene nulle part. La
/// navigation fonctionnait a travers le relais, mais le lecteur restait muet,
/// ce qui rend la panne d'autant plus deroutante.
///
/// Plutot que de faire dependre `stream_url` de la provenance de la requete —
/// ce qui obligerait a promener la `HeaderMap` a travers une dizaine
/// d'appelants, dont des chemins chauds — le serveur annonce les DEUX adresses
/// et laisse le client prendre celle qui le concerne. Il n'a pas a savoir par
/// ou on l'atteint ; il sait ou il est joignable.
///
/// `None` quand le pont est desactive : annoncer une adresse de relais pour un
/// serveur qui ne s'y enregistre pas serait un mensonge de plus.
pub fn stream_url_distant(
    backend: std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    stream_id: &str,
    ext: &str,
) -> Option<String> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(backend);
    let actif = settings
        .get("bridge_enabled")
        .ok()
        .flatten()
        .is_some_and(|v| v == "true" || v == "1");
    if !actif {
        return None;
    }
    let server_id = settings.get("server_id").ok().flatten()?;
    if server_id.is_empty() {
        return None;
    }
    Some(format!(
        "{RELAIS_ORIGINE}/stream/relay/{server_id}/{stream_id}.{ext}"
    ))
}

#[cfg(test)]
mod stream_url_distant_tests {
    use super::*;
    use std::sync::Arc;
    use tune_core::db::settings_repo::SettingsRepo;
    use tune_core::db::sqlite::SqliteDb;

    fn base() -> Arc<dyn tune_core::db::backend::DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        // CORE_SCHEMA ne cree PAS la table `settings` : elle arrive par
        // migration. Sans cet appel, chaque `set` echoue en « no such table »
        // et le test mesure une base incomplete en croyant mesurer le helper.
        tune_core::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    /// Le pont desactive : annoncer une adresse de relais pour un serveur qui
    /// ne s'y enregistre pas serait un mensonge de plus — la famille de
    /// defauts que ce depot passe sa semaine a corriger.
    #[test]
    fn pont_desactive_aucune_adresse_distante() {
        let b = base();
        SettingsRepo::with_backend(b.clone())
            .set("server_id", "abc-123")
            .unwrap();
        assert_eq!(stream_url_distant(b, "s1", "flac"), None);
    }

    /// Pont actif mais identifiant absent : on ne fabrique pas une URL avec un
    /// trou dedans.
    #[test]
    fn sans_identifiant_aucune_adresse_distante() {
        let b = base();
        SettingsRepo::with_backend(b.clone())
            .set("bridge_enabled", "true")
            .unwrap();
        assert_eq!(stream_url_distant(b, "s1", "flac"), None);
    }

    #[test]
    fn un_identifiant_vide_vaut_absence() {
        let b = base();
        let s = SettingsRepo::with_backend(b.clone());
        s.set("bridge_enabled", "true").unwrap();
        s.set("server_id", "").unwrap();
        assert_eq!(stream_url_distant(b, "s1", "flac"), None);
    }

    /// Le cas utile : le chemin doit correspondre EXACTEMENT a la route du
    /// relais, `/stream/relay/{server_id}/{*stream_path}` — sinon le proxy
    /// repond 404 et la lecture reste muette sans rien expliquer.
    #[test]
    fn pont_actif_adresse_conforme_a_la_route_du_relais() {
        let b = base();
        let s = SettingsRepo::with_backend(b.clone());
        s.set("bridge_enabled", "true").unwrap();
        s.set("server_id", "75f24b9e-fb8a-4de2-8007-99edd3454263")
            .unwrap();
        assert_eq!(
            stream_url_distant(b, "abcd", "flac"),
            Some(
                "https://bridge.mozaiklabs.fr/stream/relay/\
                 75f24b9e-fb8a-4de2-8007-99edd3454263/abcd.flac"
                    .to_string()
            )
        );
    }

    /// « 1 » vaut « true » : les reglages sont ecrits par plusieurs chemins et
    /// n'ont jamais eu de type booleen.
    #[test]
    fn le_pont_accepte_1_comme_true() {
        let b = base();
        let s = SettingsRepo::with_backend(b.clone());
        s.set("bridge_enabled", "1").unwrap();
        s.set("server_id", "x").unwrap();
        assert!(stream_url_distant(b, "s", "flac").is_some());
    }

    /// L'origine doit rester accordee au defaut code en dur cote client
    /// (tune-core/src/cloud/relay.rs). Si l'une change sans l'autre, le
    /// serveur s'enregistre quelque part et les clients demandent ailleurs.
    #[test]
    fn origine_accordee_au_client_de_relais() {
        assert_eq!(RELAIS_ORIGINE, "https://bridge.mozaiklabs.fr");
    }
}

#[cfg(test)]
mod long_wav_4016;

/// #4645 — la mesure du terrain perdu pendant le service d'un fichier.
///
/// Les chiffres de référence sont ceux du journal de Sevy Tabroc du
/// 21/09/2026 (Tune 0.9.159, darTZeel LHC-51 en DLNA, zone 10) : WAV
/// 44,1 kHz / 24 bits stéréo, 272 651 970 octets demandés pour une piste de
/// 1 030 431 ms, connexion fermée après 239 140 864 octets et 929 402 ms.
#[cfg(test)]
mod terrain_perdu_4645 {
    use super::{avance_de_livraison_ms, debit_nominal_octets_par_seconde, terrain_perdu_ms};

    /// Le nominal du WAV de Sevy : 44 100 × 2 × 3 = 264 600 octets/s.
    /// C'est ce chiffre qui a permis de lire son journal — comparé à lui, le
    /// débit servi (251,3 Kio/s, soit 257 331 o/s) est SOUS le temps réel,
    /// alors qu'il paraît sain comparé aux autres pistes de la matinée.
    #[test]
    fn le_nominal_du_wav_de_sevy_vaut_264600_octets_par_seconde() {
        assert_eq!(
            debit_nominal_octets_par_seconde("wav", 44_100, 24, 2),
            Some(264_600)
        );
        // Les pistes 16 bits de la même matinée, à 176 400 o/s : deuxième
        // nuage de débits du journal (173,9 à 176,5 Kio/s).
        assert_eq!(
            debit_nominal_octets_par_seconde("wav", 44_100, 16, 2),
            Some(176_400)
        );
    }

    /// Hors WAV, on ne rend RIEN plutôt qu'un chiffre faux : sur un format
    /// compressé la correspondance octets ↔ temps n'est pas linéaire.
    #[test]
    fn hors_wav_aucun_nominal_nest_rendu() {
        for format in ["flac", "mp3", "aac", "dsf", ""] {
            assert_eq!(
                debit_nominal_octets_par_seconde(format, 44_100, 24, 2),
                None,
                "{format} : un nominal calculé sur un format compressé mentirait"
            );
        }
    }

    /// Un flux servi exactement au temps réel n'a ni avance ni retard.
    #[test]
    fn une_livraison_au_temps_reel_na_aucune_avance() {
        assert_eq!(avance_de_livraison_ms(264_600, 264_600, 1_000), 0);
        assert_eq!(avance_de_livraison_ms(2_646_000, 264_600, 10_000), 0);
    }

    /// La mesure qui manquait au journal de Sevy : à la fermeture, la
    /// livraison accusait 25,6 s de retard sur l'horloge.
    #[test]
    fn a_la_fermeture_la_livraison_de_sevy_accusait_25_secondes_de_retard() {
        let avance = avance_de_livraison_ms(239_140_864, 264_600, 929_402);
        assert_eq!(avance, -25_620);
        assert!(
            avance < 0,
            "la livraison est passée SOUS le temps réel : c'est ce que le journal ne disait pas"
        );
    }

    /// Le terrain perdu se compte depuis le sommet, pas depuis zéro — sans
    /// quoi un renderer qui ne prend jamais d'avance (le profil du darTZeel)
    /// ne produirait aucune mesure.
    #[test]
    fn le_terrain_se_compte_depuis_le_sommet_pas_depuis_zero() {
        // Renderer sans tampon : sommet à 0, retard de 25,6 s ⇒ 25,6 s perdues.
        assert_eq!(terrain_perdu_ms(0, -25_620), 25_620);
        // Renderer avec 4 s de tampon, retombé à 0 : il a perdu ses 4 s,
        // alors qu'une lecture en absolu aurait dit « avance nulle, rien à
        // signaler ».
        assert_eq!(terrain_perdu_ms(4_000, 0), 4_000);
    }

    /// Gagner de l'avance n'est pas perdre du terrain.
    #[test]
    fn regagner_de_lavance_ne_compte_aucune_perte() {
        assert_eq!(terrain_perdu_ms(3_000, 3_000), 0);
        assert_eq!(terrain_perdu_ms(3_000, 9_000), 0);
    }
}

/// Deux défauts du chemin FICHIER de `serve_file`, tenus par leurs bords.
///
/// Ils vivaient tous deux dans les trois lignes qui lisaient l'en-tête
/// `Range` — voir [`interpreter_range`] pour le détail de ce qu'elles
/// faisaient.
#[cfg(test)]
mod range_du_chemin_fichier {
    use super::{DemandeDeRange, interpreter_range};

    /// DÉFAUT 1 — `bytes=N-` au-delà de la fin du fichier.
    ///
    /// L'ancien calcul faisait `end - start + 1` avec `end = taille - 1 <
    /// start` : une soustraction `u64` qui passe par le bas. `overflow-checks`
    /// est éteint en `release`, donc aucune panique — un `Content-Length`
    /// proche de 2^64 et un `Content-Range` à l'envers partaient au renderer.
    #[test]
    fn un_range_qui_commence_apres_la_fin_est_insatisfiable() {
        // Le cas exact du débordement : dernier octet = 42, demande à 9 999.
        assert_eq!(
            interpreter_range("bytes=9999-", 43),
            DemandeDeRange::Insatisfiable
        );
        // Juste au-delà du dernier octet : la frontière elle-même.
        assert_eq!(
            interpreter_range("bytes=43-", 43),
            DemandeDeRange::Insatisfiable
        );
        // Et sa jumelle bornée, qui débordait de la même façon.
        assert_eq!(
            interpreter_range("bytes=9999-19999", 43),
            DemandeDeRange::Insatisfiable
        );
        // TÉMOIN : le dernier octet, lui, reste servi.
        assert_eq!(
            interpreter_range("bytes=42-", 43),
            DemandeDeRange::Tranche { debut: 42, fin: 42 }
        );
        // Fichier vide : `taille - 1` débordait AUSSI, y compris sur la sonde
        // `bytes=0-` que tout renderer DLNA envoie.
        assert_eq!(
            interpreter_range("bytes=0-", 0),
            DemandeDeRange::Insatisfiable
        );
    }

    /// DÉFAUT 2 — `bytes=-N` demande les N DERNIERS octets.
    ///
    /// `split('-')` rendait `["", "N"]` : `""` ne se parse pas, `start`
    /// retombait sur `0`, et le serveur renvoyait les N PREMIERS octets sous
    /// un `Content-Range` qui les annonçait comme tels. Le client, lui, avait
    /// demandé la fin.
    #[test]
    fn un_range_suffixe_rend_la_fin_du_fichier_pas_son_debut() {
        assert_eq!(
            interpreter_range("bytes=-500", 2_000),
            DemandeDeRange::Tranche {
                debut: 1_500,
                fin: 1_999
            }
        );
        // Un suffixe plus grand que le fichier vaut le fichier entier
        // (RFC 9110 §14.1.2), pas une soustraction qui déborde.
        assert_eq!(
            interpreter_range("bytes=-5000", 2_000),
            DemandeDeRange::Tranche {
                debut: 0,
                fin: 1_999
            }
        );
        // `bytes=-0` ne désigne aucun octet.
        assert_eq!(
            interpreter_range("bytes=-0", 2_000),
            DemandeDeRange::Insatisfiable
        );
    }

    /// Les cas ordinaires, pour que les deux corrections ci-dessus ne
    /// puissent pas être obtenues en cassant le chemin qui marche : la sonde
    /// `bytes=0-` du Marantz, la reprise par tranches de l'Eversolo, et la
    /// borne haute rognée à la taille réelle.
    #[test]
    fn les_reprises_ordinaires_des_renderers_restent_servies_a_l_identique() {
        assert_eq!(
            interpreter_range("bytes=0-", 1_000),
            DemandeDeRange::Tranche { debut: 0, fin: 999 }
        );
        assert_eq!(
            interpreter_range("bytes=1310720-", 5_000_000),
            DemandeDeRange::Tranche {
                debut: 1_310_720,
                fin: 4_999_999
            }
        );
        assert_eq!(
            interpreter_range("bytes=44-99", 1_000),
            DemandeDeRange::Tranche { debut: 44, fin: 99 }
        );
        // Borne haute au-delà du fichier : rognée, pas refusée.
        assert_eq!(
            interpreter_range("bytes=44-99999", 1_000),
            DemandeDeRange::Tranche {
                debut: 44,
                fin: 999
            }
        );
    }

    /// Un en-tête qu'on ne sait pas lire doit être IGNORÉ (RFC 9110 §14.2),
    /// donc servir le fichier entier en 200 — jamais un 416, qui ferait
    /// renoncer un renderer sur une syntaxe qu'on n'a pas comprise.
    #[test]
    fn un_entete_illisible_est_ignore_et_non_refuse() {
        assert_eq!(
            interpreter_range("bytes=abc-", 1_000),
            DemandeDeRange::Totalite
        );
        assert_eq!(
            interpreter_range("chunks=0-", 1_000),
            DemandeDeRange::Totalite
        );
        assert_eq!(interpreter_range("bytes=", 1_000), DemandeDeRange::Totalite);
        // `first-pos > last-pos` : spec invalide, pas insatisfaisable.
        assert_eq!(
            interpreter_range("bytes=99-44", 1_000),
            DemandeDeRange::Totalite
        );
    }
}

/// Les mêmes deux défauts, cette fois par la fonction de PRODUCTION : la
/// réponse HTTP que `handle_stream` construit pour une session fichier.
///
/// Sans ce banc, `interpreter_range` pourrait être parfaite et `serve_file`
/// continuer à ignorer ce qu'elle rend.
#[cfg(test)]
mod range_du_chemin_fichier_de_bout_en_bout {
    use super::{StreamInfo, StreamSession, handle_stream};
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    use futures_util::StreamExt;
    use std::sync::Arc;
    use tune_core::http::streamer::SharedSessions;

    /// Une session FICHIER posée sur un fichier réel de `octets` octets, dont
    /// l'octet d'indice `i` vaut `i % 251` — un motif qui rend toute
    /// confusion début/fin VISIBLE.
    async fn session_fichier(
        id: &str,
        octets: usize,
    ) -> (tune_core::test_scratch::ScratchFile, SharedSessions) {
        // `scratch_file` et non un chemin composé à la main : le garde
        // `aucune_fuite_de_temporaires` (#3030) refuse le second.
        let fichier = tune_core::test_scratch::scratch_file(id, ".wav");
        let contenu: Vec<u8> = (0..octets).map(|i| (i % 251) as u8).collect();
        std::fs::write(fichier.path(), &contenu).expect("fichier de test");
        let info = StreamInfo {
            format: "flac".into(),
            mime_type: "audio/flac".into(),
            ..StreamInfo::default()
        };
        let session = Arc::new(StreamSession::new(id.into(), info, false, 8));
        *session.file_path.lock().await = Some(fichier.path().to_string_lossy().into_owned());
        let sessions: SharedSessions = Arc::new(tokio::sync::Mutex::new(
            [(id.to_string(), session)].into_iter().collect(),
        ));
        (fichier, sessions)
    }

    async fn demander(
        sessions: SharedSessions,
        id: &str,
        range: &str,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut entetes = HeaderMap::new();
        entetes.insert("Range", HeaderValue::from_str(range).unwrap());
        let reponse = handle_stream(Path(format!("{id}.flac")), State(sessions), entetes).await;
        let statut = reponse.status();
        let entetes_rendus = reponse.headers().clone();
        let mut corps = reponse.into_body().into_data_stream();
        let mut recu = Vec::new();
        while let Some(m) = corps.next().await {
            recu.extend_from_slice(&m.expect("erreur de flux"));
        }
        (statut, entetes_rendus, recu)
    }

    fn entete(entetes: &HeaderMap, nom: &str) -> String {
        entetes
            .get(nom)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("<absent>")
            .to_string()
    }

    /// DÉFAUT 1, de bout en bout. Avant le correctif, cette demande partait
    /// avec `Content-Length: 18446744073709542420` et
    /// `Content-Range: bytes 9999-9999/10000`… puis un corps vide.
    #[tokio::test]
    async fn un_range_au_dela_de_la_fin_repond_416_et_dit_la_taille() {
        let (_f, sessions) = session_fichier("range-416-bd71", 10_000).await;
        let (statut, entetes, corps) = demander(sessions, "range-416-bd71", "bytes=20000-").await;

        assert_eq!(
            statut,
            StatusCode::RANGE_NOT_SATISFIABLE,
            "un range qui commence après la fin doit valoir 416, pas un \
             Content-Length né d'une soustraction u64 qui déborde"
        );
        assert_eq!(
            entete(&entetes, "Content-Range"),
            "bytes */10000",
            "le 416 doit DIRE la taille réelle, sans quoi le renderer ne sait \
             pas où refaire sa demande"
        );
        assert!(
            corps.is_empty(),
            "un 416 ne porte pas d'octets audio : {} reçus",
            corps.len()
        );
    }

    /// DÉFAUT 2, de bout en bout : les 500 derniers octets, pas les
    /// 500 premiers.
    #[tokio::test]
    async fn un_range_suffixe_sert_la_fin_du_fichier() {
        const TAILLE: usize = 10_000;
        let (_f, sessions) = session_fichier("range-suffixe-bd71", TAILLE).await;
        let (statut, entetes, corps) = demander(sessions, "range-suffixe-bd71", "bytes=-500").await;

        assert_eq!(statut, StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            entete(&entetes, "Content-Range"),
            "bytes 9500-9999/10000",
            "le Content-Range doit annoncer la FIN du fichier"
        );
        assert_eq!(corps.len(), 500, "500 octets demandés, 500 servis");
        let attendu: Vec<u8> = (TAILLE - 500..TAILLE).map(|i| (i % 251) as u8).collect();
        assert_eq!(
            corps, attendu,
            "un suffixe `bytes=-500` doit rendre les 500 DERNIERS octets ; \
             servir les 500 premiers est la réponse à une autre question"
        );
    }

    /// TÉMOIN. La reprise par tranches de l'Eversolo — le cas COURANT — doit
    /// traverser ces corrections sans bouger d'un octet.
    #[tokio::test]
    async fn la_reprise_ordinaire_dun_renderer_est_servie_a_l_identique() {
        const TAILLE: usize = 10_000;
        let (_f, sessions) = session_fichier("range-temoin-bd71", TAILLE).await;
        let (statut, entetes, corps) = demander(sessions, "range-temoin-bd71", "bytes=4096-").await;

        assert_eq!(statut, StatusCode::PARTIAL_CONTENT);
        assert_eq!(entete(&entetes, "Content-Range"), "bytes 4096-9999/10000");
        assert_eq!(entete(&entetes, "Content-Length"), "5904");
        let attendu: Vec<u8> = (4096..TAILLE).map(|i| (i % 251) as u8).collect();
        assert_eq!(corps, attendu);
    }
}

/// #4958 — Beosound Stage (B&O), `714 Illegal MIME-type` sur toute lecture,
/// même après les deux reprises de #744.
///
/// Le banc rejoue le couple SOAP + HTTP de bout en bout : la VRAIE sortie
/// DLNA (`DlnaOutput::play_media`) parle à un faux renderer strict, et ce
/// renderer va chercher l'URL du flux sur le VRAI serveur de flux de ce
/// module (`router`), comme le Beosound le fait (un `HEAD` avant chaque
/// réponse, journal de FabienM du 24/09). Il refuse en 714 tout MIME absent
/// de son Sink — celui de la DIDL comme le `Content-Type` du `HEAD`. Son Sink
/// est l'extrait du journal : `audio/x-flac`, `audio/wav`… mais PAS
/// `audio/flac`.
#[cfg(test)]
mod reprise_714_sink_strict_de_bout_en_bout {
    use super::{StreamInfo, StreamSession, router};
    use axum::Router;
    use axum::extract::State;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tune_core::http::streamer::SharedSessions;
    use tune_core::outputs::dlna::DlnaOutput;
    use tune_core::outputs::traits::{OutputTarget, PlayMedia};

    /// Sink du Beosound Stage, extrait du journal de #4958.
    const SINK_BEOSOUND: &[&str] = &[
        "http-get:*:audio/x-flac:*",
        "http-get:*:audio/wav:*",
        "http-get:*:audio/wave:*",
        "http-get:*:audio/x-wav:*",
        "http-get:*:audio/mpeg:*",
        "http-get:*:audio/l16;rate=44100;channels=2:*",
        "http-get:*:audio/l16;rate=44100;channels=2:DLNA.ORG_PN=LPCM",
    ];

    #[derive(Clone, Default)]
    struct Renderer {
        sink: Arc<Vec<String>>,
        current_uri: Arc<Mutex<String>>,
        /// Pour chaque SetAVTransportURI : (MIME de la DIDL, Content-Type du HEAD).
        essais: Arc<Mutex<Vec<(String, String)>>>,
    }

    fn base_du_sink(entree: &str) -> String {
        let champ = entree.split(':').nth(2).unwrap_or("");
        champ
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
    }

    impl Renderer {
        fn accepte(&self, mime: &str) -> bool {
            let m = mime
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            self.sink.iter().any(|e| base_du_sink(e) == m)
        }
    }

    fn soap(action: &str, service: &str, inner: &str) -> String {
        format!(
            r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:{action}Response xmlns:u="urn:schemas-upnp-org:service:{service}:1">{inner}</u:{action}Response></s:Body></s:Envelope>"#
        )
    }

    fn faute_714() -> String {
        r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>714</errorCode><errorDescription>Illegal MIME-type</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#.to_string()
    }

    fn action_de(corps: &str) -> String {
        corps
            .find("<u:")
            .map(|i| &corps[i + 3..])
            .and_then(|r| r.find([' ', '>']).map(|f| r[..f].to_string()))
            .unwrap_or_default()
    }

    fn balise(xml: &str, nom: &str) -> String {
        let (o, f) = (format!("<{nom}>"), format!("</{nom}>"));
        xml.find(&o)
            .and_then(|d| {
                let d = d + o.len();
                xml[d..].find(&f).map(|e| xml[d..d + e].to_string())
            })
            .unwrap_or_default()
    }

    /// Le MIME de la DIDL : 3ᵉ champ du `protocolInfo` de `<res>`.
    fn mime_de_la_didl(corps: &str) -> String {
        let didl = balise(corps, "CurrentURIMetaData")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&amp;", "&");
        didl.find("protocolInfo=\"")
            .map(|i| &didl[i + "protocolInfo=\"".len()..])
            .and_then(|r| r.split(':').nth(2))
            .unwrap_or("")
            .to_string()
    }

    async fn av(State(r): State<Renderer>, corps: String) -> axum::response::Response {
        let action = action_de(&corps);
        match action.as_str() {
            "SetAVTransportURI" => {
                let uri = balise(&corps, "CurrentURI");
                let didl = mime_de_la_didl(&corps);
                // Comme le Beosound : un HEAD sur l'URL avant de répondre.
                let ct = tune_core::http::client::shared()
                    .head(&uri)
                    .send()
                    .await
                    .ok()
                    .and_then(|rep| {
                        rep.headers()
                            .get("Content-Type")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                r.essais.lock().await.push((didl.clone(), ct.clone()));
                if !r.accepte(&didl) || !r.accepte(&ct) {
                    return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, faute_714())
                        .into_response();
                }
                *r.current_uri.lock().await = uri;
                soap(&action, "AVTransport", "").into_response()
            }
            "GetTransportInfo" => soap(
                &action,
                "AVTransport",
                "<CurrentTransportState>STOPPED</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed>",
            )
            .into_response(),
            "GetMediaInfo" => {
                let uri = r.current_uri.lock().await.clone();
                soap(
                    &action,
                    "AVTransport",
                    &format!("<NrTracks>1</NrTracks><CurrentURI>{uri}</CurrentURI>"),
                )
                .into_response()
            }
            "GetPositionInfo" => {
                let uri = r.current_uri.lock().await.clone();
                soap(
                    &action,
                    "AVTransport",
                    &format!("<Track>1</Track><TrackURI>{uri}</TrackURI><RelTime>0:00:00</RelTime>"),
                )
                .into_response()
            }
            _ => soap(&action, "AVTransport", "").into_response(),
        }
    }

    async fn cm(State(r): State<Renderer>) -> axum::response::Response {
        soap(
            "GetProtocolInfo",
            "ConnectionManager",
            &format!("<Source></Source><Sink>{}</Sink>", r.sink.join(",")),
        )
        .into_response()
    }

    async fn rc(corps: String) -> axum::response::Response {
        soap(&action_de(&corps), "RenderingControl", "").into_response()
    }

    async fn ecouter(app: Router) -> (u16, tokio::task::JoinHandle<()>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let h = tokio::spawn(async move {
            axum::serve(l, app).await.ok();
        });
        (port, h)
    }

    /// Joue un FLAC servi `audio/flac` vers un renderer au Sink donné, et rend
    /// l'issue et les essais vus par le renderer.
    async fn jouer_un_flac(id: &str, sink: &[&str]) -> (Result<(), String>, Vec<(String, String)>) {
        let fichier = tune_core::test_scratch::scratch_file(id, ".flac");
        std::fs::write(fichier.path(), vec![0u8; 4096]).expect("fichier de test");
        let info = StreamInfo {
            format: "flac".into(),
            mime_type: "audio/flac".into(),
            ..StreamInfo::default()
        };
        let session = Arc::new(StreamSession::new(id.into(), info, false, 8));
        *session.file_path.lock().await = Some(fichier.path().to_string_lossy().into_owned());
        let sessions: SharedSessions = Arc::new(tokio::sync::Mutex::new(
            [(id.to_string(), session)].into_iter().collect(),
        ));
        let (port_flux, flux) = ecouter(router(sessions)).await;

        let renderer = Renderer {
            sink: Arc::new(sink.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        };
        let app = Router::new()
            .route("/AVTransport/control", post(av))
            .route("/ConnectionManager/control", post(cm))
            .route("/RenderingControl/control", post(rc))
            .with_state(renderer.clone());
        let (port_r, vie) = ecouter(app).await;
        let base = format!("http://127.0.0.1:{port_r}");
        let sortie = DlnaOutput::new(
            "Parents".into(),
            "uuid:28630ca6-cd4f-4a15-80ae-f170dc890b20".into(),
            "127.0.0.1".into(),
            format!("{base}/AVTransport/control"),
            format!("{base}/RenderingControl/control"),
            Some(format!("{base}/ConnectionManager/control")),
        );
        let url = format!("http://127.0.0.1:{port_flux}/stream/{id}.flac");
        let issue = sortie
            .play_media(&PlayMedia {
                url: &url,
                mime_type: "audio/flac",
                title: Some("Far from Any Road"),
                sample_rate: Some(44_100),
                bit_depth: Some(16),
                channels: Some(2),
                ..Default::default()
            })
            .await;
        let essais = renderer.essais.lock().await.clone();
        vie.abort();
        flux.abort();
        (issue, essais)
    }

    /// LE CAS DE #4958. Sink sans `audio/flac` : la reprise « orthographe
    /// exacte » annonce `audio/x-flac` dans la DIDL — le `HEAD` doit dire la
    /// même chose, sans quoi le renderer refuse toutes les reprises.
    #[tokio::test]
    async fn la_reprise_orthographe_exacte_aligne_le_content_type_du_head() {
        let (issue, essais) = jouer_un_flac("beosound-4958-a1", SINK_BEOSOUND).await;
        assert!(
            issue.is_ok(),
            "le Sink liste audio/x-flac : la lecture doit passer à la reprise \
             « orthographe exacte ». Issue : {issue:?} ; essais (DIDL, HEAD) : {essais:?}"
        );
        let dernier = essais.last().expect("au moins un SetAVTransportURI");
        assert_eq!(
            dernier,
            &("audio/x-flac".to_string(), "audio/x-flac".to_string()),
            "DIDL et Content-Type du HEAD doivent porter la même orthographe : {essais:?}"
        );
    }

    /// TÉMOIN des autres renderers : un Sink qui liste `audio/flac` passe du
    /// premier coup, avec le `Content-Type` de la session, inchangé.
    #[tokio::test]
    async fn un_sink_qui_liste_audio_flac_ne_change_rien() {
        let (issue, essais) = jouer_un_flac(
            "sink-flac-4958-b2",
            &["http-get:*:audio/flac:*", "http-get:*:audio/wav:*"],
        )
        .await;
        assert!(issue.is_ok(), "{issue:?} ; {essais:?}");
        assert_eq!(
            essais,
            vec![("audio/flac".to_string(), "audio/flac".to_string())],
            "un seul essai, au MIME de la session"
        );
    }
}

/// Le mandataire doit répondre aux reprises `Range` d'un renderer Lavf comme
/// une session de FICHIER : un vrai 206 depuis N, `Content-Range` exact,
/// `Content-Length` du reste — y compris pour une PETITE reprise sous le
/// seuil de transfert au CDN, qui recevait un 200 depuis l'octet 0.
///
/// Mesuré sur le .18 le 23/09/2026 (Abacab, DSD64 relayé depuis le .15,
/// Eversolo DMP-A8) : `bytes=0-`, `bytes=294338652-` (les 187 derniers
/// octets, le chunk ID3), `bytes=28-` — puis les trois à nouveau, trois fois
/// de suite, position 0 pendant 45 s. Ce témoin rejoue exactement ces trois
/// sondes, dans cet ordre, et compare octet pour octet ce qui revient.
#[cfg(test)]
mod temoins_du_mandataire_dsf {
    use axum::extract::{Path, State};
    use futures_util::StreamExt;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tune_core::http::streamer::{SharedSessions, StreamInfo, StreamSession};

    const LAVF: &str = "Lavf/58.45.100";

    /// Un « .dsf » de 4 096 octets : 28 octets de « DSD  », le reste
    /// numéroté, 187 octets de « métadonnées » en fin. Le contenu n'a pas à
    /// se décoder — c'est le TRANSPORT qu'on éprouve, octet pour octet.
    fn corps() -> Vec<u8> {
        (0..4096u32).map(|i| (i % 251) as u8).collect()
    }

    /// Un serveur média qui honore `Range: bytes=N-` par un 206 exact, et
    /// sert le tout en 200 sinon — ce que fait la route audio de Tune.
    async fn serveur_media(corps: Vec<u8>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/api/v1/library/tracks/581978/audio",
            listener.local_addr().unwrap()
        );
        let tache = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let corps = corps.clone();
                tokio::spawn(async move {
                    let mut requete = Vec::new();
                    let mut octet = [0u8; 1];
                    while !requete.ends_with(b"\r\n\r\n") && requete.len() < 16_384 {
                        if socket.read_exact(&mut octet).await.is_err() {
                            return;
                        }
                        requete.push(octet[0]);
                    }
                    let texte = String::from_utf8_lossy(&requete).to_string();
                    let debut = texte
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("Range: bytes=")
                                .or(l.strip_prefix("range: bytes="))
                        })
                        .and_then(|r| r.split('-').next()?.parse::<usize>().ok());
                    let total = corps.len();
                    let (statut, entetes, tranche) = match debut {
                        Some(n) if n < total => (
                            "206 Partial Content",
                            format!(
                                "Content-Range: bytes {n}-{}/{total}\r\nContent-Length: {}\r\n",
                                total - 1,
                                total - n
                            ),
                            &corps[n..],
                        ),
                        _ => ("200 OK", format!("Content-Length: {total}\r\n"), &corps[..]),
                    };
                    let entete = format!(
                        "HTTP/1.1 {statut}\r\nContent-Type: application/x-dsd\r\nAccept-Ranges: bytes\r\n{entetes}Connection: close\r\n\r\n"
                    );
                    let _ = socket.write_all(entete.as_bytes()).await;
                    let _ = socket.write_all(tranche).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (url, tache)
    }

    async fn lire_tout(reponse: axum::response::Response) -> Vec<u8> {
        let mut flux = reponse.into_body().into_data_stream();
        let mut tout = Vec::new();
        while let Some(bloc) = tokio::time::timeout(std::time::Duration::from_secs(10), flux.next())
            .await
            .expect("le corps doit arriver")
        {
            tout.extend_from_slice(&bloc.expect("bloc lisible"));
        }
        tout
    }

    #[tokio::test]
    async fn les_trois_sondes_du_dmp_a8_recoivent_chacune_un_206_exact() {
        let corps = corps();
        let total = corps.len() as u64;
        let (upstream, _serveur) = serveur_media(corps.clone()).await;

        let sid = "dsf-mandataire";
        let info = StreamInfo {
            format: "dsf".into(),
            mime_type: "application/x-dsd".into(),
            sample_rate: 2_822_400,
            bit_depth: 1,
            channels: 2,
            ..StreamInfo::default()
        };
        let session = StreamSession::new(sid.to_string(), info, false, 128);
        *session.proxy_url.lock().await = Some(upstream);
        let session = Arc::new(session);
        let sessions: SharedSessions = Arc::new(tokio::sync::Mutex::new(
            [(sid.to_string(), session)].into_iter().collect(),
        ));
        let requete = |range: &str| {
            let mut h = axum::http::HeaderMap::new();
            h.insert("User-Agent", LAVF.parse().unwrap());
            h.insert("Range", range.parse().unwrap());
            h
        };

        // Les trois sondes, dans l'ordre du journal.
        let fin_id3 = total - 187;
        for (range, debut) in [
            ("bytes=0-".to_string(), 0u64),
            (format!("bytes={fin_id3}-"), fin_id3),
            ("bytes=28-".to_string(), 28u64),
        ] {
            let rep = super::handle_stream(
                Path(format!("{sid}.dsf")),
                State(sessions.clone()),
                requete(&range),
            )
            .await;
            assert_eq!(
                rep.status(),
                axum::http::StatusCode::PARTIAL_CONTENT,
                "{range} : une reprise reçoit un 206, comme sur une session de fichier"
            );
            let entetes = rep.headers().clone();
            assert_eq!(
                entetes.get("Content-Range").and_then(|v| v.to_str().ok()),
                Some(format!("bytes {debut}-{}/{total}", total - 1).as_str()),
                "{range} : le Content-Range doit partir de l'octet demandé"
            );
            assert_eq!(
                entetes.get("Content-Length").and_then(|v| v.to_str().ok()),
                Some((total - debut).to_string().as_str()),
                "{range} : la longueur est celle du reste"
            );
            assert_eq!(
                entetes.get("Content-Type").and_then(|v| v.to_str().ok()),
                Some("application/x-dsd")
            );
            assert!(entetes.get("Accept-Ranges").is_some(), "{range}");
            let octets = lire_tout(rep).await;
            assert_eq!(
                octets,
                corps[debut as usize..].to_vec(),
                "{range} : les octets livrés sont ceux du fichier à partir de {debut}"
            );
        }
    }

    /// Sous le seuil de transfert au CDN (1 Mio pour un Lavf), la reprise est
    /// servie par un SAUT local : même contrat pour le renderer, aucun Range
    /// de plus vers l'amont.
    #[tokio::test]
    async fn une_petite_reprise_sous_le_seuil_recoit_aussi_un_206_exact() {
        let corps = corps();
        let total = corps.len() as u64;
        let (upstream, _serveur) = serveur_media(corps.clone()).await;
        let sid = "dsf-mandataire-petit";
        let info = StreamInfo {
            format: "dsf".into(),
            mime_type: "application/x-dsd".into(),
            ..StreamInfo::default()
        };
        let session = StreamSession::new(sid.to_string(), info, false, 128);
        *session.proxy_url.lock().await = Some(upstream);
        let sessions: SharedSessions = Arc::new(tokio::sync::Mutex::new(
            [(sid.to_string(), Arc::new(session))].into_iter().collect(),
        ));
        let mut h = axum::http::HeaderMap::new();
        h.insert("User-Agent", LAVF.parse().unwrap());
        h.insert("Range", "bytes=4000-".parse().unwrap());
        let rep = super::handle_stream(Path(format!("{sid}.dsf")), State(sessions), h).await;
        assert_eq!(rep.status(), axum::http::StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            rep.headers()
                .get("Content-Range")
                .and_then(|v| v.to_str().ok()),
            Some(format!("bytes 4000-{}/{total}", total - 1).as_str())
        );
        assert_eq!(lire_tout(rep).await, corps[4000..].to_vec());
    }
}
