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
///
/// `pub(super)` depuis #2856 : le RAPPORT DE BOGUE ne portait aucune section
/// de réglages, et c'est cette même liste qu'il doit rendre. Deux listes
/// auraient divergé, et la seconde n'aurait pas hérité de la garde de
/// `est_secret` posée ci-dessous.
pub(super) const SUPPORT_SETTING_KEYS: &[(&str, fn() -> Value)] = &[
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
    ("dsp_progressif_reseau", || json!(false)),
    ("auth_enabled", || json!(false)),
    // #3577 — le panneau Paroles s'ouvre vide, et ce booléen dit LEQUEL des
    // deux verrous s'est refermé.
    //
    // `routes/library/tracks.rs` n'interroge LRCLIB que si cette clé vaut la
    // chaîne `"true"` ; sinon il rend `404 {"error":"no_lyrics"}` — le MÊME
    // 404 que pour un titre réellement sans paroles. Absente de la fiche, sa
    // valeur ne pouvait donc plus se déduire de rien : le `diagnostic.md` du
    // ticket support 93 (Belkadi Yacine, 49 618 fichiers, 0.9.140) ne la
    // portait pas, et l'issue a dû clore sur « valeur NON ÉTABLIE ».
    //
    // Le client web la lit déjà dans `GET /system/config` pour nommer le
    // réglage dans l'état vide (tune-web-client#775) ; la fiche support doit
    // pouvoir en dire autant, sans quoi le premier niveau de support ne peut
    // pas trancher entre « rien à trouver » et « recherche en ligne éteinte ».
    //
    // Non sensible : un booléen de consentement, comme les huit au-dessus.
    ("lyrics_lrclib_enabled", || json!(false)),
];

/// Projette les settings bruts sur l'allowlist support. Les valeurs stockées
/// en texte ("true", "1.5", "none") sont re-typées quand c'est du JSON valide,
/// sinon renvoyées telles quelles en chaîne.
pub(super) fn support_settings(get: impl Fn(&str) -> Option<String>) -> Map<String, Value> {
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

/// Le nom du backend audio réellement ACTIF.
pub(super) fn backend_audio_actif(state: &AppState) -> &'static str {
    #[cfg(feature = "local-audio")]
    {
        tune_core::outputs::local::active_backend_name(&state.display_audio_backend())
    }
    #[cfg(not(feature = "local-audio"))]
    {
        let _ = &state.config.local_audio_backend;
        "none"
    }
}

/// Le MOTEUR AUDIO tel qu'un ticket doit pouvoir le lire (#2856).
///
/// Trois faits, jamais un seul : ce qui a été DEMANDÉ, ce qui TOURNE, et ce
/// que le mode exclusif vaut réellement — avec la raison quand les deux
/// diffèrent. C'est exactement l'écart que #3192 a mesuré chez jfpaquet :
/// sous ASIO, décocher « mode exclusif » reste sans effet (un pilote ASIO
/// ouvert en partagé n'existe pas), le son de toutes les autres applications
/// disparaît, et rien ne le dit. Un ticket qui ne porte que le backend actif
/// oblige à réécrire au testeur pour apprendre ces trois valeurs.
///
/// Aucun secret ici : trois booléens, deux noms de backend et une phrase
/// figée du binaire.
pub(super) fn moteur_audio(state: &AppState) -> Value {
    let exclusif = state.exclusive_mode_status();
    json!({
        "backend_requested": state.effective_audio_backend(),
        "backend_active": backend_audio_actif(state),
        "exclusive_mode": {
            "requested": exclusif.requested,
            "effective": exclusif.effective,
            "forced": exclusif.forced,
            "detail": exclusif.detail,
        },
    })
}

pub(super) async fn system_profile(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());

    // --- server -----------------------------------------------------------
    let audio_backend = backend_audio_actif(&state);
    // #2856 : la fiche ne portait que le backend ACTIF. Elle porte désormais
    // aussi le DEMANDÉ et l'état du mode exclusif, les deux faits qu'il
    // fallait redemander au testeur à chaque ticket audio.
    let audio = moteur_audio(&state);
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
        "audio": audio,
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
    // #2718 et tickets support 61, 87, 97, 98 — « plus de serveurs
    // multimedia ». La fiche decrivait les zones jusqu'a la marque et au
    // modele du DAC, et ne disait RIEN des serveurs multimedia : quatre
    // signalements sur trois semaines ont ete instruits sans jamais pouvoir
    // dire combien Tune en voyait. #2718 s'est refermee « mecanisme non
    // etabli » alors que la reponse tenait dans un compteur absent.
    //
    // Les champs sont EXACTEMENT ceux que `GET /network/media-servers` sert
    // deja a tout utilisateur authentifie : la fiche n'expose rien de neuf.
    let serveurs_multimedia: Vec<Value> = {
        let registre = state.media_servers.lock().await;
        let mut v: Vec<Value> = registre
            .values()
            .map(|ms| {
                json!({
                    "name": ms.name,
                    "host": ms.host,
                    "port": ms.port,
                    "reachable": ms.is_reachable(),
                    "last_seen_secs": ms.age().as_secs(),
                })
            })
            .collect();
        // Ordre stable : un `HashMap` rendrait la fiche differente a chaque
        // ouverture, et deux fiches du meme testeur cesseraient d'etre
        // comparables ligne a ligne — c'est precisement ce qu'on fait avec
        // elles quand un defaut dure trois semaines.
        v.sort_by(|a, b| {
            a["name"]
                .as_str()
                .unwrap_or_default()
                .cmp(b["name"].as_str().unwrap_or_default())
        });
        v
    };
    let network = json!({
        "advertise_ip": advertise_ip,
        "port": state.port,
        // Le compte SEPAREMENT de la liste : une liste vide et une liste
        // absente se lisent pareil dans un JSON qu'on parcourt a l'oeil.
        "media_servers_count": serveurs_multimedia.len(),
        "media_servers": serveurs_multimedia,
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

    /// La même exigence, mais énoncée par la classification du dépôt plutôt
    /// que par une liste de fragments écrite à la main ici (#2856). C'est
    /// `tune_core::secrets::est_secret` qui décide ailleurs ce qu'on caviarde ;
    /// une clé qu'elle juge secrète n'a rien à faire dans une fiche jointe à un
    /// ticket, ni dans le rapport de bogue qui lit la même liste.
    #[test]
    fn l_allowlist_ne_porte_aucune_cle_jugee_secrete_par_le_depot() {
        for (key, _) in SUPPORT_SETTING_KEYS {
            assert!(
                !tune_core::secrets::est_secret(key),
                "le réglage {key:?} est classé secret par tune_core::secrets \
                 et ne doit pas partir dans un ticket support (#2856)"
            );
        }
    }

    /// Seules les clés de l'allowlist sortent ; un store contenant des secrets
    /// n'en laisse fuiter aucun, et les valeurs texte sont re-typées.
    /// #3577 — le panneau Paroles s'ouvre vide et la fiche support ne disait
    /// pas LEQUEL des deux verrous s'est referme.
    ///
    /// `routes/library/tracks.rs` rend le MEME `404 {"error":"no_lyrics"}`
    /// pour « ce titre n'a pas de paroles » et pour « la recherche en ligne
    /// est eteinte ». Sans ce booleen dans la fiche, l'ecart n'etait pas
    /// mesurable apres coup : le `diagnostic.md` du ticket support 93 ne le
    /// portait pas, et l'issue a du clore sur « valeur NON ETABLIE ».
    #[test]
    fn la_fiche_publie_le_consentement_des_paroles_en_ligne() {
        assert!(
            SUPPORT_SETTING_KEYS
                .iter()
                .any(|(k, _)| *k == "lyrics_lrclib_enabled"),
            "sans cette cle, un ticket « panneau Paroles vide » reste indecidable"
        );
        // Absent du store = eteint, exactement la regle du serveur :
        // `settings.get(...).as_deref() == Some("true")`.
        let out = support_settings(|_| None);
        assert_eq!(out["lyrics_lrclib_enabled"], json!(false));
        // Et la valeur persistee est rendue telle quelle, re-typee.
        let out = support_settings(|k| (k == "lyrics_lrclib_enabled").then(|| "true".to_string()));
        assert_eq!(out["lyrics_lrclib_enabled"], json!(true));
    }

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
