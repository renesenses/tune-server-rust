//! Traduction des requêtes d'ambiance vers l'anglais.
//!
//! La tour texte du CLAP est entraînée en anglais : une requête libre en
//! français recalait des pistes pertinentes (les 8 presets contournent le
//! problème avec des requêtes anglaises codées en dur, et le modèle
//! `name`/`query` des ambiances sauvegardées a été séparé exprès pour
//! brancher ceci un jour — `routes/library/ambiances.rs`).
//!
//! Multi-fournisseur, avec la clé de L'UTILISATEUR (aucune clé Mozaiklabs) :
//! `anthropic_api_key`, `openai_api_key`, `gemini_api_key` dans les settings —
//! la première configurée gagne. Sans clé : `None`, l'appelant garde la
//! requête brute (comportement historique, rien ne casse).
//!
//! Cache borné dans le setting `ambiance_query_translations` (JSON
//! `{requête: traduction}`) : une requête donnée n'est traduite qu'une fois,
//! y compris entre redémarrages.

use crate::db::settings_repo::SettingsRepo;
use serde_json::{Value, json};
use tracing::{debug, warn};

const CACHE_KEY: &str = "ambiance_query_translations";
const CACHE_MAX: usize = 200;
const PROMPT: &str = "Translate this music-mood search query to English. Reply with ONLY the \
translation, nothing else. If it is already in English, reply with it unchanged.";

/// Traduit `query` en anglais via la clé API configurée par l'utilisateur.
/// `None` = pas de clé, échec réseau, ou réponse inutilisable — l'appelant
/// doit alors employer la requête brute.
pub async fn translate_query(settings: &SettingsRepo, query: &str) -> Option<String> {
    let q = query.trim();
    if q.is_empty() {
        return None;
    }
    // #3836 — un NOM DE GENRE se traduit sans clé : « Rock progressif » et
    // « Progressive rock » doivent partir vers la tour texte sous le MÊME
    // texte, sinon deux saisies d'un même genre rendent deux listes.
    if let Some(genre) = nom_de_genre_en_anglais(q) {
        debug!(query = q, translated = genre, "ambiance_translate_genre");
        return Some(genre.to_string());
    }
    if let Some(hit) = cache_get(settings, q) {
        debug!(query = q, translated = %hit, "ambiance_translate_cache_hit");
        return Some(hit);
    }

    let translated = call_provider(settings, q).await?;
    let t = translated.trim().trim_matches('"').trim().to_string();
    if t.is_empty() || t.len() > 300 {
        // Une « traduction » vide ou bavarde (le modèle a répondu autre chose
        // que la traduction seule) ferait pire que la requête brute.
        warn!(query = q, response = %translated, "ambiance_translate_unusable");
        return None;
    }
    cache_put(settings, q, &t);
    debug!(query = q, translated = %t, "ambiance_translate_ok");
    Some(t)
}

/// Noms de genre français (repliés : minuscules, sans accents, espaces
/// simples, tirets en espaces) → graphie anglaise de la hiérarchie des genres
/// (`library::genre_tree`). Seuls les noms dont la forme française DIFFÈRE
/// sont listés ; un nom déjà anglais passe par `genre_tree::nom_canonique`.
///
/// Correspondance de la requête ENTIÈRE, jamais d'un fragment : « jazz doux
/// pour le soir » n'est pas un nom de genre et garde son chemin (clé IA ou
/// requête brute). Un mot qui est aussi une humeur (« romantique ») n'y est
/// pas : le remplacer par un genre changerait le sens de la recherche.
const GENRES_EN_FRANCAIS: &[(&str, &str)] = &[
    ("rock progressif", "Progressive Rock"),
    ("rock progressive", "Progressive Rock"),
    ("rock prog", "Progressive Rock"),
    ("rock alternatif", "Alternative Rock"),
    ("rock independant", "Indie Rock"),
    ("rock inde", "Indie Rock"),
    ("rock classique", "Classic Rock"),
    ("rock psychedelique", "Psychedelic Rock"),
    ("rock garage", "Garage Rock"),
    ("post rock", "Post-Rock"),
    ("metal progressif", "Progressive Metal"),
    ("metal symphonique", "Symphonic Metal"),
    ("jazz vocal", "Vocal Jazz"),
    ("jazz latin", "Latin Jazz"),
    ("jazz manouche", "Gypsy Jazz"),
    ("musique electronique", "Electronic"),
    ("electronique", "Electronic"),
    ("musique classique", "Classical"),
    ("classique", "Classical"),
    ("musique baroque", "Baroque"),
    ("musique contemporaine", "Contemporary Classical"),
    ("opera", "Opera"),
    ("musique de chambre", "Chamber Music"),
    ("musique orchestrale", "Orchestral"),
    ("musique chorale", "Choral"),
    ("minimalisme", "Minimalism"),
    ("musique du monde", "World"),
    ("musiques du monde", "World"),
    ("bande originale", "Soundtrack"),
    ("musique de film", "Film Score"),
    ("musique de jeu video", "Video Game"),
    ("comedie musicale", "Musical"),
    ("chanson francaise", "French Chanson"),
    ("celtique", "Celtic"),
    ("folk independant", "Indie Folk"),
    ("pop independante", "Indie Pop"),
    ("blues electrique", "Electric Blues"),
    ("musique arabe", "Arabic"),
    ("musique classique indienne", "Indian Classical"),
    ("rai", "Raï"),
];

/// Replie une saisie pour la comparer au glossaire : minuscules, accents
/// retirés, tirets et soulignés en espaces, espaces simples.
fn replier(q: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let sans_accents: String = q
        .nfkd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .collect();
    sans_accents
        .to_lowercase()
        .replace(['-', '_'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Le nom anglais canonique d'une requête qui est EXACTEMENT un nom de genre,
/// français ou anglais, quelle que soit sa casse. `None` pour tout le reste.
pub fn nom_de_genre_en_anglais(q: &str) -> Option<&'static str> {
    if let Some(nom) = crate::library::genre_tree::nom_canonique(q) {
        return Some(nom);
    }
    let replie = replier(q);
    GENRES_EN_FRANCAIS
        .iter()
        .find(|(fr, _)| *fr == replie)
        .map(|(_, en)| *en)
        .or_else(|| crate::library::genre_tree::nom_canonique(&replie))
}

/// Les cles API que la traduction sait employer, dans l'ordre de preference :
/// la premiere configuree gagne.
///
/// UNE seule liste, pour deux lecteurs — `call_provider`, qui s'en sert pour
/// choisir le fournisseur, et [`cle_disponible`], qui s'en sert pour repondre
/// « oui » ou « non ». Deux listes auraient derive, et l'ecran Ambiance se
/// serait mis a promettre une traduction que personne n'aurait faite (#3839).
pub const CLES_DE_TRADUCTION: [&str; 3] = ["anthropic_api_key", "openai_api_key", "gemini_api_key"];

/// La premiere cle de traduction configuree : son nom de reglage et sa valeur.
///
/// « Configuree » veut dire NON VIDE : un reglage pose puis efface laisse une
/// chaine blanche, qui ne traduit rien et ne doit donc pas compter.
fn cle_configuree(settings: &SettingsRepo) -> Option<(&'static str, String)> {
    CLES_DE_TRADUCTION.iter().find_map(|nom| {
        settings
            .get(nom)
            .ok()
            .flatten()
            .filter(|v| !v.trim().is_empty())
            .map(|v| (*nom, v))
    })
}

/// Ce serveur peut-il traduire une requete d'ambiance ?
///
/// C'est la seule information dont l'ecran Ambiance a besoin et qu'il ne peut
/// PAS obtenir autrement : les cles API ne sortent d'aucune route, et le
/// client n'a donc aucun moyen de distinguer « deja en anglais » de « pas de
/// cle, requete envoyee brute ». Sans elle, l'ecran ne peut ni conseiller
/// l'anglais ni renvoyer aux reglages — il se tait, et deux libelles du meme
/// genre rendent deux listes sans que rien ne l'explique (#3839, fil 1751).
pub fn cle_disponible(settings: &SettingsRepo) -> bool {
    cle_configuree(settings).is_some()
}

async fn call_provider(settings: &SettingsRepo, q: &str) -> Option<String> {
    let (nom, key) = cle_configuree(settings)?;
    match nom {
        "anthropic_api_key" => anthropic_translate(&key, q).await,
        "openai_api_key" => openai_translate(&key, q).await,
        "gemini_api_key" => gemini_translate(&key, q).await,
        _ => None,
    }
}

async fn anthropic_translate(key: &str, q: &str) -> Option<String> {
    use crate::ai::client::{AnthropicClient, ContentBlock, Message, MessageContent};
    let client = AnthropicClient::new(key.to_string(), String::new());
    let messages = vec![Message {
        role: "user".into(),
        content: MessageContent::Text(q.to_string()),
    }];
    match client.chat(PROMPT, messages, &[]).await {
        Ok(resp) => resp.content.iter().find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        }),
        Err(e) => {
            warn!(error = %e, "ambiance_translate_anthropic_failed");
            None
        }
    }
}

async fn openai_translate(key: &str, q: &str) -> Option<String> {
    let body = json!({
        "model": "gpt-4o-mini",
        "max_tokens": 100,
        "messages": [
            {"role": "system", "content": PROMPT},
            {"role": "user", "content": q},
        ],
    });
    let v = post_json(
        "https://api.openai.com/v1/chat/completions",
        &[("Authorization", &format!("Bearer {key}"))],
        &body,
        "openai",
    )
    .await?;
    v["choices"][0]["message"]["content"]
        .as_str()
        .map(String::from)
}

async fn gemini_translate(key: &str, q: &str) -> Option<String> {
    let body = json!({
        "system_instruction": {"parts": [{"text": PROMPT}]},
        "contents": [{"parts": [{"text": q}]}],
        "generationConfig": {"maxOutputTokens": 100},
    });
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:generateContent?key={key}"
    );
    let v = post_json(&url, &[], &body, "gemini").await?;
    v["candidates"][0]["content"]["parts"][0]["text"]
        .as_str()
        .map(String::from)
}

async fn post_json(
    url: &str,
    headers: &[(&str, &str)],
    body: &Value,
    provider: &str,
) -> Option<Value> {
    let client = crate::http::client::shared();
    let mut req = client
        .post(url)
        .header("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(20))
        .json(body);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await.ok()?;
            if !status.is_success() {
                warn!(provider, status = %status, body = %text, "ambiance_translate_api_error");
                return None;
            }
            serde_json::from_str(&text).ok()
        }
        Err(e) => {
            warn!(provider, error = %e, "ambiance_translate_request_failed");
            None
        }
    }
}

fn cache_get(settings: &SettingsRepo, q: &str) -> Option<String> {
    let raw = settings.get(CACHE_KEY).ok().flatten()?;
    let map: Value = serde_json::from_str(&raw).ok()?;
    map.get(q).and_then(|v| v.as_str()).map(String::from)
}

fn cache_put(settings: &SettingsRepo, q: &str, translated: &str) {
    let mut map: serde_json::Map<String, Value> = settings
        .get(CACHE_KEY)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    // Borne grossière : au-delà, on repart d'un cache neuf plutôt que de
    // gérer un LRU pour un dictionnaire de requêtes d'ambiance.
    if map.len() >= CACHE_MAX {
        map.clear();
    }
    map.insert(q.to_string(), Value::String(translated.to_string()));
    if let Ok(raw) = serde_json::to_string(&Value::Object(map)) {
        let _ = settings.set(CACHE_KEY, &raw);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::SqliteDb;
    use std::sync::Arc;

    fn repo() -> SettingsRepo {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        SettingsRepo::with_backend(Arc::new(db))
    }

    #[test]
    fn cache_round_trip_et_borne() {
        let s = repo();
        assert!(cache_get(&s, "jazz feutré").is_none());
        cache_put(&s, "jazz feutré", "warm intimate jazz");
        assert_eq!(
            cache_get(&s, "jazz feutré").as_deref(),
            Some("warm intimate jazz")
        );
        // La borne vide le cache au lieu de grossir sans fin.
        for i in 0..CACHE_MAX {
            cache_put(&s, &format!("q{i}"), "t");
        }
        cache_put(&s, "après la purge", "after");
        assert_eq!(cache_get(&s, "après la purge").as_deref(), Some("after"));
    }

    /// #3839 — JeromeQ, fil 1751 : « Progressive rock » et « Rock progressif »
    /// rendent deux listes. Sans cle, la seconde part BRUTE dans une tour texte
    /// entrainee en anglais. L'ecran ne peut le dire que si le serveur le dit :
    /// les cles API ne sortent d'aucune route.
    #[test]
    fn la_cle_de_traduction_se_voit_des_qu_une_seule_est_posee_et_non_blanche() {
        let s = repo();
        assert!(
            !cle_disponible(&s),
            "aucune cle : l'ecran doit conseiller l'anglais, pas promettre une traduction"
        );
        for nom in CLES_DE_TRADUCTION {
            s.set(nom, "   ").unwrap();
            assert!(
                !cle_disponible(&s),
                "une cle BLANCHE ne traduit rien, elle ne doit pas compter ({nom})"
            );
            s.set(nom, "sk-temoin").unwrap();
            assert!(cle_disponible(&s), "cle posee sur {nom}");
            s.delete(nom).unwrap();
            assert!(!cle_disponible(&s), "cle retiree de {nom}");
        }
    }

    /// #3836 — JeromeQ, fil 1751 : « Progressive rock » et « Rock progressif »
    /// rendaient deux listes, parce que la seconde partait BRUTE (pas de clé)
    /// et que la tour texte distingue même la casse. Un nom de genre doit
    /// arriver au CLAP sous un seul et même texte, avec ou sans clé.
    #[tokio::test]
    async fn deux_saisies_du_meme_genre_partent_sous_le_meme_texte_sans_cle() {
        let s = repo();
        assert!(!cle_disponible(&s), "le témoin exige l'absence de clé");
        let attendu = Some("Progressive Rock".to_string());
        for saisie in [
            "Progressive rock",
            "progressive rock",
            "Rock progressif",
            "rock progressive",
            "  ROCK   Progressif ",
        ] {
            assert_eq!(translate_query(&s, saisie).await, attendu, "{saisie:?}");
        }
        assert_eq!(
            translate_query(&s, "Musique classique").await.as_deref(),
            Some("Classical")
        );
        assert_eq!(translate_query(&s, "opéra").await.as_deref(), Some("Opera"));
        assert_eq!(translate_query(&s, "Raï").await.as_deref(), Some("Raï"));
    }

    /// Contre-épreuve : le glossaire ne touche ni une phrase libre qui
    /// CONTIENT un genre, ni un mot d'humeur, ni un genre inconnu.
    #[tokio::test]
    async fn une_phrase_libre_n_est_pas_un_nom_de_genre() {
        let s = repo();
        for saisie in [
            "rock progressif des années 70",
            "romantique",
            "jazz doux pour le soir",
            "zouk",
        ] {
            assert_eq!(translate_query(&s, saisie).await, None, "{saisie:?}");
        }
    }

    #[tokio::test]
    async fn sans_cle_configuree_pas_de_traduction() {
        let s = repo();
        assert!(
            translate_query(&s, "jazz doux pour le soir")
                .await
                .is_none()
        );
        assert!(translate_query(&s, "  ").await.is_none());
    }
}
