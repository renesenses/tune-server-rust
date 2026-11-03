//! Native SlimProto output — plays to a Squeezelite/slim2diretta player that is
//! connected to Tune's own SlimProto server (`crate::slimproto`, port 3483).
//!
//! Unlike [`crate::outputs::squeezebox::SqueezeboxOutput`] (which drives a player
//! through an *external* LMS via its CLI), this output speaks the SlimProto wire
//! protocol directly: playback commands (`strm`/`audg`) are pushed to the
//! connected player through a per-player command channel owned by the server, and
//! status is read back from the shared player registry (updated by STAT).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use tokio::sync::Mutex;

use super::traits::{OutputCapabilities, OutputStatus, OutputTarget, PlayMedia, TransportState};
use crate::slimproto::{
    CommandChannels, FluxServi, PlayerRegistry, ServerMessage, SlimProtoPlayback, build_strm_start,
    lock_playback, new_playback_state, strm_control,
};

#[derive(Debug, Default)]
struct CurrentMedia {
    uri: Option<String>,
    duration_ms: u64,
    title: Option<String>,
    artist: Option<String>,
}

/// A native SlimProto audio output bound to one connected player (by MAC).
pub struct SlimProtoOutput {
    name: String,
    device_id: String,
    /// MAC string key into the shared player registry and command channels.
    mac_str: String,
    /// Shared registry of connected players (position/state read from here).
    players: PlayerRegistry,
    /// Per-player command senders owned by the server (push `strm`/`audg`).
    command_channels: CommandChannels,
    /// Functional state shared with the SlimProto TCP reader.  STAT confirms
    /// playback and is the authority for decoder completion/final drain.
    playback: SlimProtoPlayback,
    /// Contract of the last accepted `PlayMedia`, which SlimProto STAT does not
    /// carry back to the server.
    current_media: Arc<Mutex<CurrentMedia>>,
    /// Last volume command accepted by the player's command channel.
    volume: Arc<Mutex<f64>>,
    muted: Arc<AtomicBool>,
    /// #6476 — un `strm s` a été envoyé sur CETTE connexion du lecteur et
    /// aucun `strm q` ne l'a retiré depuis. Une reconnexion enregistre une
    /// sortie neuve : le drapeau y est faux, le lecteur n'a rien à reprendre.
    flux_pose: AtomicBool,
}

impl SlimProtoOutput {
    pub fn new(
        name: String,
        device_id: String,
        mac_str: String,
        players: PlayerRegistry,
        command_channels: CommandChannels,
    ) -> Self {
        Self::new_with_playback(
            name,
            device_id,
            mac_str,
            players,
            command_channels,
            new_playback_state(),
        )
    }

    pub(crate) fn new_with_playback(
        name: String,
        device_id: String,
        mac_str: String,
        players: PlayerRegistry,
        command_channels: CommandChannels,
        playback: SlimProtoPlayback,
    ) -> Self {
        Self {
            name,
            device_id,
            mac_str,
            players,
            command_channels,
            playback,
            current_media: Arc::new(Mutex::new(CurrentMedia::default())),
            volume: Arc::new(Mutex::new(1.0)),
            muted: Arc::new(AtomicBool::new(false)),
            flux_pose: AtomicBool::new(false),
        }
    }

    /// #6476 — le lecteur n'a plus de flux à reprendre sur place : aucun
    /// `strm s` sur cette connexion (Squeezelite s'est reconnecté), ou il est
    /// arrêté (`strm q`, déconnexion, fin, format refusé). Un `strm u` n'y
    /// rejouerait rien.
    fn sans_flux_a_reprendre(&self) -> bool {
        !self.flux_pose.load(Ordering::Relaxed)
            || lock_playback(&self.playback).transport == TransportState::Stopped
    }

    /// Push a message to this player's writer task via its command channel.
    async fn send(&self, msg: ServerMessage) -> Result<(), String> {
        let tx = {
            let chans = self.command_channels.lock().await;
            chans.get(&self.mac_str).cloned()
        };
        match tx {
            Some(tx) => tx
                .send(msg)
                .await
                .map_err(|_| "slimproto player command channel closed".to_string()),
            None => Err("slimproto player not connected".to_string()),
        }
    }
}

/// Split an `http://host:port/path` stream URL into `(port, path)`. `server_ip=0`
/// in the `strm s` frame makes the player reuse its control-connection server IP,
/// so only the HTTP port and request path are needed.
fn parse_stream_url(url: &str) -> Option<(u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let slash = rest.find('/')?;
    let authority = &rest[..slash];
    let path = rest[slash..].to_string();
    let port = authority
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(80);
    Some((port, path))
}

#[async_trait]
impl OutputTarget for SlimProtoOutput {
    fn name(&self) -> &str {
        &self.name
    }
    fn device_id(&self) -> &str {
        &self.device_id
    }
    fn output_type(&self) -> &str {
        "slimproto"
    }

    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities::v1(true, true, false, true, true, false).with_linear_volume(65536)
    }

    /// Native SlimProto has no internal next-track staging yet (phase 3 wires
    /// `set_next_url` → `strm s` autostart). Rely on the poller's natural-end
    /// advance for now so a single-track Repeat queue still loops.
    fn supports_internal_gapless(&self) -> bool {
        false
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        let (port, path) = parse_stream_url(media.url)
            .ok_or_else(|| format!("slimproto: unparseable stream url {}", media.url))?;
        // #6475 — annoncer le codec réellement servi, et pour un WAV/AIFF ses
        // vrais paramètres et la seule tranche des échantillons. Ce qu'on ne
        // sait pas annoncer avec certitude est refusé : un WAV annoncé FLAC
        // reste muet, un en-tête joué comme du son est du bruit.
        let annonce = crate::slimproto::annonce_pour(&FluxServi {
            mime_type: media.mime_type,
            url: media.url,
            sample_rate: media.sample_rate,
            bit_depth: media.bit_depth,
            channels: media.channels,
            file_path: media.file_path,
            file_size: media.file_size,
        })
        .map_err(|motif| {
            tracing::warn!(
                device_id = %self.device_id,
                mime = media.mime_type,
                url = media.url,
                %motif,
                "slimproto_flux_refuse"
            );
            format!("slimproto : ce flux ne peut pas être annoncé au lecteur ({motif})")
        })?;
        let format = annonce.format;
        tracing::info!(
            device_id = %self.device_id,
            mime = media.mime_type,
            codec = %char::from(format.codec),
            pcm = %String::from_utf8_lossy(&[format.taille, format.frequence, format.canaux, format.boutisme]),
            tranche = ?annonce.tranche,
            "slimproto_strm_s_format"
        );
        self.send(build_strm_start(port, &path, annonce)).await?;
        {
            let mut current = self.current_media.lock().await;
            current.uri = Some(media.url.to_string());
            current.duration_ms = media.duration_ms.unwrap_or(0);
            current.title = media.title.map(str::to_string);
            current.artist = media.artist.map(str::to_string);
        }
        lock_playback(&self.playback).begin_playback();
        self.flux_pose.store(true, Ordering::Relaxed);
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        self.send(strm_control(b'p')).await?;
        lock_playback(&self.playback).pause();
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        // #6476 — refuser plutôt qu'envoyer un `strm u` muet : l'orchestrateur
        // lit alors `device_released_on_pause()` et rétablit la piste.
        if self.sans_flux_a_reprendre() {
            tracing::info!(device_id = %self.device_id, "slimproto_reprise_sans_flux");
            return Err("slimproto: le lecteur n'a plus de flux à reprendre".into());
        }
        self.send(strm_control(b'u')).await?;
        lock_playback(&self.playback).resume();
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        // Best-effort: the player may already be gone; ignore a closed channel.
        let _ = self.send(strm_control(b'q')).await;
        lock_playback(&self.playback).stop();
        self.flux_pose.store(false, Ordering::Relaxed);
        Ok(())
    }

    /// #6476 — après une reconnexion de Squeezelite (ou un arrêt), le lecteur
    /// n'a plus le flux : l'orchestrateur doit le rétablir à la position
    /// (`strm s`), pas le « reprendre » par un `strm u` qui ne joue rien.
    fn device_released_on_pause(&self) -> bool {
        self.sans_flux_a_reprendre()
    }

    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Err("seek not supported on SlimProto".into())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        // SlimProto digital gain is fixed-point with 65536 = unity.
        let volume = volume.clamp(0.0, 1.0);
        let g = (volume * 65536.0).round() as u32;
        self.send(ServerMessage::Audg {
            left_gain: g,
            right_gain: g,
            digital_volume: 1,
        })
        .await?;
        *self.volume.lock().await = volume;
        self.muted.store(false, Ordering::Relaxed);
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        let volume = *self.volume.lock().await;
        let g = if muted {
            0
        } else {
            (volume * 65536.0).round() as u32
        };
        self.send(ServerMessage::Audg {
            left_gain: g,
            right_gain: g,
            digital_volume: 1,
        })
        .await?;
        self.muted.store(muted, Ordering::Relaxed);
        Ok(())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let position_ms = {
            let reg = self.players.lock().await;
            reg.get(&self.mac_str).map(|p| p.elapsed_ms as u64)
        };
        let (state, ended_naturally) = {
            let playback = lock_playback(&self.playback);
            (playback.transport, playback.ended_naturally)
        };
        let (duration_ms, current_uri, track_title, track_artist) = {
            let current = self.current_media.lock().await;
            (
                current.duration_ms,
                current.uri.clone(),
                current.title.clone(),
                current.artist.clone(),
            )
        };
        let volume = *self.volume.lock().await;
        Ok(OutputStatus {
            state,
            position_ms: position_ms.unwrap_or(0),
            duration_ms,
            volume,
            muted: self.muted.load(Ordering::Relaxed),
            current_uri,
            track_title,
            track_artist,
            ended_naturally,
            // A renderer plays at 1x: keep the poller's wall-clock guards.
            realtime: true,
            // Aucune sortie hors la locale ne produit du DoP : le DSD y part
            // tel quel ou transcode, jamais empaquete dans du PCM 24 bits.
            dop_active: false,
        })
    }

    async fn is_available(&self) -> bool {
        let reg = self.players.lock().await;
        reg.contains_key(&self.mac_str)
    }

    /// Best-effort diagnostics for remote debugging of a tester's player
    /// (Sandro): last STAT event, position, bytes and staleness. Uses a
    /// non-blocking `try_lock` so it never stalls a status call.
    fn diagnostics_json(&self) -> Option<serde_json::Value> {
        let reg = self.players.try_lock().ok()?;
        let p = reg.get(&self.mac_str)?;
        let playback = lock_playback(&self.playback);
        Some(serde_json::json!({
            "mac": self.mac_str,
            "transport": match playback.transport {
                TransportState::Playing => "playing",
                TransportState::Paused => "paused",
                TransportState::Transitioning => "transitioning",
                TransportState::Stopped => "stopped",
            },
            "decoder_finished": playback.decoder_finished,
            "ended_naturally": playback.ended_naturally,
            "playback_error": playback.failure.map(|failure| failure.diagnostic()),
            "elapsed_ms": p.elapsed_ms,
            "bytes_received": p.bytes_received,
            "last_stat_event": String::from_utf8_lossy(&p.last_event).trim_end_matches('\0'),
            "last_stat_secs_ago": p.last_stat.elapsed().as_secs(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Instant;

    use super::*;
    use crate::slimproto::SlimProtoPlayer;

    #[tokio::test]
    async fn stat_pilote_le_statut_complet_et_la_fin_naturelle() {
        let mac_str = "00:11:22:33:44:55".to_string();
        let playback = new_playback_state();
        let players = Arc::new(Mutex::new(HashMap::new()));
        players.lock().await.insert(
            mac_str.clone(),
            SlimProtoPlayer {
                mac: [0x00, 0x11, 0x22, 0x33, 0x44, 0x55],
                mac_str: mac_str.clone(),
                name: "Squeezelite Test".into(),
                addr: "127.0.0.1:12345".parse().unwrap(),
                device_type: 12,
                firmware_version: 1,
                last_stat: Instant::now(),
                elapsed_ms: 0,
                bytes_received: 0,
                last_event: [0; 4],
                playback: Arc::clone(&playback),
            },
        );

        let channels = Arc::new(Mutex::new(HashMap::new()));
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        channels.lock().await.insert(mac_str.clone(), tx);
        let output = SlimProtoOutput::new_with_playback(
            "Squeezelite Test".into(),
            "slimproto-test".into(),
            mac_str.clone(),
            Arc::clone(&players),
            channels,
            Arc::clone(&playback),
        );

        output
            .play_media(&PlayMedia {
                url: "http://127.0.0.1:8080/audio/42",
                mime_type: "audio/flac",
                title: Some("Piste témoin"),
                artist: Some("Artiste témoin"),
                duration_ms: Some(180_000),
                ..PlayMedia::default()
            })
            .await
            .unwrap();
        rx.recv().await.expect("commande strm s");

        let pending = output.get_status().await.unwrap();
        assert_eq!(pending.state, TransportState::Transitioning);
        assert_eq!(pending.duration_ms, 180_000);
        assert_eq!(
            pending.current_uri.as_deref(),
            Some("http://127.0.0.1:8080/audio/42")
        );
        assert_eq!(pending.track_title.as_deref(), Some("Piste témoin"));
        assert_eq!(pending.track_artist.as_deref(), Some("Artiste témoin"));

        {
            let mut player = players.lock().await;
            let player = player.get_mut(&mac_str).unwrap();
            player.elapsed_ms = 12_345;
            player.last_event = *b"STMs";
            lock_playback(&player.playback).apply_stat(*b"STMs");
        }
        let playing = output.get_status().await.unwrap();
        assert_eq!(playing.state, TransportState::Playing);
        assert_eq!(playing.position_ms, 12_345);
        assert!(!playing.ended_naturally);

        lock_playback(&playback).apply_stat(*b"STMd");
        let decoded = output.get_status().await.unwrap();
        assert_eq!(decoded.state, TransportState::Playing);
        assert!(!decoded.ended_naturally);

        lock_playback(&playback).apply_stat(*b"STMu");
        let drained = output.get_status().await.unwrap();
        assert_eq!(drained.state, TransportState::Stopped);
        assert!(drained.ended_naturally);
    }

    /// Sortie branchée sur un faux lecteur : rend la sortie et le récepteur
    /// des commandes que le lecteur recevrait.
    async fn sortie_de_banc() -> (SlimProtoOutput, tokio::sync::mpsc::Receiver<ServerMessage>) {
        let mac_str = "a0:36:9f:00:64:75".to_string();
        let players = Arc::new(Mutex::new(HashMap::new()));
        let channels = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        channels.lock().await.insert(mac_str.clone(), tx);
        let output = SlimProtoOutput::new(
            "Squeezelite AudioLinux".into(),
            format!("slimproto-{mac_str}"),
            mac_str,
            players,
            channels,
        );
        (output, rx)
    }

    /// Le `strm s` reçu : les six premiers octets (autostart, codec, taille,
    /// fréquence, canaux, boutisme) et la requête HTTP qui le termine.
    fn strm_s(msg: ServerMessage) -> ([u8; 6], String) {
        match msg {
            ServerMessage::Strm { command, payload } => {
                assert_eq!(command, b's', "une lecture part en strm s");
                (
                    payload[..6].try_into().unwrap(),
                    String::from_utf8_lossy(&payload[23..]).into_owned(),
                )
            }
            other => panic!("strm s attendu, reçu {other:?}"),
        }
    }

    /// Un WAV canonique : 44 octets d'en-tête puis `n` trames de silence.
    fn wav(canaux: u16, frequence: u32, bits: u16, trames: usize) -> tempfile::NamedTempFile {
        use std::io::Write;
        let align = canaux * bits / 8;
        let donnees = trames * usize::from(align);
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&(36 + donnees as u32).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&canaux.to_le_bytes());
        v.extend_from_slice(&frequence.to_le_bytes());
        v.extend_from_slice(&(frequence * u32::from(align)).to_le_bytes());
        v.extend_from_slice(&align.to_le_bytes());
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(donnees as u32).to_le_bytes());
        v.resize(44 + donnees, 0);
        let mut f = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
        f.write_all(&v).unwrap();
        f
    }

    /// #6475 (fil 2322) — LE défaut : une piste WAV locale était annoncée `f`
    /// (FLAC), et Squeezelite rejetait chaque trame. Elle part en `p` avec
    /// les paramètres de SON en-tête, et la requête ne demande que les
    /// échantillons : Squeezelite ne saute jamais l'en-tête d'un flux HTTP.
    #[tokio::test]
    async fn une_piste_wav_part_en_pcm_sans_son_en_tete() {
        let (output, mut rx) = sortie_de_banc().await;
        let fichier = wav(2, 96_000, 24, 100);
        let chemin = fichier.path().to_str().unwrap().to_string();
        output
            .play_media(&PlayMedia {
                url: "http://192.168.1.10:8888/stream/35e7a2df.wav",
                mime_type: "audio/wav",
                // Le média dit 44,1/16 : c'est l'en-tête du fichier qui fait foi.
                sample_rate: Some(44_100),
                bit_depth: Some(16),
                channels: Some(2),
                file_path: Some(&chemin),
                file_size: Some(44 + 600),
                ..PlayMedia::default()
            })
            .await
            .unwrap();
        let (entete, requete) = strm_s(rx.recv().await.expect("strm s"));
        assert_eq!(
            entete,
            [b'1', b'p', b'2', b'9', b'2', b'1'],
            "WAV 96/24 stéréo : codec p, taille '2' (24 bits), fréquence '9' \
             (96 000), 2 canaux, petit-boutiste '1'"
        );
        assert_eq!(
            requete, "GET /stream/35e7a2df.wav HTTP/1.0\r\nRange: bytes=44-643\r\n\r\n",
            "la requête ne demande que les 600 octets d'échantillons"
        );
    }

    /// Un WAV fabriqué par Tune (DSP, conversion) n'est pas le fichier source :
    /// rien ne dit son en-tête. Refusé, jamais annoncé au hasard.
    #[tokio::test]
    async fn un_wav_qui_n_est_pas_le_fichier_source_est_refuse() {
        let (output, mut rx) = sortie_de_banc().await;
        let fichier = wav(2, 44_100, 16, 10);
        let chemin = fichier.path().to_str().unwrap().to_string();
        for (file_path, file_size) in [
            (None, Some(84)),
            (Some(chemin.as_str()), None),
            (Some(chemin.as_str()), Some(9_999)),
        ] {
            let refus = output
                .play_media(&PlayMedia {
                    url: "http://192.168.1.10:8888/stream/dsp.wav",
                    mime_type: "audio/wav",
                    sample_rate: Some(44_100),
                    bit_depth: Some(16),
                    channels: Some(2),
                    file_path,
                    file_size,
                    ..PlayMedia::default()
                })
                .await;
            assert!(refus.is_err(), "{file_path:?} / {file_size:?}");
        }
        assert!(rx.try_recv().is_err(), "aucun strm s n'est parti");
    }

    /// Les codecs compressés gardent `'?'` (comme LMS) et le flux entier.
    #[test]
    fn le_codec_annonce_suit_le_format_servi() {
        let codec = |mime: &str, url: &str, bd: Option<u32>| {
            crate::slimproto::annonce_pour(&FluxServi {
                mime_type: mime,
                url,
                bit_depth: bd,
                ..FluxServi::default()
            })
            .map(|a| (a.format.codec, a.format.taille, a.tranche))
        };
        assert_eq!(codec("audio/flac", "", None), Ok((b'f', b'?', None)));
        assert_eq!(codec("audio/mpeg", "", None), Ok((b'm', b'?', None)));
        assert_eq!(codec("audio/ogg", "", None), Ok((b'o', b'?', None)));
        assert_eq!(codec("audio/opus", "", None), Ok((b'u', b'?', None)));
        assert_eq!(codec("audio/aac", "", None), Ok((b'a', b'?', None)));
        assert_eq!(codec("application/x-dsd", "", None), Ok((b'd', b'?', None)));
        assert_eq!(codec("audio/x-ms-wma", "", None), Ok((b'w', b'?', None)));
        assert_eq!(codec("audio/mp4", "", Some(16)), Ok((b'l', b'?', None)));
        assert_eq!(codec("audio/mp4", "", None), Ok((b'a', b'?', None)));
        assert_eq!(
            codec("", "http://h:1/stream/x.mp3", None),
            Ok((b'm', b'?', None))
        );
        assert!(codec("application/octet-stream", "http://h:1/stream/x", None).is_err());
        // PCM brut sans en-tête : paramètres requis, aucun '?'.
        let l16 = crate::slimproto::annonce_pour(&FluxServi {
            mime_type: "audio/L16;rate=48000;channels=2",
            ..FluxServi::default()
        })
        .unwrap();
        assert_eq!(
            [
                l16.format.codec,
                l16.format.taille,
                l16.format.frequence,
                l16.format.canaux,
                l16.format.boutisme
            ],
            [b'p', b'1', b'4', b'2', b'0']
        );
        assert!(
            crate::slimproto::annonce_pour(&FluxServi {
                mime_type: "audio/L16",
                ..FluxServi::default()
            })
            .is_err(),
            "PCM brut sans fréquence : refusé"
        );
    }
}
