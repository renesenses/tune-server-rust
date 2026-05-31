use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

pub(super) async fn update_check() -> Json<Value> {
    Json(json!({
        "current_version": tune_core::version(),
        "latest_version": null,
        "update_available": false,
        "engine": "rust",
        "message": "auto-update not yet implemented",
    }))
}

pub(super) async fn update_install(State(state): State<AppState>) -> impl IntoResponse {
    let settings = SettingsRepo::new(state.db);
    let task_id = uuid::Uuid::new_v4().to_string();
    settings.set("update_task_id", &task_id).ok();
    settings.set("update_status", "downloading").ok();

    let http_client = state.http_client.clone();
    let tid = task_id.clone();
    tokio::spawn(async move {
        let client = http_client;
        let arch = std::env::consts::ARCH;
        let os = std::env::consts::OS;
        let url = format!(
            "https://github.com/renesenses/tune-server-rust/releases/latest/download/tune-server-{os}-{arch}"
        );
        match client
            .get(&url)
            .timeout(std::time::Duration::from_secs(300))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(bytes) = resp.bytes().await {
                    let update_path = "/tmp/tune-server-update";
                    if tokio::fs::write(update_path, &bytes).await.is_ok() {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            std::fs::set_permissions(
                                update_path,
                                std::fs::Permissions::from_mode(0o755),
                            )
                            .ok();
                        }
                        tracing::info!(task_id = %tid, size = bytes.len(), "update_downloaded");
                    }
                }
            }
            _ => {
                tracing::warn!(task_id = %tid, "update_download_failed");
            }
        }
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({"task_id": task_id, "status": "downloading"})),
    )
}

pub(super) async fn update_apply() -> impl IntoResponse {
    let update_path = "/tmp/tune-server-update";
    if !std::path::Path::new(update_path).exists() {
        return Json(json!({"error": "no update downloaded"})).into_response();
    }
    let current_exe = std::env::current_exe().unwrap_or_default();
    let backup = format!("{}.old", current_exe.display());
    std::fs::rename(&current_exe, &backup).ok();
    if std::fs::rename(update_path, &current_exe).is_ok() {
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            std::process::exit(0);
        });
        Json(json!({"status": "applied", "message": "restarting with new binary"})).into_response()
    } else {
        std::fs::rename(&backup, &current_exe).ok();
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to replace binary"})),
        )
            .into_response()
    }
}

pub(super) async fn update_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::new(state.db);
    let status = settings
        .get("update_status")
        .ok()
        .flatten()
        .unwrap_or_else(|| "idle".into());
    let task_id = settings.get("update_task_id").ok().flatten();
    let update_exists = std::path::Path::new("/tmp/tune-server-update").exists();
    Json(json!({
        "status": status,
        "task_id": task_id,
        "update_ready": update_exists,
        "current_version": tune_core::version(),
    }))
}

pub(super) async fn changelog() -> Json<Value> {
    Json(json!({
        "version": tune_core::version(),
        "entries": [
            {
                "version": "0.8.6",
                "date": "2026-05-29",
                "sections": [
                    { "title": "Corrections", "items": [
                        "Play queue — file de lecture correcte en lançant album ou playlist",
                        "DLNA gapless — toggle par zone + URLs pochettes dans DIDL",
                        "Qobuz genres — format API réel pris en charge",
                        "Onboarding — onboarding_completed + genres JSON array",
                    ]},
                    { "title": "Nouveautés", "items": [
                        "OAAT — feature flag activé pour streaming bit-perfect",
                        "Library clear — endpoint POST /system/library/clear",
                    ]},
                ]
            },
            {
                "version": "0.8.5",
                "date": "2026-05-29",
                "sections": [
                    { "title": "Corrections", "items": [
                        "Windows — scan bibliothèque retournait 0 résultats (chemins)",
                        "Spotify — lecture TUNE_SPOTIFY_CLIENT_ID pour OAuth",
                        "Squeezebox/LMS — erreur JSON-parse sur réponse vide",
                        "SSDP — énumération des vraies interfaces réseau",
                        "MP4/AAC — normalisation du format",
                    ]},
                    { "title": "Nouveautés", "items": [
                        "Audio USB local — sortie audio via cpal",
                        "Changelog intégré dans l'interface web",
                    ]},
                ]
            },
            {
                "version": "0.8.4",
                "date": "2026-05-29",
                "sections": [
                    { "title": "Nouveaux protocoles", "items": [
                        "AirPlay (RAOP) — lecture native sans dépendance externe",
                        "BluOS — support Bluesound (Pulse, Node, Powernode)",
                        "OpenHome — Linn et compatibles avec UPnP eventing",
                        "OAAT — découverte mDNS, transport FLAC natif, bit-perfect",
                    ]},
                    { "title": "DLNA amélioré", "items": [
                        "Retry automatique sur erreur SOAP",
                        "Détection du mute",
                        "Pochette d'album dans les métadonnées DIDL",
                        "Meilleur support DSD (format DSF explicite pour FFmpeg)",
                    ]},
                    { "title": "Nouvelles fonctionnalités", "items": [
                        "Deezer — proxy de déchiffrement intégré",
                        "DJ Player — mode DJ avec crossfade",
                        "Profils utilisateurs multi-profils",
                        "Playlist transfer entre services de streaming",
                        "Recherche full-text corrigée (FTS5)",
                        "Alarmes — scheduler avec réveil programmé",
                        "ICY metadata — titre/artiste des webradios",
                        "Enrichissement crédits MusicBrainz automatique",
                    ]},
                    { "title": "Performances et stabilité", "items": [
                        "SQLite optimisé — requêtes accélérées",
                        "Prévention des fuites mémoire (session GC, cache eviction)",
                        "SSDP optimisé (scan unique, fréquence réduite)",
                    ]},
                ]
            },
            {
                "version": "0.8.3",
                "date": "2026-05-29",
                "sections": [
                    { "title": "Corrections", "items": [
                        "Docker fix critique — binaire vide corrigé",
                        "FTS5 recherche full-text fonctionnelle",
                        "SSDP optimisé — scan unique ssdp:all",
                        "MP3 parsing relaxé",
                    ]},
                    { "title": "Nouveautés", "items": [
                        "DMG macOS signé et notarisé (ARM + Intel)",
                        "Installer Windows setup.exe (NSIS)",
                        "Noms d'assets versionnés",
                    ]},
                ]
            },
        ]
    }))
}
