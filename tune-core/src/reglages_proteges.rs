//! Les réglages que l'écriture GÉNÉRIQUE de la table `settings` ne touche pas
//! sans une authentification admin effective.
//!
//! Deux chemins écrivent une clé quelconque reçue par HTTP : `PATCH
//! /system/config` et les imports de configuration (`/system/config/import`,
//! `/system/config-backup/import`). L'interface s'en sert pour des centaines de
//! réglages ordinaires (thème, ReplayGain, file aléatoire…), dont une partie
//! sont des clés composées à la volée : une liste exhaustive de ce qui est
//! permis casserait un écran à chaque réglage ajouté, sans message.
//!
//! La règle est donc fermée de l'autre côté : sont **protégés**
//!
//! - les secrets du serveur lui-même (`tune_core::secrets::est_secret`), sauf
//!   les identifiants de services tiers que l'utilisateur saisit lui-même ;
//! - l'authentification, la licence, le compte Mozaiklabs, le relais,
//!   l'écoute à distance, l'identité et l'appairage du serveur ;
//! - toute clé qui désigne un chemin (`_path`, `_dir`…), un exécutable
//!   (`_bin`, `_cmd`…) ou l'adresse d'un service (`_url`, `_uri`,
//!   `_endpoint`…), sauf les quelques réglages de l'interface nommés dans
//!   [`LIBRES_MALGRE_LEUR_NOM`].
//!
//! Une clé protégée ne s'écrit par ces chemins que lorsque l'authentification
//! est ACTIVE (`auth_enabled = true`) : la route exige alors un jeton admin
//! valide. Authentification désactivée, `RequireAdmin` laisse tout passer ; ces
//! clés-là sont alors refusées, parce qu'elles changent qui contrôle le
//! serveur, ce qu'il exécute ou à qui il parle.
//!
//! Chaque route garde ses propres réglages dédiés (connexion aux services,
//! dossiers de musique, relais…) : ce module ne ferme que la porte générique.

use crate::secrets::est_secret;

/// Noms exacts protégés (comparaison en minuscules).
const PROTEGES: &[&str] = &[
    "auth_enabled",
    "api_key",
    "jwt_secret",
    "server_mode",
    "server_id",
    "hardware_fingerprint",
    "remote_server_url",
    "active_server",
    "multi_server_list",
    "artwork_proxy_hosts",
    "artwork_proxy_secret",
    "credentials_vault",
    "ha_url",
    "ha_token",
    "hue_bridge_ip",
    "hue_username",
    "yt_dlp_path",
];

/// Préfixes protégés : authentification, licence, compte, relais, écoute à
/// distance, identité et appairages du serveur.
const PREFIXES_PROTEGES: &[&str] = &[
    "auth_",
    "jwt",
    "license",
    "licence",
    "mozaik_",
    "bridge_",
    "ecoute_directe_",
    "ecoute_distante_",
    "agent_tune_",
    "airplay2_pairing",
    "cloud_server_",
    "sauvegarde_cloud_",
    "developer_",
];

/// Suffixes qui désignent un chemin, un exécutable ou l'adresse d'un service.
const SUFFIXES_PROTEGES: &[&str] = &[
    "_url",
    "_urls",
    "_uri",
    "_uris",
    "_host_url",
    "_endpoint",
    "_endpoints",
    "_base",
    "_path",
    "_paths",
    "_dir",
    "_dirs",
    "_file",
    "_bin",
    "_binary",
    "_exe",
    "_executable",
    "_cmd",
    "_command",
    "_program",
];

/// Réglages de l'interface qui portent un suffixe de [`SUFFIXES_PROTEGES`]
/// sans désigner ni un exécutable ni un service : ils RESTREIGNENT un travail
/// de fond à une partie de la bibliothèque déjà déclarée. Chaque ligne cite
/// son écran.
const LIBRES_MALGRE_LEUR_NOM: &[&str] = &[
    // Les racines de la bibliothèque : `PATCH /system/config` les accepte
    // (compte des pistes retirées), comme `/system/music-dirs`, qui reste
    // ouvert au même appelant. Les protéger ici ne fermerait rien.
    "music_dirs",
    // Réglages > Analyses : dossiers inclus (`dossiersAnalyses.ts`).
    "background_analysis_included_dirs",
    // Réglages > Métadonnées : dossiers exclus de l'enrichissement.
    "metadata_enrichment_excluded_dirs",
    // Exclusions du scan.
    "scan_exclude_paths",
];

/// Identifiants de services TIERS, saisis par l'utilisateur pour son propre
/// compte : ils portent un fragment secret, mais ne donnent aucun pouvoir sur
/// le serveur. Les jetons de streaming (`auth_tokens_*`) en font partie : une
/// restauration de sauvegarde sur un serveur neuf doit les remettre en place.
const IDENTIFIANTS_DE_SERVICES_TIERS: &[&str] = &[
    "acoustid_api_key",
    "discogs_token",
    "lastfm_api_key",
    "lastfm_api_secret",
    "lastfm_session_key",
    "genius_token",
    "setlistfm_api_key",
    "audd_api_token",
    "acrcloud_access_key",
    "acrcloud_access_secret",
    "listenbrainz_token",
    "radiofrance_api_key",
];

const PREFIXE_JETONS_DE_STREAMING: &str = "auth_tokens_";

fn est_identifiant_de_service_tiers(cle: &str) -> bool {
    IDENTIFIANTS_DE_SERVICES_TIERS.contains(&cle)
        || (cle.starts_with(PREFIXE_JETONS_DE_STREAMING)
            && cle.len() > PREFIXE_JETONS_DE_STREAMING.len()
            && cle[PREFIXE_JETONS_DE_STREAMING.len()..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
}

/// Cette clé ne s'écrit-elle par les routes génériques qu'avec une
/// authentification admin effective ?
///
/// Le test porte sur le NOM, en minuscules, jamais sur la valeur.
pub fn est_protege(cle: &str) -> bool {
    let cle = cle.trim().to_ascii_lowercase();
    if cle.is_empty() {
        return true;
    }
    // Un nom qui porte un caractère de contrôle ou un séparateur de chemin
    // n'est pas un réglage de l'interface.
    if cle.chars().any(|c| c.is_control() || c == '/' || c == '\\') {
        return true;
    }
    if est_identifiant_de_service_tiers(&cle) {
        return false;
    }
    if LIBRES_MALGRE_LEUR_NOM.contains(&cle.as_str()) {
        return false;
    }
    if PROTEGES.contains(&cle.as_str()) {
        return true;
    }
    if PREFIXES_PROTEGES.iter().any(|p| cle.starts_with(p)) {
        return true;
    }
    if SUFFIXES_PROTEGES.iter().any(|s| cle.ends_with(s)) {
        return true;
    }
    est_secret(&cle)
}

/// Les clés protégées d'un lot, dans l'ordre reçu.
pub fn cles_protegees<'a, I>(cles: I) -> Vec<String>
where
    I: IntoIterator<Item = &'a str>,
{
    cles.into_iter()
        .filter(|c| est_protege(c))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::est_protege;

    #[test]
    fn les_cles_de_securite_sont_protegees() {
        for cle in [
            "jwt_secret",
            "auth_enabled",
            "api_key",
            "mozaik_base_url",
            "mozaik_premium",
            "mozaik_modules",
            "license_tier",
            "license_key",
            "yt_dlp_path",
            "bridge_url",
            "bridge_token",
            "server_mode",
            "remote_server_url",
            "ha_url",
            "hue_bridge_ip",
            "audio_embedding_model_path",
            "cd_rip_output_dir",
            "offline_cache_dir",
            "wikipedia_base_url",
            "ecoute_directe_relais_iroh",
            "agent_tune_identite",
            "airplay2_pairing:abc",
            "developer_api_keys",
            "credentials_vault",
            "openai_api_key",
            "ffmpeg_bin",
            "JWT_SECRET",
            "",
            "a/b",
        ] {
            assert!(est_protege(cle), "{cle} doit être protégée");
        }
    }

    #[test]
    fn les_reglages_de_l_interface_restent_libres() {
        for cle in [
            "replaygain_mode",
            "replaygain_preamp_db",
            "local_audio_backend",
            "local_exclusive_mode",
            "shuffle_max_tracks",
            "zone_auto_create",
            "lms_host",
            "squeezebox_enabled",
            "radios_country",
            "shortcuts",
            "server_name",
            "quality_split",
            "max_sample_rate",
            "max_bit_depth",
            "audiophile_lock_volume",
            "slimproto_discovery_enabled",
            "cloud_listens_consent",
            "cloud_listens_upload_enabled",
            "library_write_files_enabled",
            "background_analysis_included_dirs",
            "metadata_enrichment_excluded_dirs",
            "background_analysis_excluded_roots",
            "continue_listening_rewind_seconds",
            "acoustid_api_key",
            "discogs_token",
            "auth_tokens_qobuz",
            "auth_tokens_tidal",
            "dlna:uuid-renderer",
            "music_dirs",
        ] {
            assert!(!est_protege(cle), "{cle} doit rester libre");
        }
    }
}
