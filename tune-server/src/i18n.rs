//! Server-side i18n for user-facing strings returned by the API.
//!
//! The web client sends its *selected* UI locale in the `Accept-Language`
//! header (it overrides the browser default), so `lang_from_header` yields the
//! language the user actually picked in the app, and server-provided strings
//! (metadata field labels, errors, …) match the rest of the UI. Without a
//! supported language in the header, the server answers in English — the same
//! fallback as the web client (browser language, then English) — never in
//! French by default.
//!
//! Translations live in `i18n_server.json` (`{ key: { lang: value } }`),
//! embedded at build time and parsed once.

use std::collections::HashMap;
use std::sync::OnceLock;

use axum::http::HeaderMap;

/// Languages the UI ships with. Order is irrelevant; membership gates the
/// `Accept-Language` parse so an unsupported browser locale falls back to
/// [`FALLBACK`].
pub const SUPPORTED: &[&str] = &["fr", "en", "de", "es", "it", "zh", "ja", "ko", "ro", "sv"];

/// Langue de repli du serveur : l'anglais, comme le client web. Ni un en-tête
/// absent, ni une langue que l'interface ne parle pas (`pt-BR`), ni une clé
/// non traduite ne doivent faire répondre le serveur en français.
pub const FALLBACK: &str = "en";

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

/// Langues d'un en-tête `Accept-Language`, dans l'ordre de préférence :
/// réduites à leur base ([`base_tag`]), triées par poids `q` décroissant —
/// tri stable, l'ordre d'écriture départage les égalités —, sans les entrées
/// refusées (`q=0`), sans le joker `*` ni les poids illisibles.
///
/// `de;q=0.5, ja, zh-TW;q=0.8` → `["ja", "zh", "de"]`.
pub fn langues_acceptees(entete: &str) -> Vec<String> {
    let mut candidates: Vec<(f32, String)> = entete
        .split(',')
        .filter_map(|part| {
            let mut morceaux = part.split(';');
            let base = base_tag(morceaux.next().unwrap_or(""));
            if base.is_empty() || base == "*" {
                return None;
            }
            let mut q = 1.0_f32;
            for param in morceaux {
                if let Some((nom, valeur)) = param.split_once('=')
                    && nom.trim().eq_ignore_ascii_case("q")
                {
                    q = valeur.trim().parse().ok()?;
                }
            }
            (q.is_finite() && q > 0.0).then_some((q, base))
        })
        .collect();
    candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    candidates.into_iter().map(|(_, base)| base).collect()
}

/// Resolve the request language from `Accept-Language`: the preferred
/// supported language by `q` weight (e.g. `pt-BR,pt;q=0.9,fr;q=0.8` -> `fr`,
/// `zh-TW` -> `zh`). Without any supported language — or without the header
/// — [`FALLBACK`], English.
pub fn lang_from_header(headers: &HeaderMap) -> String {
    headers
        .get(axum::http::header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            langues_acceptees(s)
                .into_iter()
                .find(|base| SUPPORTED.contains(&base.as_str()))
        })
        .unwrap_or_else(|| FALLBACK.to_string())
}

/// Langue demandée par une requête, dans cet ordre de précédence :
/// 1. le paramètre `?lang=` explicite et non vide — un appel qui nomme sa
///    langue gagne ;
/// 2. l'en-tête `Accept-Language`, via [`lang_from_header`] — la locale que le
///    client web envoie sur CHAQUE requête ;
/// 3. l'anglais ([`FALLBACK`]), repli porté par `lang_from_header`.
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

/// Translate `key` into `lang`, falling back to English ([`FALLBACK`]), then
/// to French — the language the table is written in first —, then to the key
/// itself.
pub fn t(lang: &str, key: &str) -> String {
    if let Some(per_lang) = table().get(key) {
        for langue in [lang, FALLBACK, "fr"] {
            if let Some(v) = per_lang.get(langue) {
                return v.clone();
            }
        }
    }
    key.to_string()
}

#[cfg(test)]
mod tests {
    use super::{FALLBACK, base_tag, lang_from_header, lang_from_request, langues_acceptees, t};
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
        assert_eq!(lang_from_header(&en_tetes("fr-FR,fr;q=0.9,en;q=0.8")), "fr");
    }

    /// Le défaut : un en-tête absent, vide, illisible ou sans aucune langue
    /// prise en charge faisait répondre le serveur en français.
    ///
    /// Contre-épreuve : remettre `"fr"` dans le `unwrap_or_else` de
    /// `lang_from_header` fait rougir ce test.
    #[test]
    fn sans_langue_prise_en_charge_le_repli_est_l_anglais() {
        assert_eq!(FALLBACK, "en");
        assert_eq!(lang_from_header(&HeaderMap::new()), "en");
        assert_eq!(lang_from_header(&en_tetes("")), "en");
        assert_eq!(lang_from_header(&en_tetes("pt-BR,pt;q=0.9")), "en");
        assert_eq!(lang_from_header(&en_tetes("*")), "en");
        assert_eq!(lang_from_header(&en_tetes("pt;q=abc")), "en");
    }

    /// Les poids `q` gouvernent, pas l'ordre d'écriture. L'ancienne lecture
    /// prenait la première langue écrite : `de;q=0.1, ja` rendait `de`.
    ///
    /// Contre-épreuve : revenir à `find_map` sur l'ordre d'écriture fait
    /// rougir ce test.
    #[test]
    fn le_poids_q_l_emporte_sur_l_ordre_d_ecriture() {
        assert_eq!(lang_from_header(&en_tetes("de;q=0.1, ja")), "ja");
        assert_eq!(lang_from_header(&en_tetes("en;q=0.5,fr;q=0.9")), "fr");
        assert_eq!(
            lang_from_header(&en_tetes("pt-BR,pt;q=0.9,it;q=0.7,de;q=0.8")),
            "de",
            "la première langue PRISE EN CHARGE, par poids"
        );
        assert_eq!(
            lang_from_header(&en_tetes("fr;q=0, de;q=0.2")),
            "de",
            "q=0 refuse la langue"
        );
        assert_eq!(
            lang_from_header(&en_tetes("ko;q=0.8, sv;q=0.8")),
            "ko",
            "à poids égal, l'ordre d'écriture départage"
        );
    }

    #[test]
    fn le_prefixe_de_langue_est_retenu() {
        assert_eq!(lang_from_header(&en_tetes("zh-TW")), "zh");
        assert_eq!(lang_from_header(&en_tetes("zh-Hant-TW;q=0.9, pt")), "zh");
        assert_eq!(lang_from_header(&en_tetes("ES-mx ; q=0.7")), "es");
    }

    #[test]
    fn langues_acceptees_trie_et_filtre() {
        assert_eq!(
            langues_acceptees("de;q=0.5, ja, zh-TW;q=0.8, *;q=0.1, it;q=0"),
            ["ja", "zh", "de"]
        );
        assert!(langues_acceptees("").is_empty());
    }

    /// Une clé inconnue de la langue demandée se replie sur l'anglais, pas sur
    /// le français. Contre-épreuve : remettre `"fr"` en premier repli de `t`.
    #[test]
    fn une_traduction_absente_se_replie_sur_l_anglais() {
        let cle = "cloud.ssoEchec";
        assert_ne!(t("en", cle), t("fr", cle), "la clé témoin a deux textes");
        assert_eq!(t("pt", cle), t("en", cle));
        assert_eq!(t("xx", "cle.qui.n.existe.pas"), "cle.qui.n.existe.pas");
    }

    #[test]
    fn le_parametre_gagne_puis_l_en_tete_puis_l_anglais() {
        let h = en_tetes("de-DE,de;q=0.9");
        assert_eq!(lang_from_request(Some("en"), &h), "en");
        assert_eq!(
            lang_from_request(Some("  "), &h),
            "de",
            "un ?lang= vide est absent"
        );
        assert_eq!(lang_from_request(None, &h), "de");
        assert_eq!(lang_from_request(None, &HeaderMap::new()), "en");
    }
}
