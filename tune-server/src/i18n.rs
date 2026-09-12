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
pub const SUPPORTED: &[&str] = &["fr", "en", "de", "es", "it", "zh", "ja", "ko", "ro", "sv"];

const RAW: &str = include_str!("i18n_server.json");

fn table() -> &'static HashMap<String, HashMap<String, String>> {
    static TABLE: OnceLock<HashMap<String, HashMap<String, String>>> = OnceLock::new();
    TABLE.get_or_init(|| serde_json::from_str(RAW).unwrap_or_default())
}

/// Base d'une étiquette de langue, en minuscules : `fr-FR` → `fr`, `EN` →
/// `en`, `zh-Hant-TW` → `zh`. C'est la forme sous laquelle les langues sont
/// comparées partout côté serveur (`SUPPORTED`, blocs de notes de version…).
pub fn base_tag(tag: &str) -> String {
    tag.trim()
        .split('-')
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase()
}

/// Resolve the request language from `Accept-Language`, restricted to a
/// supported base tag (e.g. `fr-FR,fr;q=0.9,en;q=0.8` -> `fr`). Defaults to fr.
pub fn lang_from_header(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.split(',').find_map(|part| {
                let base = base_tag(part.split(';').next().unwrap_or(""));
                SUPPORTED.contains(&base.as_str()).then_some(base)
            })
        })
        .unwrap_or_else(|| "fr".to_string())
}

/// Langue demandée par une requête, dans cet ordre de précédence :
/// 1. le paramètre `?lang=` explicite et non vide — un appel qui nomme sa
///    langue gagne ;
/// 2. l'en-tête `Accept-Language`, via [`lang_from_header`] — la locale que le
///    client web envoie sur CHAQUE requête ;
/// 3. `fr`, repli porté par `lang_from_header`.
///
/// Il n'existe pas de « langue de l'instance » côté serveur : la langue est
/// un choix de l'interface, transmis à chaque appel. C'est LA résolution
/// partagée par les bios (`routes/library/artists.rs`) et par les notes de
/// version (`routes/system/update.rs`, #3089) — ne pas en réécrire une autre.
///
/// Un `?lang=` vide (`?lang=`) est traité comme absent : il ne nomme aucune
/// langue. Le paramètre est rendu tel quel (trim), sans restriction à
/// `SUPPORTED` : chaque appelant décide de ce qu'il fait d'une langue qu'il
/// ne sert pas.
pub fn lang_from_request(param: Option<&str>, headers: &HeaderMap) -> String {
    param
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| lang_from_header(headers))
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

#[cfg(test)]
mod tests {
    use super::{base_tag, lang_from_header, lang_from_request};
    use axum::http::HeaderMap;

    fn en_tetes(accept: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("accept-language", accept.parse().unwrap());
        h
    }

    #[test]
    fn base_tag_reduit_a_la_base_minuscule() {
        assert_eq!(base_tag("fr-FR"), "fr");
        assert_eq!(base_tag(" EN-gb "), "en");
        assert_eq!(base_tag("zh-Hant-TW"), "zh");
        assert_eq!(base_tag(""), "");
    }

    #[test]
    fn header_garde_sa_lecture() {
        assert_eq!(lang_from_header(&en_tetes("en-GB,en;q=0.9,fr;q=0.8")), "en");
        assert_eq!(lang_from_header(&en_tetes("pt-BR,pt;q=0.9")), "fr");
        assert_eq!(lang_from_header(&HeaderMap::new()), "fr");
    }

    #[test]
    fn le_parametre_gagne_puis_l_en_tete_puis_fr() {
        let h = en_tetes("de-DE,de;q=0.9");
        assert_eq!(lang_from_request(Some("en"), &h), "en");
        assert_eq!(
            lang_from_request(Some("  "), &h),
            "de",
            "un ?lang= vide est absent"
        );
        assert_eq!(lang_from_request(None, &h), "de");
        assert_eq!(lang_from_request(None, &HeaderMap::new()), "fr");
    }
}
