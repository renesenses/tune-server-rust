//! Le dialogue à outils de l'ancien assistant (`POST /ai/query`), pour TOUS
//! les fournisseurs de l'assistant (étape 3, « extension de l'existant »).
//!
//! L'hôte écrit le dialogue dans la forme de l'API Messages d'Anthropic
//! (`content` en blocs `text` / `tool_use` / `tool_result`, outils en
//! `name` / `description` / `input_schema`) et relit la réponse dans cette
//! même forme. Ce module traduit, dans les deux sens :
//!
//! - **compatible OpenAI** (OpenAI, Mistral…) et **Gemini** (son préréglage) :
//!   `POST {base}/chat/completions`, outils `{"type": "function", "function":
//!   {name, description, parameters}}`, appels rendus dans
//!   `choices[0].message.tool_calls` (arguments en TEXTE JSON), résultats
//!   renvoyés en messages `role: "tool"` avec `tool_call_id` ;
//! - **Ollama** : `POST {base}/api/chat`, mêmes outils, appels rendus dans
//!   `message.tool_calls` (arguments en OBJET, sans identifiant), résultats en
//!   messages `role: "tool"` avec `tool_name`.
//!
//! Rien n'est cru sur parole : un appel d'outil illisible (arguments qui ne
//! sont pas un objet JSON) part avec `{}`, et c'est l'exécuteur de l'hôte qui
//! refuse ce qui ne va pas.

use serde_json::{Map, Value, json};

/// Les outils, de la forme Anthropic à la forme « function » commune à
/// OpenAI et à Ollama.
pub fn outils_en_fonctions(outils: &Value) -> Vec<Value> {
    outils
        .as_array()
        .map(|o| {
            o.iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t["name"],
                            "description": t["description"],
                            "parameters": t.get("input_schema").cloned()
                                .unwrap_or_else(|| json!({"type": "object"})),
                        }
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Le texte des blocs `text` d'un contenu Anthropic (ou le texte lui-même).
fn texte_de(contenu: &Value) -> String {
    match contenu {
        Value::String(s) => s.clone(),
        Value::Array(blocs) => blocs
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn blocs<'a>(contenu: &'a Value, genre: &'a str) -> impl Iterator<Item = &'a Value> + 'a {
    contenu
        .as_array()
        .into_iter()
        .flatten()
        .filter(move |b| b["type"] == genre)
}

/// Le dialectes cible : les deux ne diffèrent que par les arguments (texte
/// ou objet) et la façon de rattacher un résultat à son appel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dialecte {
    Openai,
    Ollama,
}

/// Les messages, de la forme Anthropic à celle du dialecte, consigne en tête.
pub fn messages_vers(dialecte: Dialecte, systeme: &str, messages: &Value) -> Vec<Value> {
    let mut sortie = vec![json!({ "role": "system", "content": systeme })];
    // L'identifiant d'un appel → le nom de son outil (Ollama rattache un
    // résultat par le nom).
    let mut noms: Map<String, Value> = Map::new();
    for m in messages.as_array().into_iter().flatten() {
        let role = m["role"].as_str().unwrap_or("user");
        let contenu = &m["content"];
        let appels: Vec<&Value> = blocs(contenu, "tool_use").collect();
        let resultats: Vec<&Value> = blocs(contenu, "tool_result").collect();
        if role == "assistant" && !appels.is_empty() {
            let tool_calls: Vec<Value> = appels
                .iter()
                .map(|a| {
                    noms.insert(
                        a["id"].as_str().unwrap_or_default().to_string(),
                        a["name"].clone(),
                    );
                    let entree = a.get("input").cloned().unwrap_or_else(|| json!({}));
                    match dialecte {
                        Dialecte::Openai => json!({
                            "id": a["id"],
                            "type": "function",
                            "function": { "name": a["name"], "arguments": entree.to_string() },
                        }),
                        Dialecte::Ollama => json!({
                            "function": { "name": a["name"], "arguments": entree },
                        }),
                    }
                })
                .collect();
            let texte = texte_de(contenu);
            sortie.push(json!({
                "role": "assistant",
                "content": if texte.is_empty() && dialecte == Dialecte::Openai { Value::Null } else { json!(texte) },
                "tool_calls": tool_calls,
            }));
        } else if !resultats.is_empty() {
            for r in resultats {
                let id = r["tool_use_id"].as_str().unwrap_or_default();
                let corps = match &r["content"] {
                    Value::String(s) => s.clone(),
                    autre => autre.to_string(),
                };
                sortie.push(match dialecte {
                    Dialecte::Openai => {
                        json!({ "role": "tool", "tool_call_id": id, "content": corps })
                    }
                    Dialecte::Ollama => json!({
                        "role": "tool",
                        "tool_name": noms.get(id).cloned().unwrap_or(Value::Null),
                        "content": corps,
                    }),
                });
            }
        } else {
            sortie.push(json!({ "role": role, "content": texte_de(contenu) }));
        }
    }
    sortie
}

/// Les arguments d'un appel : un objet, lu d'un texte JSON (OpenAI) ou tel
/// quel (Ollama) ; illisible → `{}`.
fn arguments(v: &Value) -> Value {
    let lu = match v {
        Value::String(s) => serde_json::from_str::<Value>(s).unwrap_or(Value::Null),
        autre => autre.clone(),
    };
    if lu.is_object() { lu } else { json!({}) }
}

/// La réponse du dialecte, dans la forme de l'API Messages que l'hôte relit :
/// `{content: [text?, tool_use…], stop_reason}`. `message` est
/// `choices[0].message` (OpenAI) ou `message` (Ollama).
pub fn reponse_depuis(message: &Value) -> Value {
    let mut contenu = Vec::new();
    if let Some(t) = message["content"].as_str()
        && !t.trim().is_empty()
    {
        contenu.push(json!({ "type": "text", "text": t }));
    }
    for (i, appel) in message["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let Some(nom) = appel.pointer("/function/name").and_then(Value::as_str) else {
            continue;
        };
        let id = appel["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| format!("appel_{i}"));
        contenu.push(json!({
            "type": "tool_use",
            "id": id,
            "name": nom,
            "input": arguments(&appel["function"]["arguments"]),
        }));
    }
    let outils = contenu.iter().any(|b| b["type"] == "tool_use");
    json!({
        "content": contenu,
        "stop_reason": if outils { "tool_use" } else { "end_turn" },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialogue() -> Value {
        json!([
            {"role": "user", "content": "joue Kind of Blue"},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Je cherche."},
                {"type": "tool_use", "id": "t1", "name": "play_album", "input": {"query": "Kind of Blue"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "{\"ok\":true}"}
            ]}
        ])
    }

    #[test]
    fn les_outils_deviennent_des_fonctions() {
        let o = outils_en_fonctions(&json!([
            {"name": "pause", "description": "d", "input_schema": {"type": "object", "properties": {}}}
        ]));
        assert_eq!(o[0]["type"], "function");
        assert_eq!(o[0]["function"]["name"], "pause");
        assert_eq!(o[0]["function"]["parameters"]["type"], "object");
        assert!(outils_en_fonctions(&json!([])).is_empty());
    }

    #[test]
    fn le_dialogue_se_traduit_pour_openai() {
        let m = messages_vers(Dialecte::Openai, "consigne", &dialogue());
        assert_eq!(m[0], json!({"role": "system", "content": "consigne"}));
        assert_eq!(
            m[1],
            json!({"role": "user", "content": "joue Kind of Blue"})
        );
        assert_eq!(m[2]["role"], "assistant");
        assert_eq!(m[2]["content"], "Je cherche.");
        assert_eq!(m[2]["tool_calls"][0]["id"], "t1");
        assert_eq!(m[2]["tool_calls"][0]["function"]["name"], "play_album");
        // Les arguments partent en TEXTE JSON.
        let args: Value = serde_json::from_str(
            m[2]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(args["query"], "Kind of Blue");
        assert_eq!(
            m[3],
            json!({"role": "tool", "tool_call_id": "t1", "content": "{\"ok\":true}"})
        );
    }

    #[test]
    fn le_dialogue_se_traduit_pour_ollama() {
        let m = messages_vers(Dialecte::Ollama, "consigne", &dialogue());
        // Les arguments partent en OBJET, le résultat est rattaché par le nom.
        assert_eq!(
            m[2]["tool_calls"][0]["function"]["arguments"]["query"],
            "Kind of Blue"
        );
        assert_eq!(m[3]["role"], "tool");
        assert_eq!(m[3]["tool_name"], "play_album");
    }

    #[test]
    fn une_reponse_openai_redevient_des_blocs() {
        let v = reponse_depuis(&json!({
            "content": null,
            "tool_calls": [{"id": "c9", "type": "function",
                "function": {"name": "search_library", "arguments": "{\"query\":\"jazz\"}"}}]
        }));
        assert_eq!(v["stop_reason"], "tool_use");
        assert_eq!(v["content"][0]["type"], "tool_use");
        assert_eq!(v["content"][0]["id"], "c9");
        assert_eq!(v["content"][0]["input"]["query"], "jazz");
        let t = reponse_depuis(&json!({"content": "Voilà."}));
        assert_eq!(
            t,
            json!({"content": [{"type": "text", "text": "Voilà."}], "stop_reason": "end_turn"})
        );
    }

    #[test]
    fn une_reponse_ollama_sans_identifiant_en_recoit_un_et_des_arguments_illisibles_valent_vide() {
        let v = reponse_depuis(&json!({
            "content": "",
            "tool_calls": [
                {"function": {"name": "pause", "arguments": {}}},
                {"function": {"name": "set_volume", "arguments": "pas du json"}},
                {"function": {}}
            ]
        }));
        let c = v["content"].as_array().unwrap();
        assert_eq!(c.len(), 2, "un appel sans nom est ignoré");
        assert_eq!(c[0]["id"], "appel_0");
        assert_eq!(c[1]["input"], json!({}));
    }
}
