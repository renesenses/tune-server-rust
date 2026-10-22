//! Textes du chemin du signal : langue propre à chaque requête/connexion.
//! Catalogue indépendant des messages globaux du serveur (#6496, suite #6458).
use axum::http::HeaderMap;
use std::collections::BTreeMap;
use std::sync::LazyLock;

static CATALOGUE: LazyLock<BTreeMap<String, BTreeMap<String, String>>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("textes_signal.json")).expect("catalogue signal valide")
});

#[derive(Default, serde::Deserialize)]
pub struct LangueSignal {
    pub lang: Option<String>,
}

fn reconnue(value: &str) -> Option<&'static str> {
    let base = value.trim().split(['-', '_']).next()?.to_ascii_lowercase();
    [
        "fr", "en", "de", "es", "it", "zh", "ja", "ko", "ro", "sv", "vi", "hu",
    ]
    .into_iter()
    .find(|l| *l == base)
}

/// Le choix explicite Tune prime ; sinon négociation Accept-Language, puis anglais.
/// Ne passe pas par le catalogue global, qui peut ne pas encore connaître hu/vi.
pub fn langue_requete(query: Option<&str>, headers: &HeaderMap) -> &'static str {
    if let Some(value) = query.filter(|v| !v.trim().is_empty()) {
        return reconnue(value).unwrap_or("en");
    }
    let mut choix = headers
        .get("accept-language")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .split(',')
        .enumerate()
        .filter_map(|(index, part)| {
            let mut parts = part.trim().split(';');
            let lang = reconnue(parts.next()?)?;
            let quality = parts
                .find_map(|p| p.trim().strip_prefix("q="))
                .map_or(Some(1.0), |q| q.parse::<f32>().ok())?;
            (quality > 0.0 && quality <= 1.0).then_some((lang, quality, index))
        })
        .collect::<Vec<_>>();
    choix.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.2.cmp(&b.2)));
    choix.first().map_or("en", |c| c.0)
}

pub(super) fn texte(lang: &str, key: &str) -> &'static str {
    CATALOGUE[reconnue(lang).unwrap_or("en")]
        .get(key)
        .or_else(|| CATALOGUE["en"].get(key))
        .map(String::as_str)
        .expect("clé de texte du signal connue")
}

pub(super) fn formater(lang: &str, key: &str, valeurs: &[&str]) -> String {
    // Un seul passage : un nom de greffon contenant « {1} » reste un nom.
    let modele = texte(lang, key);
    let mut sortie = String::new();
    let mut reste = modele;
    while let Some(start) = reste.find('{') {
        sortie.push_str(&reste[..start]);
        let Some(end) = reste[start..].find('}') else {
            break;
        };
        let index = reste[start + 1..start + end]
            .parse::<usize>()
            .expect("paramètre numérique");
        sortie.push_str(valeurs[index]);
        reste = &reste[start + end + 1..];
    }
    sortie.push_str(reste);
    sortie
}

// Portée de la FUTURE de requête (pas une variable de thread/processus).
// Les réponses play/pause/resume réutilisent build_zone_json à plusieurs
// niveaux : cette portée couvre aussi ces retours sans modifier leur audio.
tokio::task_local! { static LANGUE: &'static str; }

pub(super) fn langue_courante() -> &'static str {
    LANGUE.try_with(|lang| *lang).unwrap_or("fr")
}

pub(crate) async fn avec_langue(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let query = axum::extract::Query::<LangueSignal>::try_from_uri(request.uri())
        .map(|q| q.0)
        .unwrap_or_default();
    let lang = langue_requete(query.lang.as_deref(), request.headers());
    LANGUE.scope(lang, next.run(request)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn douze_catalogues_complets_et_parametres_identiques() {
        assert_eq!(CATALOGUE.len(), 12);
        fn parametres(s: &str) -> Vec<&str> {
            let mut v = s
                .split('{')
                .skip(1)
                .map(|s| s.split('}').next().unwrap())
                .collect::<Vec<_>>();
            v.sort();
            v
        }
        for (lang, messages) in CATALOGUE.iter() {
            assert_eq!(
                messages.keys().collect::<Vec<_>>(),
                CATALOGUE["fr"].keys().collect::<Vec<_>>(),
                "{lang}"
            );
            for (key, value) in messages {
                assert!(!value.is_empty(), "{lang}/{key}");
                assert_eq!(
                    parametres(value),
                    parametres(&CATALOGUE["fr"][key]),
                    "{lang}/{key}"
                );
            }
        }
    }
    #[test]
    fn choix_tune_prime_sur_navigateur_y_compris_hu_vi() {
        let mut headers = HeaderMap::new();
        headers.insert("accept-language", "fr-FR,en;q=0.8".parse().unwrap());
        assert_eq!(langue_requete(Some("vi-VN"), &headers), "vi");
        assert_eq!(langue_requete(Some("hu"), &headers), "hu");
        assert_eq!(langue_requete(None, &headers), "fr");
        headers.insert("accept-language", "fr;q=0,vi;q=0.9,hu;q=1".parse().unwrap());
        assert_eq!(langue_requete(None, &headers), "hu");
        assert_eq!(langue_requete(Some("xx"), &headers), "en");
    }
    #[test]
    fn interpolation_ne_reinterprete_pas_les_valeurs() {
        assert_eq!(
            formater("en", "eq_realistic", &["{1}", "limiter"]),
            "EQ active (realistic headroom, {1}, limiter)"
        );
    }
}
