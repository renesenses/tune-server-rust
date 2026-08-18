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

async fn call_provider(settings: &SettingsRepo, q: &str) -> Option<String> {
    let key_of = |name: &str| {
        settings
            .get(name)
            .ok()
            .flatten()
            .filter(|v| !v.trim().is_empty())
    };
    if let Some(key) = key_of("anthropic_api_key") {
        return anthropic_translate(&key, q).await;
    }
    if let Some(key) = key_of("openai_api_key") {
        return openai_translate(&key, q).await;
    }
    if let Some(key) = key_of("gemini_api_key") {
        return gemini_translate(&key, q).await;
    }
    None
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
