//! Axum HTTP handlers for audio streaming.
//!
//! The business logic (session management, buffer handling) lives in
//! `tune_core::http::streamer`. This module provides the HTTP layer only.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use tracing::{info, warn};

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
fn accepts_chunked_live_stream(user_agent: Option<&str>) -> bool {
    match user_agent {
        Some(ua) if !ua.is_empty() => ua.to_ascii_lowercase().contains("lavf"),
        _ => true,
    }
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
        HeaderValue::from_str(&session.info.mime_type).unwrap(),
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
    info!(
        stream_id,
        range = range_hdr,
        agent = user_agent.as_deref().unwrap_or("-"),
        format = %session.info.format,
        "stream_request"
    );
    session.first_request.notify_waiters();

    // File serving with Range support
    let file_path = session.file_path.lock().await.clone();
    if let Some(ref path) = file_path {
        return serve_file(path, &session.info, &req_headers, session.clone()).await;
    }

    // Proxy mode
    let proxy_url = session.proxy_url.lock().await.clone();
    if let Some(ref url) = proxy_url {
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
        HeaderValue::from_str(&session.info.mime_type).unwrap(),
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

    let wants_icy = req_headers
        .get("Icy-MetaData")
        .and_then(|v| v.to_str().ok())
        == Some("1");

    if wants_icy && (session.track_title.is_some() || session.track_artist.is_some()) {
        headers.insert("icy-metaint", HeaderValue::from(ICY_METAINT as u64));
    }

    let sr = session.info.sample_rate;
    let bd = session.info.bit_depth;
    let ch = session.info.channels;
    let dur_ms = session.info.duration_ms;

    let has_icy = wants_icy && (session.track_title.is_some() || session.track_artist.is_some());
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
    let data_ready = session.data_ready.clone();
    // Les six `yield` de cette branche — en-tete WAV, blocs ICY, morceaux de
    // radio, deux vidages de tampon — sont comptes par `corps_compte`.
    let compteur = session.clone();
    let flux = async_stream::stream! {
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
                yield Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(&hdr[header_skip..]));
            }
        }

        if has_icy {
            let mut bytes_since_meta: usize = 0;
            while let Some(chunk) = session.recv_chunk().await {
                let mut offset = 0;
                while offset < chunk.len() {
                    let remaining = ICY_METAINT - bytes_since_meta;
                    let end = (offset + remaining).min(chunk.len());
                    yield Ok(bytes::Bytes::copy_from_slice(&chunk[offset..end]));
                    bytes_since_meta += end - offset;
                    offset = end;
                    if bytes_since_meta >= ICY_METAINT {
                        // Relire le titre courant a CHAQUE emission : c'est la
                        // seule chose que le renderer verra changer.
                        let block = match tune_core::http::streamer::radio_now(&icy_stream_id) {
                            Some((artist, title)) => build_icy_metadata(
                                artist.as_deref(),
                                Some(&title),
                                icy_cover.as_deref(),
                            ),
                            None => icy_block.clone(),
                        };
                        yield Ok(bytes::Bytes::from(block));
                        bytes_since_meta = 0;
                    }
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
                            Some(chunk) => yield Ok(bytes::Bytes::from(chunk)),
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

            // ── L'en-tête WAV doit survivre aux connexions de sonde ──
            //
            // Sur une conversion, l'en-tête est le premier chunk DU CANAL : la
            // connexion de sonde le consomme et la connexion de lecture ne voit
            // que du PCM nu — injouable. Celui qui le voit passer le met de
            // côté ; toute connexion suivante partant de l'octet 0 le reçoit
            // d'abord. `bytes=44-` dit explicitement « je l'ai déjà » : on ne
            // le renvoie pas.
            let saute_entete = req_headers
                .get("Range")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_range_start)
                .is_some_and(|s| s >= 44);
            if is_wav
                && wav_header_included
                && !saute_entete
                && let Some(entete) = session.wav_header_stash.get()
            {
                yield Ok(bytes::Bytes::from(entete.clone()));
            }

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

                tokio::select! {
                    biased;
                    _ = &mut superseded => continue,
                    maybe_chunk = session.recv_chunk() => {
                        let Some(chunk) = maybe_chunk else {
                            // Canal fermé : fin de piste. Vider ce qui reste.
                            if !coalesce_buf.is_empty() {
                                let restant = std::mem::take(&mut coalesce_buf);
                                yield Ok(bytes::Bytes::from(restant));
                            }
                            break;
                        };
                        // Mettre l'en-tête de côté au passage, pour les
                        // connexions suivantes. `set` n'écrit qu'une fois.
                        if is_wav
                            && wav_header_included
                            && chunk.len() >= 44
                            && chunk.starts_with(b"RIFF")
                            && session.wav_header_stash.get().is_none()
                        {
                            let _ = session.wav_header_stash.set(chunk[..44].to_vec());
                            // La connexion qui a demandé `bytes=44-` ne veut
                            // PAS l'en-tête : on ne transmet que la suite.
                            if saute_entete {
                                if chunk.len() > 44 {
                                    coalesce_buf.extend_from_slice(&chunk[44..]);
                                }
                                while coalesce_buf.len() >= MIN_HTTP_CHUNK {
                                    let flushed: Vec<u8> = coalesce_buf.drain(..MIN_HTTP_CHUNK).collect();
                                    yield Ok(bytes::Bytes::from(flushed));
                                }
                                continue;
                            }
                        }
                        coalesce_buf.extend_from_slice(&chunk);
                        while coalesce_buf.len() >= MIN_HTTP_CHUNK {
                            let flushed: Vec<u8> = coalesce_buf.drain(..MIN_HTTP_CHUNK).collect();
                            yield Ok(bytes::Bytes::from(flushed));
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

    if let Some(range) = range_header {
        let range_str = range.replace("bytes=", "");
        let parts: Vec<&str> = range_str.split('-').collect();
        let start: u64 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
        let end: u64 = parts
            .get(1)
            .and_then(|s| if s.is_empty() { None } else { s.parse().ok() })
            .unwrap_or(file_size - 1);
        let length = end - start + 1;

        let mut headers = HeaderMap::new();
        headers.insert(
            "Content-Type",
            HeaderValue::from_str(&info.mime_type).unwrap(),
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
        );
        return (StatusCode::PARTIAL_CONTENT, headers, body).into_response();
    }

    // Full file
    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_str(&info.mime_type).unwrap(),
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

    let body = build_file_body(faststart, path.to_string(), 0, file_size, session.clone());
    (StatusCode::OK, headers, body).into_response()
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

fn build_file_body(
    faststart: Option<tune_core::audio::faststart::FaststartMap>,
    path: String,
    start: u64,
    length: u64,
    byte_counter: std::sync::Arc<StreamSession>,
) -> Body {
    use std::sync::atomic::Ordering::Relaxed;
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
                            return;
                        }
                        let mut buf = vec![0u8; 65536];
                        while remaining > 0 {
                            let to_read = (remaining as usize).min(buf.len());
                            match file.read(&mut buf[..to_read]).await {
                                Ok(0) => break,
                                Ok(n) => {
                                    remaining -= n as u64;
                                    byte_counter.bytes_sent.fetch_add(n as u64, Relaxed);
                                    yield Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(&buf[..n]));
                                }
                                Err(e) => { warn!(error = %e, "file_read_error"); break; }
                            }
                        }
                    }
                    Err(e) => warn!(error = %e, "file_open_error"),
                }
            }
        } else {
            match tokio::fs::File::open(&path).await {
                Ok(mut file) => {
                    if let Err(e) = file.seek(std::io::SeekFrom::Start(start)).await {
                        warn!(error = %e, "file_seek_error");
                        return;
                    }
                    let mut buf = vec![0u8; 65536];
                    while remaining > 0 {
                        let to_read = (remaining as usize).min(buf.len());
                        match file.read(&mut buf[..to_read]).await {
                            Ok(0) => break,
                            Ok(n) => {
                                remaining -= n as u64;
                                byte_counter.bytes_sent.fetch_add(n as u64, Relaxed);
                                yield Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(&buf[..n]));
                            }
                            Err(e) => { warn!(error = %e, "file_read_error"); break; }
                        }
                    }
                }
                Err(e) => warn!(error = %e, "file_open_error"),
            }
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
fn resumable_proxy_body(
    client: &'static reqwest::Client,
    upstream_url: String,
    initial: reqwest::Response,
    abs_offset: u64,
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
        let mut resumes: u32 = 0;
        loop {
            let mut stream = resp.bytes_stream();
            let mut clean_eof = true;
            loop {
                match stream.next().await {
                    Some(Ok(chunk)) => {
                        pos += chunk.len() as u64;
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
    use super::{accepts_chunked_live_stream, corps_compte, parse_range_start};

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
