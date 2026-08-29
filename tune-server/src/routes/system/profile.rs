//! GET /system/profile — fiche système consolidée pour l'écran Support.
//!
//! Compose des informations déjà exposées ailleurs (/system/health,
//! /system/stats, /system/diagnostics) en un seul JSON compact, destiné à
//! l'onglet « Mon système » et à la pièce jointe automatique des tickets.
//! Aucune nouvelle sonde : uniquement des lectures AppState/SettingsRepo.
//!
//! Auth : token requis quand l'auth est activée (la fiche liste music_dirs
//! et l'IP LAN — pas d'accès anonyme, décision audit sécurité), mais aucun
//! rôle admin exigé : l'écran Support doit rester lisible par tout
//! utilisateur authentifié. Tout ce qui sort d'ici doit rester NON
//! sensible : les réglages passent par une liste d'inclusion stricte
//! (`SUPPORT_SETTING_KEYS`) — jamais le dump brut des settings, qui contient
//! clés API, tokens et mots de passe.

use axum::Json;
use axum::extract::State;
use serde_json::{Map, Value, json};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;

use crate::state::AppState;

/// Réglages « pertinents support », avec leur valeur par défaut quand ils ne
/// sont pas encore persistés (mêmes défauts que /system/config). Liste
/// d'inclusion STRICTE : ajouter une clé ici = l'exposer à tout utilisateur
/// authentifié (pas seulement admin).
/// Interdit : toute clé contenant un secret (api_key, jwt_secret,
/// license_key, discogs_token, auth_tokens_*, mots de passe…).
const SUPPORT_SETTING_KEYS: &[(&str, fn() -> Value)] = &[
    ("community_sync_enabled", || json!(false)),
    // Consentement de contribution (bios + images d'artistes). Non sensible,
    // et utile en support : « est-ce que cette instance envoie quelque chose ? »
    (tune_core::cloud::consent::CONTRIBUTION_SETTING_KEY, || {
        json!(tune_core::cloud::consent::CONTRIBUTION_DEFAULT)
    }),
    ("enrich_on_scan", || json!(true)),
    ("scan_import_playlists", || json!(true)),
    ("resample_policy", || json!("none")),
    ("prefetch_mode", || json!("30s")),
    ("dsd_lpcm_stream", || json!(false)),
    ("auth_enabled", || json!(false)),
];

/// Projette les settings bruts sur l'allowlist support. Les valeurs stockées
/// en texte ("true", "1.5", "none") sont re-typées quand c'est du JSON valide,
/// sinon renvoyées telles quelles en chaîne.
fn support_settings(get: impl Fn(&str) -> Option<String>) -> Map<String, Value> {
    let mut out = Map::new();
    for (key, default) in SUPPORT_SETTING_KEYS {
        let value = match get(key) {
            Some(raw) => serde_json::from_str::<Value>(&raw).unwrap_or(Value::String(raw)),
            None => default(),
        };
        out.insert((*key).to_string(), value);
    }
    out
}

pub(super) async fn system_profile(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());

    // --- server -----------------------------------------------------------
    let audio_backend = {
        #[cfg(feature = "local-audio")]
        {
            tune_core::outputs::local::active_backend_name(&state.display_audio_backend())
        }
        #[cfg(not(feature = "local-audio"))]
        {
            let _ = &state.config.local_audio_backend;
            "none"
        }
    };
    let server = json!({
        "version": tune_core::version(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        // #2117 : l'ancrage absolu voyage avec le compteur relatif, sinon la
        // charge agrégée redonne à lire la valeur ambiguë que l'agrégation
        // était censée éviter.
        "uptime_seconds": state.started_at.elapsed().as_secs(),
        "process_started_at": state.process_started_at_rfc3339(),
        "database_engine": state.backend.engine().as_str(),
        "audio_backend": audio_backend,
    });

    // --- library ----------------------------------------------------------
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let albums = AlbumRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let artists = ArtistRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let music_dirs = super::get_music_dirs_list(&state.backend);
    let library = json!({
        "tracks": tracks,
        "albums": albums,
        "artists": artists,
        "music_dirs": music_dirs,
    });

    // --- zones (compact : pas de stream_urls ni d'état de lecture) --------
    // Inclut l'appareil affecté (override utilisateur > détection UPnP) pour la
    // fiche Support : brand/model = choix utilisateur au catalogue, à défaut ce
    // que la découverte a lu dans la description du périphérique. Sans ce repli
    // la fiche affichait « — » même pour les renderers correctement détectés,
    // alors que /zones, lui, expose bien la détection.
    // Réutilise le `settings` déjà construit en tête de handler.
    let devices = state.scanner.devices().await;
    let zones: Vec<Value> = ZoneRepo::with_backend(state.backend.clone())
        .list()
        .unwrap_or_default()
        .iter()
        .map(|z| {
            let zid = z.id.unwrap_or(0);
            let detected = z
                .output_device_id
                .as_deref()
                .and_then(|did| devices.iter().find(|d| d.id == did));
            let brand = settings
                .get(&format!("zone_{zid}_brand"))
                .ok()
                .flatten()
                .filter(|s| !s.is_empty())
                .or_else(|| detected.and_then(|d| d.manufacturer.clone()));
            let model = settings
                .get(&format!("zone_{zid}_model"))
                .ok()
                .flatten()
                .filter(|s| !s.is_empty())
                .or_else(|| detected.and_then(|d| d.model.clone()));
            json!({
                "name": z.name,
                "output_type": z.output_type,
                "online": z.online,
                "brand": brand,
                "model": model,
            })
        })
        .collect();

    // --- license (tier uniquement, jamais la clé) -------------------------
    let tier = state.license.license_state().await.tier;

    // --- network ----------------------------------------------------------
    let advertise_ip = std::env::var("TUNE_ADVERTISE_IP")
        .ok()
        .filter(|ip| !ip.is_empty())
        .or_else(|| tune_core::discovery::ssdp::get_local_ip().map(|ip| ip.to_string()));
    let network = json!({
        "advertise_ip": advertise_ip,
        "port": state.port,
    });

    Json(json!({
        "server": server,
        "library": library,
        "zones": zones,
        "license": { "tier": tier },
        "network": network,
        "settings": support_settings(|k| settings.get(k).ok().flatten()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// L'allowlist ne doit jamais contenir de clé « secret-shaped » : la
    /// fiche est visible par tout utilisateur authentifié, pas seulement
    /// l'admin, et est jointe aux tickets support.
    #[test]
    fn allowlist_contains_no_secret_keys() {
        const FORBIDDEN: &[&str] = &["key", "token", "secret", "password", "credential"];
        for (key, _) in SUPPORT_SETTING_KEYS {
            for frag in FORBIDDEN {
                assert!(
                    !key.contains(frag),
                    "setting {key:?} ressemble à un secret et ne doit pas être exposé"
                );
            }
        }
    }

    /// Seules les clés de l'allowlist sortent ; un store contenant des secrets
    /// n'en laisse fuiter aucun, et les valeurs texte sont re-typées.
    #[test]
    fn support_settings_filters_and_retypes() {
        let store = |k: &str| -> Option<String> {
            match k {
                "community_sync_enabled" => Some("true".into()),
                "resample_policy" => Some("auto".into()),
                // Secrets présents dans les settings réels — jamais demandés.
                "license_key" | "discogs_token" | "jwt_secret" | "api_key" => {
                    panic!("clé sensible {k:?} lue par la fiche système")
                }
                _ => None,
            }
        };
        let out = support_settings(store);
        assert_eq!(out.len(), SUPPORT_SETTING_KEYS.len());
        assert_eq!(out["community_sync_enabled"], json!(true));
        assert_eq!(out["resample_policy"], json!("auto"));
        // Défauts appliqués pour les clés absentes du store.
        assert_eq!(out["enrich_on_scan"], json!(true));
        assert_eq!(out["prefetch_mode"], json!("30s"));
        assert!(!out.contains_key("license_key"));
    }
}
