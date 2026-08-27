use std::{
    net::{IpAddr, SocketAddr},
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

use tokio::sync::Semaphore;
use tracing::info;

use super::traits::{OutputCapabilities, OutputStatus, OutputTarget, TransportState};

/// One Cast operation gets one global budget: DNS, every address attempt,
/// TLS and all protocol exchanges included.
const CAST_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CAST_COMMAND_WORKERS: usize = 4;
static CAST_COMMAND_SLOTS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_CAST_COMMAND_WORKERS)));

fn remaining_budget(deadline: Instant) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| "chromecast command deadline elapsed".to_string())
}

async fn resolve_cast_addresses(
    host: &str,
    port: u16,
    deadline: Instant,
) -> Result<Vec<SocketAddr>, String> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    let mut addresses: Vec<_> = tokio::time::timeout(
        remaining_budget(deadline)?,
        tokio::net::lookup_host((host, port)),
    )
    .await
    .map_err(|_| format!("chromecast resolution deadline elapsed for {host}"))?
    .map_err(|error| format!("chromecast resolve {host}: {error}"))?
    .collect();
    addresses.sort_unstable();
    addresses.dedup();
    if addresses.is_empty() {
        return Err(format!("chromecast resolve {host}: no address"));
    }
    Ok(addresses)
}

async fn run_cast_command<T, F>(
    host: String,
    port: u16,
    timeout: Duration,
    slots: Arc<Semaphore>,
    operation: F,
) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(rust_cast::CastDevice<'static>) -> Result<T, String> + Send + 'static,
{
    let deadline = Instant::now() + timeout;
    let permit = tokio::time::timeout(remaining_budget(deadline)?, slots.acquire_owned())
        .await
        .map_err(|_| "chromecast worker deadline elapsed".to_string())?
        .map_err(|_| "chromecast worker pool closed".to_string())?;
    let addresses = resolve_cast_addresses(&host, port, deadline).await?;
    let worker = tokio::task::spawn_blocking(move || {
        // A timed-out caller must not release capacity while its blocking
        // worker is still alive. The deadline socket makes this finite.
        let _permit = permit;
        let device = rust_cast::CastDevice::connect_without_host_verification_with_deadline(
            host, &addresses, deadline,
        )
        .map_err(|error| format!("chromecast connect: {error}"))?;
        operation(device)
    });

    tokio::time::timeout(remaining_budget(deadline)?, worker)
        .await
        .map_err(|_| "chromecast command deadline elapsed".to_string())?
        .map_err(|error| format!("chromecast worker: {error}"))?
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

/// Le média `LOAD` tel qu'il part sur le fil, construit à partir du contrat
/// `PlayMedia`. Fonction pure : c'est elle que les tests interrogent, faute de
/// pouvoir brancher un vrai récepteur Cast.
fn build_cast_media(media: &super::traits::PlayMedia<'_>) -> rust_cast::channels::media::Media {
    use rust_cast::channels::media::{Image, Media, Metadata, MusicTrackMediaMetadata, StreamType};

    // Le type de flux et la durée se décident ENSEMBLE, sinon la barre de
    // progression ment. Une webradio est infinie : `Buffered` fait croire au
    // récepteur qu'il tient un fichier borné, et une durée sur un flux sans fin
    // n'existe pas. Les deux se lisent du même `live_stream`, ici et nulle part
    // ailleurs, pour qu'on ne puisse pas corriger l'un en oubliant l'autre.
    let (stream_type, duration) = if media.live_stream {
        (StreamType::Live, None)
    } else {
        // 0 est la valeur « inconnu » de plusieurs lignes en base. Annoncer une
        // piste de durée nulle est pire que n'annoncer aucune durée : le
        // récepteur affiche une barre déjà terminée. Le champ Cast est en
        // SECONDES, `PlayMedia` en millisecondes.
        let seconds = media
            .duration_ms
            .filter(|ms| *ms > 0)
            .map(|ms| ms as f32 / 1000.0);
        (StreamType::Buffered, seconds)
    };

    // Même règle pour la numérotation : la ligne de bibliothèque stocke 0 pour
    // « inconnu », et une piste 0 sur 0 est un chiffre inventé.
    let positive = |n: Option<u32>| n.filter(|v| *v > 0);

    Media {
        content_id: media.url.to_string(),
        content_type: media.mime_type.to_string(),
        stream_type,
        duration,
        metadata: Some(Metadata::MusicTrack(MusicTrackMediaMetadata {
            album_name: media.album.map(String::from),
            title: media.title.map(String::from),
            // `PlayMedia` ne porte pas d'artiste d'album ni de compositeur ni de
            // date de sortie : les déduire de l'artiste de piste serait annoncer
            // une valeur que Tune n'a pas mesurée.
            album_artist: None,
            artist: media.artist.map(String::from),
            composer: None,
            track_number: positive(media.track_number),
            disc_number: positive(media.disc_number),
            // `cover_url` est déjà résolue en URL absolue par l'orchestrateur
            // (`resolve_cover_url`) : le Chromecast va la chercher lui-meme sur
            // le réseau, un chemin local ne lui servirait à rien.
            images: media
                .cover_url
                .map(|url| vec![Image::new(url.to_string())])
                .unwrap_or_default(),
            release_date: None,
        })),
    }
}

pub struct ChromecastOutput {
    name: String,
    device_id: String,
    host: String,
    port: u16,
    command_timeout: Duration,
    command_slots: Arc<Semaphore>,
}

impl ChromecastOutput {
    pub fn new(name: String, device_id: String, host: String, port: u16) -> Self {
        Self {
            name,
            device_id,
            host,
            port,
            command_timeout: CAST_COMMAND_TIMEOUT,
            command_slots: Arc::clone(&CAST_COMMAND_SLOTS),
        }
    }

    #[cfg(test)]
    fn with_command_limits(mut self, timeout: Duration, slots: Arc<Semaphore>) -> Self {
        self.command_timeout = timeout;
        self.command_slots = slots;
        self
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

    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities::v1(true, true, true, true, true, false)
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

    /// `play_url` ne porte que quatre champs ; c'est `play_media` qui tient le
    /// contrat complet, donc c'est ici qu'est la vraie implémentation. La
    /// délégation va dans ce sens-là, et pas l'inverse : #2248, où le riche
    /// `PlayMedia` était réduit à URL/MIME/titre/artiste avant même d'arriver
    /// au constructeur du message `LOAD`.
    async fn play_url(
        &self,
        url: &str,
        mime_type: &str,
        title: Option<&str>,
        artist: Option<&str>,
    ) -> Result<(), String> {
        self.play_media(&super::traits::PlayMedia {
            url,
            mime_type,
            title,
            artist,
            ..Default::default()
        })
        .await
    }

    async fn play_media(&self, media: &super::traits::PlayMedia<'_>) -> Result<(), String> {
        let cast_media = build_cast_media(media);
        let url = media.url.to_string();
        let host = self.host.clone();
        let port = self.port;
        let name = self.name.clone();
        let timeout = self.command_timeout;
        let slots = Arc::clone(&self.command_slots);

        run_cast_command(host, port, timeout, slots, move |device| {
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
                .load(&transport_id, &session_id, &cast_media)
                .map_err(|e| format!("load media: {e}"))?;

            // `session_reused=false` sur une piste qui n'est pas la première
            // d'une écoute désigne le vrai coupable du carillon : la session
            // n'a pas survécu au changement de piste.
            info!(device = %name, url, session_reused, "chromecast_play");
            Ok::<(), String>(())
        })
        .await
    }

    async fn pause(&self) -> Result<(), String> {
        let host = self.host.clone();
        let port = self.port;
        let timeout = self.command_timeout;
        let slots = Arc::clone(&self.command_slots);
        run_cast_command(host, port, timeout, slots, move |device| {
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
    }

    async fn resume(&self) -> Result<(), String> {
        let host = self.host.clone();
        let port = self.port;
        let timeout = self.command_timeout;
        let slots = Arc::clone(&self.command_slots);
        run_cast_command(host, port, timeout, slots, move |device| {
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
    }

    async fn stop(&self) -> Result<(), String> {
        let host = self.host.clone();
        let port = self.port;
        let timeout = self.command_timeout;
        let slots = Arc::clone(&self.command_slots);
        run_cast_command(host, port, timeout, slots, move |device| {
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
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        let host = self.host.clone();
        let port = self.port;
        let position_secs = position_ms as f32 / 1000.0;
        let timeout = self.command_timeout;
        let slots = Arc::clone(&self.command_slots);
        run_cast_command(host, port, timeout, slots, move |device| {
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
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        let host = self.host.clone();
        let port = self.port;
        let level = volume as f32;
        let timeout = self.command_timeout;
        let slots = Arc::clone(&self.command_slots);
        run_cast_command(host, port, timeout, slots, move |device| {
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
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        let host = self.host.clone();
        let port = self.port;
        let timeout = self.command_timeout;
        let slots = Arc::clone(&self.command_slots);
        run_cast_command(host, port, timeout, slots, move |device| {
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
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let host = self.host.clone();
        let port = self.port;
        let timeout = self.command_timeout;
        let slots = Arc::clone(&self.command_slots);
        run_cast_command(host, port, timeout, slots, move |device| {
            device
                .connection
                .connect("receiver-0")
                .map_err(|e| format!("connect receiver: {e}"))?;

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

            device
                .connection
                .connect(&app.transport_id)
                .map_err(|e| format!("connect transport: {e}"))?;

            let media_status = device
                .media
                .get_status(&app.transport_id, None)
                .map_err(|e| format!("media status: {e}"))?;

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
    }

    async fn is_available(&self) -> bool {
        let host = self.host.clone();
        let port = self.port;
        let timeout = self.command_timeout;
        let slots = Arc::clone(&self.command_slots);
        run_cast_command(host, port, timeout, slots, move |device| {
            device
                .connection
                .connect("receiver-0")
                .map_err(|e| format!("connect receiver: {e}"))?;
            device
                .receiver
                .get_status()
                .map_err(|e| format!("status: {e}"))?;
            Ok(())
        })
        .await
        .is_ok()
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncReadExt;

    async fn silent_tcp_peer() -> (
        u16,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let active_for_task = Arc::clone(&active);
        let maximum_for_task = Arc::clone(&maximum);
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let active = Arc::clone(&active_for_task);
                let maximum = Arc::clone(&maximum_for_task);
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(now, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut bytes = [0u8; 1024];
                    while socket.read(&mut bytes).await.unwrap_or(0) != 0 {}
                    active.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        (port, active, maximum, task)
    }

    fn test_output(port: u16, timeout: Duration, slots: Arc<Semaphore>) -> ChromecastOutput {
        ChromecastOutput::new(
            "Cast silencieux".into(),
            "cast-silencieux".into(),
            "127.0.0.1".into(),
            port,
        )
        .with_command_limits(timeout, slots)
    }

    #[test]
    fn la_connexion_essaie_toutes_les_adresses_dans_le_budget() {
        let closed_port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addresses = [
            SocketAddr::from(([127, 0, 0, 1], closed_port)),
            listener.local_addr().unwrap(),
        ];

        let device = rust_cast::CastDevice::connect_without_host_verification_with_deadline(
            "127.0.0.1".into(),
            &addresses,
            Instant::now() + Duration::from_millis(500),
        );
        assert!(device.is_ok(), "la seconde adresse doit etre essayee");
    }

    #[tokio::test]
    async fn toutes_les_commandes_expirent_sur_un_pair_tcp_silencieux() {
        let (port, _active, _maximum, server) = silent_tcp_peer().await;
        let output = test_output(
            port,
            Duration::from_millis(80),
            Arc::new(Semaphore::new(MAX_CAST_COMMAND_WORKERS)),
        );
        let start = Instant::now();

        assert!(
            output
                .play_url(
                    "http://127.0.0.1/audio.flac",
                    "audio/flac",
                    Some("Temoin"),
                    None,
                )
                .await
                .is_err()
        );
        assert!(output.pause().await.is_err());
        assert!(output.resume().await.is_err());
        assert!(output.stop().await.is_err());
        assert!(output.seek(1_000).await.is_err());
        assert!(output.set_volume(0.5).await.is_err());
        assert!(output.set_mute(true).await.is_err());
        assert!(output.get_status().await.is_err());
        assert!(!output.is_available().await);

        assert!(
            start.elapsed() < Duration::from_secs(2),
            "neuf commandes bornees ne doivent jamais immobiliser le serveur"
        );
        server.abort();
    }

    #[tokio::test]
    async fn les_workers_cast_restent_bornes_quand_les_pairs_ne_repondent_pas() {
        let (port, active, maximum, server) = silent_tcp_peer().await;
        let slots = Arc::new(Semaphore::new(2));
        let outputs: Vec<_> = (0..8)
            .map(|_| test_output(port, Duration::from_millis(200), Arc::clone(&slots)))
            .collect();

        let results = futures_util::future::join_all(outputs.iter().map(|o| o.get_status())).await;
        assert!(results.iter().all(Result::is_err));
        tokio::time::timeout(Duration::from_secs(1), async {
            while active.load(Ordering::SeqCst) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("le faux pair doit voir toutes ses sockets se fermer");
        assert!(maximum.load(Ordering::SeqCst) <= 2);

        // `active` compte les sockets vues par le FAUX PAIR : il retombe a zero
        // des que le client ferme. Le permis, lui, appartient a la tache
        // BLOQUANTE (`let _permit`) et n'est rendu qu'a la fin de celle-ci —
        // volontairement, pour qu'un appelant expire ne libere pas de capacite
        // pendant que son worker vit encore. Les deux evenements sont donc
        // distincts, et sous charge le second traine : conclure sur le premier
        // faisait echouer ce test sur une COURSE, jamais sur une fuite (gate du
        // 27/08, 1 rouge sur 2612 en pleine charge, vert isole 4 fois sur 4).
        //
        // On attend donc le permis LUI-MEME, borne. Le test garde toute sa
        // force : une vraie fuite ne rend jamais le permis, l'attente expire,
        // et l'echec revient.
        tokio::time::timeout(Duration::from_secs(5), async {
            while slots.available_permits() != 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("les workers doivent rendre leur permis apres la deadline");
        server.abort();
    }

    #[tokio::test]
    async fn resolution_impossible_ne_retombe_pas_sur_un_connect_non_borne() {
        let output = ChromecastOutput::new(
            "Cast introuvable".into(),
            "cast-introuvable".into(),
            "definitely-not-a-real-host.invalid".into(),
            8009,
        )
        .with_command_limits(Duration::from_millis(200), Arc::new(Semaphore::new(1)));
        let start = Instant::now();
        assert!(output.get_status().await.is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn adresse_non_routable_echoue_dans_le_budget_global() {
        let output = ChromecastOutput::new(
            "Cast blackhole".into(),
            "cast-blackhole".into(),
            "192.0.2.1".into(),
            8009,
        )
        .with_command_limits(Duration::from_millis(150), Arc::new(Semaphore::new(1)));
        let start = Instant::now();
        assert!(output.pause().await.is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
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

#[cfg(test)]
mod load_message_tests {
    use super::*;

    // ---------------------------------------------------------------------
    // #2248 — le message LOAD doit PORTER le contrat PlayMedia.
    //
    // Impossible de brancher un vrai récepteur Cast ici : ce qu'on interroge
    // est le `Media` exact remis à `device.media.load(...)`, c'est-à-dire la
    // charge utile du message LOAD, champ par champ.
    // ---------------------------------------------------------------------

    use crate::outputs::traits::PlayMedia;
    use rust_cast::channels::media::{Image, Metadata, MusicTrackMediaMetadata, StreamType};

    fn music_metadata(
        media: &rust_cast::channels::media::Media,
    ) -> &rust_cast::channels::media::MusicTrackMediaMetadata {
        match media.metadata.as_ref().expect("LOAD sans metadata") {
            Metadata::MusicTrack(m) => m,
            other => panic!("metadata attendue MusicTrack, obtenue {other:?}"),
        }
    }

    /// Une piste de bibliothèque telle que l'orchestrateur la remet aujourd'hui
    /// (`orchestrator.rs`, construction de `PlayMedia`) : tout est renseigné en
    /// amont, rien n'est inventé ici.
    fn piste_complete() -> PlayMedia<'static> {
        PlayMedia {
            url: "http://192.168.1.18:8888/stream/42",
            mime_type: "audio/flac",
            title: Some("Blue in Green"),
            artist: Some("Miles Davis"),
            album: Some("Kind of Blue"),
            cover_url: Some("http://192.168.1.18:8888/api/v1/library/artwork/ab12cd"),
            duration_ms: Some(337_000),
            track_number: Some(3),
            disc_number: Some(1),
            live_stream: false,
            ..Default::default()
        }
    }

    #[test]
    fn un_fichier_est_buffered_et_porte_sa_duree_en_secondes() {
        let media = build_cast_media(&piste_complete());

        assert_eq!(media.content_id, "http://192.168.1.18:8888/stream/42");
        assert_eq!(media.content_type, "audio/flac");
        assert_eq!(media.stream_type, StreamType::Buffered);
        // 337 000 ms = 337 s, et le champ Cast est en SECONDES.
        assert_eq!(media.duration, Some(337.0_f32));
    }

    /// Le message LOAD *exact* : une seule égalité de structure, pour que le
    /// diff d'un échec montre TOUS les champs manquants d'un coup et non le
    /// premier seulement.
    #[test]
    fn un_fichier_porte_son_album_sa_pochette_et_ses_numeros() {
        let media = build_cast_media(&piste_complete());

        assert_eq!(
            music_metadata(&media),
            &MusicTrackMediaMetadata {
                title: Some("Blue in Green".to_string()),
                artist: Some("Miles Davis".to_string()),
                album_name: Some("Kind of Blue".to_string()),
                track_number: Some(3),
                disc_number: Some(1),
                images: vec![Image::new(
                    "http://192.168.1.18:8888/api/v1/library/artwork/ab12cd".to_string()
                )],
                // Tune ne connaît ni l'artiste d'album ni le compositeur ni la
                // date à cette frontière : les inventer serait annoncer une
                // valeur non mesurée.
                album_artist: None,
                composer: None,
                release_date: None,
            }
        );
    }

    #[test]
    fn une_radio_est_annoncee_live_et_sans_duree() {
        let media = build_cast_media(&PlayMedia {
            url: "http://192.168.1.18:8888/stream/radio-7",
            mime_type: "audio/mpeg",
            title: Some("FIP"),
            artist: Some("Radio France"),
            live_stream: true,
            ..Default::default()
        });

        // Buffered sur un flux infini est sémantiquement faux : le récepteur
        // croit tenir un fichier et affiche une barre de progression qui ment.
        assert_eq!(media.stream_type, StreamType::Live);
        assert_eq!(media.duration, None);
    }

    #[test]
    fn une_duree_inconnue_ne_devient_jamais_zero() {
        // Ne jamais annoncer une valeur qu'on n'a pas mesurée : 0.0 s ferait
        // afficher une piste de durée nulle, pire que pas de durée du tout.
        let inconnue = build_cast_media(&PlayMedia {
            url: "http://h/1",
            mime_type: "audio/flac",
            duration_ms: None,
            ..Default::default()
        });
        assert_eq!(inconnue.duration, None);

        // Certaines lignes stockent 0 pour « inconnu » : il ne doit pas
        // franchir la frontière non plus.
        let zero = build_cast_media(&PlayMedia {
            url: "http://h/1",
            mime_type: "audio/flac",
            duration_ms: Some(0),
            ..Default::default()
        });
        assert_eq!(zero.duration, None);
    }

    #[test]
    fn les_numeros_a_zero_valent_inconnu_et_ne_partent_pas() {
        let media = build_cast_media(&PlayMedia {
            url: "http://h/1",
            mime_type: "audio/flac",
            track_number: Some(0),
            disc_number: Some(0),
            ..Default::default()
        });
        let m = music_metadata(&media);
        assert_eq!(m.track_number, None);
        assert_eq!(m.disc_number, None);
    }

    #[test]
    fn sans_pochette_aucune_image_fantome_n_est_envoyee() {
        let media = build_cast_media(&PlayMedia {
            url: "http://h/1",
            mime_type: "audio/flac",
            cover_url: None,
            ..Default::default()
        });
        assert!(music_metadata(&media).images.is_empty());
    }

    #[test]
    fn play_url_garde_exactement_le_contrat_d_avant() {
        // `play_url` ne connaît que quatre champs : le message LOAD qu'il
        // produit doit rester celui d'avant #2248, à l'octet près.
        let media = build_cast_media(&PlayMedia {
            url: "http://h/1",
            mime_type: "audio/flac",
            title: Some("T"),
            artist: Some("A"),
            ..Default::default()
        });
        assert_eq!(media.stream_type, StreamType::Buffered);
        assert_eq!(media.duration, None);
        let m = music_metadata(&media);
        assert_eq!(m.album_name, None);
        assert_eq!(m.track_number, None);
        assert!(m.images.is_empty());
    }
}
