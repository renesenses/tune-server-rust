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
///
/// Quinze langues depuis le norvégien bokmål, le polonais et le grec (rc9) — les mêmes que
/// l'interface web (`tune-web-client/src/lib/locales/`). Chaque clé de
/// `i18n_server.json` doit porter chacune d'elles : le test
/// `chaque_cle_porte_chaque_langue` le garde.
pub const SUPPORTED: &[&str] = &[
    "fr", "en", "de", "es", "it", "zh", "ja", "ko", "ro", "sv", "hu", "vi", "nb", "pl", "el",
];

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
    alias_langue(
        tag.trim()
            .split('-')
            .next()
            .unwrap_or("")
            .trim()
            .to_lowercase(),
    )
}

/// Le norvégien s'annonce `no` (ou `nn`, nynorsk, faute de mieux) : l'interface
/// ne parle que le bokmål, `nb`. Point unique de cet alias côté serveur — le
/// client web fait de même (`langueParDefaut.ts`). `base` est déjà en
/// minuscules.
pub fn alias_langue(base: String) -> String {
    match base.as_str() {
        "no" | "nn" => "nb".to_string(),
        _ => base,
    }
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
    t_opt(lang, key).unwrap_or_else(|| key.to_string())
}

/// Comme [`t`], mais `None` quand la clé n'existe pas dans la table, au lieu
/// de rendre la clé brute. Pour un texte qui a déjà une valeur par défaut —
/// la description qu'un greffon déclare lui-même (#6458) — et qui ne doit être
/// remplacé que si une traduction existe.
pub fn t_opt(lang: &str, key: &str) -> Option<String> {
    let per_lang = table().get(key)?;
    [lang, FALLBACK, "fr"]
        .into_iter()
        .find_map(|langue| per_lang.get(langue).cloned())
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

    /// Un navigateur norvégien envoie `no`, `nb-NO` ou `nn-NO` : tous donnent
    /// le bokmål. Passe par `base_tag`, donc par `langues_acceptees`,
    /// `lang_from_header` et chaque appelant, sans les toucher.
    ///
    /// Contre-épreuve : retirer le bras `"no" | "nn"` de `alias_langue` fait
    /// rougir ce test (les en-têtes `no` et `nn` retombent sur `"en"`).
    #[test]
    fn no_et_nn_donnent_nb() {
        assert_eq!(base_tag("no"), "nb");
        assert_eq!(base_tag("nn-NO"), "nb");
        assert_eq!(base_tag("NO"), "nb");
        assert_eq!(base_tag("nb-NO"), "nb");
        assert_eq!(lang_from_header(&en_tetes("no,en;q=0.5")), "nb");
        assert_eq!(lang_from_header(&en_tetes("nn-NO,nn;q=0.9,en;q=0.5")), "nb");
        assert_eq!(langues_acceptees("no-NO,en;q=0.5"), vec!["nb", "en"]);
        assert_eq!(base_tag("fr"), "fr");
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

    /// Le hongrois et le vietnamien sont des langues de l'interface : un
    /// `Accept-Language` qui les demande doit les obtenir, et non l'anglais.
    ///
    /// Contre-épreuve : retirer `"hu"` et `"vi"` de `SUPPORTED` fait rougir
    /// ce test (les deux en-têtes retombent sur `"en"`).
    #[test]
    fn le_hongrois_et_le_vietnamien_sont_servis() {
        assert_eq!(lang_from_header(&en_tetes("hu-HU,hu;q=0.9,en;q=0.8")), "hu");
        assert_eq!(lang_from_header(&en_tetes("vi-VN,vi;q=0.9,en;q=0.8")), "vi");
        assert_eq!(
            t("hu", "metafield.composer"),
            "Szerző",
            "le texte hongrois de la table"
        );
        assert_eq!(t("vi", "metafield.composer"), "Nhà soạn nhạc");
        assert_eq!(
            t("vi", "ai.zonesAvailable"),
            "Các vùng phát khả dụng: {zones}",
            "« zone » se dit « vùng phát », comme dans l'interface web"
        );
        // Une langue inconnue de l'interface retombe sur l'anglais.
        assert_eq!(lang_from_header(&en_tetes("pt-BR")), "en");
        assert_eq!(t("pt", "metafield.composer"), t("en", "metafield.composer"));
    }

    /// Le norvégien bokmål, le polonais et le grec sont des langues de
    /// l'interface : un `Accept-Language` qui les demande doit les obtenir.
    ///
    /// Contre-épreuve : retirer `"nb"`, `"pl"` et `"el"` de `SUPPORTED` fait
    /// rougir ce test (les trois en-têtes retombent sur `"en"`).
    #[test]
    fn le_norvegien_le_polonais_et_le_grec_sont_servis() {
        assert_eq!(lang_from_header(&en_tetes("nb-NO,nb;q=0.9,en;q=0.8")), "nb");
        assert_eq!(lang_from_header(&en_tetes("pl-PL,pl;q=0.9,en;q=0.8")), "pl");
        assert_eq!(lang_from_header(&en_tetes("el-GR,el;q=0.9,en;q=0.8")), "el");
        assert_eq!(t("nb", "metafield.composer"), "Komponist");
        assert_eq!(t("pl", "metafield.composer"), "Kompozytor");
        assert_eq!(t("el", "metafield.composer"), "Συνθέτης");
        assert_eq!(
            t("el", "ai.zonesAvailable"),
            "Διαθέσιμες ζώνες: {zones}",
            "« zone » se dit « ζώνη », comme dans l'interface web"
        );
    }

    /// Les jetons `{…}` d'un texte : `{title}`, `{n}`…
    fn jetons(texte: &str) -> Vec<String> {
        let mut vus: Vec<String> = Vec::new();
        let mut reste = texte;
        while let Some(debut) = reste.find('{') {
            let Some(fin) = reste[debut..].find('}') else {
                break;
            };
            vus.push(reste[debut..debut + fin + 1].to_string());
            reste = &reste[debut + fin + 1..];
        }
        vus.sort();
        vus
    }

    /// Complétude de la table : chaque clé porte CHAQUE langue de
    /// [`super::SUPPORTED`], non vide, avec les mêmes jetons `{…}` que
    /// l'anglais. Une langue absente d'une clé se replierait en silence sur
    /// l'anglais — exactement le défaut que l'ajout du hongrois et du
    /// vietnamien aurait laissé passer sur les 199 clés.
    ///
    /// Contre-épreuve : retirer une ligne `"vi"` (ou `"hu"`) de
    /// `i18n_server.json`, ou y supprimer un `{title}`, fait rougir ce test en
    /// nommant la clé et la langue.
    #[test]
    fn chaque_cle_porte_chaque_langue() {
        let mut manques: Vec<String> = Vec::new();
        assert!(!super::table().is_empty(), "i18n_server.json illisible");
        let mut cles: Vec<&String> = super::table().keys().collect();
        cles.sort();
        for cle in cles {
            let par_langue = &super::table()[cle];
            let reference = par_langue.get("en").map(|s| jetons(s));
            for langue in super::SUPPORTED {
                match par_langue.get(*langue) {
                    None => manques.push(format!("{cle} : «{langue}» absente")),
                    Some(v) if v.trim().is_empty() => {
                        manques.push(format!("{cle} : «{langue}» vide"))
                    }
                    Some(v) => {
                        if let Some(attendus) = &reference
                            && &jetons(v) != attendus
                        {
                            manques.push(format!(
                                "{cle} : «{langue}» n'a pas les jetons {attendus:?} de l'anglais"
                            ));
                        }
                    }
                }
            }
            for langue in par_langue.keys() {
                if !super::SUPPORTED.contains(&langue.as_str()) {
                    manques.push(format!("{cle} : langue «{langue}» hors de SUPPORTED"));
                }
            }
        }
        assert!(
            manques.is_empty(),
            "table incomplète :\n{}",
            manques.join("\n")
        );
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

    /// #6857 (Alesp, fil 2395) — le refus « Lecture Spotify non activée »
    /// envoyait à « Réglages > Spotify », qui n'existe pas. Le réglage est
    /// dans l'onglet « Accès et jetons », carte « Spotify Connect », au niveau
    /// Avancé. Chaque langue doit nommer la carte et l'onglet tels que
    /// l'interface les affiche (`tune-web-client/src/lib/locales/*.ts`,
    /// `settings.tabAccess`).
    ///
    /// Contre-épreuve : remettre les anciennes phrases (« Réglages >
    /// Spotify ») dans `i18n_server.json` fait tomber ce test.
    #[test]
    fn le_refus_spotify_donne_le_vrai_chemin_du_reglage() {
        let onglet = [
            ("fr", "Accès et jetons"),
            ("en", "Access and tokens"),
            ("de", "Zugang und Token"),
            ("es", "Acceso y tokens"),
            ("it", "Accesso e token"),
            ("zh", "访问与令牌"),
            ("ja", "アクセスとトークン"),
            ("ko", "접근과 토큰"),
            ("ro", "Acces și jetoane"),
            ("sv", "Åtkomst och tokens"),
            ("hu", "Hozzáférés és tokenek"),
            ("vi", "Truy cập và token"),
            ("nb", "Tilgang og tokener"),
            ("pl", "Dostęp i tokeny"),
            ("el", "Πρόσβαση και διακριτικά"),
        ];
        assert_eq!(onglet.len(), super::SUPPORTED.len());
        for (lang, libelle) in onglet {
            let m = t(lang, "spotify.playback.disabled");
            assert!(
                m.contains(libelle),
                "{lang} : onglet « {libelle} » absent de {m:?}"
            );
            assert!(
                m.contains("Spotify Connect"),
                "{lang} : carte absente de {m:?}"
            );
            assert!(
                !m.contains("> Spotify"),
                "{lang} : ancien chemin dans {m:?}"
            );
        }
    }
}
