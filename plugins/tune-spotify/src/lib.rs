//! Spotify source, authentication and isolated audio workers as a native plugin.
mod connect_routes;
pub mod spotify;
pub mod spotify_connect;
#[cfg(feature = "native")]
pub mod spotify_native;
mod spotify_pairing;

use std::sync::Arc;
use tokio::sync::Mutex;
use tune_core::db::backend::DbBackend;
use tune_core::plugin_sdk::{PluginContext, TunePlugin};
use tune_core::streaming::registry::ServiceHandle;
use tune_core::streaming::{ServiceRegistry, StreamingService};

pub struct HostServices {
    pub backend: Arc<dyn DbBackend>,
    pub services: Arc<Mutex<ServiceRegistry>>,
    pub http_client: reqwest::Client,
    pub port: u16,
    pub client_id: Option<String>,
    pub redirect_uri: Option<String>,
    /// Invalidate the host's generic authenticated catalogue cache.
    pub invalidate_content: fn(&str),
}

pub struct SpotifyPlugin {
    host: HostServices,
    service: Option<ServiceHandle>,
    connect: Arc<spotify_connect::SpotifyConnectManager>,
}

impl SpotifyPlugin {
    pub fn new(host: HostServices) -> Self {
        let connect = Arc::new(spotify_connect::SpotifyConnectManager::new(
            "Tune".into(),
            host.port,
        ));
        Self {
            host,
            service: None,
            connect,
        }
    }

    fn route_state(&self) -> connect_routes::SpotifyHttpState {
        connect_routes::SpotifyHttpState {
            backend: self.host.backend.clone(),
            services: self.host.services.clone(),
            http_client: self.host.http_client.clone(),
            spotify_connect: self.connect.clone(),
            invalidate_content: self.host.invalidate_content,
        }
    }
}

/// Preserve both the old OAuth default and the explicitly opted-in native mode.
pub fn configured_service(
    client_id: Option<&str>,
    redirect_uri: Option<&str>,
    port: u16,
) -> Box<dyn StreamingService> {
    #[cfg(feature = "native")]
    if std::env::var("TUNE_SPOTIFY_NATIVE").as_deref() == Ok("1") {
        return Box::new(spotify_native::SpotifyNativeService::new());
    }
    Box::new(spotify::SpotifyService::with_config(
        client_id,
        redirect_uri,
        port,
    ))
}

#[async_trait::async_trait]
impl TunePlugin for SpotifyPlugin {
    fn name(&self) -> &str {
        "spotify"
    }
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }
    fn description(&self) -> &str {
        "Spotify catalogue, OAuth/Connect and optional native playback"
    }

    // Historically available in every server; preserve that default. The
    // plugin_enabled switch now removes the source and all its routes together.
    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        self.service = Some(ctx.register_streaming_service(configured_service(
            self.host.client_id.as_deref(),
            self.host.redirect_uri.as_deref(),
            self.host.port,
        ))?);
        ctx.register_router(connect_routes::router().with_state(self.route_state()));
        Ok(())
    }

    async fn on_event(&mut self, event: &tune_core::event_bus::TuneEvent) {
        if event.event_type == "system.started" {
            connect_routes::auto_start(&self.route_state()).await;
        }
    }

    async fn teardown(&mut self) -> Result<(), String> {
        self.connect.disable().await;
        if let Some(service) = self.service.take() {
            service.write().await.shutdown().await;
        }
        Ok(())
    }
}

#[cfg(feature = "native")]
pub fn worker_entry() -> tune_core::plugin_worker::PluginWorker {
    tune_core::plugin_worker::PluginWorker {
        flag: "--spotify-native-worker",
        run: |args| Box::pin(spotify_native::worker_main(args)),
    }
}
