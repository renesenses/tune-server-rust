//! Server-side i18n for user-facing strings returned by the API.
//!
//! The web client sends its *selected* UI locale in the `Accept-Language`
//! header (it overrides the browser default), so `lang_from_header` yields the
//! language the user actually picked in the app, and server-provided strings
//! (metadata field labels, errors, …) match the rest of the UI. Falls back to
//! French — the app's default — then to the key itself.
//!
//! Translations live in `i18n_server.json` (`{ key: { lang: value } }`),
//! embedded at build time and parsed once.

use std::collections::HashMap;
use std::sync::OnceLock;

use axum::http::HeaderMap;

/// Languages the UI ships with. Order is irrelevant; membership gates the
/// `Accept-Language` parse so an unsupported browser locale falls back to fr.
pub const SUPPORTED: &[&str] = &["fr", "en", "de", "es", "it", "zh", "ja", "ko"];

const RAW: &str = include_str!("i18n_server.json");

fn table() -> &'static HashMap<String, HashMap<String, String>> {
    static TABLE: OnceLock<HashMap<String, HashMap<String, String>>> = OnceLock::new();
    TABLE.get_or_init(|| serde_json::from_str(RAW).unwrap_or_default())
}

/// Resolve the request language from `Accept-Language`, restricted to a
/// supported base tag (e.g. `fr-FR,fr;q=0.9,en;q=0.8` -> `fr`). Defaults to fr.
pub fn lang_from_header(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.split(',').find_map(|part| {
                let tag = part.split(';').next().unwrap_or("").trim().to_lowercase();
                let base = tag.split('-').next().unwrap_or("");
                SUPPORTED.contains(&base).then(|| base.to_string())
            })
        })
        .unwrap_or_else(|| "fr".to_string())
}

/// Translate `key` into `lang`, falling back to French, then to the key itself.
pub fn t(lang: &str, key: &str) -> String {
    if let Some(per_lang) = table().get(key) {
        if let Some(v) = per_lang.get(lang) {
            return v.clone();
        }
        if let Some(v) = per_lang.get("fr") {
            return v.clone();
        }
    }
    key.to_string()
}
