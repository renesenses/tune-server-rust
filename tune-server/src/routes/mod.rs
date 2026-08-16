pub mod active_profile;
pub mod ai;
pub mod airplay_pairing;
pub mod appliance;
pub mod appliance_storage;
pub mod archive;
pub mod bandcamp;
pub mod bridge;
pub mod cd_rip;
pub mod cloud;
pub mod connect;
pub mod converter;
pub mod dac_calibration;
pub mod dashboard;
pub mod declick;
pub mod deezer_proxy_handler;
pub mod developer_api;
pub mod devices;
pub mod digest;
pub mod discogs;
pub mod eq_pro;
pub mod export;
pub mod graphql;
pub mod history;
pub mod home;
pub mod homeassistant;
pub mod hqplayer;
pub mod hue;
pub mod i18n;
pub mod kiosk;
pub mod lastfm_social;
pub mod library;
pub mod listenbrainz;
pub mod listening_stats;
pub mod lyrics;
pub mod marketplace;
pub mod mediasync;
pub mod metadata;
pub mod mqa;
pub mod multi_server;
pub mod network;
pub mod offline;
pub mod onboarding;
pub mod party;
pub mod peers;
pub mod playback;
pub mod playlist_manager;
pub mod playlist_transfer;
pub mod playlists;
pub mod plugins;
pub mod podcasts;
pub mod profiles;
pub mod radios;
pub mod room_calibration;
pub mod room_correction;
pub mod roon_bridge;
pub mod sacd_rip;
pub mod scrobbler;
pub mod search;
pub mod service_tokens;
pub mod setlistfm;
pub mod shazam;
pub mod siri;
pub mod skins;
pub mod smart_ai;
pub mod smart_collections;
pub mod smart_playlists;
pub mod smart_refs;
pub mod snapcast;
pub mod social;
pub mod sonos;
pub mod soundcloud;
pub mod spotify_connect;
pub mod squeezebox;
pub mod stream_handler;
pub mod streaming;
pub mod support;
pub mod system;
pub mod tagger;
pub mod tags;
pub mod upnp;
pub mod upnp_media_renderer;
pub mod upnp_media_server;
pub mod visualizer;
pub mod voice;
pub mod widget;
pub mod ws;
pub mod zone_manager;
pub mod zones;

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

use crate::state::AppState;

async fn auto_dj_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let seed = q
        .get("seed_track")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let count = q
        .get("count")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(20);
    if seed == 0 {
        return (
            StatusCode::BAD_REQUEST,
            serde_json::json!({"error": "seed_track required"}).to_string(),
        )
            .into_response();
    }
    let tracks = tune_core::playback::auto_dj::generate_queue(&state.backend, seed, count);
    axum::Json(serde_json::json!({
        "seed_track": seed,
        "count": tracks.len(),
        "tracks": tracks,
    }))
    .into_response()
}

async fn analytics_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let start = std::time::Instant::now();
    let response = next.run(request).await;
    let latency_ms = start.elapsed().as_millis() as u32;
    state
        .api_analytics
        .record(&path, &method, response.status().as_u16(), latency_ms);
    response
}

async fn demo_library(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let demo_enabled = settings.get("demo_enabled").ok().flatten().as_deref() == Some("true");
    let demo_token = settings
        .get("demo_token")
        .ok()
        .flatten()
        .unwrap_or_default();

    if !demo_enabled {
        return (
            StatusCode::FORBIDDEN,
            serde_json::json!({"error": "demo mode disabled"}).to_string(),
        )
            .into_response();
    }

    if !demo_token.is_empty() {
        let provided = q.get("token").map(|s| s.as_str()).unwrap_or("");
        if provided != demo_token {
            return (
                StatusCode::UNAUTHORIZED,
                serde_json::json!({"error": "invalid demo token"}).to_string(),
            )
                .into_response();
        }
    }

    let albums = tune_core::db::album_repo::AlbumRepo::with_backend(state.backend.clone())
        .list(50, 0)
        .unwrap_or_default();
    let stats = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);

    axum::Json(serde_json::json!({
        "demo": true,
        "read_only": true,
        "stats": { "tracks": stats },
        "albums": albums,
    }))
    .into_response()
}

async fn cache_control_middleware(
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = request.uri().path().to_string();
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    if path.starts_with("/assets/") {
        // Hashed assets (index-Bmb2F8zZ.js) — immutable, cache forever
        headers.insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("public, max-age=31536000, immutable"),
        );
    } else if path == "/" || path.ends_with(".html") || !path.contains('.') {
        // HTML pages and SPA routes — always revalidate
        headers.insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-cache, must-revalidate"),
        );
    }
    response
}

async fn api_fallback(
    axum::extract::OriginalUri(original): axum::extract::OriginalUri,
) -> impl IntoResponse {
    let path = original.path();
    if path.len() > 1 && path.ends_with('/') {
        let trimmed = path.trim_end_matches('/');
        let redirect_to = if let Some(q) = original.query() {
            format!("{trimmed}?{q}")
        } else {
            trimmed.to_string()
        };
        return axum::response::Redirect::permanent(&redirect_to).into_response();
    }
    tracing::warn!(path = %path, "api_not_found");
    (
        StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({"error": "not found", "path": path})),
    )
        .into_response()
}

/// Minimal HTML-entity escaping for untrusted text reflected into a page on the
/// server's own origin. Order matters: `&` first so we don't double-escape.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

pub fn router(state: AppState) -> Router {
    router_with_plugins(state, Vec::new())
}

/// Build the app router, mounting plugin-contributed routes under
/// `/api/v1/ext/{plugin_name}`.
///
/// They are nested *inside* the `/api/v1` tree, before its layers are
/// applied, so a plugin route gets the same auth, analytics and body-limit
/// treatment as a core one. Mounting them at the top level instead would
/// silently make every plugin endpoint unauthenticated.
pub fn router_with_plugins(
    state: AppState,
    plugin_routers: crate::plugins::PluginRouters,
) -> Router {
    let streamer_sessions = state.streamer.sessions_state();

    let web_dir = crate::config::resolve_web_dir()
        .to_string_lossy()
        .into_owned();

    let zones_and_playback = zones::router().merge(playback::router());
    let api = Router::new()
        .route(
            "/playback/shuffle-all",
            axum::routing::post(playback::shuffle_all),
        )
        .nest("/system", system::router())
        .route("/demo/library", get(demo_library))
        .nest("/library", library::router())
        .nest("/library/history", history::router())
        .nest("/history", history::router())
        .route(
            "/zones/",
            get(zones::list_zones_handler).post(zones::create_zone_handler),
        )
        .nest("/zones", zones_and_playback)
        .nest("/playlists", playlists::router())
        .nest("/radios", radios::router())
        .nest("/radio-favorites", radios::radio_favorites_router())
        .route("/radio/auto", get(auto_dj_handler))
        .route("/voice-search", axum::routing::post(voice::voice_search))
        .route(
            "/party/rooms",
            get(party::list_rooms).post(party::create_room),
        )
        .route(
            "/party/rooms/{id}",
            get(party::room_info).delete(party::delete_room),
        )
        .nest("/alarms", radios::alarms_router())
        .nest("/search", search::router())
        .nest("/devices", devices::router())
        .nest("/streaming", streaming::router())
        .nest("/profiles", profiles::router())
        .nest("/tags", tags::router())
        .nest("/metadata", metadata::router())
        .nest("/library/smart-playlists", smart_playlists::router())
        .nest("/library/smart-collections", smart_collections::router())
        .nest("/export", export::router())
        .nest("/network", network::router())
        .nest(
            "/appliance",
            appliance::router().merge(appliance_storage::router()),
        )
        .nest("/dashboard", dashboard::router())
        .nest("/digest", digest::router())
        .nest("/peers", peers::router())
        .nest("/podcasts", podcasts::router())
        .nest("/plugins", plugins::router())
        .nest("/marketplace", marketplace::router())
        // DJ mode moved to the `dj` native plugin (P5, #917); with that feature
        // it mounts at /api/v1/ext/dj. The stock server no longer serves /dj.
        .nest("/party", party::router())
        .nest("/playlist-manager", playlist_manager::router())
        .nest("/playlist-transfer", playlist_transfer::router())
        .nest("/zone-manager", zone_manager::router())
        .nest("/snapcast", snapcast::router())
        .nest("/sonos", sonos::router())
        .nest("/squeezebox", squeezebox::router())
        .nest("/spotify-connect", spotify_connect::router())
        .nest("/listenbrainz", listenbrainz::router())
        .nest("/scrobbler", scrobbler::router())
        .nest("/soundcloud", soundcloud::router())
        .nest("/bandcamp", bandcamp::router())
        .nest("/archive", archive::router())
        .nest("/discogs", discogs::router())
        .nest("/setlistfm", setlistfm::router())
        .nest("/homeassistant", homeassistant::router())
        .nest("/hue", hue::router())
        .nest("/tagger", tagger::router())
        .nest("/kiosk", kiosk::router())
        .nest("/widget", widget::router())
        .nest("/mediasync", mediasync::router())
        .nest("/cd-rip", cd_rip::router())
        .nest("/sacd-rip", sacd_rip::router())
        .nest("/hqplayer", hqplayer::router())
        .nest("/dac-calibration", dac_calibration::router())
        .nest("/room-calibration", room_calibration::router())
        .nest("/room-correction", room_correction::router())
        .nest("/outputs", airplay_pairing::router())
        .nest("/visualizer", visualizer::router())
        .nest("/graphql", graphql::router())
        .nest("/eq", eq_pro::router())
        .nest("/siri", siri::router())
        .nest("/lastfm-social", lastfm_social::router())
        .nest("/stats/listening", listening_stats::router())
        .nest("/lyrics", lyrics::router())
        .nest("/mqa", mqa::router())
        .nest("/roon-bridge", roon_bridge::router())
        .nest("/connect", connect::router())
        .nest("/converter", converter::router())
        .nest("/declick", declick::router())
        .nest("/shazam", shazam::router())
        .nest("/social", social::router())
        .nest("/home", home::router())
        .nest("/onboarding", onboarding::router())
        .nest("/i18n", i18n::router())
        .nest("/upnp", upnp::router())
        .nest("/auth", crate::auth::router())
        .nest("/cloud", cloud::router())
        .nest("/support", support::router())
        .nest("/multi-server", multi_server::router())
        .nest("/offline", offline::router())
        .nest("/smart-ai", smart_ai::router())
        .nest("/ai", ai::router())
        .nest("/developer", developer_api::router())
        .nest("/skins", skins::router())
        .route(
            "/services/tokens",
            get(service_tokens::list).post(service_tokens::list),
        )
        .route(
            "/services/tokens/{id}",
            axum::routing::post(service_tokens::save).delete(service_tokens::delete),
        )
        .route(
            "/services/tokens/{id}/test",
            axum::routing::post(service_tokens::test),
        )
        .route(
            "/services/lastfm/auth",
            axum::routing::post(service_tokens::lastfm_auth),
        )
        .route(
            "/services/lastfm/auth/token",
            axum::routing::post(service_tokens::lastfm_auth_token),
        )
        .route(
            "/services/lastfm/auth/session",
            axum::routing::post(service_tokens::lastfm_auth_session),
        )
        .route(
            "/services/lastfm/scrobble/toggle",
            axum::routing::post(service_tokens::lastfm_scrobble_toggle),
        )
        .route(
            "/services/lastfm/disconnect",
            axum::routing::post(service_tokens::lastfm_disconnect),
        )
        .fallback(api_fallback);

    // Plugin routes. `nest_service` because a plugin router is `Router<()>`
    // (already stated) while `api` is still `Router<AppState>` — nesting it
    // as a service is the only way to combine the two, and it keeps the
    // plugin's internal state entirely its own.
    //
    // One consequence: the nested service owns everything under its prefix, so
    // an unknown sub-path answers with axum's bare 404 rather than
    // `api_fallback`'s JSON body. Left deliberately — forcing our fallback on
    // the plugin router would take away its ability to answer its own 404s,
    // which matters more than a uniform error shape under /ext.
    let api = plugin_routers
        .into_iter()
        .fold(api, |api, (plugin_name, plugin_router)| {
            let mount = format!("/ext/{plugin_name}");
            tracing::info!(plugin = %plugin_name, mount = %format!("/api/v1{mount}"), "plugin_routes_mounted");
            api.nest_service(&mount, plugin_router)
        });

    let api = api
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::auth_middleware,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            analytics_middleware,
        ))
        .layer(axum::extract::DefaultBodyLimit::max(50 * 1024 * 1024));

    // UPnP MediaServer routes (ContentDirectory / ConnectionManager)
    let upnp_routes = state
        .upnp
        .as_ref()
        .map(|upnp_state| upnp_media_server::standalone_router(upnp_state.clone()));

    let deezer_proxy = axum::Router::new()
        .route(
            "/deezer-proxy/{filename}",
            get(deezer_proxy_handler::handle_deezer_proxy),
        )
        .with_state(state.services.clone());

    // Collect mountable skins before state is moved
    let mountable_skins = state.skin_manager.mountable_skins();

    let mut app = Router::new()
        .nest("/api/v1", api)
        .nest("/ws", ws::router())
        .nest("/api/v1/ws", ws::router())
        .nest("/ws/bridge", bridge::router())
        .with_state(state.clone())
        .route("/add-station", get(|axum::extract::Query(q): axum::extract::Query<radios::AddFromWebQuery>| async move {
            // q.name is attacker-controlled and reflected into HTML on the
            // server's own origin — escape it or it is a reflected XSS.
            axum::response::Html(format!(
                r#"<!DOCTYPE html><html><body style="font-family:system-ui;background:#1a1a2e;color:#eee;display:flex;justify-content:center;align-items:center;height:100vh;margin:0"><div style="text-align:center"><h1 style="color:#4ade80">✓ Radio ajoutée</h1><p><strong>{}</strong></p><p style="color:#888">Vous pouvez fermer cet onglet.</p></div></body></html>"#,
                html_escape(&q.name)
            ))
        }))
        .merge(stream_handler::router(streamer_sessions))
        .merge(deezer_proxy);

    if let Some(upnp) = upnp_routes {
        app = app.nest("/upnp", upnp);
    }

    // MediaRenderer:1 par zone (#1750) — routes toujours montées, mais
    // chaque handler vérifie l'opt-in `zone_{id}_upnp_renderer` (404 sinon)
    // et seules les zones opt-in sont annoncées en SSDP.
    app = app.nest(
        "/upnp/renderer",
        upnp_media_renderer::router().with_state(state.clone()),
    );

    // Mount all installed skins on /{skin_id}
    for (skin_id, skin_path) in mountable_skins {
        let index = format!("{}/index.html", skin_path.display());
        if std::path::Path::new(&index).exists() {
            tracing::info!(skin_id = %skin_id, path = %skin_path.display(), "skin_mounted");
            app = app.nest_service(
                &format!("/{skin_id}"),
                ServeDir::new(&skin_path).fallback(ServeFile::new(&index)),
            );
        }
    }

    let index_path = format!("{web_dir}/index.html");

    app.route(
        "/",
        get(move || async move {
            match tokio::fs::read(&index_path).await {
                Ok(html) => {
                    let mut headers = axum::http::HeaderMap::new();
                    headers.insert(
                        axum::http::header::CONTENT_TYPE,
                        axum::http::HeaderValue::from_static("text/html; charset=utf-8"),
                    );
                    headers.insert(
                        axum::http::header::CACHE_CONTROL,
                        axum::http::HeaderValue::from_static("no-cache, must-revalidate"),
                    );
                    (headers, html).into_response()
                }
                Err(_) => StatusCode::NOT_FOUND.into_response(),
            }
        }),
    )
    .fallback_service(
        ServeDir::new(&web_dir).fallback(ServeFile::new(format!("{web_dir}/index.html"))),
    )
    .layer(axum::middleware::from_fn(cache_control_middleware))
    .layer(CompressionLayer::new())
    .layer(CorsLayer::permissive())
}

#[cfg(test)]
mod escape_tests {
    use super::html_escape;

    #[test]
    fn escapes_html_metacharacters() {
        assert_eq!(
            html_escape(r#"<script>alert("x&y")</script>"#),
            "&lt;script&gt;alert(&quot;x&amp;y&quot;)&lt;/script&gt;"
        );
        assert_eq!(html_escape("O'Brien"), "O&#x27;Brien");
        assert_eq!(html_escape("plain radio"), "plain radio");
    }
}

/// Garde-fou : écrire `zone_{id}_eq_profile` sans rafraîchir la sortie qui joue.
///
/// L'égaliseur n'atteint le son d'une zone locale que si quelqu'un rebâtit
/// l'`EqProcessor` et l'installe — `Orchestrator::refresh_zone_eq`. Persister
/// la clé ne suffit pas : le réglage n'agit alors qu'à la piste SUIVANTE, et la
/// route répond quand même 200 (ou `applied: true`). C'est ce silence qui a
/// produit #1372, #1555 et #1688, et le correctif de #1725 n'avait branché
/// qu'un des quatre points d'écriture.
///
/// Ce test relit les sources plutôt que d'exercer les routes : la propriété à
/// garder est structurelle — « tout fichier qui écrit cette clé rafraîchit » —
/// et aucun harnais HTTP ne la vérifierait aussi simplement.
///
/// Granularité volontairement au FICHIER, pas à la ligne : plus grossier, mais
/// robuste aux déplacements de code. Un fichier qui ne ferait que LIRE la clé
/// déclencherait un faux positif ; il irait dans `LECTURE_SEULE` avec sa raison.
#[cfg(test)]
mod eq_refresh_guard {
    use std::fs;
    use std::path::Path;

    /// Fichiers qui mentionnent une clé sans jamais l'écrire. Vide aujourd'hui.
    const LECTURE_SEULE: &[&str] = &[];

    /// Les réglages DSP par zone qui n'atteignent le son que si quelqu'un
    /// rafraîchit la sortie vivante, et la méthode qui le fait.
    ///
    /// `zone_*_eq_profile` a coûté quatre omissions (#1725), `zone_*_crossfeed`
    /// une de plus (#1786). Toute nouvelle clé de ce type doit rejoindre cette
    /// table AVANT sa première route d'écriture, pas après le signalement.
    const REGLAGES_A_RAFRAICHIR: &[(&str, &str)] = &[
        ("_eq_profile", "refresh_zone_eq"),
        ("_crossfeed", "refresh_zone_crossfeed"),
    ];

    #[test]
    fn every_route_writing_a_dsp_setting_refreshes_the_live_output() {
        let racine = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/routes");
        let mut sources: Vec<(String, String)> = Vec::new();

        let mut piles = vec![racine.clone()];
        while let Some(dir) = piles.pop() {
            for entree in fs::read_dir(&dir).expect("lecture de src/routes") {
                let chemin = entree.expect("entrée de répertoire").path();
                if chemin.is_dir() {
                    piles.push(chemin);
                    continue;
                }
                if chemin.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let nom = chemin
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string();
                sources.push((
                    nom,
                    fs::read_to_string(&chemin).expect("lecture du fichier"),
                ));
            }
        }

        for (cle, rafraichisseur) in REGLAGES_A_RAFRAICHIR {
            let concernes: Vec<&(String, String)> =
                sources.iter().filter(|(_, s)| s.contains(cle)).collect();
            assert!(
                !concernes.is_empty(),
                "le garde-fou ne trouve aucun fichier touchant `zone_*{cle}` — \
                 la clé a probablement été renommée, et ce test ne garde plus rien"
            );
            let fautifs: Vec<&str> = concernes
                .iter()
                .filter(|(nom, _)| !LECTURE_SEULE.contains(&nom.as_str()))
                .filter(|(_, s)| !s.contains(rafraichisseur))
                .map(|(nom, _)| nom.as_str())
                .collect();
            assert!(
                fautifs.is_empty(),
                "ces routes écrivent `zone_*{cle}` sans appeler \
                 `Orchestrator::{rafraichisseur}` : {fautifs:?}\n\
                 Sans lui, le réglage n'atteint le son qu'à la piste SUIVANTE sur une \
                 zone locale, alors que la réponse annonce un succès (#1725, #1786).\n\
                 Ajouter, après le `settings.set(...)` :\n    \
                 let applique = state.orchestrator.{rafraichisseur}(zone_id).await;\n\
                 puis exposer `applied_live` dans la réponse. Si le fichier ne fait que \
                 LIRE la clé, l'inscrire dans `LECTURE_SEULE` avec sa raison."
            );
        }
    }
}
