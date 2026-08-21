use tracing::info;

use super::traits::{OutputStatus, OutputTarget, TransportState};

/// rust_cast opens a plain blocking `TcpStream` with no connect/read timeout:
/// a Chromecast that vanished from the network (sleep, Wi-Fi drop — some flap
/// every few minutes) turns that connect into a minutes-long hang that
/// strands a blocking-pool thread. Probe with a bounded connect first so a
/// dead host fails fast.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// `true` when `host:port` accepts a TCP connection within `timeout`. Returns
/// `true` on resolution failure so rust_cast surfaces the real error itself.
fn probe_reachable(host: &str, port: u16, timeout: std::time::Duration) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    match (host, port).to_socket_addrs() {
        Ok(mut addrs) => match addrs.next() {
            Some(addr) => TcpStream::connect_timeout(&addr, timeout).is_ok(),
            None => true,
        },
        Err(_) => true,
    }
}

/// Session déjà ouverte sur l'appareil pour l'application `app_id`, s'il y en
/// a une : `(transport_id, session_id)`, de quoi charger un média sans rien
/// relancer.
///
/// Envoyer `LAUNCH` à un récepteur qui fait DÉJÀ tourner l'application
/// demandée le redémarre : l'enceinte rejoue son carillon de démarrage. C'est
/// ce que FabienM entend à chaque piste (#1953), puisque `play_url` lançait
/// l'application sans jamais regarder si elle tournait. Les autres télécommandes
/// Cast font ce contrôle (pychromecast n'émet `LAUNCH` que si `app_id` diffère
/// de celui en cours) ; nous ne le faisions pas.
///
/// Rend `None` si l'appareil est au repos ou occupé par une AUTRE application
/// (YouTube, Spotify…) : il faut alors bel et bien lancer la nôtre.
fn reusable_session(
    apps: &[rust_cast::channels::receiver::Application],
    app_id: &str,
) -> Option<(String, String)> {
    apps.iter()
        .find(|a| a.app_id == app_id)
        .map(|a| (a.transport_id.clone(), a.session_id.clone()))
}

pub struct ChromecastOutput {
    name: String,
    device_id: String,
    host: String,
    port: u16,
}

impl ChromecastOutput {
    pub fn new(name: String, device_id: String, host: String, port: u16) -> Self {
        Self {
            name,
            device_id,
            host,
            port,
        }
    }
}

#[async_trait::async_trait]
impl OutputTarget for ChromecastOutput {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        "chromecast"
    }

    /// Chromecast does not consume `set_next_media` (no cast-queue / autoplay
    /// staging is implemented — `set_next_url` is the no-op default). Returning
    /// true here made the poller arm the gapless guard, which orphaned the
    /// staged track and suppressed the natural-end advance: playback stalled
    /// ~30-60s at every track boundary (Rhorn, Chromecast Audio, forum #1072).
    /// Rely on the poller's natural-end fallback instead, like slimproto.
    fn supports_internal_gapless(&self) -> bool {
        false
    }

    fn host(&self) -> Option<&str> {
        Some(&self.host)
    }

    async fn play_media(&self, media: &super::traits::PlayMedia<'_>) -> Result<(), String> {
        self.play_url(media.url, media.mime_type, media.title, media.artist)
            .await
    }

    async fn play_url(
        &self,
        url: &str,
        mime_type: &str,
        title: Option<&str>,
        artist: Option<&str>,
    ) -> Result<(), String> {
        let url = url.to_string();
        let mime = mime_type.to_string();
        let title = title.map(String::from);
        let artist = artist.map(String::from);
        let host = self.host.clone();
        let port = self.port;
        let name = self.name.clone();

        tokio::task::spawn_blocking(move || {
            let device = rust_cast::CastDevice::connect_without_host_verification(&host, port)
                .map_err(|e| format!("chromecast connect: {e}"))?;

            device
                .connection
                .connect("receiver-0")
                .map_err(|e| format!("connect receiver: {e}"))?;

            // Réutiliser la session en cours plutôt que de relancer le
            // récepteur : un LAUNCH sur une application déjà lancée la
            // redémarre, et l'enceinte carillonne (#1953). Un GET_STATUS
            // en échec retombe sur le lancement — le comportement d'avant.
            let app_id =
                rust_cast::channels::receiver::CastDeviceApp::DefaultMediaReceiver.to_string();
            let existing = device
                .receiver
                .get_status()
                .ok()
                .and_then(|s| reusable_session(&s.applications, &app_id));

            let (transport_id, session_id, session_reused) = match existing {
                Some((transport_id, session_id)) => (transport_id, session_id, true),
                None => {
                    let app = device
                        .receiver
                        .launch_app(
                            &rust_cast::channels::receiver::CastDeviceApp::DefaultMediaReceiver,
                        )
                        .map_err(|e| format!("launch app: {e}"))?;
                    (app.transport_id, app.session_id, false)
                }
            };

            device
                .connection
                .connect(&transport_id)
                .map_err(|e| format!("connect transport: {e}"))?;

            device
                .media
                .load(
                    &transport_id,
                    &session_id,
                    &rust_cast::channels::media::Media {
                        content_id: url.clone(),
                        content_type: mime,
                        stream_type: rust_cast::channels::media::StreamType::Buffered,
                        duration: None,
                        metadata: Some(rust_cast::channels::media::Metadata::MusicTrack(
                            rust_cast::channels::media::MusicTrackMediaMetadata {
                                album_name: None,
                                title,
                                album_artist: None,
                                artist,
                                composer: None,
                                track_number: None,
                                disc_number: None,
                                images: vec![],
                                release_date: None,
                            },
                        )),
                    },
                )
                .map_err(|e| format!("load media: {e}"))?;

            // `session_reused=false` sur une piste qui n'est pas la première
            // d'une écoute désigne le vrai coupable du carillon : la session
            // n'a pas survécu au changement de piste.
            info!(device = %name, url, session_reused, "chromecast_play");
            Ok::<(), String>(())
        })
        .await
        .map_err(|e| format!("spawn: {e}"))??;
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        let host = self.host.clone();
        let port = self.port;
        tokio::task::spawn_blocking(move || {
            let device = rust_cast::CastDevice::connect_without_host_verification(&host, port)
                .map_err(|e| format!("connect: {e}"))?;
            device
                .connection
                .connect("receiver-0")
                .map_err(|e| format!("connect receiver: {e}"))?;

            let status = device
                .receiver
                .get_status()
                .map_err(|e| format!("status: {e}"))?;
            if let Some(app) = status.applications.first() {
                device
                    .connection
                    .connect(&app.transport_id)
                    .map_err(|e| format!("connect transport: {e}"))?;
                let media_status = device
                    .media
                    .get_status(&app.transport_id, None)
                    .map_err(|e| format!("media status: {e}"))?;
                if let Some(entry) = media_status.entries.first() {
                    device
                        .media
                        .pause(&app.transport_id, entry.media_session_id)
                        .map_err(|e| format!("pause: {e}"))?;
                }
            }
            Ok::<(), String>(())
        })
        .await
        .map_err(|e| format!("spawn: {e}"))?
    }

    async fn resume(&self) -> Result<(), String> {
        let host = self.host.clone();
        let port = self.port;
        tokio::task::spawn_blocking(move || {
            let device = rust_cast::CastDevice::connect_without_host_verification(&host, port)
                .map_err(|e| format!("connect: {e}"))?;
            device
                .connection
                .connect("receiver-0")
                .map_err(|e| format!("connect receiver: {e}"))?;

            let status = device
                .receiver
                .get_status()
                .map_err(|e| format!("status: {e}"))?;
            if let Some(app) = status.applications.first() {
                device
                    .connection
                    .connect(&app.transport_id)
                    .map_err(|e| format!("connect transport: {e}"))?;
                let media_status = device
                    .media
                    .get_status(&app.transport_id, None)
                    .map_err(|e| format!("media status: {e}"))?;
                if let Some(entry) = media_status.entries.first() {
                    device
                        .media
                        .play(&app.transport_id, entry.media_session_id)
                        .map_err(|e| format!("play: {e}"))?;
                }
            }
            Ok::<(), String>(())
        })
        .await
        .map_err(|e| format!("spawn: {e}"))?
    }

    async fn stop(&self) -> Result<(), String> {
        let host = self.host.clone();
        let port = self.port;
        tokio::task::spawn_blocking(move || {
            let device = rust_cast::CastDevice::connect_without_host_verification(&host, port)
                .map_err(|e| format!("connect: {e}"))?;
            device
                .connection
                .connect("receiver-0")
                .map_err(|e| format!("connect receiver: {e}"))?;
            let status = device
                .receiver
                .get_status()
                .map_err(|e| format!("status: {e}"))?;
            if let Some(app) = status.applications.first() {
                device
                    .receiver
                    .stop_app(&app.session_id)
                    .map_err(|e| format!("stop: {e}"))?;
            }
            Ok::<(), String>(())
        })
        .await
        .map_err(|e| format!("spawn: {e}"))?
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        let host = self.host.clone();
        let port = self.port;
        let position_secs = position_ms as f32 / 1000.0;
        tokio::task::spawn_blocking(move || {
            let device = rust_cast::CastDevice::connect_without_host_verification(&host, port)
                .map_err(|e| format!("connect: {e}"))?;
            device
                .connection
                .connect("receiver-0")
                .map_err(|e| format!("connect receiver: {e}"))?;
            let status = device
                .receiver
                .get_status()
                .map_err(|e| format!("status: {e}"))?;
            if let Some(app) = status.applications.first() {
                device
                    .connection
                    .connect(&app.transport_id)
                    .map_err(|e| format!("connect transport: {e}"))?;
                let media_status = device
                    .media
                    .get_status(&app.transport_id, None)
                    .map_err(|e| format!("media status: {e}"))?;
                if let Some(entry) = media_status.entries.first() {
                    device
                        .media
                        .seek(
                            &app.transport_id,
                            entry.media_session_id,
                            Some(position_secs),
                            None,
                        )
                        .map_err(|e| format!("seek: {e}"))?;
                }
            }
            Ok::<(), String>(())
        })
        .await
        .map_err(|e| format!("spawn: {e}"))?
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        let host = self.host.clone();
        let port = self.port;
        let level = volume as f32;
        tokio::task::spawn_blocking(move || {
            let device = rust_cast::CastDevice::connect_without_host_verification(&host, port)
                .map_err(|e| format!("connect: {e}"))?;
            device
                .connection
                .connect("receiver-0")
                .map_err(|e| format!("connect receiver: {e}"))?;
            device
                .receiver
                .set_volume(rust_cast::channels::receiver::Volume {
                    level: Some(level),
                    muted: Some(false),
                })
                .map_err(|e| format!("volume: {e}"))?;
            Ok::<(), String>(())
        })
        .await
        .map_err(|e| format!("spawn: {e}"))?
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        let host = self.host.clone();
        let port = self.port;
        tokio::task::spawn_blocking(move || {
            let device = rust_cast::CastDevice::connect_without_host_verification(&host, port)
                .map_err(|e| format!("connect: {e}"))?;
            device
                .connection
                .connect("receiver-0")
                .map_err(|e| format!("connect receiver: {e}"))?;
            device
                .receiver
                .set_volume(rust_cast::channels::receiver::Volume {
                    level: None,
                    muted: Some(muted),
                })
                .map_err(|e| format!("mute: {e}"))?;
            Ok::<(), String>(())
        })
        .await
        .map_err(|e| format!("spawn: {e}"))?
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let host = self.host.clone();
        let port = self.port;
        tokio::task::spawn_blocking(move || {
            if !probe_reachable(&host, port, PROBE_TIMEOUT) {
                return Ok(OutputStatus::default());
            }
            let device = match rust_cast::CastDevice::connect_without_host_verification(&host, port)
            {
                Ok(d) => d,
                Err(_) => return Ok(OutputStatus::default()),
            };
            if device.connection.connect("receiver-0").is_err() {
                return Ok(OutputStatus::default());
            }

            let recv_status = device
                .receiver
                .get_status()
                .map_err(|e| format!("status: {e}"))?;

            let volume = recv_status.volume.level.unwrap_or(0.5) as f64;
            let muted = recv_status.volume.muted.unwrap_or(false);

            let Some(app) = recv_status.applications.first() else {
                return Ok(OutputStatus {
                    ended_naturally: false,
                    volume,
                    muted,
                    ..Default::default()
                });
            };

            if device.connection.connect(&app.transport_id).is_err() {
                return Ok(OutputStatus {
                    ended_naturally: false,
                    volume,
                    muted,
                    ..Default::default()
                });
            }

            let media_status = match device.media.get_status(&app.transport_id, None) {
                Ok(s) => s,
                Err(_) => {
                    return Ok(OutputStatus {
                        ended_naturally: false,
                        volume,
                        muted,
                        ..Default::default()
                    });
                }
            };

            let Some(entry) = media_status.entries.first() else {
                return Ok(OutputStatus {
                    ended_naturally: false,
                    volume,
                    muted,
                    ..Default::default()
                });
            };

            let state = match entry.player_state {
                rust_cast::channels::media::PlayerState::Playing => TransportState::Playing,
                rust_cast::channels::media::PlayerState::Paused => TransportState::Paused,
                rust_cast::channels::media::PlayerState::Buffering => TransportState::Transitioning,
                _ => TransportState::Stopped,
            };

            let position_ms = entry
                .current_time
                .map(|t| (t as f64 * 1000.0) as u64)
                .unwrap_or(0);
            let duration_ms = entry
                .media
                .as_ref()
                .and_then(|m| m.duration)
                .map(|d| (d * 1000.0) as u64)
                .unwrap_or(0);

            let current_uri = entry.media.as_ref().map(|m| m.content_id.clone());

            // The receiver reports `idle_reason = FINISHED` when a track played
            // to its end (vs CANCELLED / INTERRUPTED / ERROR). Surface that as
            // `ended_naturally` so the poller advances to the next track right
            // away. Without it, every FINISHED looked like a plain Stopped state
            // and the poller only advanced via its 30 s wall-clock fallback —
            // Chromecast albums stalled 30-60 s between tracks (#1072, Rhorn).
            let ended_naturally = matches!(
                entry.idle_reason,
                Some(rust_cast::channels::media::IdleReason::Finished)
            );

            Ok(OutputStatus {
                state,
                position_ms,
                duration_ms,
                volume,
                muted,
                current_uri,
                track_title: None,
                track_artist: None,
                ended_naturally,
                // A renderer plays at 1x: keep the poller's wall-clock guards.
                realtime: true,
                // Aucune sortie hors la locale ne produit du DoP : le DSD y part
                // tel quel ou transcode, jamais empaquete dans du PCM 24 bits.
                dop_active: false,
            })
        })
        .await
        .map_err(|e| format!("spawn: {e}"))?
    }

    async fn is_available(&self) -> bool {
        let host = self.host.clone();
        let port = self.port;
        tokio::task::spawn_blocking(move || {
            probe_reachable(&host, port, PROBE_TIMEOUT)
                && rust_cast::CastDevice::connect_without_host_verification(&host, port).is_ok()
        })
        .await
        .unwrap_or(false)
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    #[test]
    fn reachable_host_probes_true() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(probe_reachable(
            "127.0.0.1",
            port,
            std::time::Duration::from_millis(500)
        ));
    }

    #[test]
    fn dead_host_probes_false_fast() {
        // Bind then drop: the port is closed, connect is refused immediately.
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let start = std::time::Instant::now();
        assert!(!probe_reachable(
            "127.0.0.1",
            port,
            std::time::Duration::from_millis(500)
        ));
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn unresolvable_host_falls_through_true() {
        // rust_cast must surface the real error itself.
        assert!(probe_reachable(
            "definitely-not-a-real-host.invalid",
            8009,
            std::time::Duration::from_millis(500)
        ));
    }
}

/// Non-régression #1953 : une piste ne doit pas relancer le récepteur.
///
/// `LAUNCH` sur une application déjà en cours la redémarre, et l'enceinte
/// rejoue son carillon de démarrage — FabienM l'entendait à chaque titre.
/// Ces tests portent sur la DÉCISION (relancer ou réutiliser), la seule
/// partie vérifiable sans matériel : ils ne prouvent rien de l'audible.
#[cfg(test)]
mod session_reuse_tests {
    use super::*;
    use rust_cast::channels::receiver::{Application, CastDeviceApp};

    const DEFAULT_MEDIA_RECEIVER: &str = "CC1AD845";

    fn app(app_id: &str) -> Application {
        Application {
            app_id: app_id.to_string(),
            session_id: format!("session-{app_id}"),
            transport_id: format!("transport-{app_id}"),
            namespaces: vec![],
            display_name: app_id.to_string(),
            status_text: String::new(),
        }
    }

    #[test]
    fn app_id_du_lecteur_par_defaut_est_bien_celui_interroge() {
        // La comparaison ne vaut que si les deux côtés parlent du même id.
        assert_eq!(
            CastDeviceApp::DefaultMediaReceiver.to_string(),
            DEFAULT_MEDIA_RECEIVER
        );
    }

    #[test]
    fn session_en_cours_reutilisee_donc_aucun_relancement() {
        let apps = vec![app(DEFAULT_MEDIA_RECEIVER)];
        let found = reusable_session(&apps, DEFAULT_MEDIA_RECEIVER);
        assert_eq!(
            found,
            Some((
                "transport-CC1AD845".to_string(),
                "session-CC1AD845".to_string()
            )),
            "le récepteur tourne déjà : il faut charger dans SA session, pas la relancer"
        );
    }

    #[test]
    fn appareil_au_repos_impose_un_lancement() {
        assert_eq!(reusable_session(&[], DEFAULT_MEDIA_RECEIVER), None);
    }

    #[test]
    fn autre_application_impose_un_lancement() {
        // YouTube occupe l'appareil : reprendre SA session chargerait le média
        // dans une application qui ne sait pas le lire.
        let apps = vec![app("233637DE")];
        assert_eq!(reusable_session(&apps, DEFAULT_MEDIA_RECEIVER), None);
    }

    #[test]
    fn le_lecteur_est_retrouve_meme_derriere_une_autre_application() {
        let apps = vec![app("233637DE"), app(DEFAULT_MEDIA_RECEIVER)];
        assert!(reusable_session(&apps, DEFAULT_MEDIA_RECEIVER).is_some());
    }
}

/// Regression tests for forum bug #1185: Chromecast devices presenting a
/// self-signed X.509 **v1** certificate were rejected during the TLS
/// handshake with `invalid peer certificate: Other(OtherError(
/// UnsupportedCertVersion))` — rustls-webpki refuses to parse v1 certs, so
/// the stock signature-verification helpers failed before rust_cast's
/// accept-everything `verify_server_cert` was even relevant. Fixed by the
/// vendored rust_cast patch (vendor/rust_cast, `accept_unparseable_cert`).
#[cfg(test)]
mod cast_tls_tests {
    use rust_cast::NoCertificateVerification;
    use rustls::DigitallySignedStruct;
    use rustls::client::danger::ServerCertVerifier;
    use rustls::internal::msgs::codec::{Codec, Reader};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

    /// Genuine X.509 v1 self-signed cert (what 1st/2nd-gen Chromecasts and
    /// Chromecast Audio present).
    const CERT_V1: &[u8] = include_bytes!("../../tests/fixtures/chromecast_x509_v1.der");
    /// X.509 v3 control cert (parseable by webpki).
    const CERT_V3: &[u8] = include_bytes!("../../tests/fixtures/chromecast_x509_v3.der");

    /// DigitallySignedStruct::new is pub(crate); build one through the wire
    /// codec: scheme (u16) + u16-length-prefixed signature bytes.
    fn dummy_dss(scheme: u16) -> DigitallySignedStruct {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&scheme.to_be_bytes());
        bytes.extend_from_slice(&256u16.to_be_bytes());
        bytes.extend_from_slice(&[0u8; 256]);
        DigitallySignedStruct::read(&mut Reader::init(&bytes)).unwrap()
    }

    /// rsa_pkcs1_sha256 — what a TLS 1.2 Chromecast handshake uses.
    const RSA_PKCS1_SHA256: u16 = 0x0401;
    /// rsa_pss_rsae_sha256 — a scheme valid in TLS 1.3.
    const RSA_PSS_RSAE_SHA256: u16 = 0x0804;

    #[test]
    fn stock_helper_rejects_v1_cert_proving_the_bug() {
        // Control: the unpatched code path (rustls' own helper) fails on the
        // v1 cert at *parse* time — this is exactly the #1185 failure mode.
        let cert = CertificateDer::from(CERT_V1);
        let err = rustls::crypto::verify_tls12_signature(
            b"message",
            &cert,
            &dummy_dss(RSA_PKCS1_SHA256),
            &rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms,
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                rustls::Error::InvalidCertificate(rustls::CertificateError::Other(_))
            ),
            "expected UnsupportedCertVersion-class parse error, got: {err:?}"
        );
    }

    #[test]
    fn patched_verifier_accepts_v1_cert_tls12() {
        let cert = CertificateDer::from(CERT_V1);
        let res = NoCertificateVerification.verify_tls12_signature(
            b"message",
            &cert,
            &dummy_dss(RSA_PKCS1_SHA256),
        );
        assert!(
            res.is_ok(),
            "v1 cert must be tolerated (LAN, unverified): {res:?}"
        );
    }

    #[test]
    fn patched_verifier_accepts_v1_cert_tls13() {
        let cert = CertificateDer::from(CERT_V1);
        let res = NoCertificateVerification.verify_tls13_signature(
            b"message",
            &cert,
            &dummy_dss(RSA_PSS_RSAE_SHA256),
        );
        assert!(
            res.is_ok(),
            "v1 cert must be tolerated (LAN, unverified): {res:?}"
        );
    }

    #[test]
    fn patched_verifier_still_rejects_bad_signature_on_parseable_cert() {
        // The patch must NOT blanket-accept: a parseable (v3) cert with a
        // garbage signature keeps failing the standard signature check.
        let cert = CertificateDer::from(CERT_V3);
        let res = NoCertificateVerification.verify_tls12_signature(
            b"message",
            &cert,
            &dummy_dss(RSA_PKCS1_SHA256),
        );
        assert!(
            res.is_err(),
            "bad signature on parseable cert must still fail"
        );
    }

    #[test]
    fn verify_server_cert_accepts_v1_cert() {
        let cert = CertificateDer::from(CERT_V1);
        let server_name = ServerName::try_from("192.168.1.75").unwrap();
        let res = NoCertificateVerification.verify_server_cert(
            &cert,
            &[],
            &server_name,
            &[],
            UnixTime::now(),
        );
        assert!(res.is_ok());
    }
}
