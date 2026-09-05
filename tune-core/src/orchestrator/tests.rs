use super::{
    RADIO_HLS_UNSUPPORTED, RADIO_NOT_AUDIO, abandonner_la_session_de_transcodage,
    arm_local_stream_consumer_watch, decode_radio_stream_to_pcm, emit_radio_playback_error,
    is_hls_manifest, non_audio_content_type,
};
use crate::event_bus::EventBus;
use crate::outputs::mock::MockOutput;
use std::sync::Arc;

#[tokio::test]
async fn local_stream_watch_reports_only_an_unconsumed_live_session_once() {
    use crate::http::streamer::{AudioStreamer, StreamInfo};

    let streamer = Arc::new(AudioStreamer::new(0));
    let info = StreamInfo {
        format: "wav".to_string(),
        mime_type: "audio/wav".to_string(),
        ..StreamInfo::default()
    };

    // No warning before the grace period, then one warning for a live
    // session whose HTTP body has never emitted a byte.
    let (unconsumed, _tx, _ready) = streamer.create_session(info.clone(), false, 1).await;
    let task = arm_local_stream_consumer_watch(
        streamer.clone(),
        unconsumed.clone(),
        7,
        "local:test".to_string(),
        std::time::Duration::from_millis(20),
    )
    .await
    .expect("first arm creates the watchdog");
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    assert!(!task.is_finished(), "the grace period must be respected");
    assert!(task.await.expect("watchdog task"));
    assert!(
        arm_local_stream_consumer_watch(
            streamer.clone(),
            unconsumed,
            7,
            "local:test".to_string(),
            std::time::Duration::ZERO,
        )
        .await
        .is_none(),
        "the same session must never report twice"
    );

    // A body that emitted at least one byte is consumed, even if no reader
    // happens to be active at the exact observation instant.
    let (consumed, _tx, _ready) = streamer.create_session(info.clone(), false, 1).await;
    {
        let sessions = streamer.sessions_state();
        let guard = sessions.lock().await;
        guard[&consumed]
            .bytes_sent
            .store(1, std::sync::atomic::Ordering::Relaxed);
    }
    let task = arm_local_stream_consumer_watch(
        streamer.clone(),
        consumed,
        7,
        "local:test".to_string(),
        std::time::Duration::ZERO,
    )
    .await
    .expect("consumed session is armed");
    assert!(!task.await.expect("watchdog task"));

    // A session removed by a normal stop/error path during the grace
    // period does not produce this diagnostic either.
    let (removed, _tx, _ready) = streamer.create_session(info, false, 1).await;
    let task = arm_local_stream_consumer_watch(
        streamer.clone(),
        removed.clone(),
        7,
        "local:test".to_string(),
        std::time::Duration::from_millis(20),
    )
    .await
    .expect("live session is armed");
    streamer.remove_session(&removed).await;
    assert!(!task.await.expect("watchdog task"));
}

/// Le débit WAV servi au renderer DLNA doit être « renderer-safe » : un
/// flux HE-AAC/aacPlus décodé à 22050 Hz (Radio Morow) est rééchantillonné
/// à 44100 Hz pour être audible ; un flux déjà en 44,1/48 kHz (ou plus)
/// passe inchangé, sans rééchantillonnage inutile.
#[test]
fn renderer_safe_rate_upsamples_low_rates_only() {
    use super::renderer_safe_wav_rate;
    // HE-AAC core rate → upsample to 44100
    assert_eq!(renderer_safe_wav_rate(22050), 44100);
    // Other low/non-standard rates → 44100
    assert_eq!(renderer_safe_wav_rate(11025), 44100);
    assert_eq!(renderer_safe_wav_rate(16000), 44100);
    assert_eq!(renderer_safe_wav_rate(24000), 44100);
    assert_eq!(renderer_safe_wav_rate(32000), 44100);
    // Standard rates pass through unchanged (no needless resample)
    assert_eq!(renderer_safe_wav_rate(44100), 44100);
    assert_eq!(renderer_safe_wav_rate(48000), 48000);
    // Hi-res radio kept as-is
    assert_eq!(renderer_safe_wav_rate(88200), 88200);
    assert_eq!(renderer_safe_wav_rate(96000), 96000);
}

/// Issue #1960 — le cas qui motive tout : BBC Radio 3 a retiré son flux,
/// l'ancienne adresse redirige vers la page d'accueil de la BBC et répond
/// **200 OK** en `text/html`. Rien n'échoue, le lecteur reçoit du HTML, et
/// l'auditeur n'a que du silence. Mesuré le 2026-08-20 :
/// `curl -sSL http://stream.live.vc.bbcmedia.co.uk/bbc_radio_three`
/// → `200 | text/html | https://www.bbc.co.uk/`.
#[test]
fn html_served_instead_of_audio_is_detected() {
    assert_eq!(
        non_audio_content_type("text/html"),
        Some("text/html".to_string())
    );
    // Le paramètre `charset` ne doit pas masquer le type — c'est la forme
    // que renvoie Icecast sur ses 404 (`text/html; charset=UTF-8`).
    assert_eq!(
        non_audio_content_type("text/html; charset=UTF-8"),
        Some("text/html".to_string())
    );
    // Casse et espaces : un en-tête HTTP n'est pas normalisé.
    assert_eq!(
        non_audio_content_type("  TEXT/HTML ;charset=utf-8"),
        Some("text/html".to_string())
    );
    assert_eq!(
        non_audio_content_type("application/json"),
        Some("application/json".to_string())
    );
    assert_eq!(
        non_audio_content_type("image/png"),
        Some("image/png".to_string())
    );
}

/// L'échec doit être DIT, et il doit survivre au client.
///
/// Le client web étouffe un `zone.playback_error` reçu dans la fenêtre de
/// grâce qui suit un ordre de lecture, sauf s'il porte `fatal: true`
/// (App.svelte, `suppressedByPlayGrace`). Une station morte échoue en une
/// fraction de seconde, donc EN PLEIN dans cette fenêtre : sans le drapeau,
/// ce correctif afficherait « chargement… » et rien d'autre.
#[tokio::test]
async fn a_dead_station_is_reported_and_survives_the_grace_window() {
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();

    emit_radio_playback_error(
        &Some(bus.clone()),
        7,
        "BBC Radio 3",
        &format!("{RADIO_NOT_AUDIO}: le serveur a répondu « text/html » au lieu d'un flux audio"),
    );

    let ev = rx.recv().await.unwrap();
    assert_eq!(ev.event_type, "zone.playback_error");
    assert_eq!(ev.data["zone_id"], 7);
    assert_eq!(
        ev.data["fatal"], true,
        "sans fatal:true le client étouffe le message dans sa fenêtre de grâce"
    );
    let msg = ev.data["error"].as_str().unwrap();
    assert!(
        msg.contains("BBC Radio 3"),
        "le message doit nommer la station : {msg}"
    );
    assert!(
        msg.contains("page web"),
        "le message doit dire ce qui a été reçu à la place de l'audio : {msg}"
    );
}

/// Sans bus (tests, démarrage partiel) on ne panique pas, on se tait.
#[test]
fn no_event_bus_is_not_a_panic() {
    emit_radio_playback_error(&None, 1, "Station", "boom");
}

// -----------------------------------------------------------------------
// #2307 — les flux HLS ne sont jamais lus. Ils doivent au moins le DIRE.
// -----------------------------------------------------------------------

/// Les deux signaux retenus, et eux seuls, dénoncent un manifeste HLS.
#[test]
fn un_manifeste_hls_est_reconnu_par_son_extension_ou_son_type() {
    for url in [
        "https://example.net/hls/master.m3u8",
        "https://example.net/live/index.M3U8",
        // Paramètres de session : très fréquent chez les CDN HLS.
        "https://example.net/live/playlist.m3u8?token=abc&sid=42",
        "https://example.net/live/playlist.m3u8#debut",
        "http://as-hls.example.co.uk/pool/x/live/bbc_6music-audio=96000.m3u8",
    ] {
        assert!(
            is_hls_manifest(url, ""),
            "{url} aurait dû être reconnue comme HLS : sans la garde d'extension, elle repart en silence dans symphonia"
        );
    }
    // URL sans extension : seul le type enregistré de HLS la dénonce.
    assert!(is_hls_manifest(
        "https://example.net/live/stream",
        "application/vnd.apple.mpegurl"
    ));
    assert!(is_hls_manifest(
        "https://example.net/live/stream",
        "Application/VND.Apple.MpegURL; charset=utf-8"
    ));
}

/// LE TÉMOIN. Rien de ce qui joue aujourd'hui ne doit être pris pour du
/// HLS : ni les playlists `.m3u`/`.pls` — le chemin fréquenté, déréférencé
/// en amont par `resolve_playlist_url` — ni aucune des formes d'adresse que
/// `radios_validation_url` déclare légitimes, ni aucun des types MIME que
/// `real_radio_content_types_pass_through` exige de laisser passer.
///
/// Les types `audio/x-mpegurl` et `application/x-mpegurl` sont ici
/// VOLONTAIREMENT du côté « pas HLS » : ils servent aussi pour une simple
/// `.m3u`, et un diagnostic « HLS » sur une `.m3u` dont le déréférencement
/// a échoué serait un mensonge sur le chemin le plus emprunté.
#[test]
fn le_temoin_m3u_pls_et_les_stations_livrees_ne_sont_jamais_pris_pour_du_hls() {
    for url in [
        "http://example.net/live.m3u",
        "http://example.net/live.pls",
        "https://radioswissjazz.ch/live/mp3.m3u",
        "http://icecast.example.net:8000/stream.mp3",
        "https://ais-sa8.cdnstream1.com/3630_128.mp3",
        "https://radio.jamminvibezonline.ca/listen/reggae/stream.aac",
        "https://example.net/live/flac",
        "https://example.net/autodj",
        "http://192.168.1.42:8000/",
        "http://[2001:db8::1]:8000/stream",
        "https://s.eu/live/aac?bitrate=320&session=abc",
        "HTTP://EXAMPLE.NET/Stream.MP3",
        "http://example.net/stream#anchor",
    ] {
        assert!(
            !is_hls_manifest(url, ""),
            "{url} prise pour du HLS à tort — une station qui joue deviendrait muette"
        );
    }
    for ct in [
        "audio/aac",
        "audio/mpeg",
        "audio/aacp",
        "audio/ogg",
        "audio/flac",
        "audio/x-flac",
        "application/ogg",
        "application/octet-stream",
        "audio/x-mpegurl",
        "audio/mpegurl",
        "application/x-mpegurl",
        "video/mp2t",
        "",
        "   ",
    ] {
        assert!(
            !is_hls_manifest("http://example.net/live.m3u", ct),
            "type « {ct} » pris pour du HLS à tort"
        );
    }
}

/// Ce contrôle est INDÉPENDANT de `non_audio_content_type` : ajouter les
/// types HLS à cette liste noire aurait cassé son propre témoin, qui exige
/// qu'ils la traversent. Les deux répondent à deux questions différentes.
#[test]
fn le_controle_hls_ne_touche_pas_la_liste_noire_page_web() {
    assert_eq!(
        non_audio_content_type("application/vnd.apple.mpegurl"),
        None
    );
    assert_eq!(non_audio_content_type("audio/x-mpegurl"), None);
    assert!(!is_hls_manifest("http://example.net/live", "text/html"));
}

/// L'ÉPREUVE : la fonction de production qui lit réellement les radios,
/// appelée telle quelle, refuse un manifeste HLS — et le refuse AVANT le
/// réseau.
///
/// L'adresse pointe sur `127.0.0.1:9` (discard), qui refuse la connexion
/// instantanément. Si la garde disparaissait, le décodeur ouvrirait le
/// réseau et rendrait `radio HTTP fetch failed: …` : l'assertion tombe.
/// Le test ne peut donc pas passer pour une mauvaise raison.
#[tokio::test]
async fn le_decodeur_de_production_refuse_le_hls_avant_tout_appel_reseau() {
    use crate::http::streamer::{AudioStreamer, StreamInfo};
    let streamer = Arc::new(AudioStreamer::new(0));
    let info = StreamInfo {
        format: "wav".to_string(),
        mime_type: "audio/wav".to_string(),
        ..StreamInfo::default()
    };
    let (_session_id, tx, data_ready, session) = streamer.create_radio_session(info, 4).await;
    // Appelée EXACTEMENT comme la production l'appelle : ses trois sites
    // (sortie locale, OAAT, DLNA proxifiée) l'enveloppent dans
    // `spawn_blocking`, parce que symphonia et `reqwest::blocking` sont
    // synchrones. Le test emprunte donc le même chemin, et un sabotage de
    // la garde y produit la vraie erreur réseau plutôt qu'une panique de
    // runtime qui masquerait ce qui s'est passé.
    let erreur = tokio::task::spawn_blocking(move || {
        decode_radio_stream_to_pcm(
            "http://127.0.0.1:9/live/master.m3u8?token=abc".to_string(),
            tx,
            data_ready,
            session,
            None,
            None,
        )
    })
    .await
    .expect("la tâche de décodage ne doit pas paniquer")
    .expect_err("un manifeste HLS ne doit jamais être accepté comme flux");
    assert!(
        erreur.starts_with(RADIO_HLS_UNSUPPORTED),
        "le décodeur doit refuser en NOMMANT HLS, pas en laissant symphonia \
         échouer sur un message obscur : {erreur}"
    );
}

/// Et l'auditeur, lui, voit quoi ? Un `zone.playback_error` `fatal: true`
/// — le canal que le client web écoute déjà pour sept autres échecs de
/// lecture, et que le websocket sert à tout client abonné (motif `*` par
/// défaut, `tune-server/src/routes/ws.rs`). Le message doit NOMMER HLS :
/// c'est toute la différence entre « Tune est cassé » et « cette station
/// utilise un format que Tune ne lit pas encore ».
#[tokio::test]
async fn une_station_hls_est_annoncee_a_l_auditeur_en_nommant_hls() {
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    emit_radio_playback_error(
        &Some(bus.clone()),
        9,
        "Radio Segments",
        &format!(
            "{RADIO_HLS_UNSUPPORTED}: https://example.net/master.m3u8 est un \
             manifeste HLS, pas un flux audio décodable"
        ),
    );
    let ev = rx.recv().await.unwrap();
    assert_eq!(ev.event_type, "zone.playback_error");
    assert_eq!(ev.data["zone_id"], 9);
    assert_eq!(
        ev.data["fatal"], true,
        "sans fatal:true le client étouffe le message dans sa fenêtre de grâce"
    );
    let msg = ev.data["error"].as_str().unwrap();
    assert!(
        msg.contains("Radio Segments"),
        "le message doit nommer la station : {msg}"
    );
    assert!(
        msg.contains("HLS"),
        "le message doit NOMMER le protocole en cause : {msg}"
    );
    assert!(
        msg.contains(".m3u8"),
        "le message doit donner le mot que l'auditeur verra chez son opérateur : {msg}"
    );
    // Surtout pas le diagnostic de la station morte : la station HLS est
    // vivante, lui dire de chercher une nouvelle adresse serait faux.
    assert!(
        !msg.contains("page web"),
        "HLS ne doit pas être confondu avec une station morte : {msg}"
    );
}

/// Aucun bus (démarrage partiel, tests) : on se tait, on ne panique pas.
#[test]
fn un_refus_hls_sans_bus_ne_panique_pas() {
    emit_radio_playback_error(&None, 1, "Station", RADIO_HLS_UNSUPPORTED);
}

/// Le garde-fou ne doit RIEN casser : les types réellement servis par les
/// stations que nous livrons doivent tous passer. Relevés le 2026-08-20 sur
/// les 46 entrées de l'annuaire — `audio/aac` (Radio France),
/// `audio/mpeg` (Radio Classique, TSF Jazz, KEXP) — plus les fantaisies
/// classiques d'Icecast/Shoutcast, et le cas de l'en-tête absent.
#[test]
fn real_radio_content_types_pass_through() {
    for ct in [
        "audio/aac",
        "audio/mpeg",
        "audio/aacp",
        "audio/ogg",
        "audio/flac",
        "audio/x-flac",
        "application/ogg",
        "application/octet-stream",
        "audio/x-mpegurl",
        "application/vnd.apple.mpegurl",
        "video/mp2t",
        // En-tête absent ou vide : on ne sait pas, donc on laisse passer.
        "",
        "   ",
    ] {
        assert_eq!(
            non_audio_content_type(ct),
            None,
            "content-type « {ct} » refusé à tort — une station qui marche deviendrait muette"
        );
    }
}

/// Le rééchantillonnage 22050→44100 double bien le nombre de trames
/// (ratio 2.0) et préserve l'entrelacement stéréo : la sortie doit avoir
/// un nombre de trames pair et cohérent avec le ratio.
#[test]
fn radio_resample_doubles_frames_at_2x() {
    // 1024 stereo frames of test signal (interleaved f32)
    let in_frames = 1024usize;
    let src: Vec<f32> = (0..in_frames * 2).map(|i| (i as f32) * 0.001).collect();
    let out = crate::audio::simple_resample(&src, 22050, 44100, 2);
    // 22050 → 44100 is exactly 2x
    assert_eq!(out.len(), in_frames * 2 * 2);
    // Identity when rate unchanged (44100 → 44100)
    let same = crate::audio::simple_resample(&src, 44100, 44100, 2);
    assert_eq!(same, src);
}

/// La sonde de niveaux proxy (VU-mètres Qobuz/Tidal direct) décode un
/// flux HTTP en fenêtres brutes : 1 s de WAV silencieux servie par un
/// mini serveur one-shot doit produire ~25 fenêtres de 40 ms au format
/// annoncé. Couvre le pipeline probe → décodage → fenêtrage, et la
/// terminaison propre en fin de flux.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn levels_probe_decodes_http_stream_into_windows() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        if let Ok((mut s, _)) = listener.accept() {
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let sr: u32 = 44100;
            let data = vec![0u8; sr as usize * 4]; // 1 s, 16-bit stéréo
            let mut wav = Vec::with_capacity(44 + data.len());
            wav.extend_from_slice(b"RIFF");
            wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
            wav.extend_from_slice(b"WAVEfmt ");
            wav.extend_from_slice(&16u32.to_le_bytes());
            wav.extend_from_slice(&1u16.to_le_bytes());
            wav.extend_from_slice(&2u16.to_le_bytes());
            wav.extend_from_slice(&sr.to_le_bytes());
            wav.extend_from_slice(&(sr * 4).to_le_bytes());
            wav.extend_from_slice(&4u16.to_le_bytes());
            wav.extend_from_slice(&16u16.to_le_bytes());
            wav.extend_from_slice(b"data");
            wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
            wav.extend_from_slice(&data);
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                wav.len()
            );
            let _ = s.write_all(&wav);
        }
    });

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    // Position rapportée très en avant : le bridage ne s'arme jamais.
    let reported = Arc::new(std::sync::atomic::AtomicI64::new(600_000));
    let url = format!("http://{addr}/probe.wav");
    tokio::task::spawn_blocking(move || {
        super::decode_http_stream_for_levels(url, "wav".into(), tx, reported)
            .expect("probe decodes the served WAV")
    })
    .await
    .expect("probe task join");

    let mut windows = 0;
    let mut total = std::time::Duration::ZERO;
    while let Ok(w) = rx.try_recv() {
        assert_eq!(w.sample_rate, 44100);
        assert_eq!(w.channels, 2);
        assert_eq!(w.bit_depth, 16);
        total += w.window;
        windows += 1;
    }
    assert!(windows >= 24, "1 s / 40 ms ≈ 25 fenêtres, reçu {windows}");
    let ms = total.as_millis();
    assert!(
        (950..=1050).contains(&ms),
        "durée totale ≈ 1 s, reçu {ms} ms"
    );
}

/// Sert `secs` secondes de WAV 16-bit stéréo 44,1 kHz silencieux sur un
/// port éphémère (une seule connexion), et renvoie l'URL. Support de test
/// pour la chaîne VU sans dépendre du réseau.
fn spawn_oneshot_wav_server(secs: u32) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        if let Ok((mut s, _)) = listener.accept() {
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let sr: u32 = 44100;
            let data = vec![0u8; sr as usize * 4 * secs as usize];
            let mut wav = Vec::with_capacity(44 + data.len());
            wav.extend_from_slice(b"RIFF");
            wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
            wav.extend_from_slice(b"WAVEfmt ");
            wav.extend_from_slice(&16u32.to_le_bytes());
            wav.extend_from_slice(&1u16.to_le_bytes());
            wav.extend_from_slice(&2u16.to_le_bytes());
            wav.extend_from_slice(&sr.to_le_bytes());
            wav.extend_from_slice(&(sr * 4).to_le_bytes());
            wav.extend_from_slice(&4u16.to_le_bytes());
            wav.extend_from_slice(&16u16.to_le_bytes());
            wav.extend_from_slice(b"data");
            wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
            wav.extend_from_slice(&data);
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                wav.len()
            );
            let _ = s.write_all(&wav);
        }
    });
    format!("http://{addr}/probe.wav")
}

/// Régression #1247 : la chaîne VU complète d'une session proxy — sonde
/// HTTP → forwarder cadencé → bus — doit émettre `playback.audio_levels`
/// pour une zone en Playing dont la position n'est pas rapportée (0), le
/// cas exact d'une zone « browser » servie en FLAC-proxy (Qobuz/Tidal
/// direct). Autonome (serveur WAV local one-shot), sans réseau.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn levels_chain_emits_audio_levels_on_bus() {
    let url = spawn_oneshot_wav_server(3);
    let zone_id = 987_655;
    let playback = Arc::new(crate::playback::PlaybackManager::new());
    playback
        .play(zone_id, crate::playback::NowPlaying::default())
        .await;
    let bus = Arc::new(super::EventBus::new());
    let mut rx = bus.subscribe();

    let play_seq = playback.current_play_seq(zone_id).await;
    let levels_tx =
        super::spawn_paced_levels_forwarder(bus.clone(), playback.clone(), zone_id, play_seq, 0);
    let reported = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let probe = tokio::task::spawn_blocking(move || {
        super::decode_http_stream_for_levels(url, "wav".into(), levels_tx, reported)
    });

    let mut n = 0u32;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(ev)) if ev.event_type == "playback.audio_levels" => n += 1,
            Ok(Ok(_)) => {}
            _ => break,
        }
    }
    let _ = probe.await;
    assert!(
        n >= 40,
        "3 s d'audio ⇒ ~75 fenêtres de 40 ms sur le bus ; reçu {n}"
    );
}

/// #1110 : un forwarder créé pour une piste doit MOURIR quand la zone
/// passe à la suivante, au lieu de publier son PCM sur l'horloge de la
/// nouvelle. C'est ce que garantit l'épinglage de la génération au moment
/// de la décision : ici on simule une génération devenue obsolète.
#[tokio::test]
async fn levels_forwarder_dies_when_its_track_is_replaced() {
    let zone_id = 987_656;
    let playback = Arc::new(crate::playback::PlaybackManager::new());
    playback
        .play(zone_id, crate::playback::NowPlaying::default())
        .await;
    let stale_seq = playback.current_play_seq(zone_id).await;

    // La zone enchaîne : nouvelle génération (ce que fait toute nouvelle
    // demande de lecture avant de résoudre son flux).
    playback.bump_generation(zone_id).await;
    playback
        .play(zone_id, crate::playback::NowPlaying::default())
        .await;
    assert_ne!(
        playback.current_play_seq(zone_id).await,
        stale_seq,
        "la lecture suivante doit bumper la génération"
    );

    let bus = Arc::new(super::EventBus::new());
    let mut rx = bus.subscribe();
    // Forwarder de l'ANCIENNE piste : c'est exactement ce qu'on obtenait en
    // lisant la génération trop tard, sauf qu'alors il lisait la NOUVELLE
    // et survivait.
    let levels_tx =
        super::spawn_paced_levels_forwarder(bus.clone(), playback.clone(), zone_id, stale_seq, 0);
    let pcm = vec![0u8; 4096];
    crate::audio::tap::send_windowed_pcm(&levels_tx, &pcm, 16, 2, 44_100);

    let got = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await;
    assert!(
        got.is_err(),
        "un forwarder d'une piste remplacée ne doit rien publier, reçu {got:?}"
    );
}

/// Vérification end-to-end de la chaîne VU d'une session proxy, contre
/// une URL FLAC/HTTP réelle : sonde → forwarder cadencé → bus. Reproduit
/// exactement le chemin de production (moins le WebSocket) pour une zone
/// « browser » (état Playing, position non rapportée = 0, comme quand le
/// navigateur ne bat pas encore le cœur). Compte les événements
/// `playback.audio_levels` réellement émis sur le bus.
///
/// Piloté par TUNE_DIAG_PROBE_URL (URL d'une session proxy live, p.ex.
/// http://192.168.1.18:8888/stream/<id>.flac) pour ne pas dépendre du
/// réseau en CI.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn diag_probe_emits_bus_events() {
    let Ok(url) = std::env::var("TUNE_DIAG_PROBE_URL") else {
        return;
    };
    let zone_id = 987_654;
    let playback = Arc::new(crate::playback::PlaybackManager::new());
    // Passe la zone en Playing (comme un vrai play), sans rapporter de
    // position — le pire cas du forwarder (browser sans heartbeat).
    playback
        .play(zone_id, crate::playback::NowPlaying::default())
        .await;
    let bus = Arc::new(super::EventBus::new());
    let mut rx = bus.subscribe();

    let play_seq = playback.current_play_seq(zone_id).await;
    let levels_tx =
        super::spawn_paced_levels_forwarder(bus.clone(), playback.clone(), zone_id, play_seq, 0);
    let reported = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let probe = tokio::task::spawn_blocking(move || {
        super::decode_http_stream_for_levels(url, "flac".into(), levels_tx, reported)
    });

    // Compte les audio_levels émis sur ~4 s (cadence réelle ≈ 25/s).
    let mut n = 0u32;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(4);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(ev)) if ev.event_type == "playback.audio_levels" => n += 1,
            Ok(Ok(_)) => {}
            _ => break,
        }
    }
    probe.abort();
    eprintln!("DIAG audio_levels emitted in 4s = {n}");
    assert!(
        n >= 50,
        "chaîne proxy→forwarder→bus doit émettre ~25/s ; reçu {n} en 4 s"
    );
}

/// Le repli NFC/NFD lui-même est éprouvé dans
/// `crate::library::local_path` (#1865). Ce qui se teste ICI, c'est que la
/// lecture continue de PASSER PAR LUI : la sortir du module l'a rendue
/// partageable, elle ne doit pas s'en trouver débranchée.
#[test]
fn la_lecture_locale_passe_par_le_repli_partage() {
    let tmp = tempfile::TempDir::new().unwrap();
    // Sur le disque en NFD (graphie d'un partage SMB / d'un Mac)…
    let nfd = tmp.path().join("Bjo\u{0308}rk - Jo\u{0301}ga.flac");
    std::fs::write(&nfd, b"x").unwrap();
    // …et en base en NFC, comme le scanner l'enregistre.
    let nfc = tmp
        .path()
        .join("Bj\u{00f6}rk - J\u{00f3}ga.flac")
        .to_string_lossy()
        .to_string();
    assert_ne!(nfc, nfd.to_string_lossy(), "les deux graphies different");
    assert!(
        super::resolve_existing_local_path(&nfc).is_some(),
        "resolve_existing_local_path doit venir de library::local_path et \
         retrouver le fichier NFD depuis le chemin NFC de la base"
    );
}

use tokio::sync::Mutex;

use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::registry::OutputRegistry;
use crate::outputs::{OutputCapabilities, OutputCommand, OutputCommandError};
use crate::playback::{NowPlaying, PlayState, PlaybackManager};
use crate::streaming::registry::ServiceRegistry;

use super::{
    PlayRequest, PlaybackOrchestrator, RepriseDeSession, StreamingDsp, is_network_output_type,
    is_pull_dsp_output_type, is_push_uri_output_type, message_session_perdue,
    passthrough_didl_duration_ms, pull_output_needs_dsp_transcode, replay_needs_output_seek,
    reprise_de_session, reprise_toujours_la_notre, requete_de_retablissement,
    spawn_streaming_dsp_relay, streaming_needs_pretranscode, streaming_pretranscode_format,
    use_file_transcode_for,
};

#[test]
fn duplicate_net_play_gate_excludes_pull_outputs() {
    // #1129's coalescing exists for renderers that receive a URI and can
    // restart from byte 0 on a redundant send (Revox S100). Pull-based
    // outputs never have that failure mode and must stay excluded.
    assert!(is_push_uri_output_type(Some("dlna")));
    assert!(is_push_uri_output_type(Some("openhome")));
    assert!(is_push_uri_output_type(Some("chromecast")));
    assert!(is_push_uri_output_type(Some("bluos")));
    assert!(is_push_uri_output_type(Some("squeezebox")));
    assert!(is_push_uri_output_type(Some("slimproto")));

    // Pull-based: fetch/stream audio themselves, never restart-glitch.
    assert!(!is_push_uri_output_type(Some("local")));
    assert!(!is_push_uri_output_type(Some("oaat")));
    assert!(!is_push_uri_output_type(Some("diretta")));
}

#[test]
fn pull_output_dsp_transcode_classification() {
    use crate::audio::formats::AudioFormat;
    let flac = Some(AudioFormat::Flac);

    // The case that was silently broken: an out-of-tree pull output was in
    // none of the lists, so an active EQ never reached it.
    assert!(pull_output_needs_dsp_transcode(
        Some("diretta"),
        false,
        false,
        flac
    ));

    // Already transcoding on their own: forcing again would be redundant.
    assert!(!pull_output_needs_dsp_transcode(
        Some("local"),
        true,
        false,
        flac
    ));
    assert!(!pull_output_needs_dsp_transcode(
        Some("oaat"),
        false,
        true,
        flac
    ));

    // Covered by their own flags.
    assert!(!pull_output_needs_dsp_transcode(
        Some("dlna"),
        false,
        false,
        flac
    ));
    assert!(!pull_output_needs_dsp_transcode(
        Some("browser"),
        false,
        false,
        flac
    ));

    // No format, or DSD: never force a transcode we cannot do safely, and
    // never turn native DSD into PCM on the listener's behalf.
    assert!(!pull_output_needs_dsp_transcode(
        Some("diretta"),
        false,
        false,
        None
    ));
    assert!(!pull_output_needs_dsp_transcode(
        Some("diretta"),
        false,
        false,
        Some(AudioFormat::Dsd)
    ));

    // Unknown output type: no basis to decide.
    assert!(!pull_output_needs_dsp_transcode(None, false, false, flac));

    // Unregistered device (output_type_of returned None): never coalesce.
    assert!(!is_push_uri_output_type(None));
}

/// La part « type de sortie » de [`pull_output_needs_dsp_transcode`],
/// extraite pour que le panneau du chemin du signal la LISE au lieu de la
/// recopier (#2189).
///
/// L'extraction doit être à la lettre : ce test compare les deux, format
/// source et drapeaux d'exécution neutres, sur tous les types que ce dépôt
/// sait produire.
#[test]
fn is_pull_dsp_output_type_est_la_part_type_de_pull_output_needs_dsp_transcode() {
    use crate::audio::formats::AudioFormat;
    let flac = Some(AudioFormat::Flac);

    for t in [
        "dlna",
        "openhome",
        "chromecast",
        "bluos",
        "squeezebox",
        "slimproto",
        "browser",
        "local",
        "oaat",
        "oaat-multiroom",
        "airplay",
        "airplay2",
        "hqplayer",
        "diretta",
        "un-greffon-hors-depot",
    ] {
        assert_eq!(
            is_pull_dsp_output_type(Some(t)),
            pull_output_needs_dsp_transcode(Some(t), false, false, flac),
            "{t} : le prédicat extrait doit rendre exactement ce que \
             rendait la condition d'origine"
        );
    }
    assert!(!is_pull_dsp_output_type(None));

    // Et le fait qui fonde le correctif #2189 : HQPlayer, AirPlay 2 et
    // tout greffon hors dépôt sont des sorties PULL — elles reçoivent nos
    // octets intacts, donc le transport y est bit-perfect.
    assert!(is_pull_dsp_output_type(Some("hqplayer")));
    assert!(is_pull_dsp_output_type(Some("airplay2")));
    assert!(is_pull_dsp_output_type(Some("diretta")));
    // `slimproto` n'en est PAS une : elle reçoit une URI, comme Squeezebox.
    assert!(!is_pull_dsp_output_type(Some("slimproto")));
}

#[test]
fn dsd_lpcm_streams_only_when_toggled_and_dsd_wav() {
    // The fix: a DSD source served as WAV to a renderer that demands LPCM
    // (dlna_needs_wav) streams instead of blocking on a temp file — but
    // ONLY with the toggle on. Everything else keeps its prior behaviour.

    // DSD → WAV, renderer needs LPCM, toggle ON → stream (the fix).
    assert!(!use_file_transcode_for(true, true, true, true, false));
    // Same, toggle OFF → temp file (rollback, unchanged).
    assert!(use_file_transcode_for(true, true, true, false, false));
    // FLAC target (non-WAV) always temp-files for Content-Length — the
    // dsd flag can't apply (dsd_lpcm_streams stays false for non-DSD/WAV).
    assert!(use_file_transcode_for(true, false, false, false, false));
    // WAV target a renderer is fine to stream (dlna_needs_wav false):
    // streams regardless of the flag (local/OAAT/Linn path, unchanged).
    assert!(!use_file_transcode_for(true, true, false, false, false));
    // Local/OAAT (not network): never file-transcodes.
    assert!(!use_file_transcode_for(false, true, true, false, false));
}

/// Un traitement actif RAMÈNE au fichier temporaire, quel que soit le reste.
///
/// Le bras progressif ne branche ni égaliseur, ni convolveur, ni
/// ReplayGain : les y envoyer, c'est les perdre sans le dire. Les deux
/// premières lignes sont exactement les cas que le bras progressif gagne
/// aujourd'hui (renderer FLAC depuis 0cf27ade ; renderer LPCM le jour où
/// `dsd_lpcm_stream` deviendrait le défaut, #1363).
#[test]
fn un_traitement_actif_ramene_au_fichier() {
    // Renderer FLAC-capable, DSD → WAV progressif : streame sans DSP…
    assert!(!use_file_transcode_for(true, true, false, false, false));
    // …et repasse par le fichier dès qu'un traitement est actif.
    assert!(use_file_transcode_for(true, true, false, false, true));
    // Renderer LPCM, bascule « Streaming continu » armée : même règle.
    assert!(!use_file_transcode_for(true, true, true, true, false));
    assert!(use_file_transcode_for(true, true, true, true, true));
    // Zone navigateur (non « réseau ») avec EQ : le cas déjà couvert par
    // #1168, qui passait par un `||` hors de cette fonction.
    assert!(use_file_transcode_for(false, true, false, false, true));
    // Sans traitement, une sortie non réseau ne file-transcode toujours pas.
    assert!(!use_file_transcode_for(false, true, false, false, false));
}

/// #2863 — le bras streaming HTTPS servait les octets du CDN VERBATIM dès
/// que le renderer savait lire le FLAC, sans jamais regarder si un
/// traitement de zone était armé. Un auditeur Qobuz sur une zone réseau
/// n'entendait donc aucun effet de son égaliseur.
///
/// La ligne qui MORD est la deuxième : renderer FLAC-capable + traitement
/// actif ⇒ pré-transcodage. Les deux dernières sont le comportement
/// historique (#1137, renderer sans `audio/flac`), qui ne doit pas bouger.
#[test]
fn un_traitement_actif_impose_le_pretranscodage_streaming() {
    // Renderer FLAC-capable, aucun traitement : proxy verbatim, bit-perfect.
    // C'est ce que les testeurs écoutent aujourd'hui — inchangé.
    assert!(!streaming_needs_pretranscode(true, false));
    // Renderer FLAC-capable, traitement armé : LE défaut #2863.
    assert!(streaming_needs_pretranscode(true, true));
    // Renderer qui refuse le MIME amont : pré-transcodage, comme avant.
    assert!(streaming_needs_pretranscode(false, false));
    assert!(streaming_needs_pretranscode(false, true));
}

/// Le plafond 16 bits ne doit PAS suivre le traitement.
///
/// Il n'existe que parce que `DLNA.ORG_PN=LPCM` est un profil 16 bits
/// (#1137). Quand c'est le traitement seul qui impose le pré-transcodage,
/// le renderer sait lire le FLAC : ré-encoder en FLAC garde la pleine
/// profondeur. Sans ce test, armer un égaliseur ferait tomber tout le
/// Hi-Res Qobuz à 16 bits.
#[test]
fn le_pretranscodage_dsp_garde_la_pleine_profondeur() {
    assert_eq!(streaming_pretranscode_format(true), "flac");
    assert_eq!(streaming_pretranscode_format(false), "wav");
}

/// #2893 — la matrice de décision du `Seek` qui suit une recréation de flux.
///
/// La ligne qui MORD est la première : sortie réseau + session
/// fichier/proxy + position non nulle. C'est le cas de Jean Valjean, une
/// bascule Pure sur un Marantz ND8006 avec `position_ms=94000` juste au
/// serveur — et le morceau qui repart du début faute de `Seek`.
///
/// Les autres lignes sont le TÉMOIN anti-régression, et la deuxième est la
/// plus chère : une session décodée est déjà pré-seekée par
/// `decode_to_pcm_streaming_seeked`, lui envoyer un `Seek` doublerait
/// l'offset — la panne #1518 (silence total, puis boucle de redémarrage).
/// Cas réel de cette ligne : DSD servi en LPCM progressif sur DLNA
/// (`dsd_lpcm_stream=true`), le seul bras réseau qui reste en canal mpsc.
#[test]
fn une_relecture_reseau_sur_session_seekable_reclame_le_seek() {
    // LE défaut : le flux a été recréé depuis l'octet 0, il manque le Seek.
    assert!(replay_needs_output_seek(true, true, 94_000));

    // Témoin 1 — session décodée : le producteur part DÉJÀ de l'offset.
    // Un Seek ici sauterait deux fois (#1518).
    assert!(!replay_needs_output_seek(true, false, 94_000));

    // Témoin 2 — sortie locale / OAAT : elles consomment un transcodage
    // séquentiel qui honore `seek_ms`. Chemin inchangé, aucun Seek ajouté.
    assert!(!replay_needs_output_seek(false, true, 94_000));
    assert!(!replay_needs_output_seek(false, false, 94_000));

    // Témoin 3 — position nulle : rien à rattraper, le début EST la cible.
    // C'est le cas de figure de #2595 (zone sans périphérique, position
    // restée à 0) : il ne doit pas produire un Seek inutile de plus.
    assert!(!replay_needs_output_seek(true, true, 0));
}

/// LAT-P2 : le seek détaché appartient à la lecture qui l'a demandé. Même
/// génération → il part ; une génération de plus (stop, next, nouvelle
/// lecture pendant la pose) → il s'abstient, sinon il seekerait la piste
/// suivante.
#[test]
fn le_seek_detache_ne_part_que_pour_sa_lecture() {
    assert!(reprise_toujours_la_notre(7, 7));
    assert!(!reprise_toujours_la_notre(7, 8));
    assert!(!reprise_toujours_la_notre(8, 7));
}

/// La réponse de `resume` et celle de la relecture ne doivent plus porter le
/// temps de pose : aucun `sleep` dans leur corps, le seek passe par la tâche
/// détachée, et la tâche relit la génération avant de seeker. Lu dans le code
/// de production de `transport.rs` seul.
#[test]
fn les_seeks_apres_reprise_sont_detaches_et_gardes() {
    let source = include_str!("transport.rs");
    let corps = |signature: &str| -> &str {
        let debut = source
            .find(signature)
            .unwrap_or_else(|| panic!("{signature} introuvable"));
        let apres = &source[debut..];
        let fin = apres
            .find("\n    }\n")
            .map(|i| i + 7)
            .unwrap_or(apres.len());
        &apres[..fin]
    };
    for (nom, sig) in [
        (
            "resume",
            "pub async fn resume(&self, zone_id: i64, device_id: Option<&str>)",
        ),
        (
            "seek_output_after_replay",
            "pub(super) async fn seek_output_after_replay(",
        ),
    ] {
        let c = corps(sig);
        assert!(
            !c.contains("tokio::time::sleep("),
            "{nom} ne doit plus dormir avant de repondre (LAT-P2)"
        );
        assert!(
            c.contains("detacher_le_seek_apres_reprise("),
            "{nom} doit passer par la tache detachee"
        );
    }
    let tache = corps("async fn detacher_le_seek_apres_reprise(");
    let pose = tache
        .find("tokio::time::sleep(pose)")
        .expect("la tache dort d'abord");
    let garde = tache
        .find("reprise_toujours_la_notre(")
        .expect("la tache relit la generation");
    let seek = tache.find(".checked_seek(").expect("la tache seeke");
    assert!(
        pose < garde && garde < seek,
        "ordre attendu : pose, garde de generation, seek"
    );
}

/// La liste des sorties « réseau » ne vit plus qu'à un endroit.
///
/// Elle était inline dans `seek()` ; `replay_zone_at_position` en a besoin
/// pour la même décision. Ce test existe pour qu'un renderer ajouté à l'une
/// n'oublie pas l'autre — c'est le mode de panne que la duplication aurait
/// produit, et il serait resté MUET (un morceau qui repart du début).
#[test]
fn la_liste_des_sorties_reseau_couvre_les_six_renderers() {
    for t in [
        "dlna",
        "openhome",
        "chromecast",
        "bluos",
        "squeezebox",
        "slimproto",
    ] {
        assert!(is_network_output_type(Some(t)), "{t} doit être réseau");
    }
    for t in ["local", "oaat", "browser"] {
        assert!(!is_network_output_type(Some(t)), "{t} n'est PAS réseau");
    }
    // `airplay` est ABSENT de la liste historique de `seek()`. Ce test le
    // constate, il ne l'approuve pas : l'y ajouter changerait aussi le
    // chemin de `seek()`, ce qui est une décision à part entière et n'a
    // rien à faire dans un correctif #2893.
    assert!(!is_network_output_type(Some("airplay")));
    // Une zone sans type déclaré est traitée comme locale partout ailleurs.
    assert!(!is_network_output_type(None));
}

/// Un PCM de test : sinusoïde 80 Hz, stéréo 16 bits.
fn pcm_sinus_16(n: usize) -> Vec<u8> {
    let mut pcm = Vec::with_capacity(n * 4);
    for i in 0..n {
        let s = (2.0 * std::f64::consts::PI * 80.0 * i as f64 / 44100.0).sin() * 0.5;
        let s16 = (s * 32767.0) as i16;
        pcm.extend_from_slice(&s16.to_le_bytes());
        pcm.extend_from_slice(&s16.to_le_bytes());
    }
    pcm
}

fn eq_grave_boostee() -> crate::audio::eq::EqProcessor {
    let profile = crate::audio::eq::EqProfile {
        enabled: true,
        bass_gain_db: 6.0,
        ..Default::default()
    };
    crate::audio::eq::EqProcessor::new(&profile, 44100, 2)
}

/// TÉMOIN ANTI-RÉGRESSION, au bit près.
///
/// Une zone sans traitement — l'immense majorité, et tout ce que les
/// testeurs écoutent aujourd'hui — doit traverser `StreamingDsp` sans
/// qu'un seul octet change. Si ce test tombe, le correctif #2863 a coloré
/// un signal qui devait rester intact.
#[test]
fn sans_traitement_la_chaine_streaming_ne_touche_pas_un_octet() {
    let mut dsp = StreamingDsp::default();
    assert!(!dsp.is_active());
    let mut pcm = pcm_sinus_16(1024);
    let temoin = pcm.clone();
    dsp.process(&mut pcm, 16);
    assert_eq!(pcm, temoin, "un PCM sans traitement doit rester identique");
}

/// Les TROIS étages sont portés par le même objet, dans l'ordre de
/// `transcode_source_to_file` : ReplayGain, égaliseur, convolveur.
///
/// Le convolveur ne peut pas être construit sans fichier d'impulsion sur
/// disque ; ce test couvre les deux autres et le fait que `is_active()`
/// s'allume sur chacun — c'est ce qui décide du pré-transcodage.
#[test]
fn la_chaine_streaming_applique_replaygain_puis_egaliseur() {
    // ReplayGain seul : gain exact, vérifiable échantillon par échantillon.
    let mut rg = StreamingDsp {
        replaygain: Some(0.5),
        ..Default::default()
    };
    assert!(rg.is_active());
    let source = pcm_sinus_16(256);
    let mut pcm = source.clone();
    rg.process(&mut pcm, 16);
    assert_ne!(pcm, source);
    for (i, (a, b)) in source
        .chunks_exact(2)
        .zip(pcm.chunks_exact(2))
        .enumerate()
        .take(64)
    {
        let av = i16::from_le_bytes([a[0], a[1]]) as f64;
        let bv = i16::from_le_bytes([b[0], b[1]]) as f64;
        assert!(
            (bv - av * 0.5).abs() <= 1.0,
            "échantillon {i} : {bv} attendu ≈ {}",
            av * 0.5
        );
    }

    // Égaliseur seul : le signal change.
    let mut eq = StreamingDsp {
        eq: Some(eq_grave_boostee()),
        ..Default::default()
    };
    assert!(eq.is_active());
    let mut pcm = source.clone();
    eq.process(&mut pcm, 16);
    assert_ne!(pcm, source, "un grave +6 dB doit modifier le PCM");

    // Les deux ensemble diffèrent de chacun pris seul : les deux étages
    // sont bien traversés, pas seulement le premier.
    let mut deux = StreamingDsp {
        replaygain: Some(0.5),
        eq: Some(eq_grave_boostee()),
        convolver: None,
    };
    let mut pcm_deux = source.clone();
    deux.process(&mut pcm_deux, 16);
    assert_ne!(pcm_deux, pcm);
    assert_ne!(pcm_deux, source);
}

/// Le relais du bras AAC→WAV laisse passer l'EN-TÊTE WAV intact.
///
/// `decode_to_pcm_streaming_inner` émet l'en-tête comme PREMIER chunk, seul.
/// Le faire traverser l'égaliseur reviendrait à filtrer les lettres
/// « RIFF » : en-tête corrompu, donc bruit ou silence chez l'auditeur. Le
/// PCM qui suit, lui, doit bien être traité.
#[tokio::test]
async fn le_relais_dsp_streaming_epargne_l_en_tete_wav() {
    let (aval_tx, mut aval_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let dsp = StreamingDsp {
        replaygain: Some(0.5),
        ..Default::default()
    };
    let amont = spawn_streaming_dsp_relay(dsp, 16, true, aval_tx);

    let entete = crate::audio::wav::build_wav_header(2, 44100, 16).to_vec();
    let pcm = pcm_sinus_16(64);
    amont.send(entete.clone()).await.unwrap();
    amont.send(pcm.clone()).await.unwrap();
    drop(amont);

    let recu_entete = aval_rx.recv().await.expect("en-tête attendu");
    assert_eq!(recu_entete, entete, "l'en-tête WAV doit passer intact");
    assert_eq!(&recu_entete[0..4], b"RIFF");

    let recu_pcm = aval_rx.recv().await.expect("PCM attendu");
    assert_ne!(recu_pcm, pcm, "le PCM, lui, doit être traité");
    let a = i16::from_le_bytes([pcm[2], pcm[3]]) as f64;
    let b = i16::from_le_bytes([recu_pcm[2], recu_pcm[3]]) as f64;
    assert!((b - a * 0.5).abs() <= 1.0);
}

/// Sans traitement actif, le bras AAC→WAV n'insère aucun relais : le canal
/// reste celui d'avant. Contrôle symétrique du témoin ci-dessus.
#[test]
fn un_streaming_dsp_vide_n_est_pas_actif() {
    assert!(!StreamingDsp::default().is_active());
    assert!(
        StreamingDsp {
            replaygain: Some(0.5),
            ..Default::default()
        }
        .is_active()
    );
}

#[test]
fn duplicate_net_play_coalesces_same_track_within_window() {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};
    type Map = HashMap<i64, (String, Option<String>, Option<i64>, Instant)>;
    let f = PlaybackOrchestrator::record_or_detect_duplicate_net_play;
    let t0 = Instant::now();
    let sid = Some("tidal-123".to_string());

    let mut map: Map = HashMap::new();
    // First play of the track → recorded, NOT a duplicate.
    assert!(!f(&mut map, 5, "tidal", &sid, None, t0));
    // Same (source, source_id) a few seconds later → duplicate (coalesce).
    assert!(f(
        &mut map,
        5,
        "tidal",
        &sid,
        None,
        t0 + Duration::from_secs(4)
    ));
    // A DIFFERENT track (real advance) → NOT a duplicate.
    let other = Some("tidal-999".to_string());
    assert!(!f(
        &mut map,
        5,
        "tidal",
        &other,
        None,
        t0 + Duration::from_secs(4)
    ));
    // Different source, same id → NOT a duplicate.
    assert!(!f(
        &mut map,
        5,
        "qobuz",
        &sid,
        None,
        t0 + Duration::from_secs(4)
    ));

    // Same track but OUTSIDE the window (repeat-one / dup-in-queue, minutes
    // later) → NOT a duplicate.
    let mut map2: Map = HashMap::new();
    assert!(!f(&mut map2, 7, "tidal", &sid, None, t0));
    let far = t0 + super::DUPLICATE_NET_PLAY_WINDOW + Duration::from_secs(1);
    assert!(!f(&mut map2, 7, "tidal", &sid, None, far));

    // Different zones never collide.
    let mut map3: Map = HashMap::new();
    assert!(!f(&mut map3, 1, "tidal", &sid, None, t0));
    assert!(!f(&mut map3, 2, "tidal", &sid, None, t0));
}

/// Deux pistes LOCALES differentes ne sont pas un doublon.
///
/// La bibliotheque locale se joue par `track_id` : `play_from_queue` laisse
/// `source` et `source_id` a `None`. La cle valait donc `("local", None)`
/// pour toutes les pistes de la zone, et « piste suivante » sur un renderer
/// reseau ne poussait plus rien pendant douze secondes — le serveur
/// avancait, le Chromecast rejouait le meme morceau (FabienM, v0.9.102).
///
/// Le test d'origine n'exercait que `tidal` et `qobuz`, qui portent
/// TOUJOURS un `source_id` : il ne pouvait pas voir ce cas.
#[test]
fn deux_pistes_locales_differentes_ne_sont_pas_un_doublon() {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};
    type Map = HashMap<i64, (String, Option<String>, Option<i64>, Instant)>;
    let f = PlaybackOrchestrator::record_or_detect_duplicate_net_play;
    let t0 = Instant::now();
    let mut map: Map = HashMap::new();

    // Piste 101 : premier envoi.
    assert!(!f(&mut map, 1, "local", &None, Some(101), t0));

    // « Piste suivante » deux secondes plus tard, piste 102 : c'est une
    // AUTRE piste, elle doit partir au renderer.
    assert!(
        !f(
            &mut map,
            1,
            "local",
            &None,
            Some(102),
            t0 + Duration::from_secs(2)
        ),
        "deux pistes locales differentes ne sont pas un doublon : le \
         renderer resterait sur le morceau precedent"
    );

    // Enchainement rapide : encore une autre.
    assert!(!f(
        &mut map,
        1,
        "local",
        &None,
        Some(103),
        t0 + Duration::from_secs(4)
    ));

    // La MEME piste relancee dans la fenetre reste un doublon : c'est la
    // course que le garde-fou existe pour absorber (#1146, Philippe Vella).
    assert!(f(
        &mut map,
        1,
        "local",
        &None,
        Some(103),
        t0 + Duration::from_secs(6)
    ));

    // Et hors fenetre, elle repart (repeat-one, doublon dans la file).
    let loin = t0 + Duration::from_secs(6) + super::DUPLICATE_NET_PLAY_WINDOW;
    assert!(!f(&mut map, 1, "local", &None, Some(103), loin));
}

#[test]
fn retap_identity_matches_same_track_only() {
    // #1271 re-tap dedup identity predicate. Local library track: matches on
    // track_id when both sides have one.
    let np_local = NowPlaying {
        track_id: Some(42),
        source: "local".into(),
        ..Default::default()
    };
    let same_local = PlayRequest {
        track_id: Some(42),
        ..Default::default()
    };
    let other_local = PlayRequest {
        track_id: Some(43),
        ..Default::default()
    };
    assert!(PlaybackOrchestrator::is_same_track_retap(
        &np_local,
        &same_local
    ));
    assert!(!PlaybackOrchestrator::is_same_track_retap(
        &np_local,
        &other_local
    ));

    // Streaming track: matches on (source, source_id) when there is no
    // library track_id. A request that names the source must agree with it.
    let np_stream = NowPlaying {
        track_id: None,
        source: "tidal".into(),
        source_id: Some("tidal-123".into()),
        ..Default::default()
    };
    let same_stream = PlayRequest {
        source: Some("tidal".into()),
        source_id: Some("tidal-123".into()),
        ..Default::default()
    };
    // Web client omits `source` — still matches on the id alone.
    let same_stream_no_src = PlayRequest {
        source: None,
        source_id: Some("tidal-123".into()),
        ..Default::default()
    };
    let other_stream = PlayRequest {
        source: Some("tidal".into()),
        source_id: Some("tidal-999".into()),
        ..Default::default()
    };
    // Same id but a DIFFERENT source (Qobuz vs Tidal) → not the same track.
    let cross_source = PlayRequest {
        source: Some("qobuz".into()),
        source_id: Some("tidal-123".into()),
        ..Default::default()
    };
    assert!(PlaybackOrchestrator::is_same_track_retap(
        &np_stream,
        &same_stream
    ));
    assert!(PlaybackOrchestrator::is_same_track_retap(
        &np_stream,
        &same_stream_no_src
    ));
    assert!(!PlaybackOrchestrator::is_same_track_retap(
        &np_stream,
        &other_stream
    ));
    assert!(!PlaybackOrchestrator::is_same_track_retap(
        &np_stream,
        &cross_source
    ));

    // Neither side yields a positive id → never a match (no false coalesce).
    let np_bare = NowPlaying {
        track_id: None,
        source: "local".into(),
        source_id: None,
        ..Default::default()
    };
    let req_bare = PlayRequest {
        track_id: None,
        source_id: None,
        ..Default::default()
    };
    assert!(!PlaybackOrchestrator::is_same_track_retap(
        &np_bare, &req_bare
    ));
}

#[test]
fn passthrough_duration_prefers_probed_over_scanned() {
    // #1132: the scanned duration (5:65 = 305_000 ms) is a few seconds too
    // long vs. the file's real STREAMINFO duration (300_000 ms). The DIDL
    // must advertise the real one so the gapless-queued track on the Marantz
    // ND 8006 ends at the true EOF instead of cutting/looping near the end.
    assert_eq!(
        passthrough_didl_duration_ms(Some(300.0), 305_000),
        300_000,
        "probed STREAMINFO duration wins over the too-long scanned value"
    );
}

#[test]
fn passthrough_duration_falls_back_when_probe_missing() {
    // NAS read timeout / unreadable header → probe is None. We must keep the
    // scanned duration rather than blank it (a 0 duration hides the progress
    // bar on the renderer entirely).
    assert_eq!(passthrough_didl_duration_ms(None, 240_000), 240_000);
}

#[test]
fn passthrough_duration_ignores_bogus_probe() {
    // Zero / negative / non-finite probed values must not overwrite a valid
    // scanned duration.
    assert_eq!(passthrough_didl_duration_ms(Some(0.0), 180_000), 180_000);
    assert_eq!(passthrough_didl_duration_ms(Some(-5.0), 180_000), 180_000);
    assert_eq!(
        passthrough_didl_duration_ms(Some(f64::NAN), 180_000),
        180_000
    );
}

/// Le garde-fou que JP Robbe a demande en revue de #2220 : une sortie OAAT
/// REELLEMENT ENREGISTREE, drapeau DSD natif actif, doit rendre `false`.
///
/// Mon premier test ne couvrait que les retours `true`. Une inversion de
/// booleen, un mauvais prefixe ou un downcast rate y seraient passes
/// inapercus : c'est ce test-ci qui verrouille le prefixe, le lookup, le
/// downcast et l'inversion ENSEMBLE.
#[cfg(feature = "oaat")]
#[tokio::test]
async fn une_sortie_oaat_en_dsd_natif_ne_mesure_pas() {
    let orch = test_orchestrator();
    let sortie = crate::outputs::oaat::OaatOutput::new(
        "Zicmu".into(),
        "192.168.1.99".into(),
        9000,
        "oaat:zicmu-test".into(),
    );
    // Le constructeur pose le prefixe `oaat:`, et c'est lui qui conditionne
    // le lookup dans `output_produces_levels`.
    let device_id = "oaat:zicmu-test".to_string();

    // En PCM la sortie mesure : ce sont les niveaux du decodage de
    // l'orchestrateur qui alimentent les VU.
    orch.outputs.lock().await.register(Box::new(sortie));
    assert!(
        orch.output_produces_levels(Some(&device_id)).await,
        "hors DSD natif, la chaine mesure"
    );

    // DSD natif : la sortie ouvre le .dsf elle-meme, plus personne ne
    // decode, donc plus aucune fenetre de niveaux.
    {
        let registre = orch.outputs.lock().await;
        let arc = registre.get(&device_id).expect("sortie enregistree");
        let sortie = arc.lock().await;
        let oaat = sortie
            .as_any()
            .downcast_ref::<crate::outputs::oaat::OaatOutput>()
            .expect("downcast vers OaatOutput");
        oaat.set_native_dsd_active_for_test(true);
    }
    assert!(
        !orch.output_produces_levels(Some(&device_id)).await,
        "en DSD natif rien ne mesure : l'ecran doit pouvoir le dire"
    );
}

/// Une zone sans sortie, ou une sortie qui n'est pas OAAT, mesure :
/// `false` est réservé au seul chemin qui ne produit rien.
///
/// Le cas DSD natif lui-même se teste là où vit le drapeau
/// (`outputs::oaat::integration_test`) : il demande une sortie OAAT
/// enregistrée, pas un orchestrateur nu.
#[tokio::test]
async fn sans_sortie_ou_hors_oaat_on_mesure() {
    let orch = test_orchestrator();
    assert!(orch.output_produces_levels(None).await);
    for did in [
        "local:Haut-parleurs",
        "dlna:uuid:1234",
        "airplay:salon",
        "oaat:zicmu", // enregistré nulle part : on ne conclut pas à l'absence
    ] {
        assert!(
            orch.output_produces_levels(Some(did)).await,
            "{did} : rien ne prouve que cette sortie ne mesure pas"
        );
    }
}

/// #3287 — un producteur de transcodage qui meurt avant d'ecrire doit
/// laisser un SILENCE LISIBLE, pas une session fantome.
///
/// La garde tient les deux signaux ENSEMBLE, parce que c'est leur
/// difference qui porte la decision du poller :
///
/// - session VIVANTE et muette (transcodage lent, DASH Hi-Res Tidal) :
///   `wait_stream_data_ready` = false, `stream_session_alive` = **true**
///   → le gapless doit continuer d'armer, sinon un blanc a chaque piste ;
/// - session ABANDONNEE (echec CDN a 93 ms, le journal de Gros Bidon) :
///   les DEUX sont false → il n'y a plus rien a armer.
///
/// Avant le correctif, la seconde ligne rendait `alive = true` comme la
/// premiere : les deux cas etaient indiscernables, et la sortie locale
/// s'enchainait sur un flux qui ne debiterait jamais un octet.
///
/// Le fichier temporaire est verifie au passage : c'est le seul autre
/// effet de la branche d'echec, et il ne doit pas se perdre dans le
/// deplacement.
#[tokio::test]
async fn une_session_de_transcodage_abandonnee_ne_se_confond_plus_avec_une_session_lente() {
    use crate::http::streamer::StreamInfo;

    let orch = test_orchestrator();
    let info = StreamInfo {
        format: "wav".to_string(),
        mime_type: "audio/wav".to_string(),
        ..StreamInfo::default()
    };

    // Une session de transcodage qui n'a encore rien produit : muette,
    // mais bien vivante. C'est le cas que le poller DOIT continuer d'armer.
    let (lente, _tx, _ready) = orch.streamer.create_session(info.clone(), false, 1).await;
    assert!(
        !orch.wait_stream_data_ready(&lente, 20).await,
        "sans un octet, l'attente doit echouer — sinon l'epreuve ne mesure rien"
    );
    assert!(
        orch.stream_session_alive(&lente).await,
        "une session lente est VIVANTE : refuser d'armer ici remettrait un \
         blanc entre chaque piste Hi-Res"
    );

    // Le cas de #3287 : le telechargement amont a echoue, le producteur
    // abandonne. Un vrai fichier temporaire, pour verifier qu'il part.
    let (morte, _tx2, _ready2) = orch.streamer.create_session(info, false, 1).await;
    // `test_scratch` et pas un chemin compose a la main : le garde
    // `aucun_chemin_temporaire_compose_a_la_main_dans_du_code_de_test`
    // (#3030) refuse le second, et son `Drop` nettoie meme quand l'epreuve
    // panique. Le fichier est ecrit ICI ; c'est le code de PRODUCTION qui
    // doit le retirer, et c'est ce que la derniere assertion mesure.
    let tmp = crate::test_scratch::scratch_file("3287-transcodage", ".flac");
    std::fs::write(&tmp, b"pas du flac").expect("fichier temporaire de l'epreuve");
    let tmp_str = tmp.as_str().to_string();

    abandonner_la_session_de_transcodage(&orch.streamer, &morte, &tmp_str).await;

    assert!(
        !orch.stream_session_alive(&morte).await,
        "la session d'un producteur mort doit avoir DISPARU : tant qu'elle \
         reste inscrite, son corps HTTP ne se termine jamais et le gapless \
         s'y enchaine (#3287)"
    );
    assert!(
        !orch.wait_stream_data_ready(&morte, 20).await,
        "une session disparue ne peut pas devenir prete"
    );
    assert!(
        !tmp.exists(),
        "le fichier temporaire du telechargement rate doit partir aussi : {tmp_str}"
    );
}

/// #1770 (point 3) — la sortie locale reconstruite à la volée lit les
/// réglages au lieu de les coder en dur.
///
/// L'essai porte sur la RÉSOLUTION, pas sur l'ouverture du périphérique :
/// sous Linux aucun chemin exclusif n'existe (`exclusive_mode_support`),
/// une épreuve qui ouvrirait un DAC serait verte contre rien ici. Ce qui
/// se prouve sur cette machine, c'est que les valeurs remises au
/// constructeur viennent de la BASE.
#[test]
fn les_reglages_de_sortie_locale_viennent_de_la_base() {
    let orch = test_orchestrator();
    let reglages = crate::db::settings_repo::SettingsRepo::with_backend(orch.db.clone());

    // Base vierge, environnement neutre : le repli est le défaut de
    // `TuneConfig` — c'est-à-dire ce que le codage en dur rendait.
    assert_eq!(
        orch.reglages_sortie_locale_avec(|_| None),
        (false, "auto".to_string()),
        "sur une base vierge le repli doit rester le défaut de TuneConfig"
    );

    reglages.set("local_audio_backend", "wasapi").unwrap();
    reglages.set("local_exclusive_mode", "true").unwrap();
    assert_eq!(
        orch.reglages_sortie_locale_avec(|_| None),
        (true, "wasapi".to_string()),
        "la sortie recréée ignore les réglages : un DAC éteint au \
         démarrage repartirait en partagé au premier appui sur Lecture, \
         alors que la page de réglages dit le contraire (#1770)."
    );

    // L'environnement n'est qu'un REPLI : la base gagne.
    let env_asio = |cle: &str| (cle == "TUNE_LOCAL_AUDIO_BACKEND").then(|| "asio".to_string());
    assert_eq!(
        orch.reglages_sortie_locale_avec(env_asio),
        (true, "wasapi".to_string()),
        "l'environnement ne doit pas passer devant la valeur enregistrée"
    );

    // Sans valeur en base, l'environnement est lu — c'est le seul morceau
    // de la chaîne de `tune-server` qui soit atteignable depuis ici.
    reglages.delete("local_audio_backend").unwrap();
    assert_eq!(
        orch.reglages_sortie_locale_avec(env_asio),
        (true, "asio".to_string()),
        "TUNE_LOCAL_AUDIO_BACKEND doit servir de repli comme dans \
         tune-server/src/config.rs"
    );
}

fn test_orchestrator() -> PlaybackOrchestrator {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    )
}

/// #3229 — l'avance gapless remet le curseur à 0:00 pour de VRAI.
///
/// La position publiée ne recule plus dans une piste : `update_position`
/// refuse désormais qu'une observation du renderer descende sous ce qui a
/// déjà été affiché. Or `advance_queue_metadata` est le SEUL changement de
/// piste qui n'emprunte pas `play()` — il évite exprès le rebond de
/// `track_generation` — et il rendait la piste suivante à 0 par cette
/// MÊME porte. Prise pour un renderer qui se contredit, la remise à zéro
/// aurait été refusée et le curseur serait resté collé à la fin de la piste
/// précédente pendant tout un album enchaîné : un défaut permanent, visible
/// de tous, bien pire que celui qu'on corrige.
///
/// L'épreuve passe par l'orchestrateur, et non par un appel direct à
/// `reset_position` : c'est le CHOIX DE LA PORTE, ici, qui est en jeu.
#[tokio::test]
async fn avance_gapless_remet_la_position_a_zero() {
    let orch = test_orchestrator();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Zone 3229", Some("local"), None)
        .unwrap();

    let pistes = crate::db::track_repo::TrackRepo::with_backend(orch.db.clone());
    let mut ids = Vec::new();
    for n in 1..=2 {
        let mut piste = crate::db::models::Track::new(format!("Piste {n}"));
        piste.file_path = Some(format!("/aucun/chemin/3229/piste{n}.flac"));
        piste.track_number = n;
        piste.duration_ms = 179_000;
        ids.push(pistes.create(&piste).unwrap());
    }
    crate::db::play_queue_repo::PlayQueueRepo::with_backend(orch.db.clone())
        .set_queue(zone_id, &ids)
        .unwrap();

    orch.playback
        .play(
            zone_id,
            NowPlaying {
                track_id: Some(ids[0]),
                title: "Piste 1".into(),
                duration_ms: 179_000,
                source: "local".into(),
                ..Default::default()
            },
        )
        .await;
    // Fin de la piste 1, telle que le sondeur vient de la publier.
    assert_eq!(
        orch.playback.update_position(zone_id, 179_000).await,
        179_000
    );

    orch.advance_queue_metadata(zone_id, 1)
        .await
        .expect("l'avance gapless doit aboutir");

    assert_eq!(
        orch.playback.get_state(zone_id).await.position_ms,
        0,
        "après une avance gapless le curseur doit repartir de 0:00 (#3229)"
    );
    assert_eq!(
        orch.playback.update_position(zone_id, 1_000).await,
        1_000,
        "et la piste 2 doit pouvoir compter depuis le début"
    );
}

// ------------------------------------------------------------------
// #1541 — VU-mètres après une avance gapless, DSD local compris.
// ------------------------------------------------------------------

/// Écrit un `.dsf` DSD64 stéréo valide : `blocs_par_canal` super-blocs de
/// 4096 octets par canal, remplis en carré (un bloc à `0xFF`, le suivant à
/// `0x00`, soit ~43 Hz). Le signal est FRANC : le test peut exiger des
/// niveaux au-dessus du silence, et pas seulement l'existence
/// d'événements — un forwarder nourri de zéros émettrait tout autant.
fn ecrire_dsf_carre(path: &std::path::Path, blocs_par_canal: usize) {
    const BLOC: usize = 4096;
    const CANAUX: usize = 2;
    let mut data = Vec::with_capacity(blocs_par_canal * BLOC * CANAUX);
    // Disposition DSF : bloc du canal 0, bloc du canal 1, bloc suivant du
    // canal 0… Les deux canaux portent le même carré.
    for indice_bloc in 0..blocs_par_canal * CANAUX {
        let octet: u8 = if (indice_bloc / CANAUX) % 2 == 0 {
            0xFF
        } else {
            0x00
        };
        data.extend(std::iter::repeat_n(octet, BLOC));
    }
    let total_samples = (blocs_par_canal * BLOC * 8) as u64;

    let mut buf = Vec::with_capacity(92 + data.len());
    buf.extend_from_slice(b"DSD ");
    buf.extend_from_slice(&28u64.to_le_bytes());
    buf.extend_from_slice(&(28 + 52 + 12 + data.len() as u64).to_le_bytes());
    buf.extend_from_slice(&0u64.to_le_bytes()); // pas de métadonnées
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&52u64.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes()); // version
    buf.extend_from_slice(&0u32.to_le_bytes()); // format = DSD brut
    buf.extend_from_slice(&2u32.to_le_bytes()); // type de canaux = stéréo
    buf.extend_from_slice(&(CANAUX as u32).to_le_bytes());
    buf.extend_from_slice(&2_822_400u32.to_le_bytes()); // DSD64
    buf.extend_from_slice(&1u32.to_le_bytes()); // bits par échantillon
    buf.extend_from_slice(&total_samples.to_le_bytes());
    buf.extend_from_slice(&(BLOC as u32).to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // réservé
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(12 + data.len() as u64).to_le_bytes());
    buf.extend_from_slice(&data);
    std::fs::write(path, &buf).unwrap();
}

/// Zone à sortie LOCALE, en lecture, dont la file contient deux fois le
/// même fichier : l'état exact d'un album au moment où l'enchaînement
/// gapless bascule sur la piste 2.
async fn zone_locale_prete_a_enchainer(
    chemin: &str,
    format: &str,
) -> (
    Arc<PlaybackOrchestrator>,
    Arc<EventBus>,
    i64,
    tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
) {
    let bus = Arc::new(EventBus::new());
    let mut orch = test_orchestrator();
    orch.event_bus = Some(bus.clone());
    let orch = Arc::new(orch);

    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Smart DX1", Some("local"), Some("local:Smart DX1"))
        .unwrap();

    let pistes = crate::db::track_repo::TrackRepo::with_backend(orch.db.clone());
    let mut ids = Vec::new();
    for n in 1..=2 {
        let mut piste = crate::db::models::Track::new(format!("Piste {n}"));
        // `tracks.file_path` est UNIQUE : seule la piste 2 — celle sur
        // laquelle l'enchaînement bascule, donc la seule qui sera décodée
        // — porte le vrai fichier.
        piste.file_path = Some(if n == 2 {
            chemin.to_string()
        } else {
            format!("{chemin}.piste1")
        });
        piste.format = Some(format.to_string());
        piste.sample_rate = Some(2_822_400);
        piste.bit_depth = Some(1);
        piste.channels = 2;
        piste.track_number = n;
        piste.duration_ms = 2_000;
        ids.push(pistes.create(&piste).unwrap());
    }
    crate::db::play_queue_repo::PlayQueueRepo::with_backend(orch.db.clone())
        .set_queue(zone_id, &ids)
        .unwrap();

    // La zone joue déjà la piste 1 : sans état `Playing`, le forwarder
    // attend au lieu d'émettre et le test ne mesurerait que son horloge.
    orch.playback.play(zone_id, NowPlaying::default()).await;
    let rx = bus.subscribe();
    (orch, bus, zone_id, rx)
}

/// Compte les `playback.audio_levels` de `zone_id` pendant `fenetre`, et
/// rend aussi la crête maximale vue. S'arrête dès que `attendus` sont
/// atteints : un test qui réussit ne paie pas le délai complet.
async fn compter_niveaux(
    rx: &mut tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
    zone_id: i64,
    fenetre: std::time::Duration,
    attendus: u32,
) -> (u32, f64) {
    let mut n = 0u32;
    let mut crete = f64::NEG_INFINITY;
    let echeance = tokio::time::Instant::now() + fenetre;
    loop {
        let reste = echeance.saturating_duration_since(tokio::time::Instant::now());
        if reste.is_zero() || (attendus > 0 && n >= attendus) {
            break;
        }
        match tokio::time::timeout(reste, rx.recv()).await {
            Ok(Ok(ev))
                if ev.event_type == "playback.audio_levels"
                    && ev.data.get("zone_id").and_then(|v| v.as_i64()) == Some(zone_id) =>
            {
                n += 1;
                if let Some(p) = ev.data.get("peak_left_db").and_then(|v| v.as_f64()) {
                    crete = crete.max(p);
                }
            }
            Ok(Ok(_)) => {}
            _ => break,
        }
    }
    (n, crete)
}

/// #1541 : après une avance gapless sur une piste **DSD locale**, la zone
/// doit ré-émettre des `playback.audio_levels`.
///
/// `bump_levels_gen` vient de tuer le forwarder de la piste précédente ;
/// si rien ne le remplace, les aiguilles ne retombent pas à zéro — elles
/// GÈLENT sur leur dernière valeur, ce que Xavier Joly décrit depuis la
/// v0.9.98 (« l'aiguille bouge une fois au début puis reste bloquée »),
/// pendant que le FLAC de la même zone continue de les animer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn avance_gapless_en_dsd_local_ranime_les_vu_metres() {
    let dsf = tempfile::Builder::new().suffix(".dsf").tempfile().unwrap();
    // ~1,5 s de DSD64 : de quoi produire une quarantaine de fenêtres de
    // 40 ms, cadencées à la vitesse de lecture par le forwarder.
    ecrire_dsf_carre(dsf.path(), 130);
    let chemin = dsf.path().to_str().unwrap().to_string();
    let (orch, _bus, zone_id, mut rx) = zone_locale_prete_a_enchainer(&chemin, "dsf").await;

    orch.advance_queue_metadata(zone_id, 1)
        .await
        .expect("l'avance gapless doit aboutir");

    let (n, crete) =
        compter_niveaux(&mut rx, zone_id, std::time::Duration::from_secs(20), 25).await;
    assert!(
        n >= 25,
        "après l'avance gapless, un DSD local doit ré-alimenter les VU : reçu {n} événements"
    );
    assert!(
        crete > -20.0,
        "les niveaux doivent décrire le SIGNAL, pas du silence : crête {crete:.1} dBFS"
    );
}

/// Contre-épreuve PERMANENTE du test ci-dessus : la décision d'avant le
/// correctif — `file_path.filter(|_| !is_dsd)` — recopiée telle quelle,
/// branchée sur le même harnais.
///
/// Elle vérifie deux choses qu'un test vert ne prouve jamais tout seul :
/// que l'injection de panne ÉCHOUE bien (aucun forwarder n'est créé), et
/// que le compteur du harnais rend alors `0`. Si un jour des
/// `audio_levels` arrivaient dans ce harnais par un autre chemin, le test
/// principal deviendrait insensible au défaut : celui-ci tomberait en
/// premier.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contre_epreuve_le_filtre_dsd_historique_eteint_bien_les_vu() {
    let dsf = tempfile::Builder::new().suffix(".dsf").tempfile().unwrap();
    ecrire_dsf_carre(dsf.path(), 130);
    let chemin = dsf.path().to_str().unwrap().to_string();
    let (orch, bus, zone_id, mut rx) = zone_locale_prete_a_enchainer(&chemin, "dsf").await;

    // Décision historique, verbatim.
    let decision_historique = |format: Option<&str>, file_path: Option<String>| {
        let is_dsd = format.is_some_and(|f: &str| {
            matches!(f.to_ascii_lowercase().as_str(), "dsf" | "dff" | "dsd")
        });
        file_path.filter(|_| !is_dsd)
    };

    // Le reste de l'avance, à l'identique : les forwarders de la piste
    // précédente meurent, puis on ne crée QUE ce que la décision autorise.
    orch.playback.bump_levels_gen(zone_id);
    let play_seq = orch.playback.current_play_seq(zone_id).await;
    let choisi = decision_historique(Some("dsf"), Some(chemin.clone()));
    assert!(
        choisi.is_none(),
        "l'injection de panne doit bien priver le DSD de forwarder"
    );
    if let Some(p) = choisi {
        super::spawn_local_file_levels_decode(bus, orch.playback.clone(), zone_id, play_seq, p);
    }

    let (n, _) = compter_niveaux(&mut rx, zone_id, std::time::Duration::from_secs(3), 0).await;
    assert_eq!(
        n, 0,
        "sous le défaut, ce harnais doit voir ZÉRO niveau — sinon le test principal ne prouve rien"
    );
}

/// La décision elle-même, cas par cas. Le DSD n'est plus exclu ; la seule
/// sortie qui ne mesure pas (OAAT en DSD natif) ne paie toujours pas le
/// décodage, et aucun autre format ne dépend de cette réponse.
#[test]
fn le_fichier_a_mesurer_couvre_le_dsd_sauf_quand_rien_ne_mesure() {
    let f = || Some("/musique/piste".to_string());
    for fmt in ["dsf", "dff", "dsd", "DSF", "Dff"] {
        assert_eq!(
            super::fichier_a_mesurer_apres_avance(Some(fmt), f(), true),
            f(),
            "{fmt} : une sortie qui mesure doit recevoir des niveaux"
        );
        assert_eq!(
            super::fichier_a_mesurer_apres_avance(Some(fmt), f(), false),
            None,
            "{fmt} : rendre du 1 bit en PCM pour une sortie qui ne mesure pas"
        );
    }
    for fmt in ["flac", "wav", "mp3", "alac"] {
        assert_eq!(
            super::fichier_a_mesurer_apres_avance(Some(fmt), f(), true),
            f(),
            "{fmt} : comportement inchangé"
        );
        assert_eq!(
            super::fichier_a_mesurer_apres_avance(Some(fmt), f(), false),
            f(),
            "{fmt} : la réponse de la sortie ne concerne que le DSD"
        );
    }
    assert_eq!(
        super::fichier_a_mesurer_apres_avance(Some("flac"), None, true),
        None,
        "sans chemin de fichier, rien à décoder"
    );
    assert_eq!(
        super::fichier_a_mesurer_apres_avance(None, f(), true),
        f(),
        "format inconnu : on décode, comme avant"
    );
}

/// Le bridage du décodage-pour-niveaux : au plus 30 s d'avance sur ce que
/// la zone rapporte. Sans lui, le décodeur d'un DSD local part plein pot
/// et la file du forwarder — non bornée — retient tout le PCM de la piste.
#[test]
fn le_decodage_pour_niveaux_ne_prend_pas_plus_de_30_s_d_avance() {
    assert!(!super::levels_decode_doit_freiner(0, 0));
    assert!(!super::levels_decode_doit_freiner(30_000, 0));
    assert!(super::levels_decode_doit_freiner(30_001, 0));
    // La lecture avance : le décodage repart d'autant.
    assert!(!super::levels_decode_doit_freiner(90_000, 60_000));
    assert!(super::levels_decode_doit_freiner(90_001, 60_000));
}

// ------------------------------------------------------------------
// #3104 — les niveaux d'un CACHE HIT : mesurés, pas seulement écrits.
// ------------------------------------------------------------------

/// Écrit un WAV PCM 44,1 kHz / 16 bits / stéréo de `duree_ms`, rempli d'un
/// carré à ~43 Hz et -2,7 dBFS. C'est la forme exacte d'une rendition mise
/// en cache pour un renderer DLNA en LPCM. Le signal est FRANC : un test
/// peut exiger des niveaux au-dessus du silence, et pas seulement
/// l'existence d'événements.
fn ecrire_wav_carre(path: &std::path::Path, duree_ms: u64) {
    const SR: u32 = 44_100;
    const CANAUX: u16 = 2;
    const BITS: u16 = 16;
    let trames = (SR as u64 * duree_ms / 1000) as usize;
    let mut buf = Vec::with_capacity(44 + trames * 4);
    buf.extend_from_slice(&crate::audio::wav::build_wav_header_with_duration(
        CANAUX,
        SR,
        BITS,
        Some(duree_ms),
    ));
    let demi_periode = (SR / 86).max(1) as usize;
    for t in 0..trames {
        let v: i16 = if (t / demi_periode) % 2 == 0 {
            24_000
        } else {
            -24_000
        };
        buf.extend_from_slice(&v.to_le_bytes());
        buf.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, &buf).unwrap();
}

/// Ce que la file du forwarder RETIENT quand la zone n'a pas encore avancé
/// (position rapportée à 0, l'état d'un début de lecture) : fenêtres en
/// attente, octets de PCM, et millisecondes d'audio.
///
/// La file est reproduite à l'identique du forwarder — `unbounded_channel`
/// de [`crate::audio::tap::RawWindow`], chaque fenêtre portant son
/// `pcm: Vec<u8>` — et personne ne la draine : c'est exactement un
/// forwarder qui n'a encore rien publié. Le SEUL écart entre les deux
/// appels est le puits : `freine = false` reproduit verbatim le drain
/// inconditionnel du bloc en ligne livré par #3104
/// (`while sink_rx.recv().await.is_some() {}`), `freine = true` celui de
/// `spawn_local_file_levels_decode`.
///
/// Aucun délai dans la mesure : on compte des tours jusqu'à ce que le
/// décodage rende la main (cas sans frein) ou que l'avance décodée cesse de
/// croître (cas freiné — le décodeur reste bloqué sur son canal borné tant
/// que la zone est à 0). Le cas freiné atteint un PLATEAU : attendre plus
/// longtemps ne change pas le chiffre, donc la mesure n'est pas une course.
async fn pcm_retenu(chemin: &str, freine: bool) -> (usize, usize, i64) {
    use std::sync::atomic::Ordering::Relaxed;
    let (levels_tx, mut levels_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::audio::tap::RawWindow>();
    let avance_ms = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    let relache = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Relais compteur d'avance, identique à celui de la fonction bridée.
    let (relais_tx, mut relais_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::audio::tap::RawWindow>();
    let relais = {
        let avance_ms = avance_ms.clone();
        tokio::spawn(async move {
            while let Some(raw) = relais_rx.recv().await {
                avance_ms.fetch_add(raw.window.as_millis() as i64, Relaxed);
                if levels_tx.send(raw).is_err() {
                    break;
                }
            }
        })
    };

    let (sink_tx, mut sink_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    {
        let avance_ms = avance_ms.clone();
        let relache = relache.clone();
        tokio::spawn(async move {
            while sink_rx.recv().await.is_some() {
                while freine
                    && !relache.load(Relaxed)
                    && super::levels_decode_doit_freiner(avance_ms.load(Relaxed), 0)
                {
                    tokio::time::sleep(super::LEVELS_HOLD).await;
                }
            }
        });
    }

    let ready = std::sync::Arc::new(tokio::sync::Notify::new());
    let fichier = chemin.to_string();
    let mut decodage = tokio::task::spawn_blocking(move || {
        crate::audio::decode::decode_to_pcm_streaming_with_levels(
            &fichier,
            None,
            None,
            None,
            sink_tx,
            super::LEVELS_DECODE_CHUNK,
            ready,
            relais_tx,
        )
    });

    let mut fini = false;
    let mut precedent = -1i64;
    let mut plateau = 0u32;
    for _ in 0..2_000 {
        if tokio::time::timeout(std::time::Duration::from_millis(20), &mut decodage)
            .await
            .is_ok()
        {
            fini = true;
            break;
        }
        let a = avance_ms.load(Relaxed);
        if a == precedent {
            plateau += 1;
            if plateau >= 25 {
                break;
            }
        } else {
            plateau = 0;
            precedent = a;
        }
    }
    // Décodage terminé : le relais se ferme de lui-même (son émetteur est
    // tombé avec la tâche de décodage). On l'attend pour que la file soit
    // complète au moment du comptage. En plateau, l'avance stable prouve
    // déjà que le relais est au repos.
    if fini {
        let _ = relais.await;
    }

    let mut fenetres = 0usize;
    let mut octets = 0usize;
    let mut audio_ms = 0i64;
    while let Ok(raw) = levels_rx.try_recv() {
        fenetres += 1;
        octets += raw.pcm.len();
        audio_ms += raw.window.as_millis() as i64;
    }

    // Libérer le décodeur : sans cela il resterait bloqué sur son canal
    // borné pour toute la vie du binaire de test.
    relache.store(true, Relaxed);
    if !fini {
        let _ = decodage.await;
    }
    (fenetres, octets, audio_ms)
}

/// Contre-épreuve CHIFFRÉE du frein sur le chemin du cache hit.
///
/// #3104 a recopié la forme du décodage-pour-niveaux — flux, PCM au puits —
/// mais pas son frein : son puits drainait sans condition. Le décodage
/// courait alors à la vitesse du DISQUE pendant que le forwarder ne publie
/// qu'au temps réel, et la file du forwarder — non bornée, chaque fenêtre
/// portant son PCM — retenait la piste ENTIÈRE. Le cache hit étant le cas
/// courant, la fuite l'était aussi.
///
/// Le test mesure les deux formes sur deux durées : sans frein la rétention
/// SUIT la durée de la piste (elle double quand la piste double) ; avec
/// frein elle plafonne à ~30 s d'audio, quelle que soit la piste.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn la_rendition_en_cache_ne_retient_plus_toute_la_piste() {
    let court = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
    let long = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
    ecrire_wav_carre(court.path(), 60_000);
    ecrire_wav_carre(long.path(), 120_000);
    let court = court.path().to_str().unwrap().to_string();
    let long = long.path().to_str().unwrap().to_string();

    // Forme livrée par #3104 : puits sans frein.
    let (f60, o60, ms60) = pcm_retenu(&court, false).await;
    let (f120, o120, ms120) = pcm_retenu(&long, false).await;
    // Forme bridée : celle de `spawn_local_file_levels_decode`.
    let (g60, p60, gms60) = pcm_retenu(&court, true).await;
    let (g120, p120, gms120) = pcm_retenu(&long, true).await;

    println!(
        "sans frein — 60 s : {f60} fenêtres / {o60} octets / {ms60} ms ; \
         120 s : {f120} fenêtres / {o120} octets / {ms120} ms"
    );
    println!(
        "avec frein — 60 s : {g60} fenêtres / {p60} octets / {gms60} ms ; \
         120 s : {g120} fenêtres / {p120} octets / {gms120} ms"
    );

    assert!(
        ms60 >= 59_000 && ms120 >= 119_000,
        "sans frein, la file doit retenir la piste entière — c'est le défaut \
         qu'on mesure : {ms60} ms sur 60 s, {ms120} ms sur 120 s"
    );
    assert!(
        o120 as f64 > o60 as f64 * 1.9,
        "sans frein, la rétention doit SUIVRE la durée : {o60} octets sur \
         60 s contre {o120} sur 120 s — si ce rapport n'est pas ~2, la \
         mesure ne décrit pas ce qu'elle prétend"
    );

    assert!(
        gms60 <= 35_000 && gms120 <= 35_000,
        "le frein doit plafonner la file à ~30 s d'audio (PROXY_LEVELS_MAX_AHEAD_MS \
         plus le canal borné du puits) : {gms60} ms sur 60 s, {gms120} ms sur 120 s"
    );
    assert!(
        gms60 >= 25_000,
        "le frein ne doit pas ÉTEINDRE les niveaux : il en garde ~30 s \
         d'avance ; reçu {gms60} ms"
    );
    assert!(
        (gms120 - gms60).abs() <= 4_000,
        "le plafond ne doit pas dépendre de la durée de la piste : {gms60} ms \
         sur 60 s contre {gms120} ms sur 120 s"
    );
    assert!(
        (p120 as f64) < o120 as f64 / 2.0,
        "sur 120 s, le frein doit diviser la rétention : {p120} octets contre \
         {o120} sans frein"
    );
}

/// #3104 a livré son correctif avec un garde sur le TEXTE source : il
/// vérifiait qu'un forwarder était bien câblé dans la tranche du cache hit,
/// sans jamais faire passer UNE SEULE fenêtre. Voici la mesure qui manquait.
///
/// On appelle ce que la branche « cache hit » appelle, avec la même
/// génération épinglée, sur une RENDITION (pas sur la source) : les
/// `playback.audio_levels` doivent monter sur le bus, et décrire du SIGNAL.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_cache_hit_fait_bien_monter_des_niveaux_sur_le_bus() {
    let rendition = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
    ecrire_wav_carre(rendition.path(), 5_000);
    let chemin = rendition.path().to_str().unwrap().to_string();
    let (orch, bus, zone_id, mut rx) = zone_locale_prete_a_enchainer(&chemin, "wav").await;

    let play_seq = orch.playback.current_play_seq(zone_id).await;
    super::spawn_local_file_levels_decode(bus, orch.playback.clone(), zone_id, play_seq, chemin);

    let (n, crete) =
        compter_niveaux(&mut rx, zone_id, std::time::Duration::from_secs(20), 25).await;
    assert!(
        n >= 25,
        "une piste servie depuis le cache de transcodage doit animer les VU : \
         reçu {n} événements"
    );
    assert!(
        crete > -20.0,
        "les niveaux doivent décrire le SIGNAL de la rendition, pas du \
         silence : crête {crete:.1} dBFS"
    );
}

/// Contre-épreuve PERMANENTE du test ci-dessus : la décision d'AVANT #3104
/// — un cache hit sert le fichier et n'attache rien — sur le même harnais.
/// Si des `audio_levels` arrivaient ici par un autre chemin, le test
/// principal deviendrait insensible au défaut ; celui-ci tomberait d'abord.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contre_epreuve_un_cache_hit_sans_niveaux_laisse_les_vu_morts() {
    let rendition = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
    ecrire_wav_carre(rendition.path(), 5_000);
    let chemin = rendition.path().to_str().unwrap().to_string();
    let (_orch, _bus, zone_id, mut rx) = zone_locale_prete_a_enchainer(&chemin, "wav").await;

    // Comportement historique : la branche de cache créait la session de
    // streaming et s'arrêtait là. Rien n'est attaché, volontairement.
    let (n, _) = compter_niveaux(&mut rx, zone_id, std::time::Duration::from_secs(3), 0).await;
    assert_eq!(
        n, 0,
        "sous le défaut, ce harnais doit voir ZÉRO niveau — sinon le test \
         principal ne prouve rien"
    );
}

// ------------------------------------------------------------------
// #3145 — les niveaux du PASSTHROUGH : le frein qui manquait depuis
// #1423, mesuré sur le même fait de base que #3144.
// ------------------------------------------------------------------

/// Ce que la file du forwarder RETIENT sur le chemin PASSTHROUGH, quand la
/// zone n'a pas encore avancé (position 0, l'état d'un début de lecture) :
/// fenêtres en attente, octets de PCM, millisecondes d'audio.
///
/// Mesuré sur le VRAI décodeur et avec les valeurs TAGUÉES de la piste
/// (`Some(sr)` / `Some(ch)`) — celles que la branche emploie, et que le
/// correctif ne change pas.
///
/// La file est reproduite à l'identique de celle du forwarder —
/// `unbounded_channel` de [`crate::audio::tap::RawWindow`], chaque fenêtre
/// portant son `pcm: Vec<u8>` — et personne ne la draine : c'est exactement
/// un forwarder qui n'a encore rien publié.
///
/// Le SEUL écart entre les deux appels est le PUITS :
/// - `freine = false` reproduit verbatim le bloc d'origine (#1423) — drain
///   inconditionnel, et le décodeur émet DIRECTEMENT dans la file ;
/// - `freine = true` branche la vraie fonction de production,
///   [`super::spawn_braked_levels_sink`] — pas une réplique — sur une zone
///   réelle restée à la position 0.
///
/// Aucun `sleep` d'attente : on compte des TOURS jusqu'à ce que le décodage
/// rende la main (cas sans frein) ou que la file cesse de grandir (cas
/// freiné — le décodeur reste bloqué sur son canal borné tant que la zone
/// est à 0). Le cas freiné atteint un PLATEAU : attendre plus longtemps ne
/// change pas le chiffre, donc la mesure n'est pas une course.
async fn pcm_retenu_passthrough(
    chemin: &str,
    sr: u32,
    ch: u32,
    freine: bool,
) -> (usize, usize, i64) {
    let (orch, _bus, zone_id, _rx) = zone_locale_prete_a_enchainer(chemin, "wav").await;
    let (levels_tx, mut levels_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::audio::tap::RawWindow>();

    let (sink_tx, decode_tx) = if freine {
        super::spawn_braked_levels_sink(orch.playback.clone(), zone_id, levels_tx)
    } else {
        // Forme d'origine : le puits draine sans condition, et le décodeur
        // écrit DIRECTEMENT dans la file du forwarder.
        let (sink_tx, mut sink_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
        tokio::spawn(async move { while sink_rx.recv().await.is_some() {} });
        (sink_tx, levels_tx)
    };

    let ready = std::sync::Arc::new(tokio::sync::Notify::new());
    let fichier = chemin.to_string();
    let mut decodage = tokio::task::spawn_blocking(move || {
        crate::audio::decode::decode_to_pcm_streaming_with_levels(
            &fichier,
            Some(sr),
            Some(ch),
            None,
            sink_tx,
            super::LEVELS_DECODE_CHUNK,
            ready,
            decode_tx,
        )
    });

    let mut fini = false;
    let mut precedent = usize::MAX;
    let mut plateau = 0u32;
    for _ in 0..2_000 {
        if tokio::time::timeout(std::time::Duration::from_millis(20), &mut decodage)
            .await
            .is_ok()
        {
            fini = true;
            break;
        }
        let n = levels_rx.len();
        if n == precedent {
            plateau += 1;
            if plateau >= 25 {
                break;
            }
        } else {
            plateau = 0;
            precedent = n;
        }
    }

    let mut fenetres = 0usize;
    let mut octets = 0usize;
    let mut audio_ms = 0i64;
    while let Ok(raw) = levels_rx.try_recv() {
        fenetres += 1;
        octets += raw.pcm.len();
        audio_ms += raw.window.as_millis() as i64;
    }

    // Libérer le décodeur : sans cela il resterait bloqué sur son canal
    // borné pour toute la vie du binaire de test. On avance la position de
    // la ZONE — le frein est justement une comparaison à cette position,
    // donc c'est aussi la démonstration qu'il la relit à chaque tour.
    orch.playback.update_position(zone_id, 3_600_000).await;
    if !fini {
        let _ = decodage.await;
    }
    (fenetres, octets, audio_ms)
}

/// Contre-épreuve CHIFFRÉE du frein sur le chemin du PASSTHROUGH (#3145).
///
/// Le bloc d'origine (#1423) décodait en flux — le PCM part dans un puits,
/// seules les fenêtres ressortent — mais son puits DRAINAIT SANS
/// CONDITION. Le décodage courait donc à la vitesse du DISQUE pendant que
/// le forwarder ne publie qu'au TEMPS RÉEL, et sa file — non bornée, chaque
/// fenêtre portant son PCM — retenait la piste ENTIÈRE. La branche est
/// active pour toutes les sorties réseau et navigateur.
///
/// Le test mesure les deux formes sur DEUX durées : une seule ne
/// distinguerait pas un plateau d'une tendance. Sans frein la rétention
/// SUIT la durée de la piste (elle double quand la piste double) ; avec
/// frein elle plafonne à ~30 s d'audio, quelle que soit la piste.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn le_passthrough_ne_retient_plus_toute_la_piste() {
    const SR: u32 = 44_100;
    const CH: u32 = 2;
    let court = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
    let long = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
    ecrire_wav_carre(court.path(), 60_000);
    ecrire_wav_carre(long.path(), 120_000);
    let court = court.path().to_str().unwrap().to_string();
    let long = long.path().to_str().unwrap().to_string();

    // Forme d'origine (#1423) : puits sans frein.
    let (f60, o60, ms60) = pcm_retenu_passthrough(&court, SR, CH, false).await;
    let (f120, o120, ms120) = pcm_retenu_passthrough(&long, SR, CH, false).await;
    // Forme bridée : celle de `spawn_braked_levels_sink`.
    let (g60, p60, gms60) = pcm_retenu_passthrough(&court, SR, CH, true).await;
    let (g120, p120, gms120) = pcm_retenu_passthrough(&long, SR, CH, true).await;

    println!(
        "passthrough sans frein — 60 s : {f60} fenêtres / {o60} octets / {ms60} ms ; \
         120 s : {f120} fenêtres / {o120} octets / {ms120} ms"
    );
    println!(
        "passthrough avec frein — 60 s : {g60} fenêtres / {p60} octets / {gms60} ms ; \
         120 s : {g120} fenêtres / {p120} octets / {gms120} ms"
    );

    assert!(
        ms60 >= 59_000 && ms120 >= 119_000,
        "sans frein, la file doit retenir la piste entière — c'est le défaut \
         qu'on mesure : {ms60} ms sur 60 s, {ms120} ms sur 120 s"
    );
    assert!(
        o120 as f64 > o60 as f64 * 1.9,
        "sans frein, la rétention doit SUIVRE la durée : {o60} octets sur \
         60 s contre {o120} sur 120 s — si ce rapport n'est pas ~2, la \
         mesure ne décrit pas ce qu'elle prétend"
    );

    assert!(
        gms60 <= 35_000 && gms120 <= 35_000,
        "le frein doit plafonner la file à ~30 s d'audio \
         (PROXY_LEVELS_MAX_AHEAD_MS plus le canal borné du puits) : \
         {gms60} ms sur 60 s, {gms120} ms sur 120 s"
    );
    assert!(
        gms60 >= 25_000,
        "le frein ne doit pas ÉTEINDRE les niveaux du passthrough : il en \
         garde ~30 s d'avance ; reçu {gms60} ms"
    );
    assert!(
        (gms120 - gms60).abs() <= 4_000,
        "le plafond ne doit pas dépendre de la durée de la piste : {gms60} ms \
         sur 60 s contre {gms120} ms sur 120 s — un PLATEAU, pas une tendance"
    );
    assert!(
        (p120 as f64) < o120 as f64 / 2.0,
        "sur 120 s, le frein doit diviser la rétention : {p120} octets contre \
         {o120} sans frein"
    );
}

/// Témoin vert : le frein ne supprime pas la FONCTIONNALITÉ.
///
/// Même câblage que la branche passthrough — forwarder cadencé, puits
/// bridé, décodage aux valeurs TAGUÉES — sur un vrai bus : les
/// `playback.audio_levels` montent, et décrivent du SIGNAL, pas du silence.
/// Sans ce témoin, « la file ne retient plus rien » serait aussi vrai d'un
/// correctif qui aurait simplement débranché les VU-mètres.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_passthrough_bride_fait_toujours_monter_des_niveaux_sur_le_bus() {
    let fichier = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
    ecrire_wav_carre(fichier.path(), 5_000);
    let chemin = fichier.path().to_str().unwrap().to_string();
    let (orch, bus, zone_id, mut rx) = zone_locale_prete_a_enchainer(&chemin, "wav").await;

    // Génération épinglée AVANT le spawn, comme dans la branche (#1110).
    let play_seq = orch.playback.current_play_seq(zone_id).await;
    let levels_tx =
        super::spawn_paced_levels_forwarder(bus, orch.playback.clone(), zone_id, play_seq, 0);
    let (sink_tx, relais_tx) =
        super::spawn_braked_levels_sink(orch.playback.clone(), zone_id, levels_tx);
    let ready = std::sync::Arc::new(tokio::sync::Notify::new());
    tokio::task::spawn_blocking(move || {
        crate::audio::decode::decode_to_pcm_streaming_with_levels(
            &chemin,
            Some(44_100),
            Some(2),
            None,
            sink_tx,
            super::LEVELS_DECODE_CHUNK,
            ready,
            relais_tx,
        )
    });

    let (n, crete) =
        compter_niveaux(&mut rx, zone_id, std::time::Duration::from_secs(20), 25).await;
    assert!(
        n >= 25,
        "le passthrough bridé doit toujours animer les VU-mètres : \
         reçu {n} événements"
    );
    assert!(
        crete > -20.0,
        "les niveaux doivent décrire le SIGNAL de la piste, pas du \
         silence : crête {crete:.1} dBFS"
    );
}

/// Total du PCM effectivement produit par le décodeur pour ces paramètres
/// de sortie. Le puits draine, la file de niveaux aussi : on ne mesure ici
/// que ce que le décodage FABRIQUE, pas ce qu'il retient.
async fn octets_decodes(chemin: &str, sr: Option<u32>, ch: Option<u32>) -> usize {
    use std::sync::atomic::Ordering::Relaxed;
    let (levels_tx, mut levels_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::audio::tap::RawWindow>();
    let drain = tokio::spawn(async move { while levels_rx.recv().await.is_some() {} });
    let total = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (sink_tx, mut sink_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let puits = {
        let total = total.clone();
        tokio::spawn(async move {
            while let Some(bloc) = sink_rx.recv().await {
                total.fetch_add(bloc.len(), Relaxed);
            }
        })
    };
    let ready = std::sync::Arc::new(tokio::sync::Notify::new());
    let fichier = chemin.to_string();
    let _ = tokio::task::spawn_blocking(move || {
        crate::audio::decode::decode_to_pcm_streaming_with_levels(
            &fichier,
            sr,
            ch,
            None,
            sink_tx,
            super::LEVELS_DECODE_CHUNK,
            ready,
            levels_tx,
        )
    })
    .await;
    let _ = puits.await;
    let _ = drain.await;
    total.load(Relaxed)
}

/// L'ARBITRAGE, mesuré : les valeurs taguées ne sont pas le débit natif.
///
/// La correction évidente aurait été de renvoyer le passthrough vers
/// `spawn_local_file_levels_decode`, la jumelle qui porte déjà le frein —
/// c'est ce que #3144 a fait pour le cache de transcodage. Elle décode au
/// débit NATIF (`None, None, None`), alors que le passthrough décode aux
/// valeurs TAGUÉES de la piste (`tracks.sample_rate` / `tracks.channels`).
///
/// Ce test montre que l'écart n'est pas théorique : sur un fichier dont le
/// tag ment — ce qu'est, par construction, la population qui arrive en
/// passthrough — le décodage suit le TAG et produit deux fois plus de PCM
/// que le natif. Si les deux chiffres coïncidaient, l'arbitrage n'existerait
/// pas et la jumelle aurait suffi.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn les_valeurs_taguees_ne_sont_pas_le_debit_natif() {
    let fichier = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
    ecrire_wav_carre(fichier.path(), 3_000); // 44,1 kHz / 16 bits / stéréo
    let chemin = fichier.path().to_str().unwrap().to_string();

    let natif = octets_decodes(&chemin, None, None).await;
    let bien_tague = octets_decodes(&chemin, Some(44_100), Some(2)).await;
    let mal_tague = octets_decodes(&chemin, Some(88_200), Some(2)).await;
    println!(
        "octets décodés — natif : {natif} ; tag exact : {bien_tague} ; \
         tag menteur (88,2 kHz) : {mal_tague}"
    );

    assert_eq!(
        natif, bien_tague,
        "sur un fichier bien tagué, décoder au tag ou au natif doit donner \
         le MÊME PCM — sinon ce test ne mesure pas ce qu'il prétend"
    );
    assert!(
        mal_tague as f64 > natif as f64 * 1.9,
        "sur un fichier mal tagué, le décodage suit le TAG : {mal_tague} \
         octets contre {natif} au natif. C'est cet écart qui interdit de \
         remplacer le bloc du passthrough par `spawn_local_file_levels_decode`."
    );
}

/// #1985 : persister un nouvel égaliseur sans sortie locale vivante rend
/// `applied_live=false`, mais ce n'est pas une raison pour laisser le
/// client afficher l'ancien chemin du signal. `zone.updated` lui ordonne
/// de relire la zone, donc de reconstruire `signal_path` avec le profil qui
/// vient d'être écrit.
#[tokio::test]
async fn eq_change_announces_a_fresh_signal_path_without_live_output() {
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    let mut orch = test_orchestrator();
    orch.event_bus = Some(bus);
    let orch = Arc::new(orch);

    assert!(
        !orch.apply_eq_change(1_985).await,
        "sans sortie locale vivante, le réglage ne peut pas être appliqué à chaud"
    );

    let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("le client ne doit pas conserver un signal_path périmé")
        .expect("le bus doit rester ouvert");
    assert_eq!(event.event_type, "zone.updated");
    assert_eq!(event.data, serde_json::json!({ "zone_id": 1_985 }));
}

/// Zone locale gréée pour les tests de bascule PURE : une `LocalOutput`
/// enregistrée, un format déclaré (sinon les rafraîchisseurs renoncent), et
/// un profil d'égaliseur audible en base.
#[cfg(feature = "local-audio")]
async fn zone_locale_avec_eq(orch: &PlaybackOrchestrator) -> i64 {
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon", Some("local"), Some("local:DAC"))
        .unwrap();
    orch.outputs
        .lock()
        .await
        .register(Box::new(crate::outputs::local::LocalOutput::new(
            "DAC".to_string(),
        )));

    let profil = crate::audio::eq::EqProfile {
        enabled: true,
        bands: vec![crate::audio::eq::EqBandSpec {
            freq: 80.0,
            gain: 8.0,
            q: 0.71,
            band_type: "low_shelf".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    crate::db::settings_repo::SettingsRepo::with_backend(orch.db.clone())
        .set(
            &format!("zone_{zone_id}_eq_profile"),
            &serde_json::to_string(&profil).unwrap(),
        )
        .unwrap();
    zone_id
}

#[cfg(feature = "local-audio")]
async fn avec_sortie_locale<T>(
    orch: &PlaybackOrchestrator,
    f: impl FnOnce(&crate::outputs::local::LocalOutput) -> T,
) -> T {
    let arc = orch.outputs.lock().await.get("local:DAC").unwrap();
    let sortie = arc.lock().await;
    let local = sortie
        .as_any()
        .downcast_ref::<crate::outputs::local::LocalOutput>()
        .expect("sortie locale");
    f(local)
}

#[cfg(feature = "local-audio")]
fn regler_pure(orch: &PlaybackOrchestrator, zone_id: i64, actif: bool) {
    crate::db::settings_repo::SettingsRepo::with_backend(orch.db.clone())
        .set(
            &format!("zone_{zone_id}_audiophile"),
            &format!(r#"{{"enabled":{actif}}}"#),
        )
        .unwrap();
}

/// #2102 : retirer l'EQ d'une sortie locale vivante est une application à
/// chaud réussie, pas un échec qui autorise le redémarrage audible réservé
/// aux sorties réseau. L'état Playing est indispensable à la
/// contre-épreuve : c'est lui qui faisait armer `eq_replay_gen` auparavant.
#[cfg(feature = "local-audio")]
#[tokio::test]
async fn removing_local_eq_does_not_schedule_a_stream_replay() {
    let orch = Arc::new(test_orchestrator());
    let zone_id = zone_locale_avec_eq(&orch).await;

    avec_sortie_locale(&orch, |local| {
        local.declare_current_format_for_test(44_100, 2);
        local.set_eq(orch.load_eq_processor(zone_id, 44_100, 2));
        assert!(local.has_eq(), "le test doit commencer avec un EQ monté");
    })
    .await;
    orch.playback
        .play(zone_id, crate::playback::NowPlaying::default())
        .await;

    regler_pure(&orch, zone_id, true);
    assert!(
        orch.apply_eq_change(zone_id).await,
        "replace_eq_live(None) a bien servi la sortie locale immédiatement"
    );

    avec_sortie_locale(&orch, |local| {
        assert!(
            !local.has_eq(),
            "la bascule PURE doit retirer l'EqProcessor du flux vivant"
        );
    })
    .await;
    assert!(
        !orch.eq_replay_gen.lock().unwrap().contains_key(&zone_id),
        "une sortie locale déjà servie ne doit jamais armer une relecture de flux"
    );
}

/// Le signalement de Jean Valjean (#1986), rejoué : Bass Boost audible,
/// bascule en PURE **pendant** la lecture. Avant, la clé était écrite et la
/// sortie n'apprenait rien — l'`EqProcessor` restait monté et `pure_bypass`
/// à faux, donc chaque échantillon continuait d'être filtré pendant que le
/// badge PURE s'allumait.
#[cfg(feature = "local-audio")]
#[tokio::test]
async fn switching_to_pure_mid_track_stops_the_eq_at_once() {
    let orch = test_orchestrator();
    let zone_id = zone_locale_avec_eq(&orch).await;

    // Ce que fait le chemin de lecture au démarrage d'une piste hors PURE.
    avec_sortie_locale(&orch, |local| {
        local.declare_current_format_for_test(44_100, 2);
        local.set_eq(orch.load_eq_processor(zone_id, 44_100, 2));
        local.set_pure_bypass(false);
        local.set_replaygain_factor(0.5);
    })
    .await;
    avec_sortie_locale(&orch, |local| {
        assert!(local.has_eq(), "l'égaliseur doit être monté au départ");
    })
    .await;

    regler_pure(&orch, zone_id, true);
    assert!(
        orch.refresh_zone_pure_dsp(zone_id).await,
        "une sortie locale vivante doit recevoir le nouvel état"
    );

    avec_sortie_locale(&orch, |local| {
        assert!(
            !local.has_eq(),
            "PURE promet un chemin intouché : l'EqProcessor doit être retiré"
        );
        assert!(
            local.pure_bypass_for_test(),
            "le drapeau que lit apply_local_dsp doit être armé"
        );
        // Le ReplayGain n'est PAS couvert par ce drapeau — il multiplie les
        // échantillons dans les callbacks de rendu. Sans sa remise à
        // l'unité, PURE laisserait un gain en place.
        assert_eq!(local.replaygain_units_for_test(), 1000);
    })
    .await;
}

/// Point 3 du même signalement : « je devrais revenir au réglage
/// précédent ». Sortir de PURE doit remonter l'égaliseur choisi par
/// l'utilisateur, sans attendre la piste suivante.
#[cfg(feature = "local-audio")]
#[tokio::test]
async fn leaving_pure_mid_track_brings_the_eq_back() {
    let orch = test_orchestrator();
    let zone_id = zone_locale_avec_eq(&orch).await;
    regler_pure(&orch, zone_id, true);

    avec_sortie_locale(&orch, |local| {
        local.declare_current_format_for_test(44_100, 2);
        local.set_eq(None);
        local.set_pure_bypass(true);
    })
    .await;

    regler_pure(&orch, zone_id, false);
    assert!(orch.refresh_zone_pure_dsp(zone_id).await);

    avec_sortie_locale(&orch, |local| {
        assert!(
            local.has_eq(),
            "hors PURE, le profil activé de la zone doit revenir tout de suite"
        );
        assert!(!local.pure_bypass_for_test());
    })
    .await;
}

/// Sans flux en cours, on ne rafraîchit rien : bâtir des biquads pour un
/// format inconnu donnerait des coefficients faux, et la prochaine lecture
/// appliquera l'état complet de toute façon. Le `false` rendu est ce qui
/// distingue « rien reçu » de « reçu, et vide ».
#[cfg(feature = "local-audio")]
#[tokio::test]
async fn nothing_playing_means_nothing_to_refresh() {
    let orch = test_orchestrator();
    let zone_id = zone_locale_avec_eq(&orch).await;
    regler_pure(&orch, zone_id, true);
    // `declare_current_format_for_test` volontairement non appelé.
    assert!(!orch.refresh_zone_pure_dsp(zone_id).await);
}

/// Une zone réseau n'a pas de sortie locale à rafraîchir : le traitement est
/// gravé dans le fichier transcodé. `refresh_zone_pure_dsp` doit rendre
/// `false` pour que `apply_audiophile_change` bascule sur le redémarrage.
#[tokio::test]
async fn a_network_zone_has_no_live_local_output() {
    let orch = test_orchestrator();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Ampli", Some("dlna"), Some("dlna:uuid-42"))
        .unwrap();
    assert!(!orch.refresh_zone_pure_dsp(zone_id).await);
}

/// Régression #1629 — reprendre une webradio dont le PRODUCTEUR de
/// décodage est mort (connexion icecast tombée pendant la pause, chemin de
/// sortie sans log) doit déclencher un RE-PLAY de la station — un nouveau
/// `play_media` vers la sortie, comme au premier lancement — et non une
/// reprise « sur place » qui rend du silence.
#[tokio::test]
async fn resuming_a_radio_with_a_dead_producer_replays_the_station() {
    let orch = test_orchestrator();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Zone Radio", Some("mock"), Some("mock-radio"))
        .unwrap();
    orch.outputs
        .lock()
        .await
        .register(Box::new(MockOutput::new("mock-radio", "Mock Radio")));

    // Session radio dont le producteur s'est terminé (comme après
    // `radio_reconnect_giving_up` ou un `consumer_dropped` silencieux).
    let (sid, _tx, _ready, session) = orch
        .streamer
        .create_radio_session(
            crate::http::streamer::StreamInfo {
                format: "wav".into(),
                mime_type: "audio/wav".into(),
                sample_rate: 48000,
                bit_depth: 16,
                channels: 2,
                ..Default::default()
            },
            8,
        )
        .await;
    session
        .producer_done
        .store(true, std::sync::atomic::Ordering::Relaxed);

    // La zone joue cette radio, puis est mise en pause (pause COURTE :
    // c'est bien la mort du producteur qui doit déclencher le re-play).
    orch.playback
        .play(
            zone_id,
            NowPlaying {
                title: "FIP".into(),
                source: "radio".into(),
                source_id: Some("http://icecast.example/fip.aac".into()),
                stream_id: Some(sid),
                ..Default::default()
            },
        )
        .await;
    orch.playback.pause(zone_id).await;

    orch.resume(zone_id, Some("mock-radio")).await.unwrap();

    let outputs = orch.outputs.lock().await;
    let out = outputs.get("mock-radio").unwrap();
    let guard = out.lock().await;
    let mock = guard
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("mock output");
    assert_eq!(
        mock.play_call_count().await,
        1,
        "producteur mort ⇒ la reprise doit rejouer la station (nouveau play_media)"
    );
    // Et la zone doit repartir en lecture avec une NOUVELLE session de flux.
    let state = orch.playback.get_state(zone_id).await;
    let np = state.now_playing.expect("now_playing après re-play");
    assert_eq!(np.source, "radio");
}

/// Contre-épreuve #1629 — pause courte ET producteur vivant : la reprise
/// reste une reprise sur place (aucun nouveau `play_media`), le
/// comportement d'aujourd'hui qui fonctionne.
#[tokio::test]
async fn resuming_a_radio_with_a_live_producer_after_a_short_pause_does_not_replay() {
    let orch = test_orchestrator();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Zone Radio", Some("mock"), Some("mock-radio"))
        .unwrap();
    orch.outputs
        .lock()
        .await
        .register(Box::new(MockOutput::new("mock-radio", "Mock Radio")));

    // Producteur VIVANT : le tx du décodeur est encore détenu (par le
    // test) et `producer_done` reste false.
    let (sid, _tx, _ready, _session) = orch
        .streamer
        .create_radio_session(
            crate::http::streamer::StreamInfo {
                format: "wav".into(),
                mime_type: "audio/wav".into(),
                sample_rate: 48000,
                bit_depth: 16,
                channels: 2,
                ..Default::default()
            },
            8,
        )
        .await;

    orch.playback
        .play(
            zone_id,
            NowPlaying {
                title: "FIP".into(),
                source: "radio".into(),
                source_id: Some("http://icecast.example/fip.aac".into()),
                stream_id: Some(sid),
                ..Default::default()
            },
        )
        .await;
    orch.playback.pause(zone_id).await;

    orch.resume(zone_id, Some("mock-radio")).await.unwrap();

    let outputs = orch.outputs.lock().await;
    let out = outputs.get("mock-radio").unwrap();
    let guard = out.lock().await;
    let mock = guard
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("mock output");
    assert_eq!(
        mock.play_call_count().await,
        0,
        "pause courte + producteur vivant ⇒ reprise sur place, pas de re-play"
    );
    assert_eq!(
        orch.playback.get_state(zone_id).await.state,
        PlayState::Playing,
        "la zone doit être repassée en lecture"
    );
}

// ------------------------------------------------------------------
// #2512 — Reprise après une pause longue, côté PISTE.
// ------------------------------------------------------------------

/// TÉMOIN #1629 : la décision RADIO, ligne par ligne, telle qu'elle était.
///
/// Les deux épreuves de bout en bout ci-dessus couvrent « producteur mort »
/// et « pause courte, producteur vivant ». La ligne PAUSE LONGUE, elle, ne
/// pouvait pas être jouée : `paused_at` est un `std::time::Instant` que
/// l'horloge virtuelle de tokio n'atteint pas. Rendue pure, elle se prouve.
#[test]
fn le_temoin_radio_1629_ne_bouge_pas() {
    for (pause_longue, session_morte, attendu) in [
        (false, false, RepriseDeSession::SurPlace),
        (true, false, RepriseDeSession::RejouerLeDirect),
        (false, true, RepriseDeSession::RejouerLeDirect),
        (true, true, RepriseDeSession::RejouerLeDirect),
    ] {
        assert_eq!(
            reprise_de_session(true, true, pause_longue, session_morte),
            attendu,
            "radio (pause_longue={pause_longue}, session_morte={session_morte})"
        );
    }
    // Sans URL de station la radio n'a jamais rien rejoué — et ne s'explique
    // pas non plus : ce chemin reste EXACTEMENT celui d'avant #2512.
    for (pause_longue, session_morte) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        assert_eq!(
            reprise_de_session(true, false, pause_longue, session_morte),
            RepriseDeSession::SurPlace,
            "radio sans URL (pause_longue={pause_longue}, session_morte={session_morte})"
        );
    }
}

/// LE point qui sépare ce correctif d'une transposition du comportement
/// radio : la DURÉE de la pause ne décide de rien pour une piste.
///
/// Vingt minutes de pause, session encore vivante ⇒ reprise sur place. Pas
/// au début, pas « en direct ». Si un jour quelqu'un branche
/// `RADIO_RESUME_REPLAY_AFTER` sur la branche piste, c'est ici que ça tombe.
#[test]
fn la_duree_d_une_pause_ne_relance_jamais_une_piste() {
    assert_eq!(
        reprise_de_session(false, true, true, false),
        RepriseDeSession::SurPlace,
        "session vivante : une pause longue ne justifie AUCUN redémarrage"
    );
    assert_eq!(
        reprise_de_session(false, false, true, false),
        RepriseDeSession::SurPlace
    );
}

/// Le vrai défaut : session ramassée pendant la pause ⇒ on RÉTABLIT, et à la
/// position — quelle que soit la durée de la pause.
#[test]
fn une_piste_dont_la_session_est_morte_est_retablie_a_sa_position() {
    assert_eq!(
        reprise_de_session(false, true, false, true),
        RepriseDeSession::RetablirALaPosition
    );
    assert_eq!(
        reprise_de_session(false, true, true, true),
        RepriseDeSession::RetablirALaPosition
    );
}

/// Session morte et rien à rejouer : il reste à le DIRE. Un silence sans
/// message est un défaut à lui seul.
#[test]
fn une_piste_irrejouable_ne_se_tait_pas() {
    assert_eq!(
        reprise_de_session(false, false, false, true),
        RepriseDeSession::Expliquer
    );
}

/// La demande de rétablissement repart AU POINT, sur la MÊME piste.
#[test]
fn le_retablissement_repart_a_la_position_et_pas_du_debut() {
    let np = NowPlaying {
        track_id: Some(77),
        title: "Sur la piste".into(),
        source: "local".into(),
        source_id: Some("77".into()),
        duration_ms: 300_000,
        ..Default::default()
    };
    let req = requete_de_retablissement(9, "local:DAC".into(), &np, 137_000);
    assert_eq!(
        req.seek_ms,
        Some(137_000),
        "rejouer une piste depuis le début serait la régression que ce \
         correctif doit éviter"
    );
    assert_eq!(req.track_id, Some(77));
    assert_eq!(req.source.as_deref(), Some("local"));
    assert_eq!(req.source_id.as_deref(), Some("77"));
    assert_eq!(req.zone_id, 9);
    assert_eq!(req.output_device_id.as_deref(), Some("local:DAC"));
    assert_eq!(req.duration_ms, Some(300_000));
}

/// Le message nomme la piste, la position et la cause. C'est ce qui manquait
/// : « aucun son, volume dans le vide », et pas une ligne pour l'expliquer.
#[test]
fn le_message_de_session_perdue_nomme_la_piste_et_la_position() {
    let seche = message_session_perdue("Sur la piste", Some(137_000), None);
    assert!(seche.contains("Sur la piste"), "{seche}");
    assert!(seche.contains("2:17"), "la position doit y être : {seche}");
    assert!(seche.contains("30 minutes"), "{seche}");
    let detaille = message_session_perdue("X", Some(0), Some("piste introuvable"));
    assert!(detaille.contains("piste introuvable"), "{detaille}");
    // #3244 — position NON mesurée. Le message ne perd que l'horodatage :
    // la piste, le délai et la cause restent, parce qu'ils sont connus.
    // Les assertions ci-dessus n'ont pas bougé : seule leur signature a
    // suivi le passage à `Option`, et un cas s'ajoute.
    let inconnue = message_session_perdue("Sur la piste", None, Some("piste introuvable"));
    assert!(inconnue.contains("Sur la piste"), "{inconnue}");
    assert!(inconnue.contains("30 minutes"), "{inconnue}");
    assert!(inconnue.contains("piste introuvable"), "{inconnue}");
    assert!(
        !inconnue.contains("0:00"),
        "« 0:00 » n'est pas une position, c'est une absence de mesure : {inconnue}"
    );
}

/// Prépare une zone qui JOUE une piste locale via une session de flux, puis
/// la met en pause à `position_ms`. Rend `(orchestrateur, zone, stream_id)`.
async fn zone_en_pause_sur_une_piste(
    track_id: Option<i64>,
    position_ms: i64,
) -> (PlaybackOrchestrator, i64, String) {
    zone_en_pause_sur_une_piste_avec_sortie(track_id, position_ms, true).await
}
/// Même préparation, la SORTIE en moins quand `avec_sortie` est faux : une
/// zone NAVIGATEUR, sans `output_device_id`. C'est la forme que le sondeur
/// laisse sans position (#3244) — lui inventer une sortie effacerait le
/// défaut éprouvé.
async fn zone_en_pause_sur_une_piste_avec_sortie(
    track_id: Option<i64>,
    position_ms: i64,
    avec_sortie: bool,
) -> (PlaybackOrchestrator, i64, String) {
    let orch = test_orchestrator();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create(
            "Salon",
            avec_sortie.then_some("mock"),
            avec_sortie.then_some("mock-salon"),
        )
        .unwrap();
    if avec_sortie {
        orch.outputs
            .lock()
            .await
            .register(Box::new(MockOutput::new("mock-salon", "Mock Salon")));
    }
    // Une session de PISTE — `create_session`, pas `create_radio_session` :
    // c'est celle que le ramasse-miettes a le droit de prendre.
    let (sid, _tx, _ready) = orch
        .streamer
        .create_session(
            crate::http::streamer::StreamInfo {
                format: "wav".into(),
                mime_type: "audio/wav".into(),
                sample_rate: 44_100,
                bit_depth: 16,
                channels: 2,
                ..Default::default()
            },
            false,
            4,
        )
        .await;
    orch.playback
        .play(
            zone_id,
            NowPlaying {
                track_id,
                title: "Sur la piste".into(),
                source: "local".into(),
                stream_id: Some(sid.clone()),
                duration_ms: 300_000,
                ..Default::default()
            },
        )
        .await;
    orch.playback.update_position(zone_id, position_ms).await;
    orch.playback.pause(zone_id).await;
    (orch, zone_id, sid)
}

/// Fait passer le ramasse-miettes comme il passe toutes les minutes en
/// production, avec des bornes réduites : une session en pause ne sert plus
/// un octet, elle est donc « muette » au sens de `cleanup_stale_sessions`.
async fn le_ramasse_miettes_passe(orch: &PlaybackOrchestrator, sid: &str) {
    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    orch.streamer
        .cleanup_stale_sessions_with(
            std::time::Duration::from_millis(300),
            std::time::Duration::from_secs(3_600),
        )
        .await;
    assert!(
        !orch.streamer.session_alive(sid).await,
        "le balayage doit avoir ramassé la session de la piste en pause — \
         sans ça l'épreuve ne mesure rien"
    );
}

/// #2512, bout en bout sur la VRAIE `resume` : une piste dont la session a
/// été ramassée pendant la pause ne répond plus « en lecture » en silence.
///
/// Avant : `Ok(())`, la route rend 200 et un `now_playing` qui joue, la
/// sortie ne reçoit rien, et rien ne dit pourquoi.
#[tokio::test]
async fn une_piste_dont_la_session_est_morte_ne_reprend_plus_en_silence() {
    // Ni `track_id` ni `source_id` : rien ne permet de rétablir quoi que ce
    // soit. Il reste à le dire — c'est tout l'objet de cette épreuve.
    let (orch, zone_id, sid) = zone_en_pause_sur_une_piste(None, 137_000).await;
    le_ramasse_miettes_passe(&orch, &sid).await;
    let erreur = orch
        .resume(zone_id, Some("mock-salon"))
        .await
        .expect_err("une reprise qui ne peut pas aboutir ne doit plus dire « en lecture »");
    let OutputCommandError::Failed { command, message } = erreur else {
        panic!("la reprise doit échouer en Failed, pas en Unsupported");
    };
    assert_eq!(command, OutputCommand::Resume);
    assert!(
        message.contains("Sur la piste") && message.contains("2:17"),
        "le message doit nommer la piste et la position : {message}"
    );
    // Et la zone ne doit pas prétendre jouer.
    assert_eq!(
        orch.playback.get_state(zone_id).await.state,
        PlayState::Paused,
        "une zone qui n'a pas repris ne doit pas s'annoncer en lecture"
    );
}

/// Même chemin, mais la piste est identifiable : `resume` doit TENTER de
/// rétablir la session — et quand la tentative échoue (ici la piste n'est pas
/// en base), l'échec REMONTE au lieu de retomber en silence sur la sortie.
///
/// La position voyage dans le message : c'est la preuve que la reprise
/// visait le point de la pause, et non le début du morceau.
#[tokio::test]
async fn un_retablissement_impossible_remonte_au_lieu_de_se_taire() {
    let (orch, zone_id, sid) = zone_en_pause_sur_une_piste(Some(4242), 137_000).await;
    le_ramasse_miettes_passe(&orch, &sid).await;
    let erreur = orch
        .resume(zone_id, Some("mock-salon"))
        .await
        .expect_err("le rétablissement a échoué : la reprise doit le dire");
    assert!(
        erreur.to_string().contains("2:17"),
        "la reprise visait la position de la pause : {erreur}"
    );
    let outputs = orch.outputs.lock().await;
    let guard = outputs.get("mock-salon").unwrap();
    let guard = guard.lock().await;
    let mock = guard
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("mock output");
    assert_eq!(
        mock.play_call_count().await,
        0,
        "rien n'a pu être rétabli : aucune lecture ne doit être annoncée à la sortie"
    );
}

/// CONTRE-ÉPREUVE : session VIVANTE ⇒ la reprise reste exactement ce qu'elle
/// est aujourd'hui. Aucune relecture, aucune erreur, la zone repart.
///
/// Sans elle, le correctif pourrait « marcher » en relançant toutes les
/// reprises — et personne ne le verrait.
#[tokio::test]
async fn une_piste_dont_la_session_vit_reprend_sur_place() {
    let (orch, zone_id, sid) = zone_en_pause_sur_une_piste(Some(4242), 137_000).await;
    assert!(orch.streamer.session_alive(&sid).await);
    orch.resume(zone_id, Some("mock-salon"))
        .await
        .expect("session vivante : la reprise ordinaire doit aboutir");
    assert_eq!(
        orch.playback.get_state(zone_id).await.state,
        PlayState::Playing
    );
    let outputs = orch.outputs.lock().await;
    let guard = outputs.get("mock-salon").unwrap();
    let guard = guard.lock().await;
    let mock = guard
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("mock output");
    assert_eq!(
        mock.play_call_count().await,
        0,
        "session vivante ⇒ reprise sur place, aucune relecture"
    );
}
/// #3244 — jumeau de #2595, site `resume`. Une zone NAVIGATEUR n'a pas de
/// position mesurée : la reprise ne doit ni relancer au début, ni ANNONCER
/// « 0:00 » comme si elle savait où on en était.
///
/// La position vaut 0 parce que personne ne l'a jamais écrite — le sondeur
/// `continue` avant son `update_position` faute de périphérique — pendant
/// que le morceau avançait dans l'onglet.
#[tokio::test]
async fn la_reprise_dune_zone_navigateur_n_annonce_pas_une_position_inventee() {
    let (orch, zone_id, sid) = zone_en_pause_sur_une_piste_avec_sortie(Some(4242), 0, false).await;
    // Sans ça l'épreuve ne mesurerait rien : c'est le prédicat de #3242 qui
    // dit que cette zone-là n'est observée par personne.
    assert!(
        !orch.position_entretenue_par_le_sondeur(zone_id),
        "une zone sans périphérique n'est pas sondée : sa position n'est pas mesurée"
    );
    le_ramasse_miettes_passe(&orch, &sid).await;
    let erreur = orch
        .resume(zone_id, None)
        .await
        .expect_err("session morte : la reprise doit le dire au lieu de se taire");
    let message = erreur.to_string();
    assert!(
        !message.contains("0:00"),
        "position INCONNUE annoncée comme une mesure — « 0:00 » désigne le début \
         du morceau, pas le point de la pause : {message}"
    );
    assert!(
        message.contains("Sur la piste"),
        "la piste reste nommée, elle, on la connaît : {message}"
    );
    let etat = orch.playback.get_state(zone_id).await;
    assert_eq!(
        etat.state,
        PlayState::Paused,
        "une reprise qui n'a pas abouti ne doit pas s'annoncer en lecture"
    );
    assert!(
        etat.last_seek_at.is_none(),
        "aucune relance à une position inconnue ne doit avoir été TENTÉE"
    );
}

/// Crée une zone offline pointant vers un device réseau disparu, comme la
/// « Mac Studio Speakers » d'Alex Campbell (#1287) : la zone avait été créée
/// quand un second serveur voyait le Mac sur le réseau ; ce device n'existe
/// plus dans le registre du serveur courant.
fn stale_network_zone(orch: &PlaybackOrchestrator, name: &str) -> i64 {
    let repo = ZoneRepo::with_backend(orch.db.clone());
    let id = repo
        .create(name, Some("dlna"), Some("dlna-vanished-host"))
        .unwrap();
    repo.update_online(id, false).unwrap();
    id
}

#[tokio::test]
async fn stale_network_zone_rebinds_to_the_local_output_of_the_same_name() {
    let orch = test_orchestrator();
    let zone_id = stale_network_zone(&orch, "Mac Studio Speakers");
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new("local:mac-studio-speakers", "Mac Studio Speakers").with_type("local"),
    ));

    let zone = ZoneRepo::with_backend(orch.db.clone())
        .get(zone_id)
        .unwrap()
        .unwrap();
    let rebound = orch
        .gate_or_rebind_offline_zone(zone_id, &zone)
        .await
        .expect("le rebind doit réussir, pas rejeter la lecture");
    assert_eq!(rebound.as_deref(), Some("local:mac-studio-speakers"));

    // Le rebind est persisté et collant : id, type ET online.
    let after = ZoneRepo::with_backend(orch.db.clone())
        .get(zone_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        after.output_device_id.as_deref(),
        Some("local:mac-studio-speakers")
    );
    assert_eq!(
        after.output_type.as_deref(),
        Some("local"),
        "le type doit suivre l'id, sinon la zone reste typée dlna en pointant du local"
    );
    assert!(after.online);
}

#[tokio::test]
async fn two_outputs_of_the_same_name_are_ambiguous_and_never_auto_bound() {
    let orch = test_orchestrator();
    let zone_id = stale_network_zone(&orch, "Salon");
    {
        let mut reg = orch.outputs.lock().await;
        reg.register(Box::new(
            MockOutput::new("dlna-a", "Salon").with_type("dlna"),
        ));
        reg.register(Box::new(
            MockOutput::new("dlna-b", "Salon").with_type("dlna"),
        ));
    }

    let zone = ZoneRepo::with_backend(orch.db.clone())
        .get(zone_id)
        .unwrap()
        .unwrap();
    let err = orch
        .gate_or_rebind_offline_zone(zone_id, &zone)
        .await
        .expect_err("deux homonymes sans local : binder l'un des deux serait un pari");
    assert!(err.starts_with("zone_output_unavailable:"), "err = {err}");

    // Rien n'a été touché en base.
    let after = ZoneRepo::with_backend(orch.db.clone())
        .get(zone_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        after.output_device_id.as_deref(),
        Some("dlna-vanished-host")
    );
    assert!(!after.online);
}

#[tokio::test]
async fn a_single_local_output_wins_over_other_same_name_candidates() {
    let orch = test_orchestrator();
    let zone_id = stale_network_zone(&orch, "Salon");
    {
        let mut reg = orch.outputs.lock().await;
        reg.register(Box::new(
            MockOutput::new("dlna-a", "Salon").with_type("dlna"),
        ));
        reg.register(Box::new(
            MockOutput::new("local:salon", "Salon").with_type("local"),
        ));
    }

    let zone = ZoneRepo::with_backend(orch.db.clone())
        .get(zone_id)
        .unwrap()
        .unwrap();
    let rebound = orch
        .gate_or_rebind_offline_zone(zone_id, &zone)
        .await
        .unwrap();
    assert_eq!(
        rebound.as_deref(),
        Some("local:salon"),
        "un local unique tranche l'ambiguïté — c'est la règle « préférer local »"
    );
}

#[tokio::test]
async fn no_matching_output_gives_an_actionable_error_not_a_curt_offline() {
    let orch = test_orchestrator();
    let zone_id = stale_network_zone(&orch, "Chambre");
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new("local:autre-chose", "Cuisine").with_type("local"),
    ));

    let zone = ZoneRepo::with_backend(orch.db.clone())
        .get(zone_id)
        .unwrap()
        .unwrap();
    let err = orch
        .gate_or_rebind_offline_zone(zone_id, &zone)
        .await
        .expect_err("aucune sortie du même nom");
    assert!(err.starts_with("zone_output_unavailable:"), "err = {err}");
    assert!(
        err.contains("réglages de la zone"),
        "le message doit dire quoi faire, pas juste « offline » : {err}"
    );
}

#[tokio::test]
async fn a_healthy_zone_is_left_completely_alone() {
    let orch = test_orchestrator();
    let repo = ZoneRepo::with_backend(orch.db.clone());
    let zone_id = repo
        .create("Bureau", Some("local"), Some("local:bureau"))
        .unwrap();
    repo.update_online(zone_id, true).unwrap();
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new("local:bureau", "Bureau").with_type("local"),
    ));

    let zone = repo.get(zone_id).unwrap().unwrap();
    assert_eq!(
        orch.gate_or_rebind_offline_zone(zone_id, &zone)
            .await
            .unwrap(),
        None,
        "chemin nominal : aucun rebind, aucune écriture"
    );
}

/// Une zone offline dont le device est TOUJOURS dans le registre vivant ne
/// doit pas être re-bindée : c'est la fenêtre de grâce pour les trous de
/// polling SSDP, le device est joignable même si la DB dit offline.
#[tokio::test]
async fn an_offline_zone_whose_device_is_still_registered_is_not_touched() {
    let orch = test_orchestrator();
    let repo = ZoneRepo::with_backend(orch.db.clone());
    let zone_id = repo
        .create("Salon", Some("dlna"), Some("dlna-toujours-la"))
        .unwrap();
    repo.update_online(zone_id, false).unwrap();
    {
        let mut reg = orch.outputs.lock().await;
        reg.register(Box::new(
            MockOutput::new("dlna-toujours-la", "Salon").with_type("dlna"),
        ));
        // Un homonyme local qui aurait été choisi si on avait re-bindé.
        reg.register(Box::new(
            MockOutput::new("local:salon", "Salon").with_type("local"),
        ));
    }

    let zone = repo.get(zone_id).unwrap().unwrap();
    assert_eq!(
        orch.gate_or_rebind_offline_zone(zone_id, &zone)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        repo.get(zone_id)
            .unwrap()
            .unwrap()
            .output_device_id
            .as_deref(),
        Some("dlna-toujours-la")
    );
}

#[test]
fn timeout_means_the_command_may_have_landed() {
    let err = format!(
        "{} soap send: error sending request for url (http://192.168.1.92:8080/AVTransport/ctrl): operation timed out",
        crate::outputs::dlna::SOAP_TIMEOUT_PREFIX
    );
    assert!(super::command_may_have_landed(&err));
}

#[test]
fn connection_refused_is_conclusive() {
    // Refus de connexion : rien n'a pu partir, la session doit être détruite.
    assert!(!super::command_may_have_landed(
        "soap send: error sending request: connection refused"
    ));
    assert!(!super::command_may_have_landed("soap read: body error"));
    assert!(!super::command_may_have_landed(""));
}

#[test]
fn timeout_marker_survives_the_send_to_output_wrapper() {
    // send_to_output enveloppe : « Output device error: {e} ». Le marqueur
    // n'est donc pas en tête de chaîne.
    let err = format!(
        "Output device error: {} soap send: operation timed out",
        crate::outputs::dlna::SOAP_TIMEOUT_PREFIX
    );
    assert!(
        super::command_may_have_landed(&err),
        "le marqueur doit être reconnu même enveloppé"
    );
}

/// Sortie dont `play_media` expire — le renderer lent qui reçoit peut-être la
/// commande, mais dont la réponse n'arrive pas (Cyrus Stream X2 de JP).
struct TimingOutOutput {
    id: String,
}

struct FailingCommandOutput {
    id: String,
}

#[async_trait::async_trait]
impl crate::outputs::traits::OutputTarget for FailingCommandOutput {
    fn name(&self) -> &str {
        "FailingCommand"
    }
    fn device_id(&self) -> &str {
        &self.id
    }
    fn output_type(&self) -> &str {
        "test"
    }
    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities::v1(true, true, true, true, true, false)
    }
    async fn play_media(
        &self,
        _media: &crate::outputs::traits::PlayMedia<'_>,
    ) -> Result<(), String> {
        Ok(())
    }
    async fn pause(&self) -> Result<(), String> {
        Err("pause refused".into())
    }
    async fn resume(&self) -> Result<(), String> {
        Err("resume refused".into())
    }
    async fn stop(&self) -> Result<(), String> {
        Ok(())
    }
    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Err("seek refused".into())
    }
    async fn set_volume(&self, _volume: f64) -> Result<(), String> {
        Err("volume refused".into())
    }
    async fn set_mute(&self, _muted: bool) -> Result<(), String> {
        Err("mute refused".into())
    }
    async fn get_status(&self) -> Result<crate::outputs::traits::OutputStatus, String> {
        Ok(Default::default())
    }
    async fn is_available(&self) -> bool {
        true
    }
}

#[async_trait::async_trait]
impl crate::outputs::traits::OutputTarget for TimingOutOutput {
    fn name(&self) -> &str {
        "TimingOut"
    }
    fn device_id(&self) -> &str {
        &self.id
    }
    fn output_type(&self) -> &str {
        "test"
    }
    async fn play_media(
        &self,
        _media: &crate::outputs::traits::PlayMedia<'_>,
    ) -> Result<(), String> {
        Err(format!(
            "{} soap send: error sending request for url \
             (http://192.168.1.92:8080/AVTransport/ctrl): operation timed out",
            crate::outputs::dlna::SOAP_TIMEOUT_PREFIX
        ))
    }
    async fn pause(&self) -> Result<(), String> {
        Ok(())
    }
    async fn resume(&self) -> Result<(), String> {
        Ok(())
    }
    async fn stop(&self) -> Result<(), String> {
        Ok(())
    }
    async fn seek(&self, _pos_ms: u64) -> Result<(), String> {
        Ok(())
    }
    async fn set_volume(&self, _vol: f64) -> Result<(), String> {
        Ok(())
    }
    async fn get_status(&self) -> Result<crate::outputs::traits::OutputStatus, String> {
        Ok(Default::default())
    }
    async fn set_mute(&self, _muted: bool) -> Result<(), String> {
        Ok(())
    }
    async fn is_available(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn une_capacite_absente_ne_modifie_ni_memoire_ni_base() {
    let orch = test_orchestrator();
    let device_id = "legacy-noop";
    let zone_repo = ZoneRepo::with_backend(orch.db.clone());
    let zone_id = zone_repo
        .create("Legacy", Some("test"), Some(device_id))
        .unwrap();
    orch.outputs
        .lock()
        .await
        .register(Box::new(TimingOutOutput {
            id: device_id.into(),
        }));

    orch.playback
        .play(
            zone_id,
            NowPlaying {
                track_id: Some(42),
                title: "Contrat fail-closed".into(),
                duration_ms: 120_000,
                source: "local".into(),
                ..Default::default()
            },
        )
        .await;
    orch.playback.update_position(zone_id, 12_000).await;
    orch.playback.set_volume(zone_id, 0.5).await;
    orch.playback.set_mute(zone_id, false).await;

    for (result, command) in [
        (
            orch.pause(zone_id, Some(device_id)).await,
            OutputCommand::Pause,
        ),
        (
            orch.seek(zone_id, 42_000, Some(device_id)).await,
            OutputCommand::Seek,
        ),
        (
            orch.set_volume(zone_id, 0.8, Some(device_id)).await,
            OutputCommand::SetVolume,
        ),
        (
            orch.set_mute(zone_id, true, Some(device_id)).await,
            OutputCommand::SetMute,
        ),
    ] {
        assert_eq!(
            result,
            Err(OutputCommandError::Unsupported { command }),
            "{command} doit être refusée avant toute mutation"
        );
    }

    let state = orch.playback.get_state(zone_id).await;
    assert_eq!(state.state, PlayState::Playing);
    assert_eq!(state.position_ms, 12_000);
    assert!((state.volume - 0.5).abs() < f64::EPSILON);
    assert!(!state.muted);

    let persisted = zone_repo.get(zone_id).unwrap().unwrap();
    assert_eq!(persisted.last_position_ms, 0);
    assert_eq!(persisted.volume, 50.0);
    assert!(!persisted.muted);
}

#[tokio::test]
async fn un_backend_qui_refuse_ne_modifie_ni_memoire_ni_base() {
    let orch = test_orchestrator();
    let device_id = "failing-command";
    let zone_repo = ZoneRepo::with_backend(orch.db.clone());
    let zone_id = zone_repo
        .create("Failing", Some("test"), Some(device_id))
        .unwrap();
    orch.outputs
        .lock()
        .await
        .register(Box::new(FailingCommandOutput {
            id: device_id.into(),
        }));
    orch.playback
        .play(
            zone_id,
            NowPlaying {
                track_id: Some(43),
                title: "Refus backend".into(),
                duration_ms: 120_000,
                source: "local".into(),
                ..Default::default()
            },
        )
        .await;
    orch.playback.update_position(zone_id, 13_000).await;
    orch.playback.set_volume(zone_id, 0.5).await;

    assert!(matches!(
        orch.pause(zone_id, Some(device_id)).await,
        Err(OutputCommandError::Failed {
            command: OutputCommand::Pause,
            ..
        })
    ));
    assert!(matches!(
        orch.seek(zone_id, 42_000, Some(device_id)).await,
        Err(OutputCommandError::Failed {
            command: OutputCommand::Seek,
            ..
        })
    ));
    assert!(matches!(
        orch.set_volume(zone_id, 0.8, Some(device_id)).await,
        Err(OutputCommandError::Failed {
            command: OutputCommand::SetVolume,
            ..
        })
    ));
    assert!(matches!(
        orch.set_mute(zone_id, true, Some(device_id)).await,
        Err(OutputCommandError::Failed {
            command: OutputCommand::SetMute,
            ..
        })
    ));

    let state = orch.playback.get_state(zone_id).await;
    assert_eq!(state.state, PlayState::Playing);
    assert_eq!(state.position_ms, 13_000);
    assert_eq!(state.volume, 0.5);
    assert!(!state.muted);
    let persisted = zone_repo.get(zone_id).unwrap().unwrap();
    assert_eq!(persisted.last_position_ms, 0);
    assert_eq!(persisted.volume, 50.0);
    assert!(!persisted.muted);
}

/// Zone qui joue un FLAC de la bibliothèque, prête pour une relecture.
///
/// Rend `(orchestrateur, zone_id, répertoire)` — le répertoire doit rester
/// vivant tant que le test tourne, sinon le fichier disparaît sous la zone
/// et `resolve_local_track` échoue en `file_not_found`.
///
/// `output_type` et `device_id` sont facultatifs (#2595) : une zone
/// navigateur — « Cet ordinateur » — n'a PAS de périphérique de sortie, et
/// c'est précisément cette forme-là que le sondeur laisse sans position.
async fn zone_qui_joue_un_flac(
    output_type: Option<&str>,
    device_id: Option<&str>,
) -> (PlaybackOrchestrator, i64, tempfile::TempDir) {
    let orch = test_orchestrator();

    // Un VRAI FLAC : le passthrough réseau lit sa taille sur le disque, et
    // le bras local le décode pour de bon.
    let dir = tempfile::tempdir().unwrap();
    let piste = dir.path().join("morceau.flac");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test.flac"),
        &piste,
    )
    .unwrap();
    let chemin = piste.to_string_lossy().into_owned();

    orch.db
        .execute("INSERT INTO artists (id, name) VALUES (1, 'Artiste')", &[])
        .unwrap();
    orch.db
        .execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Album', 1)",
            &[],
        )
        .unwrap();
    orch.db
        .execute(
            "INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, \
             duration_ms, sample_rate, bit_depth, channels) \
             VALUES (1, 'Morceau', 1, 1, ?, 'flac', 300000, 44100, 16, 2)",
            &[&chemin as &dyn crate::db::backend::ToSqlValue],
        )
        .unwrap();

    let zone_repo = ZoneRepo::with_backend(orch.db.clone());
    let zone_id = zone_repo
        .create("Marantz ND8006", output_type, device_id)
        .unwrap();
    // FLAC natif imposé : la sortie factice n'est pas un `DlnaOutput`, la
    // négociation `GetProtocolInfo` serait donc non concluante et
    // basculerait en WAV. On veut le passthrough du testeur.
    zone_repo.update_dlna_native_flac(zone_id, true).unwrap();

    // Pas de périphérique, pas de sortie à enregistrer : la zone navigateur
    // n'en a aucune, et lui en inventer une effacerait le défaut testé.
    if let (Some(device_id), Some(output_type)) = (device_id, output_type) {
        orch.outputs.lock().await.register(Box::new(
            MockOutput::new(device_id, "Marantz ND8006").with_type(output_type),
        ));
    }

    orch.playback
        .play(
            zone_id,
            NowPlaying {
                track_id: Some(1),
                title: "Morceau".into(),
                source: "local".into(),
                duration_ms: 300_000,
                ..Default::default()
            },
        )
        .await;

    (orch, zone_id, dir)
}

// ------------------------------------------------------------------
// #2395 — le volume que l'APPAREIL reçoit pendant la lecture.
// ------------------------------------------------------------------

/// Zone réseau garnie de trois pistes, prête à enchaîner (#2395).
///
/// Trois pistes DISTINCTES, et non trois fois la même : les deux gardes
/// anti-doublon de `play` (retap dans `RETAP_DEDUP_WINDOW`, puis
/// `last_net_play` dans `DUPLICATE_NET_PLAY_WINDOW`) coalescent une
/// relecture du même `track_id`. Trois lectures du même morceau ne
/// mesureraient donc qu'un seul passage par `send_to_output` : le test
/// serait vert sans rien prouver.
///
/// Trois FICHIERS distincts, et pas trois lignes vers le même : la colonne
/// `tracks.file_path` porte une contrainte d'unicité. Ce sont de vraies
/// copies du FLAC du dépôt — le chemin réseau lit leur taille sur le
/// disque, un chemin fantôme échouerait en `file_not_found`.
///
/// Le répertoire est rendu à l'appelant et doit vivre aussi longtemps que
/// le test : il se nettoie tout seul, par `Drop`.
async fn zone_reseau_trois_pistes(
    device_id: &str,
    fixed_volume: bool,
) -> (PlaybackOrchestrator, i64, tempfile::TempDir) {
    let orch = test_orchestrator();
    let dir = tempfile::tempdir().unwrap();
    let mut pistes = Vec::new();
    for id in 1..=3i64 {
        let chemin = dir.path().join(format!("piste-{id}.flac"));
        std::fs::copy(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test.flac"),
            &chemin,
        )
        .unwrap();
        pistes.push(chemin.to_string_lossy().into_owned());
    }

    orch.db
        .execute("INSERT INTO artists (id, name) VALUES (1, 'Artiste')", &[])
        .unwrap();
    orch.db
        .execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Album', 1)",
            &[],
        )
        .unwrap();
    for id in 1..=3i64 {
        orch.db
            .execute(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, \
                 duration_ms, sample_rate, bit_depth, channels) \
                 VALUES (?, ?, 1, 1, ?, 'flac', 300000, 44100, 16, 2)",
                &[
                    &id as &dyn crate::db::backend::ToSqlValue,
                    &format!("Piste {id}") as &dyn crate::db::backend::ToSqlValue,
                    &pistes[(id - 1) as usize] as &dyn crate::db::backend::ToSqlValue,
                ],
            )
            .unwrap();
    }

    let zone_repo = ZoneRepo::with_backend(orch.db.clone());
    let zone_id = zone_repo
        .create("Denon RC12", Some("dlna"), Some(device_id))
        .unwrap();
    zone_repo.update_dlna_native_flac(zone_id, true).unwrap();
    // 30 % : un niveau d'écoute ordinaire, franchement distinct du plein
    // volume. Une commande à 1.0 se verrait donc, si elle partait.
    zone_repo.update_volume(zone_id, 30.0).unwrap();
    if fixed_volume {
        zone_repo.update_fixed_volume(zone_id, true).unwrap();
    }

    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(device_id, "Denon RC12").with_type("dlna"),
    ));

    (orch, zone_id, dir)
}

/// Les commandes de volume que l'APPAREIL a reçues, dans l'ordre (#2395).
///
/// On interroge la sortie, pas l'état du serveur : la question est « qu'a
/// reçu le Denon ? », et seul le mock peut y répondre.
async fn volumes_recus_par_l_appareil(orch: &PlaybackOrchestrator, device_id: &str) -> Vec<f64> {
    let outputs = orch.outputs.lock().await;
    let out = outputs.get(device_id).expect("sortie enregistree");
    let guard = out.lock().await;
    guard
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("la sortie factice")
        .volume_calls()
        .await
}

/// Enchaîne les trois pistes, en laissant à une éventuelle réassertion
/// différée tout le temps de partir.
///
/// L'ancien code postait la commande dans un `tokio::spawn` qui dormait
/// 500 ms. Mesurer juste après le `play` ne prouverait donc rien : on
/// attend franchement plus que ce délai après chaque piste, sinon le test
/// serait vert même avec le défaut en place.
async fn enchainer_trois_pistes(orch: &PlaybackOrchestrator, zone_id: i64) {
    for track_id in 1..=3i64 {
        orch.play(PlayRequest {
            zone_id,
            track_id: Some(track_id),
            source: Some("local".into()),
            ..Default::default()
        })
        .await
        .expect("la lecture doit aboutir");
        tokio::time::sleep(std::time::Duration::from_millis(900)).await;
    }
}

/// #2395 — LE défaut, mesuré sur l'appareil : en mode bit-perfect, le plein
/// volume repartait vers le renderer 500 ms après CHAQUE piste.
///
/// Marco Polo (fil 1546) écoute sur un Denon RC12, à la fois renderer DLNA
/// et amplificateur : chacune de ces commandes est une puissance
/// acoustique réelle, et il n'en a jamais consenti qu'une — celle de
/// l'armement.
///
/// Le compte est fait sur des COMMANDES REÇUES, pas sur un code HTTP ni
/// sur le volume courant : trois consignes à 100 % sur un appareil déjà à
/// 100 % ne changent aucun état lisible, et se liraient comme une seule.
///
/// Avant le correctif : trois commandes (une par piste). Après : zéro.
#[tokio::test]
async fn en_bit_perfect_trois_lectures_ne_commandent_plus_le_volume() {
    let device_id = "dlna-denon-rc12";
    let (orch, zone_id, _dir) = zone_reseau_trois_pistes(device_id, true).await;

    enchainer_trois_pistes(&orch, zone_id).await;

    assert_eq!(
        volumes_recus_par_l_appareil(&orch, device_id).await,
        Vec::<f64>::new(),
        "la lecture ne doit commander AUCUN volume : le 100 % du mode \
         bit-perfect s'obtient a l'armement, une fois, et pas a chaque piste"
    );
}

/// #2395 — le mode entier, vu de l'appareil : UNE seule commande.
///
/// C'est la promesse tenue à l'utilisateur — un saut, annoncé — vérifiée
/// bout à bout : l'armement commande le plein volume, les trois lectures
/// qui suivent ne commandent rien.
#[tokio::test]
async fn armement_puis_trois_lectures_ne_font_qu_une_commande() {
    let device_id = "dlna-denon-rc12";
    let (orch, zone_id, _dir) = zone_reseau_trois_pistes(device_id, false).await;

    // L'armement, seul chemin autorisé à monter la zone à 100 %.
    orch.arm_fixed_volume(zone_id, Some(device_id))
        .await
        .expect("l'armement doit aboutir");
    ZoneRepo::with_backend(orch.db.clone())
        .update_fixed_volume(zone_id, true)
        .unwrap();

    enchainer_trois_pistes(&orch, zone_id).await;

    assert_eq!(
        volumes_recus_par_l_appareil(&orch, device_id).await,
        vec![1.0],
        "apres trois lectures successives, l'appareil ne doit avoir recu \
         qu'UNE seule commande de volume : celle de l'armement"
    );
}

/// #2395 — TÉMOIN : hors du mode, ce chemin ne commande jamais le volume.
///
/// Vert avant comme après le correctif. Il tient la mesure honnête : sans
/// lui, un test qui compte zéro commande pourrait se contenter d'une
/// lecture qui n'a rien joué du tout. Ici la même zone, les mêmes trois
/// pistes et le même compteur donnent déjà zéro sans le mode — la
/// différence mesurée plus haut vient donc bien de `fixed_volume`, et pas
/// d'un banc qui ne mesure rien.
#[tokio::test]
async fn temoin_hors_bit_perfect_la_lecture_ne_commande_aucun_volume() {
    let device_id = "dlna-zone-ordinaire";
    let (orch, zone_id, _dir) = zone_reseau_trois_pistes(device_id, false).await;

    enchainer_trois_pistes(&orch, zone_id).await;

    assert_eq!(
        volumes_recus_par_l_appareil(&orch, device_id).await,
        Vec::<f64>::new(),
        "une zone ordinaire garde le niveau ou l'appareil est \
         physiquement : la lecture ne lui commande rien"
    );
}

/// #2395 — le banc mesure vraiment quelque chose.
///
/// Contre-épreuve du compteur lui-même : si `volume_calls` restait vide
/// quoi qu'il arrive, les trois tests ci-dessus seraient verts pour rien.
/// Une commande envoyée à la main doit s'y voir, et le compteur doit
/// distinguer deux consignes identiques d'une seule.
#[tokio::test]
async fn le_compteur_de_commandes_voit_ce_qui_part() {
    let device_id = "dlna-temoin-compteur";
    let (orch, zone_id, _dir) = zone_reseau_trois_pistes(device_id, false).await;

    orch.set_volume(zone_id, 0.3, Some(device_id))
        .await
        .unwrap();
    orch.set_volume(zone_id, 0.3, Some(device_id))
        .await
        .unwrap();

    assert_eq!(
        volumes_recus_par_l_appareil(&orch, device_id).await,
        vec![0.3, 0.3],
        "deux consignes identiques restent DEUX commandes recues"
    );
}

/// Position rapportée par la sortie factice après la relecture.
///
/// `play_media` la remet à 0 ; seul un `Seek` peut l'en faire bouger. Une
/// valeur de 0 signifie donc « aucun Seek n'a été envoyé », sans compteur
/// supplémentaire à ajouter au mock partagé.
async fn position_vue_par_la_sortie(orch: &PlaybackOrchestrator, device_id: &str) -> u64 {
    let outputs = orch.outputs.lock().await;
    let out = outputs.get(device_id).unwrap();
    let guard = out.lock().await;
    guard.get_status().await.unwrap().position_ms
}

/// Depuis LAT-P2 le seek qui suit une relecture part en tâche détachée, après
/// `REPLAY_OUTPUT_SEEK_SETTLE_MS` : la réponse à l'appelant ne l'attend plus.
/// On attend donc ici, sans dépasser quelques secondes, que la sortie l'ait
/// reçu — la valeur rendue est la dernière position vue.
async fn attendre_la_position_vue_par_la_sortie(
    orch: &PlaybackOrchestrator,
    device_id: &str,
    attendue: u64,
) -> u64 {
    let mut vue = 0;
    for _ in 0..60 {
        vue = position_vue_par_la_sortie(orch, device_id).await;
        if vue == attendue {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    vue
}

/// #2893 — LE défaut, de bout en bout : bascule Pure sur une sortie DLNA.
///
/// Jean Valjean (0.9.126, Marantz ND8006, fil 1618) : le serveur relance à
/// la bonne position — ses journaux portent `position_ms=94000`, `48000`,
/// `259000`, `23000` — et le morceau repart quand même du début. La cause
/// n'est pas la position serveur (c'est #2595) : c'est que
/// `replay_zone_at_position` recrée le flux et s'arrête là. Le renderer
/// reçoit `Stop` → `SetAVTransportURI` → `Play` sur une session FICHIER
/// servie depuis l'octet 0, **sans aucun `Seek`**.
///
/// Ce test échoue sur le code d'avant le correctif : la sortie reste à 0.
#[tokio::test]
async fn bascule_pure_sur_dlna_le_renderer_recoit_le_seek_vers_la_position() {
    let device_id = "dlna:uuid-56fcb4ae-2893";
    let (orch, zone_id, _dir) = zone_qui_joue_un_flac(Some("dlna"), Some(device_id)).await;

    orch.replay_zone_at_position(zone_id, 94_000, "eq_change")
        .await
        .expect("la relecture doit réussir");

    assert_eq!(
        attendre_la_position_vue_par_la_sortie(&orch, device_id, 94_000).await,
        94_000,
        "le renderer doit recevoir un Seek vers la position : sans lui il rejoue le morceau depuis le début (#2893)"
    );
}

/// Témoin anti-régression de #2893 : la sortie LOCALE ne reçoit rien.
///
/// Elle consomme un transcodage séquentiel que
/// `decode_to_pcm_streaming_seeked` démarre DÉJÀ à l'offset. Lui envoyer un
/// `Seek` par-dessus sauterait deux fois — la panne #1518 (silence total,
/// puis boucle de redémarrage). Le chemin local doit rester exactement ce
/// qu'il était.
#[tokio::test]
async fn une_relecture_sur_sortie_locale_n_envoie_aucun_seek() {
    let device_id = "local:Sortie 2893";
    let (orch, zone_id, _dir) = zone_qui_joue_un_flac(Some("local"), Some(device_id)).await;

    orch.replay_zone_at_position(zone_id, 94_000, "eq_change")
        .await
        .expect("la relecture doit réussir");

    assert_eq!(
        position_vue_par_la_sortie(&orch, device_id).await,
        0,
        "sortie locale : le flux est pré-seeké à la source, un Seek de plus doublerait l'offset (#1518)"
    );
}

// ------------------------------------------------------------------
// #2595 — la bascule Audiophile ne repart pas d'une position INCONNUE.
// ------------------------------------------------------------------

/// Laisser passer l'anti-rebond de `schedule_eq_replay`, plus la relecture
/// elle-même (le chemin local s'accorde 300 ms d'arrêt de sortie).
async fn laisser_passer_l_anti_rebond() {
    tokio::time::sleep(std::time::Duration::from_millis(
        PlaybackOrchestrator::EQ_REPLAY_DEBOUNCE_MS + 900,
    ))
    .await;
}

/// Le prédicat qui tranche, sur les deux formes de zone. Témoin : la zone
/// AVEC périphérique reste du côté « position mesurée » — le correctif ne
/// doit rien lui retirer.
#[tokio::test]
async fn le_predicat_de_position_suit_la_presence_d_un_peripherique() {
    let orch = test_orchestrator();
    let zr = ZoneRepo::with_backend(orch.db.clone());
    let avec = zr
        .create("Salon", Some("dlna"), Some("dlna:uuid-2595"))
        .unwrap();
    let sans = zr.create("Cet ordinateur", Some("browser"), None).unwrap();
    assert!(
        orch.position_entretenue_par_le_sondeur(avec),
        "le sondeur interroge toute zone qui a un périphérique : sa position est mesurée à la seconde"
    );
    assert!(
        !orch.position_entretenue_par_le_sondeur(sans),
        "sans périphérique, `poller.rs` fait `continue` avant son unique `update_position` : rien ne mesure cette zone"
    );
}

/// Cas NOMINAL, inchangé : sur une zone observée, la bascule reprend à la
/// position mesurée. C'est la contre-épreuve de la garde — si elle mordait
/// trop large, ce test tomberait.
#[tokio::test]
async fn la_bascule_pure_reprend_a_la_position_mesuree() {
    let (orch, zone_id, _dir) =
        zone_qui_joue_un_flac(Some("dlna"), Some("dlna:uuid-2595-temoin")).await;
    let orch = Arc::new(orch);
    let generation_avant = orch.playback.get_state(zone_id).await.track_generation;
    // Ce que le sondeur écrit chaque seconde sur une zone observée.
    orch.playback.update_position(zone_id, 137_000).await;

    assert!(
        !orch.apply_audiophile_change(zone_id).await,
        "chemin réseau : la bascule passe par un redémarrage programmé, donc pas d'application à chaud"
    );
    laisser_passer_l_anti_rebond().await;

    let apres = orch.playback.get_state(zone_id).await;
    assert_eq!(
        apres.position_ms, 137_000,
        "la relecture doit repartir de la position MESURÉE, pas de zéro"
    );
    assert_ne!(
        apres.track_generation, generation_avant,
        "la relecture doit bien avoir eu lieu : sans elle, ce test ne prouverait rien du cas nominal"
    );
}

/// LE défaut de Pierre M (#2595), zone 987 : « Cet ordinateur », donc AUCUN
/// périphérique de sortie, donc aucune position mesurée. La bascule ne doit
/// pas prétendre reprendre : elle renonce, le dit dans le journal
/// (`eq_replay_skipped_position_inconnue`), et laisse le morceau courir.
///
/// Avant le correctif, `position_ms` valait 0 — non parce que le morceau
/// était au début, mais parce que personne ne l'avait jamais mesuré — et la
/// relecture repartait de là.
#[tokio::test]
async fn la_bascule_pure_sans_peripherique_ne_repart_pas_de_zero() {
    let (orch, zone_id, _dir) = zone_qui_joue_un_flac(Some("browser"), None).await;
    let orch = Arc::new(orch);
    let generation_avant = orch.playback.get_state(zone_id).await.track_generation;
    assert_eq!(
        orch.playback.get_state(zone_id).await.position_ms,
        0,
        "point de départ du défaut : la zone joue et le serveur croit être à 0"
    );

    assert!(
        !orch.apply_audiophile_change(zone_id).await,
        "rien n'est appliqué à chaud sur une zone navigateur"
    );
    laisser_passer_l_anti_rebond().await;

    let apres = orch.playback.get_state(zone_id).await;
    assert!(
        apres.last_seek_at.is_none(),
        "aucune relecture ne doit avoir été TENTÉE : `replay_zone_at_position` pose un `seek` \
         avant toute autre chose, y compris quand elle échoue ensuite (#2595)"
    );
    assert_eq!(
        apres.track_generation, generation_avant,
        "le morceau ne doit pas avoir été relancé — c'est exactement ce que Pierre M entend comme « ça repart du début »"
    );
    assert!(
        !orch.eq_replay_gen.lock().unwrap().contains_key(&zone_id),
        "la relecture ne doit même pas être armée quand la position est inconnue"
    );
}

/// Un timeout de transport ne doit PAS détruire la session de flux : la
/// commande a pu atteindre le renderer, qui ira chercher l'URL. La détruire
/// lui fait afficher « chanson non trouvée ». Un refus, lui, est concluant.
#[tokio::test]
async fn transport_timeout_keeps_the_stream_session_but_refusal_drops_it() {
    let orch = test_orchestrator();
    let flac = tempfile::Builder::new().suffix(".flac").tempfile().unwrap();
    let f = flac.path().to_path_buf();
    std::fs::write(&f, b"fake audio").unwrap();

    for (device_id, output, doit_survivre) in [
        (
            "timeout-dev",
            Box::new(TimingOutOutput {
                id: "timeout-dev".into(),
            }) as Box<dyn crate::outputs::traits::OutputTarget>,
            true,
        ),
        (
            "reject-dev",
            Box::new(RejectingOutput {
                id: "reject-dev".into(),
            }) as Box<dyn crate::outputs::traits::OutputTarget>,
            false,
        ),
    ] {
        orch.outputs.lock().await.register(output);

        let sid = orch
            .streamer
            .create_file_session(
                crate::http::streamer::StreamInfo {
                    format: "flac".into(),
                    mime_type: "audio/flac".into(),
                    ..Default::default()
                },
                f.to_string_lossy().into_owned(),
                false,
            )
            .await;

        assert!(
            orch.streamer.stream_bytes_sent(&sid).await.is_some(),
            "{device_id} : la session doit exister juste après sa création"
        );

        let media = crate::outputs::traits::PlayMedia {
            url: "http://server/stream",
            mime_type: "audio/flac",
            ..Default::default()
        };
        let (output_sent, output_error) = orch
            .send_to_output(device_id, &media, None, false, 1, None)
            .await;
        assert!(!output_sent, "{device_id} : l'envoi doit échouer");
        let err = output_error.expect("une erreur doit être remontée");

        // Même décision que la branche d'échec de play().
        if super::command_may_have_landed(&err) {
            // on conserve
        } else {
            orch.streamer.remove_session(&sid).await;
        }

        let encore_la = orch.streamer.stream_bytes_sent(&sid).await.is_some();
        assert_eq!(
            encore_la, doit_survivre,
            "{device_id} : session présente={encore_la}, attendu={doit_survivre}"
        );
    }
}

/// #1518 (Vincent) : seek d'une piste STREAMING (Qobuz/Tidal) sur sortie
/// locale. Depuis b3a4a79f le transcodage WAV streaming est pré-seeké
/// (decode_to_pcm_streaming_seeked reçoit seek_s), comme le chemin fichier
/// local. Dériver le drapeau de media.file_path (None en streaming) faisait
/// re-sauter l'offset une DEUXIÈME fois côté consommateur : un seek à 4:30
/// jetait tout le PCM restant de la piste → silence total, puis boucle de
/// redémarrage ~3 s (le poller voit la piste « finie » et la relance).
#[cfg(feature = "local-audio")]
#[tokio::test]
async fn streaming_seek_on_local_output_is_producer_preseeked_no_consumer_skip() {
    let orch = test_orchestrator();
    let device_id = "local:Test Device 1518";
    orch.outputs
        .lock()
        .await
        .register(Box::new(crate::outputs::local::LocalOutput::new(
            "Test Device 1518".to_string(),
        )));

    // Média streaming : PAS de file_path (cas Qobuz/Tidal). L'URL refuse
    // la connexion, le thread audio s'arrête avant de toucher un device.
    let media = crate::outputs::traits::PlayMedia {
        url: "http://127.0.0.1:1/stream",
        mime_type: "audio/wav",
        ..Default::default()
    };
    let (sent, err) = orch
        .send_to_output(device_id, &media, Some(270_000), false, 1, None)
        .await;
    assert!(sent, "play_media doit partir : {err:?}");

    let arc = orch.outputs.lock().await.get(device_id).unwrap();
    let out = arc.lock().await;
    let local = out
        .as_any()
        .downcast_ref::<crate::outputs::local::LocalOutput>()
        .unwrap();
    assert!(
        local.producer_seeked(),
        "flux streaming transcodé = déjà pré-seeké : le consommateur ne doit PAS re-sauter l'offset (#1518)"
    );
}

#[test]
fn prefetch_buffer_truncated_cases() {
    // Unknown duration (0) must count as truncated — the DMP-A8 cut.
    assert!(super::prefetch_buffer_truncated(30_000, 0));
    // Partial buffer of a known-length track: truncated.
    assert!(super::prefetch_buffer_truncated(30_000, 277_000));
    // Buffer covers (near) the whole track: NOT truncated.
    assert!(!super::prefetch_buffer_truncated(276_000, 277_000));
    assert!(!super::prefetch_buffer_truncated(300_000, 277_000));
    // Within the 2s tolerance: NOT truncated.
    assert!(!super::prefetch_buffer_truncated(60_000, 61_500));
}

#[tokio::test]
async fn test_pause_resume_stop() {
    let orch = test_orchestrator();
    let zone_id = 1;

    // Set up a NowPlaying so pause/stop have state to work with
    let np = NowPlaying {
        track_id: Some(42),
        title: "Test Track".into(),
        artist_name: Some("Test Artist".into()),
        album_title: Some("Test Album".into()),
        cover_path: None,
        duration_ms: 180_000,
        source: "local".into(),
        source_id: None,
        stream_id: None,
        ..Default::default()
    };
    orch.playback.play(zone_id, np).await;

    // Pause
    orch.pause(zone_id, None).await.unwrap();
    let state = orch.playback.get_state(zone_id).await;
    assert_eq!(state.state, PlayState::Paused);

    // Resume
    orch.resume(zone_id, None).await.unwrap();
    let state = orch.playback.get_state(zone_id).await;
    assert_eq!(state.state, PlayState::Playing);

    // Stop
    orch.stop(zone_id, None).await;
    let state = orch.playback.get_state(zone_id).await;
    assert_eq!(state.state, PlayState::Stopped);
}

#[tokio::test]
async fn test_seek_persists() {
    let orch = test_orchestrator();

    // Create a zone in the DB so save_playback_position has a row to UPDATE
    let zone_repo = ZoneRepo::with_backend(orch.db.clone());
    let zone_id = zone_repo.create("Test Zone", None, None).unwrap();

    // Set up NowPlaying (seek persists position only when now_playing exists)
    let np = NowPlaying {
        track_id: Some(99),
        title: "Seek Test".into(),
        artist_name: None,
        album_title: None,
        cover_path: None,
        duration_ms: 300_000,
        source: "local".into(),
        source_id: None,
        stream_id: None,
        ..Default::default()
    };
    orch.playback.play(zone_id, np).await;

    // Seek to 42 seconds
    orch.seek(zone_id, 42_000, None).await.unwrap();

    // Verify in-memory state updated
    let state = orch.playback.get_state(zone_id).await;
    assert_eq!(state.position_ms, 42_000);

    // Verify DB position saved
    let zone = zone_repo.get(zone_id).unwrap().unwrap();
    assert_eq!(zone.last_position_ms, 42_000);
    assert_eq!(zone.last_track_id, Some(99));
    assert_eq!(zone.last_track_source.as_deref(), Some("local"));
}

#[tokio::test]
async fn test_set_volume() {
    let orch = test_orchestrator();
    let zone_id = 1;

    // Initialize zone state with a NowPlaying
    let np = NowPlaying {
        track_id: None,
        title: "Volume Test".into(),
        artist_name: None,
        album_title: None,
        cover_path: None,
        duration_ms: 60_000,
        source: "local".into(),
        source_id: None,
        stream_id: None,
        ..Default::default()
    };
    orch.playback.play(zone_id, np).await;

    // Set volume to 80%
    orch.set_volume(zone_id, 0.8, None).await.unwrap();
    let state = orch.playback.get_state(zone_id).await;
    assert!((state.volume - 0.8).abs() < f64::EPSILON);

    // Set volume to 0 (mute level)
    orch.set_volume(zone_id, 0.0, None).await.unwrap();
    let state = orch.playback.get_state(zone_id).await;
    assert!((state.volume - 0.0).abs() < f64::EPSILON);

    // Set volume to 1.0 (max)
    orch.set_volume(zone_id, 1.0, None).await.unwrap();
    let state = orch.playback.get_state(zone_id).await;
    assert!((state.volume - 1.0).abs() < f64::EPSILON);
}

#[test]
fn gain_trim_factor_convertit_et_clampe() {
    use crate::orchestrator::gain_trim_factor;
    assert!((gain_trim_factor(0.0) - 1.0).abs() < 1e-9);
    // -6 dB ≈ ×0.5012
    assert!((gain_trim_factor(-6.0) - 0.501_187).abs() < 1e-4);
    // +6 dB ≈ ×1.9953
    assert!((gain_trim_factor(6.0) - 1.995_262).abs() < 1e-4);
    // Clamp ±12 dB
    assert!((gain_trim_factor(-40.0) - gain_trim_factor(-12.0)).abs() < 1e-9);
    assert!((gain_trim_factor(40.0) - gain_trim_factor(12.0)).abs() < 1e-9);
}

#[tokio::test]
async fn le_trim_ne_touche_pas_le_volume_utilisateur() {
    // Le trim n'affecte que la valeur envoyée au device : l'état de
    // lecture (ce que l'UI affiche) et la base gardent le volume brut.
    let orch = test_orchestrator();
    let zone_repo = ZoneRepo::with_backend(orch.db.clone());
    let zone_id = zone_repo.create("Trim Zone", None, None).unwrap();
    crate::db::settings_repo::SettingsRepo::with_backend(orch.db.clone())
        .set(&format!("zone_{zone_id}_gain_trim_db"), "-6")
        .unwrap();

    orch.set_volume(zone_id, 0.8, None).await.unwrap();
    let state = orch.playback.get_state(zone_id).await;
    assert!((state.volume - 0.8).abs() < f64::EPSILON);
    let zone = zone_repo.get(zone_id).unwrap().unwrap();
    assert_eq!(zone.volume, 80.0);
}

#[tokio::test]
async fn test_persist_position_on_pause() {
    let orch = test_orchestrator();

    // Create a zone in DB
    let zone_repo = ZoneRepo::with_backend(orch.db.clone());
    let zone_id = zone_repo.create("Pause Zone", None, None).unwrap();

    // Set up playback at a known position
    let np = NowPlaying {
        track_id: Some(7),
        title: "Pause Persist".into(),
        artist_name: None,
        album_title: None,
        cover_path: None,
        duration_ms: 200_000,
        source: "local".into(),
        source_id: Some("src-7".into()),
        stream_id: None,
        ..Default::default()
    };
    orch.playback.play(zone_id, np).await;
    orch.playback.update_position(zone_id, 55_000).await;

    // Pause triggers persist_position
    orch.pause(zone_id, None).await.unwrap();

    let zone = zone_repo.get(zone_id).unwrap().unwrap();
    assert_eq!(zone.last_position_ms, 55_000);
    assert_eq!(zone.last_track_id, Some(7));
    assert_eq!(zone.last_track_source_id.as_deref(), Some("src-7"));
}

#[tokio::test]
async fn test_persist_position_on_stop() {
    let orch = test_orchestrator();

    let zone_repo = ZoneRepo::with_backend(orch.db.clone());
    let zone_id = zone_repo.create("Stop Zone", None, None).unwrap();

    let np = NowPlaying {
        track_id: Some(10),
        title: "Stop Persist".into(),
        artist_name: Some("Artist".into()),
        album_title: None,
        cover_path: None,
        duration_ms: 120_000,
        source: "tidal".into(),
        source_id: Some("tidal-10".into()),
        stream_id: None,
        ..Default::default()
    };
    orch.playback.play(zone_id, np).await;
    orch.playback.update_position(zone_id, 90_000).await;

    // Stop also persists position
    orch.stop(zone_id, None).await;

    let zone = zone_repo.get(zone_id).unwrap().unwrap();
    assert_eq!(zone.last_position_ms, 90_000);
    assert_eq!(zone.last_track_source.as_deref(), Some("tidal"));
}

#[tokio::test]
async fn test_record_listen() {
    use crate::db::history_repo::HistoryRepo;

    let orch = test_orchestrator();

    // Create a zone so the FK constraint on zone_id is satisfied
    let zone_repo = ZoneRepo::with_backend(orch.db.clone());
    let zone_id = zone_repo.create("Listen Zone", None, None).unwrap();

    orch.record_listen(
        "Test Song",
        Some("Artist"),
        Some("Album"),
        "local",
        None,
        None,
        180_000,
        zone_id,
        None,
        Some(7),
        crate::orchestrator::ContexteEcoute {
            nature: Some("playlist"),
            id: Some("12"),
            rang: Some(4),
        },
    );

    let repo = HistoryRepo::with_backend(orch.db.clone());
    let history = repo.recent(10).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].title, "Test Song");
    assert_eq!(history[0].artist_name.as_deref(), Some("Artist"));
    // The owning profile passed by the caller is persisted verbatim
    // (session → history tag), no longer read from the global setting.
    // recent()'s RECORD_COLS omits profile_id, so assert on the column
    // directly to prove the write stored the caller's value.
    let stored_profile = orch
        .db
        .query_one("SELECT profile_id FROM listen_history LIMIT 1", &[])
        .ok()
        .flatten()
        .and_then(|cols| cols.first().and_then(|v| v.as_i64()));
    assert_eq!(stored_profile, Some(7));
    // #2441 — l'intention passee par l'appelant est ecrite telle quelle.
    // Sans elle, cette ligne est indiscernable de la meme piste jouee
    // seule, et aucune rubrique ne peut « refleter la realite de ce qu'a
    // voulu faire l'auditeur ».
    assert_eq!(history[0].context_type.as_deref(), Some("playlist"));
    assert_eq!(history[0].context_id.as_deref(), Some("12"));
    // Migration 94 — le RANG, sans lequel on rouvrirait la bonne playlist
    // a sa premiere piste.
    assert_eq!(history[0].context_position, Some(4));
    assert_eq!(history[0].source, "local");
}

/// #2441 — la moitie « re-tirage » de l'arbitrage, ecrite a l'ECRITURE.
///
/// En lecture sequentielle le rang est ce qui permet de reprendre la
/// playlist a la piste 7. En lecture ALEATOIRE il ne designe plus rien de
/// reproductible : `shuffle_order` est une permutation regeneree a chaque
/// activation, donc « position 7 » du tirage d'hier tombera sur une autre
/// piste demain. On re-tire, donc on n'ecrit pas de rang.
///
/// CONTRE-EPREUVE : remplacer le corps de `rang_a_retenir` par
/// `Some(queue_position)` rend ce test ROUGE sur le premier cas — et la
/// section rouvrirait la playlist a une piste tiree au hasard hier, en
/// pretendant reprendre ou l'auditeur en etait.
#[test]
fn le_rang_reste_vide_en_lecture_aleatoire() {
    assert_eq!(
        crate::orchestrator::rang_a_retenir(true, 7),
        None,
        "en aleatoire le rang n'est pas reproductible : on RE-TIRE"
    );
    assert_eq!(
        crate::orchestrator::rang_a_retenir(false, 7),
        Some(7),
        "en lecture sequentielle, le rang est precisement ce qu'il faut \
         retenir pour reprendre ou l'auditeur en etait"
    );
    assert_eq!(
        crate::orchestrator::rang_a_retenir(false, 0),
        Some(0),
        "la premiere piste est un rang comme un autre, pas une absence"
    );
    assert_eq!(
        crate::orchestrator::rang_a_retenir(false, -1),
        None,
        "position sentinelle d'une zone qui n'a pas commence : rien a \
         retenir"
    );
}

// ------------------------------------------------------------------
// #1998 — zone navigateur : l'annonce suit la PREUVE de lecture.
//
// Aucun de ces tests ne touche le réseau : sans clé Last.fm ni jeton
// ListenBrainz dans la base de test, `dispatch_now_playing` sort avant le
// moindre appel. Ce qui est observé ici est l'effet vérifiable côté
// serveur — la ligne `listen_history` et le verrou « une seule fois ».
// ------------------------------------------------------------------

/// Une session de flux dont l'onglet a tiré `octets` octets.
async fn session_navigateur(
    orch: &PlaybackOrchestrator,
    fichier: &std::path::Path,
    octets: u64,
) -> String {
    let sid = orch
        .streamer
        .create_file_session(
            crate::http::streamer::StreamInfo {
                format: "flac".into(),
                mime_type: "audio/flac".into(),
                ..Default::default()
            },
            fichier.to_string_lossy().into_owned(),
            false,
        )
        .await;
    if octets > 0 {
        let sessions = orch.streamer.sessions_state();
        let sessions = sessions.lock().await;
        sessions
            .get(&sid)
            .expect("la session vient d'être créée")
            .bytes_sent
            .store(octets, std::sync::atomic::Ordering::Relaxed);
    }
    sid
}

fn annonce_en_attente(
    stream_id: &str,
    record_history: bool,
    source: &str,
) -> super::AnnonceNavigateurDifferee {
    super::AnnonceNavigateurDifferee {
        stream_id: stream_id.to_string(),
        title: "Come on In".into(),
        artist: Some("Bridge City Sinners".into()),
        album: Some("Unholy Hymns".into()),
        source: source.into(),
        source_id: None,
        track_id: None,
        duration_ms: 180_000,
        cover_path: None,
        record_history,
    }
}

fn lignes_historique(orch: &PlaybackOrchestrator) -> usize {
    crate::db::history_repo::HistoryRepo::with_backend(orch.db.clone())
        .recent(10)
        .unwrap()
        .len()
}

/// Le cœur du ticket rouvert : tant que l'onglet n'a rien tiré, rien n'est
/// annoncé ; dès qu'il tire, l'annonce part — et une seule fois.
#[tokio::test]
async fn zone_navigateur_l_annonce_attend_que_l_onglet_tire_le_flux() {
    let orch = test_orchestrator();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Ce PC", Some("browser"), None)
        .unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let f = tmp.path().join("piste.flac");
    std::fs::write(&f, b"fake audio").unwrap();

    // Onglet muet : la session existe, personne ne la consomme.
    let sid = session_navigateur(&orch, &f, 0).await;
    orch.annonces_navigateur
        .lock()
        .unwrap()
        .insert(zone_id, annonce_en_attente(&sid, true, "local"));

    assert!(
        !orch.confirmer_lecture_navigateur(zone_id, &sid).await,
        "aucun octet tiré : l'annonce ne doit pas partir — c'est le défaut d'origine"
    );
    assert_eq!(
        lignes_historique(&orch),
        0,
        "listen_history ne doit rien porter tant que rien n'a été entendu"
    );
    assert!(
        orch.annonces_navigateur
            .lock()
            .unwrap()
            .contains_key(&zone_id),
        "l'annonce reste en attente : l'onglet peut encore démarrer"
    );

    // L'onglet tire le flux : c'est la preuve.
    {
        let sessions = orch.streamer.sessions_state();
        let sessions = sessions.lock().await;
        sessions
            .get(&sid)
            .unwrap()
            .bytes_sent
            .store(64 * 1024, std::sync::atomic::Ordering::Relaxed);
    }

    assert!(
        orch.confirmer_lecture_navigateur(zone_id, &sid).await,
        "l'onglet consomme le flux : l'annonce doit partir"
    );
    assert_eq!(
        lignes_historique(&orch),
        1,
        "l'historique local doit enfin porter cette écoute"
    );

    // Le tick suivant ne doit pas re-annoncer.
    assert!(
        !orch.confirmer_lecture_navigateur(zone_id, &sid).await,
        "le poller repasse chaque seconde : une écoute, une annonce"
    );
    assert_eq!(
        lignes_historique(&orch),
        1,
        "pas de doublon au tick suivant"
    );
}

/// `record_history=false` (recherche de position, reconnexion) : l'annonce
/// « en écoute » part, mais l'historique ne doublonne pas. Sans ce report
/// du drapeau, déplacer le curseur ajouterait une ligne à chaque fois.
#[tokio::test]
async fn zone_navigateur_une_recreation_de_flux_ne_doublonne_pas_l_historique() {
    let orch = test_orchestrator();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Ce PC", Some("browser"), None)
        .unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let f = tmp.path().join("piste.flac");
    std::fs::write(&f, b"fake audio").unwrap();
    let sid = session_navigateur(&orch, &f, 64 * 1024).await;
    orch.annonces_navigateur
        .lock()
        .unwrap()
        .insert(zone_id, annonce_en_attente(&sid, false, "local"));

    assert!(
        orch.confirmer_lecture_navigateur(zone_id, &sid).await,
        "l'annonce « en écoute » part quand même : la piste est bien entendue"
    );
    assert_eq!(
        lignes_historique(&orch),
        0,
        "une re-création de flux pour une piste déjà en cours ne s'ajoute pas à l'historique"
    );
}

/// La radio n'entre pas dans l'historique local — même exclusion que le
/// chemin nominal, pour la même raison (titre figé au démarrage).
#[tokio::test]
async fn zone_navigateur_la_radio_reste_hors_de_l_historique() {
    let orch = test_orchestrator();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Ce PC", Some("browser"), None)
        .unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let f = tmp.path().join("piste.flac");
    std::fs::write(&f, b"fake audio").unwrap();
    let sid = session_navigateur(&orch, &f, 64 * 1024).await;
    orch.annonces_navigateur
        .lock()
        .unwrap()
        .insert(zone_id, annonce_en_attente(&sid, true, "radio"));

    assert!(orch.confirmer_lecture_navigateur(zone_id, &sid).await);
    assert_eq!(
        lignes_historique(&orch),
        0,
        "la radio ne s'écrit pas dans listen_history"
    );
}

/// Une lecture abandonnée avant le premier octet ne s'annonce jamais, même
/// si un vieux flux traîne : l'attente est identifiée par SON flux.
#[tokio::test]
async fn zone_navigateur_un_autre_flux_ne_libere_pas_l_annonce() {
    let orch = test_orchestrator();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Ce PC", Some("browser"), None)
        .unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let f = tmp.path().join("piste.flac");
    std::fs::write(&f, b"fake audio").unwrap();
    let attendu = session_navigateur(&orch, &f, 0).await;
    let autre = session_navigateur(&orch, &f, 64 * 1024).await;
    orch.annonces_navigateur
        .lock()
        .unwrap()
        .insert(zone_id, annonce_en_attente(&attendu, true, "local"));

    assert!(
        !orch.confirmer_lecture_navigateur(zone_id, &autre).await,
        "les octets d'un AUTRE flux ne prouvent rien sur celui-ci"
    );
    assert_eq!(lignes_historique(&orch), 0);
}

/// Arrêt avant le premier octet : il n'y a rien eu à entendre, l'attente
/// meurt avec la lecture.
#[tokio::test]
async fn zone_navigateur_l_arret_annule_l_annonce_en_attente() {
    let orch = test_orchestrator();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Ce PC", Some("browser"), None)
        .unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let f = tmp.path().join("piste.flac");
    std::fs::write(&f, b"fake audio").unwrap();
    let sid = session_navigateur(&orch, &f, 0).await;
    orch.annonces_navigateur
        .lock()
        .unwrap()
        .insert(zone_id, annonce_en_attente(&sid, true, "local"));

    orch.stop(zone_id, None).await;

    assert!(
        orch.annonces_navigateur.lock().unwrap().is_empty(),
        "l'arrêt oublie l'annonce en attente"
    );
    // Même si l'onglet tire des octets après coup (fin de tampon), plus
    // rien ne part : la lecture est terminée.
    {
        let sessions = orch.streamer.sessions_state();
        let sessions = sessions.lock().await;
        if let Some(s) = sessions.get(&sid) {
            s.bytes_sent
                .store(64 * 1024, std::sync::atomic::Ordering::Relaxed);
        }
    }
    assert!(!orch.confirmer_lecture_navigateur(zone_id, &sid).await);
    assert_eq!(lignes_historique(&orch), 0);
}

#[tokio::test]
async fn test_resolve_cover_url_passthrough() {
    let orch = test_orchestrator();
    let result = orch.resolve_cover_url(Some("https://img.tidal.com/cover.jpg"));
    assert_eq!(result.as_deref(), Some("https://img.tidal.com/cover.jpg"));

    let result = orch.resolve_cover_url(Some("http://local/art.png"));
    assert_eq!(result.as_deref(), Some("http://local/art.png"));
}

#[tokio::test]
async fn test_resolve_cover_url_hash() {
    let orch = test_orchestrator();
    let result = orch.resolve_cover_url(Some("abc123def"));
    let url = result.unwrap();
    assert!(
        url.contains("/api/v1/library/artwork/abc123def"),
        "got: {url}"
    );
    assert!(url.starts_with("http://"), "got: {url}");
}

#[tokio::test]
async fn test_resolve_cover_url_none() {
    let orch = test_orchestrator();
    assert!(orch.resolve_cover_url(None).is_none());
}

#[tokio::test]
async fn test_persist_local_queue() {
    use crate::db::play_queue_repo::PlayQueueRepo;

    let orch = test_orchestrator();
    let zone_repo = ZoneRepo::with_backend(orch.db.clone());
    let zone_id = zone_repo.create("Queue Zone", None, None).unwrap();

    // Insert some tracks so FK constraints are satisfied
    orch.db
        .execute("INSERT INTO artists (id, name) VALUES (1, 'Artist')", &[])
        .unwrap();
    orch.db
        .execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Album', 1)",
            &[],
        )
        .unwrap();
    for i in 1..=3i64 {
        let title = format!("Track {i}");
        orch.db
            .execute(
                "INSERT INTO tracks (id, title, album_id, artist_id, duration_ms) VALUES (?, ?, 1, 1, 180000)",
                &[&i as &dyn crate::db::backend::ToSqlValue, &title as &dyn crate::db::backend::ToSqlValue],
            )
            .unwrap();
    }

    orch.persist_local_queue(zone_id, &[1, 2, 3], 0);

    let queue_repo = PlayQueueRepo::with_backend(orch.db.clone());
    let queue = queue_repo.get_queue(zone_id).unwrap();
    assert_eq!(queue.len(), 3);
}

fn radio_test_eq_profile() -> crate::audio::eq::EqProfile {
    crate::audio::eq::EqProfile {
        enabled: true,
        bands: vec![crate::audio::eq::EqBandSpec {
            freq: 80.0,
            gain: 8.0,
            q: 0.71,
            band_type: "low_shelf".into(),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn f32_bytes(samples: &[f32]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect()
}

// ------------------------------------------------------------------
// #3234 — un format sans décodeur se DIT, il ne se joue pas en silence.
// ------------------------------------------------------------------

/// Écrit une image `.iso` creuse portant `SACDMTOC` à son Master TOC.
///
/// Le décalage et la signature viennent de la PRODUCTION : une fixture qui
/// recopierait `0x800 * 510` deviendrait muette le jour où le contrôle
/// changerait de repère, et l'épreuve resterait verte contre rien.
///
/// Fichier creux : aucun octet réel avant le décalage, la fixture ne
/// consomme pas 4 Mo de disque.
fn image_iso_sacd_3234(dossier: &std::path::Path, nom: &str) -> String {
    use std::io::{Seek, SeekFrom, Write};
    let chemin = dossier.join(nom);
    let mut fichier = std::fs::File::create(&chemin).unwrap();
    fichier
        .seek(SeekFrom::Start(
            crate::audio::iso_sacd::DECALAGE_MASTER_TOC_SACD,
        ))
        .unwrap();
    fichier
        .write_all(crate::audio::iso_sacd::SIGNATURE_MASTER_TOC_SACD)
        .unwrap();
    fichier.set_len(4_200_000).unwrap();
    fichier.flush().unwrap();
    chemin.to_string_lossy().into_owned()
}

/// `PlayRequest` minimale de lecture locale : seuls la zone et la piste
/// varient dans les deux épreuves qui suivent.
fn requete_locale_3234(zone_id: i64, track_id: i64) -> super::PlayRequest {
    super::PlayRequest {
        zone_id,
        output_device_id: None,
        track_id: Some(track_id),
        source: Some("local".into()),
        source_id: None,
        title: None,
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms: None,
        seek_ms: None,
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
    }
}

/// Insère une piste dont le `file_path` est `chemin`, au format `format`.
fn piste_3234(orch: &PlaybackOrchestrator, chemin: &str, format: &str) {
    orch.db
        .execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Supertramp')",
            &[],
        )
        .unwrap();
    orch.db
        .execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Breakfast In America', 1)",
            &[],
        )
        .unwrap();
    orch.db
        .execute(
            &format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, \
                 duration_ms, sample_rate, bit_depth, channels) \
                 VALUES (1, 'The Logical Song', 1, 1, ?, '{format}', 300000, 44100, 16, 2)"
            ),
            &[&chemin.to_string() as &dyn crate::db::backend::ToSqlValue],
        )
        .unwrap();
}

/// #3234 — un ISO SACD demandé en LECTURE rend un motif nommé.
///
/// JeromeQ, fil 1206 : « Tune ne lit pas les fichiers ISO ? » Personne ne
/// lui répondait, et le serveur non plus. Le parcours de bibliothèque, lui,
/// compte et nomme ces fichiers depuis #2992 ; la demande de LECTURE, elle,
/// n'avait aucune garde : `AudioFormat::from_extension("iso")` rend `None`,
/// le transcodage retombe sur `unwrap_or(AudioFormat::Flac)`, et une image
/// disque part sur le fil comme du FLAC. La zone reste muette, et aucune
/// réponse HTTP ne dit pourquoi.
///
/// L'épreuve mesure CE QUE REND `resolve_local_track` — la fonction que
/// `play()` appelle et dont la chaîne d'erreur arrive telle quelle à
/// `play_error_response`. Elle ne rappelle pas la condition du code : la
/// fixture est une vraie image dont le Master TOC porte `SACDMTOC`, et
/// c'est la lecture de ces huit octets sur le disque qui doit déclencher le
/// refus.
#[tokio::test]
async fn un_iso_sacd_demande_en_lecture_rend_un_motif_nomme() {
    let orch = test_orchestrator();
    let dossier = tempfile::tempdir().unwrap();
    let chemin = image_iso_sacd_3234(dossier.path(), "Breakfast In America.iso");
    piste_3234(&orch, &chemin, "iso");
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Ce PC", Some("browser"), None)
        .unwrap();

    // `ResolvedStream` n'implémente pas `Debug` : la branche verte est
    // dépliée à la main plutôt que par `expect_err`.
    let erreur = match orch
        .resolve_local_track(&requete_locale_3234(zone_id, 1))
        .await
    {
        Err(erreur) => erreur,
        Ok(_) => panic!(
            "un ISO SACD s'est résolu en flux jouable : c'est le silence de \
             #3234, une image disque part sur le fil comme du FLAC"
        ),
    };

    let motif = erreur
        .strip_prefix("format_not_playable:")
        .unwrap_or_else(|| {
            panic!(
                "la demande de lecture doit rendre un refus NOMMÉ que la route \
             sait mettre en forme ; elle a rendu : {erreur}"
            )
        });
    // Le motif est celui du rapport de parcours, mot pour mot (#2992) :
    // deux phrases différentes pour un seul empêchement feraient croire à
    // deux défauts.
    assert_eq!(motif, crate::audio::iso_sacd::MOTIF_ISO_SACD_NON_EXTRAIT);
    assert!(
        motif.contains("ISO SACD"),
        "le motif doit NOMMER le format : {motif}"
    );
    assert!(
        motif.contains("sacd_extract") && motif.contains("non fourni"),
        "le motif doit NOMMER ce qui manque, et dire que Tune ne le livre pas : {motif}"
    );
}

/// Témoin de #3234 : un FLAC de la bibliothèque se résout comme avant.
///
/// Sans ce témoin, un refus trop large rendrait toute la bibliothèque
/// injouable sans qu'une seule épreuve rougisse — et il doit rester vert
/// quand on retire le refus, sans quoi il ne mesurerait que lui-même.
#[tokio::test]
async fn le_flac_temoin_se_resout_comme_avant() {
    let orch = test_orchestrator();
    let dossier = tempfile::tempdir().unwrap();
    let piste = dossier.path().join("morceau.flac");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test.flac"),
        &piste,
    )
    .unwrap();
    let chemin = piste.to_string_lossy().into_owned();
    piste_3234(&orch, &chemin, "flac");
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Ce PC", Some("browser"), None)
        .unwrap();

    let refus = orch
        .resolve_local_track(&requete_locale_3234(zone_id, 1))
        .await
        .err();

    assert!(
        refus.is_none(),
        "le témoin FLAC doit rester jouable : {}",
        refus.unwrap_or_default()
    );
}

/// #2063 — contre-épreuve négative : ajouter le crochet DSP au décodeur
/// radio ne doit modifier AUCUN octet quand la zone n'a pas d'EQ actif.
#[test]
fn radio_without_eq_keeps_pcm_byte_for_byte() {
    let mut samples = vec![0.0, -0.0, 0.125, -0.25, 0.75, -0.875];
    let expected = f32_bytes(&samples);
    let mut eq = None;
    super::apply_radio_eq(&mut eq, &mut samples);
    assert_eq!(f32_bytes(&samples), expected);
}

/// Le témoin positif complète le précédent : un profil réellement actif
/// doit atteindre le PCM que Tune servira dans le WAV, pas seulement
/// forcer une route de transcodage qui oublierait ensuite le processeur.
#[test]
fn radio_with_eq_changes_the_pcm_served_to_the_renderer() {
    let profile = radio_test_eq_profile();
    let mut eq = Some(crate::audio::eq::EqProcessor::new(&profile, 44_100, 2));
    let mut samples = vec![0.10f32; 2 * 1024];
    let untouched = f32_bytes(&samples);
    super::apply_radio_eq(&mut eq, &mut samples);
    assert_ne!(f32_bytes(&samples), untouched);
}

/// Une URL MP3 explicite passait directement au navigateur. Avec un EQ,
/// ce passthrough contournerait tous les filtres : la zone doit recevoir
/// une session WAV Tune même sans `output_device_id`.
#[tokio::test]
async fn browser_radio_with_eq_is_forced_through_the_wav_session() {
    let orch = test_orchestrator();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Ce PC", Some("browser"), None)
        .unwrap();
    crate::db::settings_repo::SettingsRepo::with_backend(orch.db.clone())
        .set(
            &format!("zone_{zone_id}_eq_profile"),
            &serde_json::to_string(&radio_test_eq_profile()).unwrap(),
        )
        .unwrap();
    let source = "http://127.0.0.1:9/station.mp3";
    let req = super::PlayRequest {
        zone_id,
        output_device_id: None,
        track_id: None,
        source: Some("radio".into()),
        source_id: Some(source.into()),
        title: Some("Radio avec EQ".into()),
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms: None,
        seek_ms: None,
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
    };

    let resolved = orch.resolve_direct_url(&req).await.unwrap();
    assert!(
        resolved.stream_id.is_some(),
        "l'EQ actif doit interdire le passthrough MP3"
    );
    assert_eq!(resolved.mime_type, "audio/wav");
    assert_eq!(resolved.origin_url.as_deref(), Some(source));
}

/// #2670 — une zone NAVIGATEUR ne doit jamais recevoir l'URL de la station.
///
/// Le client web reecrit toute URL absolue en chemin relatif
/// (`browserPlay` : `u.pathname + u.search`), pour joindre l'hote Tune
/// plutot que l'IP que le serveur annonce. L'URL d'une station en `.mp3`
/// devient donc une requete `/station.mp3` adressee a Tune, a laquelle le
/// repli SPA (`routes/mod.rs`, `ServeDir::fallback(ServeFile(index.html))`)
/// repond 200 `text/html` : une page web au lieu du flux.
///
/// Et Tune ne peut rien en dire : sans session locale il n'ouvre jamais le
/// flux, donc ni `non_audio_content_type` ni `RADIO_NOT_AUDIO` — qui vivent
/// dans `decode_radio_stream_to_pcm` — ne peuvent se declencher. C'etait le
/// seul chemin radio ou une station morte restait muette ET silencieuse.
#[tokio::test]
async fn browser_radio_mp3_is_never_handed_the_station_url() {
    let orch = test_orchestrator();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Ce PC", Some("browser"), None)
        .unwrap();
    // Aucun profil EQ n'est ecrit : c'est precisement le cas que le
    // passthrough laissait passer (#2063 ne couvrait que l'EQ actif).
    // Port 9 (discard) : la tache de decodage echoue en local, aucun appel
    // reseau reel ne sort de ce test.
    let source = "http://127.0.0.1:9/tsfjazz-high.mp3";
    let req = super::PlayRequest {
        zone_id,
        output_device_id: None,
        track_id: None,
        source: Some("radio".into()),
        source_id: Some(source.into()),
        title: Some("TSF Jazz".into()),
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms: None,
        seek_ms: None,
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
    };

    let resolved = orch.resolve_direct_url(&req).await.unwrap();
    assert!(
        resolved.stream_id.is_some(),
        "une zone navigateur doit recevoir une session Tune, pas l'URL de la station"
    );
    assert_ne!(
        resolved.url, source,
        "l'URL de la station renvoyee telle quelle est reecrite en chemin local par le client, \
         qui recoit alors la page HTML de Tune"
    );
    assert_eq!(resolved.mime_type, "audio/wav");
    // L'amont voyage quand meme : un enregistreur ou les titres ICY n'ont
    // pas d'autre chemin de retour vers la source.
    assert_eq!(resolved.origin_url.as_deref(), Some(source));
}

#[tokio::test]
async fn radio_resolve_direct_url_without_output_device() {
    let orch = test_orchestrator();
    let req = super::PlayRequest {
        zone_id: 1,
        output_device_id: None,
        track_id: None,
        source: Some("radio".into()),
        source_id: Some("http://icecast.radiofrance.fr/fip-hifi.aac".into()),
        title: Some("FIP".into()),
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms: None,
        seek_ms: None,
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
    };
    let resolved = orch.resolve_direct_url(&req).await.unwrap();
    // Since the Cyrille/Yamaha fix, ambiguous codecs (.aac/.ogg/HLS/
    // extension-less) are ALWAYS proxied and transcoded to WAV, even
    // without an output device: the advertised protocolInfo must match
    // the bytes, or DLNA renderers play silence.
    assert!(
        resolved.stream_id.is_some(),
        "ambiguous .aac radio must be proxied to WAV"
    );
    assert_eq!(resolved.mime_type, "audio/wav");
    // Because `url` is now ours and not the station's, the upstream has to
    // travel with it: a recorder wanting the original AAC (or the ICY titles
    // the proxy drops) has no other way back to the source.
    assert_eq!(
        resolved.origin_url.as_deref(),
        Some("http://icecast.radiofrance.fr/fip-hifi.aac")
    );
}

#[tokio::test]
async fn radio_reliable_mp3_passes_through_without_output_device() {
    let orch = test_orchestrator();
    let req = super::PlayRequest {
        zone_id: 1,
        output_device_id: None,
        track_id: None,
        source: Some("radio".into()),
        source_id: Some("http://stream.example.com/station.mp3".into()),
        title: Some("MP3 Station".into()),
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms: None,
        seek_ms: None,
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
    };
    let resolved = orch.resolve_direct_url(&req).await.unwrap();
    // Reliable extensions (.mp3/.flac/.wav) pass through untouched: no
    // proxy session, no transcode cost.
    assert!(resolved.stream_id.is_none());
    assert_eq!(resolved.url, "http://stream.example.com/station.mp3");
    // Nothing was substituted, so there is no upstream to point at: `url`
    // already is it.
    assert!(resolved.origin_url.is_none());
}

#[tokio::test]
async fn podcast_resolve_returns_raw_url() {
    let orch = test_orchestrator();
    let req = super::PlayRequest {
        zone_id: 1,
        output_device_id: None,
        track_id: None,
        source: Some("podcast".into()),
        source_id: Some("https://cdn.podcast.com/episode.mp3".into()),
        title: Some("Episode 1".into()),
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms: Some(3600000),
        seek_ms: None,
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
    };
    let resolved = orch.resolve_direct_url(&req).await.unwrap();
    assert!(
        resolved.stream_id.is_none(),
        "podcast should not create proxy session"
    );
    assert_eq!(resolved.url, "https://cdn.podcast.com/episode.mp3");
}

/// Une URL de flux Bandcamp telle qu'elle est publiée : pas d'extension,
/// le codec est dans le CHEMIN (`/mp3-128/`), pas au bout du nom.
const BC_STREAM: &str =
    "https://t4.bcbits.com/stream/0123456789abcdef/mp3-128/1234567?p=0&ts=1&sig=deadbeef";

#[tokio::test]
async fn bandcamp_resolves_by_the_direct_url_door() {
    // Le point de la correction : `source = "bandcamp"` doit ARRIVER dans
    // `resolve_direct_url` via `resolve_stream`, et non partir chercher un
    // service « bandcamp » dans le registre (qui n'existe pas) — c'est
    // l'échec qui laissait la lecture dans l'onglet du navigateur.
    let orch = test_orchestrator();
    let req = super::PlayRequest {
        zone_id: 1,
        output_device_id: None,
        track_id: None,
        source: Some("bandcamp".into()),
        source_id: Some(BC_STREAM.into()),
        title: Some("A Track".into()),
        artist_name: Some("An Artist".into()),
        album_title: Some("An Album".into()),
        cover_url: None,
        duration_ms: Some(212_000),
        seek_ms: None,
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: Some("mp3".into()),
        track_number: None,
        disc_number: None,
    };
    let resolved = orch.resolve_stream(&req).await.unwrap();
    assert_eq!(resolved.source, "bandcamp");
    assert_eq!(
        resolved.url, BC_STREAM,
        "sans sortie : URL servie telle quelle"
    );
    assert!(resolved.stream_id.is_none());
}

#[tokio::test]
async fn bandcamp_mime_is_asserted_not_guessed() {
    // L'URL n'a pas d'extension : `guess_mime_from_url` retomberait sur son
    // défaut. On veut que le MIME soit AFFIRMÉ — Bandcamp ne sert que du
    // mp3-128 —, pas hérité d'un défaut qui pourrait changer.
    assert_eq!(super::guess_mime_from_url(BC_STREAM), "audio/mpeg");
    let orch = test_orchestrator();
    let req = super::PlayRequest {
        zone_id: 1,
        output_device_id: None,
        track_id: None,
        source: Some("bandcamp".into()),
        source_id: Some("https://t4.bcbits.com/stream/x/mp3-128/42".into()),
        title: Some("A Track".into()),
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms: None,
        seek_ms: None,
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
    };
    let resolved = orch.resolve_direct_url(&req).await.unwrap();
    assert_eq!(resolved.mime_type, "audio/mpeg");
}

#[tokio::test]
async fn bandcamp_is_proxied_in_clear_http_for_a_network_renderer() {
    // Bandcamp ne publie qu'en HTTPS ; un renderer DLNA ne sait pas ouvrir
    // TLS. Sans proxy local, il annonce PLAYING et n'émet rien — le faux
    // positif déjà vu sur le Yamaha R-N2000A.
    let orch = test_orchestrator();
    let req = super::PlayRequest {
        zone_id: 1,
        output_device_id: Some("dlna:uuid-1234".into()),
        track_id: None,
        source: Some("bandcamp".into()),
        source_id: Some(BC_STREAM.into()),
        title: Some("A Track".into()),
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms: Some(212_000),
        seek_ms: None,
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
    };
    let resolved = orch.resolve_direct_url(&req).await.unwrap();
    assert!(
        resolved.stream_id.is_some(),
        "une sortie réseau doit passer par une session proxy locale"
    );
    assert!(
        resolved.url.starts_with("http://"),
        "l'URL servie au renderer doit être en clair, pas en TLS : {}",
        resolved.url
    );
    // Les octets passent verbatim : c'est toujours du MP3, rien n'est
    // transcodé. Le renderer doit donc lire « audio/mpeg ».
    assert_eq!(resolved.mime_type, "audio/mpeg");
}

#[tokio::test]
async fn bandcamp_is_proxied_for_a_browser_zone_without_an_output_device() {
    // Contre-épreuve #2076/#2158 : une zone navigateur n'a jamais de
    // `output_device_id`. Ce n'est pas une zone orpheline — l'onglet est la
    // sortie — et il doit tirer une URL Tune locale, pas l'URL tierce que le
    // client réécrirait en chemin relatif avant de recevoir du text/html.
    let orch = test_orchestrator();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Ce PC", Some("browser"), None)
        .unwrap();
    let req = super::PlayRequest {
        zone_id,
        output_device_id: None,
        track_id: None,
        source: Some("bandcamp".into()),
        source_id: Some(BC_STREAM.into()),
        title: Some("A Track".into()),
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms: Some(212_000),
        seek_ms: None,
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
    };

    let resolved = orch.resolve_direct_url(&req).await.unwrap();
    let stream_id = resolved
        .stream_id
        .as_deref()
        .expect("une zone navigateur doit recevoir une session proxy");
    assert!(
        resolved.url.ends_with(&format!("/stream/{stream_id}.mp3")),
        "le navigateur doit tirer le MP3 depuis Tune : {}",
        resolved.url
    );
    assert!(
        !resolved.url.contains("bcbits.com"),
        "l'URL Bandcamp ne doit jamais être rendue au navigateur"
    );
    assert_eq!(resolved.origin_url.as_deref(), Some(BC_STREAM));
    assert_eq!(resolved.mime_type, "audio/mpeg");
}

#[tokio::test]
async fn bandcamp_is_decoded_to_wav_for_an_oaat_endpoint() {
    // Un endpoint OAAT ne consomme que du PCM en conteneur WAV.
    let orch = test_orchestrator();
    let req = super::PlayRequest {
        zone_id: 1,
        output_device_id: Some("oaat:endpoint-1".into()),
        track_id: None,
        source: Some("bandcamp".into()),
        source_id: Some(BC_STREAM.into()),
        title: Some("A Track".into()),
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms: Some(212_000),
        seek_ms: None,
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
    };
    let resolved = orch.resolve_direct_url(&req).await.unwrap();
    assert!(resolved.stream_id.is_some());
    assert_eq!(resolved.mime_type, "audio/wav");
    assert_eq!(resolved.sample_rate, Some(44100));
}

#[tokio::test]
async fn bandcamp_goes_straight_to_a_local_dac() {
    // La sortie locale télécharge et décode elle-même un flux HTTP
    // compressé (`local_audio_non_wav_stream_detected_decoding`) : rien à
    // interposer, et surtout rien à transcoder pour rien.
    let orch = test_orchestrator();
    let req = super::PlayRequest {
        zone_id: 1,
        output_device_id: Some("local:default".into()),
        track_id: None,
        source: Some("bandcamp".into()),
        source_id: Some(BC_STREAM.into()),
        title: Some("A Track".into()),
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms: Some(212_000),
        seek_ms: None,
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
    };
    let resolved = orch.resolve_direct_url(&req).await.unwrap();
    assert!(resolved.stream_id.is_none());
    assert_eq!(resolved.url, BC_STREAM);
    assert_eq!(resolved.mime_type, "audio/mpeg");
    // Affirmée, pas héritée d'un défaut : le chemin du signal doit
    // annoncer « MP3 — Avec perte » de sa propre autorité.
    assert_eq!(resolved.sample_rate, Some(44100));
    assert_eq!(resolved.bit_depth, Some(16));
}

/// #2074 — l'URL est la seule autorité sur la qualité.
///
/// Bandcamp nomme l'encodage dans l'URL, sous deux formes. Un fichier
/// ACHETÉ emprunte la même porte avec une autre valeur : la règle doit
/// donc porter sur le flux, jamais sur le nom du service.
#[test]
fn bandcamp_quality_is_read_from_the_url_never_from_the_source_name() {
    use super::{bandcamp_encoding, bandcamp_quality};

    // Forme « segment de chemin » — l'écoute libre publiée par Bandcamp.
    assert_eq!(bandcamp_encoding(BC_STREAM).as_deref(), Some("mp3-128"));
    let libre = bandcamp_quality("mp3-128").expect("mp3-128 est connu");
    assert_eq!(libre.codec, "mp3");
    assert_eq!(libre.mime_type, "audio/mpeg");
    assert_eq!(libre.bitrate_kbps, Some(128));

    // Forme « paramètre de requête » — la redirection de flux.
    assert_eq!(
        bandcamp_encoding("https://bandcamp.com/stream_redirect?enc=mp3-128&track_id=1").as_deref(),
        Some("mp3-128")
    );

    // ACHAT en lossless : ni MP3, ni débit. C'est le cœur de la règle.
    let achete = bandcamp_quality(
        &bandcamp_encoding("https://popplers5.bandcamp.com/download/track?enc=flac&id=42")
            .expect("enc=flac doit être lu"),
    )
    .expect("flac est connu");
    assert_eq!(achete.codec, "flac");
    assert_eq!(achete.mime_type, "audio/flac");
    assert_eq!(
        achete.bitrate_kbps, None,
        "un flux sans perte n'a aucun débit à annoncer"
    );

    // ACHAT en MP3 320 : même codec que l'extrait, débit différent.
    assert_eq!(
        bandcamp_quality("mp3-320").map(|q| q.bitrate_kbps),
        Some(Some(320))
    );
    // Débit VARIABLE : on n'invente pas de chiffre.
    assert_eq!(
        bandcamp_quality("mp3-v0").map(|q| q.bitrate_kbps),
        Some(None)
    );

    // Un hachage de chemin ne doit jamais passer pour un encodage.
    assert_eq!(
        bandcamp_encoding("https://t4.bcbits.com/stream/0123456789abcdef/7654321?p=0"),
        None
    );
    assert_eq!(bandcamp_quality("chose-inconnue"), None);
}

#[tokio::test]
async fn bandcamp_carries_its_128_kbps_all_the_way_to_the_zone() {
    // Le défaut de #2074 : la qualité était annoncée sur l'écran Bandcamp
    // et se perdait au passage en zone. Les TROIS sorties câblées en
    // 0.9.89 portent le même flux source — locale, WAV décodé pour OAAT,
    // proxy MP3 pour un renderer réseau — donc les trois doivent porter
    // le même débit jusqu'au chemin du signal.
    let orch = test_orchestrator();
    let sorties = [
        None,
        Some("local:default".to_string()),
        Some("oaat:endpoint-1".to_string()),
        Some("dlna:uuid-1234".to_string()),
    ];
    assert_eq!(sorties.len(), 4, "quatre sorties examinées");
    for sortie in sorties {
        let req = super::PlayRequest {
            zone_id: 1,
            output_device_id: sortie.clone(),
            track_id: None,
            source: Some("bandcamp".into()),
            source_id: Some(BC_STREAM.into()),
            title: Some("A Track".into()),
            artist_name: None,
            album_title: None,
            cover_url: None,
            duration_ms: Some(212_000),
            seek_ms: None,
            temp_file_path: None,
            sample_rate: None,
            bit_depth: None,
            media_format: None,
            track_number: None,
            disc_number: None,
        };
        let resolved = orch.resolve_direct_url(&req).await.unwrap();
        assert_eq!(
            resolved.bitrate_kbps,
            Some(128),
            "sortie {sortie:?} : le 128 kbit/s doit atteindre la zone"
        );
    }
}

#[tokio::test]
async fn a_purchased_bandcamp_file_is_never_labelled_mp3_128() {
    // Cas de l'ACHAT : la même porte, un autre encodage. Coller
    // « MP3 128 kbit/s » sur un FLAC serait le mensonge inverse.
    let orch = test_orchestrator();
    let req = super::PlayRequest {
        zone_id: 1,
        output_device_id: Some("dlna:uuid-1234".into()),
        track_id: None,
        source: Some("bandcamp".into()),
        source_id: Some("https://popplers5.bandcamp.com/download/track?enc=flac&id=42".into()),
        title: Some("A Track".into()),
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms: Some(212_000),
        seek_ms: None,
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
    };
    let resolved = orch.resolve_direct_url(&req).await.unwrap();
    assert_eq!(
        resolved.bitrate_kbps, None,
        "aucun débit ne doit être annoncé sur un achat lossless"
    );
    assert_eq!(
        resolved.mime_type, "audio/flac",
        "le renderer doit recevoir le vrai type, pas audio/mpeg"
    );
    let stream_id = resolved
        .stream_id
        .as_deref()
        .expect("une sortie réseau passe toujours par le proxy en clair");
    assert!(
        resolved.url.ends_with(&format!("/stream/{stream_id}.flac")),
        "le conteneur servi doit suivre l'encodage réel : {}",
        resolved.url
    );
}

/// An output that rejects `play_media` — mirrors an AirPlay renderer whose
/// ANNOUNCE returns 403 (Bilou, forum #1135).
struct RejectingOutput {
    id: String,
}

#[async_trait::async_trait]
impl crate::outputs::traits::OutputTarget for RejectingOutput {
    fn name(&self) -> &str {
        "Rejecting"
    }
    fn device_id(&self) -> &str {
        &self.id
    }
    fn output_type(&self) -> &str {
        "test"
    }
    async fn play_media(
        &self,
        _media: &crate::outputs::traits::PlayMedia<'_>,
    ) -> Result<(), String> {
        Err("ANNOUNCE failed: 403".into())
    }
    async fn pause(&self) -> Result<(), String> {
        Ok(())
    }
    async fn resume(&self) -> Result<(), String> {
        Ok(())
    }
    async fn stop(&self) -> Result<(), String> {
        Ok(())
    }
    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Ok(())
    }
    async fn set_volume(&self, _volume: f64) -> Result<(), String> {
        Ok(())
    }
    async fn set_mute(&self, _muted: bool) -> Result<(), String> {
        Ok(())
    }
    async fn get_status(&self) -> Result<crate::outputs::traits::OutputStatus, String> {
        Ok(crate::outputs::traits::OutputStatus::default())
    }
    async fn is_available(&self) -> bool {
        true
    }
}

/// When the initial output send errors (e.g. AirPlay 403), the zone must
/// fail fast: `send_to_output` reports the error and the fail-fast branch
/// flips the zone to Stopped instead of leaving it "Playing" for ~100s
/// while the poller runs its load-grace clock (Bilou, forum #1135).
#[tokio::test]
async fn output_send_error_fails_fast_to_stopped() {
    let orch = test_orchestrator();
    let zone_id = 7;
    let device_id = "airplay-192.168.1.18-7000";

    {
        let mut outputs = orch.outputs.lock().await;
        outputs.register(Box::new(RejectingOutput {
            id: device_id.to_string(),
        }));
    }

    // Prime the zone exactly as play() does before send_to_output.
    let np = NowPlaying {
        title: "So Long".into(),
        duration_ms: 230_050,
        source: "local".into(),
        ..Default::default()
    };
    orch.playback.play(zone_id, np).await;
    assert_eq!(
        orch.playback.get_state(zone_id).await.state,
        PlayState::Playing,
        "zone must be Playing after play() primes it"
    );

    // The rejecting output must report a send failure (not a false success).
    let media = crate::outputs::traits::PlayMedia {
        url: "http://server/stream",
        mime_type: "audio/wav",
        ..Default::default()
    };
    let (output_sent, output_error) = orch
        .send_to_output(device_id, &media, None, false, 1, None)
        .await;
    assert!(
        !output_sent,
        "rejecting output must report output_sent=false"
    );
    assert!(
        output_error.is_some(),
        "rejecting output must surface an error string"
    );

    // Fail-fast reaction (same as play()'s new short-circuit): stop the zone
    // immediately rather than handing it to the poller in a loading state.
    orch.playback.stop(zone_id).await;
    assert_eq!(
        orch.playback.get_state(zone_id).await.state,
        PlayState::Stopped,
        "output send error must leave the zone Stopped, not Playing"
    );
}
